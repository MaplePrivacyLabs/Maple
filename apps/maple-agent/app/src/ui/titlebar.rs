//! Custom window title bar, drawn only when the system draws none (see
//! `super::decorations`). That is the case on GNOME, whose compositor
//! leaves every window undecorated: a drag region with a centered title
//! plus round minimize, maximize, and close controls.

use gpui::{Context, Pixels, SharedString, Window, div, prelude::*, px};

use super::decorations::system_draws_titlebar;
use super::theme;

/// Window title. The system bar and the app's own bar show the same text,
/// and only one of them is ever on screen (see `super::decorations`).
pub const WINDOW_TITLE: &str = crate::profile::DISPLAY_NAME;

/// On macOS the system title bar is transparent and the app's own top
/// row sits under it, so the window controls float over the content.
pub const TRANSPARENT_TITLEBAR: bool = cfg!(target_os = "macos");

/// Where the traffic lights sit when the bar is transparent.
pub const TRAFFIC_LIGHT_POSITION: gpui::Point<Pixels> = gpui::point(px(12.), px(12.));

/// Horizontal room the traffic lights need at the left of the top row.
pub const TRAFFIC_LIGHT_INSET: Pixels = px(78.);

/// Vertical room the traffic lights take at the top of a column, for a
/// row that sits under them instead of beside them.
pub const TRAFFIC_LIGHT_ROW: Pixels = px(36.);

/// Left padding for a top row: clears the traffic lights when the bar is
/// transparent, otherwise the ordinary gutter.
pub fn top_row_inset(gutter: Pixels) -> Pixels {
    if TRANSPARENT_TITLEBAR {
        TRAFFIC_LIGHT_INSET
    } else {
        gutter
    }
}

/// Top padding for a row placed under the traffic lights: clears them
/// when the bar is transparent, otherwise the ordinary gutter.
pub fn top_row_top(gutter: Pixels) -> Pixels {
    if TRANSPARENT_TITLEBAR {
        TRAFFIC_LIGHT_ROW
    } else {
        gutter
    }
}

/// Make an element behave like the title bar it is standing in for: a
/// press on its empty area drags the window and a double press zooms it,
/// the way AppKit's bar does. Interactive children stop the press with
/// `cx.stop_propagation()` so a click on them does not start a drag.
pub fn drag_region<E>(element: E) -> E
where
    E: gpui::InteractiveElement + gpui::StatefulInteractiveElement,
{
    if !TRANSPARENT_TITLEBAR {
        return element;
    }
    element
        .window_control_area(gpui::WindowControlArea::Drag)
        .on_mouse_down(gpui::MouseButton::Left, |event, window, _cx| {
            if event.click_count >= 2 {
                #[cfg(target_os = "macos")]
                window.titlebar_double_click();
            } else {
                window.start_window_move();
            }
        })
}

/// An invisible strip along the top edge for screens with no top row of
/// their own (login, the restoring placeholder), so the window still
/// drags there.
pub fn drag_strip() -> gpui::Stateful<gpui::Div> {
    drag_region(
        div()
            .id("titlebar-drag-strip")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(BAR_HEIGHT),
    )
}

const BAR_HEIGHT: Pixels = px(40.);
const CONTROL_SIZE: Pixels = px(24.);

pub struct TitleBar {
    title: SharedString,
}

impl TitleBar {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
        }
    }

    /// App version for display, from the workspace manifest.
    pub fn version() -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
}

impl Render for TitleBar {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        if system_draws_titlebar(window) {
            // The window already has a title bar. A second one is the bug
            // this check exists to prevent.
            return div().into_any_element();
        }

        let controls = window.window_controls();
        div()
            .id("title-bar")
            .relative()
            .flex()
            .items_center()
            .justify_end()
            .w_full()
            .h(BAR_HEIGHT)
            .pr_3()
            .bg(gpui::rgb(theme::bg_sidebar()))
            // Any press in the bar that is not on a control starts a window
            // drag, which is the standard title-bar behavior. A double
            // press toggles maximize instead.
            .on_mouse_down(gpui::MouseButton::Left, |event, window, _| {
                if event.click_count >= 2 {
                    window.zoom_window();
                } else {
                    window.start_window_move();
                }
            })
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(gpui::rgb(theme::text_primary()))
                    .child(self.title.clone()),
            )
            .child(
                div()
                    .relative()
                    .flex()
                    .items_center()
                    .gap_2()
                    .children(controls.minimize.then(|| {
                        control("title-minimize", "—", |window, _cx| {
                            window.minimize_window();
                        })
                    }))
                    .children(controls.maximize.then(|| {
                        control("title-maximize", "▢", |window, _cx| {
                            window.zoom_window();
                        })
                    }))
                    .child(control("title-close", "✕", |_window, cx| {
                        cx.quit();
                    })),
            )
            .into_any_element()
    }
}

fn control(
    id: &'static str,
    label: &'static str,
    action: impl Fn(&mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(CONTROL_SIZE)
        .rounded_full()
        .bg(gpui::rgb(theme::bg_title_control()))
        .text_xs()
        .text_color(gpui::rgb(theme::text_primary()))
        .hover(|style| {
            style
                .bg(gpui::rgb(theme::bg_title_control_hover()))
                .cursor_pointer()
        })
        .on_mouse_down(gpui::MouseButton::Left, {
            let action = std::rc::Rc::new(action);
            move |_event, window, cx| {
                // Stop the press from also starting a window drag.
                cx.stop_propagation();
                action(window, cx);
            }
        })
        .child(label.to_string())
}
