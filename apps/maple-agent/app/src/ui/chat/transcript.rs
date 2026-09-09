//! The transcript list and the row renderers behind it. Rows read the
//! shared `TranscriptCtx` caches; nothing here parses or derives on a
//! frame.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{Div, Entity, IntoElement, SharedString, Window, div, prelude::*, px};
use maple_agent::agent::{AgentTimelineItem, compaction_notice_text};

use super::cache::{MAX_DIFF_LINES, MarkdownKind};
use super::commands::ChatCommand;
use super::speech::speak_message_button;
use super::{CONTENT_WIDTH, ChatScreen, TranscriptCtx};
use crate::backend::PendingPermission;

use crate::ui::icons::{icon, spinner, spinner_with_id};
use crate::ui::markdown;
use crate::ui::motion;
use crate::ui::rich_text::{self, RenderCtx};
use crate::ui::text_input::TextInput;
use crate::ui::theme;
use crate::ui::widgets;

impl ChatScreen {
    pub(super) fn render_transcript(&mut self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        // The list follows its own tail: it snaps to the end on each
        // layout until the user scrolls up, and re-engages when the view
        // returns to the bottom. Every mutation goes through splice or
        // remeasure, so the count only drifts if a code path forgot to.
        let count = self.timeline.len();
        if self.list_state.item_count() != count {
            self.list_state
                .reset_with_uniform_height(count, super::TRANSCRIPT_ROW_ESTIMATE);
        }
        let show_jump = count > 0 && !self.list_state.is_following_tail();
        let tool_details = self.tool_details;
        let entity = cx.entity().downgrade();
        let selection = self.selection.clone();
        let transcript_focus = self.transcript_focus.clone();
        // Only the visible items (plus a small overdraw) are built each
        // frame; the list measures and caches the rest. Everything else is
        // read through the entity so nothing is cloned per frame.
        let list = gpui::list(self.list_state.clone(), move |ix, window, cx| {
            let Some(chat_entity) = entity.upgrade() else {
                return div().into_any_element();
            };
            let chat = chat_entity.read(cx);
            match chat.timeline.get(ix) {
                Some(item) => {
                    let render_ctx = RenderCtx {
                        selection: selection.clone(),
                        base_ordinal: Some(chat.markdown_cache.ordinal_for(&item.id)),
                        focus: transcript_focus.clone(),
                        id_seed: item.id.clone(),
                        view: Some(window.current_view()),
                    };
                    let transcript = TranscriptCtx {
                        markdown_cache: &chat.markdown_cache,
                        derived: &chat.derived,
                        attachment_images: &chat.attachment_images,
                        chat: &entity,
                        tool_summaries: &chat.tool_summaries,
                        render: &render_ctx,
                        speech: chat.speech.as_ref(),
                        speech_available: chat.audio_caps.speech,
                        position: (ix + 1, chat.timeline.len()),
                    };
                    let expanded = tool_details != chat.toggled_tools.contains(&item.id);
                    let revision = chat
                        .timeline_index
                        .get(&item.id)
                        .map_or(0, |(_, revision)| *revision);
                    let row = render_timeline_item(item, revision, expanded, &transcript);
                    if !chat.application_vim_enabled {
                        row.into_any_element()
                    } else {
                        let application_selected = chat.application_vim_selects_timeline(&item.id);
                        let application_item_id = item.id.clone();
                        let application_entity = entity.clone();
                        div()
                            .on_mouse_down(gpui::MouseButton::Left, move |_event, window, cx| {
                                let Some(chat_entity) = application_entity.upgrade() else {
                                    return;
                                };
                                chat_entity.update(cx, |chat, cx| {
                                    chat.select_timeline_from_pointer(
                                        &application_item_id,
                                        window,
                                        cx,
                                    );
                                });
                            })
                            .when(application_selected, |row| {
                                row.rounded(theme::RADIUS_SM)
                                    .border_l_2()
                                    .border_color(gpui::rgb(theme::accent()))
                            })
                            .child(row)
                            .into_any_element()
                    }
                }
                None => div().into_any_element(),
            }
        })
        .size_full();
        div()
            .id("transcript")
            .role(gpui::Role::Log)
            .aria_label("Conversation")
            .key_context("Transcript")
            .when_some(self.transcript_focus.clone(), |div, focus| {
                div.track_focus(&focus)
            })
            .relative()
            .flex_1()
            .flex()
            .flex_col()
            .min_h_0()
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    if let Some(focus) = &this.transcript_focus {
                        window.focus(focus, cx);
                    }
                    this.transcript_menu = Some(event.position);
                    cx.notify();
                }),
            )
            .children(self.render_transcript_menu(cx))
            .child(
                // The list element does not apply padding itself, so the
                // gutter lives here. Same column width as the composer.
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .max_w(CONTENT_WIDTH)
                    .mx_auto()
                    .px_6()
                    .pb_4()
                    .child(list),
            )
            .child(crate::ui::scrollbar::scrollbar(
                "transcript-scrollbar",
                self.list_state.clone(),
            ))
            .when(show_jump, |container| {
                container.child(self.render_jump_to_latest(cx))
            })
            .when_some(self.runtime_error.clone(), |container, error| {
                container.child(
                    widgets::banner(theme::status_error())
                        .mx_6()
                        .mb_2()
                        .child(error),
                )
            })
            .children(self.render_notice(cx).map(|notice| notice.mx_6().mb_2()))
    }

    /// Pill over the bottom edge while the view is scrolled up, so a
    /// reader can return to the newest message with one click.
    fn render_jump_to_latest(&self, cx: &mut Context<Self>) -> Div {
        div()
            .absolute()
            .bottom_3()
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(motion::rise_in(
                widgets::secondary_button("jump-to-latest")
                    .py_1p5()
                    .gap_1()
                    .shadow_md()
                    .bg(gpui::rgb(theme::bg_elevated()))
                    .border_1()
                    .border_color(gpui::rgb(theme::border()))
                    .text_xs()
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.list_state.scroll_to_end();
                        cx.notify();
                    }))
                    .child(icon("chevron-down", px(12.), theme::text_secondary()))
                    .child("Jump to latest"),
                "jump-to-latest-reveal",
            ))
    }
}

