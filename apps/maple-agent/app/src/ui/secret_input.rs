//! A single-line secret input. Only masks and public metadata reach GPUI's
//! shared text, shaping cache, accessibility tree, and surrounding-text API.
//!
//! The control owns one zeroizing value, without history, spelling, or a copy /
//! cut path. Edits replace the complete allocation so deleted bytes and old
//! allocations are zeroized too. Platform input methods and the system
//! clipboard have their own memory lifetimes outside this control.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardEntry, ContentMask, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    GlobalElementId, LayoutId, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels,
    Point, ShapedLine, SharedString, Style, Subscription, TextAlign, TextRun, UTF16Selection,
    UnderlineStyle, Window, div, fill, point, prelude::*, px, relative, rgb, size,
};
use unicode_segmentation::UnicodeSegmentation;
use zeroize::Zeroizing;

use super::{text_input, theme};

/// The value changed. This event deliberately carries no secret material.
pub struct SecretInputChanged;

/// Enter was pressed outside an active IME composition.
pub struct SecretInputSubmitted;

/// Upper bound for control-owned input, independent of the store's password
/// policy. Reject oversized edits atomically instead of truncating a password.
const MAX_SECRET_BYTES: usize = 64 * 1024;

// CarbonEventsCore.h documents an OS-maintained enable count and requires
// release when the application becomes inactive. Each control owns exactly
// one successful enable, independent of other secret controls in the process.
#[cfg(target_os = "macos")]
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn EnableSecureEventInput() -> i32;
    fn DisableSecureEventInput() -> i32;
}

