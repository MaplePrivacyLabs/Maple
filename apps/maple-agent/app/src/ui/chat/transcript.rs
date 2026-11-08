//! The transcript list and the row renderers behind it. Rows read the
//! shared `TranscriptCtx` caches; nothing here parses or derives on a
//! frame.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui::{Bounds, Div, Entity, IntoElement, Pixels, SharedString, Window, div, prelude::*, px};
use maple_agent::agent::{
    AgentPermissionReason, AgentTimelineItem, EXTERNAL_AGENT_ACTIVITY_KEY, ExternalAgentActivity,
    ExternalAgentRef, USAGE_LIMIT_MESSAGE,
};

use super::cache::{MAX_DIFF_LINES, MarkdownKind};
use super::speech::speak_message_button;
use super::{CONTENT_WIDTH, ChatScreen, QuestionSelection, TranscriptCtx};

use crate::ui::i18n::{MessageCache, tr};
use crate::ui::icons::{icon, spinner, spinner_with_id};
use crate::ui::markdown;
use crate::ui::motion;
use crate::ui::rich_text::{self, RenderCtx};
use crate::ui::settings::{OpenSettingsSection, Section};
use crate::ui::text_input::TextInput;
use crate::ui::theme;
use crate::ui::typography;
use crate::ui::widgets;

impl ChatScreen {
    pub(super) fn render_transcript(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        // The list follows its own tail: it snaps to the end on each
        // layout until the user scrolls up, and re-engages when the view
        // returns to the bottom. Every mutation goes through splice or
        // remeasure, so the count only drifts if a code path forgot to.
        let count = self.timeline.len();
        if self.list_state.item_count() != count {
            self.list_state
                .reset_with_uniform_height(count, super::TRANSCRIPT_ROW_ESTIMATE);
        }
        // Entrances read one clock for the whole frame and ask for the next
        // frame only while one is still running.
        let now = motion::now();
        let reduced = cx.reduce_motion();
        if self.row_motion.settle(now, reduced) {
            window.request_animation_frame();
        }
        let switch_reveal = self.row_motion.switch_progress(now, reduced);
        let show_jump = count > 0 && !self.list_state.is_following_tail();
        let tool_details = self.tool_details;
        let entity = cx.entity().downgrade();
        let selection = self.selection.clone();
        let transcript_focus = self.transcript_focus.clone();
        // Rows note when they paint in view, so a wheel elsewhere in the
        // pane can report the rows on screen like the list's own wheel.
        self.painted_rows.set(None);
        let painted_rows = self.painted_rows.clone();
        // Only the visible items (plus a small overdraw) are built each
        // frame; the list measures and caches the rest. Everything else is
        // read through the entity so nothing is cloned per frame.
        let list = gpui::list(self.list_state.clone(), move |ix, window, cx| {
            let Some(chat_entity) = entity.upgrade() else {
                return div().into_any_element();
            };
            let chat = chat_entity.read(cx);
            // A row that arrived live enters, timed from view state so
            // scrolling it away and back never replays the entrance.
            let entrance = chat
                .timeline
                .get(ix)
                .and_then(|item| chat.row_motion.entrance(&item.id, now, reduced));
            let row = match chat.timeline.get(ix) {
                Some(item) => {
                    let render_ctx = RenderCtx {
                        selection: selection.clone(),
                        base_ordinal: Some(chat.markdown_cache.ordinal_for(&item.id)),
                        focus: transcript_focus.clone(),
                        id_seed: item.id.clone(),
                        view: Some(window.current_view()),
                    };
                    let transcript = TranscriptCtx {
                        messages: &chat.messages,
                        local_error_text: &chat.local_error_text,
                        markdown_cache: &chat.markdown_cache,
                        derived: &chat.derived,
                        attachment_images: &chat.attachment_images,
                        attachment_failures: &chat.attachment_failures,
                        chat: &entity,
                        tool_summaries: &chat.tool_summaries,
                        render: &render_ctx,
                        speech: chat.speech.as_ref(),
                        speech_available: chat.audio_caps.speech,
                        position: (ix + 1, chat.timeline.len()),
                        reveal: chat.row_motion.disclosure(&item.id, now, reduced),
                    };
                    let expanded = tool_details != chat.toggled_tools.contains(&item.id);
                    let revision = chat
                        .timeline_index
                        .get(&item.id)
                        .map_or(0, |(_, revision)| *revision);
                    let mut row =
                        div().child(render_timeline_item(item, revision, expanded, &transcript));
                    if item.truncated.is_some() {
                        let id = item.id.clone();
                        let target = entity.clone();
                        row = row.child(
                            widgets::secondary_button(SharedString::from(format!(
                                "expand-{}",
                                item.id
                            )))
                            .text_xs()
                            .mt_2()
                            .child(gpui::text!(tr("chat-full-output")))
                            .on_click(move |_, _, cx| {
                                if let Some(chat) = target.upgrade() {
                                    chat.update(cx, |chat, cx| chat.expand_paged_row(&id, cx));
                                }
                            }),
                        );
                    }
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
            };
            // Only rows the list places in view are prepainted here; rows
            // measured for overdraw are not, and a focused row kept alive
            // off screen lands outside the mask.
            let painted_rows = painted_rows.clone();
            let row = div()
                .child(row)
                .on_children_prepainted(move |bounds, window, _cx| {
                    let view = window.content_mask().bounds;
                    let on_screen = bounds
                        .first()
                        .is_some_and(|row| row.top() < view.bottom() && row.bottom() > view.top());
                    if on_screen {
                        painted_rows.set(Some(match painted_rows.get() {
                            Some((start, end)) => (start.min(ix), end.max(ix + 1)),
                            None => (ix, ix + 1),
                        }));
                    }
                });
            match entrance {
                None => row.into_any_element(),
                // Scrolled up: paint offset and opacity only, so nothing
                // on screen moves.
                Some(Entrance {
                    progress,
                    grow: None,
                    ..
                }) => motion::arrive(row, progress).into_any_element(),
                // At the tail: the row opens to its height and the rows
                // above glide up with it.
                Some(Entrance {
                    progress,
                    grow: Some(grow),
                    natural,
                }) => grow_in(
                    row,
                    progress,
                    grow,
                    natural,
                    chat.row_motion.heights.clone(),
                    chat.timeline[ix].id.clone(),
                ),
            }
        })
        .size_full();
        div()
            .id("transcript")
            .role(gpui::Role::Log)
            .aria_label(tr("chat-conversation"))
            .key_context("Transcript")
            .when_some(self.transcript_focus.clone(), |div, focus| {
                div.track_focus(&focus)
            })
            .relative()
            .flex_1()
            .flex()
            .flex_col()
            .min_h_0()
            .capture_any_mouse_down(cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                if event.button == gpui::MouseButton::Right
                    && let Some(selection) = &this.selection
                {
                    selection.update(cx, |selection, _| selection.clear_context_link());
                }
            }))
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    if let Some(focus) = &this.transcript_focus {
                        window.focus(focus, cx);
                    }
                    this.popup
                        .open(super::ChatPopup::Transcript(event.position), cx);
                }),
            )
            .children(self.render_transcript_menu(window, cx))
            .when(self.pages.before, |container| {
                container.child(
                    widgets::secondary_button("history-older")
                        .mx_6()
                        .my_1()
                        .text_xs()
                        .child(gpui::text!(if self.pages.loading {
                            tr("chat-history-loading")
                        } else {
                            tr("chat-history-older")
                        }))
                        .on_click(cx.listener(|chat, _, _, cx| chat.load_history_page(true, cx))),
                )
            })
            .child(
                // The list element does not apply padding itself, so the
                // gutter lives here. Same column width as the composer. On a
                // wide window the width never changes, so a resize keeps
                // every measured row; the pane scrolls the list from the
                // margins (`scroll_transcript_from_pane`).
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .max_w(CONTENT_WIDTH)
                    .mx_auto()
                    .px_6()
                    .pb_4()
                    // Another task's transcript fades in when it lands.
                    .when(switch_reveal < 1., |rows| {
                        rows.opacity(motion::ease_out_quint(switch_reveal))
                    })
                    .child(
                        // The list's own wheel scrolls it; the pane's must
                        // not scroll it a second time.
                        div()
                            .size_full()
                            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                            .child(list),
                    ),
            )
            .child(crate::ui::scrollbar::scrollbar(
                "transcript-scrollbar",
                self.list_state.clone(),
            ))
            .when(self.pages.after, |container| {
                container.child(
                    widgets::secondary_button("history-newer")
                        .mx_6()
                        .my_1()
                        .text_xs()
                        .child(gpui::text!(tr("chat-history-newer")))
                        .on_click(cx.listener(|chat, _, _, cx| chat.load_history_page(false, cx))),
                )
            })
            .when(show_jump, |container| {
                container.child(self.render_jump_to_latest(cx))
            })
            .when(self.pages.live_has_more, |container| {
                container.child(
                    div()
                        .mx_6()
                        .mb_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(tr("chat-activity-outside"))
                        .child(
                            widgets::secondary_button("live-reload-latest")
                                .text_xs()
                                .child(gpui::text!(tr("chat-reload-latest")))
                                .on_click(
                                    cx.listener(|chat, _, _, cx| chat.reload_paged_latest(cx)),
                                ),
                        ),
                )
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

    /// A wheel anywhere in the chat pane that nothing under the pointer
    /// takes for itself: the margins and gutters, the banners, the
    /// message being sent, the space beside the composer. It scrolls the
    /// transcript the way the list's own wheel does: the same 20 px
    /// line, letting go of the tail on the way up and taking it again at
    /// the bottom, and the rows on screen reported for paging and images.
    pub(super) fn scroll_transcript_from_pane(
        &mut self,
        event: &gpui::ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(px(20.)).y;
        if delta == px(0.) {
            return;
        }
        // Distances from the top. A list sent to its end holds a scroll
        // top past the last row until its next layout; that reads as more
        // than the range, so clamp to it.
        let range = self.list_state.max_offset_for_scrollbar().y;
        let top = (-self.list_state.scroll_px_offset_for_scrollbar().y)
            .max(px(0.))
            .min(range);
        let target = (top - delta).max(px(0.)).min(range);
        // At either end the wheel has nothing to move; leaving the list
        // alone keeps a short transcript following its tail.
        if target != top {
            self.list_state
                .set_offset_from_scrollbar(gpui::point(px(0.), -target));
            cx.notify();
        }
        // The list's own wheel reports the rows it last painted, also at
        // the end where its scroll top names no row; report the same.
        if let Some((start, end)) = self.painted_rows.get() {
            self.transcript_scrolled(start, end, cx);
        }
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
                        if this.pages.after {
                            this.reload_paged_latest(cx);
                        } else {
                            this.list_state.scroll_to_end();
                        }
                        cx.notify();
                    }))
                    .child(icon("chevron-down", px(12.), theme::text_secondary()))
                    .child(gpui::text!(tr("chat-jump-latest"))),
                "jump-to-latest-reveal",
            ))
    }

    /// The slot under the transcript: the message being sent with its
    /// status line, or the waiting dots, both `WAITING_ROW_HEIGHT` tall so
    /// one taking over from the other moves nothing.
    ///
    /// It sits outside the list, so its height is taken from the list's
    /// viewport. When it appears, or its content grows, it opens over
    /// `ENTER` instead of all at once, and the transcript above glides up.
    /// Its content stays pinned to its bottom while the top is clipped. It
    /// shrinks at once: the row taking its place opens from the space it
    /// leaves.
    pub(super) fn render_under_transcript(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let appeared = !self.row_motion.slot_on_screen;
        let snap = std::mem::take(&mut self.row_motion.slot_snap);
        let preview = self.render_send_preview(window, cx);
        let dots = self.awaiting_first_token && self.is_run_active() && !self.has_send_feedback();
        let slot = &mut self.row_motion;
        slot.slot_on_screen = preview.is_some() || dots;
        if !slot.slot_on_screen {
            slot.slot_open = None;
            slot.slot_natural.set(None);
            slot.slot_shown.set(px(0.));
            return None;
        }
        let now = motion::now();
        let reduced = cx.reduce_motion();
        // Measured last frame; `None` on the frame it appears.
        let natural = slot.slot_natural.get().map(f32::from);
        if reduced {
            slot.slot_open = None;
        } else if appeared {
            // A send opens it; a task switch or history load shows it at
            // once.
            slot.slot_open = (!snap).then(|| motion::Tween::new(0.));
            slot.slot_since = Some(now);
        }
        let cap = match (&mut slot.slot_open, natural) {
            (None, None) => None,
            (None, Some(natural)) => {
                if !reduced {
                    slot.slot_open = Some(motion::Tween::new(natural));
                }
                None
            }
            // Closed on its first frame while its content is measured.
            (Some(open), None) => {
                window.request_animation_frame();
                Some(px(open.value(now)))
            }
            (Some(open), Some(natural)) => {
                // Shrinking follows the content at once; growing opens.
                // The first opening runs from when the slot appeared, not
                // from the frame its content was first measured.
                if natural < open.value(now) {
                    *open = motion::Tween::new(natural);
                }
                let start = match slot.slot_since.take() {
                    Some(since) if open.target() == 0. => since,
                    _ => now,
                };
                open.set(
                    natural,
                    motion::ENTER,
                    motion::ease_out_cubic,
                    start,
                    reduced,
                );
                if open.is_moving(now) {
                    window.request_animation_frame();
                }
                Some(px(open.value(now)))
            }
        };
        let natural_height = slot.slot_natural.clone();
        let shown_height = slot.slot_shown.clone();
        let content = div()
            .flex_none()
            .w_full()
            .flex()
            .flex_col()
            .children(preview)
            .when(dots, |content| {
                // Same gutter as transcript text so the dots line up.
                content.child(
                    div()
                        .w_full()
                        .max_w(CONTENT_WIDTH)
                        .mx_auto()
                        .px_6()
                        .child(render_waiting_indicator()),
                )
            });
        Some(
            div()
                .flex_none()
                .w_full()
                .flex()
                .flex_col()
                .justify_end()
                .overflow_hidden()
                .when_some(cap, |slot, cap| slot.max_h(cap))
                .child(content)
                .on_children_prepainted(move |bounds, _, _| {
                    if let Some(content) = bounds.first() {
                        let height = content.size.height;
                        natural_height.set(Some(height));
                        shown_height.set(cap.map_or(height, |cap| height.min(cap)));
                    }
                })
                .into_any_element(),
        )
    }
}

