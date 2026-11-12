//! The line a turn's steps fold under.
//!
//! A turn is what Maple does between two messages from the user. Its
//! thinking rows and tool calls are its steps. The first step carries the
//! turn's line: the Maple mark, what Maple is doing, and for how long. The
//! steps stay folded under the line until it is clicked. Before a turn has
//! a step, the same line waits in the slot under the transcript.

use std::time::{Duration, Instant};

use gpui::{Context, Div, SharedString, div, prelude::*, px};
use maple_agent::agent::AgentTimelineItem;

use super::ChatScreen;
use super::step;
use super::transcript::{
    WAITING_ROW_HEIGHT, format_subagent_elapsed, has_tool_input, plan_entries, plan_finished,
};
use crate::ui::i18n::{MessageCache, tr};
use crate::ui::morph_text::morph_text;
use crate::ui::motion;
use crate::ui::theme;

/// Gap between two repaints of a running turn's time where loops hold
/// still: the seconds stay true without the tenths.
const RUN_TICK: Duration = Duration::from_secs(1);

/// One pass of the sheen across the mark, and of the shine across the
/// line's words. Different lengths, so the two drift instead of marching.
const MARK_CYCLE: Duration = Duration::from_millis(1600);
const SHINE_CYCLE: Duration = Duration::from_millis(1900);

/// What a transcript row is to its turn's line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TurnRow {
    /// A message, a notice, an error: always shown.
    Plain,
    /// The turn's first step. The line is drawn above it.
    Lead,
    /// A later step, folded under the lead at this index.
    Step(usize),
}

/// Each loaded row's place in its turn, kept beside the timeline so no
/// frame regroups it.
#[derive(Default)]
pub(super) struct Turns {
    rows: Vec<TurnRow>,
    /// The lead of the turn at the end of the loaded rows, once that turn
    /// has a step.
    tail: Option<usize>,
}

fn starts_turn(item: &AgentTimelineItem) -> bool {
    item.item_type == "message" && item.role.as_deref() == Some("user")
}

fn is_tool(item: &AgentTimelineItem) -> bool {
    matches!(item.item_type.as_str(), "tool" | "toolCall")
}

fn is_step(item: &AgentTimelineItem) -> bool {
    if is_tool(item) {
        // An unfinished list shows only in the card above the composer.
        // Its row draws nothing, so it is no step to fold or to lead with.
        return !has_tool_input(item, "todos")
            || plan_entries(item).is_some_and(|plan| plan_finished(&plan));
    }
    matches!(item.item_type.as_str(), "thinking" | "reasoning")
}

/// What decides a row's place in its turn: whether it starts one, and
/// whether it is a step.
pub(super) fn part(item: &AgentTimelineItem) -> (bool, bool) {
    (starts_turn(item), is_step(item))
}

impl Turns {
    /// Group a whole timeline: a snapshot, a history page, a trimmed window.
    pub(super) fn rebuild(&mut self, timeline: &[AgentTimelineItem]) {
        self.rows.clear();
        self.tail = None;
        for item in timeline {
            self.push(item);
        }
    }

    /// One row arrived at the end.
    pub(super) fn push(&mut self, item: &AgentTimelineItem) {
        let row = if starts_turn(item) {
            self.tail = None;
            TurnRow::Plain
        } else if !is_step(item) {
            TurnRow::Plain
        } else if let Some(lead) = self.tail {
            TurnRow::Step(lead)
        } else {
            self.tail = Some(self.rows.len());
            TurnRow::Lead
        };
        self.rows.push(row);
    }

    pub(super) fn row(&self, index: usize) -> TurnRow {
        self.rows.get(index).copied().unwrap_or(TurnRow::Plain)
    }

    pub(super) fn tail(&self) -> Option<usize> {
        self.tail
    }

