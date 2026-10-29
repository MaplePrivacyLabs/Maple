//! Composer upload/admission interleavings through the real backend facade and
//! framed IPC client. The host holds individual replies; no model or account is used.

use super::*;
use crate::host::{RemoteClient, ipc, protocol, receipts::Outcome};
use gpui::TestAppContext;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Clone)]
struct ObservedRequest {
    id: u64,
    method: String,
    send_id: Option<String>,
    params: Value,
}

struct UploadHost {
    backend: Arc<AgentBackend>,
    requests: Arc<Mutex<Vec<ObservedRequest>>>,
    replies: mpsc::UnboundedSender<(u64, Result<Value, String>)>,
    runtime: Option<tokio::runtime::Runtime>,
    _root: tempfile::TempDir,
}

impl UploadHost {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let endpoint = ipc::Endpoint::for_test(root.path().to_path_buf());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let (replies, mut responses) = mpsc::unbounded_channel::<(u64, Result<Value, String>)>();
        let backend = runtime.block_on(async {
            let (client, server) = tokio::io::duplex(64 * 1024);
            tokio::spawn(async move {
                let (mut input, mut output) = tokio::io::split(server);
                // Keep reads outside select! so a released reply cannot cancel
                // a partially consumed request frame.
                let (incoming, mut received) = mpsc::channel(32);
                let reader = tokio::spawn(async move {
                    while let Ok(request) = protocol::read::<protocol::Request>(&mut input).await {
                        if incoming.send(request).await.is_err() {
                            break;
                        }
                    }
                });
                loop {
                    let response = tokio::select! {
                        reply = responses.recv() => {
                            let Some(reply) = reply else { break };
                            reply
                        }
                        request = received.recv() => {
                            let Some(request) = request else { break };
                            recorded.lock().unwrap().push(ObservedRequest {
                                id: request.id,
                                method: request.method.clone(),
                                send_id: request.send_id,
                                params: request.params,
                            });
                            if matches!(request.method.as_str(), "import_attachment" | "attachment_upload_status" | "send_message" | "send_outcome" | "stop_runtime") {
                                continue;
                            }
                            let result = match request.method.as_str() {
                                "cancel_run" | "cancel_attachment_upload" | "set_ui_settings" | "set_remote_sidebar" | "flush_ui_settings" | "logout_and_clear" | "detach" => Ok(Value::Null),
                                "get_ui_settings" => Ok(serde_json::to_value(crate::settings::AppSettings::default()).unwrap()),
                                "get_remote_sidebars" => Ok(json!({})),
                                "list_slash_commands" => Ok(json!([])),
                                _ => Err("Optional upload fixture operation unavailable".into()),
                            };
                            (request.id, result)
                        }
                    };
                    if protocol::write(&mut output, &protocol::Message::Response {
                        id: response.0,
                        result: response.1,
                    }).await.is_err() {
                        break;
                    }
                }
                reader.abort();
            });
            Arc::new(AgentBackend::from_host_client_for_test(RemoteClient::attach_for_test(
                endpoint,
                Box::new(client),
                protocol::Welcome {
                    version: protocol::VERSION,
                    pid: std::process::id(),
                    build: protocol::BUILD.into(),
                    user_id: Some("user".into()),
                    store: maple_agent::store::StoreState::Open,
                },
            )))
        });
        Self {
            backend,
            requests,
            replies,
            runtime: Some(runtime),
            _root: root,
        }
    }

    fn requests(&self, method: &str) -> Vec<ObservedRequest> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.method == method)
            .cloned()
            .collect()
    }

    fn reply(&self, request: &ObservedRequest, result: Result<Value, String>) {
        self.replies.send((request.id, result)).unwrap();
    }

    fn finish_upload(&self) {
        self.reply(
            &self.requests("import_attachment")[0],
            Ok(serde_json::to_value(maple_agent::store::StoredAttachment {
                id: "0123456789abcdef0123456789abcdef".into(),
                name: "file.txt".into(),
                mime_type: "application/octet-stream".into(),
                source: "maple-attachment://0123456789abcdef0123456789abcdef".into(),
                size: 15,
                execution_path: "/synthetic-execution/file.txt".into(),
            })
            .unwrap()),
        );
    }
}