/// Height of the row under the transcript that shows the waiting dots or,
/// while a message is being sent, its status line. Both take the same
/// slot so one replacing the other moves nothing.
pub(super) const WAITING_ROW_HEIGHT: Pixels = px(36.);

/// Space between the message being sent and its status line: the
/// transcript's bottom padding, so the real row lands where the preview
/// was.
pub(super) const PREVIEW_GAP: Pixels = px(16.);

/// Entrances in the transcript, timed from view state. A `gpui::list`
/// drops a row's element state when it scrolls away, so an entrance kept
/// there would replay; these start times outlive the elements, and a
/// finished one is pruned, so a row animates once.
#[derive(Default)]
pub(super) struct RowMotion {
    /// Rows that arrived live in the task on screen, by item id.
    arrived: HashMap<String, Arrival>,
    /// Natural heights of rows growing into place, measured as they
    /// paint; a row's first frame has none and opens from its start.
    pub(super) heights: Rc<RefCell<HashMap<String, Pixels>>>,
    /// Rows whose details the user just opened, by item id.
    disclosed: HashMap<String, Instant>,
    /// When another task's transcript landed.
    switched: Option<Instant>,
    /// The user row that takes over from the send preview on screen,
    /// where the preview's bubble was, and how tall the slot under the
    /// transcript was then.
    handed_off: Option<(String, Option<Bounds<Pixels>>, Pixels)>,
    /// Where the send preview's bubble painted last frame, if it showed.
    pub(super) preview_body: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Rows apply without an entrance (events replayed on reconnect).
    quiet: bool,
    /// The slot under the transcript (waiting dots or the sent message's
    /// status) showed last frame.
    pub(super) slot_on_screen: bool,
    /// The slot's height cap while it opens and after, so it grows rather
    /// than appearing at full height; `None` draws it at its natural height.
    slot_open: Option<motion::Tween>,
    /// The slot's natural height and the height it drew, last frame.
    slot_natural: Rc<Cell<Option<Pixels>>>,
    slot_shown: Rc<Cell<Pixels>>,
    /// The next slot to appear shows at once: it came with a task switch
    /// or a history load, not a send.
    slot_snap: bool,
    /// When the slot last appeared, for its first opening.
    slot_since: Option<Instant>,
}

#[derive(Clone, Copy)]
struct Arrival {
    at: Instant,
    grow: Option<Grow>,
}

/// How a row that arrived at the followed tail opens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Grow {
    /// Height it opens from: 0, or the space a vanished preview or
    /// waiting row left, so the rows above stay put on the first frame.
    pub(super) from: Pixels,
    /// Window y its content starts at (the preview's bubble), easing to
    /// its own place; such a row is already visible and does not fade.
    pub(super) glide_from: Option<Pixels>,
}

/// A row's entrance at one moment.
pub(super) struct Entrance {
    pub(super) progress: f32,
    pub(super) grow: Option<Grow>,
    pub(super) natural: Option<Pixels>,
}

impl RowMotion {
    /// A row was added live; history, paging and snapshots never call this.
    /// `at_tail`: the list follows its tail, so the row grows into place;
    /// otherwise it only fades and rises.
    pub(super) fn arrive(&mut self, id: &str, now: Instant, at_tail: bool) {
        let handed_off = self.handed_off.take();
        if self.quiet {
            return;
        }
        let grow = match handed_off {
            // The preview's bubble becomes this row: it opens from the
            // space the bubble left (all of the slot but the status line
            // that stays) and glides from where the bubble was.
            Some((handed, body, slot)) if handed == id => {
                let Some(body) = body.filter(|_| at_tail) else {
                    return;
                };
                Some(Grow {
                    from: (slot - WAITING_ROW_HEIGHT).max(px(0.)),
                    glide_from: Some(body.origin.y),
                })
            }
            _ => at_tail.then_some(Grow {
                from: px(0.),
                glide_from: None,
            }),
        };
        self.arrived
            .insert(id.to_owned(), Arrival { at: now, grow });
    }

    /// The row `id` also takes the place of `height` that left the space
    /// under the transcript (the waiting row or the preview's status).
    pub(super) fn open_from_more(&mut self, id: &str, height: Pixels) {
        if let Some(Arrival {
            grow: Some(grow), ..
        }) = self.arrived.get_mut(id)
        {
            grow.from += height;
        }
    }

    /// The next row with `id` replaces the send preview on screen.
    pub(super) fn hand_off(&mut self, id: String) {
        self.handed_off = Some((id, self.preview_body.get(), self.slot_shown.get()));
    }

    /// Apply rows without an entrance until called again with `false`.
    pub(super) fn set_quiet(&mut self, quiet: bool) {
        self.quiet = quiet;
    }