fn enable_secure_event_input() -> i32 {
    #[cfg(target_os = "macos")]
    // SAFETY: this guard is confined to GPUI's UI thread. The Carbon call has
    // no pointer arguments, and a successful call owns one matching release.
    unsafe {
        EnableSecureEventInput()
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}

fn disable_secure_event_input() -> i32 {
    #[cfg(target_os = "macos")]
    // SAFETY: same UI-thread confinement as enable; the guard calls this only
    // while it owns a successful enable that has not yet been released.
    unsafe {
        DisableSecureEventInput()
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}

struct SecureEventInput {
    enabled: bool,
    enable: fn() -> i32,
    disable: fn() -> i32,
    // Carbon's calls are not thread safe. A guard cannot be moved into a
    // backend task or dropped there, including on non-macOS development hosts.
    _ui_thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Default for SecureEventInput {
    fn default() -> Self {
        Self {
            enabled: false,
            enable: enable_secure_event_input,
            disable: disable_secure_event_input,
            _ui_thread: std::marker::PhantomData,
        }
    }
}

impl SecureEventInput {
    fn set_active(&mut self, active: bool) -> bool {
        if active && !self.enabled {
            if (self.enable)() == 0 {
                self.enabled = true;
            } else {
                log::warn!("Could not enable secure keyboard input");
            }
        } else if !active && self.enabled {
            if (self.disable)() == 0 {
                self.enabled = false;
            } else {
                // Retain ownership after failure so blur/drop can retry the
                // release instead of abandoning an outstanding native count.
                log::warn!("Could not release secure keyboard input");
            }
        }
        self.enabled == active
    }
}

impl Drop for SecureEventInput {
    fn drop(&mut self) {
        self.set_active(false);
    }
}

#[derive(Default)]
struct SecretBuffer {
    value: Zeroizing<String>,
    selected: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
}

impl SecretBuffer {
    fn cursor(&self) -> usize {
        if self.reversed {
            self.selected.start
        } else {
            self.selected.end
        }
    }

    fn move_to(&mut self, offset: usize, select: bool) {
        let offset = snap_byte(&self.value, offset);
        if select {
            let anchor = if self.reversed {
                self.selected.end
            } else {
                self.selected.start
            };
            self.selected = anchor.min(offset)..anchor.max(offset);
            self.reversed = offset < anchor;
        } else {
            self.selected = offset..offset;
            self.reversed = false;
        }
    }

    fn previous(&self, offset: usize) -> usize {
        self.value
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    fn next(&self, offset: usize) -> usize {
        self.value
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.value.len())
    }

    fn word_boundary(&self, forward: bool) -> usize {
        let cursor = self.cursor();
        if forward {
            self.value
                .unicode_word_indices()
                .map(|(index, word)| index + word.len())
                .find(|end| *end > cursor)
                .unwrap_or(self.value.len())
        } else {
            self.value
                .unicode_word_indices()
                .rev()
                .find_map(|(index, _)| (index < cursor).then_some(index))
                .unwrap_or(0)
        }
    }

    fn edit_range(&self, range_utf16: Option<Range<usize>>) -> Range<usize> {
        range_utf16
            .map(|range| utf16_range_to_bytes(&self.value, range))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selected.clone())
    }

    fn replace(&mut self, range: Range<usize>, text: &str) -> bool {
        let start = snap_byte(&self.value, range.start.min(range.end));
        let end = snap_byte(&self.value, range.start.max(range.end));
        let Some(len) = self
            .value
            .len()
            .checked_sub(end - start)
            .and_then(|len| len.checked_add(text.len()))
        else {
            return false;
        };
        if len > MAX_SECRET_BYTES || text.contains(['\r', '\n']) {
            return false;
        }
        // Reserve once before any secret bytes are copied. String::replace_range
        // could leave removed bytes in spare capacity or free an unwiped buffer.
        let mut replacement = Zeroizing::new(String::with_capacity(len));
        replacement.push_str(&self.value[..start]);
        replacement.push_str(text);
        replacement.push_str(&self.value[end..]);
        self.value = replacement;
        let cursor = start + text.len();
        self.selected = cursor..cursor;
        self.reversed = false;
        self.marked = None;
        true
    }

    fn replace_marked(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        selected_utf16: Option<Range<usize>>,
    ) -> bool {
        let range = self.edit_range(range_utf16);
        let start = range.start;
        if !self.replace(range, text) {
            return false;
        }
        self.marked = (!text.is_empty()).then_some(start..start + text.len());
        if let Some(selected) = selected_utf16 {
            // An IME selection is relative to its newly marked text, not to the
            // whole input (which can already have a multibyte prefix).
            let selected = utf16_range_to_bytes(text, selected);
            self.selected = start + selected.start..start + selected.end;
        }
        true
    }

    #[cfg(test)]
    fn replace_owned(&mut self, value: Zeroizing<String>) -> bool {
        if value.len() > MAX_SECRET_BYTES || value.contains(['\r', '\n']) {
            return false;
        }
        self.value = value;
        self.selected = self.value.len()..self.value.len();
        self.reversed = false;
        self.marked = None;
        true
    }

    fn take(&mut self) -> Zeroizing<String> {
        self.selected = 0..0;
        self.reversed = false;
        self.marked = None;
        std::mem::take(&mut self.value)
    }

    fn masked_range(&self, range_utf16: Range<usize>) -> (String, Range<usize>) {
        let range = utf16_range_to_bytes(&self.value, range_utf16);
        let actual =
            bytes_to_utf16(&self.value, range.start)..bytes_to_utf16(&self.value, range.end);
        ("*".repeat(actual.len()), actual)
    }
}

/// No `Clone`, `Debug`, serialization, or plaintext-returning text accessor.
pub struct SecretInput {
    buffer: SecretBuffer,
    focus: FocusHandle,
    placeholder: SharedString,
    placeholder_key: Option<&'static str>,
    masks: SharedString,
    tab_index: Option<isize>,
    selecting: bool,
    scroll_x: Pixels,
    last_bounds: Option<Bounds<Pixels>>,
    layout: Option<MaskLayout>,
    secure_input: SecureEventInput,
    focus_subscriptions: Vec<Subscription>,
}

impl EventEmitter<SecretInputChanged> for SecretInput {}
impl EventEmitter<SecretInputSubmitted> for SecretInput {}

impl SecretInput {
    /// A catalog-backed placeholder and accessibility label that follow live language changes.
    pub fn localized(key: &'static str, cx: &mut Context<Self>) -> Self {
        let mut input = Self::new("", cx);
        input.placeholder_key = Some(key);
        input
    }

    fn placeholder(&self) -> SharedString {
        self.placeholder_key
            .map_or_else(|| self.placeholder.clone(), super::i18n::tr)
    }

    pub fn new(placeholder: &str, cx: &mut Context<Self>) -> Self {
        Self {
            buffer: SecretBuffer::default(),
            focus: cx.focus_handle(),
            placeholder_key: None,
            placeholder: placeholder.to_owned().into(),
            masks: "".into(),
            tab_index: None,
            selecting: false,
            scroll_x: px(0.),
            last_bounds: None,
            layout: None,
            secure_input: SecureEventInput::default(),
            focus_subscriptions: Vec::new(),
        }
    }

    pub fn with_tab_index(mut self, index: isize) -> Self {
        self.tab_index = Some(index);
        // track_focus supplies our own handle, so GPUI does not apply the
        // element's tab_index to it. Configure the handle's actual tab stop
        // as well or focus_next/focus_prev skip this secret field.
        self.focus = self.focus.clone().tab_index(index).tab_stop(true);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.value.is_empty()
    }

    pub fn len_chars(&self) -> usize {
        self.buffer.value.chars().count()
    }

    /// Borrow for a synchronous local check, such as password strength or
    /// confirmation. The control makes no copy; callers must not retain one.
    pub fn with_secret<R>(&self, check: impl FnOnce(&str) -> R) -> R {
        check(&self.buffer.value)
    }

    /// Move the sole retained value into its consumer and reset selection,
    /// composition, and render metadata. The consumer owns zeroization now.
    pub fn take_secret(&mut self, cx: &mut Context<Self>) -> Zeroizing<String> {
        self.secure_input.set_active(false);
        let value = self.buffer.take();
        self.changed(cx);
        value
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.secure_input.set_active(false);
        drop(self.buffer.take());
        self.changed(cx);
    }

    /// Install an already zeroizing value by ownership, without cloning it.
    /// An invalid single-line value is rejected and zeroized on return.
    #[cfg(test)]
    pub fn replace_secret(&mut self, value: Zeroizing<String>, cx: &mut Context<Self>) -> bool {
        if !self.buffer.replace_owned(value) {
            return false;
        }
        self.changed(cx);
        true
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus, cx);
    }

    fn sync_secure_input(&mut self, window: &Window) {
        self.secure_input
            .set_active(window.is_window_active() && self.focus.is_focused(window));
    }

    fn observe_secure_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_subscriptions.is_empty() {
            return;
        }
        let focus = self.focus.clone();
        self.focus_subscriptions
            .push(cx.on_focus(&focus, window, |input, window, _cx| {
                input.sync_secure_input(window);
            }));
        self.focus_subscriptions.push(cx.on_focus_out(
            &focus,
            window,
            |input, _event, _window, _cx| {
                input.secure_input.set_active(false);
            },
        ));
        self.focus_subscriptions.push(cx.observe_window_activation(
            window,
            |input, window, _cx| {
                input.sync_secure_input(window);
            },
        ));
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.masks = "*".repeat(self.len_chars()).into();
        self.layout = None;
        self.selecting = false;
        if self.is_empty() {
            self.scroll_x = px(0.);
        }
        cx.emit(SecretInputChanged);
        cx.notify();
    }

    fn move_to(&mut self, offset: usize, select: bool, cx: &mut Context<Self>) {
        self.buffer.move_to(offset, select);
        cx.notify();
    }

    fn delete_selection(&mut self, cx: &mut Context<Self>) {
        if self.buffer.replace(self.buffer.selected.clone(), "") {
            self.changed(cx);
        }
    }

    fn backspace(&mut self, _: &text_input::Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if self.buffer.selected.is_empty() {
            let previous = self.buffer.previous(self.buffer.cursor());
            self.buffer.move_to(previous, true);
        }
        self.delete_selection(cx);
    }

    fn delete(&mut self, _: &text_input::Delete, _: &mut Window, cx: &mut Context<Self>) {
        if self.buffer.selected.is_empty() {
            let next = self.buffer.next(self.buffer.cursor());
            self.buffer.move_to(next, true);
        }
        self.delete_selection(cx);
    }

    fn left(&mut self, _: &text_input::Left, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.buffer.selected.is_empty() {
            self.buffer.previous(self.buffer.cursor())
        } else {
            self.buffer.selected.start
        };
        self.move_to(offset, false, cx);
    }

    fn right(&mut self, _: &text_input::Right, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.buffer.selected.is_empty() {
            self.buffer.next(self.buffer.cursor())
        } else {
            self.buffer.selected.end
        };
        self.move_to(offset, false, cx);
    }

    fn select_left(&mut self, _: &text_input::SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.buffer.previous(self.buffer.cursor()), true, cx);
    }

    fn select_right(
        &mut self,
        _: &text_input::SelectRight,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_to(self.buffer.next(self.buffer.cursor()), true, cx);
    }

    fn select_all(&mut self, _: &text_input::SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_to(0, false);
        self.move_to(self.buffer.value.len(), true, cx);
    }

    fn home<A>(&mut self, _: &A, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, false, cx);
    }

    fn end<A>(&mut self, _: &A, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.buffer.value.len(), false, cx);
    }

    fn select_start<A>(&mut self, _: &A, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, true, cx);
    }

    fn select_end<A>(&mut self, _: &A, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.buffer.value.len(), true, cx);
    }

    fn word_left(&mut self, _: &text_input::WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.buffer.word_boundary(false), false, cx);
    }

    fn word_right(&mut self, _: &text_input::WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.buffer.word_boundary(true), false, cx);
    }

    fn select_word_left(
        &mut self,
        _: &text_input::SelectWordLeft,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_to(self.buffer.word_boundary(false), true, cx);
    }

    fn select_word_right(
        &mut self,
        _: &text_input::SelectWordRight,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_to(self.buffer.word_boundary(true), true, cx);
    }

    fn delete_word_backward(
        &mut self,
        _: &text_input::DeleteWordBackward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.buffer.selected.is_empty() {
            self.buffer.move_to(self.buffer.word_boundary(false), true);
        }
        self.delete_selection(cx);
    }

    fn delete_word_forward(
        &mut self,
        _: &text_input::DeleteWordForward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.buffer.selected.is_empty() {
            self.buffer.move_to(self.buffer.word_boundary(true), true);
        }
        self.delete_selection(cx);
    }

    fn delete_to_start(
        &mut self,
        _: &text_input::DeleteToLineStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer.move_to(0, true);
        self.delete_selection(cx);
    }

    fn delete_to_end(
        &mut self,
        _: &text_input::DeleteToLineEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer.move_to(self.buffer.value.len(), true);
        self.delete_selection(cx);
    }

    fn block_action<A>(&mut self, _: &A, _: &mut Window, cx: &mut Context<Self>) {
        // Consume these actions so a parent menu cannot copy, cut, or undo
        // on behalf of the focused secret field.
        cx.stop_propagation();
    }

    fn character_palette(
        &mut self,
        _: &text_input::ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &text_input::Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        // Move GPUI's owned clipboard strings into zeroizing wrappers. Do not
        // use ClipboardItem::text(), which builds an unzeroized extra copy.
        let total = item
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                ClipboardEntry::String(value) => Some(value.text.len()),
                _ => None,
            })
            .try_fold(0usize, |sum, len| sum.checked_add(len));
        let mut text = Zeroizing::new(String::with_capacity(
            total.filter(|len| *len <= MAX_SECRET_BYTES).unwrap_or(0),
        ));
        for entry in item.into_entries() {
            if let ClipboardEntry::String(value) = entry {
                let value_text = Zeroizing::new(value.text);
                let _metadata = value.metadata.map(Zeroizing::new);
                if total.is_some_and(|len| len <= MAX_SECRET_BYTES) {
                    text.push_str(&value_text);
                }
            }
        }
        if total.is_some_and(|len| len <= MAX_SECRET_BYTES) && !text.is_empty() {
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn index_at(&self, position: Point<Pixels>) -> Option<usize> {
        if self.is_empty() {
            return Some(0);
        }
        let bounds = self.last_bounds?;
        let layout = self.layout.as_ref()?;
        let index = layout
            .line
            .closest_index_for_x(position.x - bounds.left() + self.scroll_x);
        Some(byte_for_char(&self.buffer.value, index))
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        self.sync_secure_input(window);
        let Some(index) = self.index_at(event.position) else {
            return;
        };
        if event.click_count >= 2 {
            self.buffer.move_to(0, false);
            self.buffer.move_to(self.buffer.value.len(), true);
            self.selecting = false;
            cx.notify();
        } else {
            self.selecting = true;
            self.move_to(index, event.modifiers.shift, cx);
        }
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting
            && let Some(index) = self.index_at(event.position)
        {
            self.move_to(index, true, cx);
        }
    }
}

