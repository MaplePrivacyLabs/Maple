//! Local diagnostics and experimental salvage. A preview is an opaque host
//! token; the GUI never supplies a salvage report as authority to replace data.

use gpui::{Context, Entity, EventEmitter, Render, Subscription, Window, div, prelude::*};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{cell::RefCell, sync::Arc};

use super::{
    secret_input::{SecretInput, SecretInputChanged},
    text_input::TextInput,
    theme, widgets,
};
use crate::backend::AgentBackend;

#[derive(Clone, Default, Deserialize)]
pub(crate) struct RepairProgress {
    #[serde(default)]
    pub busy: bool,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub completed: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub cancellable: bool,
}

#[derive(Deserialize)]
struct RepairStatus {
    #[serde(default)]
    authenticated: bool,
    preview_id: Option<String>,
    diagnostic: Option<Value>,
    #[serde(default)]
    progress: RepairProgress,
}

#[derive(Clone, Deserialize)]
struct SalvagePage {
    preview_id: String,
    retained_tasks: u64,
    dropped_tasks: u64,
    retained_rows: u64,
    dropped_rows_count: u64,
    tasks_to_rebuild: u64,
    dropped_rows: Vec<Value>,
    next_offset: Option<u64>,
}

#[derive(Clone, Copy)]
pub(crate) enum RepairPanelKind {
    Damaged { password: bool },
}
pub(crate) struct RepairStateChanged;

pub(crate) struct RepairPanel {
    backend: Arc<AgentBackend>,
    actions_available: bool,
    authenticated: bool,
    password: Entity<SecretInput>,
    confirmation: Entity<TextInput>,
    diagnostic: Option<Value>,
    preview: Option<SalvagePage>,
    reviewed_all: bool,
    busy: bool,
    progress: RepairProgress,
    message: Option<String>,
    bridges: RefCell<Vec<gpui::Task<()>>>,
    _subscriptions: Vec<Subscription>,
    _progress_task: Option<gpui::Task<()>>,
}
impl EventEmitter<RepairStateChanged> for RepairPanel {}

