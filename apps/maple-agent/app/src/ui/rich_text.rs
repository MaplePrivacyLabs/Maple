//! Interactive transcript text: drag selection across paragraphs, clickable
//! links, and code blocks with a copy button.
//!
//! gpui 0.2.2 has no selection for static text, so this module provides it:
//! a shared [`TextSelection`] entity records an anchor/head position, and
//! every rendered paragraph is a [`RichText`] element that maps mouse
//! positions to byte indices through `TextLayout::index_for_position`.
//! Paragraphs are keyed by an `ordinal` (base per message + block index) so
//! a drag can span paragraphs; the text of visible paragraphs is registered
//! in the entity to rebuild the copied string.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use gpui::{
    App, Div, Element, ElementId, Entity, FocusHandle, GlobalElementId, Hitbox, HitboxBehavior,
    InspectorElementId, LayoutId, MouseButton, Pixels, SharedString, StyledText, Window, div,
    prelude::*,
};
use unicode_segmentation::UnicodeSegmentation;

use super::theme;
use super::widgets;

/// Styled runs of a paragraph, shared between the parsed block and the
/// element rendered from it each frame.
pub type Highlights = Arc<[(Range<usize>, HighlightStyleT)]>;

/// Clickable link ranges with destinations, shared like [`Highlights`].
pub type Links = Arc<[(Range<usize>, String)]>;
/// Byte ranges set in the monospace face: inline code spans.
pub type Mono = Arc<[Range<usize>]>;

/// Everything the parser resolved inside one paragraph.
#[derive(Clone, Default)]
pub struct Inline {
    pub highlights: Highlights,
    pub links: Links,
    pub mono: Mono,
}

/// Registry entries kept on each side of the selection when it overflows.
const REGISTRY_CAP: usize = 8192;
const REGISTRY_WINDOW: u64 = 2048;

/// Background drawn under selected text: Maple coral at ~40% alpha.
fn selection_background() -> gpui::Hsla {
    gpui::rgba(0xff977166).into()
}

/// Snap a byte index to the nearest char boundary at or before it.
fn snap_to_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// The word that contains byte `index` of `text`, as a byte range. A run
/// of whitespace or punctuation counts as a word, like other editors.
pub fn word_range_at(text: &str, index: usize) -> Range<usize> {
    let index = snap_to_boundary(text, index);
    for (start, word) in text.split_word_bound_indices() {
        let end = start + word.len();
        if index < end {
            return start..end;
        }
    }
    // Past the end: the last word, or an empty range in empty text.
    text.split_word_bound_indices()
        .next_back()
        .map_or(0..0, |(start, word)| start..start + word.len())
}

/// A position inside the transcript's paragraph space.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SelectionPos {
    /// Paragraph key: message base ordinal plus block index.
    pub ordinal: u64,
    /// Byte index into that paragraph's text.
    pub index: usize,
}

/// Unit a drag snaps to, from the click count that started it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Granularity {
    #[default]
    Char,
    Word,
    Paragraph,
}

impl Granularity {
    fn from_click_count(count: usize) -> Self {
        match count {
            0 | 1 => Self::Char,
            2 => Self::Word,
            _ => Self::Paragraph,
        }
    }
}

/// Shared drag-selection state, owned by the chat screen.
#[derive(Default)]
pub struct TextSelection {
    anchor: Option<SelectionPos>,
    head: Option<SelectionPos>,
    down: Option<SelectionPos>,
    selecting: bool,
    granularity: Granularity,
    /// The unit under the press (a word or paragraph), kept so a drag in
    /// either direction extends from it.
    origin: Option<(SelectionPos, SelectionPos)>,
    /// Text of the paragraphs rendered recently, keyed by ordinal, so the
    /// copied string can be rebuilt without touching chat state.
    registry: HashMap<u64, String>,
}

impl TextSelection {
    pub fn is_selecting(&self) -> bool {
        self.selecting
    }

