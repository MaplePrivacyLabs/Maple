//! Background work that fills in a loaded transcript: one-line tool
//! summaries from the title model, and the attachment images the
//! runtime stores out of band.

use std::sync::Arc;

use gpui::{Context, SharedString};
use maple_agent::agent::AgentTimelineItem;

use super::ChatScreen;
use super::transcript::{attachment_refs, has_tool_input, tool_output_markdown};
use crate::ui::chat::images::image_format_from_bytes;

/// A thinking block shorter than this reads faster than its summary;
/// skip the model call.
const THINKING_SUMMARY_MIN_CHARS: usize = 200;

/// The text a summary request describes: the output of a tool call, the
/// text of a thinking block.
fn summary_source(item: &AgentTimelineItem) -> Option<String> {
    match item.item_type.as_str() {
        "thinking" | "reasoning" => item.text.clone().filter(|text| !text.trim().is_empty()),
        _ => tool_output_markdown(item),
    }
}

impl ChatScreen {
    /// Ask the title model for a one-line summary of the completed tool
    /// call at `index`. At most a few
    /// requests ride at once; the rest wait in `summary_queue`. Each item
    /// is only ever asked once per session visit.
    pub(super) fn maybe_summarize_tool(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.timeline.get(index) else {
            return;
        };
        if !matches!(item.item_type.as_str(), "tool" | "toolCall")
            || item.status.as_deref() != Some("completed")
            || self.tool_summaries.contains_key(&item.id)
            || self.summary_requests.contains(&item.id)
        {
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
        } else {
            None
        };
        if let Some(reason) = skip {
            log::debug!("Skipping tool summary for {}: {reason}", item.id);
            return;
        }
        let Some(output) = tool_output_markdown(item) else {
            log::debug!("Skipping tool summary for {}: no text output", item.id);
            return;
        };
        let item_id = item.id.clone();
        self.summary_requests.insert(item_id.clone());
        if self.pending_summaries >= 3 {
            self.summary_queue.push_back(item_id);
            return;
        }
        self.start_summary(item_id, output, cx);
    }

    /// Ask the title model for a one-line header of the thinking block at
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
        let Some(text) = summary_source(item) else {
            return;
        };
        if text.trim().len() < THINKING_SUMMARY_MIN_CHARS {
            return;
        }
        let item_id = item.id.clone();
        self.summary_requests.insert(item_id.clone());
        if self.pending_summaries >= 3 {
            self.summary_queue.push_back(item_id);
            return;
        }
        self.start_summary(item_id, text, cx);
    }

    /// Start queued summaries while a slot is free.
    fn drain_summary_queue(&mut self, cx: &mut Context<Self>) {
        while self.pending_summaries < 3 {
            let Some(item_id) = self.summary_queue.pop_front() else {
                return;
            };
            let Some(&(index, _)) = self.timeline_index.get(&item_id) else {
                continue;
            };
            let Some(output) = self.timeline.get(index).and_then(summary_source) else {
                continue;
            };
            self.start_summary(item_id, output, cx);
        }
    }

    fn start_summary(&mut self, item_id: String, output: String, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            self.summary_requests.remove(&item_id);
            return;
        };
        let Some(&(index, _)) = self.timeline_index.get(&item_id) else {
            self.summary_requests.remove(&item_id);
            return;
        };
        let item = &self.timeline[index];
        let thinking = matches!(item.item_type.as_str(), "thinking" | "reasoning");
        let input = item.input.clone();
        if !thinking && input.is_none() {
            self.summary_requests.remove(&item_id);
            return;
        }
        let tool_name = item.title.clone().unwrap_or_else(|| item.item_type.clone());
        log::debug!("Requesting summary for {item_id} ({tool_name})");
        let host = self.backend_for(&session_id);
        let generation = self.summary_generation;
        self.pending_summaries += 1;
        let store_id = item_id.clone();
        self.call(
            async move {
                let summary = if thinking {
                    host.summarize_thinking(session_id.clone(), output).await?
                } else {
                    host.summarize_tool_call(session_id.clone(), tool_name, input, output)
                        .await?
                };
                if let Some(summary) = &summary
                    && let Err(error) = host
                        .store_tool_summary(session_id.clone(), store_id, summary.clone())
                        .await
                {
                    log::warn!("Cannot store tool summary: {error}");
                }
                Ok::<_, String>(summary)
            },
            cx,
            move |this, result, cx| {
                // The screen moved to another session meanwhile; the slot
                // count restarted and this label has nowhere to go.
                if this.summary_generation != generation {
                    return;
                }
                this.pending_summaries = this.pending_summaries.saturating_sub(1);
                match result {
                    Ok(Some(summary)) => {
                        if let Some(&(index, _)) = this.timeline_index.get(&item_id) {
                            this.remeasure_item(index);
                        }
                        this.tool_summaries
                            .insert(item_id, SharedString::from(summary));
                        cx.notify();
                    }
                    Ok(None) => {
                        log::debug!("Tool summary for {item_id} came back empty");
                    }
                    Err(error) => {
                        // A failed request must not pin the raw title for
                        // the rest of the visit: forget it so the next
                        // reload or session switch asks again.
                        log::warn!("Tool summary for {item_id} failed: {error}");
                        this.summary_requests.remove(&item_id);
                    }
                }
                this.drain_summary_queue(cx);
            },
        );
    }

    /// Request summaries for the completed tool calls and finished
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

    /// Fetch the images behind sent attachments that are not decoded yet.
    /// Each id is requested once; the result re-measures the rows that
    /// show it. Scans the whole timeline: for a loaded snapshot only.
    pub(super) fn load_attachment_images(&mut self, cx: &mut Context<Self>) {
        let wanted: Vec<String> = self
            .timeline
            .iter()
            .flat_map(attachment_refs)
            .map(|(id, _)| id.to_string())
            .filter(|id| !self.attachment_requests.contains(id))
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
        let wanted: Vec<String> = attachment_refs(item)
            .map(|(id, _)| id.to_string())
            .filter(|id| !self.attachment_requests.contains(id))
            .collect();
        self.request_attachments(wanted, cx);
    }

    fn request_attachments(&mut self, wanted: Vec<String>, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        for id in wanted {
            self.attachment_requests.insert(id.clone());
            let host = self.session_backend();
            let session = session_id.clone();
            let attachment_id = id.clone();
            self.call(
                async move {
                    host.read_image_attachment(session.clone(), attachment_id.clone())
                        .await
                },
                cx,
                move |this, result, cx| match result {
                    Ok(bytes) => this.insert_attachment_image(&id, bytes, cx),
                    Err(message) => log::debug!("attachment {id} not loaded: {message}"),
                },
            );
        }
    }

    fn insert_attachment_image(&mut self, id: &str, bytes: Vec<u8>, cx: &mut Context<Self>) {
        let Some(format) = image_format_from_bytes(&bytes) else {
            return;
        };
        self.attachment_images.insert(
            id.to_string(),
            Arc::new(gpui::Image::from_bytes(format, bytes)),
        );
        // Rows that show this image change height; tell the list.
        let rows: Vec<usize> = self
            .timeline
            .iter()
            .enumerate()
            .filter(|(_, item)| attachment_refs(item).any(|(candidate, _)| candidate == id))
            .map(|(index, _)| index)
            .collect();
        for index in rows {
            self.remeasure_item(index);
        }
        cx.notify();
    }
}
