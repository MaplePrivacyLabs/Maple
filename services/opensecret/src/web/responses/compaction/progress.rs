//! Advisory streaming metadata for compaction work. Only publication supplies a
//! completed item; a sampled or abandoned candidate is never a checkpoint.

use std::sync::Mutex;

use tokio::{sync::mpsc, time::Instant};
use uuid::Uuid;

use super::CompactionOutput;
use crate::web::responses::handlers::StorageMessage;

const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Internal notifications share the ordinary ordered client channel. Abandonment
/// only removes provisional client state; it does not introduce a wire event.
#[derive(Debug, Clone)]
pub enum CompactionProgress {
    Started { item_id: Uuid },
    Compacting { item_id: Uuid },
    Finished(CompactionOutput),
    Abandoned { item_id: Uuid },
}

#[derive(Default)]
pub(super) struct ProgressPublisher {
    state: Mutex<ProgressState>,
}

#[derive(Default)]
struct ProgressState {
    sender: Option<mpsc::Sender<StorageMessage>>,
    pending: Option<PendingItem>,
    last_progress_at: Option<Instant>,
}

struct PendingItem {
    id: Uuid,
}

impl ProgressState {
    fn begin(&mut self) -> (Uuid, Option<CompactionProgress>) {
        if let Some(pending) = &self.pending {
            return (pending.id, None);
        }
        let item_id = Uuid::new_v4();
        self.pending = Some(PendingItem { id: item_id });
        (item_id, Some(CompactionProgress::Started { item_id }))
    }

    /// Called only after a valid provider chunk added visible summary text.
    fn sampled(&mut self, now: Instant) -> Option<CompactionProgress> {
        let item_id = self.pending.as_ref()?.id;
        // One cadence spans the entire logical Response, including sequential
        // checkpoint prefixes, tool turns, helper fragments, and replans.
        if self
            .last_progress_at
            .is_some_and(|previous| now.saturating_duration_since(previous) < PROGRESS_INTERVAL)
        {
            return None;
        }
        self.last_progress_at = Some(now);
        Some(CompactionProgress::Compacting { item_id })
    }

    fn finish(&mut self, output: CompactionOutput) -> Option<CompactionProgress> {
        // An unrelated or repeated publication cannot complete this lifecycle.
        if self
            .pending
            .as_ref()
            .is_none_or(|pending| pending.id != output.id)
        {
            return None;
        }
        self.pending = None;
        Some(CompactionProgress::Finished(output))
    }

    fn abandon(&mut self) -> Option<CompactionProgress> {
        self.pending
            .take()
            .map(|pending| CompactionProgress::Abandoned {
                item_id: pending.id,
            })
    }
}

impl ProgressPublisher {
    pub(super) fn set_sender(&self, sender: mpsc::Sender<StorageMessage>) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .sender = Some(sender);
    }

    pub(super) async fn begin(&self) -> Uuid {
        let (item_id, event, sender) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let (item_id, event) = state.begin();
            (item_id, event, state.sender.clone())
        };
        Self::send(sender, event).await;
        item_id
    }

    pub(super) fn item_id(&self) -> Option<Uuid> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pending
            .as_ref()
            .map(|pending| pending.id)
    }

    pub(super) async fn sampled(&self) {
        let (event, sender) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (state.sampled(Instant::now()), state.sender.clone())
        };
        Self::send(sender, event).await;
    }

    pub(super) async fn finish(&self, output: CompactionOutput) {
        let (event, sender) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (state.finish(output), state.sender.clone())
        };
        Self::send(sender, event).await;
    }

    pub(super) async fn abandon(&self) {
        let (event, sender) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (state.abandon(), state.sender.clone())
        };
        Self::send(sender, event).await;
    }

    async fn send(sender: Option<mpsc::Sender<StorageMessage>>, event: Option<CompactionProgress>) {
        if let (Some(sender), Some(event)) = (sender, event) {
            // A disconnected caller does not cancel an owned Response. The
            // enclosing execution deadline/cancellation bounds backpressure.
            let _ = sender.send(StorageMessage::CompactionProgress(event)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(id: Uuid) -> CompactionOutput {
        CompactionOutput {
            id,
            encrypted_content: "opaque-checkpoint".into(),
            after_item_uuid: None,
        }
    }

    #[test]
    fn identity_only_compaction_has_lifecycle_without_a_sampling_event() {
        let mut state = ProgressState::default();
        let (id, started) = state.begin();
        assert!(matches!(started, Some(CompactionProgress::Started { item_id }) if item_id == id));
        assert!(
            matches!(state.finish(output(id)), Some(CompactionProgress::Finished(item)) if item.id == id)
        );
        assert!(state.pending.is_none());
    }

    #[test]
    fn progress_is_rate_limited_across_helper_fragments_and_replans() {
        let now = Instant::now();
        let mut state = ProgressState::default();
        let (id, _) = state.begin();
        assert!(
            matches!(state.sampled(now), Some(CompactionProgress::Compacting { item_id }) if item_id == id)
        );
        assert!(state
            .sampled(now + PROGRESS_INTERVAL - std::time::Duration::from_nanos(1))
            .is_none());
        assert!(
            matches!(state.sampled(now + PROGRESS_INTERVAL), Some(CompactionProgress::Compacting { item_id }) if item_id == id)
        );
        let (same_id, duplicate) = state.begin();
        assert_eq!(same_id, id);
        assert!(duplicate.is_none());
        assert!(state
            .sampled(now + PROGRESS_INTERVAL + std::time::Duration::from_secs(1))
            .is_none());
        assert!(
            matches!(state.sampled(now + PROGRESS_INTERVAL * 2), Some(CompactionProgress::Compacting { item_id }) if item_id == id)
        );
    }

    #[test]
    fn abandoning_unpublished_work_never_synthesizes_a_completed_checkpoint() {
        let mut state = ProgressState::default();
        let (id, _) = state.begin();
        assert!(state.finish(output(Uuid::new_v4())).is_none());
        assert!(
            matches!(state.abandon(), Some(CompactionProgress::Abandoned { item_id }) if item_id == id)
        );
        assert!(state.finish(output(id)).is_none());
        assert!(state.abandon().is_none());
        assert!(state.sampled(Instant::now() + PROGRESS_INTERVAL).is_none());
        let (next, _) = state.begin();
        assert_ne!(id, next);
    }

    #[test]
    fn a_published_checkpoint_closes_only_once_and_next_work_gets_a_new_id() {
        let mut state = ProgressState::default();
        let (id, _) = state.begin();
        assert!(matches!(
            state.finish(output(id)),
            Some(CompactionProgress::Finished(_))
        ));
        assert!(state.finish(output(id)).is_none());
        let (next, started) = state.begin();
        assert_ne!(id, next);
        assert!(
            matches!(started, Some(CompactionProgress::Started { item_id }) if item_id == next)
        );
    }

    #[test]
    fn publishing_a_prefix_never_resets_the_logical_response_progress_rate() {
        let now = Instant::now();
        let mut state = ProgressState::default();
        let (first, _) = state.begin();
        assert!(state.sampled(now).is_some());
        assert!(state.finish(output(first)).is_some());
        let (second, _) = state.begin();
        assert_ne!(first, second);
        assert!(state.sampled(now + PROGRESS_INTERVAL / 2).is_none());
        assert!(matches!(
            state.sampled(now + PROGRESS_INTERVAL),
            Some(CompactionProgress::Compacting { item_id }) if item_id == second
        ));
    }
}
