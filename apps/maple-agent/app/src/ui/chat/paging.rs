//! Bounded transcript windows. View/cache keys use durable or live occurrence
//! identity; repeated timestamp merge keys remain distinct rows.

use super::ChatScreen;
use crate::backend::{HistoryAnchorRequest, RowRangeRequest};
use base64::Engine;
use gpui::{Context, ListOffset};
use maple_agent::agent::AgentTimelineItem;
use std::collections::{HashMap, HashSet};

const MAX_ROWS: usize = 2000;
const MAX_BYTES: usize = 32 * 1024 * 1024;

#[derive(Default)]
pub(super) struct TranscriptPages {
    pub before: bool,
    pub after: bool,
    pub epoch: Option<i64>,
    pub loading: bool,
    pub live_has_more: bool,
    pub generation: u64,
    pub expanding: HashSet<String>,
    pub weights: HashMap<String, usize>,
}

impl TranscriptPages {
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
        if self.pages.loading || (older && !self.pages.before) || (!older && !self.pages.after) {
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
        self.call(
            async move {
                let request = HistoryAnchorRequest {
                    task,
                    rows_epoch: identity.rows_epoch,
                    row_seq: identity.row_seq,
                    limit_rows: Some(200),
                    limit_bytes: None,
                };
                if older {
                    backend.history_before_anchor(&user, request).await
                } else {
                    backend.history_forward_after(&user, request).await
                }
            },
            cx,
            move |this, result, cx| {
                if !this.is_selected(&target) || !this.pages.finish_loading(generation) {
                    return;
                }
                match result {
                    Ok(page) => {
                        if this.pages.epoch != Some(page.rows_epoch) {
                            this.reload_timeline(&target, cx);
                            return;
                        }
                        let anchor = this.scroll_anchor();
                        let mut items = Vec::with_capacity(page.rows.len());
                        for row in page.rows {
                            let key =
                                format!("stored:{}:{}:{}", target, row.rows_epoch, row.row_seq);
                            if let Some(summary) = row.summary.clone() {
                                this.tool_summaries.insert(key, summary.into());
                            }
                            items.push(normalize(crate::backend::persisted_item(&target, row)));
                        }
                        if older {
                            this.pages.before = page.next.is_some();
                        } else {
                            this.pages.after = page.next.is_some();
                        }
                        let known: HashSet<_> =
                            this.timeline.iter().map(|item| item.id.as_str()).collect();
                        items.retain(|item| !known.contains(item.id.as_str()));
                        if older {
                            items.append(&mut this.timeline);
                            this.timeline = items;
                        } else {
                            this.timeline.extend(items);
                        }
                        this.bound_transcript_window(!older);
                        this.rebuild_window(anchor);
                        this.warm_markdown(cx);
                        this.load_attachment_images(cx);
                        cx.notify();
                    }
                    Err(error) => {
                        if error.contains("epoch") || error.contains("changed") {
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
        self.markdown_cache.clear();
        self.derived.clear();
        self.tool_summaries
            .retain(|id, _| self.timeline_index.contains_key(id));
        self.unsaved_summaries
            .retain(|id| self.timeline_index.contains_key(id));
        self.output_summaries
            .retain(|id| self.timeline_index.contains_key(id));
        self.pages
            .weights
            .retain(|id, _| self.timeline_index.contains_key(id));
        self.list_state
            .reset_with_uniform_height(self.timeline.len(), super::TRANSCRIPT_ROW_ESTIMATE);
        if let Some((id, mut offset)) = anchor
            && let Some((index, _)) = self.timeline_index.get(&id)
        {
            offset.item_ix = *index;
            self.list_state.scroll_to(offset);
        }
    }

    pub(super) fn bound_transcript_window(&mut self, keep_tail: bool) {
        let mut bytes = self
            .timeline
            .iter()
            .map(|item| {
                *self
                    .pages
                    .weights
                    .entry(item.id.clone())
                    .or_insert_with(|| item_bytes(item))
            })
            .sum::<usize>();
        let mut changed = false;
        while self.timeline.len() > MAX_ROWS || (bytes > MAX_BYTES && self.timeline.len() > 1) {
            let item = if keep_tail {
                self.pages.before = true;
                self.timeline.remove(0)
            } else {
                self.pages.after = true;
                self.timeline.pop().unwrap()
            };
            bytes = bytes.saturating_sub(item_bytes(&item));
            changed = true;
        }
        if changed {
            self.attachment_images.clear();
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
            self.notice = Some("Full output is available after this message is saved.".into());
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
        self.call(
            async move {
                let mut request = RowRangeRequest {
                    task,
                    rows_epoch: identity.rows_epoch,
                    row_seq: identity.row_seq,
                    update_seq: Some(identity.update_seq),
                    offset: 0,
                    len: 0,
                };
                let meta = backend.read_row(&user, request.clone()).await?;
                if meta.total > MAX_BYTES as u64 {
                    return Err("Full output exceeds the 32 MiB view budget.".into());
                }
                let mut bytes = Vec::with_capacity(meta.total as usize);
                while (bytes.len() as u64) < meta.total {
                    request.offset = bytes.len() as u64;
                    request.len = 1024 * 1024;
                    let range = backend.read_row(&user, request.clone()).await?;
                    if range.update_seq != meta.update_seq || range.total != meta.total {
                        return Err("The output changed; reopen it.".into());
                    }
                    let part = base64::engine::general_purpose::STANDARD
                        .decode(range.data)
                        .map_err(|_| "Invalid output range")?;
                    if part.is_empty() {
                        return Err("The output range did not advance.".into());
                    }
                    bytes.extend(part);
                }
                let mut item: AgentTimelineItem =
                    serde_json::from_slice(&bytes).map_err(|_| "Invalid full output")?;
                item.persisted = Some(identity);
                Ok::<_, String>(normalize(item))
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
                            this.notice = Some("The output changed; reopen it.".into());
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
                            this.notice =
                                Some("Full output exceeds the current 32 MiB view budget.".into());
                        } else {
                            this.pages.weights.remove(&key);
                            this.timeline[index] = item;
                            this.markdown_cache.clear();
                            this.derived.clear();
                            this.remeasure_item(index);
                            this.warm_markdown(cx);
                        }
                    }
                    Err(error) => this.notice = Some(error.into()),
                }
                cx.notify();
            },
        );
    }

    pub(super) fn reload_paged_latest(&mut self, cx: &mut Context<Self>) {
        if let Some(task) = self.selected_session.clone() {
            self.reload_timeline(&task, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::AgentBackend;
    use gpui::AppContext;
    use maple_agent::agent::{AgentSessionSummary, AgentTaskState};
    use std::sync::Arc;

    #[test]
    fn a_late_history_reply_cannot_finish_the_next_tasks_request() {
        let mut pages = TranscriptPages {
            generation: 8,
            before: true,
            after: true,
            epoch: Some(12),
            loading: true,
            live_has_more: true,
            ..TranscriptPages::default()
        };
        let previous_request = pages.generation;
        pages.reset_for_navigation();
        assert!(!pages.before && !pages.after && !pages.loading && !pages.live_has_more);
        assert_eq!(pages.epoch, None);
        pages.loading = true;
        assert!(!pages.finish_loading(previous_request));
        assert!(pages.loading, "stale completion cleared the new request");
        assert!(pages.finish_loading(pages.generation));
        assert!(!pages.loading);
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
                    web_enabled: true,
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
            assert!(!chat.pages.after && !chat.pages.live_has_more);
        });
    }
}