    pub fn has_selection(&self) -> bool {
        self.anchor.is_some() && self.anchor != self.head
    }

    /// True when a mouse release has nothing to finish.
    fn is_idle(&self) -> bool {
        !self.selecting && self.down.is_none()
    }

    /// Forget the selection and every registered paragraph. Call this when
    /// the transcript changes session so stale text cannot be copied.
    pub fn clear(&mut self) {
        self.reset_selection();
        self.registry.clear();
    }

    fn reset_selection(&mut self) {
        self.anchor = None;
        self.head = None;
        self.down = None;
        self.selecting = false;
        self.granularity = Granularity::Char;
        self.origin = None;
    }

    /// The selection unit that contains `pos` at the current granularity.
    fn unit_at(&self, pos: SelectionPos) -> (SelectionPos, SelectionPos) {
        let Some(text) = self.registry.get(&pos.ordinal) else {
            return (pos, pos);
        };
        let range = match self.granularity {
            Granularity::Char => return (pos, pos),
            Granularity::Word => word_range_at(text, pos.index),
            Granularity::Paragraph => 0..text.len(),
        };
        let at = |index| SelectionPos {
            ordinal: pos.ordinal,
            index,
        };
        (at(range.start), at(range.end))
    }

    #[cfg(test)]
    fn begin(&mut self, pos: SelectionPos) {
        self.begin_with_clicks(pos, 1);
    }

    /// Start a selection: one click anchors at a character, a double click
    /// selects the word, a triple click the paragraph.
    fn begin_with_clicks(&mut self, pos: SelectionPos, click_count: usize) {
        self.granularity = Granularity::from_click_count(click_count);
        let (lo, hi) = self.unit_at(pos);
        self.anchor = Some(lo);
        self.head = Some(hi);
        self.down = Some(pos);
        self.origin = Some((lo, hi));
        self.selecting = true;
    }

    fn extend(&mut self, pos: SelectionPos) {
        if !self.selecting {
            return;
        }
        let (unit_lo, unit_hi) = self.unit_at(pos);
        match self.origin {
            Some((origin_lo, origin_hi)) if self.granularity != Granularity::Char => {
                if (pos.ordinal, pos.index) >= (origin_lo.ordinal, origin_lo.index) {
                    self.anchor = Some(origin_lo);
                    self.head = Some(unit_hi);
                } else {
                    self.anchor = Some(origin_hi);
                    self.head = Some(unit_lo);
                }
            }
            _ => self.head = Some(pos),
        }
    }

    /// Select every registered paragraph. The caller registers the
    /// paragraphs that are not on screen first (see `register`).
    pub fn select_all(&mut self) {
        self.reset_selection();
        self.anchor = Some(SelectionPos {
            ordinal: 0,
            index: 0,
        });
        self.head = Some(SelectionPos {
            ordinal: u64::MAX,
            index: usize::MAX,
        });
    }

    /// Drop a press nobody consumed: the release landed on no paragraph,
    /// so no click can follow. Without this a stale press would let a
    /// later release over a link open it without a click.
    fn forget_press(&mut self) {
        self.down = None;
    }

    /// End a drag from a paragraph that is not under the pointer. Leaves
    /// `down` in place so the hovered paragraph can still detect a click.
    fn end_drag(&mut self) {
        self.selecting = false;
        if self.anchor.is_some() && self.anchor == self.head {
            // A click without a drag leaves nothing selected.
            self.anchor = None;
            self.head = None;
        }
    }

    /// End a drag. Returns the URL when the press was a click (no drag)
    /// inside a link range.
    fn end_with_click(
        &mut self,
        ordinal: u64,
        index: usize,
        links: &[(Range<usize>, String)],
    ) -> Option<String> {
        let down = self.down.take();
        self.end_drag();
        let down = down?;
        if down.ordinal != ordinal || down.index != index {
            return None;
        }
        links
            .iter()
            .find(|(range, _)| range.contains(&index))
            .map(|(_, url)| url.clone())
    }

