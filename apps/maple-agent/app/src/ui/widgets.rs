//! Shared chrome for the gpui screens: popup panels, buttons, switches,
//! tooltips, card rows, input frames, and banners.
//!
//! Every helper returns a bare `div()` builder, so it costs the same as
//! the inline chain it replaces: no allocation, no parsing, nothing
//! cached. Callers keep their own ids, children, and click handlers.
//!
//! Shapes follow the brand kit: buttons are pills, small controls use
//! `theme::RADIUS_SM`, popups and inputs `theme::RADIUS_MD`, cards
//! `theme::RADIUS_LG`. Every clickable helper has a hover plate, a
//! pressed state, an accessible role, and a keyboard focus ring, so no
//! control is silent under the pointer, to a screen reader, or to Tab.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use gpui::{
    Action, Anchor, AnyElement, AnyView, App, BoxShadow, Div, ElementId, Hsla, Pixels, Point, Role,
    SharedString, Stateful, StyleRefinement, Window, anchored, div, prelude::*, px,
};

use super::icons::icon;
use super::theme;

/// How long an icon-only control waits before showing its tooltip. Icons
/// are their own label, so the hint should come quickly.
pub const ICON_TOOLTIP_DELAY: Duration = Duration::from_millis(300);

fn accent() -> Hsla {
    gpui::rgb(theme::accent()).into()
}

/// The keyboard focus ring: a 2 px coral halo drawn as a spread shadow,
/// so it never changes the element's size. Applied through
/// `focus_visible`, it shows only while navigating with the keyboard.
pub fn focus_ring(style: StyleRefinement) -> StyleRefinement {
    style.shadow(vec![
        BoxShadow::new(px(0.), px(0.), accent()).spread_radius(px(2.)),
    ])
}

/// The pressed state's depth: a faint inset shadow along the top edge,
/// the brand's "physical" press without any motion.
fn press_depth(style: StyleRefinement) -> StyleRefinement {
    style.shadow(vec![
        BoxShadow::new(px(0.), px(1.), gpui::hsla(0., 0., 0., 0.18))
            .blur_radius(px(2.))
            .inset(),
    ])
}

/// Floating panel behind a context menu or dropdown: an elevated surface
/// with a border and a shadow, stacked as a column. It blocks the pointer
/// from what lies beneath it.
///
/// Build menus with [`super::popup`], which places this panel and wires
/// its dismissal, keyboard, and rows.
pub fn popup_panel(id: impl Into<ElementId>, width: Pixels) -> Stateful<Div> {
    div()
        .id(id)
        .occlude()
        .role(Role::Menu)
        .w(width)
        .p_1()
        .rounded(theme::RADIUS_MD)
        .bg(gpui::rgb(theme::bg_elevated()))
        .border_1()
        .border_color(gpui::rgb(theme::border()))
        .shadow_md()
        .flex()
        .flex_col()
}

/// Shared pill shape for [`primary_button`], [`secondary_button`], and
/// [`ghost_button`]: the brand's button sizing (8 px vertical, 20 px
/// horizontal padding) with a medium-weight label, a button role, and
/// the focus ring for when the caller makes it a tab stop.
fn pill(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .role(Role::Button)
        .flex()
        .items_center()
        .justify_center()
        .gap_1p5()
        .px(theme::SPACE_MD)
        .py_2()
        .rounded_full()
        .text_sm()
        .font_weight(gpui::FontWeight::MEDIUM)
        .focus_visible(focus_ring)
}

/// The mark that says a control leaves the app for the browser. Goes after
/// the label inside any button or link that opens a URL, in that control's
/// text color.
pub fn external_mark(color: u32) -> gpui::Svg {
    icon("external-link", px(12.), color)
}

/// The mark for a control that navigates within the app (to another page or
/// section), as opposed to [`external_mark`].
pub fn navigate_mark(color: u32) -> gpui::Svg {
    icon("arrow-right", px(12.), color)
}