impl Drop for UploadHost {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

fn chat_window<'a>(
    host: &UploadHost,
    cx: &'a mut TestAppContext,
) -> (Entity<ChatScreen>, &'a mut gpui::VisualTestContext) {
    struct WindowHost(Entity<ChatScreen>);
    impl Render for WindowHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(1200.))
                .h(px(800.))
                .flex()
                .flex_col()
                .child(self.0.clone())
        }
    }
    cx.executor().allow_parking();
    let chat = cx.new(|cx| {
        let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
        crate::desktop::register_key_bindings(cx);
        let mut chat = ChatScreen::new_without_start(host.backend.clone(), "user".into(), cx);
        chat.selected_session = Some("s1".into());
        chat.booting = false;
        chat.trust_prompts = false;
        chat.active_runs.insert("s1".into(), "run-1".into());
        chat.sidebar
            .update(cx, |sidebar, _| sidebar.reset_persisted_for_test());
        chat
    });
    let (_, cx) = cx.add_window_view(|_, _| WindowHost(chat.clone()));
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    (chat, cx)
}

fn wait_for(
    cx: &mut gpui::VisualTestContext,
    label: &str,
    mut ready: impl FnMut(&mut gpui::VisualTestContext) -> bool,
) {
    for _ in 0..500 {
        cx.run_until_parked();
        if ready(cx) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("upload fixture timed out waiting for {label}");
}

fn start_upload(chat: &Entity<ChatScreen>, cx: &mut gpui::VisualTestContext, host: &UploadHost) {
    chat.update(cx, |chat, cx| {
        chat.draft_attachments = vec![DraftAttachment::for_test(1, "file.txt", true)];
        chat.composer
            .clone()
            .unwrap()
            .update(cx, |input, cx| input.set_text("with file", cx));
        chat.send_inner(cx);
    });
    wait_for(cx, "held import", |_| {
        host.requests("import_attachment").len() == 1
    });
    assert!(
        host.requests("send_message").is_empty(),
        "bytes precede the identified prompt"
    );
}

fn type_followup(
    chat: &Entity<ChatScreen>,
    cx: &mut gpui::VisualTestContext,
    text: &str,
    steer: bool,
) {
    chat.update(cx, |chat, cx| {
        chat.composer
            .clone()
            .unwrap()
            .update(cx, |input, cx| input.set_text(text, cx));
        if steer {
            chat.steer_text(text.into(), cx);
        } else {
            chat.send_inner(cx);
        }
    });
}

fn wait_prepared(chat: &Entity<ChatScreen>, cx: &mut gpui::VisualTestContext) {
    wait_for(cx, "uploaded files waiting for admission", |cx| {
        chat.update(cx, |chat, _| {
            chat.sending_previews
                .values()
                .any(|preview| preview.prepared().is_some())
        })
    });
}

#[gpui::test]
fn slow_upload_keeps_stop_queue_and_steer_operational(cx: &mut TestAppContext) {
    let host = UploadHost::new();
    let (chat, cx) = chat_window(&host, cx);
    start_upload(&chat, cx, &host);
    cx.run_until_parked();
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let stop = cx
        .debug_bounds("stop-run")
        .expect("Stop remains visible during upload");
    cx.simulate_click(stop.center(), gpui::Modifiers::default());
    wait_for(cx, "Stop RPC while import remains held", |_| {
        host.requests("cancel_run").len() == 1
    });
    assert_eq!(
        host.requests("cancel_run")[0].params,
        json!(["user", "run-1"])
    );

    type_followup(&chat, cx, "queue this", false);
    wait_for(cx, "queued text admission", |_| {
        host.requests("send_message").len() == 1
    });
    let queued = host.requests("send_message")[0].clone();
    assert_eq!(queued.params[1]["text"], "queue this");
    assert_eq!(queued.params[1]["steer"], false);
    assert_eq!(queued.params[1]["attachments"], json!([]));
    host.reply(&queued, Ok(json!("run-1")));
    wait_for(cx, "queue acknowledgement", |cx| {
        chat.update(cx, |chat, _| !chat.send_pending())
    });

    type_followup(&chat, cx, "steer this", true);
    wait_for(cx, "steering admission", |_| {
        host.requests("send_message").len() == 2
    });
    let steering = host.requests("send_message")[1].clone();
    assert_eq!(steering.params[1]["text"], "steer this");
    assert_eq!(steering.params[1]["steer"], true);
    host.finish_upload();
    wait_prepared(&chat, cx);
    assert_eq!(
        host.requests("send_message").len(),
        2,
        "upload waits for the newer text admission"
    );
    host.reply(&steering, Ok(json!("run-1")));
    wait_for(cx, "file prompt after steering settles", |_| {
        host.requests("send_message").len() == 3
    });
    let file_send = host.requests("send_message")[2].clone();
    assert_eq!(file_send.params[1]["text"], "with file");
    assert_eq!(
        file_send.params[1]["attachments"],
        json!(["0123456789abcdef0123456789abcdef"])
    );
    assert_ne!(file_send.send_id, queued.send_id);
    assert_ne!(file_send.send_id, steering.send_id);
    chat.update(cx, |chat, cx| {
        let (id, upload) = chat
            .attachment_uploads
            .iter()
            .next()
            .expect("send acknowledgement is held");
        assert!(upload.fence.handed_off());
        let id = id.clone();
        chat.cancel_attachment_send(&id, cx);
    });
    assert!(
        host.requests("cancel_attachment_upload").is_empty(),
        "cancellation cannot win after prompt handoff"
    );
    host.reply(&file_send, Ok(json!("run-1")));
    wait_for(cx, "file acknowledgement", |cx| {
        chat.update(cx, |chat, _| chat.attachment_uploads.is_empty())
    });
}

#[gpui::test]
fn uploaded_prompt_waits_for_unknown_text_receipt_without_replaying_text(cx: &mut TestAppContext) {
    let host = UploadHost::new();
    let (chat, cx) = chat_window(&host, cx);
    start_upload(&chat, cx, &host);
    type_followup(&chat, cx, "possibly received", false);
    wait_for(cx, "text admission", |_| {
        host.requests("send_message").len() == 1
    });
    let text = host.requests("send_message")[0].clone();
    host.finish_upload();
    wait_prepared(&chat, cx);
    host.reply(
        &text,
        Err("Send outcome unknown. Synthetic lost acknowledgement.".into()),
    );
    wait_for(cx, "receipt query", |_| {
        host.requests("send_outcome").len() == 1
    });
    let receipt = host.requests("send_outcome")[0].clone();
    assert_eq!(receipt.params, json!(text.send_id.as_ref().unwrap()));
    assert_eq!(host.requests("send_message").len(), 1);
    host.reply(
        &receipt,
        Ok(serde_json::to_value(Outcome::Pending).unwrap()),
    );
    wait_for(cx, "pending receipt retained", |cx| {
        chat.update(cx, |chat, _| {
            !chat.checking_sends
                && chat
                    .unresolved_sends
                    .values()
                    .any(UnresolvedSend::awaiting_receipt)
        })
    });
    assert_eq!(
        host.requests("send_message").len(),
        1,
        "Pending authorizes neither replay nor the prepared prompt"
    );
    chat.update(cx, |chat, cx| chat.check_unresolved_sends(cx));
    wait_for(cx, "same receipt queried again", |_| {
        host.requests("send_outcome").len() == 2
    });
    let resolved = host.requests("send_outcome")[1].clone();
    assert_eq!(resolved.params, receipt.params);
    host.reply(
        &resolved,
        Ok(serde_json::to_value(Outcome::Finished(Ok(json!("run-1")))).unwrap()),
    );
    wait_for(cx, "prepared prompt admitted after receipt", |_| {
        host.requests("send_message").len() == 2
    });
    let file_send = host.requests("send_message")[1].clone();
    assert_eq!(file_send.params[1]["text"], "with file");
    assert_ne!(file_send.send_id, text.send_id);
    assert_eq!(
        host.requests("send_message")
            .iter()
            .filter(|request| request.send_id == text.send_id)
            .count(),
        1
    );
    host.reply(&file_send, Ok(json!("run-1")));
    wait_for(cx, "uploaded send settled", |cx| {
        chat.update(cx, |chat, _| chat.attachment_uploads.is_empty())
    });
}

#[gpui::test]
fn cancelling_prepared_upload_before_handoff_never_dispatches_its_prompt(cx: &mut TestAppContext) {
    let host = UploadHost::new();
    let (chat, cx) = chat_window(&host, cx);
    start_upload(&chat, cx, &host);
    type_followup(&chat, cx, "keep this text", false);
    wait_for(cx, "text admission", |_| {
        host.requests("send_message").len() == 1
    });
    let text = host.requests("send_message")[0].clone();
    host.finish_upload();
    wait_prepared(&chat, cx);
    chat.update(cx, |chat, cx| {
        let (id, upload) = chat
            .selected_upload()
            .expect("prepared upload remains cancellable");
        assert!(!upload.fence.handed_off());
        let id = id.to_owned();
        chat.cancel_attachment_send(&id, cx);
    });
    wait_for(cx, "upload cancellation RPC", |_| {
        host.requests("cancel_attachment_upload").len() == 1
    });
    wait_for(cx, "cancelled draft restored", |cx| {
        chat.update(cx, |chat, cx| {
            chat.attachment_uploads.is_empty()
                && chat.draft_attachments.len() == 1
                && chat.composer_text(cx).as_deref() == Some("with file")
        })
    });
    host.reply(&text, Ok(json!("run-1")));
    wait_for(cx, "newer text acknowledgement", |cx| {
        chat.update(cx, |chat, _| !chat.send_pending())
    });
    assert_eq!(
        host.requests("send_message").len(),
        1,
        "a cancelled prepared upload never hands off after the other send settles"
    );
    chat.update(cx, |chat, _| {
        assert!(
            chat.sending_previews
                .values()
                .all(|preview| preview.prepared().is_none())
        );
        assert!(chat.draft_attachments[0].committed.is_none());
    });
}

#[gpui::test]
fn uploaded_prompt_waits_for_history_recovery_before_admission(cx: &mut TestAppContext) {
    let host = UploadHost::new();
    let (chat, cx) = chat_window(&host, cx);
    start_upload(&chat, cx, &host);
    chat.update(cx, |chat, _| {
        chat.host_recovery.unavailable = true;
        chat.runtime_error = Some("Connected. Refreshing task history…".into());
    });
    host.finish_upload();
    wait_prepared(&chat, cx);
    assert!(host.requests("send_message").is_empty());
    chat.update(cx, |chat, cx| {
        // The recovery snapshot installs these states together before the
        // production completion callback dispatches prepared uploads.
        chat.host_recovery.unavailable = false;
        chat.runtime_error = None;
        chat.dispatch_prepared_uploads(cx);
    });
    wait_for(cx, "admission after history recovery", |_| {
        host.requests("send_message").len() == 1
    });
    let sent = host.requests("send_message")[0].clone();
    assert_eq!(sent.params[1]["text"], "with file");
    assert_eq!(sent.params[1]["attachments"].as_array().unwrap().len(), 1);
    chat.update(cx, |chat, cx| chat.dispatch_prepared_uploads(cx));
    host.reply(&sent, Ok(json!("run-1")));
    wait_for(cx, "settled upload send", |cx| {
        chat.update(cx, |chat, _| !chat.send_pending())
    });
    assert_eq!(host.requests("send_message").len(), 1);
}

#[gpui::test]
fn sign_out_ignores_preference_failure_but_preserves_runtime_failure(cx: &mut TestAppContext) {
    let host = UploadHost::new();
    let (chat, cx) = chat_window(&host, cx);
    let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
    crate::settings::install_test_settings(crate::settings::AppSettings::default());
    // No settings sink exists, so the queued presentation edit cannot flush.
    crate::settings::update_settings_and_wait(|settings| settings.theme = "dark".into());

    chat.update(cx, |chat, cx| chat.sign_out(cx));
    wait_for(cx, "runtime shutdown despite failed preferences", |_| {
        host.requests("stop_runtime").len() == 1
    });
    host.reply(
        &host.requests("stop_runtime")[0],
        Err("Synthetic runtime shutdown failure".into()),
    );
    wait_for(cx, "authoritative shutdown failure", |cx| {
        chat.update(cx, |chat, _| !chat.sign_out_pending)
    });
    chat.update(cx, |chat, _| {
        assert!(chat.accept_send_results);
        assert_eq!(
            chat.notice.as_deref(),
            Some("Synthetic runtime shutdown failure")
        );
        chat.notice = None;
    });
    assert!(host.requests("logout_and_clear").is_empty());

    chat.update(cx, |chat, cx| chat.sign_out(cx));
    wait_for(cx, "second runtime shutdown", |_| {
        host.requests("stop_runtime").len() == 2
    });
    host.reply(
        &host.requests("stop_runtime")[1],
        Ok(json!({
            "running": false,
            "projectRoot": null,
            "model": null,
            "activeRuns": {}
        })),
    );
    wait_for(cx, "logout despite failed preferences", |cx| {
        host.requests("logout_and_clear").len() == 1
            && chat.update(cx, |chat, _| !chat.sign_out_pending)
    });
    chat.update(cx, |chat, _| {
        assert!(!chat.accept_send_results);
        assert!(chat.notice.is_none());
    });
    crate::settings::install_test_settings(crate::settings::AppSettings::default());
}