    /// One past the last step of the turn led by the row at `lead`.
    pub(super) fn end(&self, lead: usize) -> usize {
        let mut end = lead + 1;
        for (index, row) in self.rows.iter().enumerate().skip(end) {
            match row {
                TurnRow::Step(of) if *of == lead => end = index + 1,
                TurnRow::Lead => break,
                _ => {}
            }
        }
        end
    }
}

/// How long a run has worked: the time since it began, less the time it
/// spent waiting on the user.
#[derive(Clone, Copy, Debug)]
pub(super) struct RunClock {
    began: Instant,
    waited: Duration,
    waiting_since: Option<Instant>,
}

/// A run this window saw start, with its clock.
pub(super) struct TimedRun {
    run_id: String,
    clock: RunClock,
    /// The lead row of the turn the run is working on, once that turn has
    /// a step: the row its time is kept under when the turn ends.
    lead: Option<String>,
}

impl RunClock {
    fn new(now: Instant) -> Self {
        Self {
            began: now,
            waited: Duration::ZERO,
            waiting_since: None,
        }
    }

    fn worked(&self, now: Instant) -> Duration {
        self.waiting_since
            .unwrap_or(now)
            .saturating_duration_since(self.began)
            .saturating_sub(self.waited)
    }

    /// Start counting again from `now`, for the run's next turn. A wait
    /// under way carries on.
    fn restart(&mut self, now: Instant) {
        self.began = now;
        self.waited = Duration::ZERO;
        self.waiting_since = self.waiting_since.map(|_| now);
    }

    /// A question for the user stops the clock; its answer starts it again.
    fn set_waiting(&mut self, waiting: bool, now: Instant) {
        match (waiting, self.waiting_since) {
            (true, None) => self.waiting_since = Some(now),
            (false, Some(since)) => {
                self.waited += now.saturating_duration_since(since);
                self.waiting_since = None;
            }
            _ => {}
        }
    }
}

/// What the line says while its turn runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Thinking,
    Working,
    Writing,
    /// A question or a permission request waits on the user.
    Waiting,
}

impl Phase {
    fn label(self) -> SharedString {
        tr(match self {
            Phase::Thinking => "chat-thinking-label",
            Phase::Working => "chat-turn-working",
            Phase::Writing => "chat-turn-writing",
            Phase::Waiting => "chat-turn-waiting",
        })
    }
}

/// What a turn's line shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Line {
    /// The turn is running. A run this window did not see start has no
    /// clock, and its line shows no time.
    Live {
        phase: Phase,
        worked: Option<Duration>,
        /// Loops hold still: the mark rests and the time drops its tenths.
        still: bool,
    },
    /// The turn is over. `worked` is how long it took, when this window
    /// timed it; a turn read back from history has none.
    Done { worked: Option<Duration> },
}

/// "Worked for 11s", "Worked for 1m 4s", or "Worked" for an untimed turn.
fn worked_label(messages: &MessageCache, worked: Option<Duration>) -> SharedString {
    let Some(worked) = worked else {
        return tr("chat-turn-worked");
    };
    let seconds = (worked.as_secs_f32().round() as u64).max(1);
    if seconds < 60 {
        messages.get("chat-turn-worked-seconds", [("seconds", seconds.into())])
    } else {
        messages.get(
            "chat-turn-worked-minutes",
            [
                ("minutes", (seconds / 60).into()),
                ("seconds", (seconds % 60).into()),
            ],
        )
    }
}

/// "6.8s" under a minute, then a clock like the subagent rows.
pub(super) fn live_elapsed(worked: Duration, tenths: bool) -> String {
    if worked.as_secs() >= 60 {
        format_subagent_elapsed(worked)
    } else if tenths {
        let tenths = worked.as_millis() / 100;
        format!("{}.{}s", tenths / 10, tenths % 10)
    } else {
        format!("{}s", worked.as_secs())
    }
}

#[derive(Clone, Copy)]
enum Mark {
    /// A coral sheen crosses the mark.
    Working,
    /// Nothing is running: the mark rests in the running tone.
    Resting,
    /// Maple waits on the user: the mark rests in coral.
    Waiting,
}

