//! Controls that float over the transcript, and the question card at its
//! tail.
//!
//! At the top edge, a date chip says when the messages in view were sent
//! while the reader scrolls back, and that older history is loading. At
//! the bottom edge, the jump button takes them back to the latest message.
//! When something happens below while they read, it opens into a chip that
//! says what, then settles back to a round button that still shows it: the
//! breathing dot while Maple works, a count of replies, the reply bubble
//! while Maple waits on them, the alert after a failed run.
//!
//! A question or permission request is the thread's last item, so it is in
//! view only at the bottom; the jump button is the way to it. Each floating
//! control is one element that eases in and out on [`motion::FLOAT`] and
//! keeps drawing while it leaves, so turning back mid-way carries on from
//! where it is drawn.

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Context, Div, FontWeight, Hsla, IntoElement, Pixels, SharedString, Window, div,
    prelude::*, px, rems,
};

use super::ChatScreen;
use super::transcript::{Grow, grow_in, render_question_card};
use crate::backend::PendingQuestion;
use crate::ui::i18n::tr;
use crate::ui::icons::{icon, spinner_with_id};
use crate::ui::morph_text::morph_text;
use crate::ui::motion::{self, HoverBg as _, Tween};
use crate::ui::theme;

/// The round button's side, border included.
const BUTTON: Pixels = px(34.);
/// The count badge's height, and its smallest width.
const BADGE: Pixels = px(17.);
/// How far the badge sits outside the round button's top right corner.
const BADGE_OUT: Pixels = px(5.);
/// Inset of the open chip's lead (the badge, or a mark) from its left end:
/// the border plus padding concentric with the pill's round end.
const LEAD_COUNTED: Pixels = px(8.);
const LEAD_MARK: Pixels = px(10.);
const MARK: Pixels = px(13.);
const GAP: Pixels = px(6.);
const PAD_END: Pixels = px(13.);
/// How long the chip stays open on its own: passive news, then news that
/// waits on the reader or went wrong, a beat longer to read.
const HOLD: Duration = Duration::from_millis(2600);
const HOLD_LOUD: Duration = Duration::from_millis(3400);
/// A pointer passing over the button does not open it...
const HOVER_INTENT: Duration = Duration::from_millis(60);
/// ...and slipping off its edge does not snap it shut.
const HOVER_GRACE: Duration = Duration::from_millis(220);
/// How long the date chip stays after the reader stops scrolling back, and
/// after they start reading down again.
const DATE_LINGER: Duration = Duration::from_millis(1200);
const DATE_LEAVE: Duration = Duration::from_millis(400);
/// The count brightens to Cloud when it changes, then eases back.
const TICK: Duration = Duration::from_millis(520);
/// One soft halo when the button turns amber or red.
const PING: Duration = Duration::from_millis(600);
/// The question card brightens its edge when the button lands on it.
const FLASH: Duration = Duration::from_millis(600);
/// An answered card closes its space, and its content fades faster.
const CARD_CLOSE: Duration = Duration::from_millis(220);
const CARD_FADE: Duration = Duration::from_millis(140);
/// A jump glides at most this share of the viewport: a longer one lands
/// that far short of the end at once, so a jump from the start of a long
/// task costs no more than a short one and draws no rows on the way.
const GLIDE_REACH: f32 = 0.6;
/// The list row the card takes, keyed apart from timeline ids.
pub(super) const CARD_ROW: &str = "question-card-row";

/// What the jump button says, ranked like the sidebar's `TaskActivity::of`:
/// what waits on the reader, then a run in progress, then how the last run
/// ended, then replies they have not seen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Jump {
    Latest,
    Working,
    /// Replies that arrived below; `running` while Maple still works.
    New {
        count: usize,
        running: bool,
    },
    Question,
    Permission,
    Failed,
}

impl Jump {
    pub(super) fn of(ask: Option<bool>, running: bool, failed: bool, unseen: usize) -> Self {
        match ask {
            Some(true) => return Self::Permission,
            Some(false) => return Self::Question,
            None => {}
        }
        if running {
            return if unseen > 0 {
                Self::New {
                    count: unseen,
                    running: true,
                }
            } else {
                Self::Working
            };
        }
        if failed {
            Self::Failed
        } else if unseen > 0 {
            Self::New {
                count: unseen,
                running: false,
            }
        } else {
            Self::Latest
        }
    }

    /// Waits on the reader, or went wrong: these keep the chip open longer.
    fn loud(self) -> bool {
        matches!(self, Self::Question | Self::Permission | Self::Failed)
    }

    fn count(self) -> Option<usize> {
        match self {
            Self::New { count, .. } => Some(count),
            _ => None,
        }
    }

    /// The same news in other words: a count that changed, or a run that
    /// ended with the replies still unread, is not news.
    fn same_news(self, other: Self) -> bool {
        std::mem::discriminant(&self) == std::mem::discriminant(&other)
    }
}

/// When the chip opens on its own. New replies announce once: after the
/// chip has said "new messages" and settled, more replies only tick its
/// count, until the reader has read to the end. "Maple is working" is said
/// once per run, however often the reader scrolls up during it. What waits
/// on the reader or went wrong announces every time it turns up.
#[derive(Debug, Default)]
pub(super) struct Announcer {
    announced: bool,
    last: Option<Jump>,
    /// The run "Maple is working" was said for.
    working_run: Option<String>,
}

impl Announcer {
    /// Whether `jump` opens the chip; `run` is the run in progress.
    pub(super) fn update(&mut self, jump: Jump, run: Option<&str>) -> bool {
        let previous = self.last.replace(jump);
        let announce = match jump {
            Jump::Latest => false,
            Jump::New { .. } => !std::mem::replace(&mut self.announced, true),
            // Latched on the first sight, even when the chip is already
            // open under the pointer.
            Jump::Working => {
                let said = run.is_some() && self.working_run.as_deref() == run;
                self.working_run = run.map(str::to_owned);
                !said
            }
            _ => previous.is_none_or(|previous| !previous.same_news(jump)),
        };
        // Nothing unread is left to have announced.
        if matches!(jump, Jump::Latest | Jump::Working) {
            self.announced = false;
        }
        announce
    }

    /// The reader caught up, or the button left: what is unread starts
    /// over, but the run in progress has still been announced.
    pub(super) fn reset(&mut self) {
        *self = Self {
            working_run: self.working_run.take(),
            ..Self::default()
        };
    }
}