impl Focusable for SecretInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EntityInputHandler for SecretInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        // Native IMEs may query surrounding text. Never hand the platform a
        // plaintext String. Preserve UTF-16 length for its replacement ranges.
        let (masks, range) = self.buffer.masked_range(range_utf16);
        *actual_range = Some(range);
        Some(masks)
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: bytes_to_utf16(&self.buffer.value, self.buffer.selected.start)
                ..bytes_to_utf16(&self.buffer.value, self.buffer.selected.end),
            reversed: self.buffer.reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.buffer.marked.as_ref().map(|range| {
            bytes_to_utf16(&self.buffer.value, range.start)
                ..bytes_to_utf16(&self.buffer.value, range.end)
        })
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.buffer.edit_range(range_utf16);
        if self.buffer.replace(range, text) {
            self.changed(cx);
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        selected_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .buffer
            .replace_marked(range_utf16, text, selected_utf16)
        {
            self.changed(cx);
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout.as_ref()?;
        let range = utf16_range_to_bytes(&self.buffer.value, range_utf16);
        let start = self.buffer.value[..range.start].chars().count();
        let end = self.buffer.value[..range.end].chars().count();
        Some(Bounds::from_corners(
            point(
                bounds.left() + layout.line.x_for_index(start) - self.scroll_x,
                bounds.top(),
            ),
            point(
                bounds.left() + layout.line.x_for_index(end) - self.scroll_x,
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        self.index_at(position)
            .map(|byte| bytes_to_utf16(&self.buffer.value, byte))
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer.selected = utf16_range_to_bytes(&self.buffer.value, range_utf16);
        self.buffer.reversed = false;
        cx.notify();
    }

    fn text_length_utf16(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        Some(self.buffer.value.encode_utf16().count())
    }

    fn accepts_text_input(&self, _: &mut Window, _: &mut Context<Self>) -> bool {
        // A failed native enable never silently accepts keyboard entry in a
        // field that the user expects to have macOS secure input protection.
        !cfg!(target_os = "macos") || self.secure_input.enabled
    }
}

struct MaskLayout {
    line: ShapedLine,
    font_size: Pixels,
    runs: Vec<TextRun>,
}

struct SecretElement {
    input: Entity<SecretInput>,
}

struct SecretPrepaint {
    line: ShapedLine,
    scroll_x: Pixels,
    selection: Option<PaintQuad>,
    cursor: Option<PaintQuad>,
}

impl IntoElement for SecretElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SecretElement {
    type RequestLayoutState = ();
    type PrepaintState = SecretPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let text = if input.is_empty() {
            input.placeholder()
        } else {
            input.masks.clone()
        };
        let style = window.text_style();
        let color = if input.is_empty() {
            theme::placeholder()
        } else {
            rgb(theme::text_primary()).into()
        };
        let run = TextRun {
            len: text.len(),
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked) = input.buffer.marked.as_ref() {
            let start = input.buffer.value[..marked.start].chars().count();
            let end = input.buffer.value[..marked.end].chars().count();
            vec![
                TextRun {
                    len: start,
                    ..run.clone()
                },
                TextRun {
                    len: end - start,
                    underline: Some(UnderlineStyle {
                        color: Some(color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: text.len() - end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = input
            .layout
            .as_ref()
            .filter(|cache| {
                cache.line.text == text && cache.font_size == font_size && cache.runs == runs
            })
            .map(|cache| cache.line.clone())
            .unwrap_or_else(|| {
                window
                    .text_system()
                    .shape_line(text, font_size, &runs, None)
            });
        let start = input.buffer.value[..input.buffer.selected.start]
            .chars()
            .count();
        let end = input.buffer.value[..input.buffer.selected.end]
            .chars()
            .count();
        let cursor = if input.buffer.reversed { start } else { end };
        let cursor_x = if input.is_empty() {
            px(0.)
        } else {
            line.x_for_index(cursor)
        };
        let width = bounds.size.width;
        let mut scroll_x = input.scroll_x;
        if cursor_x - scroll_x > width - px(2.) {
            scroll_x = cursor_x - width + px(2.);
        }
        if cursor_x - scroll_x < px(0.) {
            scroll_x = cursor_x;
        }
        scroll_x = scroll_x
            .min((line.width() - width + px(2.)).max(px(0.)))
            .max(px(0.));
        let (selection, cursor) = if input.buffer.selected.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + cursor_x - scroll_x, bounds.top()),
                        size(px(2.), bounds.size.height),
                    ),
                    rgb(theme::text_cursor()),
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            bounds.left() + line.x_for_index(start) - scroll_x,
                            bounds.top(),
                        ),
                        point(
                            bounds.left() + line.x_for_index(end) - scroll_x,
                            bounds.bottom(),
                        ),
                    ),
                    theme::text_selection(),
                )),
                None,
            )
        };
        self.input.update(cx, |input, _| {
            input.layout = Some(MaskLayout {
                line: line.clone(),
                font_size,
                runs,
            });
            input.scroll_x = scroll_x;
            input.last_bounds = Some(bounds);
        });
        SecretPrepaint {
            line,
            scroll_x,
            selection,
            cursor,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let focused = focus.is_focused(window);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            if focused && let Some(selection) = prepaint.selection.take() {
                window.paint_quad(selection);
            }
            let _ = prepaint.line.paint(
                point(bounds.left() - prepaint.scroll_x, bounds.top()),
                window.line_height(),
                TextAlign::Left,
                None,
                window,
                cx,
            );
            if focused && let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        });
    }
}