/// The Maple mark on a 5 by 5 grid: a block with the notch of the M cut
/// from its top edge. `phase` is how far a working mark's sheen is through
/// its pass.
fn maple_mark(mark: Mark, phase: f32) -> Div {
    const SHAPE: [[bool; 5]; 5] = [
        [true, false, false, false, true],
        [true, true, false, true, true],
        [true; 5],
        [true; 5],
        [true; 5],
    ];
    const CELL: gpui::Pixels = px(3.);
    let resting: gpui::Hsla = gpui::rgb(theme::status_running()).into();
    let coral: gpui::Hsla = gpui::rgb(theme::accent_ink()).into();
    let mut grid = div()
        .debug_selector(|| "turn-mark".to_string())
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(1.));
    for (row, cells) in SHAPE.iter().enumerate() {
        let mut line = div().flex().gap(px(1.));
        for (column, on) in cells.iter().enumerate() {
            let cell = div().size(CELL).rounded(px(0.5));
            line = line.child(if !on {
                cell
            } else {
                match mark {
                    Mark::Working => {
                        // The sheen runs corner to corner, then rests for
                        // the last fifth of the pass.
                        let head = -2.2 + (phase / 0.8).min(1.) * 10.8;
                        let near =
                            (1. - (column as f32 + row as f32 * 0.6 - head).abs() / 2.).max(0.);
                        let level = motion::smoothstep(near);
                        cell.bg(motion::mix(resting, coral, level))
                            .opacity(0.5 + 0.5 * level)
                    }
                    Mark::Resting => cell.bg(resting).opacity(0.9),
                    Mark::Waiting => cell.bg(coral),
                }
            });
        }
        grid = grid.child(line);
    }
    grid
}

/// The line, as tall as the slot under the transcript it first shows in,
/// so taking its place in the list moves nothing. `fold` names the turn's
/// lead and says whether its steps show; the slot's line has no steps to
/// open and passes `None`.
pub(super) fn render_line(
    line: Line,
    fold: Option<(&str, bool)>,
    chat: &gpui::WeakEntity<ChatScreen>,
    messages: &MessageCache,
) -> Div {
    let key = fold.map_or("slot", |(lead, _)| lead);
    let id = |part: &str| SharedString::from(format!("turn-{part}-{key}"));
    let (label, mark, time) = match line {
        Line::Live {
            phase,
            worked,
            still,
        } => {
            let mark = match phase {
                Phase::Waiting => Mark::Waiting,
                _ if still => Mark::Resting,
                _ => Mark::Working,
            };
            // Beside "Waiting for you" the stopped time would read as
            // how long the user has kept Maple waiting.
            let time = worked
                .filter(|_| phase != Phase::Waiting)
                .map(|worked| live_elapsed(worked, !still));
            (phase.label(), mark, time)
        }
        Line::Done { worked } => (worked_label(messages, worked), Mark::Resting, None),
    };
    let running = matches!(mark, Mark::Working);
    let words = morph_text(id("words"), label.clone());
    let button = div()
        .id(id("line"))
        .debug_selector(|| "turn-line".to_string())
        .aria_label(label)
        .h(px(32.))
        .flex()
        .items_center()
        .gap_2p5()
        .text_sm()
        .font_weight(gpui::FontWeight::MEDIUM)
        // The shine carries the words while Maple works, so they rest
        // dimmer under it.
        .text_color(gpui::rgb(if running {
            theme::text_muted()
        } else {
            theme::text_secondary()
        }))
        // The mark sits in the column of the steps' nodes, and the words
        // start where the steps' titles do.
        .child(
            div()
                .flex_none()
                .w(step::NODE)
                .mr(px(2.))
                .flex()
                .justify_center()
                .child(if running {
                    motion::ticker(id("mark"), MARK_CYCLE, |phase| {
                        maple_mark(Mark::Working, phase).into_any_element()
                    })
                    .into_any_element()
                } else {
                    maple_mark(mark, 0.).into_any_element()
                }),
        )
        .child(if running {
            words.shine(SHINE_CYCLE, gpui::rgb(theme::text_heading()))
        } else {
            words
        })
        .children(time.map(|time| {
            div()
                .debug_selector(|| "turn-time".to_string())
                .font_family(crate::assets::FONT_MONO)
                .font_weight(gpui::FontWeight::NORMAL)
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(gpui::text!(time))
        }));
    let row = div().h(WAITING_ROW_HEIGHT).flex().items_center();
    let Some((lead, open)) = fold else {
        return row.child(button.role(gpui::Role::Status));
    };
    let lead = lead.to_owned();
    let chat = chat.clone();
    row.child(
        button
            .role(gpui::Role::DisclosureTriangle)
            .aria_expanded(open)
            .hover(|style| {
                style
                    .cursor_pointer()
                    .text_color(gpui::rgb(theme::text_primary()))
            })
            .child(motion::chevron(
                id("chevron"),
                open,
                px(14.),
                theme::text_muted(),
            ))
            .on_click(move |_event, _window, cx: &mut gpui::App| {
                chat.update(cx, |chat, cx| chat.toggle_turn(&lead, cx)).ok();
            }),
    )
}