/// A glide to the end of the thread.
#[derive(Clone, Copy, Debug)]
pub(super) struct Glide {
    from: Pixels,
    started: Instant,
    duration: Duration,
    /// Lands on the question card: flash it and focus its answer.
    to_card: bool,
    /// Where it last put the list. Anything else moving the list (keys,
    /// the scrollbar) takes over from it.
    at: Pixels,
}

/// The question card's row at the thread's tail.
#[derive(Default)]
pub(super) struct TailCard {
    /// The list holds the card's row, one past the last timeline row.
    pub(super) in_list: bool,
    /// The request on screen, and a copy that draws it while it closes.
    request: Option<String>,
    last: Option<PendingQuestion>,
    step: usize,
    /// It arrived at the followed tail and grows into place.
    arrived: Option<Instant>,
    /// Answered at the tail: it closes its space.
    closing: Option<(PendingQuestion, usize, Instant)>,
    /// Its natural height, measured as it paints; the transcript's own
    /// row heights are pruned as their entrances end.
    heights: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<String, Pixels>>>,
}

/// When `created_ms` was, on this computer's clock.
fn local_time(created_ms: u128) -> chrono::DateTime<chrono::Local> {
    chrono::DateTime::<chrono::Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(created_ms as u64),
    )
}

/// How the date chip and the thread's start name a day.
#[derive(Debug, PartialEq, Eq)]
enum DayName {
    Today,
    Yesterday,
    Date { month: u32, day: u32 },
}

impl DayName {
    fn of(sent: chrono::NaiveDate, today: chrono::NaiveDate) -> Self {
        if sent == today {
            Self::Today
        } else if today.pred_opt() == Some(sent) {
            Self::Yesterday
        } else {
            Self::Date {
                month: chrono::Datelike::month(&sent),
                day: chrono::Datelike::day(&sent),
            }
        }
    }
}

/// What the jump button says, kept while its news and the language stay.
#[derive(Clone)]
struct JumpText {
    jump: Jump,
    locale: fluent_templates::LanguageIdentifier,
    words: SharedString,
    label: SharedString,
    count: SharedString,
}

/// Everything the floating controls remember between frames.
pub(super) struct Floating {
    /// Replies that arrived while the reader was scrolled up.
    pub(super) unseen: usize,
    /// The last run failed while they were scrolled up.
    pub(super) failed: bool,
    /// A key answered the question while it was out of view: glide to it
    /// on the next frame instead.
    reveal_card: bool,
    announcer: Announcer,
    jump: Option<Jump>,
    open: bool,
    /// The pointer is over the button, or the keyboard is on it: either
    /// holds the chip open.
    hovered: bool,
    focused: bool,
    focus: Option<gpui::FocusHandle>,
    /// The open chip's measured words and badge, kept while they read the
    /// same: (words, count, rem size) and their widths.
    widths: Option<(SharedString, SharedString, Pixels, Pixels, Pixels)>,
    text: Option<JumpText>,
    close_task: Option<gpui::Task<()>>,
    hover_task: Option<gpui::Task<()>>,
    shown: Tween,
    opened: Tween,
    width: Tween,
    badge_in_chip: Tween,
    badge_shown: Tween,
    you: Tween,
    fail: Tween,
    tick: Option<Instant>,
    ping: Option<Instant>,
    pub(super) glide: Option<Glide>,
    pub(super) card_flash: Option<Instant>,
    pub(super) card: TailCard,
    date_until: Option<Instant>,
    date_task: Option<gpui::Task<()>>,
    date_shown: Tween,
    last_top: Option<Pixels>,
    /// The list scrolled since the last render.
    scrolled: bool,
    /// The row count older history was last asked for at, while the
    /// reader is at the top: one request per visit, so a failed load is
    /// not retried every frame.
    pub(super) top_load: Option<usize>,
}

impl Default for Floating {
    fn default() -> Self {
        Self {
            unseen: 0,
            failed: false,
            reveal_card: false,
            announcer: Announcer::default(),
            jump: None,
            open: false,
            hovered: false,
            focused: false,
            focus: None,
            widths: None,
            text: None,
            close_task: None,
            hover_task: None,
            shown: Tween::new(0.),
            opened: Tween::new(0.),
            width: Tween::pixels(BUTTON.as_f32()),
            badge_in_chip: Tween::new(0.),
            badge_shown: Tween::new(0.),
            you: Tween::new(0.),
            fail: Tween::new(0.),
            tick: None,
            ping: None,
            glide: None,
            card_flash: None,
            card: TailCard::default(),
            date_until: None,
            date_task: None,
            date_shown: Tween::new(0.),
            last_top: None,
            scrolled: false,
            top_load: None,
        }
    }
}

impl Floating {
    /// Another task opened, or a new one: nothing shown carries over, and
    /// dropping the old timers cancels them.
    pub(super) fn reset(&mut self) {
        let card_in_list = self.card.in_list;
        let focus = self.focus.take();
        *self = Self::default();
        // The list still holds the row until the next render removes it.
        self.card.in_list = card_in_list;
        self.focus = focus;
    }

    fn held(&self) -> bool {
        self.hovered || self.focused
    }

    /// A row arrived live while the reader was scrolled up: replies count.
    pub(super) fn note_arrival(&mut self, item_type: &str, role: Option<&str>) {
        if item_type == "message" && role != Some("user") {
            self.unseen += 1;
        }
    }

    /// The selected task's run ended. Only the latest outcome shows: a
    /// failure while the reader is up the thread, cleared by any later end.
    pub(super) fn run_finished(&mut self, failed: bool, following: bool) {
        self.failed = failed && !following;
    }

    /// The jump button is open into a chip.
    #[cfg(test)]
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// The jump button's focus, once it has drawn.
    #[cfg(test)]
    pub(super) fn focus(&self) -> Option<gpui::FocusHandle> {
        self.focus.clone()
    }

    /// Rows arrived or left above the view: the next scroll reads its
    /// direction afresh rather than against a top that has moved.
    pub(super) fn forget_scroll(&mut self) {
        self.last_top = None;
    }

    /// The reader is back at the end: everything below has been seen.
    fn caught_up(&mut self) {
        self.unseen = 0;
        self.failed = false;
        self.announcer.reset();
    }
}

