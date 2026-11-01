//! Bounded Linux updater confirmation. The platform's fallback prompt does not
//! constrain its detail text; keep this dialog in the owning desktop instead.

use gpui::{App, Context, Entity, FocusHandle, Render, SharedString, Window, div, prelude::*, px};
use tokio::sync::oneshot;

use super::{
    i18n::{tr, tr_args},
    theme, widgets,
};

pub(crate) struct UpdateConfirmation {
    title: SharedString,
    answer: Option<oneshot::Sender<usize>>,
    previous_focus: Option<FocusHandle>,
    later_focus: FocusHandle,
    restart_focus: FocusHandle,
}

pub(crate) fn open(
    version: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) -> (Entity<UpdateConfirmation>, oneshot::Receiver<usize>) {
    let (sender, receiver) = oneshot::channel();
    let previous_focus = window.focused(cx);
    let view = cx.new(|cx| UpdateConfirmation {
        title: version.map_or_else(
            || tr("update-confirmation-title"),
            |version| {
                tr_args(
                    "update-confirmation-title-version",
                    [("version", version.into())],
                )
            },
        ),
        answer: Some(sender),
        previous_focus,
        later_focus: cx.focus_handle(),
        restart_focus: cx.focus_handle(),
    });
    let later_focus = view.read(cx).later_focus.clone();
    window.focus(&later_focus, cx);
    (view, receiver)
}

impl UpdateConfirmation {
    fn respond(&mut self, answer: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sender) = self.answer.take() {
            let _ = sender.send(answer);
            if let Some(previous) = &self.previous_focus {
                window.focus(previous, cx);
            }
            cx.notify();
        }
    }
}

