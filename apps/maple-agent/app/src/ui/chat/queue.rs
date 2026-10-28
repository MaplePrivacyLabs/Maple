//! The pending-message queue: the chips under the composer and the
//! actions that steer, edit, or drop a message the runtime is holding
//! behind the active run.

use gpui::{
    Context, Div, InteractiveElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, div, prelude::FluentBuilder, px,
};
use maple_agent::agent::AgentQueuedMessage;

use super::{COMPOSER_PLACEHOLDER, ChatScreen, QUEUE_EDIT_PLACEHOLDER, QueueEdit};
use crate::backend::QueuedMessageRequest;
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
        if self.queue_busy || !self.queue.iter().any(|item| item.queue_id == queue_id) {
            return;
        }
        // Open-task replies may contain only a queue preview. Fetch the full
        // message before creating the send's optimistic state or fingerprint.
        self.queue_busy = true;
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let generation = self.selection_generation;
        let queue_id = queue_id.to_string();
        let target = session_id.clone();
        let queued = queue_id.clone();
        self.call(
            async move {
                backend
                    .queued_message(
                        &user_id,
                        QueuedMessageRequest {
                            task: session_id,
                            queue_id,
                        },
                    )
                    .await
            },
            cx,
            move |this, result, cx| {
                this.queue_busy = false;
                if this.selection_generation != generation || !this.is_selected(&target) {
                    cx.notify();
                    return;
                }
                match result {
                    Ok(item) if item.session_id == target && item.queue_id == queued => {
                        this.send_to_session(&target, item.text, true, Some(queued), cx);
                    }
                    Ok(_) => this.notice = Some("The queued message changed; try again".into()),
                    Err(message) => this.notice = Some(message.into()),
                }
                cx.notify();
            },
        );
        cx.notify();
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
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let generation = self.selection_generation;
        let queue_id = queue_id.to_string();
        let target = session_id.clone();
        self.call(
            async move {
                backend
                    .cancel_queued_message(&user_id, &session_id, &queue_id)
                    .await
            },
            cx,
            move |this, result, cx| {
                this.queue_busy = false;
                match result {
                    Ok(snapshot) => {
                        if this.is_selected(&target) {
                            this.set_queue(snapshot.items);
                        }
                    }
                    Err(message)
                        if this.selection_generation == generation && this.is_selected(&target) =>
                    {
                        this.notice = Some(message.into());
                    }
                    Err(_) => {}
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
        if !self.queue.iter().any(|item| item.queue_id == queue_id) {
            return;
        }
        self.queue_busy = true;
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let generation = self.selection_generation;
        let queue_id = queue_id.to_string();
        let target = session_id.clone();
        let held_id = queue_id.clone();
        self.call(
            async move {
                let item = backend
                    .queued_message(
                        &user_id,
                        QueuedMessageRequest {
                            task: session_id.clone(),
                            queue_id: queue_id.clone(),
                        },
                    )
                    .await?;
                if item.session_id != session_id || item.queue_id != queue_id {
                    return Err("The queued message changed; try again".to_string());
                }
                backend
                    .begin_queued_message_edit(&user_id, &session_id, &queue_id)
                    .await?;
                // Read again under the hold: another client could have edited
                // this message between the first read and acquiring the hold.
                match backend
                    .queued_message(
                        &user_id,
                        QueuedMessageRequest {
                            task: session_id.clone(),
                            queue_id: queue_id.clone(),
                        },
                    )
                    .await
                {
                    Ok(current)
                        if current.session_id == session_id && current.queue_id == queue_id =>
                    {
                        Ok(current.text)
                    }
                    result => {
                        let _ = backend
                            .end_queued_message_edit(&user_id, &session_id, &queue_id)
                            .await;
                        Err(result
                            .err()
                            .unwrap_or_else(|| "The queued message changed; try again".to_string()))
                    }
                }
            },
            cx,
            move |this, result, cx| {
                this.queue_busy = false;
                match result {
                    Ok(text)
                        if this.selection_generation == generation && this.is_selected(&target) =>
                    {
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
                    Ok(_) => {
                        // Selection moved while the hold was requested.
                        this.release_queue_hold(&target, &held_id, cx);
                    }
                    Err(message)
                        if this.selection_generation == generation && this.is_selected(&target) =>
                    {
                        this.notice = Some(message.into());
                    }
                    Err(_) => {}
                }
                cx.notify();
            },
        );
        cx.notify();
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
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let session_id = session_id.to_string();
        let queue_id = queue_id.to_string();
        self.call(
            async move {
                backend
                    .end_queued_message_edit(&user_id, &session_id, &queue_id)
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
                .map(|button| {
                    crate::ui::motion::hover_background(
                        button,
                        gpui::transparent_black(),
                        gpui::rgb(theme::bg_sidebar_row_hover()),
                    )
                })
                .when(busy, |button| button.highlighted(false))
                .when(!busy, |button| button.hover(|style| style.cursor_pointer()))
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
