//! Bounded transcript windows. View/cache keys use durable or live occurrence
//! identity; repeated timestamp merge keys remain distinct rows.

use super::{ChatScreen, LOAD_RETRIES, LoadMode};
use crate::backend::{HistoryAnchorRequest, RowRangeRequest};
use crate::ui::i18n::UiText;
use crate::ui::task::Failure as UiFailure;
use base64::Engine;
use gpui::{Context, ListOffset};
use maple_agent::agent::AgentTimelineItem;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

const MAX_ROWS: usize = 2000;
const MAX_BYTES: usize = 32 * 1024 * 1024;
/// A finished reply that streamed in and names a picture, but has no list
/// of saved pictures yet: rows read from history already carry theirs.
pub(super) fn wants_reply_pictures(item: &AgentTimelineItem) -> bool {
    item.item_type == "message"
        && item.role.as_deref() != Some("user")
        && item.live.is_some()
        && item.persisted.is_some()
        && item.input.is_none()
        && item.text.as_deref().is_some_and(|text| text.contains("!["))
}

/// Largest saved reply row read for its picture list.
const REPLY_PICTURES_ROW_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Default)]
pub(super) struct TranscriptPages {
    pub before: bool,
    pub after: bool,
    pub epoch: Option<i64>,
    pub loading: bool,
    /// The row count older history was last requested for near the top.
    /// Leaving the top rearms it, so a failure is not retried every frame.
    pub top_load: Option<usize>,
    /// The host had to omit or shorten some of the live overlay in its page.
    pub snapshot_partial: bool,
    /// Highest host revision skipped because the UI held an older window.
    pub missed_live_revision: Option<u64>,
    /// A skipped occurrence without a host revision needs a quiescent open.
    pub missed_live_without_revision: bool,
    pub generation: u64,
    pub expanding: HashSet<String>,
    pub weights: RowWeights,
}

/// What each loaded row counts against the byte budget, and the sum. A row
/// that changes adjusts the sum in place, so bounding the window after a
/// streamed chunk never rescans the transcript.
#[derive(Default)]
pub(super) struct RowWeights {
    by_id: HashMap<String, usize>,
    /// Always the sum of `by_id`.
    total: usize,
}

impl RowWeights {
    pub(super) fn total(&self) -> usize {
        self.total
    }

    /// Record what `item` weighs now, in place of what it weighed before.
    pub(super) fn set(&mut self, item: &AgentTimelineItem) {
        let bytes = item_bytes(item);
        match self.by_id.get_mut(&item.id) {
            Some(previous) => {
                self.total = self.total - *previous + bytes;
                *previous = bytes;
            }
            None => {
                self.by_id.insert(item.id.clone(), bytes);
                self.total += bytes;
            }
        }
    }

    /// Forget a row that left the window or took another key.
    pub(super) fn remove(&mut self, id: &str) {
        if let Some(bytes) = self.by_id.remove(id) {
            self.total -= bytes;
        }
    }

    pub(super) fn clear(&mut self) {
        self.by_id.clear();
        self.total = 0;
    }

    fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        let total = &mut self.total;
        self.by_id.retain(|id, bytes| {
            let kept = keep(id);
            if !kept {
                *total -= *bytes;
            }
            kept
        });
    }

    /// Weigh exactly the rows of `timeline`, for when rows arrived or left
    /// in bulk. A row weighed before keeps that weight: `set` refreshes it
    /// whenever the row changes.
    fn weigh(&mut self, timeline: &[AgentTimelineItem]) {
        let mut known = std::mem::take(&mut self.by_id);
        self.by_id.reserve(timeline.len());
        self.total = 0;
        for item in timeline {
            let (id, bytes) = known
                .remove_entry(&item.id)
                .unwrap_or_else(|| (item.id.clone(), item_bytes(item)));
            if let Entry::Vacant(slot) = self.by_id.entry(id) {
                slot.insert(bytes);
                self.total += bytes;
            }
        }
    }
}

impl TranscriptPages {
    pub(super) fn has_missed_live(&self) -> bool {
        self.missed_live_revision.is_some() || self.missed_live_without_revision
    }

    pub(super) fn has_live_gap(&self) -> bool {
        self.snapshot_partial || self.has_missed_live()
    }

    pub(super) fn note_missed_live(&mut self, revision: u64) {
        if revision == 0 {
            self.missed_live_without_revision = true;
        } else {
            self.missed_live_revision = Some(
                self.missed_live_revision
                    .map_or(revision, |previous| previous.max(revision)),
            );
        }
    }