/// Width of `text` in the window's font at `size` and `weight`.
fn text_width(window: &Window, text: &SharedString, size: Pixels, weight: FontWeight) -> Pixels {
    let mut style = window.text_style();
    style.font_size = size.into();
    style.font_weight = weight;
    let run = style.to_run(text.len());
    window
        .text_system()
        .shape_line(text.clone(), size, &[run], None)
        .width
}

fn hsla(color: u32) -> Hsla {
    gpui::rgb(color).into()
}

/// Progress through `duration` since `at`, or `None` once it has passed.
fn since(at: Option<Instant>, duration: Duration, now: Instant) -> Option<f32> {
    let elapsed = now.saturating_duration_since(at?);
    (elapsed < duration).then(|| elapsed.as_secs_f32() / duration.as_secs_f32())
}

/// One state's mark, cross-fading with the others in one `size` box.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Arrow,
    Running,
    You,
    Fail,
}

/// The element ids of one mark's layers: arrow, running, you, fail, and
/// the running dot's loop.
type MarkIds = [&'static str; 5];
const COMPACT_IDS: MarkIds = [
    "jump-compact-arrow",
    "jump-compact-running",
    "jump-compact-you",
    "jump-compact-fail",
    "jump-compact-breath",
];
const LEAD_IDS: MarkIds = [
    "jump-lead-arrow",
    "jump-lead-running",
    "jump-lead-you",
    "jump-lead-fail",
    "jump-lead-breath",
];

fn mark(ids: MarkIds, state: Mark, size: Pixels) -> Div {
    let layer = |id: &'static str, this: Mark| {
        let glyph = move || -> AnyElement {
            match this {
                Mark::Arrow => icon("arrow-down", size, theme::text_secondary()).into_any_element(),
                Mark::You => {
                    icon("message-square", size, theme::status_warning()).into_any_element()
                }
                Mark::Fail => icon("circle-alert", size, theme::status_error()).into_any_element(),
                Mark::Running => motion::ticker(
                    SharedString::new_static(ids[4]),
                    motion::RUNNING_BREATH,
                    |phase| {
                        let grey = gpui::rgb(theme::sidebar_running());
                        // Phase 0 is the full dot, so a paused loop rests on it.
                        let breath = motion::pulse(phase, 0.25, 0., 1.);
                        div()
                            .size(px(8.))
                            .rounded_full()
                            .bg(grey.alpha(0.5 + 0.5 * breath))
                            .shadow(vec![
                                gpui::BoxShadow::new(
                                    px(0.),
                                    px(0.),
                                    grey.alpha(0.3 * breath).into(),
                                )
                                .blur_radius(px(4.)),
                            ])
                            .into_any_element()
                    },
                )
                .into_any_element(),
            }
        };
        motion::animated(
            SharedString::new_static(id),
            if state == this { 1. } else { 0. },
            motion::TOGGLE,
            move |shown| {
                if shown <= 0. {
                    return div().into_any_element();
                }
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .opacity(shown)
                    .child(glyph())
                    .into_any_element()
            },
        )
    };
    div()
        .relative()
        .flex_none()
        .size(size)
        .child(layer(ids[0], Mark::Arrow))
        .child(layer(ids[1], Mark::Running))
        .child(layer(ids[2], Mark::You))
        .child(layer(ids[3], Mark::Fail))
}

impl ChatScreen {
    /// What the jump button says now.
    pub(super) fn jump_state(&self) -> Jump {
        let ask = self.current_question().map(|question| {
            question
                .questions
                .get(
                    self.question_step
                        .min(question.questions.len().saturating_sub(1)),
                )
                .is_some_and(|entry| entry.permission.is_some())
        });
        Jump::of(
            ask,
            self.is_run_active(),
            self.floating.failed,
            self.floating.unseen,
        )
    }

    /// "Start of task" with the day the task's first message was sent.
    /// Built once per row and day, like the hover timestamp.
    pub(super) fn start_label(&self, created_ms: u128) -> SharedString {
        let today = chrono::Local::now().date_naive();
        self.messages.computed(
            "chat-history-start",
            &format!("{created_ms}:{today}"),
            || {
                let day = self.day_label(created_ms);
                self.messages
                    .get("chat-history-start", [("date", day.to_string().into())])
            },
        )
    }

    /// The date chip's words for a row: its day and time, as in
    /// `Today · 14:05` or `Nov 21 · 09:30`. Built once per row and day.
    pub(super) fn date_label(&self, created_ms: u128) -> SharedString {
        let today = chrono::Local::now().date_naive();
        self.messages
            .computed("chat-date", &format!("{created_ms}:{today}"), || {
                let sent = local_time(created_ms);
                self.messages.get(
                    "chat-date",
                    [
                        ("day", self.day_label(created_ms).to_string().into()),
                        ("time", sent.format("%H:%M").to_string().into()),
                    ],
                )
            })
    }

    /// The day a row was sent: Today, Yesterday or the month and day.
    fn day_label(&self, created_ms: u128) -> SharedString {
        let sent = local_time(created_ms).date_naive();
        let today = chrono::Local::now().date_naive();
        match DayName::of(sent, today) {
            DayName::Today => tr("chat-day-today"),
            DayName::Yesterday => tr("chat-day-yesterday"),
            DayName::Date { month, day } => self.messages.get(
                "chat-day",
                [
                    (
                        "month",
                        tr(super::transcript::month_key(month)).to_string().into(),
                    ),
                    ("day", day.into()),
                ],
            ),
        }
    }

    /// The transcript scrolled. The list reports this from inside its own
    /// state, which cannot be read there, so the direction is read on the
    /// next render (`read_scroll`).
    pub(super) fn note_transcript_scroll(&mut self, cx: &mut Context<Self>) {
        self.floating.scrolled = true;
        cx.notify();
    }

