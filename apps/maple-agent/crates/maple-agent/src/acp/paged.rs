//! Bounded ACP navigation and replay helpers. One source row may be restored
//! for replay, but an entire conversation is never assembled here.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use agent_client_protocol::v1::schema::{ContentBlock, SessionUpdate};
use tokio_util::sync::CancellationToken;

use crate::agent::AgentRuntimeHandle;
use crate::agent::AgentTimelineItem;
use crate::store::ReplaySnapshot;
use crate::store::{AccountId, Store, StoreError, StoreResult, TaskHistoryStamp};

pub(super) const ACP_LIST_ROWS: u16 = 100;
pub(super) const ACP_LIST_BYTES: usize = 4 * 1024 * 1024 - 2048;
pub(super) const ACP_HISTORY_ROWS: u16 = 200;
pub(super) const ACP_HISTORY_BYTES: usize = 512 * 1024;
pub(super) const ACP_TEXT_CHUNK_BYTES: usize = 64 * 1024;
const CAPTURE_BUDGET: Duration = Duration::from_secs(30);

pub(super) fn notification_text(update: &mut SessionUpdate) -> Option<&mut String> {
    let chunk = match update {
        SessionUpdate::UserMessageChunk(chunk)
        | SessionUpdate::AgentMessageChunk(chunk)
        | SessionUpdate::AgentThoughtChunk(chunk) => chunk,
        _ => return None,
    };
    match &mut chunk.content {
        ContentBlock::Text(text) => Some(&mut text.text),
        _ => None,
    }
}

pub(super) fn next_text_boundary(text: &str, start: usize) -> usize {
    let mut end = start.saturating_add(ACP_TEXT_CHUNK_BYTES).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Distinct persisted rows may share the legacy timestamp/projector key. ACP's
/// messageId must use the persisted identity so clients cannot merge them.
pub(super) fn replay_row_id(task: &str, epoch: i64, row_seq: i64) -> String {
    format!("maple-row:{task}:{epoch}:{row_seq}")
}

/// Cancellation survives a dropped ACP future. The store also owns this lease
/// independently, so completed host drain closes scratch and wipes its key even
/// if an ACP task is detached while waiting for outbound credit.
pub(super) struct ReplayHistory {
    cancelled: Arc<AtomicBool>,
    snapshot: Option<Arc<ReplaySnapshot>>,
}

impl Drop for ReplayHistory {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(snapshot) = self.snapshot.take() {
            let _ = snapshot.close();
        }
    }
}

impl ReplayHistory {
    pub(super) async fn capture(
        agent: &AgentRuntimeHandle,
        store: Arc<Store>,
        account: AccountId,
        task: &str,
        expected: TaskHistoryStamp,
        cancellation: &CancellationToken,
    ) -> StoreResult<Self> {
        let mut history = Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            snapshot: None,
        };
        // Register before spawning: a queued blocking worker cannot escape a
        // concurrent account/store drain and start later in the new generation.
        let capture = store.prepare_replay_capture(
            account,
            task,
            expected,
            Arc::clone(&history.cancelled),
            CAPTURE_BUDGET,
        )?;
        agent
            .verify_generation()
            .await
            .map_err(|_| StoreError::NotFound)?;
        let mut worker = tokio::task::spawn_blocking(move || capture.run());
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                history.cancelled.store(true, Ordering::Release);
                // Do not detach a worker merely because its caller cancelled.
                // SQLite and per-row checks release the local WAL snapshot.
                let _ = worker.await;
                return Err(StoreError::Busy);
            }
            result = &mut worker => result.map_err(|_| StoreError::Busy)?,
        };
        let snapshot = result?;
        if snapshot.stamp() != expected {
            let _ = snapshot.close();
            return Err(StoreError::Conflict);
        }
        history.snapshot = Some(snapshot);
        if cancellation.is_cancelled() {
            return Err(StoreError::Busy);
        }
        Ok(history)
    }

    pub(super) async fn next_row(
        &self,
        cancellation: &CancellationToken,
    ) -> StoreResult<Option<(AgentTimelineItem, crate::agent::AgentPersistedRowIdentity)>> {
        let snapshot = Arc::clone(self.snapshot.as_ref().ok_or(StoreError::Busy)?);
        let mut worker = tokio::task::spawn_blocking(move || snapshot.next_row());
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                self.cancelled.store(true, Ordering::Release);
                let _ = worker.await;
                Err(StoreError::Busy)
            }
            result = &mut worker => result.map_err(|_| StoreError::Busy)?,
        }
    }
}

pub(super) fn history_error(
    error: StoreError,
    cancellation: &CancellationToken,
) -> agent_client_protocol::Error {
    if cancellation.is_cancelled() {
        return replay_error("Maple ACP history loading was cancelled");
    }
    replay_error(match error {
        StoreError::DiskFull => {
            "There is not enough disk space to prepare this history load. Its saved history is unchanged."
        }
        StoreError::Io {
            kind: std::io::ErrorKind::TimedOut,
        } => "Preparing this history snapshot took too long; try loading it again.",
        StoreError::Busy => {
            "Maple could not prepare another history snapshot right now; try loading it again."
        }
        StoreError::Locked => "Unlock Maple before loading this session.",
        StoreError::NotFound | StoreError::Conflict | StoreError::EpochChanged => {
            "This task or account changed while history was loading; reload the session."
        }
        _ => "Maple could not read the saved history snapshot; reload the session.",
    })
}

pub(super) fn replay_error(message: &'static str) -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error().data(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_frame_boundaries_preserve_unicode_and_json_escapes() {
        let original = "\0\n\"\\🦀é".repeat(100_000);
        let mut offset = 0;
        let mut reconstructed = String::new();
        while offset < original.len() {
            let end = next_text_boundary(&original, offset);
            let chunk = &original[offset..end];
            assert!(chunk.len() <= ACP_TEXT_CHUNK_BYTES);
            assert!(serde_json::to_vec(chunk).unwrap().len() <= ACP_TEXT_CHUNK_BYTES * 6 + 2);
            reconstructed.push_str(chunk);
            offset = end;
        }
        assert_eq!(reconstructed, original);
    }

    #[test]
    fn duplicate_legacy_message_keys_have_distinct_replay_ids() {
        assert_ne!(replay_row_id("task", 1, 1), replay_row_id("task", 1, 2));
        assert_ne!(replay_row_id("task", 1, 1), replay_row_id("task", 2, 1));
        assert_ne!(replay_row_id("task", 1, 1), replay_row_id("other", 1, 1));
    }
}