    /// Reconcile one bounded open with occurrences deferred by the history gate.
    /// A partial page cannot prove that a deferred occurrence was recovered.
    pub(super) fn reconcile_snapshot(
        &mut self,
        timeline_revision: u64,
        snapshot_partial: bool,
        local_events_changed: bool,
    ) {
        if !snapshot_partial {
            if self
                .missed_live_revision
                .is_some_and(|missed| missed <= timeline_revision)
            {
                self.missed_live_revision = None;
            }
            if !local_events_changed {
                self.missed_live_without_revision = false;
            }
        }
        self.snapshot_partial = snapshot_partial;
    }

    /// Host revisions start over after recovery; buffered events re-mark any gap.
    pub(super) fn reset_live_recovery(&mut self) {
        self.snapshot_partial = false;
        self.missed_live_revision = None;
        self.missed_live_without_revision = false;
    }

    pub(super) fn reset_for_navigation(&mut self) {
        *self = Self {
            generation: self.generation.wrapping_add(1),
            ..Self::default()
        };
    }

    fn finish_loading(&mut self, generation: u64) -> bool {
        if self.generation != generation {
            return false;
        }
        self.loading = false;
        true
    }
}

pub(super) fn normalize(mut item: AgentTimelineItem) -> AgentTimelineItem {
    if let Some(key) = crate::backend::timeline_view_key(&item) {
        item.id = key;
    }
    item
}

fn value_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(s) => s.len() + 24,
        serde_json::Value::Array(items) => 24 + items.iter().map(value_bytes).sum::<usize>(),
        serde_json::Value::Object(items) => {
            48 + items
                .iter()
                .map(|(k, v)| k.len() + 32 + value_bytes(v))
                .sum::<usize>()
        }
        _ => 16,
    }
}

pub(super) fn item_bytes(item: &AgentTimelineItem) -> usize {
    256 + item.id.len()
        + item.text.as_ref().map_or(0, String::len)
        + item.title.as_ref().map_or(0, String::len)
        + item.input.as_ref().map_or(0, value_bytes)
        + item.output.as_ref().map_or(0, value_bytes)
}

impl ChatScreen {
    pub(super) fn transcript_scrolled(&mut self, start: usize, end: usize, cx: &mut Context<Self>) {
        self.request_visible_attachment_images(start, end, cx);
        if start < 5 && self.pages.before {
            self.load_history_page(true, cx);
        } else if end.saturating_add(5) >= self.timeline.len() && self.pages.after {
            self.load_history_page(false, cx);
        }
    }

    pub(super) fn load_history_page(&mut self, older: bool, cx: &mut Context<Self>) {
        if self.pages.loading
            || (older && !self.pages.before)
            || (!older && !self.pages.after)
            || (older && self.list_state.is_scrollbar_dragging())
        {
            return;
        }
        let Some(task) = self.selected_session.clone() else {
            return;
        };
        let identity = if older {
            self.timeline.iter().find_map(|item| item.persisted.clone())
        } else {
            self.timeline
                .iter()
                .rev()
                .find_map(|item| item.persisted.clone())
        };
        let Some(identity) = identity else { return };
        self.pages.loading = true;
        let generation = self.pages.generation;
        let backend = self.backend.clone();
        let user = self.user_id.clone();
        let target = task.clone();
        self.call_semantic(
            async move {
                let request = HistoryAnchorRequest {
                    task,
                    rows_epoch: identity.rows_epoch,
                    row_seq: identity.row_seq,
                    limit_rows: Some(200),
                    limit_bytes: None,
                };
                if older {
                    backend.call_presented::<maple_agent::store::HistoryPage>("history_before", (&user, request,)).await.map_err(UiFailure::from)
                } else {
                    backend.call_presented::<maple_agent::store::HistoryPage>("history_forward", (&user, request,)).await.map_err(UiFailure::from)
                }
            },
            cx,
            move |this, result, cx| {
                if !this.is_selected(&target) || !this.pages.finish_loading(generation) {
                    return;
                }
                match result {
                    Ok(page) => this.apply_history_page(older, &target, page, cx),
                    Err(error) => {
                        if matches!(&error, UiFailure::Host(failure) if matches!(failure.code.as_str(), "paging-changed" | "storage-epoch"))
                            || error.raw_message().is_some_and(|raw| raw.contains("epoch") || raw.contains("changed")) {
                            this.reload_timeline(&target, cx);
                        } else {
                            this.notice = Some(error.into());
                            cx.notify();
                        }
                    }
                }
            },
        );
    }