    /// A live row took its persisted id; its entrance carries on.
    pub(super) fn rekey(&mut self, old: &str, new: &str) {
        if let Some(arrival) = self.arrived.remove(old) {
            self.arrived.insert(new.to_owned(), arrival);
        }
        if let Some(at) = self.disclosed.remove(old) {
            self.disclosed.insert(new.to_owned(), at);
        }
        let mut heights = self.heights.borrow_mut();
        if let Some(height) = heights.remove(old) {
            heights.insert(new.to_owned(), height);
        }
    }

    /// The user opened or closed a row's details. Closing is immediate.
    pub(super) fn disclose(&mut self, id: &str, open: bool, now: Instant) {
        if open {
            self.disclosed.insert(id.to_owned(), now);
        } else {
            self.disclosed.remove(id);
        }
    }

    /// Another task's transcript landed.
    pub(super) fn switch_task(&mut self, now: Instant) {
        self.switched = Some(now);
    }

    /// A new transcript replaced every row; none of them is arriving.
    pub(super) fn clear_rows(&mut self) {
        self.arrived.clear();
        self.heights.borrow_mut().clear();
        self.disclosed.clear();
        self.handed_off = None;
        self.slot_snap = true;
    }

    /// The row's entrance, or `None` for a row that did not arrive live or
    /// has settled.
    pub(super) fn entrance(&self, id: &str, now: Instant, reduced: bool) -> Option<Entrance> {
        let arrival = self.arrived.get(id)?;
        let progress = motion::progress(arrival.at, motion::ENTER, now, reduced);
        (progress < 1.).then(|| Entrance {
            progress,
            grow: arrival.grow,
            natural: arrival
                .grow
                .and_then(|_| self.heights.borrow().get(id).copied()),
        })
    }

    /// Progress of the row's entrance; 1 when it has none.
    #[cfg(test)]
    pub(super) fn arrival(&self, id: &str, now: Instant, reduced: bool) -> f32 {
        self.entrance(id, now, reduced)
            .map_or(1., |entrance| entrance.progress)
    }

    /// Progress of the fade of details the user just opened.
    pub(super) fn disclosure(&self, id: &str, now: Instant, reduced: bool) -> f32 {
        self.disclosed
            .get(id)
            .map_or(1., |at| motion::progress(*at, motion::REVEAL, now, reduced))
    }

    /// Progress of the fade of a task's transcript after a switch.
    pub(super) fn switch_progress(&self, now: Instant, reduced: bool) -> f32 {
        self.switched
            .map_or(1., |at| motion::progress(at, motion::REVEAL, now, reduced))
    }

    /// Drop what has finished; true while something still moves, so the
    /// caller asks for another frame.
    pub(super) fn settle(&mut self, now: Instant, reduced: bool) -> bool {
        if reduced {
            self.arrived.clear();
            self.heights.borrow_mut().clear();
            self.disclosed.clear();
            self.switched = None;
            return false;
        }
        let running = |at: &Instant, duration| now.saturating_duration_since(*at) < duration;
        self.arrived
            .retain(|_, arrival| running(&arrival.at, motion::ENTER));
        self.heights
            .borrow_mut()
            .retain(|id, _| self.arrived.contains_key(id));
        self.disclosed.retain(|_, at| running(at, motion::REVEAL));
        self.switched = self.switched.filter(|at| running(at, motion::REVEAL));
        !self.arrived.is_empty() || !self.disclosed.is_empty() || self.switched.is_some()
    }
}

/// A row opening at the tail: its box eases from `grow.from` to its
/// natural height, clipped, so the rows above glide up rather than jump.
/// Its natural height is measured from the unclipped content each frame,
/// so a reply streaming in meanwhile still ends at its true height.
fn grow_in(
    row: Div,
    progress: f32,
    grow: Grow,
    natural: Option<Pixels>,
    heights: Rc<RefCell<HashMap<String, Pixels>>>,
    id: String,
) -> gpui::AnyElement {
    let eased = motion::ease_out_cubic(progress);
    let height = natural.map_or(grow.from, |natural| {
        grow.from + (natural - grow.from) * eased
    });
    let content = div().absolute().top_0().left_0().right_0().child(row);
    let content = match grow.glide_from {
        Some(from_y) => motion::glide_from(content, from_y, progress).into_any_element(),
        None => content
            .opacity(motion::ease_out_quint(progress))
            .into_any_element(),
    };
    div()
        .relative()
        .h(height)
        .overflow_hidden()
        .child(content)
        .on_children_prepainted(move |bounds, _, _| {
            if let Some(content) = bounds.first() {
                heights.borrow_mut().insert(id.clone(), content.size.height);
            }
        })
        .into_any_element()
}

pub(super) fn render_timeline_item(
    item: &AgentTimelineItem,
    revision: u64,
    expanded: bool,
    transcript: &TranscriptCtx,
) -> gpui::AnyElement {
    let item_id = item.id.clone();
    let (role, label): (Option<gpui::Role>, Option<SharedString>) = match item.item_type.as_str() {
        "message" if item.role.as_deref() == Some("user") => {
            (Some(gpui::Role::Article), Some(tr("chat-you")))
        }
        "message" => (Some(gpui::Role::Article), Some("Maple".into())),
        "error" => (
            Some(gpui::Role::Alert),
            error_presentation(item, transcript.local_error_text, transcript.messages).0,
        ),
        _ => (None, None),
    };
    let description = (item.item_type == "error").then(|| {
        error_presentation(item, transcript.local_error_text, transcript.messages)
            .1
            .map(|text| text.to_string())
            .unwrap_or_default()
    });
    let item = match item.item_type.as_str() {
        "message" => render_message(item, revision, transcript),
        "thinking" | "reasoning" => render_thinking(item, revision, expanded, transcript),
        "tool" | "toolCall" => {
            // Dispatch on payload shape; runtime titles are humanized
            // ("todo write", "ask user") and vary by detail suffix.
            if has_tool_input(item, "todos") {
                // The pinned card above the composer shows an unfinished
                // list; a finished one stays here as a foldable row.
                match render_finished_plan(item, revision, expanded, transcript) {
                    Some(plan) => plan,
                    None => return div().into_any_element(),
                }
            } else if has_tool_input(item, "edits")
                || (has_tool_input(item, "content") && has_tool_input(item, "path"))
            {
                render_tool_with_diff(item, revision, expanded, transcript)
            } else {
                render_tool(item, revision, expanded, transcript)
            }
        }
        "error" => render_error(item, transcript),
        "permission" => render_permission_row(item),
        _ => render_system(item, transcript),
    };
    // Per-item spacing (instead of a container gap) keeps non-renderable
    // items from producing phantom gaps. Rows a screen reader should
    // announce get a role, a sender, and their position in the
    // conversation; the others stay plain containers.
    let row = div().pb_2().child(item);
    match role {
        Some(role) => {
            let (position, count) = transcript.position;
            let row_id = SharedString::from(format!("timeline-row-{item_id}"));
            row.id(row_id.clone())
                .debug_selector(move || row_id.to_string())
                .role(role)
                .when_some(label, |row, label| row.aria_label(label))
                .when_some(description, |row, text| row.aria_description(text))
                .accessibility_id(item_id)
                .aria_position_in_set(position)
                .aria_size_of_set(count)
                .into_any_element()
        }
        None => row.into_any_element(),
    }
}

pub(super) struct AttachmentRef<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub mime: &'a str,
    pub size: u64,
    pub unavailable: bool,
}

pub(super) fn attachment_metadata(
    item: &AgentTimelineItem,
) -> impl Iterator<Item = AttachmentRef<'_>> {
    item.input
        .as_ref()
        .and_then(|input| input.get("attachments"))
        .and_then(|items| items.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?;
            let name = entry.get("name")?.as_str()?;
            let mime = entry
                .get("mimeType")
                .and_then(|value| value.as_str())
                .unwrap_or("application/octet-stream");
            let size = entry
                .get("size")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            Some(AttachmentRef {
                id,
                name,
                mime,
                size,
                unavailable: mime == "image/unknown",
            })
        })
}

/// All file references count as message content, including retired images.
pub(super) fn attachment_refs(item: &AgentTimelineItem) -> impl Iterator<Item = (&str, &str)> {
    attachment_metadata(item).map(|attachment| (attachment.id, attachment.name))
}

/// Only renderable image references need bytes. Generic files and known retired
/// images never occupy the thumbnail request queue.
pub(super) fn image_attachment_refs(
    item: &AgentTimelineItem,
) -> impl Iterator<Item = (&str, &str)> {
    attachment_metadata(item)
        .filter(|attachment| attachment.mime.starts_with("image/") && !attachment.unavailable)
        .map(|attachment| (attachment.id, attachment.name))
}

/// Hover-revealed button that copies one message's text.
fn copy_message_button(
    item_id: &str,
    group: &SharedString,
    text: SharedString,
    view: Option<gpui::EntityId>,
) -> crate::ui::motion::HoverBackground {
    widgets::copy_button(
        SharedString::from(format!("copy-message-{item_id}")),
        text,
        Some(group),
        view,
    )
}