impl RepairPanel {
    pub(crate) fn new(
        backend: Arc<AgentBackend>,
        kind: RepairPanelKind,
        actions_available: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let RepairPanelKind::Damaged {
            password: requires_password,
        } = kind;
        let password = cx.new(|cx| SecretInput::new("Current unlock password", cx));
        let confirmation = cx.new(|cx| TextInput::new("Type the confirmation exactly", cx));
        let changed = cx.subscribe(
            &password,
            |_this, _input, _event: &SecretInputChanged, cx| cx.notify(),
        );
        let mut this = Self {
            backend,
            actions_available,
            authenticated: !requires_password,
            password,
            confirmation,
            diagnostic: None,
            preview: None,
            reviewed_all: false,
            busy: false,
            progress: RepairProgress::default(),
            message: None,
            bridges: RefCell::new(Vec::new()),
            _subscriptions: vec![changed],
            _progress_task: None,
        };
        this.poll_progress(cx);
        this.restore(cx);
        this
    }
    fn call<T: Send + 'static>(
        &self,
        future: impl std::future::Future<Output = Result<T, String>> + Send + 'static,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
    ) {
        let bridge = super::task::call(&self.backend, future, cx, then);
        super::task::retain(&self.bridges, bridge);
    }
    fn poll_progress(&mut self, cx: &mut Context<Self>) {
        self._progress_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                let Ok(backend) = this.update(cx, |this, _cx| this.backend.clone()) else {
                    break;
                };
                let control = backend.clone();
                let result = backend
                    .spawn(async move {
                        control
                            .local_control::<RepairProgress>("repair_progress", ())
                            .await
                    })
                    .await;
                if let Ok(Ok(progress)) = result
                    && this
                        .update(cx, |this, cx| {
                            this.progress = progress;
                            cx.notify();
                        })
                        .is_err()
                {
                    break;
                }
            }
        }));
    }
    fn cancel(&mut self, cx: &mut Context<Self>) {
        if !self.progress.cancellable {
            return;
        }
        let backend = self.backend.clone();
        self.call(
            async move {
                backend
                    .local_control::<bool>("cancel_store_repair", ())
                    .await
            },
            cx,
            |this, result, cx| {
                this.message = Some(match result {
                    Ok(true) => "Cancelling recovery before replacement…".into(),
                    Ok(false) => "Recovery can no longer be cancelled. Wait for its result.".into(),
                    Err(message) => message,
                });
                cx.notify();
            },
        );
    }
    fn restore(&mut self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        self.call(
            async move {
                backend
                    .local_control::<RepairStatus>("repair_status", ())
                    .await
            },
            cx,
            |this, result, cx| {
                match result {
                    Ok(status) => {
                        this.authenticated |= status.authenticated;
                        this.diagnostic = status.diagnostic;
                        this.progress = status.progress;
                        if let Some(preview_id) = status.preview_id {
                            this.load_preview(preview_id, 0, cx);
                        }
                    }
                    Err(message) => this.message = Some(message),
                }
                cx.notify();
            },
        );
    }
    fn authenticate(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.password.read(cx).is_empty() {
            return;
        }
        let password = self.password.update(cx, |input, cx| input.take_secret(cx));
        self.busy = true;
        self.message = None;
        let backend = self.backend.clone();
        self.call(
            async move {
                backend
                    .local_control::<Value>(
                        "authenticate_repair_password",
                        PasswordRequest {
                            password: &password,
                        },
                    )
                    .await
            },
            cx,
            |this, result, cx| {
                this.busy = false;
                match result {
                    Ok(_) => {
                        this.authenticated = true;
                        this.restore(cx);
                    }
                    Err(message) => this.message = Some(message),
                }
                cx.notify();
            },
        );
    }
    fn check(&mut self, cx: &mut Context<Self>) {
        if self.busy || !self.authenticated {
            return;
        }
        self.busy = true;
        self.message = None;
        let backend = self.backend.clone();
        self.call(
            async move {
                backend
                    .local_control::<Value>(
                        "check_store_integrity",
                        serde_json::json!({"kind":"full"}),
                    )
                    .await
            },
            cx,
            |this, result, cx| {
                this.busy = false;
                match result {
                    Ok(report) => this.diagnostic = Some(report),
                    Err(message) => this.message = Some(message),
                }
                cx.notify();
            },
        );
    }
    fn preview(&mut self, cx: &mut Context<Self>) {
        if self.busy || !self.authenticated || !self.actions_available {
            return;
        }
        self.busy = true;
        self.message = None;
        self.preview = None;
        self.reviewed_all = false;
        let backend = self.backend.clone();
        self.call(
            async move {
                backend
                    .local_control::<SalvagePage>("preview_salvage", ())
                    .await
            },
            cx,
            |this, result, cx| {
                this.busy = false;
                match result {
                    Ok(page) => {
                        this.reviewed_all = true;
                        this.preview = Some(page);
                    }
                    Err(message) => this.message = Some(message),
                }
                cx.notify();
            },
        );
    }
    fn load_preview(&mut self, preview_id: String, offset: u64, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        let backend = self.backend.clone();
        self.call(
            async move {
                backend
                    .local_control::<SalvagePage>(
                        "salvage_preview_page",
                        serde_json::json!({"preview_id":preview_id,"offset":offset}),
                    )
                    .await
            },
            cx,
            |this, result, cx| {
                this.busy = false;
                match result {
                    Ok(page) => {
                        this.reviewed_all = true;
                        this.preview = Some(page);
                    }
                    Err(message) => this.message = Some(message),
                }
                cx.notify();
            },
        );
    }
    fn save_readable(&mut self, cx: &mut Context<Self>) {
        if self.busy || !self.authenticated || !self.actions_available || !self.reviewed_all {
            return;
        }
        let Some(preview) = &self.preview else {
            return;
        };
        let preview_id = preview.preview_id.clone();
        let confirmation = self.confirmation.read(cx).text();
        if confirmation != "SAVE READABLE DATA" {
            self.message =
                Some("Type SAVE READABLE DATA exactly after reviewing the preview.".into());
            cx.notify();
            return;
        }
        self.busy = true;
        self.message = None;
        let backend = self.backend.clone();
        self.call(
            async move {
                backend
                    .local_control::<Value>(
                        "salvage_store",
                        serde_json::json!({"preview_id":preview_id,"confirmation":confirmation}),
                    )
                    .await
            },
            cx,
            |this, result, cx| {
                this.busy = false;
                match result {
                    Ok(_) => {
                        this.preview = None;
                        cx.emit(RepairStateChanged);
                    }
                    Err(message) => this.message = Some(message),
                }
                cx.notify();
            },
        );
    }
}