    /// Apply a reply from the currently selected task and page generation.
    fn apply_history_page(
        &mut self,
        older: bool,
        target: &str,
        page: maple_agent::store::HistoryPage,
        cx: &mut Context<Self>,
    ) {
        // A reset clears GPUI's frozen drag height while the scrollbar
        // still holds its original thumb coordinates. Drop the reply and
        // rearm prefetch instead; mouse-up renders and fetches it again.
        if older && self.list_state.is_scrollbar_dragging() {
            self.pages.top_load = None;
            return;
        }
        if self.pages.epoch != Some(page.rows_epoch) {
            self.reload_timeline(target, cx);
            return;
        }
        let following_tail = self.list_state.is_following_tail();
        let mut items = Vec::with_capacity(page.rows.len());
        for row in page.rows {
            let key = format!("stored:{}:{}:{}", target, row.rows_epoch, row.row_seq);
            if let Some(summary) = row.summary.clone() {
                self.tool_summaries.insert(key, summary.into());
            }
            items.push(normalize(crate::backend::persisted_item(target, row)));
        }
        if older {
            self.pages.before = page.next.is_some();
        } else {
            self.pages.after = page.next.is_some();
        }
        let recover_at_tail =
            !older && !self.pages.after && self.pages.has_missed_live() && following_tail;
        let anchor = self.history_page_anchor(recover_at_tail);
        let known: HashSet<_> = self.timeline.iter().map(|item| item.id.as_str()).collect();
        items.retain(|item| !known.contains(item.id.as_str()));
        if older {
            items.append(&mut self.timeline);
            self.timeline = items;
        } else {
            self.timeline.extend(items);
        }
        self.bound_transcript_window(!older);
        self.rebuild_window(anchor);
        self.warm_markdown(cx);
        self.load_attachment_images(cx);
        if recover_at_tail {
            self.recover_paged_latest(cx);
        }
        cx.notify();
    }

    /// Only automatic live-tail recovery should keep following a page
    /// rebuild. A normal forward page must not skip the rows being read,
    /// even at the temporary bottom of a partial history window.
    pub(super) fn history_page_anchor(
        &self,
        recover_at_tail: bool,
    ) -> Option<(String, ListOffset)> {
        if recover_at_tail {
            None
        } else {
            self.scroll_anchor()
        }
    }

    pub(super) fn scroll_anchor(&self) -> Option<(String, ListOffset)> {
        let offset = self.list_state.logical_scroll_top();
        self.timeline
            .get(offset.item_ix)
            .map(|item| (item.id.clone(), offset))
    }

    pub(super) fn rebuild_window(&mut self, anchor: Option<(String, ListOffset)>) {
        self.timeline_index = self
            .timeline
            .iter()
            .enumerate()
            .map(|(i, item)| (item.id.clone(), (i, 0)))
            .collect();
        self.turns.rebuild(&self.timeline);
        self.toggled_chunks
            .retain(|id| self.timeline_index.contains_key(id));
        self.turn_worked
            .retain(|id, _| self.timeline_index.contains_key(id));
        self.step_permissions
            .retain(|id, _| self.timeline_index.contains_key(id));
        self.turn_changes.clear();
        self.open_changes
            .retain(|id, _| self.timeline_index.contains_key(id));
        self.markdown_cache.clear();
        self.derived.clear();
        self.messages = Default::default();
        self.local_error_text
            .retain(|id, _| self.timeline_index.contains_key(id));
        self.tool_summaries
            .retain(|id, _| self.timeline_index.contains_key(id));
        self.unsaved_summaries
            .retain(|id| self.timeline_index.contains_key(id));
        self.output_summaries
            .retain(|id| self.timeline_index.contains_key(id));
        self.pages
            .weights
            .retain(|id| self.timeline_index.contains_key(id));
        self.list_state
            .reset_with_uniform_height(self.timeline.len(), super::TRANSCRIPT_ROW_ESTIMATE);
        if let Some((id, mut offset)) = anchor
            && let Some((index, _)) = self.timeline_index.get(&id)
        {
            offset.item_ix = *index;
            self.list_state.scroll_to(offset);
        }
    }

