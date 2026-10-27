//! Explicitly submitted feedback. Drafts never enter chat history.
use super::ChatScreen;
use crate::feedback::{self, Submission};
use crate::ui::{text_input::TextInput, theme, widgets};
use gpui::{Context, Div, Entity, Focusable, Window, div, prelude::*, px};

pub(super) struct FeedbackForm {
    input: Entity<TextInput>,
    category: &'static str,
    request: Option<Submission>,
    busy: bool,
    error: Option<String>,
    focus_pending: bool,
    controls: Vec<gpui::FocusHandle>,
}
impl ChatScreen {
    pub(super) fn open_feedback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.feedback.is_some() {
            return;
        }
        self.popup.close(cx);
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.close_popups(cx));
        let owner = cx.entity().downgrade();
        let input = cx.new(|cx| {
            TextInput::new("What would you like us to know?", cx)
                .multiline(8)
                .with_tab_index(0)
                .on_key(move |event, _, window, cx| {
                    if !matches!(event.keystroke.key.as_str(), "escape" | "tab") {
                        return false;
                    }
                    let owner = owner.clone();
                    let event = event.clone();
                    window.defer(cx, move |window, cx| {
                        owner
                            .update(cx, |this, cx| {
                                if event.keystroke.key == "escape" {
                                    this.close_feedback(cx);
                                } else {
                                    this.feedback_tab(event.keystroke.modifiers.shift, window, cx);
                                }
                            })
                            .ok();
                    });
                    true
                })
        });
        cx.observe(&input, |_, _, cx| cx.notify()).detach();
        window.focus(&input.read(cx).focus_handle(cx), cx);
        let mut controls = vec![input.read(cx).focus_handle(cx)];
        controls.extend((1..6).map(|index| cx.focus_handle().tab_index(index).tab_stop(true)));
        self.feedback = Some(FeedbackForm {
            input,
            category: "other",
            request: None,
            busy: false,
            error: None,
            focus_pending: true,
            controls,
        });
        cx.notify();
    }
    pub(super) fn close_feedback(&mut self, cx: &mut Context<Self>) {
        if self.feedback.as_ref().is_some_and(|f| f.busy) {
            return;
        }
        self.feedback = None;
        self.screen_focus_pending = true;
        cx.notify();
    }
    pub(super) fn focus_feedback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(form) = self.feedback.as_mut()
            && form.focus_pending
        {
            form.focus_pending = false;
            window.focus(&form.input.read(cx).focus_handle(cx), cx);
        }
    }
    fn feedback_tab(&self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.feedback.as_ref().filter(|form| !form.busy) else {
            return;
        };
        let current = window.focused(cx);
        let index = form
            .controls
            .iter()
            .position(|handle| Some(handle) == current.as_ref())
            .unwrap_or(0);
        let next = if backwards {
            (index + form.controls.len() - 1) % form.controls.len()
        } else {
            (index + 1) % form.controls.len()
        };
        window.focus(&form.controls[next], cx);
    }
    fn submit_feedback(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.feedback.as_mut() else {
            return;
        };
        if form.busy {
            return;
        }
        let candidate = match Submission::new(form.category, &form.input.read(cx).text()) {
            Ok(candidate) => candidate,
            Err(error) => {
                form.error = Some(error);
                form.focus_pending = true;
                cx.notify();
                return;
            }
        };
        let request = form
            .request
            .as_ref()
            .filter(|old| old.message == candidate.message && old.category == candidate.category)
            .cloned()
            .unwrap_or(candidate);
        form.request = Some(request.clone());
        form.busy = true;
        form.error = None;
        self.dialog_focus.get_or_insert_with(|| cx.focus_handle());
        self.dialog_focus_pending = true;
        cx.notify();
        self.call(
            async move { feedback::submit(&request).await },
            cx,
            |this, result, cx| {
                if let Some(form) = this.feedback.as_mut() {
                    form.busy = false;
                    match result {
                        Ok(()) => {
                            this.feedback = None;
                            this.notice = Some("Thanks for your feedback!".into());
                            this.screen_focus_pending = true;
                        }
                        Err(error) => {
                            form.error = Some(error);
                            form.focus_pending = true;
                        }
                    }
                }
                cx.notify();
            },
        );
    }
    pub(super) fn render_feedback(&self, cx: &mut Context<Self>) -> Option<gpui::Stateful<Div>> {
        let form = self.feedback.as_ref()?;
        let busy = form.busy;
        let mut categories = div().flex().gap_2();
        for (index, (value, label)) in [("bug", "Bug"), ("idea", "Idea"), ("other", "Other")]
            .into_iter()
            .enumerate()
        {
            categories = categories.child(
                widgets::secondary_button(value)
                    .aria_label(label)
                    .track_focus(&form.controls[index + 1])
                    .text_sm()
                    .py_1()
                    .when(form.category == value, |b| {
                        b.bg(gpui::rgb(theme::bg_sidebar_row_selected()))
                    })
                    .child(label)
                    .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            if let Some(form) = this.feedback.as_mut().filter(|f| !f.busy) {
                                form.category = value;
                                cx.notify();
                            }
                            cx.stop_propagation();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(form) = this.feedback.as_mut().filter(|f| !f.busy) {
                            form.category = value;
                            cx.notify();
                        }
                    })),
            );
        }
        let length = form.input.read(cx).text_ref().encode_utf16().count();
        Some(div().id("feedback-backdrop").absolute().inset_0().occlude()
            .bg(theme::scrim()).flex().items_center().justify_center()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(div().id("feedback-form").role(gpui::Role::Dialog).aria_label("Send feedback")
                .key_context("Dialog")
                .when_some(self.dialog_focus.clone(), |card, focus| card.track_focus(&focus))
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                    match event.keystroke.key.as_str() {
                        "escape" => { this.close_feedback(cx); cx.stop_propagation(); }
                        "tab" => { this.feedback_tab(event.keystroke.modifiers.shift, window, cx); cx.stop_propagation(); }
                        _ => {}
                    }
                }))
                .w(px(460.)).max_w(gpui::relative(0.95)).max_h(gpui::relative(0.9))
                .overflow_y_scroll().p_5().rounded(theme::RADIUS_XL).shadow_lg()
                .bg(gpui::rgb(theme::bg_elevated())).border_1().border_color(gpui::rgb(theme::border()))
                .flex().flex_col().gap_3().text_color(gpui::rgb(theme::text_primary()))
                .child(div().text_lg().child("Send feedback"))
                .child(categories)
                .child(widgets::input_frame().min_h(px(120.)).child(
                    if busy { div().text_sm().child("Sending your feedback…").into_any_element() }
                    else { form.input.clone().into_any_element() }
                ))
                .child(div().text_xs().text_color(gpui::rgb(theme::text_secondary())).child(format!("{length} / 5,000 · Shift+Enter for a new line")))
                .child(div().text_xs().text_color(gpui::rgb(theme::text_secondary())).child("Sent anonymously with app version, platform and build profile. No account or chat history is attached. Please avoid sharing sensitive information."))
                .when_some(form.error.clone(), |card, error| card.child(widgets::banner(theme::status_error()).child(error)))
                .child(div().flex().justify_end().gap_2()
                    .child(widgets::secondary_button("feedback-cancel").aria_label("Cancel feedback").track_focus(&form.controls[4]).child("Cancel")
                        .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| { if matches!(event.keystroke.key.as_str(), "enter" | "space") { this.close_feedback(cx); cx.stop_propagation(); } }))
                        .when(busy, |b| b.opacity(0.5))
                        .on_click(cx.listener(|this, _, _, cx| this.close_feedback(cx))))
                    .child(widgets::primary_button("feedback-submit").aria_label("Send feedback").debug_selector(|| "feedback-submit".into()).track_focus(&form.controls[5])
                        .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| { if matches!(event.keystroke.key.as_str(), "enter" | "space") { this.submit_feedback(cx); cx.stop_propagation(); } }))
                        .when(busy, |b| b.opacity(0.5)).child(if busy { "Sending…" } else { "Send feedback" })
                        .on_click(cx.listener(|this, _, _, cx| this.submit_feedback(cx)))))
            ))
    }
}