impl Render for SecretInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.observe_secure_focus(window, cx);
        self.sync_secure_input(window);
        div()
            .id(("secret-input", cx.entity_id().as_u64()))
            .role(gpui::Role::TextInput)
            .aria_label(self.placeholder())
            .aria_placeholder(self.placeholder())
            .key_context("TextInput SecretInput input_role = other editor_vim_mode = disabled")
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .when_some(self.tab_index, |element, index| element.tab_index(index))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home::<text_input::Home>))
            .on_action(cx.listener(Self::end::<text_input::End>))
            .on_action(cx.listener(Self::home::<text_input::DocumentStart>))
            .on_action(cx.listener(Self::end::<text_input::DocumentEnd>))
            .on_action(cx.listener(Self::home::<text_input::ParagraphStart>))
            .on_action(cx.listener(Self::end::<text_input::ParagraphEnd>))
            .on_action(cx.listener(Self::select_start::<text_input::SelectLineStart>))
            .on_action(cx.listener(Self::select_end::<text_input::SelectLineEnd>))
            .on_action(cx.listener(Self::select_start::<text_input::SelectDocumentStart>))
            .on_action(cx.listener(Self::select_end::<text_input::SelectDocumentEnd>))
            .on_action(cx.listener(Self::select_start::<text_input::SelectParagraphStart>))
            .on_action(cx.listener(Self::select_end::<text_input::SelectParagraphEnd>))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::delete_word_backward))
            .on_action(cx.listener(Self::delete_word_forward))
            .on_action(cx.listener(Self::delete_to_start))
            .on_action(cx.listener(Self::delete_to_end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::character_palette))
            .on_action(cx.listener(Self::block_action::<text_input::Copy>))
            .on_action(cx.listener(Self::block_action::<text_input::Cut>))
            .on_action(cx.listener(Self::block_action::<text_input::Undo>))
            .on_action(cx.listener(Self::block_action::<text_input::Redo>))
            .on_action(cx.listener(Self::block_action::<text_input::Up>))
            .on_action(cx.listener(Self::block_action::<text_input::Down>))
            .on_action(cx.listener(Self::block_action::<text_input::SelectUp>))
            .on_action(cx.listener(Self::block_action::<text_input::SelectDown>))
            .on_key_down(
                cx.listener(|input, event: &gpui::KeyDownEvent, window, cx| {
                    let plain = !event.keystroke.modifiers.control
                        && !event.keystroke.modifiers.alt
                        && !event.keystroke.modifiers.platform;
                    if !plain || input.buffer.marked.is_some() {
                        return;
                    }
                    if event.keystroke.key.eq_ignore_ascii_case("tab") {
                        if event.keystroke.modifiers.shift {
                            window.focus_prev(cx);
                        } else {
                            window.focus_next(cx);
                        }
                        cx.stop_propagation();
                    } else if event.keystroke.key.eq_ignore_ascii_case("enter") {
                        input.secure_input.set_active(false);
                        cx.emit(SecretInputSubmitted);
                        cx.stop_propagation();
                    }
                }),
            )
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(gpui::MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .w_full()
            .child(SecretElement { input: cx.entity() })
    }
}