pub(super) fn render_timeline_item(
    item: &AgentTimelineItem,
    revision: u64,
    expanded: bool,
    transcript: &TranscriptCtx,
) -> gpui::AnyElement {
    let item_id = item.id.clone();
    let (role, label): (Option<gpui::Role>, Option<&'static str>) = match item.item_type.as_str() {
        "message" if item.role.as_deref() == Some("user") => {
            (Some(gpui::Role::Article), Some("You"))
        }
        "message" => (Some(gpui::Role::Article), Some("Maple")),
        "error" => (Some(gpui::Role::Alert), None),
        _ => (None, None),
    };
    let item = match item.item_type.as_str() {
        "message" => render_message(item, revision, transcript),
        "thinking" | "reasoning" => render_thinking(item, revision, expanded, transcript),
        "tool" | "toolCall" => {
            // Dispatch on payload shape; runtime titles are humanized
            // ("todo write", "ask user") and vary by detail suffix.
            if has_tool_input(item, "todos") {
                // The pinned plan card above the composer shows the list.
                return div().into_any_element();
            } else if has_tool_input(item, "edits")
                || (has_tool_input(item, "content") && has_tool_input(item, "path"))
            {
                render_tool_with_diff(item, revision, expanded, transcript)
            } else {
                render_tool(item, revision, expanded, transcript)
            }
        }
        "error" => render_error(item),
        "permission" => render_permission_row(item),
        _ => render_system(item),
    };
    // Per-item spacing (instead of a container gap) keeps non-renderable
    // items from producing phantom gaps. Rows a screen reader should
    // announce get a role, a sender, and their position in the
    // conversation; the others stay plain containers.
    let row = div().pb_2().child(item);
    match role {
        Some(role) => {
            let (position, count) = transcript.position;
            row.id(SharedString::from(format!("timeline-row-{item_id}")))
                .role(role)
                .when_some(label, |row, label| row.aria_label(label))
                .accessibility_id(item_id)
                .aria_position_in_set(position)
                .aria_size_of_set(count)
                .into_any_element()
        }
        None => row.into_any_element(),
    }
}

/// Attachment `(id, name)` pairs stored on a user message.
pub(super) fn attachment_refs(item: &AgentTimelineItem) -> impl Iterator<Item = (&str, &str)> {
    item.input
        .as_ref()
        .and_then(|input| input.get("imageAttachments"))
        .and_then(|items| items.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(|id| id.as_str())?;
            let name = entry.get("name").and_then(|name| name.as_str())?;
            Some((id, name))
        })
}

/// Hover-revealed button that copies one message's text.
fn copy_message_button(
    item_id: &str,
    group: &SharedString,
    text: SharedString,
    view: Option<gpui::EntityId>,
) -> gpui::Stateful<Div> {
    widgets::copy_button(
        SharedString::from(format!("copy-message-{item_id}")),
        text,
        Some(group),
        view,
    )
}

/// When a message was sent, revealed beside its actions on hover: the
/// clock for today, otherwise the date too.
fn timestamp_label(item: &AgentTimelineItem, group: &SharedString) -> Div {
    let sent = chrono::DateTime::<chrono::Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(item.created_ms as u64),
    );
    let text = if sent.date_naive() == chrono::Local::now().date_naive() {
        sent.format("%H:%M").to_string()
    } else {
        sent.format("%b %-d, %H:%M").to_string()
    };
    div()
        .text_xs()
        .text_color(gpui::rgb(theme::text_faint()))
        .opacity(0.)
        .group_hover(group.clone(), |style| style.opacity(1.))
        .child(text)
}

