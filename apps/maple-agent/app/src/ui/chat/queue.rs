//! The pending-message queue: the chips under the composer and the
//! actions that steer, edit, or drop a message the runtime is holding
//! behind the active run.

use gpui::{
    Context, Div, InteractiveElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, div, prelude::FluentBuilder, px,
};
use maple_agent::agent::AgentQueuedMessage;

use super::{COMPOSER_PLACEHOLDER, ChatScreen, QUEUE_EDIT_PLACEHOLDER, QueueEdit};
use crate::ui::icons::icon;
use crate::ui::theme;

impl ChatScreen {
    /// Replace the queue and the one-line preview each chip shows.
    pub(super) fn set_queue(&mut self, items: Vec<AgentQueuedMessage>) {
        self.queue_previews = items
            .iter()
            .map(|item| SharedString::from(item.text.lines().next().unwrap_or("").to_string()))
            .collect();
        self.queue = items;
    }

    /// Send a queued chip into the active run now.
    fn steer_queued(&mut self, queue_id: &str, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        let Some(item) = self.queue.iter().find(|item| item.queue_id == queue_id) else {
            return;
        };
        let text = item.text.clone();
        self.send_to_session(&session_id, text, true, Some(queue_id.to_string()), cx);
    }

    /// Drop a queued chip.
    fn remove_queued(&mut self, queue_id: &str, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        if self.queue_busy {
            return;
        }
        self.queue_busy = true;
        let host = self.backend_for(&session_id);
        let queue_id = queue_id.to_string();
        let target = session_id.clone();
        self.call(
            async move {
                host.cancel_queued_message(session_id.clone(), queue_id.clone())
                    .await
            },
            cx,
            move |this, result, cx| {
                this.queue_busy = false;
                match result {
                    Ok(snapshot) => {
                        if this.is_selected(&target) {
                            this.queue = snapshot.items;
                        }
                    }
                    Err(message) => this.notice = Some(message.into()),
                }
                cx.notify();
            },
        );
    }

    /// Pull a queued chip into the composer. The runtime holds its place
    /// until the edit is sent (Enter updates it in place) or discarded.
    fn edit_queued(&mut self, queue_id: &str, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        if self.queue_busy || self.queue_edit.is_some() {
            return;
        }
        let Some(item) = self.queue.iter().find(|item| item.queue_id == queue_id) else {
            return;
        };
        let text = item.text.clone();
        self.queue_busy = true;
        let host = self.backend_for(&session_id);
        let queue_id = queue_id.to_string();
        let target = session_id.clone();
        let held_id = queue_id.clone();
        self.call(
            async move {
                host.begin_queued_message_edit(session_id.clone(), queue_id.clone())
                    .await
            },
            cx,
            move |this, result, cx| {
                this.queue_busy = false;
                match result {
                    Ok(()) if this.is_selected(&target) => {
                        let draft = this
                            .composer
                            .as_ref()
                            .map(|composer| composer.read(cx).text())
                            .unwrap_or_default();
                        this.queue_edit = Some(QueueEdit {
                            queue_id: held_id.clone(),
                            draft,
                        });
                        if let Some(composer) = this.composer.clone() {
                            composer.update(cx, |input, cx| {
                                input.set_text(&text, cx);
                                input.set_placeholder(QUEUE_EDIT_PLACEHOLDER, cx);
                            });
                        }
                    }
                    Ok(()) => {
                        // Selection moved while the hold was requested.
                        this.release_queue_hold(&target, &held_id, cx);
                    }
                    Err(message) => this.notice = Some(message.into()),
                }
                cx.notify();
            },
        );
    }

    /// Drop the edit without changing the queued message; the stashed
    /// draft returns to the composer.
    pub(super) fn discard_queue_edit(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        if let Some(edit) = self.queue_edit.as_ref() {
            let queue_id = edit.queue_id.clone();
            self.release_queue_hold(&session_id, &queue_id, cx);
        }
        self.finish_queue_edit(cx);
    }

