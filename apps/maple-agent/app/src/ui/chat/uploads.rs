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
        if let Some((committed_session, id)) = &draft.committed
            && committed_session == session
        {
            ids.push(id.clone());
            let _ = progress.send(index + 1);
            continue;
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