fn snap_byte(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn bytes_to_utf16(text: &str, offset: usize) -> usize {
    text[..snap_byte(text, offset)].encode_utf16().count()
}

fn byte_for_char(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map(|(index, _)| index)
        .unwrap_or(text.len())
}

fn byte_for_utf16(text: &str, offset: usize, round_up: bool) -> usize {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units >= offset {
            return byte;
        }
        let end = units + ch.len_utf16();
        if end > offset {
            return if round_up { byte + ch.len_utf8() } else { byte };
        }
        units = end;
    }
    text.len()
}

fn utf16_range_to_bytes(text: &str, range: Range<usize>) -> Range<usize> {
    let start = range.start.min(range.end);
    let end = range.start.max(range.end);
    let start_byte = byte_for_utf16(text, start, false);
    if start == end {
        start_byte..start_byte
    } else {
        start_byte..byte_for_utf16(text, end, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        static SECURE_EVENTS: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    fn mock_enable() -> i32 {
        SECURE_EVENTS.with_borrow_mut(|events| events.push("enable"));
        0
    }

    fn mock_disable() -> i32 {
        SECURE_EVENTS.with_borrow_mut(|events| events.push("disable"));
        0
    }

    fn mock_failed_enable() -> i32 {
        SECURE_EVENTS.with_borrow_mut(|events| events.push("failed_enable"));
        -1
    }

    fn mock_secure_input() -> SecureEventInput {
        SecureEventInput {
            enable: mock_enable,
            disable: mock_disable,
            ..SecureEventInput::default()
        }
    }

    #[test]
    fn secure_event_input_is_balanced_across_focus_submit_and_drop() {
        SECURE_EVENTS.with_borrow_mut(Vec::clear);
        let mut first = mock_secure_input();
        assert!(first.set_active(true));
        assert!(first.set_active(true)); // Rendering cannot add another count.
        let mut second = mock_secure_input();
        assert!(second.set_active(true));
        assert!(first.set_active(false)); // Blur releases only the first lease.
        assert!(first.set_active(false));
        drop(second); // Disposing a focused second control releases its lease.
        assert!(first.set_active(true));
        assert!(first.set_active(false)); // Submit/deactivation releases it.
        drop(first);
        SECURE_EVENTS.with_borrow(|events| {
            assert_eq!(
                *events,
                [
                    "enable", "enable", "disable", "disable", "enable", "disable"
                ]
            );
        });
    }

    #[test]
    fn failed_secure_enable_never_owns_or_disables_a_native_count() {
        SECURE_EVENTS.with_borrow_mut(Vec::clear);
        let mut guard = SecureEventInput {
            enable: mock_failed_enable,
            ..mock_secure_input()
        };
        assert!(!guard.set_active(true));
        assert!(!guard.enabled);
        drop(guard);
        SECURE_EVENTS.with_borrow(|events| assert_eq!(*events, ["failed_enable"]));
    }

    #[test]
    fn utf16_ranges_never_split_surrogates_or_utf8_scalars() {
        let text = "é😀中";
        assert_eq!(utf16_range_to_bytes(text, 1..3), 2..6);
        assert_eq!(utf16_range_to_bytes(text, 2..3), 2..6);
        assert_eq!(utf16_range_to_bytes(text, 2..2), 2..2);
        assert_eq!(utf16_range_to_bytes(text, 99..100), text.len()..text.len());
        assert_eq!(bytes_to_utf16(text, 6), 3);
    }

    #[test]
    fn grapheme_moves_and_deletion_keep_unicode_intact() {
        let mut buffer = SecretBuffer::default();
        assert!(buffer.replace(0..0, "a👩‍💻e\u{301}中"));
        let end = buffer.value.len();
        let before_last = buffer.previous(end);
        let before_accent = buffer.previous(before_last);
        assert_eq!(&buffer.value[before_accent..before_last], "e\u{301}");
        assert!(buffer.replace(before_accent..before_last, ""));
        assert_eq!(&*buffer.value, "a👩‍💻中");
        assert_eq!(buffer.next(1), "a👩‍💻".len());
    }

    #[test]
    fn ime_selection_is_relative_to_new_composition_after_unicode_prefix() {
        let mut buffer = SecretBuffer::default();
        assert!(buffer.replace(0..0, "é😀 tail"));
        buffer.move_to("é😀".len(), false);
        assert!(buffer.replace_marked(None, "漢😀字", Some(1..3)));
        assert_eq!(buffer.marked, Some(6..16));
        assert_eq!(&buffer.value[buffer.selected.clone()], "😀");
        assert_eq!(&*buffer.value, "é😀漢😀字 tail");
        assert!(buffer.replace_marked(None, "かな", Some(2..2)));
        assert_eq!(&*buffer.value, "é😀かな tail");
        let range = buffer.edit_range(None);
        assert!(buffer.replace(range, "仮名"));
        assert_eq!(&*buffer.value, "é😀仮名 tail");
        assert!(buffer.marked.is_none());
    }

    #[test]
    fn surrounding_text_is_only_a_utf16_length_preserving_mask() {
        let mut buffer = SecretBuffer::default();
        assert!(buffer.replace(0..0, "é😀中"));
        assert_eq!(buffer.masked_range(0..99), ("****".to_owned(), 0..4));
        assert_eq!(buffer.masked_range(2..3), ("**".to_owned(), 1..3));
        assert_eq!(buffer.masked_range(2..2), (String::new(), 1..1));
    }

    #[test]
    fn selection_reversal_keeps_its_anchor() {
        let mut buffer = SecretBuffer::default();
        assert!(buffer.replace(0..0, "abc中"));
        buffer.move_to(2, false);
        buffer.move_to(0, true);
        assert_eq!(buffer.selected, 0..2);
        assert!(buffer.reversed);
        buffer.move_to(buffer.value.len(), true);
        assert_eq!(buffer.selected, 2..6);
        assert!(!buffer.reversed);
    }

    #[test]
    fn take_and_clear_reset_all_secret_ownership_and_edit_state() {
        let mut buffer = SecretBuffer::default();
        assert!(buffer.replace_marked(None, "sample fixture", Some(0..6)));
        let address = buffer.value.as_ptr();
        let taken = buffer.take();
        assert_eq!(taken.as_ptr(), address, "take must move rather than clone");
        assert_eq!(&*taken, "sample fixture");
        assert!(buffer.value.is_empty());
        assert_eq!(buffer.selected, 0..0);
        assert!(!buffer.reversed);
        assert!(buffer.marked.is_none());
        assert!(buffer.replace(0..0, "next fixture"));
        drop(buffer.take());
        assert!(buffer.value.is_empty());
        assert!(buffer.marked.is_none());
    }

    #[test]
    fn owned_replacement_moves_allocation_and_rejects_invalid_edits_atomically() {
        let mut buffer = SecretBuffer::default();
        let value = Zeroizing::new("generated fixture words".to_owned());
        let address = value.as_ptr();
        assert!(buffer.replace_owned(value));
        assert_eq!(buffer.value.as_ptr(), address);
        assert!(!buffer.replace(0..0, "\n"));
        assert!(!buffer.replace(0..0, &"x".repeat(MAX_SECRET_BYTES)));
        assert!(!buffer.replace_owned(Zeroizing::new("invalid\rvalue".to_owned())));
        assert_eq!(&*buffer.value, "generated fixture words");
    }

    #[gpui::test]
    fn secure_control_only_publishes_masks_and_empties_on_take(cx: &mut gpui::TestAppContext) {
        let input = cx.new(|cx| SecretInput::new("Store password", cx));
        input.update(cx, |input, cx| {
            assert!(input.replace_secret(Zeroizing::new("é😀 fixture".to_owned()), cx));
            assert_eq!(&*input.masks, "**********");
            assert_eq!(input.len_chars(), 10);
            assert!(input.layout.is_none());
            let secret = input.take_secret(cx);
            assert_eq!(&*secret, "é😀 fixture");
            assert!(input.is_empty());
            assert!(input.masks.is_empty());
            assert!(input.buffer.marked.is_none());
            input.clear(cx);
            assert!(input.is_empty());
        });
    }
}