/// Accessible default/primary action button: high-contrast neutral inverted pill.
pub fn default_button(id: impl Into<ElementId>) -> Stateful<Div> {
    pill(id)
        .bg(gpui::rgb(theme::bg_default_button()))
        .text_color(gpui::rgb(theme::text_on_default_button()))
        .hover(|style| {
            style
                .bg(gpui::rgb(theme::bg_default_button_hover()))
                .cursor_pointer()
        })
        .active(|style| press_depth(style.bg(gpui::rgb(theme::bg_default_button_active()))))
}

/// Legacy accent-filled action button; new and Settings surfaces use
/// [`default_button`] for WCAG contrast until legacy callers migrate globally.
pub fn primary_button(id: impl Into<ElementId>) -> Stateful<Div> {
    pill(id)
        .bg(gpui::rgb(theme::accent()))
        .text_color(gpui::rgb(theme::on_accent()))
        .hover(|style| style.bg(gpui::rgb(theme::accent_hover())).cursor_pointer())
        .active(|style| press_depth(style.bg(gpui::rgb(theme::send_bottom()))))
}
/// Neutral pill for the second action beside a [`primary_button`]
/// (Cancel, Back, alternate sign-in): a pebble fill that darkens on hover.
pub fn secondary_button(id: impl Into<ElementId>) -> Stateful<Div> {
    pill(id)
        .bg(gpui::rgb(theme::bg_sidebar_pill()))
        .text_color(gpui::rgb(theme::text_primary()))
        .hover(|style| {
            style
                .bg(gpui::rgb(theme::bg_sidebar_row_selected()))
                .cursor_pointer()
        })
        .active(|style| press_depth(style.bg(gpui::rgb(theme::border()))))
}

/// A key named beside the control it works: one fixed height, so a row of
/// them shares a baseline with its labels whatever each one holds.
pub fn keycap(label: impl Into<gpui::SharedString>) -> Div {
    div()
        .flex_none()
        .h(px(18.))
        .min_w(px(18.))
        .px(px(5.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .border_1()
        .border_color(gpui::rgb(theme::border()))
        .bg(gpui::rgb(theme::bg_input()))
        .font_family(crate::assets::FONT_MONO)
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_size(px(10.5))
        .line_height(px(16.))
        .text_color(gpui::rgb(theme::text_muted()))
        .child(label.into())
}

/// Text-only button for low-emphasis actions: no fill, no border, and
/// the label brightens on hover.
pub fn ghost_button(id: impl Into<ElementId>) -> Stateful<Div> {
    pill(id)
        .text_color(gpui::rgb(theme::text_secondary()))
        .hover(|style| {
            style
                .text_color(gpui::rgb(theme::text_primary()))
                .bg(theme::overlay_hover())
                .cursor_pointer()
        })
        .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_pill())))
}

/// Destructive pill (Remove, Deny): error fill with a light label.
pub fn danger_button(id: impl Into<ElementId>) -> Stateful<Div> {
    pill(id)
        .bg(gpui::rgb(theme::status_error()))
        .text_color(gpui::rgb(theme::on_accent()))
        .hover(|style| style.opacity(0.9).cursor_pointer())
        .active(|style| press_depth(style.opacity(0.85)))
}

/// A destructive action at rest: outlined in the error color, filled only
/// on hover. For the row that *starts* a destructive flow; the filled
/// [`danger_button`] is for the step that commits it.
pub fn danger_outline_button(id: impl Into<ElementId>) -> Stateful<Div> {
    let error = theme::status_error();
    pill(id)
        .border_1()
        .border_color(gpui::rgb(error))
        .text_color(gpui::rgb(error))
        .hover(move |style| style.bg(gpui::rgba((error << 8) | 0x1f)).cursor_pointer())
        .active(move |style| press_depth(style.bg(gpui::rgba((error << 8) | 0x33))))
}