fn render_message(item: &AgentTimelineItem, revision: u64, transcript: &TranscriptCtx) -> Div {
    let ctx = transcript.render;
    let attachment_images = transcript.attachment_images;
    let chat = transcript.chat;
    let is_user = item.role.as_deref() == Some("user");
    let text = item.text.as_deref().unwrap_or("");
    let mut attachments = attachment_refs(item).peekable();
    let has_images = attachments.peek().is_some();
    if text.trim().is_empty() && !(is_user && has_images) {
        return div();
    }
    let group = SharedString::from(format!("message-{}", item.id));
    // The display text is already shaped and cached for this revision;
    // the buttons share it instead of copying the message per frame.
    let display = transcript.derived.get(item, revision).text.clone();
    let copy = (!text.trim().is_empty())
        .then(|| copy_message_button(&item.id, &group, display.clone(), transcript.render.view));
    if is_user {
        let user_ctx = RenderCtx {
            selection: ctx.selection.clone(),
            base_ordinal: ctx.base_ordinal.map(|base| base + 2048),
            focus: ctx.focus.clone(),
            id_seed: format!("{}#user", ctx.id_seed),
            view: ctx.view,
        };
        let ordinal = user_ctx.base_ordinal;
        div()
            .group(group.clone())
            .flex()
            .flex_col()
            .items_end()
            .gap_0p5()
            .child(
                div()
                    .max_w(gpui::relative(0.75))
                    .px_4()
                    .py_2()
                    .rounded(theme::RADIUS_MD)
                    .bg(gpui::rgb(theme::bg_user_bubble()))
                    .border_1()
                    .border_color(gpui::rgb(theme::user_bubble_border()))
                    .text_color(gpui::rgb(theme::text_primary()))
                    .when(has_images, |bubble| {
                        bubble.child(div().flex().flex_wrap().gap_2().mb_1().children(
                            attachments.map(|(id, name)| {
                                match attachment_images.get(id) {
                                    // The picture itself, scaled to fit; click
                                    // opens it full size. gpui keeps the aspect
                                    // ratio from the decoded size.
                                    Some(image) => {
                                        let click_image = Arc::clone(image);
                                        let chat = chat.clone();
                                        div().child(
                                            div()
                                                .id(gpui::SharedString::from(format!(
                                                    "attachment-{id}"
                                                )))
                                                .hover(|style| style.cursor_pointer())
                                                .on_click(
                                                    move |_event, _window, cx: &mut gpui::App| {
                                                        chat.update(cx, |chat, cx| {
                                                            chat.open_lightbox(
                                                                Arc::clone(&click_image),
                                                                cx,
                                                            );
                                                        })
                                                        .ok();
                                                    },
                                                )
                                                .child(
                                                    gpui::img(gpui::ImageSource::Image(
                                                        Arc::clone(image),
                                                    ))
                                                    .max_w(px(320.))
                                                    .max_h(px(240.))
                                                    .rounded(theme::RADIUS_SM)
                                                    .overflow_hidden()
                                                    .object_fit(gpui::ObjectFit::Contain)
                                                    .border_1()
                                                    .border_color(gpui::rgb(theme::border())),
                                                ),
                                        )
                                    }
                                    // Name chip until the bytes arrive (or if they
                                    // never do, such as a deleted attachment).
                                    None => div()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .px_2()
                                        .py_0p5()
                                        .rounded(theme::RADIUS_SM)
                                        .bg(gpui::rgb(theme::bg_elevated()))
                                        .text_xs()
                                        .text_color(gpui::rgb(theme::text_secondary()))
                                        .child(icon("paperclip", px(12.), theme::text_secondary()))
                                        .child(name.to_string()),
                                }
                            }),
                        ))
                    })
                    .when(!text.trim().is_empty(), |bubble| {
                        bubble.child(rich_text::plain_paragraph(
                            transcript.derived.get(item, revision).text.clone(),
                            ordinal,
                            &user_ctx,
                        ))
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(timestamp_label(item, &group))
                    .children(copy),
            )
    } else {
        div()
            .group(group.clone())
            .max_w_full()
            .pr_2()
            .flex()
            .flex_col()
            .gap_0p5()
            .text_color(gpui::rgb(theme::text_primary()))
            .child(markdown::render_with(
                &transcript
                    .markdown_cache
                    .get(&item.id, MarkdownKind::Body, revision, text),
                ctx,
            ))
            .children(copy.map(|button| {
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(button)
                    .child(timestamp_label(item, &group))
                    .when(transcript.speech_available, |row| {
                        let speech = transcript.speech.filter(|speech| speech.item_id == item.id);
                        row.child(speak_message_button(
                            &item.id,
                            &group,
                            display.clone(),
                            speech,
                            chat.clone(),
                        ))
                    })
            }))
    }
}

fn render_thinking(
    item: &AgentTimelineItem,
    revision: u64,
    expanded: bool,
    transcript: &TranscriptCtx,
) -> Div {
    let text = transcript.derived.get(item, revision).text.clone();
    if text.trim().is_empty() {
        return div().child(
            div()
                .text_color(gpui::rgb(theme::status_running()))
                .text_sm()
                .child("Thinking…"),
        );
    }
    let item_id = item.id.clone();
    let chat = transcript.chat.clone();
    // The model summary stands in for the generic "Thinking" label.
    let title = transcript
        .tool_summaries
        .get(&item.id)
        .cloned()
        .unwrap_or_else(|| SharedString::from("Thinking"));
    // Only the header toggles, so clicks in the body still select text.
    let header = div()
        .id(gpui::SharedString::from(format!(
            "thinking-toggle-{}",
            item.id
        )))
        .role(gpui::Role::DisclosureTriangle)
        .aria_label(title.clone())
        .aria_expanded(expanded)
        .flex()
        .items_center()
        .gap_1()
        .text_sm()
        .text_color(gpui::rgb(theme::text_muted()))
        .hover(|style| {
            style
                .cursor_pointer()
                .text_color(gpui::rgb(theme::text_secondary()))
        })
        .on_click(move |_event, _window, cx: &mut gpui::App| {
            chat.update(cx, |chat, cx| {
                chat.toggle_tool(&item_id, cx);
            })
            .ok();
        })
        .child(icon(
            if expanded {
                "chevron-down"
            } else {
                "chevron-right"
            },
            px(14.),
            theme::text_muted(),
        ))
        .child(div().line_clamp(1).text_ellipsis_middle().child(title));
    let card = div()
        .px_3()
        .py_2()
        .rounded(theme::RADIUS_SM)
        .bg(gpui::rgb(theme::bg_elevated()))
        .flex()
        .flex_col()
        .gap_1()
        .child(header);
    if !expanded {
        return card;
    }
    card.child(
        div()
            .text_sm()
            .text_color(gpui::rgb(theme::text_secondary()))
            .child(markdown::render_with(
                &transcript
                    .markdown_cache
                    .get(&item.id, MarkdownKind::Body, revision, &text),
                transcript.render,
            )),
    )
}

/// Goose's runtime strings rebranded for Maple users, who never see goose.
pub(super) fn maple_display_text(text: &str) -> std::borrow::Cow<'_, str> {
    std::borrow::Cow::Borrowed(compaction_notice_text(text).unwrap_or(text))
}

fn tool_status_style(status: Option<&str>) -> (&'static str, u32) {
    match status {
        Some("completed") => ("completed", theme::status_success()),
        Some("failed") | Some("error") => ("failed", theme::status_error()),
        Some("cancelled") | Some("controlled_externally") => ("stopped", theme::text_muted()),
        _ => ("running", theme::status_running()),
    }
}

/// True when the tool call input has the given top-level key.
pub(super) fn has_tool_input(item: &AgentTimelineItem, key: &str) -> bool {
    item.input
        .as_ref()
        .and_then(|value| value.as_object())
        .is_some_and(|map| map.contains_key(key))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

/// One row of a todo_write list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PlanEntry {
    pub(super) content: SharedString,
    pub(super) status: PlanStatus,
}

/// One subagent working for the selected task.
#[derive(Clone, Debug)]
pub(super) struct ActiveSubagent {
    /// Request ID of the `delegate` call that started it.
    pub(super) id: String,
    /// What the subagent was asked to do.
    pub(super) task: SharedString,
    /// It works in the background; the task collects the result later.
    pub(super) background: bool,
    /// When the card first showed it, for the elapsed time.
    pub(super) started: std::time::Instant,
    /// The tool it called most recently, if any.
    pub(super) activity: Option<SharedString>,
}

/// The todo list carried by a todo_write tool item, or `None` for any
/// other item.
pub(super) fn plan_entries(item: &AgentTimelineItem) -> Option<Vec<PlanEntry>> {
    if !matches!(item.item_type.as_str(), "tool" | "toolCall") {
        return None;
    }
    let todos = item.input.as_ref()?.get("todos")?.as_array()?;
    Some(
        todos
            .iter()
            .map(|todo| PlanEntry {
                content: todo
                    .get("content")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string()
                    .into(),
                status: match todo.get("status").and_then(|value| value.as_str()) {
                    Some("completed") => PlanStatus::Completed,
                    Some("in_progress") => PlanStatus::InProgress,
                    _ => PlanStatus::Pending,
                },
            })
            .collect(),
    )
}

/// One row of the subagent card: what the subagent was asked to do, the
/// tool it is running now, and how long it has worked.
pub(super) fn render_subagent_row(subagent: &ActiveSubagent, now: std::time::Instant) -> Div {
    let elapsed = now.saturating_duration_since(subagent.started);
    let mut row = div()
        .flex()
        .items_center()
        .gap_2()
        .child(spinner(&subagent.id, px(12.), theme::status_running()))
        .child(
            div()
                .flex_none()
                .max_w(px(260.))
                .text_sm()
                .text_color(gpui::rgb(theme::text_primary()))
                .truncate()
                .child(subagent.task.clone()),
        );
    if subagent.background {
        row = row.child(
            div()
                .flex_none()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child("background"),
        );
    }
    row.child(
        div()
            .flex_1()
            .min_w_0()
            .text_xs()
            .text_color(gpui::rgb(theme::text_secondary()))
            .truncate()
            .child(
                subagent
                    .activity
                    .clone()
                    .unwrap_or_else(|| SharedString::new_static("Starting")),
            ),
    )
    .child(
        div()
            .flex_none()
            .text_xs()
            .text_color(gpui::rgb(theme::text_muted()))
            .child(format_subagent_elapsed(elapsed)),
    )
}

/// `m:ss` while a subagent is under an hour, `h:mm:ss` after that.
pub(super) fn format_subagent_elapsed(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
    if hours == 0 {
        format!("{minutes}:{seconds:02}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}")
    }
}

pub(super) fn render_plan_row(entry: &PlanEntry) -> Div {
    let completed = entry.status == PlanStatus::Completed;
    let checkbox = div()
        .flex_none()
        .size(px(14.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.))
        .border_1()
        .map(|checkbox| match entry.status {
            PlanStatus::Completed => checkbox
                .border_color(gpui::rgb(theme::status_success()))
                .bg(gpui::rgb(theme::status_success()))
                .child(icon("check", px(11.), theme::on_accent())),
            PlanStatus::InProgress => checkbox
                .border_color(gpui::rgb(theme::status_running()))
                .child(
                    div()
                        .size(px(6.))
                        .rounded(px(1.))
                        .bg(gpui::rgb(theme::status_running())),
                ),
            PlanStatus::Pending => checkbox.border_color(gpui::rgb(theme::text_muted())),
        });
    div().flex().items_center().gap_2().child(checkbox).child(
        div()
            .text_sm()
            .text_color(gpui::rgb(if completed {
                theme::text_muted()
            } else {
                theme::text_primary()
            }))
            .when(completed, |text| text.line_through())
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .child(entry.content.clone()),
    )
}

/// +/- lines of an edit or write tool input, stopping at the display cap.
pub(super) fn diff_lines_for(item: &AgentTimelineItem) -> Vec<(char, SharedString)> {
    let mut lines: Vec<(char, SharedString)> = Vec::new();
    let Some(serde_json::Value::Object(map)) = item.input.as_ref() else {
        return lines;
    };
    let push = |lines: &mut Vec<(char, SharedString)>, sign: char, text: &str| {
        for line in text.lines() {
            if lines.len() >= MAX_DIFF_LINES {
                return false;
            }
            lines.push((sign, SharedString::from(line.to_string())));
        }
        true
    };
    if let Some(path) = map.get("path").and_then(|v| v.as_str()) {
        push(&mut lines, ' ', path);
    }
    if let Some(serde_json::Value::Array(edits)) = map.get("edits") {
        for edit in edits {
            if let Some(old) = edit.get("oldText").and_then(|v| v.as_str())
                && !push(&mut lines, '-', old)
            {
                return lines;
            }
            if let Some(new) = edit.get("newText").and_then(|v| v.as_str())
                && !push(&mut lines, '+', new)
            {
                return lines;
            }
        }
    }
    if let Some(content) = map.get("content").and_then(|v| v.as_str()) {
        push(&mut lines, '+', content);
    }
    lines
}

/// Tool card whose payload renders as a colored diff when it carries
/// edit/write replacements.
fn render_tool_with_diff(
    item: &AgentTimelineItem,
    revision: u64,
    details: bool,
    transcript: &TranscriptCtx,
) -> Div {
    let card = render_tool(item, revision, details, transcript);
    if !details {
        return card;
    }
    let diff_lines = Arc::clone(&transcript.derived.get(item, revision).diff_lines);
    if diff_lines.is_empty() {
        return card;
    }
    let mut diff = div()
        .flex()
        .flex_col()
        .mt_1()
        .rounded(theme::RADIUS_SM)
        .bg(gpui::rgb(theme::bg_code_block()))
        .border_1()
        .border_color(gpui::rgb(theme::border_subtle()))
        .overflow_x_hidden();
    for (sign, line) in diff_lines.iter() {
        let color = match sign {
            '+' => theme::status_success(),
            '-' => theme::status_error(),
            _ => theme::text_secondary(),
        };
        diff = diff.child(
            div()
                .flex()
                .gap_1()
                .px_2()
                .text_xs()
                .font_family(crate::assets::FONT_MONO)
                .child(
                    div()
                        .w(gpui::px(10.))
                        .text_color(gpui::rgb(color))
                        .child(sign.to_string()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(gpui::rgb(color))
                        .line_clamp(1)
                        .text_ellipsis()
                        .child(line.clone()),
                ),
        );
    }
    // The diff is content: swallow clicks so selecting it does not toggle.
    card.child(
        div()
            .id(gpui::SharedString::from(format!("tool-diff-{}", item.id)))
            .on_click(
                |_event: &gpui::ClickEvent, _window: &mut Window, cx: &mut gpui::App| {
                    cx.stop_propagation();
                },
            )
            .child(diff),
    )
}

/// Title shown before the model summary arrives: just the tool label from
/// the timeline's descriptive title ("Terminal: cargo test" -> "Terminal").
/// The labels mirror `friendly_tool_label` in the agent's timeline
/// projection; titles built any other way (generated titles, skill loads)
/// are kept as-is.
pub(super) fn tool_label_title(title: &str) -> &str {
    const LABELS: &[&str] = &[
        "Terminal",
        "Python",
        "Subagent",
        "Load",
        "Editor",
        "Web Search",
        "Read file",
        "Write file",
        "List files",
        "Find files",
        "Search",
    ];
    match title.split_once(": ") {
        Some((label, _)) if LABELS.contains(&label) => label,
        _ => title,
    }
}

fn render_tool(
    item: &AgentTimelineItem,
    revision: u64,
    details: bool,
    transcript: &TranscriptCtx,
) -> Div {
    let (label, status_color) = tool_status_style(item.status.as_deref());
    let running = !matches!(
        item.status.as_deref(),
        Some("completed" | "failed" | "error" | "cancelled" | "controlled_externally")
    );
    let item_id = item.id.clone();
    let chat_header = transcript.chat.clone();
    let summary = transcript.tool_summaries.get(&item.id).cloned();
    let has_summary = summary.is_some();
    // The model summary stands in for the title; until it lands, show just
    // the tool label ("Terminal"), not the raw `tool: args` title.
    let title = summary.clone().unwrap_or_else(|| {
        let raw = item.title.as_deref().unwrap_or(&item.item_type);
        SharedString::from(tool_label_title(raw).to_string())
    });
    let card = div()
        .id(gpui::SharedString::from(format!("tool-toggle-{item_id}")))
        .role(gpui::Role::DisclosureTriangle)
        .aria_label(title.clone())
        .aria_expanded(details)
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .py_2()
        .rounded(theme::RADIUS_SM)
        .bg(gpui::rgb(theme::bg_tool_card()))
        .border_1()
        .border_color(gpui::rgb(theme::border_subtle()))
        .hover(|style| {
            style
                .border_color(gpui::rgb(theme::border()))
                .cursor_pointer()
        })
        .on_click(move |_event, _window, cx: &mut gpui::App| {
            chat_header
                .update(cx, |chat, cx| {
                    chat.toggle_tool(&item_id, cx);
                })
                .ok();
        })
        .child(
            div()
                .flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(gpui::rgb(theme::text_primary()))
                        .line_clamp(1)
                        .text_ellipsis_middle()
                        .child(title),
                )
                .when(running, |header| {
                    header.child(spinner_with_id(
                        SharedString::from(format!("tool-spinner-{}", item.id)),
                        px(12.),
                        theme::accent(),
                    ))
                })
                .child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(status_color))
                        .child(label),
                )
                .child(div().flex_1())
                .child(icon(
                    if details {
                        "chevron-down"
                    } else {
                        "chevron-right"
                    },
                    px(14.),
                    theme::text_muted(),
                )),
        );
    // Collapsed: the header alone; the summary takes over the title when
    // it lands. Only expanded cards pay for the derived payload strings.
    if !details {
        return div().child(card);
    }
    let derived = transcript.derived.get(item, revision);
    // A click anywhere on the card, payload included, toggles it.
    let mut payload = div().flex().flex_col().gap_1();
    // Python retains its result details after summarization: the namespace,
    // traceback, and output-loss notices remain inspectable.
    if let Some(input) = &derived.input_line {
        payload = payload.child(
            div()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .font_family(crate::assets::FONT_MONO)
                .overflow_x_hidden()
                .child(input.clone()),
        );
    }
    if tool_output_visible(item, has_summary)
        && let Some(output) = &derived.output_text
    {
        payload = payload.child(
            div()
                .mt_1()
                .w_full()
                .text_sm()
                .text_color(gpui::rgb(theme::text_secondary()))
                .child(markdown::render(&transcript.markdown_cache.get(
                    &item.id,
                    MarkdownKind::ToolOutput,
                    revision,
                    output,
                ))),
        );
    }
    div().child(card.child(payload))
}