    /// Bound the window after rows arrived or left in bulk.
    pub(super) fn bound_transcript_window(&mut self, keep_tail: bool) {
        self.pages.weights.weigh(&self.timeline);
        self.evict_past_budget(keep_tail);
    }

    /// Bound the window after the row at `index` arrived or changed. Every
    /// other row keeps its weight, so this costs the same at any transcript
    /// length; streaming calls it once per chunk.
    pub(super) fn bound_after_row_change(&mut self, index: usize, keep_tail: bool) {
        self.pages.weights.set(&self.timeline[index]);
        self.evict_past_budget(keep_tail);
    }

    fn evict_past_budget(&mut self, keep_tail: bool) {
        let mut changed = false;
        while self.timeline.len() > MAX_ROWS
            || (self.pages.weights.total() > MAX_BYTES && self.timeline.len() > 1)
        {
            let item = if keep_tail {
                self.pages.before = true;
                self.timeline.remove(0)
            } else {
                self.pages.after = true;
                self.timeline.pop().unwrap()
            };
            self.local_error_text.remove(&item.id);
            self.pages.weights.remove(&item.id);
            changed = true;
        }
        if changed {
            self.release_all_pictures();
            self.attachment_requests.clear();
            self.attachment_failures.clear();
            self.attachment_queue.clear();
        }
    }