/// Square icon-only button with a hover plate, used in list rows and
/// toolbars. `label` is what the icon means: it names the control for
/// screen readers and shows as a quick tooltip, since the icon is the
/// only visible label.
pub fn icon_button(
    id: impl Into<ElementId>,
    name: &'static str,
    label: impl Into<SharedString>,
    size: Pixels,
    color: u32,
) -> Stateful<Div> {
    let label = label.into();
    icon_button_base(id, name, label.clone(), size, color).tooltip(tooltip(label, None))
}

/// [`icon_button`] for a control that fires `action`: its tooltip also
/// names the shortcut bound to it. gpui allows one tooltip per element,
/// so this is the way to add the hint, not a second `.tooltip`.
pub fn icon_button_for_action(
    id: impl Into<ElementId>,
    name: &'static str,
    label: impl Into<SharedString>,
    size: Pixels,
    color: u32,
    action: &dyn Action,
) -> Stateful<Div> {
    let label = label.into();
    icon_button_base(id, name, label.clone(), size, color)
        .tooltip(tooltip_for_action(label, action))
}

fn icon_button_base(
    id: impl Into<ElementId>,
    name: &'static str,
    label: impl Into<SharedString>,
    size: Pixels,
    color: u32,
) -> Stateful<Div> {
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label)
        .flex_none()
        .size_7()
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::RADIUS_SM)
        .focus_visible(focus_ring)
        .hover(|style| {
            style
                .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                .cursor_pointer()
        })
        .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_row_selected())))
        .tooltip_show_delay(ICON_TOOLTIP_DELAY)
        .child(icon(name, size, color))
}

/// On/off switch: a pill track with a knob that sits right when `on`.
/// The caller attaches `on_click`. The track is the brand accent when on
/// and the hairline colour when off, so state reads without a label.
pub fn switch(id: impl Into<ElementId>, on: bool) -> Stateful<Div> {
    switch_track(on)
        .id(id)
        .role(Role::Switch)
        .aria_toggled(if on {
            gpui::Toggled::True
        } else {
            gpui::Toggled::False
        })
        .focus_visible(focus_ring)
        .hover(|style| style.cursor_pointer().opacity(0.9))
        .active(|style| style.opacity(0.8))
}

/// A labeled checkbox row. The caller supplies its tab index and click/key
/// handlers, so mouse and keyboard activation share the same state change.
pub fn checkbox(
    id: impl Into<ElementId>,
    checked: bool,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    let label = label.into();
    div()
        .id(id)
        .role(Role::CheckBox)
        .aria_label(label.clone())
        .aria_toggled(if checked {
            gpui::Toggled::True
        } else {
            gpui::Toggled::False
        })
        .flex()
        .items_center()
        .gap_2()
        .py_1()
        .rounded(theme::RADIUS_SM)
        .text_sm()
        .text_color(gpui::rgb(theme::text_primary()))
        .focus_visible(focus_ring)
        .hover(|style| style.cursor_pointer())
        .active(|style| style.opacity(0.85))
        .child(
            div()
                .flex_none()
                .size(px(18.))
                .rounded(px(4.))
                .border_1()
                .border_color(gpui::rgb(if checked {
                    theme::accent()
                } else {
                    theme::text_muted()
                }))
                .bg(gpui::rgb(if checked {
                    theme::accent()
                } else {
                    theme::bg_input()
                }))
                .flex()
                .items_center()
                .justify_center()
                .when(checked, |square| {
                    square.child(icon("check", px(14.), theme::on_accent()))
                }),
        )
        .child(label)
}

/// The look of a [`switch`] without its role or input, for a row that is
/// itself the control, such as a switch item in a popup menu.
pub fn switch_track(on: bool) -> Div {
    div()
        .flex_none()
        .w(px(34.))
        .h(px(20.))
        .p(px(3.))
        .rounded_full()
        .bg(gpui::rgb(if on {
            theme::accent()
        } else {
            theme::bg_sidebar_row_selected()
        }))
        .flex()
        .items_center()
        .when(on, |track| track.justify_end())
        .child(
            div()
                .size(px(14.))
                .rounded_full()
                .bg(gpui::rgb(theme::on_accent()))
                .shadow_2xs(),
        )
}