    /// The edit ended (sent or discarded): restore the composer.
    pub(super) fn finish_queue_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.queue_edit.take() else {
            return;
        };
        if let Some(composer) = self.composer.clone() {
            composer.update(cx, |input, cx| {
                input.set_text(&edit.draft, cx);
                input.set_placeholder(COMPOSER_PLACEHOLDER, cx);
            });
        }
        cx.notify();
    }

    /// An edit cannot follow a session switch; release the hold and put the
    /// composer back.
    pub(super) fn abandon_queue_edit(&mut self, cx: &mut Context<Self>) {
        if self.queue_edit.is_none() {
            return;
        }
        if let (Some(previous), Some(edit)) =
            (self.selected_session.clone(), self.queue_edit.as_ref())
        {
            let queue_id = edit.queue_id.clone();
            self.release_queue_hold(&previous, &queue_id, cx);
        }
        self.finish_queue_edit(cx);
    }

    fn release_queue_hold(&self, session_id: &str, queue_id: &str, cx: &mut Context<Self>) {
        let host = self.backend_for(session_id);
        let session_id = session_id.to_string();
        let queue_id = queue_id.to_string();
        self.call(
            async move {
                host.end_queued_message_edit(session_id.clone(), queue_id.clone())
                    .await
            },
            cx,
            |_this, result, _cx| {
                if let Err(message) = result {
                    log::debug!("end_queued_message_edit failed: {message}");
                }
            },
        );
    }

    /// Chips for messages waiting behind the active run.
    pub(super) fn render_queue(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.queue.is_empty() {
            return None;
        }
        let busy = self.queue_busy;
        let action = |id: String, icon_name: &'static str| {
            div()
                .id(SharedString::from(id))
                .size_5()
                .flex()
                .items_center()
                .justify_center()
                .rounded(theme::RADIUS_SM)
                .when(busy, |button| button.opacity(0.5))
                .when(!busy, |button| {
                    button.hover(|style| {
                        style
                            .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                            .cursor_pointer()
                    })
                })
                .child(icon(icon_name, px(12.), theme::text_secondary()))
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .px_4()
                .pt_3()
                .child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(format!("{} queued for after this turn", self.queue.len())),
                )
                .children(self.queue.iter().enumerate().map(|(index, item)| {
                    let steer_id = item.queue_id.clone();
                    let edit_id = item.queue_id.clone();
                    let remove_id = item.queue_id.clone();
                    let editing = self
                        .queue_edit
                        .as_ref()
                        .is_some_and(|edit| edit.queue_id == item.queue_id);
                    let preview: SharedString = if editing {
                        "Editing in the composer…".into()
                    } else {
                        self.queue_previews.get(index).cloned().unwrap_or_default()
                    };
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .rounded(theme::RADIUS_MD)
                        .bg(gpui::rgb(theme::bg_elevated()))
                        .border_1()
                        .border_color(gpui::rgb(theme::border_subtle()))
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_secondary()))
                        .when(editing, |row| {
                            row.border_color(gpui::rgb(theme::accent()))
                                .text_color(gpui::rgb(theme::text_muted()))
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .line_clamp(1)
                                .text_ellipsis()
                                .child(preview),
                        )
                        .when(!item.attachments.is_empty(), |row| {
                            row.child(icon("paperclip", px(12.), theme::text_muted()))
                        })
                        .when(!editing, |row| {
                            row.child(
                                action(format!("queue-steer-{}", item.queue_id), "arrow-up")
                                    .on_click(cx.listener(move |this, _event, _window, cx| {
                                        this.steer_queued(&steer_id, cx);
                                    })),
                            )
                            .child(
                                action(format!("queue-edit-{}", item.queue_id), "pencil").on_click(
                                    cx.listener(move |this, _event, _window, cx| {
                                        this.edit_queued(&edit_id, cx);
                                    }),
                                ),
                            )
                            .child(
                                action(format!("queue-remove-{}", item.queue_id), "x").on_click(
                                    cx.listener(move |this, _event, _window, cx| {
                                        this.remove_queued(&remove_id, cx);
                                    }),
                                ),
                            )
                        })
                        .when(editing, |row| {
                            row.child(
                                action(format!("queue-discard-{}", item.queue_id), "x").on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.discard_queue_edit(cx);
                                    }),
                                ),
                            )
                        })
                })),
        )
    }
}