    /// Anchor and head in reading order, when a selection exists.
    fn bounds(&self) -> Option<(SelectionPos, SelectionPos)> {
        let anchor = self.anchor?;
        let head = self.head?;
        Some(
            if (head.ordinal, head.index) >= (anchor.ordinal, anchor.index) {
                (anchor, head)
            } else {
                (head, anchor)
            },
        )
    }

    /// The selected byte range within one paragraph, if any.
    fn range_for(&self, ordinal: u64) -> Option<Range<usize>> {
        let (lo, hi) = self.bounds()?;
        if ordinal < lo.ordinal || ordinal > hi.ordinal {
            return None;
        }
        let text = self.registry.get(&ordinal)?;
        let start = if ordinal == lo.ordinal { lo.index } else { 0 };
        let end = if ordinal == hi.ordinal {
            hi.index
        } else {
            text.len()
        };
        let (start, end) = (snap_to_boundary(text, start), snap_to_boundary(text, end));
        (start < end).then_some(start..end)
    }

    /// The selected text across paragraphs, joined with newlines.
    pub fn selected_text(&self) -> String {
        let Some((lo, hi)) = self.bounds() else {
            return String::new();
        };
        let mut ordinals: Vec<u64> = self
            .registry
            .keys()
            .copied()
            .filter(|ordinal| (lo.ordinal..=hi.ordinal).contains(ordinal))
            .collect();
        ordinals.sort_unstable();
        let mut parts: Vec<&str> = Vec::new();
        for ordinal in ordinals {
            let text = &self.registry[&ordinal];
            let start = if ordinal == lo.ordinal { lo.index } else { 0 };
            let end = if ordinal == hi.ordinal {
                hi.index
            } else {
                text.len()
            };
            let (start, end) = (snap_to_boundary(text, start), snap_to_boundary(text, end));
            if start < end {
                parts.push(&text[start..end]);
            }
        }
        parts.join("\n")
    }

    fn needs_register(&self, ordinal: u64, text: &str) -> bool {
        self.registry
            .get(&ordinal)
            .is_none_or(|registered| registered != text)
    }

    /// Keep a paragraph's text so it can be copied. Rendered paragraphs
    /// register themselves; `select_all` callers register the rest.
    pub fn register(&mut self, ordinal: u64, text: &str) {
        // A paragraph inside the selection changed (streaming): its byte
        // indices no longer mean the same characters, so drop the selection.
        let changed = self
            .registry
            .get(&ordinal)
            .is_some_and(|registered| registered != text);
        if changed
            && let Some((lo, hi)) = self.bounds()
            && (lo.ordinal..=hi.ordinal).contains(&ordinal)
        {
            self.reset_selection();
        }
        if self.registry.len() > REGISTRY_CAP {
            // Keep only a window around the active selection.
            match self.bounds() {
                Some((lo, hi)) => {
                    let floor = lo.ordinal.saturating_sub(REGISTRY_WINDOW);
                    let ceil = hi.ordinal.saturating_add(REGISTRY_WINDOW);
                    self.registry.retain(|key, _| (floor..=ceil).contains(key));
                }
                None => self.registry.clear(),
            }
        }
        self.registry.insert(ordinal, text.to_string());
    }
}

/// Context for turning parsed markdown blocks into interactive elements.
#[derive(Clone, Default)]
pub struct RenderCtx {
    /// Selection shared by every paragraph of the transcript.
    pub selection: Option<Entity<TextSelection>>,
    /// First ordinal of the message being rendered; block indexes are added.
    pub base_ordinal: Option<u64>,
    /// Focus handle moved to the transcript when a drag starts, so the
    /// copy keybinding applies while text is selected.
    pub focus: Option<FocusHandle>,
    /// Unique element-id seed for the message being rendered.
    pub id_seed: String,
    /// The view whose render this is, so a copy button can ask for one
    /// repaint of that view instead of refreshing the whole window.
    pub view: Option<gpui::EntityId>,
}