/// Small status pill beside a card's title (an integration's availability,
/// a paired computer's update state). `color` tints only the label.
pub fn status_badge(label: impl Into<SharedString>, color: u32) -> Div {
    let label = label.into();
    div()
        .flex_none()
        .px_2()
        .py_0p5()
        .rounded_full()
        .bg(gpui::rgb(theme::bg_sidebar_pill()))
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(gpui::rgb(color))
        .child(label)
}

/// Card chrome for a settings-style row: padded elevated surface with a
/// subtle border.
pub fn card_row() -> Div {
    div()
        .p_4()
        .rounded(theme::RADIUS_LG)
        .bg(gpui::rgb(theme::bg_elevated()))
        .border_1()
        .border_color(gpui::rgb(theme::border_subtle()))
}

/// One card holding rows that belong together, split by hairlines: a
/// setting and its details, or a list. Each row brings its own layout; the
/// card supplies the padding and the lines.
pub fn card_stack(rows: impl IntoIterator<Item = AnyElement>) -> Div {
    let mut card = div()
        .flex()
        .flex_col()
        .rounded(theme::RADIUS_LG)
        .bg(gpui::rgb(theme::bg_elevated()))
        .border_1()
        .border_color(gpui::rgb(theme::border_subtle()));
    for (index, row) in rows.into_iter().enumerate() {
        card = card.child(
            div()
                .px_4()
                .py_3()
                .when(index > 0, |row| {
                    row.border_t_1().border_color(gpui::rgb(theme::border()))
                })
                .child(row),
        );
    }
    card
}

/// Frame around a [`super::text_input::TextInput`]. The explicit text
/// color matters: the input inherits the ambient color, which renders
/// near-black on the dark field without it. The border takes the accent
/// while the input inside has keyboard focus.
pub fn input_frame() -> Div {
    div()
        .px_3()
        .py_2()
        .rounded(theme::RADIUS_MD)
        .bg(gpui::rgb(theme::bg_input()))
        .border_1()
        .border_color(gpui::rgb(theme::border()))
        .in_focus(|style| style.border_color(gpui::rgb(theme::accent())))
        .text_color(gpui::rgb(theme::text_primary()))
}

/// What a [`banner`] reports. The tone picks the icon and the hue of its
/// tile; the card itself stays neutral.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Something failed or is blocked.
    Error,
    /// Something needs attention before it fails.
    Warning,
    /// Feedback without a severity of its own, such as a page that opened.
    Info,
}

impl Tone {
    fn icon(self) -> &'static str {
        match self {
            Tone::Error => "circle-alert",
            Tone::Warning => "triangle-alert",
            Tone::Info => "info",
        }
    }

    fn hue(self) -> u32 {
        match self {
            Tone::Error => theme::status_error(),
            Tone::Warning => theme::status_warning(),
            Tone::Info => theme::text_muted(),
        }
    }
}

/// Full-width status message: a neutral card with the tone's icon on a
/// tinted tile, then the message. Callers add any actions as further
/// children; they line up at the end. Status colors stay on the tile, so
/// the message keeps the body contrast of the surface it sits on.
pub fn banner(tone: Tone, message: impl IntoElement) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2p5()
        .px_3()
        .py_2p5()
        .rounded(theme::RADIUS_LG)
        .bg(gpui::rgb(theme::bg_elevated()))
        .border_1()
        .border_color(gpui::rgb(theme::border()))
        .text_sm()
        .text_color(gpui::rgb(theme::text_primary()))
        .child(tone_tile(tone))
        .child(div().flex_1().min_w_0().child(message))
}

