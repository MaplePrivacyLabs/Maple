//! A turn's status line and independently disclosed activity chunks.
//!
//! A turn runs between user messages. Visible messages and notices split
//! its steps into chunks, without splitting its clock or animated status.

use std::collections::HashMap;
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
use crate::ui::motion::HoverBg as _;
use crate::ui::theme;
use crate::ui::{icons::icon, widgets};

/// Gap between two repaints of a running turn's time where loops hold
/// still. The time counts whole seconds, so one repaint a second is enough.
const RUN_TICK: Duration = Duration::from_secs(1);

/// One pass of the pulse along the mark's stroke, and of the shine across
/// the line's words. Only the words shine; the mark moves instead.
const MARK_CYCLE: Duration = Duration::from_millis(2000);
const SHINE_CYCLE: Duration = Duration::from_millis(1900);

/// A transcript row's place in its turn and activity chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TurnRow {
    Plain,
    /// The first chunk also carries the turn's status line.
    Lead,
    Chunk,
    Step(usize),
}

impl TurnRow {
    pub(super) fn lead(self, index: usize) -> Option<usize> {
        match self {
            Self::Plain => None,
            Self::Lead | Self::Chunk => Some(index),
            Self::Step(lead) => Some(lead),
        }
    }
}

/// Header data maintained on row changes, never derived during rendering.
#[derive(Default)]
pub(super) struct Chunk {
    end: usize,
    icons: Vec<(&'static str, usize)>,
    /// How many of the chunk's steps are in each state. A step waiting on
    /// the user, or refused, still counts by its call's own status.
    states: [usize; step::StepState::Denied as usize + 1],
}

impl Chunk {
    fn add_icon(&mut self, icon: &'static str) {
        if let Some((_, count)) = self.icons.iter_mut().find(|(name, _)| *name == icon) {
            *count += 1;
        } else {
            self.icons.push((icon, 1));
        }
    }