impl ChatScreen {
    /// Whether the run's line waits in the slot under the transcript:
    /// nothing has come back yet, so no step can carry it.
    pub(super) fn line_in_slot(&self) -> bool {
        self.awaiting_first_token && self.is_run_active() && !self.has_send_feedback()
    }

    /// Whether the run on screen is working on the turn led by the row at
    /// `lead`. Until a new run's first row lands the last turn is an
    /// earlier one, and the line waits in the slot instead. A window paged
    /// back in history does not hold the turn the run works on at all.
    fn turn_runs(&self, lead: usize) -> bool {
        self.turns.tail() == Some(lead)
            && self.is_run_active()
            && !self.awaiting_first_token
            && !self.pages.after
    }

    /// The line of the turn led by the row at `lead`.
    pub(super) fn turn_line(&self, lead: usize, still: bool) -> Line {
        if self.turn_runs(lead) {
            return self.live_line(still);
        }
        Line::Done {
            worked: self
                .timeline
                .get(lead)
                .and_then(|item| self.turn_worked.get(&item.id))
                .copied(),
        }
    }

    /// The line of the run on screen.
    pub(super) fn live_line(&self, still: bool) -> Line {
        let phase = if self.current_question().is_some() {
            Phase::Waiting
        } else {
            match self.timeline.last() {
                Some(item) if is_tool(item) => Phase::Working,
                Some(item) if item.item_type == "message" && !starts_turn(item) => Phase::Writing,
                _ => Phase::Thinking,
            }
        };
        let now = motion::now();
        Line::Live {
            phase,
            worked: self
                .selected_session
                .as_ref()
                .and_then(|session| self.run_clocks.get(session))
                .map(|timed| timed.clock.worked(now)),
            still,
        }
    }

    /// Whether the steps of the turn led by the row at `lead` show. "Show
    /// tool call details" opens every turn; a click on the line inverts
    /// that for its turn.
    pub(super) fn turn_open(&self, lead: usize) -> bool {
        self.timeline
            .get(lead)
            .is_some_and(|item| self.tool_details != self.toggled_turns.contains(&item.id))
    }