/// The tone's icon on a small tile tinted with its hue, lighter at the
/// top on dark and deeper at the bottom on light, so it reads as lit
/// from above in both themes. Info stays a whisper.
fn tone_tile(tone: Tone) -> Div {
    let hue = tone.hue();
    let (top, bottom, edge) = match (tone, theme::is_light()) {
        (Tone::Info, false) => (0.14, 0.05, 0.22),
        (Tone::Info, true) => (0.06, 0.12, 0.2),
        (_, false) => (0.24, 0.08, 0.3),
        (_, true) => (0.07, 0.14, 0.26),
    };
    div()
        .flex_none()
        .size_7()
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::RADIUS_SM)
        .bg(gpui::linear_gradient(
            180.,
            gpui::linear_color_stop(tint(hue, top), 0.),
            gpui::linear_color_stop(tint(hue, bottom), 1.),
        ))
        .border_1()
        .border_color(tint(hue, edge))
        .child(icon(tone.icon(), px(15.), hue))
}

/// `color` at `alpha` opacity.
fn tint(color: u32, alpha: f32) -> Hsla {
    let mut color: Hsla = gpui::rgb(color).into();
    color.a = alpha;
    color
}

/// Transient notice: a soft surface with a leading icon and a close
/// button, for news the user can dismiss. A [`banner`] stays until its
/// cause is resolved.
pub fn notice(
    id: impl Into<ElementId>,
    text: SharedString,
    on_close: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .pl_3()
        .pr_1p5()
        .py_1p5()
        .rounded(theme::RADIUS_MD)
        .bg(gpui::rgb(theme::bg_elevated()))
        .border_1()
        .border_color(gpui::rgb(theme::border()))
        .shadow_sm()
        .text_sm()
        .text_color(gpui::rgb(theme::text_primary()))
        .child(icon("zap", ROW_ICON, theme::status_warning()))
        .child(div().flex_1().min_w_0().line_clamp(2).child(text))
        .child(
            icon_button(
                id,
                "x",
                super::i18n::tr("native-dismiss"),
                px(12.),
                theme::text_muted(),
            )
            .on_click(on_close),
        )
}

/// Icon size in a popup menu row or an [`icon_button`] in a list row.
pub const ROW_ICON: Pixels = px(14.);

// ---- Tooltips -------------------------------------------------------------

/// Small label shown above the pointer after gpui's hover delay, with an
/// optional fixed shortcut hint. Attach with `.tooltip(widgets::tooltip(..))`.
/// Prefer [`tooltip_for_action`] when the control fires an action: that
/// reads the live keymap instead of a literal that a rebind would outdate.
pub fn tooltip(
    label: impl Into<SharedString>,
    shortcut: Option<&'static str>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let label = label.into();
    move |window, cx| {
        let label = label.clone();
        cx.new(|_| Tooltip {
            label,
            detail: None,
            shortcut: shortcut.map(SharedString::from),
            pointer: window.mouse_position(),
        })
        .into()
    }
}

/// A tooltip whose shortcut hint is whatever `action` is bound to right
/// now, resolved when the tooltip shows. No hint when it is unbound.
pub fn tooltip_for_action(
    label: impl Into<SharedString>,
    action: &dyn Action,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let label = label.into();
    let action = action.boxed_clone();
    move |window, cx| {
        let label = label.clone();
        let shortcut = window
            .highest_precedence_binding_for_action(&*action)
            .map(|binding| SharedString::from(format_keystrokes(binding.keystrokes())));
        let pointer = window.mouse_position();
        cx.new(|_| Tooltip {
            label,
            detail: None,
            shortcut,
            pointer,
        })
        .into()
    }
}

/// A tooltip with a title and a short explanation under it, for a status
/// that needs a sentence rather than a name.
pub fn tooltip_detail(
    label: impl Into<SharedString>,
    detail: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let label = label.into();
    let detail = detail.into();
    move |window, cx| {
        let (label, detail) = (label.clone(), detail.clone());
        cx.new(|_| Tooltip {
            label,
            detail: Some(detail),
            shortcut: None,
            pointer: window.mouse_position(),
        })
        .into()
    }
}

