//! Background work that fills in a loaded transcript: one-line tool
//! summaries from the summary model, and the attachment images the
//! runtime stores out of band.

use std::{collections::HashSet, sync::Arc};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use gpui::{Context, SharedString};
use maple_agent::agent::AgentTimelineItem;

use super::ChatScreen;
use super::transcript::{has_tool_input, image_attachment_refs, tool_output_markdown};
use crate::backend::{
    AgentBackend, ReadAttachmentRequest, StoreToolSummaryRequest, ToolStartSummary,
};
use crate::ui::chat::attachments::image_format_from_bytes;

/// A thinking block shorter than this reads faster than its summary;
/// skip the model call.
const THINKING_SUMMARY_MIN_CHARS: usize = 200;
const ATTACHMENT_CHUNK_BYTES: usize = 1024 * 1024;
// Generated/tool images can be 20 MiB; user image admission remains 10 MiB.
const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;
const MAX_CACHED_ATTACHMENT_BYTES: usize = 32 * 1024 * 1024;
const MAX_CACHED_ATTACHMENTS: usize = 64;
const MAX_ATTACHMENT_READS: usize = 4;
const ATTACHMENT_OVERDRAW_ROWS: usize = 5;
const INITIAL_ATTACHMENT_ROWS: usize = 20;

/// Summary requests that ride at once; the rest wait in the queue.
const SUMMARY_SLOTS: usize = 3;

/// The text of a thinking block, which its summary describes.
fn thinking_text(item: &AgentTimelineItem) -> Option<String> {
    item.text.clone().filter(|text| !text.trim().is_empty())
}

/// What one summary request asks the summary model about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SummaryAsk {
    Thinking(String),
    /// A tool call from its input, as it starts.
    ToolStart,
    /// A finished tool call with its output, for a host that does not
    /// summarize from input.
    ToolOutput(String),
}

/// Test seam: summary model requests and stores are recorded instead of
/// sent, so a test delivers each reply when it chooses.
#[cfg(test)]
#[derive(Default)]
pub(super) struct SummaryProbe {
    /// Token, the row's key when asked, and what was asked.
    pub(super) requests: Vec<(u64, String, SummaryAsk)>,
    pub(super) saves: Vec<StoreToolSummaryRequest>,
}

impl ChatScreen {
    /// Ask the summary model for a one-line summary of the tool call at
    /// `index` as soon as its row carries the model's full input; the
    /// summary describes the call, so it does not wait for the output. A
    /// call the host could not summarize from its input is asked again once
    /// it completes, with its output. Each item is only ever asked once per
    /// session visit.
    pub(super) fn maybe_summarize_tool(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.timeline.get(index) else {
            return;
        };
        if !matches!(item.item_type.as_str(), "tool" | "toolCall")
            || self.tool_summaries.contains_key(&item.id)
            || self.summary_requests.contains(&item.id)
        {
            return;
        }
        // The call's completion asks again.
        let from_output = self.output_summaries.contains(&item.id);
        if from_output && item.status.as_deref() != Some("completed") {
            return;
        }
        let skip = if !self.summaries_enabled {
            Some("summaries are off in settings")
        } else if has_tool_input(item, "todos") {
            Some("todo list")
        } else if super::transcript::has_external_agent_activity(item) {
            // The row draws the agent's own activity; a one-line summary
            // would hide what it did.
            Some("external agent activity")
        } else if item.input.as_ref().is_none_or(serde_json::Value::is_null) {
            Some("no input")
        } else if from_output && tool_output_markdown(item).is_none() {
            Some("no text output")
        } else {
            None
        };
        if let Some(reason) = skip {
            log::debug!("Skipping tool summary for {}: {reason}", item.id);
            return;
        }
        let item_id = item.id.clone();
        self.request_summary(item_id, cx);
    }