    /// Scrolling back brings the date chip; reading down again sends it off
    /// soon after.
    pub(super) fn read_scroll(&mut self, cx: &mut Context<Self>) {
        // Read every frame, so rows that load above or arrive below move
        // the baseline without reading as the reader's scroll.
        let top = -self.list_state.scroll_px_offset_for_scrollbar().y;
        let last = self.floating.last_top.replace(top);
        if !std::mem::take(&mut self.floating.scrolled) {
            return;
        }
        let Some(last) = last else {
            return;
        };
        let now = motion::now();
        if top < last - px(2.) && !self.list_state.is_following_tail() {
            self.floating.date_until = Some(now + DATE_LINGER);
            self.schedule_date_hide(DATE_LINGER, cx);
        } else if top > last + px(6.)
            && self
                .floating
                .date_until
                .is_some_and(|until| until > now + DATE_LEAVE)
        {
            self.floating.date_until = Some(now + DATE_LEAVE);
            self.schedule_date_hide(DATE_LEAVE, cx);
        }
    }

    /// One pending hide: a newer one replaces it, and dropping the old
    /// task cancels it.
    fn schedule_date_hide(&mut self, after: Duration, cx: &mut Context<Self>) {
        self.floating.date_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(after).await;
            this.update(cx, |_, cx| cx.notify()).ok();
        }));
    }

    fn schedule_jump_close(&mut self, after: Duration, cx: &mut Context<Self>) {
        self.floating.close_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(after).await;
            this.update(cx, |this, cx| {
                if !this.floating.held() {
                    this.floating.open = false;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Hovering the button, or reaching it with the keyboard, peeks at what
    /// it says; leaving closes it again after a beat, since the reader has
    /// now seen it.
    fn jump_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        self.floating.hovered = hovered;
        self.peek_changed(cx);
    }

    fn peek_changed(&mut self, cx: &mut Context<Self>) {
        self.floating.hover_task = None;
        if !self.floating.held() {
            if self.floating.open {
                self.schedule_jump_close(HOVER_GRACE, cx);
            }
            return;
        }
        // Holding it holds an open chip open.
        self.floating.close_task = None;
        if self.floating.open || matches!(self.floating.jump, None | Some(Jump::Latest)) {
            return;
        }
        // The keyboard means it at once; a passing pointer may not.
        let intent = if cx.reduce_motion() || self.floating.focused {
            Duration::ZERO
        } else {
            HOVER_INTENT
        };
        self.floating.hover_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(intent).await;
            this.update(cx, |this, cx| {
                if this.floating.held() {
                    this.floating.open = true;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn jump_clicked(&mut self, cx: &mut Context<Self>) {
        let to_card = matches!(self.floating.jump, Some(Jump::Question | Jump::Permission));
        // The button leaves from under the keyboard: the composer takes the
        // keys back, or the question's answer once the jump lands on it.
        if self.floating.focused && !to_card {
            self.screen_focus_pending = true;
        }
        if self.pages.after {
            // The latest page loads in place of a glide; the answer still
            // takes the keys once the reader is at the end.
            if to_card {
                self.question_focus_pending = true;
            }
            self.reload_paged_latest(cx);
        } else {
            self.start_glide(to_card, cx);
        }
        cx.notify();
    }

    /// The question card is the thread's last row. A key that answers it
    /// while the reader is further up brings the card into view instead,
    /// so nothing, least of all a permission, is answered unseen.
    pub(super) fn question_out_of_view(&mut self, cx: &mut Context<Self>) -> bool {
        if self.current_question().is_none() || self.list_state.is_following_tail() {
            return false;
        }
        self.floating.reveal_card = true;
        cx.notify();
        true
    }

    /// Glide to the end: only the last stretch moves (see `GLIDE_REACH`),
    /// in a time that grows with the distance, re-reading the end every
    /// frame so rows that arrive or finish measuring move the landing.
    pub(super) fn start_glide(&mut self, to_card: bool, cx: &mut Context<Self>) {
        let max = self.list_state.max_offset_for_scrollbar().y;
        let top = (-self.list_state.scroll_px_offset_for_scrollbar().y)
            .max(px(0.))
            .min(max);
        let distance = max - top;
        if cx.reduce_motion() || distance < px(2.) {
            self.list_state.scroll_to_end();
            self.land_glide(to_card, cx);
            return;
        }
        let reach = self.list_state.viewport_bounds().size.height * GLIDE_REACH;
        let from = if distance > reach {
            let from = max - reach;
            self.list_state
                .set_offset_from_scrollbar(gpui::point(px(0.), -from));
            from
        } else {
            top
        };
        let span = (max - from).as_f32();
        let millis = (180. + span * 0.45).clamp(260., 420.);
        self.floating.glide = Some(Glide {
            from,
            started: motion::now(),
            duration: Duration::from_millis(millis as u64),
            to_card,
            at: from,
        });
    }

    /// Any wheel, press or key on the transcript takes over from a glide.
    pub(super) fn cancel_glide(&mut self) {
        self.floating.glide = None;
    }

    /// Advance a glide; called once a frame before the list lays out.
    pub(super) fn step_glide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.floating.reveal_card) {
            self.start_glide(true, cx);
        }
        let Some(glide) = self.floating.glide else {
            return;
        };
        // Something else moved the list since the last frame: it leads.
        let top = -self.list_state.scroll_px_offset_for_scrollbar().y;
        if (top - glide.at).abs() > px(2.) {
            self.floating.glide = None;
            return;
        }
        let now = motion::now();
        let progress = now.saturating_duration_since(glide.started).as_secs_f32()
            / glide.duration.as_secs_f32();
        if progress >= 1. || cx.reduce_motion() {
            self.floating.glide = None;
            self.list_state.scroll_to_end();
            self.land_glide(glide.to_card, cx);
            return;
        }
        let end = self.list_state.max_offset_for_scrollbar().y;
        let at = glide.from + (end - glide.from) * motion::ease_out_quint(progress);
        self.list_state
            .set_offset_from_scrollbar(gpui::point(px(0.), -at));
        if let Some(glide) = &mut self.floating.glide {
            glide.at = at;
        }
        window.request_animation_frame();
    }

    /// Runs from a click as well as a frame, so it asks for the next frame
    /// by notifying: a frame request outside paint is not allowed.
    fn land_glide(&mut self, to_card: bool, cx: &mut Context<Self>) {
        if to_card && self.current_question().is_some() {
            // A folded card has no answer to focus: the reader came for it.
            self.question_collapsed = false;
            self.floating.card_flash = Some(motion::now());
            self.question_focus_pending = true;
            cx.notify();
        }
    }

    /// Keep the card's row in the list in step with the question shown:
    /// it is one past the last timeline row while a question waits, and
    /// while an answered card closes at the tail.
    pub(super) fn sync_tail_card(&mut self, following: bool, now: Instant, reduced: bool) {
        let request = self
            .current_question()
            .map(|question| question.request_id.clone());
        let current = (request != self.floating.card.request)
            .then(|| self.current_question().cloned())
            .flatten();
        let card = &mut self.floating.card;
        if request != card.request {
            let previous = card.last.take();
            card.closing = match previous {
                Some(previous) if request.is_none() && following && !reduced => {
                    Some((previous, card.step, now))
                }
                _ => None,
            };
            card.arrived = (request.is_some() && following && !reduced).then_some(now);
            card.request = request;
            card.last = current;
        }
        let card = &mut self.floating.card;
        card.step = self.question_step;
        if card
            .closing
            .as_ref()
            .is_some_and(|(_, _, at)| now.saturating_duration_since(*at) >= CARD_CLOSE)
        {
            card.closing = None;
        }
        if card
            .arrived
            .is_some_and(|at| motion::progress(at, motion::ENTER, now, reduced) >= 1.)
        {
            card.arrived = None;
        }
        let want = card.request.is_some() || card.closing.is_some();
        if want != card.in_list {
            let rows = self.timeline.len();
            if want {
                self.list_state.splice(rows..rows, 1);
            } else {
                self.list_state.splice(rows..rows + 1, 0);
            }
            self.floating.card.in_list = want;
        }
    }

    /// Rows the list holds: the timeline, and the card's row after it.
    pub(super) fn list_rows(&self) -> usize {
        self.timeline.len() + usize::from(self.floating.card.in_list)
    }

    /// The card's row: the question card as the thread's last item. It
    /// grows into place when it arrives at the followed tail, brightens its
    /// edge when the jump button lands on it, and closes its space once
    /// answered there.
    pub(super) fn render_tail_card(
        &mut self,
        now: Instant,
        reduced: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let heights = self.floating.card.heights.clone();
        if let Some((question, step, at)) = &self.floating.card.closing {
            let t = (now.saturating_duration_since(*at).as_secs_f32() / CARD_CLOSE.as_secs_f32())
                .clamp(0., 1.);
            let fade = (now.saturating_duration_since(*at).as_secs_f32() / CARD_FADE.as_secs_f32())
                .clamp(0., 1.);
            window.request_animation_frame();
            let natural = heights.borrow().get(CARD_ROW).copied();
            let card = render_question_card(
                question,
                *step,
                self.question_collapsed,
                None,
                &self.question_selected,
                &[],
                &self.messages,
                cx,
            );
            let height =
                natural.map_or(px(0.), |natural| natural * (1. - motion::ease_out_cubic(t)));
            return div()
                .relative()
                .h(height)
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .opacity(1. - motion::ease_out_quint(fade))
                        .child(card),
                )
                .into_any_element();
        }
        let Some(question) = self.current_question() else {
            return div().into_any_element();
        };
        let step = self.question_step;
        let key_labels = question
            .questions
            .get(step.min(question.questions.len().saturating_sub(1)))
            .filter(|entry| entry.permission.is_none())
            .map(|entry| {
                (0..entry.options.len().min(9))
                    .map(|index| super::question_key_label(index, window))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut card = render_question_card(
            question,
            step,
            self.question_collapsed,
            self.pending_question_input.clone(),
            &self.question_selected,
            &key_labels,
            &self.messages,
            cx,
        );
        if let Some(flash) = since(self.floating.card_flash, FLASH, now).filter(|_| !reduced) {
            let left = 1. - motion::ease_out_quint(flash);
            let amber = hsla(theme::status_warning());
            card = card
                .border_color(motion::mix(hsla(theme::border()), amber, left))
                .shadow(vec![
                    gpui::BoxShadow::new(px(0.), px(0.), amber.opacity(0.36 * left))
                        .spread_radius(px(8.) * (1. - left)),
                ]);
            window.request_animation_frame();
        } else {
            self.floating.card_flash = None;
        }
        let row = div().child(card);
        match self.floating.card.arrived {
            Some(at) => {
                let natural = heights.borrow().get(CARD_ROW).copied();
                grow_in(
                    row,
                    motion::progress(at, motion::ENTER, now, reduced),
                    Grow {
                        from: px(0.),
                        glide_from: None,
                    },
                    natural,
                    heights,
                    CARD_ROW.to_owned(),
                )
            }
            None => {
                // Measured while at rest too, so a close starts from it.
                div()
                    .child(row)
                    .on_children_prepainted(move |bounds, _, _| {
                        if let Some(row) = bounds.first() {
                            heights
                                .borrow_mut()
                                .insert(CARD_ROW.to_owned(), row.size.height);
                        }
                    })
                    .into_any_element()
            }
        }
    }

    /// The date chip over the transcript's top edge: while the reader
    /// scrolls back, when the message at the top was sent; while older
    /// history loads, that it is loading.
    pub(super) fn render_date_chip(
        &mut self,
        following: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let now = motion::now();
        let reduced = cx.reduce_motion();
        let loading = self.pages.loading && self.pages.loading_older;
        let want = !following
            && !self.timeline.is_empty()
            && (loading || self.floating.date_until.is_some_and(|until| until > now));
        if !want && self.floating.date_until.is_some_and(|until| until <= now) {
            self.floating.date_until = None;
        }
        let token = if want {
            motion::FLOAT
        } else {
            motion::FLOAT_OUT
        };
        let (shown, moving) =
            self.floating
                .date_shown
                .step(if want { 1. } else { 0. }, token, now, reduced);
        if moving {
            window.request_animation_frame();
        }
        if shown <= 0. && !want {
            return None;
        }
        let top = self.list_state.logical_scroll_top();
        let label = if loading {
            tr("chat-history-loading")
        } else {
            let at = self
                .timeline
                .get(top.item_ix)
                .or_else(|| self.timeline.last())?;
            let first = !self.pages.before
                && top.item_ix == 0
                && -self.list_state.scroll_px_offset_for_scrollbar().y < px(60.);
            if first {
                self.start_label(at.created_ms)
            } else {
                self.date_label(at.created_ms)
            }
        };
        let chip = div()
            .id("date-chip-body")
            .flex()
            .items_center()
            .gap(GAP)
            .px(px(10.))
            .py(px(3.))
            .rounded_full()
            .bg(gpui::rgb(theme::bg_elevated()))
            .border_1()
            .border_color(gpui::rgb(theme::border()))
            .shadow_md()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(gpui::rgb(theme::text_secondary()))
            .when(want, |chip| chip.block_mouse_except_scroll())
            .role(gpui::Role::Status)
            .when(loading, |chip| {
                chip.child(spinner_with_id(
                    "date-chip-spinner".into(),
                    px(12.),
                    theme::text_muted(),
                ))
            })
            .child(gpui::text!(label));
        Some(
            div()
                .absolute()
                .top(px(10.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .debug_selector(|| "date-chip".into())
                .child(motion::floating(chip, shown, !want, -1.))
                .into_any_element(),
        )
    }

    /// The jump button over the transcript's bottom edge (see the module
    /// docs). `show`: the reader is scrolled up and no glide is under way.
    pub(super) fn render_jump(
        &mut self,
        show: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let now = motion::now();
        let reduced = cx.reduce_motion();
        // At the end everything below has been read, whether the button
        // is still fading or already gone (a long glide outlasts it).
        if !show && self.list_state.is_following_tail() {
            self.floating.caught_up();
        }
        // The button leaving from under the keyboard (a wheel to the end, a
        // jump) hands the keys back to the composer.
        if !show
            && self
                .floating
                .focus
                .as_ref()
                .is_some_and(|focus| focus.is_focused(window))
        {
            self.screen_focus_pending = true;
            window.request_animation_frame();
        }
        let token = if show {
            motion::FLOAT
        } else {
            motion::FLOAT_OUT
        };
        let (shown, moving) =
            self.floating
                .shown
                .step(if show { 1. } else { 0. }, token, now, reduced);
        if moving {
            window.request_animation_frame();
        }
        if !show && shown <= 0. {
            // Gone: rest as the plain round button, so it comes back from
            // a known shape and announces afresh.
            let floating = &mut self.floating;
            // A button that leaves from under the pointer or the keyboard
            // gets no hover or blur event for it.
            floating.hovered = false;
            floating.focused = false;
            if floating.jump.take().is_some() {
                floating.open = false;
                floating.announcer.reset();
                floating.opened = Tween::new(0.);
                floating.width = Tween::pixels(BUTTON.as_f32());
                floating.badge_in_chip = Tween::new(0.);
                floating.badge_shown = Tween::new(0.);
                floating.close_task = None;
            }
            return None;
        }
        if show {
            let jump = self.jump_state();
            let previous = self.floating.jump.replace(jump);
            if previous != Some(jump) {
                self.jump_changed(previous, jump, cx);
            }
        }
        let jump = self.floating.jump.unwrap_or(Jump::Latest);
        let counted = jump.count().is_some();
        // The keyboard on the button peeks like the pointer does.
        let focus = self
            .floating
            .focus
            .get_or_insert_with(|| cx.focus_handle().tab_stop(true).tab_index(0))
            .clone();
        let focused = show && focus.is_focused(window);
        if focused != self.floating.focused {
            self.floating.focused = focused;
            self.peek_changed(cx);
        }
        let JumpText {
            words,
            label,
            count,
            ..
        } = self.jump_text(jump);
        let rem = window.rem_size();
        let (word_width, badge_width) = match &self.floating.widths {
            Some((cached_words, cached_count, cached_rem, word_width, badge_width))
                if *cached_words == words && *cached_count == count && *cached_rem == rem =>
            {
                (*word_width, *badge_width)
            }
            _ => {
                let word_width = text_width(
                    window,
                    &words,
                    rems(0.75).to_pixels(rem),
                    FontWeight::MEDIUM,
                );
                let badge_width = if counted {
                    (text_width(window, &count, px(11.), FontWeight::BOLD) + px(8.)).max(BADGE)
                } else {
                    BADGE
                };
                self.floating.widths =
                    Some((words.clone(), count.clone(), rem, word_width, badge_width));
                (word_width, badge_width)
            }
        };
        let lead = if counted {
            LEAD_COUNTED + badge_width
        } else {
            LEAD_MARK + MARK
        };
        let open_width = lead + GAP + word_width + PAD_END + px(2.);
        let floating = &mut self.floating;
        let open = floating.open && jump != Jump::Latest;
        let step = |tween: &mut Tween, to: f32, token, window: &mut Window| {
            let (value, moving) = tween.step(to, token, now, reduced);
            if moving {
                window.request_animation_frame();
            }
            value
        };
        let opened = step(
            &mut floating.opened,
            if open { 1. } else { 0. },
            motion::FLOAT,
            window,
        );
        let target_width = open_width.as_f32().ceil();
        let open_w = step(&mut floating.width, target_width, motion::FLOAT, window);
        let width = BUTTON + (px(open_w) - BUTTON) * opened;
        let in_chip = step(
            &mut floating.badge_in_chip,
            if open && counted { 1. } else { 0. },
            motion::FLOAT,
            window,
        );
        let badge_wanted = counted;
        let badge_token = if badge_wanted {
            motion::FLOAT
        } else {
            motion::FLOAT_OUT
        };
        let badge_shown = step(
            &mut floating.badge_shown,
            if badge_wanted { 1. } else { 0. },
            badge_token,
            window,
        );
        let you = step(
            &mut floating.you,
            if matches!(jump, Jump::Question | Jump::Permission) {
                1.
            } else {
                0.
            },
            motion::TOGGLE,
            window,
        );
        let fail = step(
            &mut floating.fail,
            if jump == Jump::Failed { 1. } else { 0. },
            motion::TOGGLE,
            window,
        );
        let tick = since(floating.tick, TICK, now).filter(|_| !reduced);
        let ping = since(floating.ping, PING, now).filter(|_| !reduced);
        if tick.is_some() || ping.is_some() {
            window.request_animation_frame();
        }
        // Settling: the words go first, then the width closes and the
        // round button's mark comes back. Opening runs the other way.
        let words_shown = ((opened - 0.5) / 0.5).clamp(0., 1.);
        let mark_shown = 1. - (opened / 0.5).clamp(0., 1.);
        let compact = match jump {
            Jump::Question | Jump::Permission => Mark::You,
            Jump::Failed => Mark::Fail,
            Jump::Working | Jump::New { running: true, .. } => Mark::Running,
            Jump::Latest | Jump::New { running: false, .. } => Mark::Arrow,
        };
        let lead_mark = if compact == Mark::Arrow && !counted {
            Mark::Arrow
        } else {
            compact
        };
        let edge = motion::mix(
            motion::mix(
                hsla(theme::border()),
                hsla(theme::status_warning()).opacity(0.5),
                you,
            ),
            hsla(theme::status_error()).opacity(0.5),
            fail,
        );
        let elevated = hsla(theme::bg_elevated());
        let hover = motion::mix(elevated, hsla(theme::bg_sidebar_pill()), 0.5);
        let mut shadows = vec![
            gpui::BoxShadow::new(px(0.), px(4.), gpui::hsla(0., 0., 0., 0.25))
                .blur_radius(px(6.))
                .spread_radius(px(-1.)),
            gpui::BoxShadow::new(px(0.), px(2.), gpui::hsla(0., 0., 0., 0.2))
                .blur_radius(px(4.))
                .spread_radius(px(-2.)),
        ];
        if let Some(ping) = ping {
            let color = if jump == Jump::Failed {
                theme::status_error()
            } else {
                theme::status_warning()
            };
            let eased = motion::ease_out_expo(ping);
            shadows.push(
                gpui::BoxShadow::new(px(0.), px(0.), hsla(color).opacity(0.32 * (1. - eased)))
                    .spread_radius(px(8.) * eased),
            );
        }
        let mut word_text = morph_text("jump-words", words.clone());
        if matches!(jump, Jump::Working | Jump::New { running: true, .. }) && !theme::is_light() {
            word_text = word_text
                .shine(
                    motion::RUNNING_SHINE,
                    gpui::rgb(theme::sidebar_running_shine()),
                )
                .shine_band(0.55, 0.3);
        }
        let island = div()
            .id("jump-to-latest")
            .debug_selector(|| "jump-to-latest".into())
            .role(gpui::Role::Button)
            .aria_label(label)
            .key_context("Button")
            .track_focus(&focus)
            .focus_visible(crate::ui::widgets::focus_ring)
            .on_key_down(ChatScreen::sidebar_button_key_down)
            .relative()
            .flex_none()
            .h(BUTTON)
            .w(width)
            .rounded_full()
            .overflow_hidden()
            .border_1()
            .border_color(edge)
            .shadow(shadows)
            .cursor_pointer()
            .text_xs()
            .font_weight(FontWeight::MEDIUM)
            .text_color(gpui::rgb(theme::text_primary()))
            // Shown, it takes the pointer from the row under it (the wheel
            // still scrolls the thread); leaving, it lets it through.
            // The plain arrow never opens, so the pointer gets its words.
            .when(show && jump == Jump::Latest, |island| {
                island.tooltip(crate::ui::widgets::tooltip(words.clone(), None))
            })
            .when(show, |island| {
                island
                    .block_mouse_except_scroll()
                    .on_click(cx.listener(|this, _, _, cx| this.jump_clicked(cx)))
                    .on_hover(
                        cx.listener(|this, hovered: &bool, _, cx| this.jump_hovered(*hovered, cx)),
                    )
            })
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .size(BUTTON - px(2.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .opacity(mark_shown)
                    .child(mark(COMPACT_IDS, compact, px(16.))),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .flex()
                    .items_center()
                    .gap(GAP)
                    .pl(if counted {
                        LEAD_COUNTED - px(1.)
                    } else {
                        LEAD_MARK - px(1.)
                    })
                    .whitespace_nowrap()
                    .opacity(words_shown)
                    .child(if counted {
                        // The badge rides over this space.
                        div().flex_none().w(badge_width).h(BADGE)
                    } else {
                        mark(LEAD_IDS, lead_mark, MARK)
                    })
                    .child(word_text),
            )
            .bg(elevated)
            .hover_bg(hover);
        // One badge in two places: the round button's corner, cut out of
        // its edge by a ring in the window's ground, and the open chip's
        // lead, where the ring fades since it crosses no edge there.
        let corner = gpui::point(BUTTON + BADGE_OUT - badge_width, -BADGE_OUT);
        let lead_at = gpui::point(LEAD_COUNTED, (BUTTON - BADGE) / 2.);
        let badge_at = gpui::point(
            corner.x + (lead_at.x - corner.x) * in_chip,
            corner.y + (lead_at.y - corner.y) * in_chip,
        );
        let badge_fill = motion::mix(
            hsla(theme::text_primary()),
            hsla(theme::sidebar_running_shine()),
            tick.map_or(0., |tick| 1. - motion::ease_out_quint(tick)),
        );
        let badge = div()
            .absolute()
            .left(badge_at.x)
            .top(badge_at.y)
            .h(BADGE)
            .min_w(BADGE)
            .px(px(4.))
            .rounded(BADGE / 2.)
            .flex()
            .items_center()
            .justify_center()
            .bg(badge_fill)
            .shadow(vec![
                gpui::BoxShadow::new(px(0.), px(0.), hsla(theme::bg_app()).opacity(1. - in_chip))
                    .spread_radius(px(2.)),
            ])
            .opacity(badge_shown)
            .text_size(px(11.))
            .line_height(BADGE)
            .font_weight(FontWeight::BOLD)
            .child(morph_text("jump-count", count).color(gpui::rgb(theme::bg_app())));
        Some(
            div()
                .absolute()
                .bottom_3()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(motion::floating(
                    div()
                        .relative()
                        .w(width)
                        .h(BUTTON)
                        .child(island)
                        .when(badge_shown > 0., |wrap| wrap.child(badge)),
                    shown,
                    !show,
                    1.,
                ))
                .into_any_element(),
        )
    }

    /// What the button says for `jump`, built when it changes rather than
    /// every frame.
    fn jump_text(&mut self, jump: Jump) -> JumpText {
        let locale = crate::ui::i18n::current_locale();
        if let Some(text) = &self.floating.text
            && text.jump == jump
            && text.locale == locale
        {
            return text.clone();
        }
        let n = jump.count().unwrap_or(0);
        // Words: the open chip's lead says the count, so they drop it.
        let words: SharedString = match jump {
            Jump::Latest => tr("chat-jump-latest"),
            Jump::Working => tr("chat-jump-working"),
            Jump::New { .. } => {
                crate::ui::i18n::tr_args("chat-jump-new", [("count", (n as i64).into())])
            }
            Jump::Question => tr("chat-notification-question"),
            Jump::Permission => tr("chat-notification-permission"),
            Jump::Failed => tr("chat-task-failed"),
        };
        // Said as what the button does, then what it found.
        let status: SharedString = match jump {
            Jump::New { .. } => {
                crate::ui::i18n::tr_args("chat-jump-new-label", [("count", (n as i64).into())])
            }
            _ => words.clone(),
        };
        let label: SharedString = match jump {
            Jump::Latest => words.clone(),
            _ => {
                crate::ui::i18n::tr_args("chat-jump-label", [("status", status.to_string().into())])
            }
        };
        let count: SharedString = match jump.count() {
            Some(count) if count > 99 => "99+".into(),
            Some(count) => count.to_string().into(),
            None => "".into(),
        };
        let text = JumpText {
            jump,
            locale,
            words,
            label,
            count,
        };
        self.floating.text = Some(text.clone());
        text
    }

    /// The button's news changed: open it to say so, or tick its count.
    fn jump_changed(&mut self, previous: Option<Jump>, jump: Jump, cx: &mut Context<Self>) {
        let now = motion::now();
        let run = self
            .selected_session
            .as_ref()
            .and_then(|session| self.active_runs.get(session))
            .map(String::as_str);
        let announce = self.floating.announcer.update(jump, run);
        if let (Some(before), Some(after)) = (previous.and_then(Jump::count), jump.count())
            && before != after
        {
            self.floating.tick = Some(now);
        }
        let turned_loud = jump.loud() && previous.is_none_or(|previous| !previous.same_news(jump));
        if turned_loud && previous.is_some() {
            self.floating.ping = Some(now);
        }
        if jump == Jump::Latest {
            self.floating.open = false;
        } else if announce {
            self.floating.open = true;
            if !self.floating.held() {
                self.schedule_jump_close(if jump.loud() { HOLD_LOUD } else { HOLD }, cx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_replies_announce_once_and_then_only_count() {
        let mut announcer = Announcer::default();
        let new = |count| Jump::New {
            count,
            running: false,
        };
        assert!(!announcer.update(Jump::Latest, None));
        assert!(announcer.update(new(1), None), "the first reply announces");
        assert!(
            !announcer.update(new(2), None),
            "a second reply only ticks the count"
        );
        assert!(!announcer.update(new(12), None));
        // The run ending with replies still unread is not news either.
        assert!(!announcer.update(
            Jump::New {
                count: 12,
                running: true
            },
            Some("r1")
        ));
        // A question is different news, and so is a failure.
        assert!(announcer.update(Jump::Question, Some("r1")));
        assert!(
            !announcer.update(new(13), None),
            "back to replies, already announced"
        );
        assert!(announcer.update(Jump::Failed, None));
    }

    #[test]
    fn the_announcement_rearms_once_nothing_is_unread() {
        let mut announcer = Announcer::default();
        let new = Jump::New {
            count: 1,
            running: true,
        };
        assert!(announcer.update(Jump::Working, Some("r1")));
        assert!(announcer.update(new, Some("r1")));
        // Read to the end: the next reply announces again.
        announcer.reset();
        assert!(announcer.update(new, Some("r1")));
        // The next run starting with nothing unread re-arms it too.
        assert!(announcer.update(Jump::Working, Some("r2")));
        assert!(announcer.update(new, Some("r2")));
    }

    #[test]
    fn days_are_named_from_today() {
        let day = |y, m, d| chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap();
        let today = day(2026, 3, 1);
        assert_eq!(DayName::of(today, today), DayName::Today);
        assert_eq!(DayName::of(day(2026, 2, 28), today), DayName::Yesterday);
        assert_eq!(
            DayName::of(day(2026, 2, 27), today),
            DayName::Date { month: 2, day: 27 }
        );
        assert_eq!(
            DayName::of(day(2025, 3, 1), today),
            DayName::Date { month: 3, day: 1 },
            "a year back is a date, not today"
        );
    }

    #[test]
    fn maple_working_is_said_once_per_run() {
        let mut announcer = Announcer::default();
        assert!(announcer.update(Jump::Working, Some("r1")));
        // A question in the same run, answered: back to working, unsaid.
        assert!(announcer.update(Jump::Question, Some("r1")));
        assert!(!announcer.update(Jump::Working, Some("r1")));
        // Scrolling to the end and back up during the run says nothing new.
        announcer.reset();
        assert!(!announcer.update(Jump::Working, Some("r1")));
        // The run ends; the next one is news.
        assert!(!announcer.update(Jump::Latest, None));
        assert!(announcer.update(Jump::Working, Some("r2")));
    }

    #[test]
    fn the_button_ranks_like_the_sidebar() {
        assert_eq!(Jump::of(Some(false), true, true, 3), Jump::Question);
        assert_eq!(Jump::of(Some(true), false, false, 0), Jump::Permission);
        assert_eq!(
            Jump::of(None, true, true, 3),
            Jump::New {
                count: 3,
                running: true
            }
        );
        assert_eq!(Jump::of(None, true, false, 0), Jump::Working);
        assert_eq!(Jump::of(None, false, true, 3), Jump::Failed);
        assert_eq!(
            Jump::of(None, false, false, 2),
            Jump::New {
                count: 2,
                running: false
            }
        );
        assert_eq!(Jump::of(None, false, false, 0), Jump::Latest);
    }

    #[test]
    fn only_replies_count_and_the_last_run_marks_a_failure() {
        let mut floating = Floating::default();
        floating.note_arrival("message", Some("assistant"));
        floating.note_arrival("message", Some("user"));
        floating.note_arrival("tool", None);
        floating.note_arrival("error", None);
        assert_eq!(floating.unseen, 1);
        assert!(!floating.failed, "an error row is not a failed run");
        floating.run_finished(true, false);
        assert!(floating.failed);
        // A later run that ends well replaces it.
        floating.run_finished(false, false);
        assert!(!floating.failed);
        // A failure read at the end is already seen.
        floating.run_finished(true, true);
        assert!(!floating.failed);
        floating.run_finished(true, false);
        floating.caught_up();
        assert_eq!(floating.unseen, 0);
        assert!(!floating.failed);
    }
}