fn tool_output_visible(item: &AgentTimelineItem, has_summary: bool) -> bool {
    !has_summary
        || item
            .output
            .as_ref()
            .and_then(|output| output.pointer("/structuredContent/maple_python/version"))
            .and_then(serde_json::Value::as_u64)
            == Some(1)
}

/// Approval has no result marker yet. Match the Maple tool identity rather
/// than recognizing arbitrary extensions whose names happen to end in Python.
fn is_python_tool(tool_name: &str) -> bool {
    matches!(tool_name, "python_code" | "developer__python_code")
}

/// Prepare arguments on request arrival. Python source is literal multiline
/// text, including the final line; the scrollable card never substitutes an
/// abbreviated excerpt for the code being approved.
pub(super) fn permission_arguments(tool_name: &str, arguments: &serde_json::Value) -> Arc<str> {
    if arguments.is_null() {
        return "".into();
    }
    if is_python_tool(tool_name) {
        let formatted = format_tool_input(arguments);
        if arguments.get("reset").is_none() {
            format!("reset: false\n{formatted}").into()
        } else {
            formatted.into()
        }
    } else {
        serde_json::to_string_pretty(arguments)
            .unwrap_or_default()
            .into()
    }
}

/// Readable form of the call arguments: one `key: value` line per
/// field, strings shown as-is, nested values as pretty JSON.
pub(super) fn tool_input_line(item: &AgentTimelineItem) -> Option<String> {
    let value = item.input.as_ref().filter(|value| !value.is_null())?;
    Some(format_tool_input(value))
}