    /// Ask the summary model for a one-line header of the thinking block at
    /// `index`, once it is final: a later item follows it or its run
    /// ended. Shares the request slots and store with tool summaries.
    pub(super) fn maybe_summarize_thinking(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.timeline.get(index) else {
            return;
        };
        if !matches!(item.item_type.as_str(), "thinking" | "reasoning")
            || self.tool_summaries.contains_key(&item.id)
            || self.summary_requests.contains(&item.id)
            || !self.summaries_enabled
        {
            return;
        }
        // The newest item of an active run is still streaming; it gets
        // its summary when the next item or the run's end finalizes it.
        if index + 1 == self.timeline.len() && self.is_run_active() {
            return;
        }
        if thinking_text(item).is_none_or(|text| text.trim().len() < THINKING_SUMMARY_MIN_CHARS) {
            return;
        }
        let item_id = item.id.clone();
        self.request_summary(item_id, cx);
    }

    fn request_summary(&mut self, item_id: String, cx: &mut Context<Self>) {
        self.summary_requests.insert(item_id.clone());
        if self.summary_inflight.len() >= SUMMARY_SLOTS {
            self.summary_queue.push_back(item_id);
            return;
        }
        self.start_summary(item_id, cx);
    }

    /// Start queued summaries while a slot is free.
    fn drain_summary_queue(&mut self, cx: &mut Context<Self>) {
        while self.summary_inflight.len() < SUMMARY_SLOTS {
            let Some(item_id) = self.summary_queue.pop_front() else {
                return;
            };
            self.start_summary(item_id, cx);
        }
    }

