//! Shared chrome for the gpui screens: popup panels, buttons, switches,
//! tooltips, card rows, input frames, and banners.
//!
//! Helpers preserve the div builder API and callers keep their ids, children,
//! and click handlers. Hover backgrounds share a short reversible transition.
//!
//! Shapes follow the brand kit: buttons are pills, small controls use
//! `theme::RADIUS_SM`, popups and inputs `theme::RADIUS_MD`, cards
//! `theme::RADIUS_LG`. Every clickable helper has a hover plate, a
//! pressed state, an accessible role, and a keyboard focus ring, so no
//! control is silent under the pointer, to a screen reader, or to Tab.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use gpui::{
    Action, Anchor, AnyView, App, BoxShadow, Div, ElementId, Hsla, Pixels, Point, Role,
    SharedString, Stateful, StyleRefinement, Window, anchored, div, prelude::*, px,
};

use super::icons::icon;
use super::motion::{HoverBackground, hover_background};
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

/// Accent-filled action button: coral pill with a light label.
pub fn primary_button(id: impl Into<ElementId>) -> HoverBackground {
    hover_background(
        pill(id)
            .bg(gpui::rgb(theme::accent()))
            .text_color(gpui::rgb(theme::on_accent()))
            .hover(|style| style.cursor_pointer())
            .active(|style| press_depth(style.bg(gpui::rgb(theme::send_bottom())))),
        gpui::rgb(theme::accent()),
        gpui::rgb(theme::accent_hover()),
    )
}

/// Neutral pill for the second action beside a [`primary_button`]
/// (Cancel, Back, alternate sign-in): a pebble fill that darkens on hover.
pub fn secondary_button(id: impl Into<ElementId>) -> HoverBackground {
    hover_background(
        pill(id)
            .bg(gpui::rgb(theme::bg_sidebar_pill()))
            .text_color(gpui::rgb(theme::text_primary()))
            .hover(|style| style.cursor_pointer())
            .active(|style| press_depth(style.bg(gpui::rgb(theme::border())))),
        gpui::rgb(theme::bg_sidebar_pill()),
        gpui::rgb(theme::bg_sidebar_row_selected()),
    )
}

/// Text-only button for low-emphasis actions: no fill, no border, and
/// the label brightens on hover.
pub fn ghost_button(id: impl Into<ElementId>) -> HoverBackground {
    hover_background(
        pill(id)
            .text_color(gpui::rgb(theme::text_secondary()))
            .hover(|style| {
                style
                    .text_color(gpui::rgb(theme::text_primary()))
                    .cursor_pointer()
            })
            .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_pill()))),
        gpui::transparent_black(),
        theme::overlay_hover(),
    )
}

/// Destructive pill (Remove, Deny): error fill with a light label.
pub fn danger_button(id: impl Into<ElementId>) -> HoverBackground {
    let error: Hsla = gpui::rgb(theme::status_error()).into();
    hover_background(
        pill(id)
            .text_color(gpui::rgb(theme::on_accent()))
            .hover(|style| style.cursor_pointer())
            .active(|style| press_depth(style.opacity(0.85))),
        error,
        gpui::hsla(error.h, error.s, error.l, 0.9),
    )
}

/// Square icon-only button with a hover plate, used in list rows and
/// toolbars. `label` is what the icon means: it names the control for
/// screen readers and shows as a quick tooltip, since the icon is the
/// only visible label.
pub fn icon_button(
    id: impl Into<ElementId>,
    name: &'static str,
    label: &'static str,
    size: Pixels,
    color: u32,
) -> HoverBackground {
    icon_button_base(id, name, label, size, color).tooltip(tooltip(label, None))
}

/// [`icon_button`] for a control that fires `action`: its tooltip also
/// names the shortcut bound to it. gpui allows one tooltip per element,
/// so this is the way to add the hint, not a second `.tooltip`.
pub fn icon_button_for_action(
    id: impl Into<ElementId>,
    name: &'static str,
    label: &'static str,
    size: Pixels,
    color: u32,
    action: &dyn Action,
) -> HoverBackground {
    icon_button_base(id, name, label, size, color).tooltip(tooltip_for_action(label, action))
}

fn icon_button_base(
    id: impl Into<ElementId>,
    name: &'static str,
    label: &'static str,
    size: Pixels,
    color: u32,
) -> HoverBackground {
    hover_background(
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
            .hover(|style| style.cursor_pointer())
            .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_row_selected())))
            .tooltip_show_delay(ICON_TOOLTIP_DELAY)
            .child(icon(name, size, color)),
        gpui::transparent_black(),
        gpui::rgb(theme::bg_sidebar_row_hover()),
    )
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
pub fn checkbox(id: impl Into<ElementId>, checked: bool, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .role(Role::CheckBox)
        .aria_label(label)
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

/// Full-width status message on a solid fill, such as
/// `theme::status_error()` or `theme::status_warning()`. The label takes
/// the on-accent white, which reads on every status fill.
pub fn banner(background: u32) -> Div {
    div()
        .px_3()
        .py_2()
        .rounded(theme::RADIUS_MD)
        .bg(gpui::rgb(background))
        .text_color(gpui::rgb(theme::on_accent()))
        .text_sm()
}

/// Transient notice: a soft surface with a leading icon and a close
/// button. Softer than [`banner`], which is for errors that block.
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
        .child(icon_button(id, "x", "Dismiss", px(12.), theme::text_muted()).on_click(on_close))
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
            shortcut,
            pointer,
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
            parts.push("Ctrl".to_string());
        }
        if modifiers.alt {
            parts.push("Alt".to_string());
        }
        if modifiers.shift {
            parts.push("Shift".to_string());
        }
        if modifiers.platform {
            parts.push("Win".to_string());
        }
        parts.push(key_glyph(key));
        out = parts.join("+");
    }
    out
}

fn key_glyph(key: &str) -> String {
    match key {
        "escape" => "Esc".to_string(),
        "enter" => "↵".to_string(),
        "backspace" => "⌫".to_string(),
        "delete" => "⌦".to_string(),
        "tab" => "⇥".to_string(),
        "space" => "Space".to_string(),
        "up" => "↑".to_string(),
        "down" => "↓".to_string(),
        "left" => "←".to_string(),
        "right" => "→".to_string(),
        "pageup" => "PgUp".to_string(),
        "pagedown" => "PgDn".to_string(),
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        key if key.chars().count() == 1 => key.to_uppercase(),
        key => key.to_string(),
    }
}

struct Tooltip {
    label: SharedString,
    shortcut: Option<SharedString>,
    /// Where the pointer was when the tooltip showed, in window coordinates.
    pointer: Point<Pixels>,
}

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
            .items_center()
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
            .child(self.label.clone())
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
) -> super::motion::HoverBackground {
    let id = id.into();
    let copied = copied_recently(&id);
    let click_id = id.clone();
    div()
        .id(id)
        .role(Role::Button)
        .aria_label("Copy")
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
        .child(if copied { "Copied" } else { "Copy" })
        .map(|button| {
            super::motion::hover_background(
                button,
                gpui::transparent_black(),
                gpui::rgb(theme::bg_sidebar_row_hover()),
            )
        })
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