fn format_tool_input(value: &serde_json::Value) -> String {
    use serde_json::Value;
    let scalar = |value: &Value| match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(_) | Value::Number(_) => value.to_string(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
        }
    };
    match value {
        Value::Object(map) if !map.is_empty() => map
            .iter()
            .map(|(key, value)| {
                let text = scalar(value);
                if text.contains('\n') {
                    format!("{key}:\n{text}")
                } else {
                    format!("{key}: {text}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => scalar(value),
    }
}

/// Extract readable text from a tool output for markdown rendering.
pub(super) fn tool_output_markdown(item: &AgentTimelineItem) -> Option<String> {
    let value = item.output.as_ref().filter(|value| !value.is_null())?;
    let text = extract_output_text(value)?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn extract_output_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Object(map) => {
            // Common tool result shapes: {"text": ...}, {"stdout": ...},
            // {"content": [{"type": "text", "text": ...}, ...]}.
            for key in ["text", "stdout", "stderr", "output"] {
                if let Some(inner) = map.get(key)
                    && let Some(text) = extract_output_text(inner)
                {
                    return Some(text);
                }
            }
            if let Some(serde_json::Value::Array(items)) = map.get("content") {
                let mut joined = String::new();
                for entry in items {
                    if let Some(text) = entry.get("text").and_then(|t| t.as_str()) {
                        joined.push_str(text);
                        joined.push('\n');
                    }
                }
                if !joined.is_empty() {
                    return Some(joined);
                }
            }
            None
        }
        _ => None,
    }
}

fn render_error(item: &AgentTimelineItem) -> Div {
    let text = item
        .text
        .clone()
        .or_else(|| item.title.clone())
        .unwrap_or_default();
    if text.trim().is_empty() {
        return div();
    }
    widgets::banner(theme::status_error()).child(text)
}

fn render_permission_row(item: &AgentTimelineItem) -> Div {
    let title = item
        .title
        .clone()
        .unwrap_or_else(|| "Permission".to_string());
    let status = match item.status.as_deref() {
        Some("completed") => ("allowed", theme::status_success()),
        Some("denied") | Some("cancelled") => ("denied", theme::text_muted()),
        _ => ("waiting", theme::status_warning()),
    };
    div()
        .flex()
        .gap_2()
        .items_center()
        .px_3()
        .py_2()
        .rounded(theme::RADIUS_SM)
        .bg(gpui::rgb(theme::permission_fill()))
        .border_1()
        .border_color(gpui::rgb(theme::permission_border()))
        .child(
            div()
                .text_sm()
                .text_color(gpui::rgb(theme::text_primary()))
                .child(title),
        )
        .child(
            div()
                .text_xs()
                .text_color(gpui::rgb(status.1))
                .child(status.0),
        )
}

fn render_system(item: &AgentTimelineItem) -> Div {
    let text = item
        .text
        .clone()
        .or_else(|| item.title.clone())
        .unwrap_or_default();
    if text.trim().is_empty() {
        return div();
    }
    let text = maple_display_text(&text).into_owned();
    div()
        .text_sm()
        .text_color(gpui::rgb(theme::text_muted()))
        .child(text)
}

pub(super) fn render_question_card(
    question: &crate::backend::PendingQuestion,
    step: usize,
    input: Option<Entity<TextInput>>,
    selected: &HashMap<usize, usize>,
    cx: &mut Context<ChatScreen>,
) -> Div {
    let mut card = div()
        .my_3()
        .mx_auto()
        .w_full()
        .max_w(CONTENT_WIDTH - px(48.))
        .px_4()
        .py_3()
        .rounded(theme::RADIUS_MD)
        .bg(gpui::rgb(theme::bg_elevated()))
        .border_1()
        .border_color(gpui::rgb(theme::status_running()))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(gpui::rgb(theme::text_primary()))
                .child("Question from Maple"),
        );
    let step = step.min(question.questions.len().saturating_sub(1));
    let has_more = step + 1 < question.questions.len();
    for (question_index, entry) in question
        .questions
        .iter()
        .enumerate()
        .filter(|(i, _)| *i == step)
    {
        let mut block = div().flex().flex_col().gap_1();
        if question.questions.len() > 1 {
            block = block.child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(format!(
                        "Question {} of {}",
                        step + 1,
                        question.questions.len()
                    )),
            );
        }
        block = block.child(
            div()
                .text_xs()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(gpui::rgb(theme::text_secondary()))
                .child(entry.header.clone()),
        );
        block = block.child(
            div()
                .text_sm()
                .text_color(gpui::rgb(theme::text_secondary()))
                .child(entry.question.clone()),
        );
        for (option_index, option) in entry.options.iter().enumerate() {
            let is_picked = selected.get(&question_index) == Some(&option_index);
            let marker = div()
                .size_3()
                .rounded_full()
                .border_1()
                .border_color(gpui::rgb(if is_picked {
                    theme::accent()
                } else {
                    theme::border()
                }))
                .when(is_picked, |dot| dot.bg(gpui::rgb(theme::accent())));
            // flex_1 is load-bearing: without it the row squeezes this
            // block to a character wide and the label wraps vertically.
            let label_element = if option.description.is_empty() {
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_primary()))
                    .child(option.label.clone())
            } else {
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_sm()
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(option.label.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(option.description.clone()),
                    )
            };
            block = block.child(
                div()
                    .id(gpui::SharedString::from(format!(
                        "question-option-{question_index}-{option_index}"
                    )))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded(theme::RADIUS_SM)
                    .hover(|style| {
                        style
                            .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                            .cursor_pointer()
                    })
                    .on_click({
                        cx.listener(move |this, _event, _window, cx| {
                            this.toggle_question_option(question_index, option_index, cx);
                        })
                    })
                    .child(marker)
                    .child(label_element),
            );
        }
        card = card.child(block);
    }
    // "Other (type your own)": one shared free-form answer per card; it
    // stands in for a question without a picked option and rides along
    // as a note when an option is picked too.
    if let Some(input) = input {
        card = card.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(widgets::input_frame().flex_1().child(input))
                .child(
                    widgets::primary_button("question-submit")
                        .py_2()
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.submit_question(cx);
                        }))
                        .child(if has_more { "Next" } else { "Answer" }),
                ),
        );
    }
    card.child(
        div()
            .id("question-skip")
            .flex()
            .items_center()
            .gap_1p5()
            .px_2()
            .py_1()
            .rounded(theme::RADIUS_SM)
            .text_xs()
            .text_color(gpui::rgb(theme::text_muted()))
            .hover(|style| {
                style
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .cursor_pointer()
            })
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.skip_question(cx);
            }))
            .child("Skip (Esc)"),
    )
}