impl RenderCtx {
    /// Ordinal of block `index` when text blocks are interactive.
    pub fn for_block(&self, index: usize) -> Option<u64> {
        self.base_ordinal.map(|base| base + index as u64)
    }

    /// Element-id name shared by every code block of the message.
    pub fn id_name(&self) -> SharedString {
        if self.id_seed.is_empty() {
            SharedString::new_static("md")
        } else {
            SharedString::from(self.id_seed.clone())
        }
    }
}

/// One paragraph of markdown as an interactive element: styled runs,
/// clickable links, and drag selection.
pub struct RichText {
    ordinal: Option<u64>,
    text: SharedString,
    highlights: Highlights,
    links: Links,
    mono: Mono,
    selection: Option<Entity<TextSelection>>,
    focus: Option<FocusHandle>,
    styled: Option<StyledText>,
}

type HighlightStyleT = gpui::HighlightStyle;

/// Overlay the selection background on the paragraph's styled runs.
/// `base` must be sorted and non-overlapping. Returns `None` when there is
/// nothing to overlay and `base` can be used as is.
fn merged_highlights(
    base: &[(Range<usize>, HighlightStyleT)],
    selection: Option<Range<usize>>,
    text: &str,
) -> Option<Vec<(Range<usize>, HighlightStyleT)>> {
    let selection = selection?;
    let selection = snap_to_boundary(text, selection.start)..snap_to_boundary(text, selection.end);
    if selection.start >= selection.end {
        return None;
    }
    let text_len = text.len();
    let mut points: Vec<usize> = Vec::with_capacity(2 + base.len() * 2);
    points.push(selection.start);
    points.push(selection.end);
    for (range, _) in base {
        // Segment boundaries of the base styles, whether inside or outside
        // the selection, so unstyled gaps keep their original styling.
        points.push(range.start.min(text_len));
        points.push(range.end.min(text_len));
    }
    points.sort_unstable();
    points.dedup();
    let mut merged: Vec<(Range<usize>, HighlightStyleT)> = Vec::with_capacity(points.len());
    // Base runs are sorted, so a cursor that only moves forward finds the
    // run that covers each segment.
    let mut base_ix = 0;
    for pair in points.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        if start >= end {
            continue;
        }
        while base_ix < base.len() && base[base_ix].0.end <= start {
            base_ix += 1;
        }
        let base_style = base
            .get(base_ix)
            .filter(|(range, _)| range.start <= start)
            .map(|(_, style)| *style)
            .unwrap_or_default();
        let selected = selection.start <= start && start < selection.end;
        let mut style = base_style;
        if selected {
            style.background_color = Some(selection_background());
        }
        if let Some((last_range, last_style)) = merged.last_mut()
            && last_range.end == start
            && *last_style == style
        {
            last_range.end = end;
            continue;
        }
        merged.push((start..end, style));
    }
    Some(merged)
}

impl RichText {
    fn styled_text(&mut self, cx: &App) -> StyledText {
        let selection_range = self
            .selection
            .as_ref()
            .zip(self.ordinal)
            .and_then(|(entity, ordinal)| entity.read(cx).range_for(ordinal));
        let styled = StyledText::new(self.text.clone());
        let styled = match merged_highlights(&self.highlights, selection_range, &self.text) {
            Some(highlights) => styled.with_highlights(highlights),
            None => styled.with_highlights(self.highlights.iter().cloned()),
        };
        if self.mono.is_empty() {
            return styled;
        }
        // Inline code stays inside the one shaped line; the ranges come
        // sorted and disjoint from the parser, as the override requires.
        styled.with_font_family_overrides(
            self.mono
                .iter()
                .map(|range| (range.clone(), SharedString::from(crate::assets::FONT_MONO))),
        )
    }
}