impl Render for UpdateConfirmation {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.answer.is_none() {
            return div().into_any_element();
        }
        let width = (window.viewport_size().width - px(48.)).clamp(px(0.), px(480.));
        let height = (window.viewport_size().height - px(48.)).max(px(0.));
        div()
            .id("update-confirmation-backdrop")
            .absolute()
            .inset_0()
            .size_full()
            .occlude()
            .bg(theme::scrim())
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("update-confirmation")
                    .debug_selector(|| "update-confirmation".to_string())
                    .role(gpui::Role::Dialog)
                    .aria_label(self.title.clone())
                    .key_context("Dialog")
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                        match event.keystroke.key.as_str() {
                            "escape" => this.respond(0, window, cx),
                            "tab" => {
                                let next = if this.restart_focus.is_focused(window) {
                                    &this.later_focus
                                } else {
                                    &this.restart_focus
                                };
                                window.focus(next, cx);
                            }
                            "enter" | "space" | " " if !event.keystroke.modifiers.modified() => {
                                this.respond(
                                    usize::from(this.restart_focus.is_focused(window)),
                                    window,
                                    cx,
                                );
                            }
                            _ => return,
                        }
                        cx.stop_propagation();
                    }))
                    .w(width)
                    .max_h(height)
                    .p_5()
                    .rounded(theme::RADIUS_XL)
                    .shadow_lg()
                    .bg(gpui::rgb(theme::bg_elevated()))
                    .border_1()
                    .border_color(gpui::rgb(theme::border()))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .text_color(gpui::rgb(theme::text_primary()))
                    .child(
                        div()
                            .w_full()
                            .flex_shrink_0()
                            .text_lg()
                            .child(self.title.clone()),
                    )
                    .child(
                        div()
                            .id("update-confirmation-body")
                            .debug_selector(|| "update-confirmation-body".to_string())
                            .w_full()
                            .min_w_0()
                            .min_h_0()
                            .overflow_y_scroll()
                            .text_sm()
                            .child(tr("update-confirmation-detail")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .flex_wrap()
                            .justify_end()
                            .gap_2()
                            .child(
                                widgets::secondary_button("update-confirmation-later")
                                    .debug_selector(|| "update-confirmation-later".to_string())
                                    .track_focus(&self.later_focus)
                                    .focus_visible(widgets::focus_ring)
                                    .child(tr("update-confirmation-later"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.respond(0, window, cx)
                                    })),
                            )
                            .child(
                                widgets::primary_button("update-confirmation-restart")
                                    .debug_selector(|| "update-confirmation-restart".to_string())
                                    .track_focus(&self.restart_focus)
                                    .focus_visible(widgets::focus_ring)
                                    .child(tr("update-confirmation-restart"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.respond(1, window, cx)
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};

    struct PromptHost {
        prompt: Option<Entity<UpdateConfirmation>>,
    }

    impl Render for PromptHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().relative().size_full().children(self.prompt.clone())
        }
    }

    fn show(
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<PromptHost>,
        oneshot::Receiver<usize>,
        &mut gpui::VisualTestContext,
    ) {
        let answer = Rc::new(RefCell::new(None));
        let result = answer.clone();
        let (host, cx) = cx.add_window_view(move |window, cx| {
            let (prompt, receiver) = open(Some("0.0.14"), window, cx);
            assert_eq!(
                prompt.read(cx).title.as_ref(),
                "Restart Maple to install v\u{2068}0.0.14\u{2069}?"
            );
            *result.borrow_mut() = Some(receiver);
            PromptHost {
                prompt: Some(prompt),
            }
        });
        let receiver = answer.borrow_mut().take().unwrap();
        (host, receiver, cx)
    }

    #[gpui::test]
    fn updater_confirmation_without_version_uses_generic_title(cx: &mut gpui::TestAppContext) {
        cx.add_window_view(|window, cx| {
            let (prompt, _answer) = open(None, window, cx);
            assert_eq!(prompt.read(cx).title.as_ref(), "Restart Maple to update?");
            PromptHost {
                prompt: Some(prompt),
            }
        });
    }

    #[gpui::test]
    fn updater_confirmation_wraps_and_keeps_actions_inside_the_viewport(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_host, _answer, cx) = show(cx);
        for (width, height) in [(1213., 800.), (640., 600.), (320., 480.), (640., 220.)] {
            cx.simulate_resize(gpui::size(px(width), px(height)));
            let card = cx.debug_bounds("update-confirmation").unwrap();
            let body = cx.debug_bounds("update-confirmation-body").unwrap();
            let later = cx.debug_bounds("update-confirmation-later").unwrap();
            let restart = cx.debug_bounds("update-confirmation-restart").unwrap();
            assert!(
                card.left() >= px(0.) && card.right() <= px(width),
                "{card:?}"
            );
            assert!(
                card.top() >= px(0.) && card.bottom() <= px(height),
                "{card:?}"
            );
            assert!(
                body.size.height > px(25.),
                "the complete warning must wrap: {body:?}"
            );
            assert!(
                body.left() >= card.left() && body.right() <= card.right(),
                "{body:?}"
            );
            for action in [later, restart] {
                assert!(
                    action.left() >= card.left() && action.right() <= card.right(),
                    "{action:?}"
                );
                assert!(
                    action.top() >= body.bottom() && action.bottom() <= card.bottom(),
                    "{action:?}"
                );
            }
        }
    }

    #[gpui::test]
    fn updater_confirmation_defaults_to_later_and_escape_cancels(cx: &mut gpui::TestAppContext) {
        for key in ["enter", "escape"] {
            let (_host, mut answer, cx) = show(cx);
            cx.simulate_keystrokes(key);
            assert_eq!(answer.try_recv().unwrap(), 0);
        }
    }

    #[gpui::test]
    fn updater_confirmation_requires_selecting_restart(cx: &mut gpui::TestAppContext) {
        let (_host, mut answer, cx) = show(cx);
        cx.simulate_click(gpui::point(px(8.), px(8.)), gpui::Modifiers::default());
        assert_eq!(
            answer.try_recv(),
            Err(oneshot::error::TryRecvError::Empty),
            "the backdrop must not confirm"
        );
        cx.simulate_keystrokes("tab enter");
        assert_eq!(answer.try_recv().unwrap(), 1);
    }

    #[gpui::test]
    fn dropping_the_updater_confirmation_cancels_without_an_answer(cx: &mut gpui::TestAppContext) {
        let (host, mut answer, cx) = show(cx);
        host.update(cx, |host, cx| {
            host.prompt = None;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(answer.try_recv(), Err(oneshot::error::TryRecvError::Closed));
    }
}