/// Pulsing dots shown between send and the first streamed content. They
/// breathe on the shared low-rate clock, so waiting costs the same as a
/// spinner rather than a full-rate animation.
pub(super) fn render_waiting_indicator() -> gpui::Stateful<Div> {
    const CYCLE: std::time::Duration = std::time::Duration::from_millis(1400);
    div()
        .id("waiting-indicator")
        .role(gpui::Role::Status)
        .aria_label("Maple is thinking")
        .flex()
        .items_center()
        .gap_2()
        .px_4()
        .py_2()
        .child(motion::ticker("waiting-dots", CYCLE, |phase| {
            let mut row = div().flex().items_center().gap_1p5();
            for index in 0..3 {
                let level = motion::pulse(phase, -(index as f32) * 0.18, 0.25, 1.0);
                row = row.child(
                    div()
                        .size(px(7.))
                        .rounded_full()
                        .bg(gpui::rgb(theme::accent()))
                        .opacity(level),
                );
            }
            row.into_any_element()
        }))
        .child(
            div()
                .text_sm()
                .text_color(gpui::rgb(theme::text_muted()))
                .child("Maple is thinking"),
        )
}

pub(super) fn render_permission_card(
    permission: &PendingPermission,
    responding: bool,
    application_choice: Option<usize>,
    cx: &mut Context<ChatScreen>,
) -> Div {
    let description: SharedString = match permission.prompt.as_deref() {
        Some(prompt) => prompt.to_string().into(),
        None => format!("Run tool {}?", permission.tool_name).into(),
    };
    let arguments: SharedString = permission.arguments.clone().into();
    let mut card = div()
        .my_3()
        .mx_auto()
        .w_full()
        .max_w(CONTENT_WIDTH - px(48.))
        .px_4()
        .py_3()
        .rounded(theme::RADIUS_MD)
        .bg(gpui::rgb(theme::permission_fill()))
        .border_1()
        .border_color(gpui::rgb(theme::permission_border()))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(gpui::rgb(theme::text_primary()))
                .child("Permission required"),
        )
        .child(
            div()
                .text_sm()
                .text_color(gpui::rgb(theme::text_secondary()))
                .child(description),
        );
    if !arguments.is_empty() {
        let payload = div()
            .text_xs()
            .text_color(gpui::rgb(theme::text_muted()))
            .font_family(crate::assets::FONT_MONO)
            .max_h(gpui::px(120.))
            .child(arguments);
        card = if is_python_tool(&permission.tool_name) {
            card.child(
                payload
                    .id(SharedString::from(permission.request_id.clone()))
                    .overflow_scroll()
                    .whitespace_nowrap(),
            )
        } else {
            card.child(payload.overflow_hidden())
        };
    }
    let mut buttons = div().flex().gap_2();
    for (index, (id, label, allow, color)) in [
        (
            "permission-allow-once",
            "Allow once",
            true,
            theme::status_success(),
        ),
        ("permission-deny", "Deny", false, theme::status_error()),
    ]
    .into_iter()
    .enumerate()
    {
        buttons = buttons.child(
            div()
                .id(id)
                .px_4()
                .py_1()
                .rounded_full()
                .font_weight(gpui::FontWeight::MEDIUM)
                .when(application_choice == Some(index), |button| {
                    button
                        .border_2()
                        .border_color(gpui::rgb(theme::text_primary()))
                })
                .bg(gpui::rgb(if responding { theme::border() } else { color }))
                .text_sm()
                .text_color(gpui::rgb(theme::on_accent()))
                .when(!responding, |el| {
                    el.hover(|style| style.opacity(0.9).cursor_pointer())
                        .active(|style| style.opacity(0.75))
                        .on_click(cx.listener(move |this, _event, window, cx| {
                            this.execute_command(
                                ChatCommand::RespondPermission { allow },
                                window,
                                cx,
                            );
                        }))
                })
                .child(label.to_string()),
        );
    }
    if responding {
        card = card.child(
            div()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child("Sending decision…"),
        );
    }
    card.child(buttons)
}