/// Render a chord the way the platform writes it: symbols on macOS
/// (`⌘⇧N`), words elsewhere (`Ctrl+Shift+N`). Multi-stroke chords are
/// separated by a space.
pub fn format_keystrokes(keystrokes: &[gpui::KeybindingKeystroke]) -> String {
    keystrokes
        .iter()
        .map(format_keystroke)
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_keystroke(keystroke: &gpui::KeybindingKeystroke) -> String {
    let modifiers = keystroke.modifiers();
    let key = keystroke.key();
    let mut out = String::new();
    if cfg!(target_os = "macos") {
        if modifiers.control {
            out.push('⌃');
        }
        if modifiers.alt {
            out.push('⌥');
        }
        if modifiers.shift {
            out.push('⇧');
        }
        if modifiers.platform {
            out.push('⌘');
        }
        out.push_str(&key_glyph(key));
    } else {
        let mut parts = Vec::new();
        if modifiers.control {
            parts.push(super::i18n::tr("native-key-control").to_string());
        }
        if modifiers.alt {
            parts.push(super::i18n::tr("native-key-alt").to_string());
        }
        if modifiers.shift {
            parts.push(super::i18n::tr("native-key-shift").to_string());
        }
        if modifiers.platform {
            parts.push(super::i18n::tr("native-key-platform").to_string());
        }
        parts.push(key_glyph(key));
        out = parts.join("+");
    }
    out
}

fn key_glyph(key: &str) -> String {
    match key {
        "escape" => super::i18n::tr("native-key-escape").to_string(),
        "enter" => "↵".to_string(),
        "backspace" => "⌫".to_string(),
        "delete" => "⌦".to_string(),
        "tab" => "⇥".to_string(),
        "space" => super::i18n::tr("native-key-space").to_string(),
        "up" => "↑".to_string(),
        "down" => "↓".to_string(),
        "left" => "←".to_string(),
        "right" => "→".to_string(),
        "pageup" => super::i18n::tr("native-key-pageup").to_string(),
        "pagedown" => super::i18n::tr("native-key-pagedown").to_string(),
        "home" => super::i18n::tr("native-key-home").to_string(),
        "end" => super::i18n::tr("native-key-end").to_string(),
        key if key.chars().count() == 1 => key.to_uppercase(),
        key => key.to_string(),
    }
}

struct Tooltip {
    label: SharedString,
    /// A second line under the label, wrapped to a reading width.
    detail: Option<SharedString>,
    shortcut: Option<SharedString>,
    /// Where the pointer was when the tooltip showed, in window coordinates.
    pointer: Point<Pixels>,
}

/// How wide a tooltip's explanation runs before it wraps.
const TOOLTIP_DETAIL_WIDTH: Pixels = px(260.);

/// Space between the pointer and a tooltip above it.
const TOOLTIP_GAP: Pixels = px(16.);
/// Space below the pointer when a tooltip flips under it, enough to clear
/// the cursor's arrow.
const TOOLTIP_CURSOR_CLEARANCE: Pixels = px(20.);

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        // gpui puts a tooltip's corner just past the pointer, where the
        // cursor covers it. The view stays zero-sized there and anchors the
        // label at the pointer itself: centered, 16 px above it.
        // The transparent padding holds both gaps, and `anchored` fits the
        // padded box, so it flips below (`TOOLTIP_CURSOR_CLEARANCE` under
        // the pointer) when the whole box has no room above, and slides
        // inside the window at the sides. A hoverable tooltip must not
        // reuse this root: gpui keeps one shown while its root is hovered.
        let label = div()
            .debug_selector(|| "tooltip".into())
            .flex()
            .gap_2()
            .px_2()
            .py_1()
            .rounded(theme::RADIUS_SM)
            .bg(gpui::rgb(theme::bg_elevated()))
            .border_1()
            .border_color(gpui::rgb(theme::border()))
            .shadow_md()
            .text_xs()
            .text_color(gpui::rgb(theme::text_primary()))
            .map(|label| match self.detail.clone() {
                Some(detail) => label
                    .flex_col()
                    .gap_0p5()
                    .px_2p5()
                    .py_1p5()
                    .max_w(TOOLTIP_DETAIL_WIDTH)
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(self.label.clone()),
                    )
                    .child(
                        div()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(detail),
                    ),
                None => label.items_center().child(self.label.clone()),
            })
            .children(self.shortcut.clone().map(|keys| {
                div()
                    .px_1()
                    .rounded(px(4.))
                    .bg(gpui::rgb(theme::bg_sidebar_pill()))
                    .font_family(crate::assets::FONT_MONO)
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(keys)
            }));
        div().child(
            anchored()
                .position(self.pointer)
                .anchor(Anchor::BottomCenter)
                .child(
                    div()
                        .pt(TOOLTIP_CURSOR_CLEARANCE)
                        .pb(TOOLTIP_GAP)
                        .child(label),
                ),
        )
    }
}