impl Element for RichText {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut styled = self.styled_text(cx);
        let result = styled.request_layout(None, inspector_id, window, cx);
        self.styled = Some(styled);
        result
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: gpui::Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        if self.styled.is_none() {
            self.styled = Some(self.styled_text(cx));
        }
        let styled = self.styled.as_mut().expect("styled text built in layout");
        styled.prepaint(None, inspector_id, bounds, state, window, cx);
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: gpui::Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(styled) = self.styled.as_mut() else {
            return;
        };
        let mut styled_prepaint = ();
        styled.paint(
            None,
            None,
            _bounds,
            _state,
            &mut styled_prepaint,
            window,
            cx,
        );

        let layout = styled.layout().clone();
        let hitbox = hitbox.clone();

        // Keep the paragraph text available for copy.
        if let Some(entity) = &self.selection
            && let Some(ordinal) = self.ordinal
        {
            let text = self.text.clone();
            let needs = entity.read(cx).needs_register(ordinal, &text);
            if needs {
                entity.update(cx, |state, _| state.register(ordinal, &text));
            }
        }

        // IBeam over selectable text; the pointing hand wins on links.
        if self.selection.is_some() {
            let over_link = !self.links.is_empty()
                && layout
                    .index_for_position(window.mouse_position())
                    .map(|ix| self.links.iter().any(|(range, _)| range.contains(&ix)))
                    .unwrap_or(false);
            let style = if over_link {
                gpui::CursorStyle::PointingHand
            } else {
                gpui::CursorStyle::IBeam
            };
            window.set_cursor_style(style, &hitbox);
        }

        let entity = self.selection.clone();
        let ordinal = self.ordinal;
        let focus = self.focus.clone();

        // Press: start a selection at the character under the pointer.
        {
            let entity = entity.clone();
            let layout = layout.clone();
            let hitbox = hitbox.clone();
            window.on_mouse_event(
                move |event: &gpui::MouseDownEvent, phase, window: &mut Window, cx: &mut App| {
                    if phase != gpui::DispatchPhase::Bubble
                        || event.button != MouseButton::Left
                        || !hitbox.is_hovered(window)
                    {
                        return;
                    }
                    if let Some(focus) = &focus {
                        window.focus(focus, cx);
                    }
                    let Some(entity) = &entity else {
                        return;
                    };
                    if let Some(ordinal) = ordinal {
                        // Past the end of a line, Err carries the nearest index.
                        let index = layout
                            .index_for_position(event.position)
                            .unwrap_or_else(|ix| ix);
                        let click_count = event.click_count;
                        entity.update(cx, |state, cx| {
                            state.begin_with_clicks(SelectionPos { ordinal, index }, click_count);
                            cx.notify();
                        });
                    }
                },
            );
        }

        // Drag: move the head while the pointer stays over this paragraph.
        {
            let entity = entity.clone();
            let layout = layout.clone();
            let hitbox = hitbox.clone();
            window.on_mouse_event(
                move |event: &gpui::MouseMoveEvent, phase, window: &mut Window, cx: &mut App| {
                    if phase != gpui::DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                        return;
                    }
                    let Some(entity) = &entity else {
                        return;
                    };
                    if !entity.read(cx).is_selecting() {
                        return;
                    }
                    if let Some(ordinal) = ordinal {
                        let index = layout
                            .index_for_position(event.position)
                            .unwrap_or_else(|ix| ix);
                        entity.update(cx, |state, cx| {
                            state.extend(SelectionPos { ordinal, index });
                            cx.notify();
                        });
                    }
                },
            );
        }

        // Release: finish the selection; a no-drag press on a link opens it.
        // Every visible paragraph gets this event, in reverse paint order.
        // Only the hovered paragraph consumes `down`, so the others cannot
        // hide a click on a link from it.
        {
            let entity = entity.clone();
            let links = self.links.clone();
            window.on_mouse_event(
                move |event: &gpui::MouseUpEvent, phase, window: &mut Window, cx: &mut App| {
                    if phase != gpui::DispatchPhase::Bubble || event.button != MouseButton::Left {
                        return;
                    }
                    let Some(entity) = &entity else {
                        return;
                    };
                    if entity.read(cx).is_idle() {
                        return;
                    }
                    if !hitbox.is_hovered(window) {
                        entity.update(cx, |state, cx| {
                            state.end_drag();
                            cx.notify();
                        });
                        // The hovered paragraph, if any, consumes the press
                        // later in this same dispatch; once every listener
                        // ran, a press still there belongs to nobody.
                        let entity = entity.clone();
                        cx.defer(move |cx| {
                            entity.update(cx, |state, _| state.forget_press());
                        });
                        return;
                    }
                    let index = layout.index_for_position(event.position).ok();
                    let url = entity.update(cx, |state, cx| {
                        let url = match (ordinal, index) {
                            (Some(ordinal), Some(index)) => {
                                state.end_with_click(ordinal, index, &links)
                            }
                            _ => state.end_with_click(0, 0, &[]),
                        };
                        cx.notify();
                        url
                    });
                    if let Some(url) = url {
                        open_link(&url);
                    }
                },
            );
        }
    }
}