#[cfg(test)]
mod python_presentation_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn approval_preserves_multiline_source_and_explicit_reset() {
        let source = format!("{}\nprint('final Ω line')", "value = 1\n".repeat(12_000));
        for name in ["python_code", "developer__python_code"] {
            let arguments = permission_arguments(name, &json!({"code": source, "reset": true}));
            assert!(arguments.contains(&source));
            assert!(arguments.contains("reset: true"));
            assert!(!arguments.contains("\\n"));
            assert!(arguments.contains("print('final Ω line')"));
        }
        let defaults = permission_arguments("python_code", &json!({"code": "40 + 2"}));
        assert!(defaults.contains("reset: false"));
        assert!(defaults.contains("code: 40 + 2"));
    }

    #[test]
    fn approval_formatting_is_specific_to_maples_python_tool() {
        let arguments = json!({"code": "first\nsecond", "reset": false});
        for name in ["shell", "other__python_code", "python_code_extra"] {
            assert_eq!(
                permission_arguments(name, &arguments).as_ref(),
                serde_json::to_string_pretty(&arguments).unwrap()
            );
        }
        // A malformed reset value stays inspectable rather than appearing
        // to be the valid default that the user did not submit.
        assert!(
            permission_arguments("python_code", &json!({"code": "42", "reset": "invalid"}))
                .contains("reset: invalid")
        );
    }

    #[test]
    fn python_results_remain_inspectable_after_summary_without_changing_other_tools() {
        let mut item = AgentTimelineItem {
            id: "call".into(),
            item_type: "tool".into(),
            role: None,
            title: Some("Python: print('hello')".into()),
            text: None,
            status: Some("completed".into()),
            input: Some(json!({"code": "print('hello')", "reset": false})),
            output: None,
            created_ms: 0,
            merge: "replace".into(),
        };
        assert_eq!(tool_label_title(item.title.as_deref().unwrap()), "Python");
        assert!(tool_output_visible(&item, false));
        assert!(!tool_output_visible(&item, true));
        for kind in ["execution", "reset", "error"] {
            item.output = Some(json!({
                "text": "Traceback\nValueError: invalid\nOutput truncated; Python state was lost.",
                "structuredContent": {"maple_python": {"version": 1, "kind": kind}}
            }));
            assert!(tool_output_visible(&item, true));
            assert!(
                tool_output_markdown(&item)
                    .unwrap()
                    .contains("Python state was lost.")
            );
        }
        item.output = Some(json!({"structuredContent": {"maple_python": {"version": 2}}}));
        assert!(!tool_output_visible(&item, true));
        item.output = Some(json!({"text": "maple_python version 1"}));
        assert!(!tool_output_visible(&item, true));
    }
}