/// When a message was sent, revealed beside its actions on hover: the
/// clock for today, otherwise the date too.
fn timestamp_label(item: &AgentTimelineItem, group: &SharedString, messages: &MessageCache) -> Div {
    let sent = chrono::DateTime::<chrono::Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(item.created_ms as u64),
    );
    let today = chrono::Local::now().date_naive();
    let text = messages.computed(
        "chat-timestamp",
        &format!("{}:{today}", item.created_ms),
        || {
            if sent.date_naive() == today {
                return sent.format("%H:%M").to_string().into();
            }
            let month = tr(match chrono::Datelike::month(&sent) {
                1 => "chat-month-1",
                2 => "chat-month-2",
                3 => "chat-month-3",
                4 => "chat-month-4",
                5 => "chat-month-5",
                6 => "chat-month-6",
                7 => "chat-month-7",
                8 => "chat-month-8",
                9 => "chat-month-9",
                10 => "chat-month-10",
                11 => "chat-month-11",
                12 => "chat-month-12",
                _ => unreachable!("chrono month"),
            });
            messages.get(
                "chat-message-date",
                [
                    ("month", month.to_string().into()),
                    ("day", chrono::Datelike::day(&sent).into()),
                    ("time", sent.format("%H:%M").to_string().into()),
                ],
            )
        },
    );
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
    let derived = transcript.derived.get(item, revision);
    let has_attachments = !derived.attachments.is_empty();
    if text.trim().is_empty() && !(is_user && has_attachments) {
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
                typography::chat_reading(div())
                    .max_w(gpui::relative(0.75))
                    .px_4()
                    .py_2()
                    .rounded(theme::RADIUS_MD)
                    .bg(gpui::rgb(theme::bg_user_bubble()))
                    .border_1()
                    .border_color(gpui::rgb(theme::user_bubble_border()))
                    .text_color(gpui::rgb(theme::text_primary()))
                    .when(has_attachments, |bubble| {
                        bubble.child(div().flex().flex_wrap().gap_2().mb_1().children(
                            derived.attachments.iter().map(|attachment| {
                                let id = attachment.id.as_str();
                                let unavailable = attachment.unavailable
                                    || transcript.attachment_failures.contains(id);
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
                                        .child(
                                            div()
                                                .flex()
                                                .flex_col()
                                                .gap_0p5()
                                                .child(attachment.name.clone())
                                                .child(
                                                    div()
                                                        .text_color(gpui::rgb(theme::text_muted()))
                                                        .child(if unavailable {
                                                            tr("chat-image-unavailable")
                                                        } else {
                                                            attachment.detail.clone()
                                                        }),
                                                ),
                                        ),
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
                    .child(timestamp_label(item, &group, transcript.messages))
                    .children(copy),
            )
    } else {
        typography::chat_reading(div())
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
                    .child(timestamp_label(item, &group, transcript.messages))
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
                .child(tr("chat-thinking")),
        );
    }
    let item_id = item.id.clone();
    let chat = transcript.chat.clone();
    // The model summary stands in for the generic "Thinking" label.
    let title = transcript
        .tool_summaries
        .get(&item.id)
        .cloned()
        .unwrap_or_else(|| {
            crate::ui::timeline_labels::title(item, transcript.messages)
                .unwrap_or_else(|| tr("chat-thinking-title"))
        });
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
        .child(motion::chevron(
            SharedString::from(format!("thinking-chevron-{}", item.id)),
            expanded,
            px(14.),
            theme::text_muted(),
        ))
        .child(
            div()
                // Same prose-summary rule as the tool card title.
                .min_w_0()
                .flex_1()
                .truncate()
                .debug_selector(|| "thinking-title".to_string())
                .child(title),
        );
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
    card.child(revealed(
        typography::chat_reading(div())
            .text_color(gpui::rgb(theme::text_secondary()))
            .child(markdown::render_with(
                &transcript
                    .markdown_cache
                    .get(&item.id, MarkdownKind::Body, revision, &text),
                transcript.render,
            )),
        transcript.reveal,
    ))
}

/// Details the user just opened fade in; `reveal` is how far along they
/// are. Their height is there at once, so the list measures it once.
fn revealed<E: Styled>(details: E, reveal: f32) -> E {
    if reveal < 1. {
        details.opacity(motion::ease_out_quint(reveal))
    } else {
        details
    }
}

fn tool_status_style(status: Option<&str>) -> (SharedString, u32) {
    match status {
        Some("completed") => (tr("chat-status-completed"), theme::status_success()),
        Some("failed") | Some("error") => (tr("chat-status-failed"), theme::status_error()),
        Some("cancelled") => (tr("chat-status-stopped"), theme::text_muted()),
        _ => (tr("chat-status-running"), theme::status_running()),
    }
}

/// A tool card's spinner and status label. When the call finishes on
/// screen the running state goes at once and the outcome fades in over
/// [`motion::TOGGLE`]: two labels drawn over each other would read as
/// one garbled word. A card that mounts finished (history, a row scrolled
/// back) shows its state at once.
fn tool_status(item_id: &str, running: bool, label: SharedString, color: u32) -> motion::Animated {
    let spinner_id = SharedString::from(format!("tool-spinner-{item_id}"));
    motion::animated(
        SharedString::from(format!("tool-status-{item_id}")),
        if running { 0. } else { 1. },
        motion::TOGGLE,
        motion::ease_out_cubic,
        move |done| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .opacity(if running { 1. - done } else { done })
                .when(running, |state| {
                    state.child(spinner_with_id(
                        spinner_id.clone(),
                        px(12.),
                        theme::accent(),
                    ))
                })
                .child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(color))
                        .child(label.clone()),
                )
                .into_any_element()
        },
    )
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
    /// Row ID of the agent, shared by every event about it.
    pub(super) id: String,
    /// What the subagent was asked to do.
    pub(super) task: SharedString,
    /// It works in the background; the task collects the result later.
    pub(super) background: bool,
    /// When the card first showed it, for the elapsed time.
    pub(super) started: std::time::Instant,
    /// The tool it called most recently, if any.
    pub(super) activity: Option<SharedString>,
    /// Set for an external agent (Codex), which the user can stop.
    pub(super) external: Option<ExternalAgentRef>,
}

/// A click handler for the Stop control of an external agent's row.
pub(super) type StopHandler = Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static>;

/// The activity payload an external agent's tool row carries, if any.
pub(super) fn external_agent_activity(item: &AgentTimelineItem) -> Option<ExternalAgentActivity> {
    if !matches!(item.item_type.as_str(), "tool" | "toolCall") {
        return None;
    }
    let payload = item
        .output
        .as_ref()?
        .get("structuredContent")?
        .get(EXTERNAL_AGENT_ACTIVITY_KEY)?;
    serde_json::from_value(payload.clone()).ok()
}

/// True when the row belongs to an external agent turn. Cheap: a key
/// lookup, no parse.
pub(super) fn has_external_agent_activity(item: &AgentTimelineItem) -> bool {
    matches!(item.item_type.as_str(), "tool" | "toolCall")
        && item
            .output
            .as_ref()
            .and_then(|output| output.get("structuredContent"))
            .is_some_and(|content| content.get(EXTERNAL_AGENT_ACTIVITY_KEY).is_some())
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

/// A list with every entry checked off. An empty list clears the plan and
/// is never finished.
pub(super) fn plan_finished(plan: &[PlanEntry]) -> bool {
    !plan.is_empty()
        && plan
            .iter()
            .all(|entry| entry.status == PlanStatus::Completed)
}

/// The transcript row of a finished todo list: a foldable card like a tool
/// call's, or `None` while the list is unfinished and the pinned card
/// shows it.
fn render_finished_plan(
    item: &AgentTimelineItem,
    revision: u64,
    details: bool,
    transcript: &TranscriptCtx,
) -> Option<Div> {
    let derived = transcript.derived.get(item, revision);
    let plan = derived.plan.as_deref().filter(|plan| plan_finished(plan))?;
    let count = format!("{}/{}", plan.len(), plan.len());
    let item_id = item.id.clone();
    let chat = transcript.chat.clone();
    let mut card = div()
        .id(SharedString::from(format!("plan-toggle-{item_id}")))
        .debug_selector(|| "plan-row".to_string())
        .role(gpui::Role::DisclosureTriangle)
        .aria_label(
            transcript
                .messages
                .get("chat-plan-done", [("count", count.clone().into())]),
        )
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
            chat.update(cx, |chat, cx| chat.toggle_tool(&item_id, cx))
                .ok();
        })
        .child(
            div()
                .flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(gpui::rgb(theme::text_primary()))
                        .child(tr("chat-plan")),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(count),
                )
                .child(motion::chevron(
                    SharedString::from(format!("plan-chevron-{}", item.id)),
                    details,
                    px(14.),
                    theme::text_muted(),
                )),
        );
    if details {
        card = card.child(revealed(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .children(plan.iter().map(render_plan_row)),
            transcript.reveal,
        ));
    }
    Some(div().child(card))
}

/// One row of the subagent card: what the subagent was asked to do, the
/// tool it is running now, and how long it has worked. An external agent's
/// row also carries a Stop control.
pub(super) fn render_subagent_row(
    subagent: &ActiveSubagent,
    now: std::time::Instant,
    on_stop: Option<StopHandler>,
) -> Div {
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
    if let Some(external) = &subagent.external {
        row = row.child(
            div()
                .flex_none()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(external.provider.clone()),
        );
    }
    if subagent.background {
        row = row.child(
            div()
                .flex_none()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(tr("chat-background")),
        );
    }
    let row = row
        .child(
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
                        .unwrap_or_else(|| tr("chat-starting-agent")),
                ),
        )
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(format_subagent_elapsed(elapsed)),
        );
    let Some(on_stop) = on_stop else {
        return row;
    };
    row.child(
        div()
            .id(SharedString::from(format!("subagent-stop-{}", subagent.id)))
            .flex_none()
            .px_2()
            .py_0p5()
            .rounded(theme::RADIUS_SM)
            .border_1()
            .border_color(gpui::rgb(theme::border_subtle()))
            .text_xs()
            .text_color(gpui::rgb(theme::text_secondary()))
            .cursor_pointer()
            .hover(|style| style.border_color(gpui::rgb(theme::border())))
            .on_click(on_stop)
            .child(tr("chat-stop")),
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
    card.child(revealed(
        div()
            .id(gpui::SharedString::from(format!("tool-diff-{}", item.id)))
            .on_click(
                |_event: &gpui::ClickEvent, _window: &mut Window, cx: &mut gpui::App| {
                    cx.stop_propagation();
                },
            )
            .child(diff),
        transcript.reveal,
    ))
}