impl IntoElement for RichText {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

/// Open a URL in the system browser off the UI thread.
pub fn open_link(url: &str) {
    let url = url.to_string();
    std::thread::spawn(move || {
        if let Err(error) = webbrowser::open(&url) {
            log::debug!("failed to open {url}: {error}");
        }
    });
}

/// A styled, selectable paragraph. `ordinal` enables selection when set.
pub fn paragraph(
    text: SharedString,
    inline: Inline,
    text_size: Option<Pixels>,
    weight: Option<gpui::FontWeight>,
    ordinal: Option<u64>,
    ctx: &RenderCtx,
) -> Div {
    let mut container = div().w_full();
    if let Some(size) = text_size {
        container = container.text_size(size);
    }
    if let Some(weight) = weight {
        container = container.font_weight(weight);
    }
    let Inline {
        highlights,
        links,
        mono,
    } = inline;
    container.child(RichText {
        ordinal,
        text,
        highlights,
        links,
        mono,
        selection: ctx.selection.clone(),
        focus: ctx.focus.clone(),
        styled: None,
    })
}

/// Plain (unstyled) selectable text, used for user message bubbles.
pub fn plain_paragraph(text: SharedString, ordinal: Option<u64>, ctx: &RenderCtx) -> Div {
    paragraph(text, Inline::default(), None, None, ordinal, ctx)
}

/// Code block with a language label and a copy button. `label` is the
/// display name of the language; `copy_id` must be unique per block.
/// `ordinal` makes the code selectable like a paragraph.
pub fn code_block(
    code: SharedString,
    label: SharedString,
    id_name: SharedString,
    index: u64,
    ordinal: Option<u64>,
    ctx: &RenderCtx,
) -> Div {
    let copy_code = code.clone();
    let copy_id = ElementId::NamedInteger(id_name.clone(), index);
    let body_id = ElementId::NamedInteger(SharedString::from(format!("{id_name}-code")), index);
    div()
        .w_full()
        .my_1()
        .rounded(theme::RADIUS_SM)
        .bg(gpui::rgb(theme::bg_code_block()))
        .border_1()
        .border_color(gpui::rgb(theme::border_subtle()))
        .overflow_hidden()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_3()
                .py_1p5()
                .border_b_1()
                .border_color(gpui::rgb(theme::border_subtle()))
                .child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(label),
                )
                .child(widgets::copy_button(copy_id, copy_code, None, ctx.view)),
        )
        .child(
            // Long lines scroll sideways instead of wrapping; a vertical
            // wheel still reaches the transcript.
            div()
                .id(body_id)
                .px_3()
                .py_2()
                .w_full()
                .overflow_x_scroll()
                .restrict_scroll_to_axis()
                .whitespace_nowrap()
                .font_family(crate::assets::FONT_MONO)
                .text_size(gpui::px(13.))
                .text_color(gpui::rgb(theme::code_text()))
                // The bare element, not a full-width paragraph div, so the
                // scroll area measures the longest line.
                .child(RichText {
                    ordinal,
                    text: code,
                    highlights: Highlights::default(),
                    links: Links::default(),
                    mono: Mono::default(),
                    selection: ctx.selection.clone(),
                    focus: ctx.focus.clone(),
                    styled: None,
                }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(ordinal: u64, index: usize) -> SelectionPos {
        SelectionPos { ordinal, index }
    }

    #[test]
    fn click_without_drag_clears_and_opens_link() {
        let mut selection = TextSelection::default();
        selection.begin(pos(10, 4));
        let links = vec![(3..7, "https://example.test".to_string())];
        let url = selection.end_with_click(10, 4, &links);
        assert_eq!(url.as_deref(), Some("https://example.test"));
        assert!(!selection.has_selection());
        assert!(selection.is_idle());
    }

    #[test]
    fn end_drag_keeps_down_for_the_hovered_paragraph() {
        // Another paragraph's listener runs first and must not eat the click.
        let mut selection = TextSelection::default();
        selection.begin(pos(10, 4));
        selection.end_drag();
        assert!(!selection.is_selecting());
        assert!(!selection.has_selection());
        assert!(!selection.is_idle());
        let links = vec![(3..7, "https://example.test".to_string())];
        let url = selection.end_with_click(10, 4, &links);
        assert_eq!(url.as_deref(), Some("https://example.test"));
        assert!(selection.is_idle());
    }

    #[test]
    fn drag_keeps_selection() {
        let mut selection = TextSelection::default();
        selection.begin(pos(10, 2));
        selection.extend(pos(12, 5));
        assert!(selection.has_selection());
        assert_eq!(selection.bounds(), Some((pos(10, 2), pos(12, 5))));
    }

    #[test]
    fn range_for_spans_partial_and_whole_paragraphs() {
        let mut selection = TextSelection::default();
        selection.registry.insert(10, "hello world".to_string());
        selection.registry.insert(11, "second".to_string());
        selection.registry.insert(12, "third".to_string());
        selection.begin(pos(10, 6));
        selection.extend(pos(12, 2));
        assert_eq!(selection.range_for(10), Some(6..11));
        assert_eq!(selection.range_for(11), Some(0..6));
        assert_eq!(selection.range_for(12), Some(0..2));
        assert_eq!(selection.range_for(13), None);
        assert_eq!(selection.selected_text(), "world\nsecond\nth");
    }

    #[test]
    fn changed_text_inside_selection_clears_it() {
        let mut selection = TextSelection::default();
        selection.register(10, "héllo");
        selection.begin(pos(10, 0));
        selection.extend(pos(10, 3));
        assert!(selection.has_selection());
        selection.register(10, "h€llo");
        assert!(!selection.has_selection());
        assert_eq!(selection.selected_text(), "");
    }

    #[test]
    fn stale_indices_snap_to_char_boundaries() {
        let mut selection = TextSelection::default();
        selection.registry.insert(10, "h€llo".to_string());
        // Index 3 is inside the 3-byte euro sign.
        selection.begin(pos(10, 0));
        selection.extend(pos(10, 3));
        assert_eq!(selection.selected_text(), "h");
        assert_eq!(selection.range_for(10), Some(0..1));
        let merged = merged_highlights(&[], Some(0..3), "h€llo");
        assert_eq!(merged.map(|runs| runs[0].0.clone()), Some(0..1));
    }

    #[test]
    fn a_release_over_no_paragraph_forgets_the_press() {
        let mut selection = TextSelection::default();
        selection.begin(pos(10, 4));
        selection.end_drag();
        assert!(!selection.is_idle());
        selection.forget_press();
        assert!(selection.is_idle());
        // A later release over a link is not a click.
        let links = vec![(3..7, "https://example.test".to_string())];
        assert_eq!(selection.end_with_click(10, 4, &links), None);
    }

    #[test]
    fn double_click_selects_the_word_and_drags_by_words() {
        let mut selection = TextSelection::default();
        selection.register(10, "hello big world");
        selection.begin_with_clicks(pos(10, 7), 2);
        assert_eq!(selection.selected_text(), "big");
        selection.extend(pos(10, 13));
        assert_eq!(selection.selected_text(), "big world");
        selection.extend(pos(10, 1));
        assert_eq!(selection.selected_text(), "hello big");
        selection.end_drag();
        assert!(selection.has_selection());
    }

    #[test]
    fn triple_click_selects_the_paragraph() {
        let mut selection = TextSelection::default();
        selection.register(10, "first line");
        selection.register(11, "second");
        selection.begin_with_clicks(pos(10, 3), 3);
        assert_eq!(selection.selected_text(), "first line");
        selection.extend(pos(11, 2));
        assert_eq!(selection.selected_text(), "first line\nsecond");
    }

    #[test]
    fn word_range_at_handles_edges() {
        assert_eq!(word_range_at("ab cd", 0), 0..2);
        assert_eq!(word_range_at("ab cd", 2), 2..3);
        assert_eq!(word_range_at("ab cd", 4), 3..5);
        assert_eq!(word_range_at("ab cd", 9), 3..5);
        assert_eq!(word_range_at("", 0), 0..0);
    }

    #[test]
    fn select_all_covers_every_registered_paragraph_in_order() {
        let mut selection = TextSelection::default();
        selection.register(4096, "one");
        selection.register(8192, "two");
        selection.register(4097, "one b");
        selection.select_all();
        assert!(selection.has_selection());
        assert_eq!(selection.selected_text(), "one\none b\ntwo");
        assert_eq!(selection.range_for(8192), Some(0..3));
    }

    #[test]
    fn clear_forgets_registered_paragraphs() {
        let mut selection = TextSelection::default();
        selection.register(10, "hello");
        selection.begin(pos(10, 0));
        selection.clear();
        assert!(selection.registry.is_empty());
        assert!(selection.is_idle());
    }

    #[test]
    fn registry_stays_bounded_around_the_selection() {
        let mut selection = TextSelection::default();
        selection.registry.insert(5, "anchor".to_string());
        selection.begin(pos(5, 0));
        selection.extend(pos(5, 3));
        for ordinal in 0..(REGISTRY_CAP as u64 + 10) {
            let text = if ordinal == 5 { "anchor" } else { "x" };
            selection.register(ordinal, text);
        }
        assert!(selection.has_selection());
        assert!(selection.registry.len() <= REGISTRY_CAP + 1);
        assert!(selection.registry.contains_key(&5));
    }

    #[test]
    fn merged_highlights_overlay_selection() {
        let base = vec![(0..3, HighlightStyleT::default())];
        let merged = merged_highlights(&base, Some(2..6), "abcdefghij").expect("overlay");
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].0, 0..2);
        assert!(merged[0].1.background_color.is_none());
        assert_eq!(merged[1].0, 2..6);
        assert!(merged[1].1.background_color.is_some());
        assert!(merged_highlights(&base, None, "abcdefghij").is_none());
    }

    #[test]
    fn merged_highlights_keep_gaps_between_runs() {
        let bold = HighlightStyleT {
            font_weight: Some(gpui::FontWeight::BOLD),
            ..Default::default()
        };
        let base = vec![(0..2, bold), (4..6, bold)];
        let merged = merged_highlights(&base, Some(1..5), "abcdefgh").expect("overlay");
        let runs: Vec<_> = merged
            .iter()
            .map(|(range, style)| {
                (
                    range.clone(),
                    style.font_weight.is_some(),
                    style.background_color.is_some(),
                )
            })
            .collect();
        assert_eq!(
            runs,
            vec![
                (0..1, true, false),
                (1..2, true, true),
                (2..4, false, true),
                (4..5, true, true),
                (5..6, true, false),
            ]
        );
    }
}