    fn start_summary(&mut self, item_id: String, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            self.summary_requests.remove(&item_id);
            return;
        };
        let Some(&(index, _)) = self.timeline_index.get(&item_id) else {
            self.summary_requests.remove(&item_id);
            return;
        };
        let item = &self.timeline[index];
        let ask = if matches!(item.item_type.as_str(), "thinking" | "reasoning") {
            thinking_text(item).map(SummaryAsk::Thinking)
        } else if item.input.is_none() {
            None
        } else if !self.output_summaries.contains(&item_id) {
            Some(SummaryAsk::ToolStart)
        } else if item.status.as_deref() == Some("completed") {
            tool_output_markdown(item).map(SummaryAsk::ToolOutput)
        } else {
            // The call's completion asks again.
            None
        };
        let Some(ask) = ask else {
            self.summary_requests.remove(&item_id);
            return;
        };
        let input = item.input.clone();
        let tool_name = item.title.clone().unwrap_or_else(|| item.item_type.clone());
        let from = match &ask {
            SummaryAsk::Thinking(_) => "thinking",
            SummaryAsk::ToolStart => "input",
            SummaryAsk::ToolOutput(_) => "output",
        };
        log::debug!("Requesting summary for {item_id} ({tool_name}) from {from}");
        // The token names the request; `summary_inflight` maps it to the
        // row's current key, which a live row's alias may change.
        let token = self.next_summary_token;
        self.next_summary_token += 1;
        self.summary_inflight.insert(token, item_id.clone());
        #[cfg(test)]
        if let Some(probe) = &mut self.summary_probe {
            probe.requests.push((token, item_id, ask));
            return;
        }
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let generation = self.summary_generation;
        self.call(
            async move {
                match ask {
                    SummaryAsk::Thinking(text) => backend
                        .summarize_thinking(&user_id, &session_id, text)
                        .await
                        .map(ToolStartSummary::Label),
                    SummaryAsk::ToolStart => {
                        backend
                            .summarize_starting_tool_call(&user_id, &session_id, tool_name, input)
                            .await
                    }
                    SummaryAsk::ToolOutput(output) => backend
                        .summarize_tool_call(&user_id, &session_id, tool_name, input, output)
                        .await
                        .map(ToolStartSummary::Label),
                }
            },
            cx,
            move |this, result, cx| this.finish_summary(token, generation, result, cx),
        );
    }

    /// One summary request answered. It lands on the row its token now
    /// names, even if that row became durable meanwhile.
    pub(super) fn finish_summary(
        &mut self,
        token: u64,
        generation: u64,
        result: Result<ToolStartSummary, String>,
        cx: &mut Context<Self>,
    ) {
        // The screen moved to another session meanwhile; the slots
        // restarted and this label has nowhere to go.
        if self.summary_generation != generation {
            return;
        }
        let Some(item_id) = self.summary_inflight.remove(&token) else {
            return;
        };
        match result {
            Ok(ToolStartSummary::Label(Some(summary))) => {
                self.tool_summaries
                    .insert(item_id.clone(), SharedString::from(summary));
                if let Some(&(index, _)) = self.timeline_index.get(&item_id) {
                    self.remeasure_item(index);
                    // A call summarized as it starts may not have a
                    // durable row yet; it is stored once it does.
                    self.unsaved_summaries.insert(item_id);
                    self.save_summary(index, cx);
                }
                cx.notify();
            }
            Ok(ToolStartSummary::Label(None)) => {
                log::debug!("Tool summary for {item_id} came back empty");
            }
            Ok(ToolStartSummary::NeedsOutput) => {
                log::debug!("Summary for {item_id} waits for its output");
                // The host does not summarize from input, or its support is
                // unknown: this call is summarized from its output, now if it
                // already finished, else on completion. The next call checks
                // support again.
                self.output_summaries.insert(item_id.clone());
                self.summary_requests.remove(&item_id);
                if let Some(&(index, _)) = self.timeline_index.get(&item_id) {
                    self.maybe_summarize_tool(index, cx);
                }
            }
            Err(error) => {
                // A failed request must not pin the raw title for the rest
                // of the visit: forget it so the next update, reload or
                // session switch asks again.
                log::warn!("Tool summary for {item_id} failed: {error}");
                self.summary_requests.remove(&item_id);
            }
        }
        self.drain_summary_queue(cx);
    }

    /// A row's key changed from `old` to `new` (its live row became
    /// durable). Its summary state moves with it, so a queued or in-flight
    /// request lands on the row as it is now and the new key never asks
    /// again.
    pub(super) fn rekey_summary(&mut self, old: &str, new: &str) {
        if let Some(summary) = self.tool_summaries.remove(old) {
            self.tool_summaries.insert(new.to_string(), summary);
        }
        for keys in [
            &mut self.summary_requests,
            &mut self.unsaved_summaries,
            &mut self.output_summaries,
        ] {
            if keys.remove(old) {
                keys.insert(new.to_string());
            }
        }
        for key in self
            .summary_queue
            .iter_mut()
            .chain(self.summary_inflight.values_mut())
        {
            if key == old {
                *key = new.to_string();
            }
        }
    }

    /// Store the summary of the row at `index` once the row is durable. A
    /// legacy item id can repeat, so it is stored only through the exact
    /// durable occurrence; until then it stays on screen only.
    pub(super) fn save_summary(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.timeline.get(index) else {
            return;
        };
        let (Some(row), Some(summary)) = (&item.persisted, self.tool_summaries.get(&item.id))
        else {
            return;
        };
        if !self.unsaved_summaries.remove(&item.id) {
            return;
        }
        let request = StoreToolSummaryRequest {
            task: row.task_id.clone(),
            rows_epoch: row.rows_epoch,
            row_seq: row.row_seq,
            summary: summary.to_string(),
        };
        #[cfg(test)]
        if let Some(probe) = &mut self.summary_probe {
            probe.saves.push(request);
            return;
        }
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.save_tool_summary(&user_id, request).await },
            cx,
            |_, result, _| {
                if let Err(error) = result {
                    log::warn!("Cannot store tool summary: {error}");
                }
            },
        );
    }

    /// Request summaries for the tool calls and finished
    /// thinking blocks of the loaded timeline (session switch or reload).
    pub(super) fn summarize_loaded_tools(&mut self, cx: &mut Context<Self>) {
        if self.selected_session.is_none() {
            return;
        }
        let first = self.timeline.len().saturating_sub(40);
        for index in (first..self.timeline.len()).rev() {
            self.maybe_summarize_tool(index, cx);
            self.maybe_summarize_thinking(index, cx);
        }
    }

    /// Prime only the tail before layout. Scroll events request the actual
    /// viewport and a small overdraw, including newly prepended history.
    pub(super) fn load_attachment_images(&mut self, cx: &mut Context<Self>) {
        self.request_attachment_rows(
            self.timeline.len().saturating_sub(INITIAL_ATTACHMENT_ROWS),
            self.timeline.len(),
            cx,
        );
    }

    pub(super) fn request_visible_attachment_images(
        &mut self,
        start: usize,
        end: usize,
        cx: &mut Context<Self>,
    ) {
        let start = start
            .saturating_sub(ATTACHMENT_OVERDRAW_ROWS)
            .min(self.timeline.len());
        let end = end
            .saturating_add(ATTACHMENT_OVERDRAW_ROWS)
            .min(self.timeline.len());
        self.request_attachment_rows(start, end.max(start), cx);
    }

    fn request_attachment_rows(&mut self, start: usize, end: usize, cx: &mut Context<Self>) {
        self.attachment_viewport = start..end;
        let visible: HashSet<String> = self.timeline[start..end]
            .iter()
            .flat_map(image_attachment_refs)
            .map(|(id, _)| id.to_string())
            .collect();
        // A fast scroll must not leave a backlog downloading distant pages.
        // Reads already in flight keep their slots until their completion.
        let requested = &mut self.attachment_requests;
        self.attachment_queue.retain(|id| {
            if visible.contains(id) {
                true
            } else {
                requested.remove(id);
                false
            }
        });
        let wanted: Vec<String> = visible
            .into_iter()
            .filter(|id| {
                !self.attachment_images.contains_key(id) && !self.attachment_requests.contains(id)
            })
            .collect();
        self.request_attachments(wanted, cx);
    }

    /// Same for one arriving item, so a streaming run does not rescan the
    /// timeline on every event.
    pub(super) fn load_attachment_images_for(
        &mut self,
        item: &AgentTimelineItem,
        cx: &mut Context<Self>,
    ) {
        // New tail rows are useful only while following the tail. Repaints of
        // older rows wait until the viewport reaches them.
        if !self.list_state.is_following_tail() {
            let Some(&(index, _)) = self.timeline_index.get(&item.id) else {
                return;
            };
            if !self.attachment_viewport.contains(&index) {
                return;
            }
        }
        let wanted: Vec<String> = image_attachment_refs(item)
            .map(|(id, _)| id.to_string())
            .filter(|id| {
                !self.attachment_images.contains_key(id) && !self.attachment_requests.contains(id)
            })
            .collect();
        self.request_attachments(wanted, cx);
    }

    fn request_attachments(&mut self, wanted: Vec<String>, cx: &mut Context<Self>) {
        for id in wanted {
            if self.attachment_requests.insert(id.clone()) {
                self.attachment_queue.push_back(id);
            }
        }
        self.drain_attachment_queue(cx);
    }

    fn drain_attachment_queue(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        while self.pending_attachments < MAX_ATTACHMENT_READS {
            let Some(id) = self.attachment_queue.pop_front() else {
                break;
            };
            if self.attachment_images.contains_key(&id) {
                continue;
            }
            self.pending_attachments += 1;
            let backend = self.backend.clone();
            let user_id = self.user_id.clone();
            let session = session_id.clone();
            let attachment_id = id.clone();
            let generation = self.selection_generation;
            let target = session_id.clone();
            self.call(
                async move {
                    read_attachment_image(backend, user_id, session, attachment_id).await
                },
                cx,
                move |this, result, cx| {
                    this.pending_attachments = this.pending_attachments.saturating_sub(1);
                    if this.selection_generation == generation && this.is_selected(&target) {
                        match result {
                            Ok(image) => this.insert_attachment_image(&id, image, cx),
                            Err(_) => {
                                this.attachment_failures.insert(id.clone());
                                let rows: Vec<_> = this.timeline.iter().enumerate().filter(|(_, item)|
                                    image_attachment_refs(item).any(|(candidate, _)| candidate == id))
                                    .map(|(index, _)| index).collect();
                                for index in rows { this.remeasure_item(index); }
                                cx.notify();
                            },
                        }
                    }
                    // Stale reads still occupied a slot; their completion can
                    // now admit a request for the task currently on screen.
                    this.drain_attachment_queue(cx);
                },
            );
        }
    }

    fn insert_attachment_image(
        &mut self,
        id: &str,
        image: Arc<gpui::Image>,
        cx: &mut Context<Self>,
    ) {
        let start = self.attachment_viewport.start.min(self.timeline.len());
        let end = self
            .attachment_viewport
            .end
            .max(start)
            .min(self.timeline.len());
        let visible: HashSet<String> = self.timeline[start..end]
            .iter()
            .flat_map(image_attachment_refs)
            .map(|(id, _)| id.to_string())
            .collect();
        self.attachment_images.remove(id);
        let mut cached: usize = self
            .attachment_images
            .values()
            .map(|image| image.bytes.capacity())
            .sum();
        let mut evictions: Vec<String> = self
            .attachment_images
            .keys()
            .filter(|id| !visible.contains(*id))
            .cloned()
            .collect();
        evictions.extend(
            self.attachment_images
                .keys()
                .filter(|id| visible.contains(*id))
                .cloned(),
        );
        let mut changed = HashSet::from([id.to_string()]);
        for candidate in evictions {
            if cached.saturating_add(image.bytes.capacity()) <= MAX_CACHED_ATTACHMENT_BYTES
                && self.attachment_images.len() < MAX_CACHED_ATTACHMENTS
            {
                break;
            }
            if let Some(evicted) = self.attachment_images.remove(&candidate) {
                cached = cached.saturating_sub(evicted.bytes.capacity());
                self.attachment_requests.remove(&candidate);
                changed.insert(candidate);
            }
        }
        self.attachment_images.insert(id.to_string(), image);
        // Re-measure only rows at the viewport; an offscreen evicted image is
        // requested and measured when that row becomes visible again.
        let rows: Vec<usize> = (start..end)
            .filter(|&index| {
                image_attachment_refs(&self.timeline[index])
                    .any(|(candidate, _)| changed.contains(candidate))
            })
            .collect();
        for index in rows {
            self.remeasure_item(index);
        }
        cx.notify();
    }
}