    fn add(&mut self, (icon, state): (&'static str, step::StepState)) {
        self.add_icon(icon);
        self.states[state as usize] += 1;
    }

    fn replace(
        &mut self,
        before: (&'static str, step::StepState),
        after: (&'static str, step::StepState),
    ) {
        self.states[before.1 as usize] -= 1;
        self.states[after.1 as usize] += 1;
        if before.0 != after.0 {
            for (name, count) in &mut self.icons {
                if *name == before.0 {
                    *count -= 1;
                }
            }
            self.icons.retain(|(_, count)| *count > 0);
            self.add_icon(after.0);
        }
    }

    pub(super) fn count(&self) -> usize {
        self.states.iter().sum()
    }

    pub(super) fn state(&self, live: bool) -> Option<step::StepState> {
        use step::StepState;
        [StepState::Failed, StepState::Running, StepState::Stopped]
            .into_iter()
            .find(|state| {
                self.states[*state as usize] > 0 && (*state != StepState::Running || live)
            })
    }
}

#[derive(Default)]
pub(super) struct Turns {
    rows: Vec<TurnRow>,
    chunks: HashMap<usize, Chunk>,
    /// First step of the latest user turn, kept stable for its clock.
    tail: Option<usize>,
    /// The reply that ends a turn, by its row, with the turn's lead: the
    /// last message from Maple after the turn's last step.
    replies: HashMap<usize, usize>,
    /// The reply of the turn at the end, while no step has followed it.
    tail_reply: Option<usize>,
    chunk: Option<usize>,
}

pub(super) fn starts_turn(item: &AgentTimelineItem) -> bool {
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

/// Classification changes can move both turn and chunk boundaries.
pub(super) fn part(item: &AgentTimelineItem) -> (bool, bool, bool) {
    let step = is_step(item);
    (
        starts_turn(item),
        step,
        !step && !is_tool(item) && super::navigation::timeline_item_is_navigable(item),
    )
}

pub(super) fn step_info(item: &AgentTimelineItem) -> (&'static str, step::StepState) {
    if is_tool(item) {
        if has_tool_input(item, "todos") {
            return ("check", step::StepState::Done);
        }
        return (
            step::tool_icon(item),
            step::StepState::of(item.status.as_deref()),
        );
    }
    let state = if item
        .text
        .as_deref()
        .is_none_or(|text| text.trim().is_empty())
    {
        step::StepState::Running
    } else {
        step::StepState::Done
    };
    ("brain", state)
}

impl Turns {
    /// Group a whole timeline: a snapshot, a history page, a trimmed window.
    pub(super) fn rebuild(&mut self, timeline: &[AgentTimelineItem]) {
        self.rows.clear();
        self.chunks.clear();
        self.chunk = None;
        self.tail = None;
        self.replies.clear();
        self.tail_reply = None;
        for item in timeline {
            self.push(item);
        }
    }

    /// One row arrived at the end.
    pub(super) fn push(&mut self, item: &AgentTimelineItem) {
        let index = self.rows.len();
        let (starts_turn, is_step, breaks_chunk) = part(item);
        let row = if starts_turn {
            self.tail = None;
            self.chunk = None;
            self.tail_reply = None;
            TurnRow::Plain
        } else if !is_step {
            // Hidden plans and empty streamed messages do not separate
            // the activities around them.
            if breaks_chunk {
                self.chunk = None;
            }
            // What Maple says after the turn's steps is its reply, until
            // it says more or takes another step. A message with nothing
            // in it yet draws no row, so it cannot carry the card.
            if let Some(lead) = self.tail
                && breaks_chunk
                && item.item_type == "message"
            {
                if let Some(earlier) = self.tail_reply.replace(index) {
                    self.replies.remove(&earlier);
                }
                self.replies.insert(index, lead);
            }
            TurnRow::Plain
        } else {
            if let Some(reply) = self.tail_reply.take() {
                self.replies.remove(&reply);
            }
            if let Some(lead) = self.chunk {
                TurnRow::Step(lead)
            } else {
                self.chunk = Some(index);
                if self.tail.is_none() {
                    self.tail = Some(index);
                    TurnRow::Lead
                } else {
                    TurnRow::Chunk
                }
            }
        };
        if let Some(lead) = row.lead(index) {
            let chunk = self.chunks.entry(lead).or_default();
            chunk.end = index + 1;
            chunk.add(step_info(item));
        }
        self.rows.push(row);
    }

    pub(super) fn row(&self, index: usize) -> TurnRow {
        self.rows.get(index).copied().unwrap_or(TurnRow::Plain)
    }

    pub(super) fn tail(&self) -> Option<usize> {
        self.tail
    }

    /// The lead of the turn the row at `row` is the reply of.
    pub(super) fn reply_of(&self, row: usize) -> Option<usize> {
        self.replies.get(&row).copied()
    }

    /// The reply of the turn at the end, if it has one yet.
    pub(super) fn tail_reply(&self) -> Option<usize> {
        self.tail_reply
    }

    /// The lead of the turn the step at `index` belongs to. A chunk's own
    /// lead is not its turn's unless it is the turn's first.
    pub(super) fn turn_of(&self, index: usize) -> Option<usize> {
        match self.tail {
            Some(tail) if index >= tail => Some(tail),
            _ => self
                .rows
                .get(..=index)?
                .iter()
                .rposition(|row| *row == TurnRow::Lead),
        }
    }

    pub(super) fn chunk(&self, lead: usize) -> Option<&Chunk> {
        self.chunks.get(&lead)
    }

    pub(super) fn end(&self, lead: usize) -> usize {
        self.chunk(lead).map_or(lead + 1, |chunk| chunk.end)
    }

    pub(super) fn update_step(
        &mut self,
        index: usize,
        before: (&'static str, step::StepState),
        after: (&'static str, step::StepState),
    ) -> Option<usize> {
        if before == after {
            return None;
        }
        let lead = self.row(index).lead(index)?;
        self.chunks.get_mut(&lead)?.replace(before, after);
        Some(lead)
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
    /// `None` once the view missed some of the run's rows: it is still
    /// the run it saw start, but it can no longer say a true time for it.
    clock: Option<RunClock>,
    /// The lead row of the turn the run is working on, once that turn has
    /// a step: the row its time is kept under when the turn ends.
    lead: Option<String>,
}

impl TimedRun {
    /// The view did not take in a row of this run. Where its turns began
    /// is lost, so it shows no time rather than a wrong one.
    pub(super) fn stop_timing(&mut self) {
        self.clock = None;
        self.lead = None;
    }
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
        /// Loops hold still: the mark rests.
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
    // Whole seconds, counted as the running time counts them, so the
    // line never says more when it stops than it showed while running.
    let seconds = worked.as_secs().max(1);
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

/// "6s" under a minute, then a clock like the subagent rows. A running
/// time counts whole seconds: tenths are too much movement on a line that
/// is on screen for the whole turn.
pub(super) fn live_elapsed(worked: Duration) -> String {
    if worked.as_secs() >= 60 {
        format_subagent_elapsed(worked)
    } else {
        format!("{}s", worked.as_secs())
    }
}

#[derive(Clone, Copy)]
enum Mark {
    /// The blocks along the M's stroke shrink and grow, one after another.
    Working,
    /// Nothing is running: the mark rests in the running tone.
    Resting,
    /// Maple waits on the user: the mark rests in coral.
    Waiting,
}

/// The Maple mark on a 5 by 5 grid: a block with the notch of the M cut
/// from its top edge. `phase` is how far a working mark's pulse is along
/// the stroke.
fn maple_mark(mark: Mark, phase: f32) -> Div {
    /// The M as it is drawn, by (row, column): up the left side, through
    /// the notch, and down the right side.
    const STROKE: [(usize, usize); 13] = [
        (4, 0),
        (3, 0),
        (2, 0),
        (1, 0),
        (0, 0),
        (1, 1),
        (2, 2),
        (1, 3),
        (0, 4),
        (1, 4),
        (2, 4),
        (3, 4),
        (4, 4),
    ];
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
            // The box keeps its size so a shrinking block moves nothing.
            let cell = div().size(CELL).flex().items_center().justify_center();
            let block = div().rounded(px(0.5));
            line = line.child(if !on {
                cell
            } else {
                match mark {
                    Mark::Working => {
                        // The pulse runs the stroke, starting and ending
                        // clear of it so every block is whole while it
                        // rests for the last sixth of the pass. Blocks
                        // off the stroke hold still.
                        let head = -1.5 + (phase / 0.84).min(1.) * (STROKE.len() as f32 + 2.);
                        let level =
                            STROKE
                                .iter()
                                .position(|&at| at == (row, column))
                                .map_or(0., |index| {
                                    motion::smoothstep(
                                        (1. - (index as f32 - head).abs() / 1.5).max(0.),
                                    )
                                });
                        cell.child(
                            block
                                .size(CELL * (1. - 0.6 * level))
                                .bg(resting)
                                .opacity(0.9),
                        )
                    }
                    Mark::Resting => cell.child(block.size(CELL).bg(resting).opacity(0.9)),
                    Mark::Waiting => cell.child(block.size(CELL).bg(coral)),
                }
            });
        }
        grid = grid.child(line);
    }
    grid
}

/// The summary keeps the same height and identity when it takes over the
/// waiting slot. Chunk disclosures do not replace or restart its motion.
pub(super) fn render_line(line: Line, key: &str, messages: &MessageCache) -> Div {
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
            let time = worked.filter(|_| phase != Phase::Waiting).map(live_elapsed);
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
    div()
        .h(WAITING_ROW_HEIGHT)
        .flex()
        .items_center()
        .child(button.role(gpui::Role::Status))
}

pub(super) fn render_chunk(
    lead: &str,
    chunk: &Chunk,
    open: bool,
    live: bool,
    // One of the chunk's calls waits for the user to allow it. Folded, the
    // step that says so is hidden, so the header says it.
    waiting: bool,
    chat: &gpui::WeakEntity<ChatScreen>,
    messages: &MessageCache,
) -> Div {
    let label = messages.get("chat-activity-steps", [("count", chunk.count().into())]);
    let state = if waiting {
        Some(step::StepState::Waiting)
    } else {
        chunk.state(live)
    };
    let status = state.map(|state| {
        let (key, ink) = match state {
            step::StepState::Failed => ("chat-status-failed", theme::status_error()),
            step::StepState::Stopped => ("chat-status-stopped", theme::text_muted()),
            step::StepState::Waiting => ("chat-status-waiting", theme::accent_ink()),
            _ => ("chat-status-running", theme::text_muted()),
        };
        let id = SharedString::from(format!("activity-status-{lead}"));
        div()
            .id(id.clone())
            .debug_selector(move || id.to_string())
            .role(gpui::Role::Status)
            .aria_label(tr(key))
            .text_xs()
            .text_color(gpui::rgb(ink))
            .child(gpui::text!(tr(key)))
    });
    let debug = format!("activity-chunk-{lead}");
    let target = chat.clone();
    let key = lead.to_owned();
    div()
        .flex()
        .when(!open, |row| row.pb_2())
        .items_start()
        .child(
            div()
                .id(SharedString::from(format!("activity-toggle-{lead}")))
                .debug_selector(move || debug.clone())
                .role(gpui::Role::DisclosureTriangle)
                .aria_label(label.clone())
                .aria_expanded(open)
                .tab_index(0)
                .on_key_down(ChatScreen::sidebar_button_key_down)
                .focus_visible(widgets::focus_ring)
                .min_h(px(32.))
                .max_w_full()
                .px_2()
                .ml(px(-8.))
                .rounded(theme::RADIUS_SM)
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2p5()
                .text_sm()
                .text_color(gpui::rgb(theme::text_secondary()))
                .hover_bg(theme::overlay_hover())
                .hover(|style| {
                    style
                        .cursor_pointer()
                        .text_color(gpui::rgb(theme::text_primary()))
                })
                .child(
                    div()
                        .w(step::NODE)
                        .flex()
                        .justify_center()
                        .child(motion::chevron(
                            SharedString::from(format!("activity-chevron-{lead}")),
                            open,
                            px(14.),
                            theme::text_muted(),
                        )),
                )
                .child(gpui::text!(label))
                .child(
                    div().flex().gap_1p5().children(
                        chunk
                            .icons
                            .iter()
                            .map(|(name, _)| icon(name, px(12.), theme::text_muted())),
                    ),
                )
                .children(status)
                .on_click(move |_, _, cx| {
                    target
                        .update(cx, |chat, cx| chat.toggle_chunk(&key, cx))
                        .ok();
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
    /// earlier one, and the line waits in the slot instead. A window that
    /// is paged back in history, or still lacks the run's newest rows,
    /// does not hold the turn the run works on at all.
    pub(super) fn turn_runs(&self, lead: usize) -> bool {
        self.turns.tail().is_some_and(|turn| lead >= turn)
            && self.is_run_active()
            && !self.awaiting_first_token
            && !self.pages.after
            && !self.pages.live_has_more
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
                .and_then(|timed| timed.clock.as_ref())
                .map(|clock| clock.worked(now)),
            still,
        }
    }

    /// The existing details preference is the default; a click changes
    /// only this chunk, including while more steps arrive in it.
    pub(super) fn chunk_open(&self, lead: usize) -> bool {
        self.timeline
            .get(lead)
            .is_some_and(|item| self.tool_details != self.toggled_chunks.contains(&item.id))
    }

    /// Show or fold one chunk and re-measure only its rows.
    pub(super) fn toggle_chunk(&mut self, lead_id: &str, cx: &mut Context<Self>) {
        let Some(&(lead, _)) = self.timeline_index.get(lead_id) else {
            return;
        };
        if !self.toggled_chunks.remove(lead_id) {
            self.toggled_chunks.insert(lead_id.to_owned());
        }
        let open = self.chunk_open(lead);
        self.row_motion.disclose_chunk(lead_id, open, motion::now());
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
                clock: Some(RunClock::new(motion::now())),
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
        let Some(clock) = timed.clock.as_mut() else {
            return;
        };
        if self.turns.row(index) == TurnRow::Lead {
            timed.lead = Some(item.id.clone());
        } else if starts_turn(item) {
            let now = motion::now();
            if let Some(lead) = timed.lead.take() {
                self.turn_worked.insert(lead, clock.worked(now));
            }
            clock.restart(now);
        }
    }

    /// Keep the clocks in step with the runs and the questions, wherever
    /// either changed. A run that left `active_runs` stops its clock, and
    /// the turn it was working on keeps how long it worked. A run that
    /// waits on the user holds its time.
    pub(super) fn sync_run_clocks(&mut self) {
        // Once its turn is over, a reply may carry the card of what the
        // turn changed: that row is taller from then on, so measure it
        // again the first time it is seen settled.
        let running = self.turns.tail().is_some_and(|lead| self.turn_runs(lead));
        let settled = self.turns.tail_reply().filter(|_| !running);
        if std::mem::replace(&mut self.settled_reply, settled) != settled
            && let Some(reply) = settled
        {
            self.list_state.remeasure_items(reply..reply + 1);
        }
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
                && let Some(clock) = timed.clock
                && let Some(lead) = timed
                    .lead
                    .filter(|lead| self.timeline_index.contains_key(lead))
            {
                self.turn_worked.insert(lead, clock.worked(now));
            }
        }
        let questions = &self.pending_questions;
        for (session, timed) in &mut self.run_clocks {
            let waiting = questions
                .iter()
                .any(|question| &question.session_id == session);
            if let Some(clock) = timed.clock.as_mut() {
                clock.set_waiting(waiting, now);
            }
        }
    }

    /// A live row took its persisted id; what was kept under the old one
    /// follows it.
    pub(super) fn rekey_turn(&mut self, old: &str, new: &str) {
        if self.toggled_chunks.remove(old) {
            self.toggled_chunks.insert(new.to_owned());
        }
        if let Some(worked) = self.turn_worked.remove(old) {
            self.turn_worked.insert(new.to_owned(), worked);
        }
        for timed in self.run_clocks.values_mut() {
            if timed.lead.as_deref() == Some(old) {
                timed.lead = Some(new.to_owned());
            }
        }
        if let Some(allowed) = self.step_permissions.remove(old) {
            self.step_permissions.insert(new.to_owned(), allowed);
        }
        self.turn_changes.forget(old);
        if let Some(open) = self.open_changes.remove(old) {
            self.open_changes.insert(new.to_owned(), open);
        }
    }

    /// The permission request on screen, if the question card shows one.
    pub(super) fn current_permission(&self) -> Option<&maple_agent::agent::AgentPermissionPrompt> {
        self.current_question()?
            .questions
            .iter()
            .find_map(|entry| entry.permission.as_ref())
    }

    /// The tool call the permission request on screen is about. The
    /// request names its tool and what the call would do, not the call,
    /// and every call of a reply is on screen as running before any is
    /// asked about. So the call is the turn's one unanswered call with
    /// that tool and that action. Where two could be meant, none is
    /// marked: a wrong step would keep an answer that was not its own.
    pub(super) fn waiting_step(&self) -> Option<usize> {
        let prompt = self.current_permission()?;
        let mut meant = self
            .timeline
            .iter()
            .enumerate()
            .rev()
            .take_while(|(_, item)| !starts_turn(item))
            .filter(|(_, item)| {
                is_tool(item)
                    && step::StepState::of(item.status.as_deref()) == step::StepState::Running
                    && !self.step_permissions.contains_key(&item.id)
                    && item.label.as_ref().and_then(|label| label.name.as_deref())
                        == Some(prompt.tool.as_str())
                    // An input with no command, path, url or query has
                    // nothing to compare; the tool alone decides.
                    && call_action(item).is_none_or(|action| same_action(&prompt.action, action))
            });
        let (index, _) = meant.next()?;
        meant.next().is_none().then_some(index)
    }

    /// The user answered a permission request: the call it was about keeps
    /// what they said for as long as this view shows the task.
    pub(super) fn settle_permission(&mut self, question: &crate::backend::PendingQuestion) {
        let Some(asked) = question
            .questions
            .iter()
            .position(|entry| entry.permission.is_some())
        else {
            return;
        };
        let Some(call) = self.waiting_step().and_then(|step| self.timeline.get(step)) else {
            return;
        };
        // Only the first option, "Allow once", lets the call run.
        let allowed = self
            .question_selected
            .get(&asked)
            .is_some_and(|selection| selection.picked.contains(&0));
        self.step_permissions.insert(call.id.clone(), allowed);
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

/// What a call would do, as a permission request words it: the first of
/// these fields its input has.
fn call_action(item: &AgentTimelineItem) -> Option<&str> {
    let input = item.input.as_ref()?;
    ["command", "path", "url", "query"]
        .into_iter()
        .find_map(|field| input.get(field)?.as_str())
}

/// Whether the action a request shows is the call's. The request cuts the
/// middle out of a long one and says so on a line of its own.
fn same_action(shown: &str, full: &str) -> bool {
    shown == full
        || shown.split_once("\n[").is_some_and(|(start, _)| {
            !start.is_empty() && full.len() > shown.len() && full.starts_with(start)
        })
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
    fn messages_split_chunks_without_splitting_the_turn() {
        let timeline = [
            item("message", Some("user")),
            item("thinking", None),
            AgentTimelineItem {
                text: Some("Checking the remaining files.".into()),
                ..item("message", Some("assistant"))
            },
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
                TurnRow::Chunk,
                TurnRow::Plain,
                TurnRow::Plain,
                TurnRow::Plain,
                TurnRow::Lead,
            ]
        );
        assert_eq!(turns.end(1), 2);
        assert_eq!(turns.end(3), 4);
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
        assert_eq!(live.replies, snapshot.replies);
        // A window that starts mid-turn leads with its first loaded step.
        assert_eq!(snapshot.row(0), TurnRow::Lead);
        // A new message from the user starts a turn with no line yet.
        live.push(&item("message", Some("user")));
        assert_eq!(live.tail(), None);
    }

    #[test]
    fn a_later_chunk_belongs_to_its_turns_first_step() {
        let said = || AgentTimelineItem {
            text: Some("Checking the result.".into()),
            ..item("message", Some("assistant"))
        };
        let mut turns = Turns::default();
        turns.rebuild(&[
            item("message", Some("user")),
            item("tool", None),
            said(),
            item("tool", None),
            item("message", Some("user")),
            item("tool", None),
            said(),
            item("tool", None),
        ]);
        assert_eq!(turns.row(3), TurnRow::Chunk);
        assert_eq!(turns.turn_of(3), Some(1));
        assert_eq!(turns.turn_of(7), Some(5));
        // The reply after a later chunk still ends the whole turn.
        turns.push(&said());
        assert_eq!(turns.reply_of(8), Some(5));
    }

    #[test]
    fn a_turns_reply_is_what_maple_says_after_its_last_step() {
        let said = || AgentTimelineItem {
            text: Some("Done.".into()),
            ..item("message", Some("assistant"))
        };
        let mut turns = Turns::default();
        turns.rebuild(&[
            item("message", Some("user")),
            // Said before any step: not a reply to carry what changed.
            said(),
            item("tool", None),
            said(),
        ]);
        assert_eq!(turns.reply_of(1), None);
        assert_eq!(turns.reply_of(3), Some(2));
        // Another step: the turn is not over, and what was said is no
        // longer its last word.
        turns.push(&item("tool", None));
        assert_eq!(turns.reply_of(3), None);
        turns.push(&said());
        assert_eq!(turns.reply_of(5), Some(2));
        assert_eq!(turns.tail_reply(), Some(5));
        // A plain turn has no steps, so nothing to list.
        turns.push(&item("message", Some("user")));
        turns.push(&said());
        assert_eq!(turns.reply_of(7), None);
        // A message that is still empty draws nothing, so the reply before
        // it keeps the card.
        let mut turns = Turns::default();
        turns.rebuild(&[
            item("tool", None),
            said(),
            item("message", Some("assistant")),
        ]);
        assert_eq!(turns.reply_of(1), Some(0));
        assert_eq!(turns.tail_reply(), Some(1));
    }

    #[test]
    fn hidden_plans_do_not_split_activity_and_status_updates_stay_in_their_chunk() {
        let done = AgentTimelineItem {
            status: Some("completed".into()),
            ..item("tool", None)
        };
        let plan = AgentTimelineItem {
            input: Some(serde_json::json!({"todos": [{"text": "Read", "status": "pending"}]})),
            ..item("tool", None)
        };
        let mut turns = Turns::default();
        turns.rebuild(&[done.clone(), plan, done.clone()]);
        assert_eq!(turns.row(2), TurnRow::Step(0));
        let chunk = turns.chunk(0).unwrap();
        assert_eq!(chunk.count(), 2);
        assert_eq!(chunk.state(true), None);
        let failed = AgentTimelineItem {
            status: Some("failed".into()),
            ..done.clone()
        };
        assert_eq!(
            turns.update_step(2, step_info(&done), step_info(&failed)),
            Some(0)
        );
        assert_eq!(
            turns.chunk(0).unwrap().state(false),
            Some(step::StepState::Failed)
        );
        turns.update_step(2, step_info(&failed), step_info(&done));
        assert_eq!(turns.chunk(0).unwrap().state(false), None);
        assert_eq!(turns.chunk(0).unwrap().count(), 2);
        turns.push(&AgentTimelineItem {
            input: Some(serde_json::json!({"todos": [{"text": "Read", "status": "completed"}]})),
            ..item("tool", None)
        });
        assert_eq!(turns.chunk(0).unwrap().count(), 3);
        assert!(
            turns
                .chunk(0)
                .unwrap()
                .icons
                .iter()
                .any(|(icon, _)| *icon == "check")
        );
    }

    #[test]
    fn empty_messages_and_internal_rows_do_not_split_activity_chunks() {
        let mut timeline = [
            item("tool", None),
            AgentTimelineItem {
                text: Some("\n\n".into()),
                ..item("message", Some("assistant"))
            },
            item("internal", None),
            item("tool", None),
        ];
        let mut turns = Turns::default();
        turns.rebuild(&timeline);
        assert_eq!(turns.row(3), TurnRow::Step(0));
        assert_eq!(turns.chunk(0).unwrap().count(), 2);
        let before = part(&timeline[1]);
        timeline[1].text = Some("Now checking the result.".into());
        assert_ne!(before, part(&timeline[1]));
        turns.rebuild(&timeline);
        assert_eq!(turns.row(3), TurnRow::Chunk);
        assert_eq!(turns.chunk(0).unwrap().count(), 1);
        assert_eq!(turns.tail(), Some(0));
    }

    #[test]
    fn finished_thinking_clears_its_chunk_status_during_a_live_turn() {
        let thinking = item("thinking", None);
        let mut turns = Turns::default();
        turns.push(&thinking);
        assert_eq!(
            turns.chunk(0).unwrap().state(true),
            Some(step::StepState::Running)
        );
        let done = AgentTimelineItem {
            text: Some("The next action is clear.".into()),
            ..thinking.clone()
        };
        turns.update_step(0, step_info(&thinking), step_info(&done));
        assert_eq!(turns.chunk(0).unwrap().state(true), None);
        turns.push(&AgentTimelineItem {
            text: Some("Applying the change.".into()),
            ..item("message", Some("assistant"))
        });
        turns.push(&item("tool", None));
        assert_eq!(turns.chunk(0).unwrap().state(true), None);
        assert_eq!(
            turns.chunk(2).unwrap().state(true),
            Some(step::StepState::Running)
        );
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
    fn a_running_time_counts_whole_seconds_then_reads_as_a_clock() {
        assert_eq!(live_elapsed(Duration::from_millis(890)), "0s");
        assert_eq!(live_elapsed(Duration::from_millis(6_890)), "6s");
        assert_eq!(live_elapsed(Duration::from_millis(59_999)), "59s");
        assert_eq!(live_elapsed(Duration::from_secs(64)), "1:04");
    }
}