/// Title shown before the model summary arrives: just the tool label from
/// the timeline's descriptive title ("Terminal: cargo test" -> "Terminal").
/// The labels mirror `friendly_tool_label` in the agent's timeline
/// projection; titles built any other way (generated titles, skill loads)
/// are kept as-is.
pub(super) fn tool_label_title(title: &str) -> &str {
    const LABELS: &[&str] = &[
        "Terminal",
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
        Some("completed" | "failed" | "error" | "cancelled")
    );
    let item_id = item.id.clone();
    let chat_header = transcript.chat.clone();
    // An external agent's row draws its own activity; a summary asked
    // when its call started would hide what it did.
    let summary = (!has_external_agent_activity(item))
        .then(|| transcript.tool_summaries.get(&item.id).cloned())
        .flatten();
    // The model summary stands in for the title; until it lands, show just
    // the tool label ("Terminal"), not the raw `tool: args` title.
    let title = summary.clone().unwrap_or_else(|| {
        if let Some(label) = crate::ui::timeline_labels::tool_label(item, transcript.messages)
            .or_else(|| crate::ui::timeline_labels::title(item, transcript.messages))
        {
            return label;
        }
        let raw = item.title.as_deref().unwrap_or(&item.item_type);
        SharedString::from(tool_label_title(raw).to_string())
    });
    let card = div()
        .id(gpui::SharedString::from(format!("tool-toggle-{item_id}")))
        .debug_selector(|| "tool-card".to_string())
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
                        // The summary is prose: it grows to fill the row
                        // and ellipsizes at the row's end. Without min_w_0
                        // the flex item collapses to its longest word and
                        // the old middle-ellipsis cut mid-sentence inside
                        // a sliver while the row sat empty.
                        .min_w_0()
                        .flex_1()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(gpui::rgb(theme::text_primary()))
                        .truncate()
                        .debug_selector(|| "tool-title".to_string())
                        .child(title),
                )
                .child(tool_status(&item.id, running, label, status_color))
                .child(motion::chevron(
                    SharedString::from(format!("tool-chevron-{}", item.id)),
                    details,
                    px(14.),
                    theme::text_muted(),
                )),
        );
    // Collapsed: the header alone; the summary takes over the title when it lands.
    // Expanded cards also show the cached arguments and output.
    if !details {
        return div().child(card);
    }
    let derived = transcript.derived.get(item, revision);
    if let Some(activity) = &derived.external_agent {
        return div().child(card.child(revealed(
            render_external_agent_activity(item, revision, activity, transcript),
            transcript.reveal,
        )));
    }
    // A click anywhere on the card, payload included, toggles it.
    let mut payload = div().flex().flex_col().gap_1();
    // Expanded: the call arguments and the raw output. The summary is
    // made from the input alone, so it never stands in for the output.
    if let Some(input) = &derived.input_line {
        payload = payload.child(
            div()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .font_family(crate::assets::FONT_MONO)
                .overflow_x_hidden()
                .child(gpui::text!(
                    id = SharedString::from(format!("tool-input-{}", item.id)),
                    input.clone()
                )),
        );
    }
    if let Some(output) = &derived.output_text {
        payload = payload.child(
            typography::chat_reading(div())
                .mt_1()
                .w_full()
                .text_color(gpui::rgb(theme::text_secondary()))
                .child(markdown::render_with(
                    &transcript.markdown_cache.get(
                        &item.id,
                        MarkdownKind::ToolOutput,
                        revision,
                        output,
                    ),
                    &readable_ctx(transcript.render),
                )),
        );
    }
    div().child(card.child(revealed(payload, transcript.reveal)))
}

/// Keep a card's output out of the shared selection, which would treat its
/// clicks as a text drag, but give it the ordinals that put it in the
/// accessibility tree.
fn readable_ctx(ctx: &RenderCtx) -> RenderCtx {
    RenderCtx {
        base_ordinal: ctx.base_ordinal,
        ..RenderCtx::default()
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

/// The expanded body of an external agent's row: what it said, ran, and
/// changed, plus its todo list. The payload was parsed once into the
/// derived cache; this only lays it out.
fn render_external_agent_activity(
    item: &AgentTimelineItem,
    revision: u64,
    activity: &ExternalAgentActivity,
    transcript: &TranscriptCtx,
) -> Div {
    let muted = gpui::rgb(theme::text_muted());
    let secondary = gpui::rgb(theme::text_secondary());
    let mut body = div().flex().flex_col().gap_1p5().mt_1();
    let mut meta = format!("{} · {}", activity.provider, activity.agent_id);
    if let Some(error) = &activity.error {
        meta.push_str(" · ");
        meta.push_str(error);
    }
    if let Some(pending) = &activity.pending_permission {
        meta.push_str(" · ");
        meta.push_str(&transcript.messages.get(
            "chat-external-permission",
            [("action", pending.clone().into())],
        ));
    }
    body = body.child(div().text_xs().text_color(muted).child(meta));
    if !activity.text.trim().is_empty() {
        body = body.child(div().w_full().text_sm().text_color(secondary).child(
            markdown::render_with(
                &transcript.markdown_cache.get(
                    &item.id,
                    MarkdownKind::ToolOutput,
                    revision,
                    &activity.text,
                ),
                &readable_ctx(transcript.render),
            ),
        ));
    }
    if !activity.commands.is_empty() {
        let mut list = div().flex().flex_col().gap_0p5();
        for command in &activity.commands {
            let (label, color) = match command.status.as_str() {
                "running" => (tr("chat-status-running"), theme::status_running()),
                "failed" => (tr("chat-status-failed"), theme::status_error()),
                _ => (tr("chat-status-done"), theme::status_success()),
            };
            let exit = command
                .exit_code
                .map(|code| {
                    transcript
                        .messages
                        .get("chat-command-exit", [("code", code.into())])
                        .to_string()
                })
                .unwrap_or_default();
            list = list.child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_xs().text_color(gpui::rgb(color)).child(label))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(secondary)
                            .font_family(crate::assets::FONT_MONO)
                            .truncate()
                            .child(format!("{}{exit}", command.command)),
                    ),
            );
        }
        body = body
            .child(div().text_xs().text_color(muted).child(tr("chat-commands")))
            .child(list);
    }
    if !activity.file_changes.is_empty() {
        let mut list = div().flex().flex_col().gap_0p5();
        for change in &activity.file_changes {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(secondary)
                    .font_family(crate::assets::FONT_MONO)
                    .truncate()
                    .child(format!("{} {}", change.kind, change.path)),
            );
        }
        body = body
            .child(div().text_xs().text_color(muted).child(tr("chat-files")))
            .child(list);
    }
    if !activity.todos.is_empty() {
        let mut list = div().flex().flex_col().gap_0p5();
        for todo in &activity.todos {
            list = list.child(render_plan_row(&PlanEntry {
                content: todo.text.clone().into(),
                status: if todo.completed {
                    PlanStatus::Completed
                } else {
                    PlanStatus::Pending
                },
            }));
        }
        body = body
            .child(div().text_xs().text_color(muted).child(tr("chat-plan")))
            .child(list);
    }
    body
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

/// Both visible error copy and accessibility text use this presentation layer.
/// Raw body fields still drive protocol/usage-limit behavior below.
pub(super) fn error_presentation(
    item: &AgentTimelineItem,
    local: &std::collections::HashMap<String, crate::ui::i18n::UiText>,
    messages: &crate::ui::i18n::MessageCache,
) -> (Option<SharedString>, Option<SharedString>) {
    if let Some(body) = local.get(&item.id) {
        return (Some(tr("chat-send-failed-heading")), Some(body.resolve()));
    }
    (
        super::super::timeline_labels::title(item, messages)
            .or_else(|| item.title.clone().map(Into::into)),
        super::super::timeline_labels::body(item, messages)
            .or_else(|| item.text.clone().map(Into::into)),
    )
}

fn render_error(item: &AgentTimelineItem, transcript: &TranscriptCtx) -> Div {
    let text = error_presentation(item, transcript.local_error_text, transcript.messages)
        .1
        .map(|text| text.to_string())
        .or_else(|| item.title.clone())
        .unwrap_or_default();
    if text.trim().is_empty() {
        return div();
    }
    let banner = widgets::banner(theme::status_error());
    if item
        .label
        .as_ref()
        .is_none_or(|label| label.kind != "error-usage")
        && !item
            .text
            .as_deref()
            .is_some_and(|text| text.contains(USAGE_LIMIT_MESSAGE))
    {
        return banner.child(gpui::text!(text));
    }
    let chat = transcript.chat.clone();
    let button_id = format!("open-billing-{}", item.id);
    banner
        .flex()
        .flex_col()
        .items_start()
        .gap_2()
        .child(gpui::text!(text))
        .child(
            widgets::secondary_button(button_id.clone())
                .debug_selector({
                    let button_id = button_id.clone();
                    move || button_id.clone()
                })
                .child(gpui::text!(tr("chat-billing-open")))
                .on_click(move |_event, _window, cx| {
                    cx.stop_propagation();
                    let Some(chat) = chat.upgrade() else {
                        return;
                    };
                    chat.update(cx, |_chat, cx| {
                        cx.emit(OpenSettingsSection(Section::Billing));
                    });
                }),
        )
}

