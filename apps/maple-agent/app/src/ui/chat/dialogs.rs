//! Project-level dialogs and operations the screen owns even though the
//! sidebar asks for them: the trust prompt, the remove-project
//! confirmation, archiving, and leaving a task. They touch the canonical
//! session list and the project context, which live here.

use gpui::{Context, Div, div, prelude::*, px};
use maple_agent::agent::AgentProjectTrustStatus;

use super::ChatScreen;
use crate::ui::theme;
use crate::ui::widgets;

impl ChatScreen {
    /// Ask for a trust decision when the current project provides skills
    /// or guidance and none is saved yet. Home and the process launch
    /// directory are already trusted in that case, so this stays closed.
    pub(super) fn check_project_trust(&mut self, cx: &mut Context<Self>) {
        self.trust_prompt = None;
        let Some(root) = self.project_root.clone() else {
            return;
        };
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.project_trust(&user_id, root).await },
            cx,
            |this, result, cx| {
                if let Ok(status) = result
                    && this.trust_prompts
                    && this.project_root.as_deref() == Some(status.path.as_str())
                    && status.available
                    && !status.protected_features.is_empty()
                    && status.decision.is_none()
                {
                    this.trust_prompt = Some(status);
                    this.dialog_focus.get_or_insert_with(|| cx.focus_handle());
                    this.dialog_focus_pending = true;
                    cx.notify();
                }
            },
        );
    }

    pub(super) fn set_project_trust(
        &mut self,
        path: String,
        trusted: bool,
        cx: &mut Context<Self>,
    ) {
        if self.trust_saving {
            return;
        }
        self.trust_saving = true;
        cx.notify();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.set_project_trust(&user_id, path, trusted).await },
            cx,
            move |this, result, cx| {
                this.trust_saving = false;
                match result {
                    Ok(status) => {
                        if this.trust_prompt.as_ref().map(|p| &p.path) == Some(&status.path) {
                            this.trust_prompt = None;
                        }
                        this.notice = Some(
                            if trusted {
                                "Project trusted: its skills and guidance are available to new tasks"
                            } else {
                                "Project kept untrusted"
                            }
                            .into(),
                        );
                    }
                    Err(message) => this.notice = Some(message.into()),
                }
                cx.notify();
            },
        );
    }

    /// Modal that asks whether project-provided guidance may be used.
    pub(super) fn render_trust_prompt(
        &self,
        status: &AgentProjectTrustStatus,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let name = self.sidebar.read(cx).root_name(&status.path);
        let saving = self.trust_saving;
        let button = |id: &'static str, label: &'static str, primary: bool| {
            if primary {
                widgets::primary_button(id)
            } else {
                widgets::secondary_button(id)
            }
            .py_1p5()
            .when(saving, |button| button.opacity(0.6))
            .child(label)
        };
        let keep_path = status.path.clone();
        let trust_path = status.path.clone();
        let key_keep = status.path.clone();
        let key_trust = status.path.clone();
        div()
            .id("trust-backdrop")
            .absolute()
            .size_full()
            .top_0()
            .left_0()
            .occlude()
            .bg(theme::scrim())
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("trust-card")
                    .role(gpui::Role::Dialog)
                    .aria_label(format!("Trust {name}?"))
                    .when_some(self.dialog_focus.clone(), |card, focus| {
                        card.track_focus(&focus)
                    })
                    .key_context("Dialog")
                    .on_key_down(cx.listener(
                        move |this, event: &gpui::KeyDownEvent, _window, cx| {
                            if this.trust_saving {
                                return;
                            }
                            match dialog_key(event) {
                                Some(DialogKey::Confirm) => {
                                    this.set_project_trust(key_trust.clone(), true, cx);
                                    cx.stop_propagation();
                                }
                                Some(DialogKey::Cancel) => {
                                    this.set_project_trust(key_keep.clone(), false, cx);
                                    cx.stop_propagation();
                                }
                                None => {}
                            }
                        },
                    ))
                    .w(px(460.))
                    .p_5()
                    .rounded(theme::RADIUS_XL)
                    .shadow_lg()
                    .bg(gpui::rgb(theme::bg_elevated()))
                    .border_1()
                    .border_color(gpui::rgb(theme::border()))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(format!("Trust {name}?")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(gpui::rgb(theme::text_secondary()))
                            .child(
                                "Trusting a project lets Maple use project-provided guidance, \
                                 including agent skills. These instructions can influence how \
                                 agents work and use tools. Maple's tool permissions still \
                                 apply, and you can change this choice later from the \
                                 project's menu.",
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_family(crate::assets::FONT_MONO)
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(status.path.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(button("trust-keep", "Keep untrusted", false).on_click(
                                cx.listener(move |this, _event, _window, cx| {
                                    this.set_project_trust(keep_path.clone(), false, cx);
                                }),
                            ))
                            .child(button("trust-allow", "Trust project", true).on_click(
                                cx.listener(move |this, _event, _window, cx| {
                                    this.set_project_trust(trust_path.clone(), true, cx);
                                }),
                            )),
                    ),
            )
    }

    /// Ask before a project leaves the sidebar.
    pub(super) fn request_remove_root(&mut self, root: &str, cx: &mut Context<Self>) {
        self.confirm_remove_root = Some(root.to_string());
        self.dialog_focus.get_or_insert_with(|| cx.focus_handle());
        self.dialog_focus_pending = true;
        cx.notify();
    }

    fn confirm_remove_root(&mut self, cx: &mut Context<Self>) {
        if let Some(root) = self.confirm_remove_root.take() {
            self.archive_root(&root, cx);
        }
        cx.notify();
    }

    /// Modal that confirms a project removal.
    pub(super) fn render_confirm_remove(
        &self,
        root: &str,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let name = self.sidebar.read(cx).root_name(root);
        let button = |id: &'static str, label: &'static str, primary: bool| {
            if primary {
                widgets::danger_button(id)
            } else {
                widgets::secondary_button(id)
            }
            .py_1p5()
            .child(label)
        };
        div()
            .id("confirm-remove-backdrop")
            .absolute()
            .size_full()
            .top_0()
            .left_0()
            .occlude()
            .bg(theme::scrim())
            .flex()
            .items_center()
            .justify_center()
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.confirm_remove_root = None;
                cx.notify();
            }))
            .child(
                div()
                    .id("confirm-remove-card")
                    .role(gpui::Role::Dialog)
                    .aria_label(format!("Remove {name}?"))
                    .when_some(self.dialog_focus.clone(), |card, focus| {
                        card.track_focus(&focus)
                    })
                    .key_context("Dialog")
                    .on_key_down(
                        cx.listener(
                            |this, event: &gpui::KeyDownEvent, _window, cx| match dialog_key(event)
                            {
                                Some(DialogKey::Confirm) => {
                                    this.confirm_remove_root(cx);
                                    cx.stop_propagation();
                                }
                                Some(DialogKey::Cancel) => {
                                    this.confirm_remove_root = None;
                                    cx.notify();
                                    cx.stop_propagation();
                                }
                                None => {}
                            },
                        ),
                    )
                    .w(px(420.))
                    .p_5()
                    .rounded(theme::RADIUS_XL)
                    .shadow_lg()
                    .bg(gpui::rgb(theme::bg_elevated()))
                    .border_1()
                    .border_color(gpui::rgb(theme::border()))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .on_click(|_event, _window, cx| cx.stop_propagation())
                    .child(
                        div()
                            .text_lg()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(format!("Remove {name}?")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(gpui::rgb(theme::text_secondary()))
                            .child(
                                "The project leaves the sidebar and its tasks move to \
                                 Archived, where you can restore them. No files are deleted.",
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_family(crate::assets::FONT_MONO)
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(root.to_string()),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(button("confirm-remove-cancel", "Cancel", false).on_click(
                                cx.listener(|this, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.confirm_remove_root = None;
                                    cx.notify();
                                }),
                            ))
                            .child(button("confirm-remove-ok", "Remove", true).on_click(
                                cx.listener(|this, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.confirm_remove_root(cx);
                                }),
                            )),
                    ),
            )
    }

    /// Drop the selection and everything the composer shows for it: the
    /// transcript, the side thread, the queue, and a permission card that
    /// belongs to the task. Questions stay queued per session.
    pub(super) fn leave_selected_session(&mut self, cx: &mut Context<Self>) {
        let left = self.clear_selected_session_presentation(cx);
        if let Some(left) = left.as_deref() {
            let showing = self
                .pending_permissions
                .iter()
                .any(|permission| permission.session_id == left);
            self.pending_permissions
                .retain(|permission| permission.session_id != left);
            if showing {
                self.permission_responding = false;
            }
        }
    }

    /// Archive or restore one task. The service event updates the row;
    /// an archived selection moves to the newest task in the same root.
    pub(super) fn set_session_archived(
        &mut self,
        session_id: &str,
        archived: bool,
        cx: &mut Context<Self>,
    ) {
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let session_id = session_id.to_string();
        let changed_id = session_id.clone();
        self.call(
            async move {
                backend
                    .set_session_archived(&user_id, &session_id, archived)
                    .await
            },
            cx,
            move |this, result, cx| {
                match result {
                    Ok(session) => {
                        let root = session.project_root.clone();
                        this.upsert_session(session, cx);
                        if archived && this.selected_session.as_deref() == Some(&*changed_id) {
                            this.leave_selected_session(cx);
                            let next = this
                                .sessions
                                .iter()
                                .find(|s| !s.archived && s.project_root == root)
                                .map(|s| s.id.clone());
                            if let Some(id) = next {
                                this.select_session(&id, cx);
                            }
                        }
                    }
                    Err(error) => this.notice = Some(error.into()),
                }
                cx.notify();
            },
        );
    }

    /// Archive every task in a project and drop the project from the
    /// sidebar. The UI selects the next project when this one was current.
    pub(super) fn archive_root(&mut self, root: &str, cx: &mut Context<Self>) {
        if self.root_selecting {
            self.notice = Some("Wait for the project selection to finish, then try again".into());
            cx.notify();
            return;
        }
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let path = root.to_string();
        let fallback = self
            .recent_roots
            .iter()
            .chain(
                self.sessions
                    .iter()
                    .filter(|s| !s.archived)
                    .map(|s| &s.project_root),
            )
            .find(|candidate| candidate.as_str() != root)
            .cloned();
        let task_ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|s| !s.archived && s.project_root == root)
            .map(|s| s.id.clone())
            .collect();
        let removed = path.clone();
        let next_root = fallback.clone();
        let removed_task_ids = task_ids.clone();
        self.call(
            async move {
                for id in task_ids {
                    backend.set_session_archived(&user_id, &id, true).await?;
                }
                backend.remove_project_root(&user_id, path, fallback).await
            },
            cx,
            move |this, result, cx| {
                match result {
                    Ok(()) => {
                        this.recent_roots.retain(|candidate| candidate != &removed);
                        for session in &mut this.sessions {
                            if session.project_root == removed {
                                session.archived = true;
                                this.completed_unread_sessions.remove(&session.id);
                            }
                        }
                        this.sidebar.update(cx, |sidebar, cx| {
                            sidebar.forget_tasks(&removed_task_ids, cx);
                        });
                        let was_current = this.project_root.as_deref() == Some(&*removed);
                        if was_current {
                            this.leave_selected_session(cx);
                            this.set_project_context(next_root.clone(), cx);
                        }
                        this.sync_sidebar(cx);
                        if was_current && let Some(next) = next_root {
                            this.persist_project_root(next, cx);
                            this.refresh_sessions(cx);
                        } else {
                            this.refresh_roots(cx);
                        }
                    }
                    Err(error) => this.notice = Some(error.into()),
                }
                cx.notify();
            },
        );
    }
}

/// What a key press means to a modal dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DialogKey {
    Confirm,
    Cancel,
}

/// Enter confirms and Escape cancels; any modifier means neither.
fn dialog_key(event: &gpui::KeyDownEvent) -> Option<DialogKey> {
    let modifiers = &event.keystroke.modifiers;
    if modifiers.control || modifiers.alt || modifiers.platform || modifiers.shift {
        return None;
    }
    match event.keystroke.key.as_str() {
        "enter" => Some(DialogKey::Confirm),
        "escape" => Some(DialogKey::Cancel),
        _ => None,
    }
}