async fn read_attachment_image(
    backend: Arc<AgentBackend>,
    user: String,
    task: String,
    attachment: String,
) -> Result<Arc<gpui::Image>, String> {
    let mut bytes = Vec::new();
    let mut expected = None;
    let mut mime = None;
    loop {
        let reply = backend
            .read_attachment(
                &user,
                ReadAttachmentRequest {
                    task: task.clone(),
                    attachment: attachment.clone(),
                    offset: bytes.len() as u64,
                    len: ATTACHMENT_CHUNK_BYTES,
                },
            )
            .await?;
        let size =
            usize::try_from(reply.size).map_err(|_| "Attachment size is invalid".to_string())?;
        if size == 0 || size > MAX_ATTACHMENT_BYTES {
            return Err("Attachment exceeds the image size limit".into());
        }
        if expected.is_some_and(|expected| expected != size)
            || mime.as_ref().is_some_and(|mime| mime != &reply.mime)
        {
            return Err("Attachment changed while loading".into());
        }
        if !matches!(
            reply.mime.as_str(),
            "image/png" | "image/jpeg" | "image/webp"
        ) {
            return Err("Attachment is not a supported image".into());
        }
        if expected.is_none() {
            bytes
                .try_reserve_exact(size)
                .map_err(|_| "Attachment could not be allocated".to_string())?;
        }
        expected = Some(size);
        mime = Some(reply.mime);
        if reply.data.len() > ATTACHMENT_CHUNK_BYTES.div_ceil(3) * 4 {
            return Err("Attachment range exceeds its size limit".into());
        }
        let chunk = tokio::task::spawn_blocking(move || {
            STANDARD
                .decode(reply.data)
                .map_err(|_| "Attachment encoding is invalid".to_string())
        })
        .await
        .map_err(|_| "Attachment decoding did not complete".to_string())??;
        if chunk.is_empty()
            || chunk.len() > ATTACHMENT_CHUNK_BYTES
            || bytes.len().saturating_add(chunk.len()) > size
        {
            return Err("Attachment range is incomplete".into());
        }
        bytes.extend(chunk);
        if bytes.len() == size {
            break;
        }
    }
    tokio::task::spawn_blocking(move || {
        bytes.shrink_to_fit();
        let format = image_format_from_bytes(&bytes)
            .ok_or_else(|| "Attachment is not a supported image".to_string())?;
        Ok::<_, String>(Arc::new(gpui::Image::from_bytes(format, bytes)))
    })
    .await
    .map_err(|_| "Image preparation did not complete".to_string())?
}