    /// Show or fold a turn's steps and re-measure their rows.
    pub(super) fn toggle_turn(&mut self, lead_id: &str, cx: &mut Context<Self>) {
        let Some(&(lead, _)) = self.timeline_index.get(lead_id) else {
            return;
        };
        if !self.toggled_turns.remove(lead_id) {
            self.toggled_turns.insert(lead_id.to_owned());
        }
        let open = self.turn_open(lead);
        self.row_motion.disclose_turn(lead_id, open, motion::now());
        if !open {
            self.keep_application_selection_shown(lead);
        } else if self.list_state.is_following_tail()
            && !self.turn_runs(lead)
            && let Some((first, _, into)) = self.painted_rows.get()
            && into >= px(0.)
        {
            // At its end the list names no row to hold, and would keep
            // its tail in view and carry the clicked line away. Hold the
            // first row on screen instead, so a finished turn's steps open
            // under its line. A running turn keeps following, so its
            // newest step stays in view.
            self.list_state.scroll_to(gpui::ListOffset {
                item_ix: first,
                offset_in_item: into,
            });
        }
        self.list_state.remeasure_items(lead..self.turns.end(lead));
        cx.notify();
    }

    /// A run this window sees start gets a clock. Both the send that
    /// started it and its first event report it; the second changes
    /// nothing.
    pub(super) fn run_began(&mut self, session_id: &str, run_id: &str, cx: &mut Context<Self>) {
        if self
            .run_clocks
            .get(session_id)
            .is_some_and(|timed| timed.run_id == run_id)
        {
            return;
        }
        self.run_clocks.insert(
            session_id.to_owned(),
            TimedRun {
                run_id: run_id.to_owned(),
                clock: RunClock::new(motion::now()),
                lead: None,
            },
        );
        self.schedule_run_tick(cx);
    }

    /// A row arrived at the end of the task while its run is timed. A
    /// turn's first step is the row the turn's time is kept under. A
    /// message from the user, queued or steering the run, ends the turn
    /// before it, and the time starts again for the next.
    pub(super) fn time_arrival(&mut self, session_id: &str, index: usize) {
        let Some(timed) = self.run_clocks.get_mut(session_id) else {
            return;
        };
        let Some(item) = self.timeline.get(index) else {
            return;
        };
        if self.turns.row(index) == TurnRow::Lead {
            timed.lead = Some(item.id.clone());
        } else if starts_turn(item) {
            let now = motion::now();
            if let Some(lead) = timed.lead.take() {
                self.turn_worked.insert(lead, timed.clock.worked(now));
            }
            timed.clock.restart(now);
        }
    }

    /// Keep the clocks in step with the runs and the questions, wherever
    /// either changed. A run that left `active_runs` stops its clock, and
    /// the turn it was working on keeps how long it worked. A run that
    /// waits on the user holds its time.
    pub(super) fn sync_run_clocks(&mut self) {
        if self.run_clocks.is_empty() {
            return;
        }
        let now = motion::now();
        let ended: Vec<String> = self
            .run_clocks
            .iter()
            .filter(|(session, timed)| self.active_runs.get(*session) != Some(&timed.run_id))
            .map(|(session, _)| session.clone())
            .collect();
        for session in ended {
            let Some(timed) = self.run_clocks.remove(&session) else {
                continue;
            };
            // A run that ended before its turn had a step has no line to
            // say the time on.
            if self.is_selected(&session)
                && let Some(lead) = timed
                    .lead
                    .filter(|lead| self.timeline_index.contains_key(lead))
            {
                self.turn_worked.insert(lead, timed.clock.worked(now));
            }
        }
        let questions = &self.pending_questions;
        for (session, timed) in &mut self.run_clocks {
            let waiting = questions
                .iter()
                .any(|question| &question.session_id == session);
            timed.clock.set_waiting(waiting, now);
        }
    }

    /// A live row took its persisted id; what was kept under the old one
    /// follows it.
    pub(super) fn rekey_turn(&mut self, old: &str, new: &str) {
        if self.toggled_turns.remove(old) {
            self.toggled_turns.insert(new.to_owned());
        }
        if let Some(worked) = self.turn_worked.remove(old) {
            self.turn_worked.insert(new.to_owned(), worked);
        }
        for timed in self.run_clocks.values_mut() {
            if timed.lead.as_deref() == Some(old) {
                timed.lead = Some(new.to_owned());
            }
        }
    }