/// The row for an MCP elicitation ("Input requested"). It has no answer
/// path yet; the row shows whether the turn settled it.
fn render_permission_row(item: &AgentTimelineItem) -> Div {
    let title = item
        .title
        .clone()
        .unwrap_or_else(|| tr("chat-input-requested").to_string());
    let status = match item.status.as_deref() {
        Some("completed") => (tr("chat-status-allowed"), theme::status_success()),
        Some("denied") | Some("cancelled") => (tr("chat-status-denied"), theme::text_muted()),
        _ => (tr("chat-status-waiting"), theme::status_warning()),
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
                .child(gpui::text!(
                    id = SharedString::from(format!("permission-title-{}", item.id)),
                    title
                )),
        )
        .child(
            div()
                .text_xs()
                .text_color(gpui::rgb(status.1))
                .child(gpui::text!(
                    id = SharedString::from(format!("permission-status-{}", item.id)),
                    status.0
                )),
        )
}

fn render_system(item: &AgentTimelineItem, transcript: &TranscriptCtx) -> Div {
    let text = crate::ui::timeline_labels::body(item, transcript.messages)
        .or_else(|| item.text.clone().map(SharedString::from))
        .or_else(|| crate::ui::timeline_labels::title(item, transcript.messages))
        .or_else(|| item.title.clone().map(SharedString::from))
        .unwrap_or_default();
    if text.trim().is_empty() {
        return div();
    }
    div()
        .text_sm()
        .text_color(gpui::rgb(theme::text_muted()))
        .child(gpui::text!(
            id = SharedString::from(format!("system-text-{}", item.id)),
            text
        ))
}

/// What an option shows. The answer stays the option's own label: a
/// permission card shows its two options in the interface's language.
pub(super) fn question_option_label(
    entry: &maple_agent::agent::AgentQuestion,
    index: usize,
) -> SharedString {
    match (&entry.permission, index) {
        (Some(_), 0) => tr("chat-permission-allow"),
        (Some(_), _) => tr("chat-permission-deny"),
        (None, _) => entry
            .options
            .get(index)
            .map(|option| SharedString::from(option.label.clone()))
            .unwrap_or_default(),
    }
}

/// A permission request's desktop notification text, in the interface's
/// language: the reason, then the call as the card shows it.
pub(super) fn permission_notification_body(
    prompt: &maple_agent::agent::AgentPermissionPrompt,
) -> String {
    format!(
        "{}\n{}",
        tr(permission_reason_key(prompt.reason)),
        prompt.action
    )
    .chars()
    .take(140)
    .collect()
}

/// The catalog key that words why a tool call needs the user.
pub(super) fn permission_reason_key(reason: AgentPermissionReason) -> &'static str {
    match reason {
        AgentPermissionReason::Secrets => "chat-permission-reason-secrets",
        AgentPermissionReason::SecretsLeaving => "chat-permission-reason-secrets-leaving",
        AgentPermissionReason::SendsOut => "chat-permission-reason-sends-out",
        AgentPermissionReason::RemoteCode => "chat-permission-reason-remote-code",
        AgentPermissionReason::RewritesHistory => "chat-permission-reason-rewrites-history",
        AgentPermissionReason::DiscardsWork => "chat-permission-reason-discards-work",
        AgentPermissionReason::DisablesSafeguards => "chat-permission-reason-disables-safeguards",
        AgentPermissionReason::Destructive => "chat-permission-reason-destructive",
        AgentPermissionReason::ChangesProject => "chat-permission-reason-changes-project",
        AgentPermissionReason::ChangesOutsideProject => {
            "chat-permission-reason-changes-outside-project"
        }
        AgentPermissionReason::Uncertain => "chat-permission-reason-uncertain",
        AgentPermissionReason::Unavailable => "chat-permission-reason-unavailable",
        AgentPermissionReason::TooLong => "chat-permission-reason-too-long",
    }
}

pub(super) fn render_question_card(
    question: &crate::backend::PendingQuestion,
    step: usize,
    collapsed: bool,
    input: Option<Entity<TextInput>>,
    selected: &HashMap<usize, QuestionSelection>,
    messages: &MessageCache,
    cx: &mut Context<ChatScreen>,
) -> Div {
    let mut card = div()
        .debug_selector(|| "question-card".to_string())
        // Like the composer, the card keeps its own wheel; the space
        // beside it scrolls the transcript.
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
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
        .gap_2();
    let step = step.min(question.questions.len().saturating_sub(1));
    let card_label = if question
        .questions
        .get(step)
        .is_some_and(|entry| entry.permission.is_some())
    {
        tr("chat-permission-label")
    } else {
        tr("chat-question-label")
    };
    // Folding works like the Plan card: the header stays, naming the
    // question, so the transcript above can be read before answering.
    let header = div()
        .id("question-card-header")
        .debug_selector(|| "question-card-header".to_string())
        .role(gpui::Role::DisclosureTriangle)
        .aria_label(card_label.clone())
        .aria_expanded(!collapsed)
        .flex()
        .items_center()
        .gap_2()
        .min_w_0()
        .cursor_pointer()
        .on_click(cx.listener(|this, _event, _window, cx| {
            this.toggle_question_collapsed(cx);
        }))
        .child(motion::chevron(
            "question-card-chevron",
            !collapsed,
            px(14.),
            theme::text_secondary(),
        ))
        .child(
            div()
                .flex_none()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(gpui::rgb(theme::text_primary()))
                .child(card_label),
        )
        .when_some(
            collapsed
                .then(|| question.questions.get(step))
                .flatten()
                .map(|entry| {
                    if let Some(prompt) = &entry.permission {
                        prompt.action.clone()
                    } else if entry.header.trim().is_empty() {
                        entry.question.clone()
                    } else {
                        entry.header.clone()
                    }
                }),
            |header, summary| {
                header.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(summary),
                )
            },
        );
    card = card.child(header);
    if collapsed {
        return card;
    }
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
                    .child(messages.get(
                        "chat-question-position",
                        [
                            ("position", (step + 1).into()),
                            ("count", question.questions.len().into()),
                        ],
                    )),
            );
        }
        if let Some(prompt) = &entry.permission {
            block = block.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(gpui::text!(tr(permission_reason_key(prompt.reason)))),
            );
            block = block.child(
                div()
                    .id("permission-action")
                    .debug_selector(|| "permission-action".to_string())
                    .max_h(px(160.))
                    .overflow_y_scroll()
                    .px_2()
                    .py_1p5()
                    .rounded(theme::RADIUS_SM)
                    .bg(gpui::rgb(theme::bg_sidebar_pill()))
                    .font_family(crate::assets::FONT_MONO)
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_primary()))
                    .child(gpui::text!(prompt.action.clone())),
            );
        } else {
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
        }
        if entry.multi_select {
            block = block.child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(if has_more {
                        tr("chat-question-next-hint")
                    } else {
                        tr("chat-question-answer-hint")
                    }),
            );
        }
        for (option_index, option) in entry.options.iter().enumerate() {
            let selection = selected.get(&question_index);
            let is_picked = selection.is_some_and(|s| s.picked.contains(&option_index));
            let marker = div()
                .size_3()
                .when(entry.multi_select, |marker| marker.rounded_sm())
                .when(!entry.multi_select, |marker| marker.rounded_full())
                .border_1()
                .border_color(gpui::rgb(if is_picked {
                    theme::accent()
                } else {
                    theme::border()
                }))
                .when(is_picked, |dot| dot.bg(gpui::rgb(theme::accent())));
            let label = question_option_label(entry, option_index);
            // flex_1 is load-bearing: without it the row squeezes this
            // block to a character wide and the label wraps vertically.
            let label_element = if option.description.is_empty() {
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_primary()))
                    .child(gpui::text!(label))
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
                            .child(gpui::text!(label)),
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
                    .when(
                        entry.multi_select
                            && selection.is_some_and(|s| s.cursor == Some(option_index)),
                        |row| row.bg(gpui::rgb(theme::bg_sidebar_row_hover())),
                    )
                    .map(|element| {
                        let rest: gpui::Hsla = if entry.multi_select
                            && selection.is_some_and(|s| s.cursor == Some(option_index))
                        {
                            gpui::rgb(theme::bg_sidebar_row_hover()).into()
                        } else {
                            gpui::transparent_black()
                        };
                        crate::ui::motion::hover_background(
                            element,
                            rest,
                            gpui::rgb(theme::bg_sidebar_row_hover()),
                        )
                    })
                    .hover(|style| style.cursor_pointer())
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
                        .child(gpui::text!(if has_more {
                            tr("chat-next")
                        } else {
                            tr("chat-answer")
                        })),
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
            .child(tr("chat-skip")),
    )
}

/// Pulsing dots shown between send and the first streamed content. They
/// breathe on the shared low-rate clock, so waiting costs the same as a
/// spinner rather than a full-rate animation. They fade in when they
/// appear and leave at once when the reply arrives.
pub(super) fn render_waiting_indicator() -> impl IntoElement {
    const CYCLE: std::time::Duration = std::time::Duration::from_millis(1400);
    let indicator = div()
        .id("waiting-indicator")
        .role(gpui::Role::Status)
        .aria_label(tr("chat-thinking-label"))
        .flex()
        .items_center()
        .gap_2()
        .px_4()
        // The send preview's status line takes the same slot.
        .h(WAITING_ROW_HEIGHT)
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
                .child(tr("chat-thinking-label")),
        );
    motion::fade_in(indicator, "waiting-indicator-reveal")
}

#[cfg(test)]
mod motion_tests {
    use std::time::{Duration, Instant};

    use gpui::{Bounds, Context, Pixels, TestAppContext};

    use super::WAITING_ROW_HEIGHT;