// ---- Copy feedback --------------------------------------------------------

/// Redraw `view`, or the whole window when the button has no view.
fn repaint(view: Option<gpui::EntityId>, window: &mut Window, cx: &mut App) {
    match view {
        Some(view) => cx.notify(view),
        None => window.refresh(),
    }
}

/// How long a copy button reads "Copied".
const COPIED_FOR: Duration = Duration::from_millis(1400);

thread_local! {
    /// The copy button pressed most recently and when. One slot is enough:
    /// a second copy replaces the first, which is what the eye expects.
    static COPIED: RefCell<Option<(ElementId, Instant)>> = const { RefCell::new(None) };
}

fn copied_recently(id: &ElementId) -> bool {
    COPIED.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|(copied, at)| copied == id && at.elapsed() < COPIED_FOR)
    })
}

/// Small "Copy" button that writes `text` to the clipboard and reads
/// "Copied" with a check for a moment afterwards. The swap keeps the same
/// height, so a transcript row does not need re-measuring. The button
/// paints itself hidden until its `group` is hovered when `reveal` is set.
pub fn copy_button(
    id: impl Into<ElementId>,
    text: SharedString,
    reveal: Option<&SharedString>,
    view: Option<gpui::EntityId>,
) -> Stateful<Div> {
    let id = id.into();
    let copied = copied_recently(&id);
    let click_id = id.clone();
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(super::i18n::tr("common-copy"))
        .flex()
        .items_center()
        .gap_1()
        .px_1p5()
        .py_0p5()
        .rounded(theme::RADIUS_SM)
        .text_xs()
        .text_color(gpui::rgb(if copied {
            theme::status_success()
        } else {
            theme::text_muted()
        }))
        .when_some(reveal, |button, group| {
            button
                .opacity(if copied { 1. } else { 0. })
                .group_hover(group.clone(), |style| style.opacity(1.))
        })
        .focus_visible(focus_ring)
        .hover(|style| {
            style
                .bg(theme::overlay_hover())
                .text_color(gpui::rgb(theme::text_secondary()))
                .cursor_pointer()
        })
        .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_pill())))
        .on_click(move |_event, window, cx: &mut App| {
            cx.stop_propagation();
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_string()));
            COPIED.with(|slot| *slot.borrow_mut() = Some((click_id.clone(), Instant::now())));
            // Repaint the view that owns the button; a whole-window refresh
            // would throw away every cached view in it.
            repaint(view, window, cx);
            // One more paint after the label reverts; no polling between.
            window
                .spawn(cx, async move |cx| {
                    cx.background_executor().timer(COPIED_FOR).await;
                    cx.update(|window, cx| repaint(view, window, cx)).ok();
                })
                .detach();
        })
        .child(icon(
            if copied { "check" } else { "copy" },
            px(12.),
            if copied {
                theme::status_success()
            } else {
                theme::text_secondary()
            },
        ))
        .child(super::i18n::tr(if copied {
            "common-copied"
        } else {
            "common-copy"
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, Modifiers, TestAppContext, VisualTestContext, size};

    const WINDOW: gpui::Size<Pixels> = size(px(400.), px(300.));

    /// Buttons against the top, left, and right edges of the window, one
    /// well inside it, and one with a two-line label and a shortcut.
    struct Host;

    impl Render for Host {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            let button = |id: &'static str, label: &'static str, shortcut| {
                div()
                    .id(id)
                    .debug_selector(move || id.into())
                    .absolute()
                    .size_6()
                    .tooltip(tooltip(label, shortcut))
            };
            div()
                .size_full()
                .child(button("top", "Settings", None).top_0().left(px(100.)))
                .child(
                    button("middle", "Settings", None)
                        .top(px(200.))
                        .left(px(100.)),
                )
                .child(
                    button("left", "Toggle sidebar", None)
                        .top(px(200.))
                        .left_0(),
                )
                .child(
                    button("right", "Toggle sidebar", None)
                        .top(px(200.))
                        .right_0(),
                )
                .child(
                    button("multiline", "Queue message\nafter this run", Some("↵"))
                        .top(px(200.))
                        .left(px(200.)),
                )
        }
    }

    fn setup(cx: &mut TestAppContext) -> &mut VisualTestContext {
        let (_, cx) = cx.add_window_view(|_, _| Host);
        cx.simulate_resize(WINDOW);
        cx.run_until_parked();
        cx
    }

    /// Hover the middle of `selector` until its tooltip shows. Returns the
    /// tooltip's bounds and the pointer, after checking that the tooltip
    /// lies inside the window.
    fn hover(
        cx: &mut VisualTestContext,
        selector: &'static str,
    ) -> (Bounds<Pixels>, Point<Pixels>) {
        let pointer = cx.debug_bounds(selector).expect("button").center();
        cx.simulate_mouse_move(pointer, None, Modifiers::default());
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        let tooltip = cx.debug_bounds("tooltip").expect("tooltip");
        assert!(
            tooltip.left() >= px(0.)
                && tooltip.top() >= px(0.)
                && tooltip.right() <= WINDOW.width
                && tooltip.bottom() <= WINDOW.height,
            "the tooltip {tooltip:?} lies inside the window"
        );
        (tooltip, pointer)
    }

    /// Whether two positions match to within gpui's whole-pixel rounding.
    fn near(a: Pixels, b: Pixels) -> bool {
        (a - b).abs() <= px(0.5)
    }

    #[gpui::test]
    fn a_tooltip_sits_centered_above_the_pointer(cx: &mut TestAppContext) {
        let cx = setup(cx);
        for selector in ["middle", "multiline"] {
            let (tooltip, pointer) = hover(cx, selector);
            assert!(
                near(tooltip.bottom(), pointer.y - px(16.)),
                "{selector}: the tooltip {tooltip:?} is the gap above the pointer {pointer:?}"
            );
            assert!(
                near(tooltip.center().x, pointer.x),
                "{selector}: the tooltip {tooltip:?} is centered on the pointer {pointer:?}"
            );
        }
    }

    #[gpui::test]
    fn without_room_above_a_tooltip_flips_below_the_cursor(cx: &mut TestAppContext) {
        let cx = setup(cx);
        let (tooltip, pointer) = hover(cx, "top");
        assert!(
            near(tooltip.top(), pointer.y + TOOLTIP_CURSOR_CLEARANCE),
            "the tooltip {tooltip:?} is the clearance below the pointer {pointer:?}"
        );
    }

    #[gpui::test]
    fn at_the_sides_a_tooltip_stays_above_the_pointer_inside_the_window(cx: &mut TestAppContext) {
        let cx = setup(cx);
        for selector in ["left", "right"] {
            let (tooltip, pointer) = hover(cx, selector);
            assert!(
                near(tooltip.bottom(), pointer.y - px(16.)),
                "{selector}: the tooltip {tooltip:?} is the gap above the pointer {pointer:?}"
            );
        }
    }
}