impl Render for RepairPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut pane = div()
            .flex()
            .flex_col()
            .gap_3()
            .text_color(gpui::rgb(theme::text_primary()));
        pane = pane.child(div().text_sm().child(
            "Inspect this computer's current local store. Previous app data is left untouched.",
        ));
        if !self.authenticated && !self.actions_available {
            pane = pane.child(
                div()
                    .text_sm()
                    .child("Diagnostics for an encrypted store are unavailable in this build."),
            );
        } else if !self.authenticated {
            pane = pane
                .child(
                    div().text_sm().child(
                        "Enter the current unlock password to inspect this encrypted store.",
                    ),
                )
                .child(self.password.clone())
                .when(self.actions_available, |pane| {
                    pane.child(
                        widgets::primary_button("repair-authenticate")
                            .on_click(
                                cx.listener(|this, _event, _window, cx| this.authenticate(cx)),
                            )
                            .child(gpui::text!("Authenticate for diagnostics")),
                    )
                });
        } else {
            pane = pane.child(
                widgets::secondary_button("repair-diagnostics")
                    .on_click(cx.listener(|this, _event, _window, cx| this.check(cx)))
                    .child(gpui::text!("Check integrity")),
            );
            if let Some(report) = &self.diagnostic {
                pane = pane
                    .child(div().text_sm().child(diagnostic_summary(report)))
                    .child(
                        widgets::secondary_button("copy-repair-diagnostic")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                if let Some(report) = &this.diagnostic
                                    && let Ok(text) = serde_json::to_string_pretty(report)
                                {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                                }
                            }))
                            .child(gpui::text!("Copy a diagnostic report")),
                    );
            }
            if self.actions_available {
                pane = pane
                    .child(div().text_sm().child(
                        "Experimental recovery can save some readable data. Previewing is limited to five minutes and may stop without a result. A failed or cancelled preview leaves the original unchanged.",
                    ))
                    .child(
                        widgets::secondary_button("repair-preview-salvage")
                            .on_click(cx.listener(|this, _event, _window, cx| this.preview(cx)))
                            .child(gpui::text!("Preview experimental recovery")),
                    );
            }
            if self.actions_available
                && let Some(preview) = &self.preview
            {
                pane = pane
                    .child(div().text_sm().child(format!(
                        "{} tasks and {} readable source rows can be kept. {} tasks and {} rows will be dropped. {} retained tasks need their timelines rebuilt.",
                        preview.retained_tasks, preview.retained_rows, preview.dropped_tasks,
                        preview.dropped_rows_count, preview.tasks_to_rebuild,
                    )))
                    .children(preview.dropped_rows.iter().map(|row| div().text_xs().child(row.to_string())));
                if let Some(offset) = preview.next_offset {
                    let id = preview.preview_id.clone();
                    pane = pane.child(
                        widgets::secondary_button("repair-next-preview")
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.load_preview(id.clone(), offset, cx)
                            }))
                            .child(gpui::text!("Review next dropped rows")),
                    );
                }
                pane = pane.child(div().text_sm().child(
                    "Saving readable data permanently deletes the original damaged store after its verified replacement is installed. The listed unreadable data will be lost. Type SAVE READABLE DATA to confirm.",
                ));
                if self.reviewed_all {
                    pane = pane.child(self.confirmation.clone()).child(
                        widgets::primary_button("repair-save-readable")
                            .on_click(
                                cx.listener(|this, _event, _window, cx| this.save_readable(cx)),
                            )
                            .child(gpui::text!("Save readable data and replace original")),
                    );
                }
            }
        }
        if self.busy || self.progress.busy {
            let bytes = if self.progress.total > 0 {
                format!(
                    " {} of {} bytes.",
                    self.progress.completed, self.progress.total
                )
            } else {
                String::new()
            };
            pane = pane.child(div().text_sm().child(format!(
                "Local recovery: {}.{bytes}",
                if self.progress.phase.is_empty() {
                    "preparing"
                } else {
                    &self.progress.phase
                }
            )));
            if self.progress.cancellable {
                pane = pane.child(
                    widgets::secondary_button("repair-cancel")
                        .on_click(cx.listener(|this, _event, _window, cx| this.cancel(cx)))
                        .child(gpui::text!("Cancel")),
                );
            }
        }
        if let Some(message) = &self.message {
            pane = pane.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::status_warning()))
                    .child(message.clone()),
            );
        }
        pane
    }
}

#[derive(Serialize)]
struct PasswordRequest<'a> {
    password: &'a str,
}
fn diagnostic_summary(report: &Value) -> String {
    let status = report
        .pointer("/outcome/status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let complete = report
        .get("complete")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let findings = report
        .get("findings")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    format!(
        "Integrity check: {status}. Complete: {}. Findings: {findings}. The diagnostic report contains no task content.",
        if complete { "yes" } else { "no" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_summary_uses_only_redacted_status_and_counts() {
        let report = serde_json::json!({"outcome":{"status":"partial"},"complete":false,"findings":[{"kind":"timeout"}],"untrusted":"synthetic-content"});
        let text = diagnostic_summary(&report);
        assert!(text.contains("partial"));
        assert!(text.contains("Findings: 1"));
        assert!(!text.contains("synthetic-content"));
    }
}