    use crate::ui::chat::*;
    use crate::ui::motion;

    fn item(id: &str, item_type: &str, text: Option<&str>) -> AgentTimelineItem {
        AgentTimelineItem {
            label: None,
            persisted: None,
            live: None,
            truncated: None,
            revision: 0,
            id: id.to_string(),
            item_type: item_type.to_string(),
            role: None,
            title: None,
            text: text.map(str::to_string),
            status: None,
            input: None,
            output: None,
            created_ms: 0,
            merge: "replace".to_string(),
        }
    }

    fn user_item(id: &str, text: &str) -> AgentTimelineItem {
        AgentTimelineItem {
            role: Some("user".to_string()),
            ..item(id, "message", Some(text))
        }
    }

    fn screen(cx: &mut TestAppContext) -> Entity<ChatScreen> {
        cx.executor().allow_parking();
        cx.new(|cx| {
            let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
            let backend = std::sync::Arc::new(
                crate::backend::AgentBackend::for_test(
                    "http://127.0.0.1:9".to_string(),
                    String::new(),
                )
                .expect("backend"),
            );
            let mut chat = ChatScreen::new_inner_for_test(backend, "user".to_string(), cx);
            chat.selected_session = Some("s1".to_string());
            chat
        })
    }

    /// Pins the motion clock for one test and returns it to the real
    /// clock when the test ends, pass or fail.
    struct Clock(Instant);

    impl Clock {
        fn start() -> Self {
            let start = Instant::now();
            motion::set_test_now(Some(start));
            Self(start)
        }

        fn at(&self, offset: Duration) -> Instant {
            motion::set_test_now(Some(self.0 + offset));
            self.0 + offset
        }
    }

    impl Drop for Clock {
        fn drop(&mut self) {
            motion::set_test_now(None);
        }
    }

    /// Whether any transcript entrance still asks for frames at `now`.
    fn in_flight(
        chat: &Entity<ChatScreen>,
        now: Instant,
        cx: &mut gpui::VisualTestContext,
    ) -> bool {
        chat.update(cx, |chat, _| chat.row_motion.settle(now, false))
    }

    /// The chat pane's middle: the transcript and the slot under it.
    struct PaneHost {
        chat: Entity<ChatScreen>,
    }