    pub(super) fn expand_paged_row(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(item) = self.timeline.iter().find(|item| item.id == id) else {
            return;
        };
        let Some(identity) = item.persisted.clone() else {
            self.notice = Some(UiText::key("chat-output-unsaved"));
            cx.notify();
            return;
        };
        if !self.pages.expanding.insert(id.into()) {
            return;
        }
        let backend = self.backend.clone();
        let user = self.user_id.clone();
        let task = identity.task_id.clone();
        let target = task.clone();
        let key = id.to_owned();
        let generation = self.pages.generation;
        self.call_semantic(
            async move {
                let mut request = RowRangeRequest {
                    task,
                    rows_epoch: identity.rows_epoch,
                    row_seq: identity.row_seq,
                    update_seq: Some(identity.update_seq),
                    offset: 0,
                    len: 0,
                };
                let meta = backend
                    .call_presented::<maple_agent::store::RowRange>(
                        "read_row",
                        (&user, request.clone()),
                    )
                    .await
                    .map_err(UiFailure::from)?;
                if meta.total > MAX_BYTES as u64 {
                    return Err(UiFailure::Local(
                        "chat-output-full-budget",
                        "Full output exceeds the 32 MiB view budget.",
                    ));
                }
                let mut bytes = Vec::with_capacity(meta.total as usize);
                while (bytes.len() as u64) < meta.total {
                    request.offset = bytes.len() as u64;
                    request.len = 1024 * 1024;
                    let range = backend
                        .call_presented::<maple_agent::store::RowRange>(
                            "read_row",
                            (&user, request.clone()),
                        )
                        .await
                        .map_err(UiFailure::from)?;
                    if range.update_seq != meta.update_seq || range.total != meta.total {
                        return Err(UiFailure::Local(
                            "chat-output-changed",
                            "The output changed; reopen it.",
                        ));
                    }
                    let part = base64::engine::general_purpose::STANDARD
                        .decode(range.data)
                        .map_err(|_| {
                            UiFailure::Local("chat-output-range-invalid", "Invalid output range")
                        })?;
                    if part.is_empty() {
                        return Err(UiFailure::Local(
                            "chat-output-range-stalled",
                            "The output range did not advance.",
                        ));
                    }
                    bytes.extend(part);
                }
                let mut item: AgentTimelineItem = serde_json::from_slice(&bytes)
                    .map_err(|_| UiFailure::Local("chat-output-invalid", "Invalid full output"))?;
                item.persisted = Some(identity);
                Ok::<_, UiFailure>(normalize(item))
            },
            cx,
            move |this, result, cx| {
                this.pages.expanding.remove(&key);
                if this.pages.generation != generation || !this.is_selected(&target) {
                    return;
                }
                match result {
                    Ok(item) => {
                        let Some((index, _)) = this.timeline_index.get(&key).copied() else {
                            return;
                        };
                        if this.timeline[index].persisted != item.persisted {
                            this.notice = Some(UiText::key("chat-output-changed"));
                        } else if this
                            .timeline
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| *i != index)
                            .map(|(_, item)| item_bytes(item))
                            .sum::<usize>()
                            .saturating_add(item_bytes(&item))
                            > MAX_BYTES
                        {
                            this.notice = Some(UiText::key("chat-output-budget"));
                        } else {
                            this.pages.weights.remove(&key);
                            this.local_error_text.remove(&key);
                            this.timeline[index] = item;
                            this.pages.weights.set(&this.timeline[index]);
                            // Restoring a full todo status can promote or
                            // demote an entire activity step and its chunk.
                            this.turns.rebuild(&this.timeline);
                            this.markdown_cache.clear();
                            this.derived.clear();
                            this.messages = Default::default();
                            // The whole input may change what its turn's
                            // card lists, and so the height of the reply
                            // that carries it.
                            this.turn_changes.clear();
                            this.list_state.remeasure_items(index..this.timeline.len());
                            this.warm_markdown(cx);
                        }
                    }
                    Err(error) => this.notice = Some(error.into()),
                }
                cx.notify();
            },
        );
    }

    /// A reply that streamed in carries no picture list: the execution
    /// computer copies its pictures as it saves the reply. Read that saved
    /// row once, for a reply that names a picture, and take its list.
    pub(super) fn read_reply_pictures(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.timeline.get(index) else {
            return;
        };
        let Some(identity) = item.persisted.clone() else {
            return;
        };
        if !wants_reply_pictures(item) || !self.picture_rows_read.insert(item.id.clone()) {
            return;
        }
        let backend = self.backend.clone();
        let user = self.user_id.clone();
        let key = item.id.clone();
        let target = identity.task_id.clone();
        let generation = self.pages.generation;
        self.call_semantic(
            async move {
                let mut request = RowRangeRequest {
                    task: identity.task_id.clone(),
                    rows_epoch: identity.rows_epoch,
                    row_seq: identity.row_seq,
                    update_seq: Some(identity.update_seq),
                    offset: 0,
                    len: 0,
                };
                let meta = backend
                    .call_presented::<maple_agent::store::RowRange>(
                        "read_row",
                        (&user, request.clone()),
                    )
                    .await
                    .map_err(UiFailure::from)?;
                // A reply this long is shown truncated anyway.
                if meta.total > REPLY_PICTURES_ROW_BYTES {
                    return Ok(None);
                }
                let mut bytes = Vec::with_capacity(meta.total as usize);
                while (bytes.len() as u64) < meta.total {
                    request.offset = bytes.len() as u64;
                    request.len = 1024 * 1024;
                    let range = backend
                        .call_presented::<maple_agent::store::RowRange>(
                            "read_row",
                            (&user, request.clone()),
                        )
                        .await
                        .map_err(UiFailure::from)?;
                    let part = base64::engine::general_purpose::STANDARD
                        .decode(range.data)
                        .ok()
                        .filter(|part| {
                            !part.is_empty()
                                && range.update_seq == meta.update_seq
                                && range.total == meta.total
                        });
                    let Some(part) = part else {
                        return Ok(None);
                    };
                    bytes.extend(part);
                }
                let saved: Option<AgentTimelineItem> = serde_json::from_slice(&bytes).ok();
                Ok::<_, UiFailure>(
                    saved.and_then(|saved| saved.input).filter(|input| {
                        input.get(maple_agent::store::MARKDOWN_IMAGES_KEY).is_some()
                    }),
                )
            },
            cx,
            move |this, result, cx| {
                let input = match result {
                    Ok(Some(input)) => input,
                    Ok(None) => return,
                    // A failed read is tried again on the row's next update.
                    Err(_) => {
                        this.picture_rows_read.remove(&key);
                        return;
                    }
                };
                if this.pages.generation == generation && this.is_selected(&target) {
                    this.apply_reply_pictures(&key, &target, input, cx);
                }
            },
        );
    }

    /// Give a streamed reply the picture list of its saved row.
    pub(super) fn apply_reply_pictures(
        &mut self,
        key: &str,
        task: &str,
        input: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let Some((index, revision)) = self.timeline_index.get_mut(key) else {
            return;
        };
        let index = *index;
        let row = &mut self.timeline[index];
        if row.persisted.as_ref().map(|saved| saved.task_id.as_str()) != Some(task)
            || row.input.is_some()
        {
            return;
        }
        *revision += 1;
        row.input = Some(input);
        let row = row.clone();
        self.remeasure_item(index);
        self.load_attachment_images_for(&row, cx);
        cx.notify();
    }

    /// Explicit Jump-to-latest recovery may replace the reader's window.
    pub(super) fn reload_paged_latest(&mut self, cx: &mut Context<Self>) {
        self.load_paged_latest(LoadMode::Reload, cx);
    }

    /// Automatic catch-up always yields to a reader who changed position.
    pub(super) fn recover_paged_latest(&mut self, cx: &mut Context<Self>) {
        self.load_paged_latest(LoadMode::Recover, cx);
    }

    fn load_paged_latest(&mut self, mode: LoadMode, cx: &mut Context<Self>) {
        if self.reload_pending {
            return;
        }
        if let Some(task) = self.selected_session.clone() {
            self.load_session(&task, mode, LOAD_RETRIES, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::AgentBackend;
    use gpui::AppContext;
    use maple_agent::agent::{
        AgentLiveRowIdentity, AgentPersistedRowIdentity, AgentSessionSummary, AgentTaskState,
    };
    use std::sync::Arc;

    fn chat_with_task(cx: &mut gpui::TestAppContext) -> gpui::Entity<ChatScreen> {
        let _settings = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        cx.new(|cx| {
            let mut chat = ChatScreen::new_without_start(backend, "user".into(), cx);
            chat.selected_session = Some("task".into());
            chat
        })
    }

    fn message(text: &str) -> AgentTimelineItem {
        serde_json::from_value(serde_json::json!({
            "id": "message", "itemType": "message", "text": text, "createdMs": 1,
            "merge": "replace"
        }))
        .unwrap()
    }

    fn stored(row_seq: i64, text: &str) -> AgentTimelineItem {
        AgentTimelineItem {
            persisted: Some(AgentPersistedRowIdentity {
                task_id: "task".into(),
                rows_epoch: 0,
                row_seq,
                update_seq: row_seq,
            }),
            ..message(text)
        }
    }

    fn live(occurrence: u64, text: &str) -> AgentTimelineItem {
        AgentTimelineItem {
            live: Some(AgentLiveRowIdentity {
                run_id: "run".into(),
                occurrence,
            }),
            ..message(text)
        }
    }

    fn history_page(row_seq: i64, next: Option<&str>) -> maple_agent::store::HistoryPage {
        maple_agent::store::HistoryPage {
            rows: vec![maple_agent::store::TimelineRow {
                rows_epoch: 0,
                row_seq,
                update_seq: row_seq,
                item: message("Older"),
                summary: None,
                summary_truncated: None,
            }],
            next: next.map(str::to_string),
            rows_epoch: 0,
        }
    }

    #[gpui::test]
    fn an_older_page_keeps_the_anchor_when_it_prepends(cx: &mut gpui::TestAppContext) {
        let chat = chat_with_task(cx);
        chat.update(cx, |chat, cx| {
            chat.replace_timeline(vec![stored(100, "Current")]);
            chat.pages.epoch = Some(0);
            chat.pages.before = true;
            chat.list_state.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: gpui::px(23.),
            });
            let anchored = chat.timeline[0].id.clone();

            chat.apply_history_page(true, "task", history_page(99, None), cx);

            assert_eq!(chat.timeline.len(), 2);
            assert_eq!(chat.timeline[1].id, anchored);
            assert!(!chat.pages.before);
            let offset = chat.list_state.logical_scroll_top();
            assert_eq!(offset.item_ix, 1);
            assert_eq!(offset.offset_in_item, gpui::px(23.));
        });
    }

    #[gpui::test]
    fn forward_pages_keep_readers_at_their_next_row(cx: &mut gpui::TestAppContext) {
        let chat = chat_with_task(cx);
        chat.update(cx, |chat, cx| {
            for missed_live in [false, true] {
                chat.pages = TranscriptPages::default();
                chat.replace_timeline(vec![stored(100, "Next row to read")]);
                chat.pages.epoch = Some(0);
                chat.pages.after = true;
                if missed_live {
                    chat.pages.note_missed_live(100);
                }
                // Set up the headless list with following and a real row
                // anchor. Setting Tail on a loaded list instead uses an
                // end-of-list sentinel, which is a different fixture.
                chat.list_state.reset_with_uniform_height(0, gpui::px(40.));
                chat.list_state.set_follow_mode(gpui::FollowMode::Tail);
                chat.list_state.scroll_to(ListOffset {
                    item_ix: 0,
                    offset_in_item: gpui::px(23.),
                });
                assert!(chat.list_state.is_following_tail());
                let anchor = chat.scroll_anchor().expect("the next row has a stable ID");

                chat.apply_history_page(false, "task", history_page(101, Some("more")), cx);

                assert!(chat.pages.after);
                assert_eq!(chat.timeline.len(), 2);
                assert_eq!(chat.timeline[0].text.as_deref(), Some("Next row to read"));
                assert_eq!(
                    chat.list_state.logical_scroll_top(),
                    anchor.1,
                    "the reader's row must not be skipped (missed live: {missed_live})"
                );
                assert!(!chat.list_state.is_following_tail());
                assert!(!chat.reload_pending, "no recovery before the live tail");
            }
        });
    }

    #[gpui::test]
    fn an_older_page_landing_mid_drag_is_retried_after_release(cx: &mut gpui::TestAppContext) {
        let chat = chat_with_task(cx);
        chat.update(cx, |chat, cx| {
            chat.replace_timeline(vec![stored(100, "Current")]);
            chat.pages.epoch = Some(0);
            chat.pages.before = true;
            chat.pages.top_load = Some(1);
            chat.list_state.scrollbar_drag_started();

            chat.apply_history_page(true, "task", history_page(99, Some("more")), cx);

            assert_eq!(chat.timeline.len(), 1, "the drag's list is untouched");
            assert!(chat.pages.before);
            assert_eq!(chat.pages.top_load, None);
            assert!(chat.list_state.is_scrollbar_dragging());

            chat.list_state.scrollbar_drag_ended();
            chat.load_history_page(true, cx);
            assert!(
                chat.pages.loading,
                "mouse-up allows the replacement request"
            );
        });
    }

    /// The running total must match a fresh sum over the loaded rows.
    #[track_caller]
    fn assert_weights_exact(chat: &ChatScreen) {
        assert_eq!(
            chat.pages.weights.total(),
            chat.timeline.iter().map(item_bytes).sum::<usize>()
        );
        assert_eq!(chat.pages.weights.by_id.len(), chat.timeline.len());
    }

    #[gpui::test]
    fn row_weights_follow_streamed_rekeyed_and_evicted_rows(cx: &mut gpui::TestAppContext) {
        let chat = chat_with_task(cx);
        chat.update(cx, |chat, _| {
            chat.replace_timeline((1..=3).map(|seq| stored(seq, "history")).collect());
            assert_weights_exact(chat);

            // A live row arrives, grows with each chunk, and can shrink.
            for text in ["An", "An answer", "An answer that grows", "Edited"] {
                chat.apply_timeline_item("task", live(1, text));
                assert_eq!(chat.timeline.len(), 4);
                assert_eq!(chat.timeline[3].text.as_deref(), Some(text));
                assert_weights_exact(chat);
            }

            // Its commit moves the same row to its stored key.
            let mut committed = stored(4, "Committed");
            committed.live = live(1, "").live;
            chat.apply_timeline_item("task", committed);
            assert_eq!(chat.timeline.len(), 4);
            assert_eq!(chat.timeline[3].id, "stored:task:0:4");
            assert_weights_exact(chat);

            // Rows past the cap push the oldest out of the window.
            for occurrence in 2..=(MAX_ROWS as u64 + 2) {
                chat.apply_timeline_item("task", live(occurrence, "step"));
            }
            assert_eq!(chat.timeline.len(), MAX_ROWS);
            assert!(chat.pages.before && !chat.pages.after);
            assert_eq!(chat.timeline[0].id, "live:run:3");
            assert_weights_exact(chat);
        });
    }

    #[gpui::test]
    fn a_streamed_row_past_the_byte_budget_evicts_the_oldest_row(cx: &mut gpui::TestAppContext) {
        let chat = chat_with_task(cx);
        chat.update(cx, |chat, _| {
            let large = "x".repeat(MAX_BYTES / 4);
            chat.replace_timeline((1..=3).map(|seq| stored(seq, &large)).collect());
            assert_eq!(chat.timeline.len(), 3);

            // The fourth quarter fits only until the row outgrows it.
            chat.apply_timeline_item("task", live(1, &large[..large.len() / 2]));
            assert_eq!(chat.timeline.len(), 4);
            assert!(!chat.pages.before);

            chat.apply_timeline_item("task", live(1, &large));
            assert_eq!(chat.timeline.len(), 3);
            assert_eq!(chat.timeline[0].id, "stored:task:0:2");
            assert_eq!(chat.timeline[2].text.as_deref(), Some(large.as_str()));
            assert!(chat.pages.before && !chat.pages.after);
            assert_weights_exact(chat);
        });
    }

    #[test]
    fn a_late_history_reply_cannot_finish_the_next_tasks_request() {
        let mut pages = TranscriptPages {
            generation: 8,
            before: true,
            after: true,
            epoch: Some(12),
            loading: true,
            snapshot_partial: true,
            missed_live_revision: Some(100),
            missed_live_without_revision: true,
            ..TranscriptPages::default()
        };
        let previous_request = pages.generation;
        pages.reset_for_navigation();
        assert!(!pages.before && !pages.after && !pages.loading);
        assert!(!pages.has_live_gap());
        assert_eq!(pages.epoch, None);
        pages.loading = true;
        assert!(!pages.finish_loading(previous_request));
        assert!(pages.loading, "stale completion cleared the new request");
        assert!(pages.finish_loading(pages.generation));
        assert!(!pages.loading);
    }

    #[test]
    fn a_snapshot_must_cover_a_deferred_revision_to_clear_it() {
        let mut pages = TranscriptPages::default();
        pages.note_missed_live(100);

        // An open that began before the gated event cannot clear its marker,
        // even when its returned live page itself is complete.
        pages.reconcile_snapshot(99, false, true);
        assert_eq!(pages.missed_live_revision, Some(100));
        assert!(pages.has_live_gap());

        pages.reconcile_snapshot(100, false, false);
        assert!(!pages.has_live_gap());
    }

    #[test]
    fn a_partial_snapshot_retains_the_gap_without_requesting_another_load() {
        let mut pages = TranscriptPages::default();
        pages.note_missed_live(100);
        pages.reconcile_snapshot(100, true, false);

        assert!(pages.snapshot_partial);
        assert!(pages.has_missed_live());

        // Run completion can provide a quiescent complete page later.
        pages.reconcile_snapshot(100, false, false);
        assert!(!pages.has_live_gap());
    }

    #[test]
    fn an_unrevisioned_occurrence_waits_for_a_quiescent_complete_snapshot() {
        let mut pages = TranscriptPages::default();
        pages.note_missed_live(0);

        pages.reconcile_snapshot(100, false, true);
        assert!(pages.missed_live_without_revision);
        pages.reconcile_snapshot(100, false, false);
        assert!(!pages.has_live_gap());

        pages.note_missed_live(100);
        pages.reconcile_snapshot(100, false, false);
        pages.reset_live_recovery();
        assert!(!pages.has_live_gap());
    }

    #[gpui::test]
    fn new_task_receives_live_rows_after_leaving_a_paged_back_task(cx: &mut gpui::TestAppContext) {
        let _settings = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let chat = cx.new(|cx| ChatScreen::new_without_start(backend, "user".into(), cx));
        chat.update(cx, |chat, cx| {
            chat.selected_session = Some("old-task".into());
            chat.pages.after = true;
            chat.pages.before = true;
            chat.pages.loading = true;
            chat.pages.epoch = Some(12);
            let old_request = chat.pages.generation;
            chat.new_session(cx);
            assert!(chat.pages.generation > old_request);
            assert!(!chat.pages.after && !chat.pages.before && !chat.pages.loading);
            let selection = chat.selection_generation;
            chat.finish_new_session(
                AgentSessionSummary {
                    id: "new-task".into(),
                    title: "New task".into(),
                    project_root: std::env::temp_dir().to_string_lossy().into_owned(),
                    created_ms: 1,
                    updated_ms: 1,
                    message_count: 1,
                    model: None,
                    thinking_level: None,
                    web_enabled: true,
                    permission_mode: Default::default(),
                    state: AgentTaskState::Active,
                    acp: false,
                },
                selection,
                cx,
            );
            let item: AgentTimelineItem = serde_json::from_value(serde_json::json!({
                "id":"first-message", "itemType":"message", "role":"user",
                "text":"Visible first message", "createdMs":1, "merge":"replace"
            }))
            .unwrap();
            chat.apply_incoming_item("new-task", item, cx);
            assert_eq!(chat.timeline.len(), 1);
            assert_eq!(
                chat.timeline[0].text.as_deref(),
                Some("Visible first message")
            );
            assert!(!chat.pages.after && !chat.pages.has_live_gap());
        });
    }
}
