//! Upload admission precedes prompt admission. Cancelling and handing off the
//! prompt compete for one atomic transition, so a cancelled upload never sends.

use super::*;
use crate::backend::{AttachmentUploadRequest, ImportAttachmentRequest};
use maple_agent::store::{AttachmentUploadStatus, StoredAttachment};
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicUsize, Ordering},
};

const UPLOADING: u8 = 0;
const CANCELLED: u8 = 1;
const HANDED_OFF: u8 = 2;

#[derive(Default)]
struct UploadState {
    phase: AtomicU8,
    active_operation: AtomicUsize,
    cancellation: tokio::sync::Notify,
}

#[derive(Clone, Default)]
pub(super) struct UploadFence(Arc<UploadState>);
impl UploadFence {
    pub fn cancel(&self) -> bool {
        let cancelled = self
            .0
            .phase
            .compare_exchange(UPLOADING, CANCELLED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if cancelled {
            self.0.cancellation.notify_waiters();
        }
        cancelled
    }

    async fn wait_cancelled(&self) {
        let notified = self.0.cancellation.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.cancelled() {
            notified.await;
        }
    }

    pub fn hand_off(&self) -> bool {
        self.0
            .phase
            .compare_exchange(UPLOADING, HANDED_OFF, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
    pub fn handed_off(&self) -> bool {
        self.0.phase.load(Ordering::SeqCst) == HANDED_OFF
    }
    fn set_active_operation(&self, index: usize) {
        self.0.active_operation.store(index, Ordering::SeqCst);
    }
    fn active_operation(&self) -> usize {
        self.0.active_operation.load(Ordering::SeqCst)
    }
    pub fn cancelled(&self) -> bool {
        self.0.phase.load(Ordering::SeqCst) == CANCELLED
    }
}

pub(super) struct PendingUpload {
    pub session: String,
    pub count: usize,
    pub label: SharedString,
    pub fence: UploadFence,
    pub cancel_on_drop: bool,
    backend: Arc<AgentBackend>,
    user: String,
    operations: Vec<String>,
}

pub(super) struct PreparedSend {
    pub request: AgentSendMessageRequest,
    pub send: UnresolvedSend,
    pub fence: UploadFence,
}

impl Drop for PendingUpload {
    fn drop(&mut self) {
        // Sign-out, account replacement and erase-success tear down the view.
        // Detached import work must lose its permission to send immediately.
        if self.cancel_on_drop && self.fence.cancel() {
            drop(self.backend.spawn(cancel_operations(
                self.backend.clone(),
                self.user.clone(),
                self.session.clone(),
                self.operations.clone(),
                self.fence.active_operation(),
            )));
        }
    }
}

async fn cancel_operations(
    backend: Arc<AgentBackend>,
    user: String,
    session: String,
    mut operations: Vec<String>,
    active: usize,
) -> Result<(), String> {
    if !operations.is_empty() {
        let active = active.min(operations.len() - 1);
        operations.rotate_left(active);
    }
    for (index, operation_id) in operations.into_iter().enumerate() {
        let request = AttachmentUploadRequest {
            session_id: session.clone(),
            operation_id,
        };
        // The active stream's cancellation goes first and can briefly retry a
        // busy control slot. Cleanup of the other max-nine operations follows
        // sequentially, preserving the separate Stop/question control capacity.
        let attempts = if index == 0 { 3 } else { 1 };
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            for attempt in 0..attempts {
                if backend
                    .cancel_attachment_upload(&user, request.clone())
                    .await
                    .is_ok()
                {
                    break;
                }
                if attempt + 1 < attempts {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        })
        .await;
    }
    Ok(())
}

impl ChatScreen {
    /// Upload completion is not prompt admission. A newer text send can be
    /// settling (including an unknown receipt) while these bytes finish.
    pub(super) fn dispatch_prepared_uploads(&mut self, cx: &mut Context<Self>) {
        if !self.accept_send_results {
            return;
        }
        let ready: Vec<_> = self
            .sending_previews
            .iter()
            .filter(|&(id, preview)| {
                !self.discarded_sends.contains(id) && preview.prepared().is_some()
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ready {
            let Some(prepared) = self
                .sending_previews
                .get(&id)
                .and_then(|preview| preview.prepared())
            else {
                continue;
            };
            let session = &prepared.send.session;
            let recovering = self.host_recovery.unavailable
                || self.host_recovery.ended
                || self.booting
                || self.runtime_error.is_some();
            if !prepared.fence.cancelled()
                && (recovering
                    || self.sending_previews.values().any(|preview| {
                        preview.session.as_ref() == Some(session) && preview.admitting()
                    })
                    || self
                        .unresolved_sends
                        .values()
                        .any(|send| &send.session == session && send.awaiting_receipt()))
            {
                if let Some(upload) = self.attachment_uploads.get_mut(&id) {
                    upload.label = if recovering {
                        "Files ready · waiting for the host to recover"
                    } else {
                        "Files ready · waiting for the previous send"
                    }
                    .into();
                }
                continue;
            }
            let prepared = self
                .sending_previews
                .get_mut(&id)
                .and_then(|preview| preview.take_prepared())
                .unwrap();
            self.admit_prepared_send(id, prepared, cx);
        }
    }

    pub(super) fn admit_prepared_send(
        &mut self,
        id: String,
        prepared: PreparedSend,
        cx: &mut Context<Self>,
    ) {
        let PreparedSend {
            request,
            send,
            fence,
        } = prepared;
        if !fence.hand_off() {
            self.finish_attachment_send(
                id,
                send.clone(),
                Ok((send, Err("Upload cancelled. No message was sent.".into()))),
                cx,
            );
            return;
        }
        let backend = send.backend.clone();
        let user = self.user_id.clone();
        let send_id = id.clone();
        let retained = send.clone();
        self.call(
            async move {
                let result = backend
                    .send_message_identified(&user, request, send_id)
                    .await;
                Ok((send, result))
            },
            cx,
            move |this, result, cx| {
                this.finish_attachment_send(id, retained, result, cx);
            },
        );
    }

    pub(super) fn finish_attachment_send(
        &mut self,
        send_id: String,
        mut retained: UnresolvedSend,
        result: Result<(UnresolvedSend, Result<String, String>), String>,
        cx: &mut Context<Self>,
    ) {
        let mut pending = self.attachment_uploads.remove(&send_id);
        let cancelled = pending
            .as_ref()
            .is_some_and(|upload| upload.fence.cancelled());
        if result.is_ok() {
            // A normal failed/unknown upload retains its operation for receipt
            // reconciliation. Only view teardown cancels it on drop.
            if let Some(upload) = &mut pending {
                upload.cancel_on_drop = false;
            }
        } else if pending
            .as_ref()
            .is_some_and(|upload| !upload.fence.handed_off())
        {
            for draft in &mut retained.attachments {
                draft.reset_upload();
            }
        }
        drop(pending);
        if !self.accept_send_results || self.discarded_sends.remove(&send_id) {
            return;
        }
        match result {
            Ok((mut send, result)) => {
                if cancelled {
                    for draft in &mut send.attachments { draft.reset_upload(); }
                }
                self.finish_send(send_id, send, result, cx);
            }
            Err(_) => self.finish_send(send_id, retained,
                Err("Send outcome unknown. The background request ended before reporting its result.".into()), cx),
        }
    }

    pub(super) fn begin_uploads(
        &mut self,
        send_id: &str,
        session: &str,
        drafts: &[DraftAttachment],
        cx: &mut Context<Self>,
    ) -> (UploadFence, tokio::sync::mpsc::UnboundedSender<usize>) {
        let fence = UploadFence::default();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        if !drafts.is_empty() {
            self.attachment_uploads.insert(
                send_id.to_string(),
                PendingUpload {
                    session: session.to_string(),
                    count: drafts.len(),
                    label: format!("Uploading files · 0 of {}", drafts.len()).into(),
                    fence: fence.clone(),
                    cancel_on_drop: true,
                    backend: self.backend.clone(),
                    user: self.user_id.clone(),
                    operations: drafts
                        .iter()
                        .map(|draft| draft.operation_id.clone())
                        .collect(),
                },
            );
            let id = send_id.to_string();
            let task = cx.spawn(async move |this, cx| {
                while let Some(completed) = receiver.recv().await {
                    if this
                        .update(cx, |this, cx| {
                            if let Some(upload) = this.attachment_uploads.get_mut(&id) {
                                if upload.fence.cancelled() {
                                    return;
                                }
                                upload.label =
                                    format!("Uploading files · {completed} of {}", upload.count)
                                        .into();
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
            crate::ui::task::retain(&self.bridged_tasks, task);
        }
        (fence, sender)
    }

    pub(super) fn cancel_attachment_send(&mut self, send_id: &str, cx: &mut Context<Self>) {
        let Some(upload) = self.attachment_uploads.get_mut(send_id) else {
            return;
        };
        if !upload.fence.cancel() {
            return;
        }
        upload.label = "Cancelling upload…".into();
        let backend = upload.backend.clone();
        let user = upload.user.clone();
        let session = upload.session.clone();
        let operations = upload.operations.clone();
        let active = upload.fence.active_operation();
        // This runs concurrently with import, not behind its blocking await.
        self.call(
            cancel_operations(backend, user, session, operations, active),
            cx,
            |_, _, _| {},
        );
        self.dispatch_prepared_uploads(cx);
        cx.notify();
    }

    pub(super) fn selected_upload(&self) -> Option<(&str, &PendingUpload)> {
        self.attachment_uploads
            .iter()
            .find(|(_, upload)| {
                self.selected_session.as_deref() == Some(upload.session.as_str())
                    && !upload.fence.handed_off()
            })
            .map(|(id, upload)| (id.as_str(), upload))
    }
}

pub(super) async fn commit_attachments(
    backend: &AgentBackend,
    user: &str,
    session: &str,
    drafts: &mut [DraftAttachment],
    fence: &UploadFence,
    progress: tokio::sync::mpsc::UnboundedSender<usize>,
) -> Result<Vec<String>, String> {
    let mut ids = Vec::with_capacity(drafts.len());
    for (index, draft) in drafts.iter_mut().enumerate() {
        fence.set_active_operation(index);
        if fence.cancelled() {
            return Err("Upload cancelled. No message was sent.".into());
        }
        if let Some((committed_session, _)) = &draft.committed
            && committed_session == session
        {
            // Unbound committed files can expire while a failed draft stays
            // open. Only an authoritative Missing permits another import;
            // a lost status reply must retain the cached identity and bytes.
            let query = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                backend.attachment_upload_status(
                    user,
                    AttachmentUploadRequest {
                        session_id: session.to_owned(),
                        operation_id: draft.operation_id.clone(),
                    },
                ),
            );
            let status = tokio::select! {
                result = query => result.map_err(|_| "Could not confirm the retained file upload. Your draft is retained; try again when the connection recovers.".to_string())??,
                () = fence.wait_cancelled() => return Err("Upload cancelled. No message was sent.".into()),
            };
            match status {
                AttachmentUploadStatus::Complete(attachment) => {
                    draft.committed = Some((session.to_owned(), attachment.id.clone()));
                    ids.push(attachment.id);
                    let _ = progress.send(index + 1);
                    continue;
                }
                AttachmentUploadStatus::Missing => {
                    // Reuse the immutable operation as well as the staged
                    // bytes, keeping cancellation and duplicate-ACK handling
                    // attached to the same operation throughout the retry.
                    draft.committed = None;
                }
                AttachmentUploadStatus::Cancelled => return Err("The file upload was cancelled. Your draft is retained.".into()),
                AttachmentUploadStatus::Uploading => return Err("The retained file upload is still in progress. Your draft is retained; try again when it completes.".into()),
            }
        }
        let staged = draft
            .staged
            .as_ref()
            .ok_or("Attachment is still being prepared")?;
        let import = backend.import_attachment(
            user,
            ImportAttachmentRequest {
                session_id: session.to_string(),
                operation_id: draft.operation_id.clone(),
                path: staged.path.to_string_lossy().into_owned(),
                name: draft.name.clone(),
                mime_type: draft.mime_type.clone(),
            },
        );
        let imported = tokio::select! {
            result = import => result,
            () = fence.wait_cancelled() => return Err("Upload cancelled. No message was sent.".into()),
        };
        let imported = match imported {
            Ok(attachment) => attachment,
            Err(error) if !fence.cancelled() => {
                // An ACK can be lost after publication. Query the same host and
                // immutable operation once; never guess or resend a prompt.
                let query = tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    backend.attachment_upload_status(
                        user,
                        AttachmentUploadRequest {
                            session_id: session.to_string(),
                            operation_id: draft.operation_id.clone(),
                        },
                    ),
                );
                let status = tokio::select! {
                    result = query => result.ok().and_then(Result::ok),
                    () = fence.wait_cancelled() => return Err("Upload cancelled. No message was sent.".into()),
                };
                resolve_upload_receipt(error, status)?
            }
            Err(_) => return Err("Upload cancelled. No message was sent.".into()),
        };
        if fence.cancelled() {
            // The bounded cancellation worker handles publication races. Return
            // the draft promptly; never wait for a second cancellation RPC.
            return Err("Upload cancelled. No message was sent.".into());
        }
        draft.committed = Some((session.to_string(), imported.id.clone()));
        ids.push(imported.id);
        let _ = progress.send(index + 1);
    }
    Ok(ids)
}

fn resolve_upload_receipt(
    error: String,
    status: Option<AttachmentUploadStatus>,
) -> Result<StoredAttachment, String> {
    match status {
        Some(AttachmentUploadStatus::Complete(attachment)) => Ok(attachment),
        Some(AttachmentUploadStatus::Cancelled) => {
            Err("The file upload was cancelled. Your draft is retained.".into())
        }
        Some(AttachmentUploadStatus::Uploading) | None => Err(format!(
            "File upload could not be confirmed: {error}. Your draft is retained; try again when the connection recovers."
        )),
        Some(AttachmentUploadStatus::Missing) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct UploadHost {
        complete: HashMap<String, StoredAttachment>,
        imports: Vec<(String, String, Vec<u8>)>,
        fail_second: bool,
        fail_status: bool,
    }

    // Exercise the production client and upload orchestration. The fixture's
    // missing receipt represents an unbound file removed by store maintenance;
    // store tests separately exercise the 24-hour collection policy.
    fn upload_host(
        root: &std::path::Path,
    ) -> (
        AgentBackend,
        Arc<std::sync::Mutex<UploadHost>>,
        tokio::task::JoinHandle<()>,
    ) {
        use crate::host::{RemoteClient, ipc, protocol};
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        let state = Arc::new(std::sync::Mutex::new(UploadHost::default()));
        let shared = state.clone();
        let task = tokio::spawn(async move {
            while let Ok(request) = protocol::read::<protocol::Request>(&mut server).await {
                let result = {
                    let mut state = shared.lock().unwrap();
                    match request.method.as_str() {
                        "import_attachment" => {
                            let request: ImportAttachmentRequest =
                                serde_json::from_value(request.params[1].clone()).unwrap();
                            if state.fail_second && request.name == "second.txt" {
                                Err("Synthetic second-file failure".into())
                            } else {
                                let bytes = std::fs::read(&request.path).unwrap();
                                state.imports.push((
                                    request.operation_id.clone(),
                                    request.name.clone(),
                                    bytes.clone(),
                                ));
                                let descriptor = StoredAttachment {
                                    id: format!("file-{}", state.imports.len()),
                                    name: request.name,
                                    mime_type: request.mime_type,
                                    source: "maple-attachment://fixture".into(),
                                    size: bytes.len() as u64,
                                    execution_path: request.path,
                                };
                                state
                                    .complete
                                    .insert(request.operation_id, descriptor.clone());
                                Ok(serde_json::to_value(descriptor).unwrap())
                            }
                        }
                        "attachment_upload_status" => {
                            let request: AttachmentUploadRequest =
                                serde_json::from_value(request.params[1].clone()).unwrap();
                            if state.fail_status {
                                Err("Synthetic unavailable receipt".into())
                            } else {
                                Ok(serde_json::to_value(
                                    state
                                        .complete
                                        .get(&request.operation_id)
                                        .cloned()
                                        .map(AttachmentUploadStatus::Complete)
                                        .unwrap_or(AttachmentUploadStatus::Missing),
                                )
                                .unwrap())
                            }
                        }
                        _ => Err("Unexpected fixture request".into()),
                    }
                };
                if protocol::write(
                    &mut server,
                    &protocol::Message::Response {
                        id: request.id,
                        result,
                    },
                )
                .await
                .is_err()
                {
                    break;
                }
            }
        });
        let client = RemoteClient::attach_for_test(
            ipc::Endpoint::for_test(root.to_owned()),
            Box::new(client),
            protocol::Welcome {
                version: protocol::VERSION,
                pid: std::process::id(),
                build: protocol::BUILD.into(),
                user_id: Some("user".into()),
                store: maple_agent::store::StoreState::Open,
            },
        );
        (AgentBackend::from_host_client_for_test(client), state, task)
    }

    #[tokio::test]
    async fn expired_partial_upload_reimports_staged_bytes_only_after_authoritative_missing() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("original.txt");
        std::fs::write(&original, b"original bytes").unwrap();
        let (staged, size) = staging::copy_file(&root.path().join("drafts"), &original).unwrap();
        let mut first = DraftAttachment::preparing(1, "first.txt".into());
        first.staged = Some(staged);
        first.size = size;
        first.mime_type = "text/plain".into();
        let mut second = DraftAttachment::preparing(2, "second.txt".into());
        second.staged =
            Some(staging::copy_bytes(&root.path().join("drafts"), b"second bytes").unwrap());
        second.mime_type = "text/plain".into();
        let mut drafts = vec![first, second];
        let (backend, host, task) = upload_host(root.path());
        host.lock().unwrap().fail_second = true;
        let (progress, _) = tokio::sync::mpsc::unbounded_channel();
        let failure = commit_attachments(
            &backend,
            "user",
            "task",
            &mut drafts,
            &UploadFence::default(),
            progress,
        )
        .await
        .unwrap_err();
        assert!(failure.contains("second-file failure"));
        let committed = drafts[0].committed.clone().unwrap();
        let operation = drafts[0].operation_id.clone();
        assert!(drafts[1].committed.is_none());

        // The execution copy expires while the retained desktop draft stays
        // alive. Changing the source must not change the retry's bytes.
        std::fs::write(&original, b"changed original").unwrap();
        {
            let mut host = host.lock().unwrap();
            host.complete.remove(&operation);
            host.fail_status = true;
            host.fail_second = false;
        }
        let (progress, _) = tokio::sync::mpsc::unbounded_channel();
        assert!(
            commit_attachments(
                &backend,
                "user",
                "task",
                &mut drafts,
                &UploadFence::default(),
                progress
            )
            .await
            .is_err()
        );
        assert_eq!(drafts[0].committed.as_ref(), Some(&committed));
        assert_eq!(drafts[0].operation_id, operation);
        assert_eq!(
            host.lock().unwrap().imports.len(),
            1,
            "an unavailable receipt must never trigger reimport"
        );

        host.lock().unwrap().fail_status = false;
        let (progress, _) = tokio::sync::mpsc::unbounded_channel();
        let ids = commit_attachments(
            &backend,
            "user",
            "task",
            &mut drafts,
            &UploadFence::default(),
            progress,
        )
        .await
        .unwrap();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], committed.1);
        assert_eq!(
            drafts[0].operation_id, operation,
            "retry keeps the cancellation identity"
        );
        let imports = &host.lock().unwrap().imports;
        assert_eq!(imports.len(), 3);
        assert_eq!(imports[1].2, b"original bytes");
        assert_eq!(imports[2].2, b"second bytes");
        task.abort();
    }
    #[test]
    fn lost_upload_ack_resolves_only_a_committed_receipt() {
        let descriptor = StoredAttachment {
            id: "file-id".into(),
            name: "report.zip".into(),
            mime_type: "application/zip".into(),
            source: "maple-attachment://file-id".into(),
            size: 123,
            execution_path: "/uploads/report.zip".into(),
        };
        assert_eq!(
            resolve_upload_receipt(
                "connection lost".into(),
                Some(AttachmentUploadStatus::Complete(descriptor.clone()))
            )
            .unwrap(),
            descriptor
        );
        for status in [
            None,
            Some(AttachmentUploadStatus::Uploading),
            Some(AttachmentUploadStatus::Missing),
            Some(AttachmentUploadStatus::Cancelled),
        ] {
            assert!(resolve_upload_receipt("connection lost".into(), status).is_err());
        }
    }

    #[test]
    fn cancellation_wins_before_prompt_handoff() {
        let fence = UploadFence::default();
        assert!(fence.cancel());
        assert!(!fence.hand_off());
        assert!(fence.cancelled());
    }

    #[tokio::test]
    async fn cancellation_wakes_pending_and_later_upload_waiters() {
        let fence = UploadFence::default();
        let waiting = fence.clone();
        let waiter = tokio::spawn(async move { waiting.wait_cancelled().await });
        tokio::task::yield_now().await;
        assert!(fence.cancel());
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("cancellation must wake the in-flight import")
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), fence.wait_cancelled())
            .await
            .expect("an already-cancelled import must not miss the notification");
        assert!(!fence.hand_off());
    }

    #[test]
    fn prompt_handoff_cannot_be_reported_as_cancelled() {
        let fence = UploadFence::default();
        assert!(fence.hand_off());
        assert!(!fence.cancel());
        assert!(!fence.cancelled());
    }
}