    impl Render for PaneHost {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let pane = self.chat.update(cx, |chat, cx| {
                let transcript = chat.render_transcript(window, cx);
                let under = chat.render_under_transcript(window, cx);
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(transcript)
                    .children(under)
            });
            div().w(px(1200.)).h(px(800.)).flex().flex_col().child(pane)
        }
    }

    fn pane(
        cx: &mut TestAppContext,
        rows: usize,
    ) -> (Entity<ChatScreen>, &mut gpui::VisualTestContext) {
        let chat = screen(cx);
        chat.update(cx, |chat, _| {
            chat.replace_timeline(
                (0..rows)
                    .map(|index| item(&format!("h{index}"), "message", Some("Earlier row")))
                    .collect(),
            );
        });
        let (_host, cx) = cx.add_window_view(|_, _| PaneHost { chat: chat.clone() });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        cx.run_until_parked();
        (chat, cx)
    }

    fn redraw(chat: &Entity<ChatScreen>, cx: &mut gpui::VisualTestContext) {
        chat.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
    }

    fn bounds(cx: &mut gpui::VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} painted"))
    }

    /// Samples each selector's top at 60 fps through one entrance that
    /// began at `start`.
    fn tops_through_entrance(
        chat: &Entity<ChatScreen>,
        clock: &Clock,
        start: Duration,
        selectors: &[&'static str],
        cx: &mut gpui::VisualTestContext,
    ) -> Vec<Vec<Pixels>> {
        let mut tops = vec![Vec::new(); selectors.len()];
        let mut at = Duration::ZERO;
        while at <= motion::ENTER + Duration::from_millis(16) {
            clock.at(start + at);
            redraw(chat, cx);
            for (selector, tops) in selectors.iter().zip(&mut tops) {
                tops.push(bounds(cx, selector).origin.y);
            }
            at += Duration::from_millis(16);
        }
        tops
    }

    /// Every frame-to-frame move stays within what the easing allows: no
    /// single-frame jump.
    fn assert_glides(tops: &[Pixels], label: &str) {
        let total = (tops[0] - tops[tops.len() - 1]).abs();
        // ease_out_cubic is steepest at its start: 3x the mean speed, or
        // 16 ms * 3 / 180 ms of the distance per frame.
        let limit = total * 0.3 + px(1.);
        for pair in tops.windows(2) {
            assert!(
                (pair[1] - pair[0]).abs() <= limit,
                "{label} jumped {:?} in one frame (limit {limit:?}): {tops:?}",
                pair[1] - pair[0]
            );
        }
    }

    #[gpui::test]
    fn a_row_arriving_while_scrolled_up_rises_without_a_layout_change(cx: &mut TestAppContext) {
        let clock = Clock::start();
        let (chat, cx) = pane(cx, 3);
        assert!(!in_flight(&chat, clock.0, cx), "history arrives at rest");
        chat.update(cx, |chat, cx| {
            chat.list_state.pause_following_tail();
            chat.apply_incoming_item("s1", item("live", "message", Some("Hello")), cx);
        });
        redraw(&chat, cx);
        let entering = bounds(cx, "timeline-row-live");

        let midway = clock.at(motion::ENTER / 2);
        let progress = chat.update(cx, |chat, _| chat.row_motion.arrival("live", midway, false));
        assert!(
            progress > 0. && progress < 1.,
            "mid-entrance, got {progress}"
        );
        assert!(in_flight(&chat, midway, cx), "frames continue mid-entrance");

        let done = clock.at(motion::ENTER);
        redraw(&chat, cx);
        let settled = bounds(cx, "timeline-row-live");
        assert_eq!(entering.size, settled.size, "the rise never changes layout");
        assert_eq!(entering.origin.y - settled.origin.y, motion::RISE);
        assert!(!in_flight(&chat, done, cx), "frames stop once it settles");

        // Scrolled away and back, the row is drawn at rest: no replay.
        chat.update(cx, |chat, _| {
            chat.list_state.scroll_to(gpui::ListOffset {
                item_ix: 0,
                offset_in_item: px(0.),
            });
        });
        redraw(&chat, cx);
        chat.update(cx, |chat, _| chat.list_state.scroll_to_end());
        redraw(&chat, cx);
        assert_eq!(cx.debug_bounds("timeline-row-live"), Some(settled));
        assert!(!in_flight(&chat, done, cx));
    }

    #[gpui::test]
    fn a_tail_row_grows_into_place_and_older_rows_glide(cx: &mut TestAppContext) {
        let clock = Clock::start();
        let (chat, cx) = pane(cx, 30);
        let before = bounds(cx, "timeline-row-h29");
        chat.update(cx, |chat, cx| {
            chat.apply_incoming_item(
                "s1",
                item("live", "message", Some("Hello\n\nA second paragraph")),
                cx,
            );
        });
        let tops = tops_through_entrance(&chat, &clock, Duration::ZERO, &["timeline-row-h29"], cx)
            .remove(0);
        assert!(
            (tops[0] - before.origin.y).abs() <= px(1.),
            "older rows hold still on the arrival frame: {before:?} then {tops:?}"
        );
        assert!(
            tops[tops.len() - 1] < before.origin.y,
            "older rows made room"
        );
        assert!(
            tops.windows(2).all(|pair| pair[1] <= pair[0]),
            "older rows only rise: {tops:?}"
        );
        assert_glides(&tops, "an older row");
        assert!(!in_flight(&chat, clock.at(motion::ENTER), cx));

        // The settled layout is the one without motion.
        let older = bounds(cx, "timeline-row-h29");
        let live = bounds(cx, "timeline-row-live");
        chat.update(cx, |chat, _| {
            let rows = chat.timeline.clone();
            chat.replace_timeline(rows);
        });
        redraw(&chat, cx);
        assert_eq!(bounds(cx, "timeline-row-h29"), older);
        assert_eq!(bounds(cx, "timeline-row-live"), live);
    }

    #[gpui::test]
    fn a_reply_streaming_while_it_opens_ends_at_its_true_height(cx: &mut TestAppContext) {
        let clock = Clock::start();
        let (chat, cx) = pane(cx, 30);
        chat.update(cx, |chat, cx| {
            chat.apply_incoming_item("s1", item("reply", "message", Some("Answer:")), cx);
        });
        redraw(&chat, cx);
        clock.at(motion::ENTER / 3);
        redraw(&chat, cx);
        chat.update(cx, |chat, cx| {
            chat.apply_incoming_item(
                "s1",
                AgentTimelineItem {
                    merge: "append".to_string(),
                    text: Some("\n\nMore lines\n\nAnd more".to_string()),
                    ..item("reply", "message", None)
                },
                cx,
            );
        });
        for step in [motion::ENTER / 2, motion::ENTER * 3 / 4, motion::ENTER] {
            clock.at(step);
            redraw(&chat, cx);
        }
        let older = bounds(cx, "timeline-row-h29");
        let reply = bounds(cx, "timeline-row-reply");
        chat.update(cx, |chat, _| {
            let rows = chat.timeline.clone();
            chat.replace_timeline(rows);
        });
        redraw(&chat, cx);
        assert_eq!(bounds(cx, "timeline-row-h29"), older);
        assert_eq!(bounds(cx, "timeline-row-reply"), reply);
    }

    #[gpui::test]
    fn the_sent_bubble_rises_in_and_its_status_does_not(cx: &mut TestAppContext) {
        let clock = Clock::start();
        let (chat, cx) = pane(cx, 3);
        chat.update(cx, |chat, _| {
            let preview = sending::SendPreview::new(Some("s1".into()), "hello", &[])
                .capture_timeline(&chat.timeline);
            chat.sending_previews.insert("send-1".into(), preview);
        });
        redraw(&chat, cx);
        let entering = bounds(cx, "pending-send-body");
        let status = bounds(cx, "pending-send-status");
        clock.at(motion::ENTER);
        redraw(&chat, cx);
        let settled = bounds(cx, "pending-send-body");
        assert_eq!(entering.origin.y - settled.origin.y, motion::RISE);
        assert_eq!(entering.size, settled.size);
        assert_eq!(
            bounds(cx, "pending-send-status"),
            status,
            "the status line has no offset of its own or its parent's"
        );
        assert_eq!(status.size.height, WAITING_ROW_HEIGHT);
    }

    #[gpui::test]
    fn sending_opens_the_slot_so_older_rows_glide_up(cx: &mut TestAppContext) {
        let clock = Clock::start();
        let (chat, cx) = pane(cx, 30);
        let before = bounds(cx, "timeline-row-h29");
        chat.update(cx, |chat, _| {
            let preview = sending::SendPreview::new(Some("s1".into()), "hello", &[])
                .capture_timeline(&chat.timeline);
            chat.sending_previews.insert("send-1".into(), preview);
        });
        let tops = tops_through_entrance(&chat, &clock, Duration::ZERO, &["timeline-row-h29"], cx)
            .remove(0);
        assert!(
            (tops[0] - before.origin.y).abs() <= px(1.),
            "older rows hold still on the send frame: {before:?} then {tops:?}"
        );
        assert!(tops[tops.len() - 1] < before.origin.y, "the slot made room");
        assert!(
            tops.windows(2).all(|pair| pair[1] <= pair[0]),
            "older rows only rise: {tops:?}"
        );
        assert_glides(&tops, "an older row");
        let older = bounds(cx, "timeline-row-h29");
        let bubble = bounds(cx, "pending-send-body");
        let status = bounds(cx, "pending-send-status");

        // The settled layout is the one without motion.
        cx.update(|_, cx| cx.set_reduce_motion(true));
        redraw(&chat, cx);
        assert_eq!(bounds(cx, "timeline-row-h29"), older);
        assert_eq!(bounds(cx, "pending-send-body"), bubble);
        assert_eq!(bounds(cx, "pending-send-status"), status);
    }

    #[gpui::test]
    fn the_real_row_takes_over_from_the_bubble_without_a_jump(cx: &mut TestAppContext) {
        let clock = Clock::start();
        let (chat, cx) = pane(cx, 30);
        chat.update(cx, |chat, _| {
            let preview = sending::SendPreview::new(Some("s1".into()), "hello", &[])
                .capture_timeline(&chat.timeline);
            chat.sending_previews.insert("send-1".into(), preview);
        });
        redraw(&chat, cx);
        let start = motion::ENTER * 2;
        clock.at(start);
        redraw(&chat, cx);
        let bubble = bounds(cx, "pending-send-body");
        let older = bounds(cx, "timeline-row-h29");
        let status = bounds(cx, "pending-send-status");

        chat.update(cx, |chat, cx| {
            chat.handle_service_event(
                AgentServiceEvent::TimelineItem {
                    session_id: "s1".into(),
                    run_id: Some("run-1".into()),
                    item: user_item("mine", "hello"),
                },
                cx,
            );
        });
        let tops = tops_through_entrance(
            &chat,
            &clock,
            start,
            &[
                "timeline-row-mine",
                "timeline-row-h29",
                "pending-send-status",
            ],
            cx,
        );
        assert!(cx.debug_bounds("pending-send-body").is_none(), "handed off");
        assert!(
            (tops[0][0] - bubble.origin.y).abs() <= px(1.),
            "the row starts where the bubble was: {bubble:?} then {:?}",
            tops[0]
        );
        assert!(
            (tops[1][0] - older.origin.y).abs() <= px(1.),
            "older rows hold still on the handoff frame: {older:?} then {:?}",
            tops[1]
        );
        assert!(
            tops[2].iter().all(|top| *top == status.origin.y),
            "the status line keeps its slot: {:?}",
            tops[2]
        );
        assert_glides(&tops[0], "the handed-off row");
        assert_glides(&tops[1], "an older row");
    }

    #[gpui::test]
    fn history_task_switches_paging_and_reconnects_do_not_animate(cx: &mut TestAppContext) {
        let clock = Clock::start();
        let chat = screen(cx);
        chat.update(cx, |chat, cx| {
            let now = clock.0;
            // A history load.
            chat.replace_timeline(vec![
                user_item("u1", "hi"),
                item("a1", "message", Some("yo")),
            ]);
            assert_eq!(chat.row_motion.arrival("a1", now, false), 1.);
            // Another task's snapshot replaces a row that was still entering.
            chat.apply_incoming_item("s1", item("a2", "message", Some("more")), cx);
            assert!(chat.row_motion.arrival("a2", now, false) < 1.);
            chat.set_active_session(
                AgentSessionSummary {
                    web_enabled: true,
                    permission_mode: Default::default(),
                    state: maple_agent::agent::AgentTaskState::Active,
                    acp: false,
                    id: "s2".to_string(),
                    title: "Other".to_string(),
                    project_root: "/tmp/proj".to_string(),
                    created_ms: 0,
                    updated_ms: 0,
                    message_count: 1,
                    model: None,
                },
                vec![
                    item("a2", "message", Some("more")),
                    item("b1", "message", None),
                ],
                HashMap::new(),
                cx,
            );
            assert_eq!(chat.row_motion.arrival("a2", now, false), 1.);
            assert_eq!(chat.row_motion.arrival("b1", now, false), 1.);
            // A forward history page is open: live rows wait for a reload.
            chat.pages.after = true;
            chat.apply_incoming_item("s2", item("paged", "message", Some("x")), cx);
            assert_eq!(chat.row_motion.arrival("paged", now, false), 1.);
            chat.pages.after = false;
            // Pushes replayed after a reconnect rebuild the transcript.
            chat.row_motion.set_quiet(true);
            chat.apply_incoming_item("s2", item("replayed", "message", Some("x")), cx);
            chat.row_motion.set_quiet(false);
            assert_eq!(chat.row_motion.arrival("replayed", now, false), 1.);
            assert!(!chat.row_motion.settle(now, false));
            // Reduced motion: a live row is at rest at once.
            chat.apply_incoming_item("s2", item("live", "message", Some("x")), cx);
            assert_eq!(chat.row_motion.arrival("live", now, true), 1.);
            assert!(!chat.row_motion.settle(now, true));
        });
    }

    #[gpui::test]
    fn streaming_into_a_row_does_not_restart_its_entrance(cx: &mut TestAppContext) {
        let clock = Clock::start();
        let chat = screen(cx);
        let chunk = || AgentTimelineItem {
            merge: "append".to_string(),
            text: Some(" more".to_string()),
            ..item("reply", "message", None)
        };
        chat.update(cx, |chat, cx| {
            chat.apply_incoming_item("s1", item("reply", "message", Some("Hi")), cx);
            let midway = clock.at(motion::ENTER / 2);
            let before = chat.row_motion.arrival("reply", midway, false);
            chat.apply_incoming_item("s1", chunk(), cx);
            assert_eq!(chat.row_motion.arrival("reply", midway, false), before);
            let done = clock.at(motion::ENTER);
            assert!(!chat.row_motion.settle(done, false));
            chat.apply_incoming_item("s1", chunk(), cx);
            assert_eq!(chat.timeline[0].text.as_deref(), Some("Hi more more"));
            assert_eq!(chat.row_motion.arrival("reply", done, false), 1.);
            assert!(!chat.row_motion.settle(done, false));
        });
    }

    #[gpui::test]
    fn the_row_replacing_the_send_preview_does_not_enter_again(cx: &mut TestAppContext) {
        let clock = Clock::start();
        let chat = screen(cx);
        chat.update(cx, |chat, cx| {
            let preview = sending::SendPreview::new(Some("s1".into()), "hello", &[])
                .capture_timeline(&chat.timeline);
            chat.sending_previews.insert("send-1".into(), preview);
            assert!(chat.has_send_feedback());
            let projected = |id: &str, run: &str| AgentServiceEvent::TimelineItem {
                session_id: "s1".into(),
                run_id: Some(run.into()),
                item: user_item(id, "hello"),
            };
            chat.handle_service_event(projected("mine", "run-1"), cx);
            assert_eq!(chat.timeline.len(), 1);
            assert_eq!(chat.row_motion.arrival("mine", clock.0, false), 1.);
            // A message sent from another computer has no preview here.
            chat.handle_service_event(projected("theirs", "run-2"), cx);
            assert!(chat.row_motion.arrival("theirs", clock.0, false) < 1.);
        });
    }

    #[gpui::test]
    fn opening_a_task_shows_the_spinner_only_after_a_moment(cx: &mut TestAppContext) {
        let clock = Clock::start();
        let chat = screen(cx);
        chat.update(cx, |chat, cx| {
            chat.select_session("s2", cx);
            assert_eq!(chat.loading_session.as_deref(), Some("s2"));
            assert!(!chat.loading_spinner_due());
            clock.at(super::super::LOADING_SPINNER_DELAY - Duration::from_millis(1));
            assert!(!chat.loading_spinner_due());
            clock.at(super::super::LOADING_SPINNER_DELAY);
            assert!(chat.loading_spinner_due());
            // Clicking the same task again keeps the open's start time.
            chat.select_session("s2", cx);
            assert!(chat.loading_spinner_due());
        });
    }
}