    /// Paint once a second while a run is timed, so the time on its line
    /// stays true where loops hold still: under reduced motion, and in a
    /// window that is not active. The last run to end stops the timer.
    fn schedule_run_tick(&self, cx: &mut Context<Self>) {
        if self.run_clocks.is_empty() || self.run_tick_pending.replace(true) {
            return;
        }
        cx.spawn(async move |entity, cx| {
            cx.background_executor().timer(RUN_TICK).await;
            entity
                .update(cx, |this, cx| {
                    this.run_tick_pending.set(false);
                    if this.run_clocks.is_empty() {
                        return;
                    }
                    this.schedule_run_tick(cx);
                    if this.is_run_active() {
                        cx.notify();
                    }
                })
                .ok();
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(item_type: &str, role: Option<&str>) -> AgentTimelineItem {
        AgentTimelineItem {
            label: None,
            persisted: None,
            live: None,
            truncated: None,
            revision: 0,
            id: String::new(),
            item_type: item_type.into(),
            role: role.map(str::to_owned),
            title: None,
            text: None,
            status: None,
            input: None,
            output: None,
            created_ms: 0,
            merge: "replace".into(),
        }
    }

    #[test]
    fn a_turn_folds_its_later_steps_under_its_first() {
        let timeline = [
            item("message", Some("user")),
            item("thinking", None),
            item("message", Some("assistant")),
            item("tool", None),
            item("error", None),
            item("message", Some("user")),
            item("message", Some("assistant")),
            item("toolCall", None),
        ];
        let mut turns = Turns::default();
        turns.rebuild(&timeline);
        let rows: Vec<_> = (0..timeline.len()).map(|index| turns.row(index)).collect();
        assert_eq!(
            rows,
            [
                TurnRow::Plain,
                TurnRow::Lead,
                TurnRow::Plain,
                TurnRow::Step(1),
                TurnRow::Plain,
                TurnRow::Plain,
                TurnRow::Plain,
                TurnRow::Lead,
            ]
        );
        // The first turn ends at its last step, not at the error after it.
        assert_eq!(turns.end(1), 4);
        assert_eq!(turns.end(7), 8);
        assert_eq!(turns.tail(), Some(7));
    }

    #[test]
    fn rows_arriving_one_by_one_group_like_a_snapshot() {
        let timeline = [
            item("tool", None),
            item("reasoning", None),
            item("message", Some("user")),
            item("message", Some("assistant")),
            item("tool", None),
        ];
        let mut snapshot = Turns::default();
        snapshot.rebuild(&timeline);
        let mut live = Turns::default();
        for row in &timeline {
            live.push(row);
        }
        assert_eq!(live.rows, snapshot.rows);
        assert_eq!(live.tail, snapshot.tail);
        // A window that starts mid-turn leads with its first loaded step.
        assert_eq!(snapshot.row(0), TurnRow::Lead);
        // A new message from the user starts a turn with no line yet.
        live.push(&item("message", Some("user")));
        assert_eq!(live.tail(), None);
    }

    #[test]
    fn the_clock_stops_while_a_question_waits() {
        let began = Instant::now();
        let at = |seconds| began + Duration::from_secs(seconds);
        let mut clock = RunClock::new(began);
        clock.set_waiting(true, at(4));
        // Waiting: the time holds, however long the answer takes.
        assert_eq!(clock.worked(at(30)), Duration::from_secs(4));
        // A second question while one waits does not restart the wait.
        clock.set_waiting(true, at(10));
        clock.set_waiting(false, at(30));
        assert_eq!(clock.worked(at(37)), Duration::from_secs(11));
    }

    #[test]
    fn the_time_reads_in_seconds_then_as_a_clock() {
        assert_eq!(live_elapsed(Duration::from_millis(6_890), true), "6.8s");
        assert_eq!(live_elapsed(Duration::from_millis(6_890), false), "6s");
        assert_eq!(live_elapsed(Duration::from_millis(59_999), true), "59.9s");
        assert_eq!(live_elapsed(Duration::from_secs(64), true), "1:04");
    }
}
