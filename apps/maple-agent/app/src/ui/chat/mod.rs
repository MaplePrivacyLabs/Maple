//! The agent chat surface: session sidebar, streaming transcript with tool
//! calls, questions, and the composer. Pure consumer of the backend
//! facade + event stream.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use gpui::{
    AppContext, Div, Entity, EntityInputHandler, EventEmitter, Focusable, Render, SharedString,
    Window, div, prelude::*, px,
};
use maple_agent::agent::{
    AgentCreateSessionRequest, AgentIntegration, AgentIntegrationAvailability,
    AgentIntegrationBackend, AgentMcpServer, AgentMcpTransport, AgentProjectTrustStatus,
    AgentQueuedMessage, AgentSendMessageRequest, AgentServiceEvent, AgentSessionIntegrationKind,
    AgentSessionMcpServer, AgentSessionSummary, AgentSlashCommand, AgentSubagent, AgentTaskState,
    AgentThinkingState, AgentTimelineItem, CatalogEntry, RecentProjectRoot, SideQuestionEvent,
    ThinkingLevel,
};

use crate::backend::{AgentBackend, PendingQuestion};
use crate::ui::icons::{icon, spinner};
use crate::ui::markdown;
use crate::ui::motion;
use crate::ui::popup::{Menu, MenuItem, Placement, Popup};
use crate::ui::rich_text::{self, RenderCtx};
use crate::ui::settings::{OpenSettingsSection, Section};
use crate::ui::text_input::TextInput;
use crate::ui::theme;
use crate::ui::titlebar;
use crate::ui::widgets;

mod cache;
mod commands;
mod composer;
pub(crate) mod computers;
use computers::{Computer, Destination, Navigate, NavigationChanged};
mod attachments;
mod dialogs;
mod feedback;
mod navigation;
mod paging;
mod project_folder;
mod queue;
mod sending;
mod sidebar;
mod speech;
mod staging;
mod summaries;
#[cfg(test)]
mod tests;
mod thinking;
mod transcript;
#[cfg(test)]
mod upload_tests;
mod uploads;

use self::cache::{DerivedCache, INLINE_PARSE_LIMIT, MarkdownCache, MarkdownKind};
use self::commands::ChatCommand;
use self::composer::{SideQuestionPanel, SlashEntry, slash_entries_for};
use self::navigation::ApplicationVimState;
#[cfg(test)]
use self::sidebar::SessionActivity;
use self::sidebar::{Sidebar, SidebarEvent, TaskActivity, root_display_name, session_summary_eq};
use self::transcript::{
    ActiveSubagent, PlanEntry, PlanStatus, plan_entries, plan_finished, render_question_card,
    render_waiting_indicator,
};

gpui::actions!(
    chat,
    [
        ChatEscape,
        ChooseProject,
        CopySelection,
        FocusSearch,
        NewTask,
        NextTask,
        OpenAppSettings,
        PreviousTask,
        SelectAllTranscript,
        ToggleArchived,
        ToggleSidebar,
    ]
);

/// Answer the question card with one of its numbered options.
#[derive(Clone, Debug, Default, PartialEq, gpui::Action)]
#[action(namespace = chat, no_json)]
pub struct PickQuestionOption {
    /// Zero-based position in the option list.
    pub index: usize,
}

pub struct LoggedOut;

pub(crate) struct SignOutFailed {
    pub(crate) user_id: String,
    pub(crate) message: String,
}

/// Emitted when the user opens app settings from the chat header.
pub struct OpenSettings;

/// A queued message pulled into the composer; it keeps its place in the
/// queue until the edit is sent or discarded.
#[derive(Debug, Clone)]
struct QueueEdit {
    queue_id: String,
    /// The composer text before the edit began, restored afterwards.
    draft: String,
}

/// Width of the task sidebar.
const SIDEBAR_WIDTH: gpui::Pixels = px(300.);
/// Width of the rail that stands in for the sidebar while it is collapsed.
const SIDEBAR_RAIL_WIDTH: gpui::Pixels = px(48.);
const SIDEBAR_TRANSITION_DURATION: std::time::Duration = std::time::Duration::from_millis(200);

/// One reversible transition. Sampling the current position when retargeting
/// avoids jumping back to an endpoint on a second press.
struct SidebarTransition {
    from: f32,
    to: f32,
    started: std::time::Instant,
    duration: std::time::Duration,
}

impl SidebarTransition {
    fn value(&self, now: std::time::Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.started).as_secs_f32();
        let t = (elapsed / self.duration.as_secs_f32()).clamp(0., 1.);
        let eased = 1. - (1. - t).powi(3);
        self.from + (self.to - self.from) * eased
    }
}
/// Height of the task header and the clear strip above the collapsed rail.
const CHAT_HEADER_HEIGHT: gpui::Pixels = px(40.);
/// Where the header's toolbar starts, from the window's left edge, as the
/// sidebar goes from collapsed (0) to expanded (1). Beside traffic lights
/// it stays put: they reach 30 px past the rail, followed by an 8 px gap.
/// Without them the Maple logo holds the top row's corner, and the
/// toolbar follows it at its own 4 px button gap: after the rail's mark
/// button, or after the wordmark with the same padding, on a whole pixel.
fn header_toolbar_left(sidebar_expansion: f32) -> gpui::Pixels {
    if titlebar::TRANSPARENT_TITLEBAR {
        return SIDEBAR_RAIL_WIDTH + px(38.);
    }
    let padding = (sidebar::TOP_ROW_HEIGHT - sidebar::WORDMARK_HEIGHT) / 2.;
    let mark = sidebar::WORDMARK_LEFT - padding + sidebar::TOP_ROW_HEIGHT;
    let wordmark = sidebar::WORDMARK_LEFT
        + crate::ui::icons::wordmark_width(sidebar::WORDMARK_HEIGHT)
        + padding;
    (mark + (wordmark - mark) * sidebar_expansion + px(4.)).round()
}
/// Reading width shared by the transcript and the composer; they must
/// stay in one column.
const CONTENT_WIDTH: gpui::Pixels = px(900.);
/// Header title when no task is selected.
const DEFAULT_TASK_TITLE: &str = "New Task";

fn notification_body(
    details_enabled: bool,
    generic: &str,
    details: impl FnOnce() -> String,
) -> String {
    if details_enabled {
        details()
    } else {
        generic.to_owned()
    }
}

#[cfg(test)]
mod notification_policy_tests {
    use super::notification_body;

    #[test]
    fn private_notifications_never_evaluate_sensitive_content() {
        assert_eq!(
            notification_body(false, "Open Maple to respond.", || {
                panic!("question text must not be read for a private notification")
            }),
            "Open Maple to respond."
        );
        assert_eq!(
            notification_body(false, "A task finished.", || {
                panic!("task title must not be read for a private notification")
            }),
            "A task finished."
        );
    }

    #[test]
    fn opted_in_notifications_use_the_detailed_body() {
        assert_eq!(
            notification_body(true, "A task finished.", || "“Example” finished".into()),
            "“Example” finished"
        );
    }
}

/// How long a notice stays before it clears itself.
const NOTICE_TTL: std::time::Duration = std::time::Duration::from_secs(8);
/// How long the plan card stays after its last item is checked off live,
/// so the final check is seen before the card goes.
const PLAN_DONE_HOLD: std::time::Duration = std::time::Duration::from_millis(1500);

const COMPOSER_PLACEHOLDER: &str = "Ask Maple to work in this folder…";
const SIDE_THREAD_PLACEHOLDER: &str = "Ask a side question (Esc to leave the thread)…";
const QUEUE_EDIT_PLACEHOLDER: &str =
    "Edit the queued message, then send to keep its place. Escape discards.";
/// Sent prompts kept for Up/Down recall.
const PROMPT_HISTORY_LIMIT: usize = 50;
/// Gap between two repaints of the subagent card, which shows a live
/// elapsed time. It runs only while a subagent works.
const SUBAGENT_TICK: std::time::Duration = std::time::Duration::from_secs(1);

use self::attachments::DraftAttachment;

/// Shared read-only state the transcript rows render from.
struct TranscriptCtx<'a> {
    markdown_cache: &'a MarkdownCache,
    derived: &'a DerivedCache,
    attachment_images: &'a HashMap<String, Arc<gpui::Image>>,
    attachment_failures: &'a HashSet<String>,
    chat: &'a gpui::WeakEntity<ChatScreen>,
    tool_summaries: &'a HashMap<String, SharedString>,
    render: &'a RenderCtx,
    /// Message being spoken, if any.
    speech: Option<&'a SpeechState>,
    /// The account can use text-to-speech.
    speech_available: bool,
    /// One-based position of the row and the row count, for
    /// assistive technology ("message 4 of 17").
    position: (usize, usize),
}

/// Text-to-speech progress for one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpeechState {
    pub item_id: String,
    /// False while the first chunk is still being synthesized.
    pub playing: bool,
}

#[derive(Default)]
pub(super) struct QuestionSelection {
    cursor: Option<usize>,
    picked: BTreeSet<usize>,
}

/// The chat's popup menus. One is open at a time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum ChatPopup {
    /// The header chip's project menu.
    Project,
    Model,
    Thinking,
    Integrations,
    Settings,
    /// The transcript's right-click menu, at a window position.
    Transcript(gpui::Point<gpui::Pixels>),
}

/// What the composer asked for while no task existed. The first send
/// creates the task and then runs this against it. The composer keeps
/// showing the text meanwhile; it clears when the send goes out.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FirstSend {
    /// Plain text (with the staged attachments) for the new task.
    Message { text: String, steer: bool },
    /// A `/command` that needs a task, as typed.
    Command(String),
    /// A `/btw` question for the new task.
    SideQuestion(String),
}

impl FirstSend {
    /// The message as it was typed, for the composer and the history.
    fn text(&self) -> String {
        match self {
            FirstSend::Message { text, .. } | FirstSend::Command(text) => text.clone(),
            FirstSend::SideQuestion(question) => format!("/btw {question}"),
        }
    }

    fn steer(&self) -> bool {
        matches!(self, FirstSend::Message { steer: true, .. })
    }
}

/// An integration toggled on the draft, applied once the task exists.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DraftMcpChange {
    name: String,
    kind: AgentSessionIntegrationKind,
    enabled: bool,
}

struct ProjectMcpSave {
    root: String,
    name: String,
    enabled: bool,
}

/// What the composer shows that a task just created does not have yet.
/// Applied before the first send.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NewSessionChanges {
    web: Option<bool>,
    mcp: Vec<DraftMcpChange>,
}

impl NewSessionChanges {
    fn is_empty(&self) -> bool {
        self.web.is_none() && self.mcp.is_empty()
    }
}

/// The row the draft's integrations chip shows for a configured MCP
/// server: what a new task would start with.
fn draft_mcp_row(server: AgentMcpServer) -> AgentSessionMcpServer {
    AgentSessionMcpServer {
        display_name: server.name.clone(),
        name: server.name,
        kind: AgentSessionIntegrationKind::Mcp,
        description: server.description,
        transport: match server.transport {
            AgentMcpTransport::Stdio { .. } => "stdio",
            AgentMcpTransport::StreamableHttp { .. } => "streamable_http",
        }
        .to_string(),
        enabled: server.enabled,
        available: true,
    }
}

/// The curated CUA integration's id, which is also the name the runtime
/// reads it under in a create request's server list.
const CUA_DRIVER_ID: &str = "cua-driver";

/// A name reduced to what the runtime keys servers by, so every spelling
/// of the CUA integration a configured server may carry matches its card.
fn integration_key(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn is_custom_mcp(name: &str, kind: AgentSessionIntegrationKind) -> bool {
    kind == AgentSessionIntegrationKind::Mcp
        && integration_key(name) != integration_key(CUA_DRIVER_ID)
}

fn same_integration(
    name: &str,
    kind: AgentSessionIntegrationKind,
    other_name: &str,
    other_kind: AgentSessionIntegrationKind,
) -> bool {
    kind == other_kind
        && (name == other_name
            || (is_custom_mcp(name, kind)
                && is_custom_mcp(other_name, other_kind)
                && AgentMcpServer::key_for_name(name) == AgentMcpServer::key_for_name(other_name)))
}

/// The rows the draft's integrations chip shows: what a new task would
/// start with, laid out as the runtime lays out a task's rows. The
/// configured servers come first (a custom one spelled like the CUA
/// integration is shadowed by it, as at creation); then the external
/// agents enabled in Settings, off until selected for the task; then the
/// CUA integration, on when it is the default for new tasks.
fn draft_mcp_rows(
    servers: Vec<AgentMcpServer>,
    integrations: &[AgentIntegration],
) -> Vec<AgentSessionMcpServer> {
    let cua = integrations.iter().find(|card| card.id == CUA_DRIVER_ID);
    let mut rows: Vec<AgentSessionMcpServer> = servers
        .into_iter()
        .filter(|server| {
            cua.is_none() || integration_key(&server.name) != integration_key(CUA_DRIVER_ID)
        })
        .map(draft_mcp_row)
        .collect();
    rows.extend(
        integrations
            .iter()
            .filter(|card| card.is_external_agent() && card.enabled_for_new_tasks)
            .map(|card| AgentSessionMcpServer {
                name: card.id.clone(),
                kind: AgentSessionIntegrationKind::ExternalAgent,
                display_name: card.name.clone(),
                description: card.description.clone(),
                transport: "external_agent".to_string(),
                enabled: false,
                available: card.availability == AgentIntegrationAvailability::Available,
            }),
    );
    if let Some(card) = cua {
        rows.push(AgentSessionMcpServer {
            name: card.id.clone(),
            kind: AgentSessionIntegrationKind::Mcp,
            display_name: card.name.clone(),
            description: card.description.clone(),
            transport: match card.backend {
                Some(AgentIntegrationBackend::Embedded) => "embedded",
                None => "unconfigured",
            }
            .to_string(),
            enabled: card.backend.is_some() && card.enabled_for_new_tasks,
            available: card.backend.is_some()
                && card.availability == AgentIntegrationAvailability::Available,
        });
    }
    rows
}

struct SendReceiptQuery {
    id: String,
    outcome: Option<crate::host::receipts::Outcome>,
    backend: Arc<AgentBackend>,
}

#[derive(Clone)]
struct UnresolvedSend {
    backend: Arc<AgentBackend>,
    outcome: Option<crate::host::receipts::Outcome>,
    session: String,
    text: String,
    attachments: Vec<DraftAttachment>,
    from_composer: bool,
}

impl UnresolvedSend {
    fn awaiting_receipt(&self) -> bool {
        matches!(
            self.outcome,
            None | Some(crate::host::receipts::Outcome::Pending)
        )
    }
}

pub struct ChatScreen {
    backend: Arc<AgentBackend>,
    user_id: String,
    /// The task list; its own entity so it renders only when it changes.
    sidebar: Entity<Sidebar>,
    computers: Arc<Vec<Computer>>,
    navigation_ready: bool,
    /// Paired computers initially observe only navigation summaries. Catalogs,
    /// tools and a conversation are loaded when that computer is first opened.
    navigation_only: bool,
    visible: bool,
    next_navigation_refresh: std::time::Instant,
    pending_navigation: Option<(u64, Destination)>,
    sessions: Vec<AgentSessionSummary>,
    selected_session: Option<String>,
    timeline: Vec<AgentTimelineItem>,
    pages: paging::TranscriptPages,
    /// Active run per session id, kept across selection changes.
    active_runs: HashMap<String, String>,
    /// Successfully completed tasks that have not been opened since their
    /// terminal event. This is intentionally independent of selection and
    /// project grouping; the sidebar derives presentation from it.
    completed_unread_sessions: HashSet<String>,
    /// Tasks whose last run failed and that have not been opened since;
    /// kept the same way as `completed_unread_sessions`.
    failed_unread_sessions: HashSet<String>,
    /// Ids of runs whose Finished event already arrived, newest last and
    /// capped at `FINISHED_RUNS_KEPT`. A send that returns after its run
    /// finished must not mark that run active again.
    finished_runs: std::collections::VecDeque<String>,
    /// Suppresses duplicate session creation while one is in flight.
    session_setup_pending: bool,
    /// Waiting for the draft's MCP choices before starting a create.
    first_send_waiting_for_mcp: bool,
    /// The new-task screen is a draft the user started (or the auto-select
    /// fell back to). Nothing is selected while it shows, and no session
    /// list or boot fallback may open a task over it: the text typed into
    /// it belongs to the task the first send creates.
    draft: bool,
    /// The send that is creating the task, run once the task lands. A
    /// selection change meanwhile drops it.
    pending_first_send: Option<FirstSend>,
    /// UI-only previews. They never enter persisted history or drive retries.
    first_send_preview: Option<sending::SendPreview>,
    sending_previews: HashMap<String, sending::SendPreview>,
    /// The integration rows a new task would start with, once the draft's
    /// chip has loaded them. `None` until then: the create request then
    /// names no servers and the task takes the defaults, rather than an
    /// empty list that would start nothing.
    draft_mcp_defaults: Option<Vec<AgentSessionMcpServer>>,
    /// Integration toggles made on the draft, applied over the defaults
    /// (they survive a reload of the rows). The MCP servers switched on go
    /// into the create request's name list; the rest are applied to the
    /// new task the way the chip applies them to a live one.
    draft_mcp_changes: Vec<DraftMcpChange>,
    /// Latest draft catalog read; a stale response must not restore old rows.
    draft_mcp_generation: u64,
    /// Save clicks in order, coalescing changes that have not started yet.
    project_mcp_saves: std::collections::VecDeque<ProjectMcpSave>,
    project_mcp_save_pending: bool,
    /// An ask_user question waiting for the user's text answer.
    /// Questions waiting for the user, oldest first. The model can issue
    /// several ask_user calls in one turn; every card must be answerable.
    pending_questions: Vec<PendingQuestion>,
    /// Lazily created answer input for the question card.
    pending_question_input: Option<Entity<TextInput>>,
    /// Fraction of the context window in use for the selected session.
    context_fraction: Option<f32>,
    /// Session the usage poller follows, if one is running. A run on a
    /// newly selected session replaces the poller of the previous one.
    usage_poller_session: Option<String>,
    /// Bumped when a poller is replaced; the old task exits at its next
    /// tick instead of racing the new one.
    usage_poller_generation: u64,
    /// Last ledger-confirmed context tokens for the selected session.
    ledger_context_tokens: i64,
    /// Context limit used with the estimate above.
    context_limit: i64,
    composer: Option<Entity<TextInput>>,
    /// Persisted opt-in for modal editing in this composer only.
    composer_vim_enabled: bool,
    /// Independent, persisted application-level Vim navigation profile.
    application_vim_enabled: bool,
    /// Stable-ID semantic selection and count state for application Vim.
    application_vim: ApplicationVimState,
    /// Focus proxy used while application navigation, rather than a text
    /// field, owns keyboard input.
    application_focus: Option<gpui::FocusHandle>,
    /// Reclaim the appropriate Chat focus after mounting or returning from
    /// Settings. The destination depends on the current navigation profile.
    screen_focus_pending: bool,
    /// Composer holds non-blank text; refreshed when the composer changes.
    composer_has_text: bool,
    /// Palette rows for the composer's current "/" token; rebuilt when the
    /// composer changes, not per frame.
    slash_entries: Vec<SlashEntry>,
    models: Vec<String>,
    selected_model: Option<String>,
    /// Last explicit pick / saved default. New tasks restore this.
    default_model: Option<String>,
    thinking: Option<AgentThinkingState>,
    /// Cached per task and model inside this account/computer's screen.
    thinking_cache: thinking::ThinkingCache,
    thinking_loading: bool,
    thinking_generation: u64,
    thinking_saving: bool,
    /// Desktop notifications enabled (settings).
    notify_enabled: bool,
    /// Local-store policy, independent of the selected execution computer.
    notify_details: bool,
    /// Mirrors `window.is_window_active()` from the last render; refreshed
    /// on activation changes because they force a redraw.
    window_active: bool,
    sidebar_plan: Option<crate::billing::PlanUsage>,
    runtime_error: Option<SharedString>,
    persistence_status: maple_agent::store::PersistenceStatus,
    host_recovery: HostRecovery,
    notice: Option<SharedString>,
    booting: bool,
    /// Task being opened whose snapshot has not landed yet, so the pane
    /// can say so instead of showing the previous task.
    loading_session: Option<String>,
    loading_timeline: HashMap<String, AgentTimelineItem>,
    snapshot_revision: u64,
    /// Virtualized transcript state; bottom-aligned like a chat log.
    list_state: gpui::ListState,
    /// First and past-last transcript rows painted in view in the last
    /// frame. A wheel outside the list reports them for paging and
    /// images, as the list's own wheel reports its visible range; the
    /// list's scroll top names no row while it rests at the end.
    painted_rows: std::rc::Rc<std::cell::Cell<Option<(usize, usize)>>>,
    /// Set when the transcript should jump to its newest content on the
    /// next render (session switch or send); streaming follows only while
    /// the view is already at the bottom.
    /// Whether tool cards show their input/output payloads. Toggled from
    /// the header; off gives a one-line card per tool call.
    tool_details: bool,
    /// Project context shown by the UI and used explicitly for new tasks.
    /// Existing tasks always execute in their own persisted project root.
    project_root: Option<String>,
    recent_roots: Vec<RecentProjectRoot>,
    /// Which of the chat's popup menus is open: the header's project menu,
    /// a composer chip's menu, or the transcript's right-click menu.
    popup: Popup<ChatScreen, ChatPopup>,
    /// Focus for whichever modal dialog is open, so Enter and Escape
    /// reach it instead of the composer. Created the first time a dialog
    /// opens: creating it up front shifts the window's focus-id order,
    /// which the typing-focus test showed gpui is sensitive to.
    dialog_focus: Option<gpui::FocusHandle>,
    /// A dialog just opened; the next render moves focus into it.
    dialog_focus_pending: bool,
    feedback: Option<feedback::FeedbackForm>,
    /// Whether the project-trust question may open its dialog. Tests that
    /// drive typing turn it off, since the dialog rightly takes focus.
    trust_prompts: bool,
    /// Host-owned folder browser for remote computers (and native-picker fallback).
    project_folder_dialog: Option<Entity<project_folder::ProjectFolderDialog>>,
    root_picker_open: bool,
    root_selecting: bool,
    /// Header label for the project root; set when the root changes so
    /// render does not format it.
    project_label: SharedString,
    /// Git branch of the project root, read off the UI thread. `None`
    /// when the root is not a git checkout.
    project_branch: Option<String>,
    /// `project_branch` in parentheses, ready for the header.
    branch_label: Option<SharedString>,
    /// Watches the git dir of the current root so a checkout by the agent
    /// or from a terminal updates the branch. Replaced when the git dir
    /// changes, dropped with the root.
    branch_watcher: Option<notify::RecommendedWatcher>,
    watched_git_dir: Option<std::path::PathBuf>,
    /// Sidebar hidden; a toggle in the main pane brings it back.
    sidebar_collapsed: bool,
    sidebar_transition: Option<SidebarTransition>,
    /// Files staged for the next message.
    draft_attachments: Vec<DraftAttachment>,
    unresolved_sends: HashMap<String, UnresolvedSend>,
    attachment_uploads: HashMap<String, uploads::PendingUpload>,
    accept_send_results: bool,
    sign_out_pending: bool,
    discarded_sends: HashSet<String>,
    checking_sends: bool,
    /// Source of `DraftAttachment::id`.
    draft_counter: u64,
    /// True while the native file dialog is open.
    attachment_picking: bool,
    /// The catalog's vision and context window per model id, looked up
    /// when a model is chosen.
    model_catalog: HashMap<String, CatalogEntry>,
    /// Friendly labels from the advanced catalog; selection always uses IDs.
    model_labels: HashMap<String, String>,
    /// The first model load is in flight. Until it lands with no model
    /// chosen, the composer controls are laid out but not shown, so the
    /// row appears once in its final shape instead of shifting.
    models_loading: bool,
    /// The controls were held back on the last frame; the next frame that
    /// shows them fades them in.
    controls_held: bool,
    /// When the held-back controls were revealed, and a counter that gives
    /// each reveal its own animation.
    controls_revealed: Option<(std::time::Instant, u32)>,
    /// MCP servers with their enabled state for the selected task.
    session_mcp: Vec<AgentSessionMcpServer>,
    /// Count of enabled servers in `session_mcp`, for the composer chip.
    mcp_enabled_count: usize,
    /// Composer fills the pane (fullscreen editing).
    composer_expanded: bool,
    /// Latest todo list from the selected task, pinned above the composer
    /// while any entry is unfinished. A finished list stays in the
    /// transcript.
    plan: Vec<PlanEntry>,
    /// Count of completed entries in `plan`, for the card header.
    plan_done: usize,
    /// Set while a plan that just finished live keeps its card for
    /// `PLAN_DONE_HOLD`; the task clears it.
    plan_hold: Option<gpui::Task<()>>,
    /// Open `/btw` side question, if any. Never part of the transcript.
    btw: Option<SideQuestionPanel>,
    /// Counter for side question request ids.
    btw_sequence: u64,
    /// Tasks whose pinned plan card shows only its header.
    plan_collapsed: HashSet<String>,
    /// External agents working for the selected task, pinned above the
    /// composer. Empty unless an external agent is working.
    subagents: Vec<ActiveSubagent>,
    /// A repaint that keeps the subagent elapsed times moving is already
    /// on its way; one at a time is enough.
    subagent_tick_pending: std::cell::Cell<bool>,
    /// Bumped whenever an event adds or removes a subagent row. A
    /// snapshot from `refresh_subagents` is applied only if no event
    /// landed while it was in flight; a stale one would wipe a row the
    /// events already know about (or revive one they removed).
    subagent_epoch: u64,
    /// Whether a running runtime has been seen, so the skills list is
    /// re-read once Goose knows the account's skills directory.
    runtime_running_seen: bool,
    /// Web tools on for the selected task (mirrors the session record).
    web_enabled: bool,
    /// Settings default applied to newly created tasks.
    default_web_enabled: bool,
    /// Parsed markdown per timeline item (keyed by item id and revision),
    /// so visible messages are parsed once, not every frame.
    markdown_cache: MarkdownCache,
    /// Whether a notice auto-dismiss timer is in flight.
    notice_dismiss_pending: std::cell::Cell<bool>,
    /// Per-item display strings, rebuilt when the item's revision moves.
    derived: DerivedCache,
    /// Background parses of the newest rows after a timeline load, so
    /// the first scroll finds them ready. A newer load replaces it.
    markdown_warmup: Option<gpui::Task<()>>,
    /// Item id to `(index in timeline, revision)`; the revision counts
    /// applied updates so caches can tell a changed item from a stable one.
    timeline_index: HashMap<String, (usize, u64)>,
    /// Title of the selected task, shown in the header.
    selected_title: SharedString,
    /// Guards against a slow session load overwriting a newer selection.
    selection_generation: u64,
    /// Same guard for mid-run history reloads. Separate from the selection
    /// generation so a reload never cancels a task switch in flight.
    reload_generation: u64,
    /// Per-session count of applied timeline events; a load whose snapshot
    /// predates newer events is discarded instead of clobbering them.
    timeline_revisions: HashMap<String, u64>,
    /// Decoded images for sent attachments, keyed by attachment id, so the
    /// transcript shows the picture instead of only its file name.
    attachment_images: HashMap<String, Arc<gpui::Image>>,
    /// Attachment ids with a read in flight or already failed; never asked
    /// for twice.
    attachment_requests: HashSet<String>,
    attachment_failures: HashSet<String>,
    attachment_queue: std::collections::VecDeque<String>,
    pending_attachments: usize,
    attachment_viewport: std::ops::Range<usize>,
    /// Shared drag-selection state for transcript text.
    selection: Option<Entity<rich_text::TextSelection>>,
    /// Focus handle of the transcript; a selection press moves focus here
    /// so the copy keybinding applies.
    transcript_focus: Option<gpui::FocusHandle>,
    /// True from send until the first agent item of the run arrives; shows
    /// the waiting indicator under the transcript.
    awaiting_first_token: bool,
    /// Item ids whose tool card or thinking row was clicked; membership
    /// inverts the `tool_details` default for that item.
    toggled_tools: HashSet<String>,
    /// Ticked options of a multi-select question.
    question_selected: HashMap<usize, QuestionSelection>,
    /// Index of the question being shown within the current card; a batch
    /// iterates one question at a time instead of listing them all.
    question_step: usize,
    /// Answers recorded for earlier steps of the current card, by index.
    question_step_answers: HashMap<usize, Vec<String>>,
    /// The question card is folded to its header so the transcript above
    /// it can be read; a new question or any answer key unfolds it.
    question_collapsed: bool,
    /// Full-size image shown over the chat until dismissed.
    lightbox: Option<Arc<gpui::Image>>,
    /// Sent prompts, oldest first, for Up/Down recall in the composer.
    prompt_history: Vec<String>,
    /// Position in `prompt_history` while browsing; `None` when typing.
    history_index: Option<usize>,
    /// The unsent draft saved when browsing started.
    history_draft: String,
    /// Root waiting for the user to confirm removal.
    /// The modal asking before a project removal or a task deletion.
    confirm: Option<dialogs::ConfirmDialog>,
    /// Project that has skills or guidance and no trust decision yet.
    trust_prompt: Option<AgentProjectTrustStatus>,
    trust_saving: bool,
    /// Messages waiting behind the selected session's active run.
    queue: Vec<AgentQueuedMessage>,
    /// First line of each queued message, for the chips.
    queue_previews: Vec<SharedString>,
    /// A newer release, until the banner is dismissed.
    update: crate::update::Snapshot,
    queue_busy: bool,
    queue_edit: Option<QueueEdit>,
    /// Slash commands from the installed skills of the current project root.
    slash_commands: Vec<AgentSlashCommand>,
    /// Highlighted row in the open slash palette, if any.
    slash_selected: Option<usize>,
    /// Set when a pending question should steal focus at the next render.
    question_focus_pending: bool,
    /// One-line model summaries per tool item id.
    tool_summaries: HashMap<String, SharedString>,
    /// Item ids with a summary request sent; never asked twice.
    summary_requests: HashSet<String>,
    /// Item ids whose summary landed before their row was durable; each is
    /// stored when its row gains a persisted identity.
    unsaved_summaries: HashSet<String>,
    /// Summary requests in flight by token, each with its row's current
    /// key; caps how many ride at once.
    summary_inflight: HashMap<u64, String>,
    next_summary_token: u64,
    /// Tool rows the host could not summarize from their input; each is
    /// summarized from its output once it completes.
    output_summaries: HashSet<String>,
    #[cfg(test)]
    summary_probe: Option<summaries::SummaryProbe>,
    /// Item ids waiting for a free summary slot, oldest first.
    summary_queue: std::collections::VecDeque<String>,
    /// Bumped on session switch; results from an older generation are
    /// dropped instead of landing on the wrong screen state.
    summary_generation: u64,
    /// Tool call summaries enabled (settings).
    summaries_enabled: bool,
    /// Microphone and speaker, on their own thread.
    audio: Arc<crate::audio::AudioEngine>,
    /// Voice endpoints the account offers; refreshed after runtime start.
    audio_caps: maple_agent::agent::AudioCapabilities,
    /// The microphone is open.
    recording: bool,
    /// The microphone is being opened; set before the async start so a
    /// second click cannot start a second recording.
    recording_starting: bool,
    /// A recording is at Whisper.
    transcribing: bool,
    /// Text-to-speech in progress.
    speech: Option<SpeechState>,
    /// Backend-call bridges retained so their final drop stays on this
    /// thread; see [`ChatScreen::call`].
    bridged_tasks: std::cell::RefCell<Vec<gpui::Task<()>>>,
    /// Bumped on every speak or stop; stale chunks are dropped.
    speech_generation: u64,
    tts_voice: String,
    tts_speed: f32,
}

/// What a session snapshot load replaces once it lands.
#[derive(Clone, Copy)]
enum LoadMode {
    /// Make the session current (sidebar click, boot, or project selection).
    Select,
    /// Swap the timeline only (mid-run history compaction).
    Reload,
}

#[derive(Default)]
struct HostRecovery {
    epoch: u64,
    unavailable: bool,
    ended: bool,
    store_maintenance: bool,
    events: Option<Vec<AgentServiceEvent>>,
    overflow: bool,
}

/// How often a stale snapshot is fetched again before it is applied.
const LOAD_RETRIES: u8 = 2;

/// Finished run ids remembered for a late send acknowledgement.
const FINISHED_RUNS_KEPT: usize = 64;

impl ChatScreen {
    pub(crate) fn apply_notification_settings(&mut self, settings: &crate::settings::AppSettings) {
        self.notify_enabled = settings.desktop_notifications;
        self.notify_details = settings.notification_details_enabled();
    }

    #[cfg(test)]
    pub(crate) fn notification_preferences_for_test(&self) -> (bool, bool) {
        (self.notify_enabled, self.notify_details)
    }

    pub fn connection_ended(&self) -> bool {
        self.host_recovery.ended
    }

    #[cfg(test)]
    pub(crate) fn seed_store_maintenance_for_test(
        &mut self,
        selected: Option<String>,
        draft: &str,
        cx: &mut Context<Self>,
    ) {
        self.selected_session = selected;
        self.fill_empty_composer(draft, cx);
        self.booting = false;
    }

    #[cfg(test)]
    pub(crate) fn store_maintenance_snapshot_for_test(
        &self,
        cx: &Context<Self>,
    ) -> (Option<String>, Option<String>, bool, Option<String>) {
        (
            self.selected_session.clone(),
            self.composer_text(cx),
            self.booting,
            self.runtime_error.as_ref().map(ToString::to_string),
        )
    }

    pub(crate) fn execution_backend(&self) -> Arc<AgentBackend> {
        self.backend.clone()
    }

    pub fn new(backend: Arc<AgentBackend>, user_id: String, cx: &mut Context<Self>) -> Self {
        let this = Self::new_mounted(backend, user_id, cx);
        this.start(cx);
        this
    }

    /// Build the mounted chat surface without starting backend refreshes.
    /// Production immediately calls `start`; focused GPUI tests use the
    /// deterministic seam so unrelated Tokio scheduling cannot replace their
    /// fixture state mid-interaction.
    fn new_mounted(backend: Arc<AgentBackend>, user_id: String, cx: &mut Context<Self>) -> Self {
        let weak = cx.entity().downgrade();
        let mut this = Self::new_inner(backend, user_id, cx);
        this.attach_composer(weak.clone(), cx);
        this.markdown_cache.attach(weak.clone(), cx.to_async());
        this.selection = Some(cx.new(|_| rich_text::TextSelection::default()));
        this.transcript_focus = Some(cx.focus_handle());
        this.application_focus = Some(cx.focus_handle());
        this.initialize_application_vim_surface();
        this.list_state.set_scroll_handler(move |event, _, cx| {
            if let Some(chat) = weak.upgrade() {
                chat.update(cx, |chat, cx| {
                    chat.transcript_scrolled(event.visible_range.start, event.visible_range.end, cx)
                });
            }
        });
        this
    }

    #[cfg(test)]
    pub(crate) fn new_without_start(
        backend: Arc<AgentBackend>,
        user_id: String,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_mounted(backend, user_id, cx)
    }

    /// What the sidebar asked for. Events arrive after the sidebar's own
    /// update finished, so answering them may update it again.
    fn on_sidebar_event(
        &mut self,
        _sidebar: Entity<Sidebar>,
        event: &SidebarEvent,
        cx: &mut Context<Self>,
    ) {
        match event.clone() {
            SidebarEvent::Navigate(request) => cx.emit(request),
            SidebarEvent::RenameProject(request) => cx.emit(request),
            SidebarEvent::TaskAction(request) => cx.emit(request),
            SidebarEvent::ConfirmTaskDelete { target, title } => {
                self.request_delete_computer_task(target, title, cx);
            }
            SidebarEvent::PreferencesChanged => {
                self.refresh_project_caption(cx);
                cx.emit(NavigationChanged);
                cx.notify();
            }
            SidebarEvent::ProjectsChanged(roots) => {
                self.merge_project_metadata(roots, cx);
                cx.notify();
            }
            SidebarEvent::PagesReady {
                sessions,
                generation,
            } => self.apply_session_list(sessions, generation, cx),
            SidebarEvent::ProjectsReady { roots } => {
                self.apply_recent_roots(roots, cx);
                cx.notify();
            }
            SidebarEvent::Select(id) => {
                self.sessions = self.sidebar.read(cx).navigation_sessions();
                self.select_session(&id, cx);
            }
            SidebarEvent::SessionChanged(session) => {
                self.upsert_session(session, cx);
                cx.notify();
            }
            SidebarEvent::SetState { session_id, state } => {
                self.sessions = self.sidebar.read(cx).navigation_sessions();
                self.set_session_state(&session_id, state, cx)
            }
            SidebarEvent::DeleteTask(id) => {
                self.sessions = self.sidebar.read(cx).navigation_sessions();
                self.request_delete_task(&id, cx);
            }
            SidebarEvent::RemoveRoot(root) => self.request_remove_root(&root, cx),
            SidebarEvent::SetTrust { path, trusted } => self.set_project_trust(path, trusted, cx),
            SidebarEvent::ChooseProject => self.choose_root_dialog(cx),
            SidebarEvent::Notice(message) => {
                self.notice = Some(message);
                cx.notify();
            }
        }
    }

    /// Push everything the sidebar shows: sessions, the selection, which
    /// tasks wait, run, or ended unseen, and the recent roots. Called when
    /// any of them changed; the sidebar rebuilds only what moved.
    fn sync_sidebar(&mut self, cx: &mut Context<Self>) {
        let sessions = self.sessions.clone();
        let selected = self.selected_session.clone();
        let activity = self.task_activity();
        let roots = self.recent_roots.clone();
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.set_sessions(sessions, cx);
            sidebar.set_selected(selected, cx);
            sidebar.set_activity(activity, cx);
            sidebar.set_recent_roots(roots, cx);
        });
        self.refresh_selected_title();
        cx.emit(NavigationChanged);
    }

    /// Push only the row indicators: a question arrived or went away.
    /// Other computers' sidebars show them too.
    fn sync_sidebar_activity(&mut self, cx: &mut Context<Self>) {
        let activity = self.task_activity();
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.set_activity(activity, cx));
        cx.emit(NavigationChanged);
    }

    /// Which tasks wait on the user, run, or ended unseen.
    fn task_activity(&self) -> TaskActivity {
        TaskActivity {
            running: self.active_runs.keys().cloned().collect(),
            waiting: self
                .pending_questions
                .iter()
                .map(|question| question.session_id.clone())
                .collect(),
            failed: self.failed_unread_sessions.clone(),
            unread: self.completed_unread_sessions.clone(),
        }
    }

    /// Seeing a task reads how its last run ended. Reports whether that
    /// cleared a marker.
    fn mark_seen(&mut self, session_id: &str) -> bool {
        let completed = self.completed_unread_sessions.remove(session_id);
        let failed = self.failed_unread_sessions.remove(session_id);
        completed || failed
    }

    fn sync_sidebar_selection(&mut self, cx: &mut Context<Self>) {
        let selected = self.selected_session.clone();
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.set_selected(selected, cx));
        self.refresh_selected_title();
    }

    pub(super) fn set_sidebar_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        if self.sidebar_collapsed == collapsed {
            return;
        }
        let now = std::time::Instant::now();
        let from = self
            .sidebar_transition
            .as_ref()
            .map(|transition| transition.value(now))
            .unwrap_or(if self.sidebar_collapsed { 0. } else { 1. });
        let to = if collapsed { 0. } else { 1. };
        self.sidebar_collapsed = collapsed;
        let distance = (to - from).abs();
        self.sidebar_transition =
            (!cx.reduce_motion() && distance > f32::EPSILON).then(|| SidebarTransition {
                from,
                to,
                started: now,
                duration: SIDEBAR_TRANSITION_DURATION.mul_f32(distance),
            });
        if self.popup.is_open(&ChatPopup::Settings) {
            self.popup.close(cx);
        }
        if collapsed {
            self.sidebar.update(cx, |sidebar, cx| {
                sidebar.close_popups(cx);
            });
        }
        cx.notify();
    }

    fn sidebar_expansion(&mut self, window: &mut Window, cx: &Context<Self>) -> f32 {
        if cx.reduce_motion() {
            self.sidebar_transition = None;
        }
        let now = std::time::Instant::now();
        if let Some(transition) = &self.sidebar_transition {
            if now.saturating_duration_since(transition.started) < transition.duration {
                window.request_animation_frame();
                return transition.value(now);
            }
            self.sidebar_transition = None;
        }
        if self.sidebar_collapsed { 0. } else { 1. }
    }

    pub(super) fn toggle_sidebar_visibility(&mut self, cx: &mut Context<Self>) {
        self.set_sidebar_collapsed(!self.sidebar_collapsed, cx);
    }

    pub(super) fn toggle_archived_visibility(&mut self, cx: &mut Context<Self>) {
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.toggle_archived_visibility(cx));
    }

    pub(super) fn focus_sidebar_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_sidebar_collapsed(false, cx);
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.focus_search(window, cx));
        cx.notify();
    }

    pub(super) fn toggle_task_pin(&mut self, session_id: &str, cx: &mut Context<Self>) {
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.toggle_task_pin(session_id, cx));
    }

    #[cfg(test)]
    fn sidebar_active(&self, cx: &gpui::App) -> Vec<usize> {
        self.sidebar.read(cx).active_rows().to_vec()
    }

    #[cfg(test)]
    fn sidebar_settled(&self, cx: &gpui::App) -> Vec<usize> {
        self.sidebar.read(cx).settled_rows().to_vec()
    }

    #[cfg(test)]
    fn sidebar_pinned(&self, cx: &gpui::App) -> Vec<usize> {
        self.sidebar.read(cx).pinned_rows().to_vec()
    }

    #[cfg(test)]
    fn archived_indices(&self, cx: &gpui::App) -> Vec<usize> {
        self.sidebar.read(cx).archived_rows().to_vec()
    }

    #[cfg(test)]
    fn project_root_paths(&self, cx: &gpui::App) -> Vec<String> {
        self.sidebar
            .read(cx)
            .project_root_paths()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[cfg(test)]
    fn set_sidebar_filter(&mut self, filter: &str, cx: &mut Context<Self>) {
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.set_filter_for_test(filter, cx);
            cx.notify();
        });
    }

    /// Stand in for the runtime confirming a state change: set the
    /// session's state as its updated record would and push the list.
    #[cfg(test)]
    fn set_task_state(&mut self, session_id: &str, state: AgentTaskState, cx: &mut Context<Self>) {
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
        {
            session.state = state;
        }
        self.sync_sidebar(cx);
    }

    #[cfg(test)]
    fn settle_task(&mut self, session_id: &str, cx: &mut Context<Self>) {
        self.set_task_state(session_id, AgentTaskState::Settled, cx);
    }

    #[cfg(test)]
    fn unsettle_task(&mut self, session_id: &str, cx: &mut Context<Self>) {
        self.set_task_state(session_id, AgentTaskState::Active, cx);
    }

    #[cfg(test)]
    pub(super) fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.cancel_rename(cx));
    }

    /// Create and wire the composer; called by the real constructor.
    fn attach_composer(&mut self, weak: gpui::WeakEntity<Self>, cx: &mut Context<Self>) {
        let vim_enabled = self.composer_vim_enabled;
        let application_vim_enabled = self.application_vim_enabled;
        let composer = cx.new(|cx| {
            TextInput::new(COMPOSER_PLACEHOLDER, cx)
                .composer_vim(vim_enabled)
                .application_vim(application_vim_enabled)
                .on_vim_leave({
                    let weak = weak.clone();
                    move |window, cx| {
                        if let Some(chat) = weak.upgrade() {
                            chat.update(cx, |chat, cx| {
                                chat.restore_region_from_composer(window, cx)
                            });
                        }
                    }
                })
                .multiline(8)
                .spell_check()
                .on_key({
                    let weak = weak.clone();
                    // This hook runs while the composer entity is being
                    // updated: never read or update the composer here.
                    // Text arrives as an argument; composer writes defer.
                    move |event: &gpui::KeyDownEvent, text: &SharedString, _window, cx| -> bool {
                        let Some(this) = weak.upgrade() else {
                            return false;
                        };
                        let key = event.keystroke.key.clone();
                        let steer_combo = key == "enter"
                            && !event.keystroke.modifiers.shift
                            && !event.keystroke.modifiers.alt
                            && (event.keystroke.modifiers.control
                                || event.keystroke.modifiers.platform);
                        if steer_combo {
                            let text = text.to_string();
                            let weak = weak.clone();
                            cx.defer(move |cx| {
                                weak.update(cx, |chat, cx| chat.steer_text(text, cx)).ok();
                            });
                            return true;
                        }
                        let plain = !event.keystroke.modifiers.control
                            && !event.keystroke.modifiers.alt
                            && !event.keystroke.modifiers.platform
                            && !event.keystroke.modifiers.shift;
                        if !plain {
                            return false;
                        }
                        let token = text.strip_prefix('/').unwrap_or_default();
                        let token_ok = text.starts_with('/')
                            && !token.contains(char::is_whitespace)
                            && !token.contains('/');
                        match key.as_str() {
                            "down" | "up" if token_ok => this.update(cx, |chat, cx| {
                                chat.navigate_slash_palette(&key, token, cx)
                            }),
                            "down" | "up" => {
                                let recalled = this
                                    .update(cx, |chat, _| chat.recall_prompt(&key, text.as_ref()));
                                match recalled {
                                    Some(recalled) => {
                                        let weak = weak.clone();
                                        cx.defer(move |cx| {
                                            weak.update(cx, |chat, cx| {
                                                if let Some(composer) = chat.composer.clone() {
                                                    composer.update(cx, |input, cx| {
                                                        input.set_text(&recalled, cx)
                                                    });
                                                }
                                            })
                                            .ok();
                                        });
                                        true
                                    }
                                    None => false,
                                }
                            }
                            "tab" if token_ok => {
                                let name = this.update(cx, |chat, _| {
                                    let entries = slash_entries_for(token, &chat.slash_commands);
                                    let index = chat
                                        .slash_selected
                                        .filter(|index| *index < entries.len())
                                        .unwrap_or(0);
                                    entries.get(index).map(|entry| entry.name.clone())
                                });
                                if let Some(name) = name {
                                    let weak = weak.clone();
                                    cx.defer(move |cx| {
                                        weak.update(cx, |chat, cx| {
                                            chat.complete_slash_command(&name, cx)
                                        })
                                        .ok();
                                    });
                                    true
                                } else {
                                    false
                                }
                            }
                            _ => {
                                // Typing anything else restarts selection.
                                this.update(cx, |chat, cx| {
                                    if chat.slash_selected.is_some() {
                                        chat.slash_selected = None;
                                        cx.notify();
                                    }
                                });
                                false
                            }
                        }
                    }
                })
                .on_enter({
                    let weak = weak.clone();
                    // The hook runs inside the composer's update; defer the
                    // send so it may read and clear the composer safely.
                    move |text, _, cx| {
                        let weak = weak.clone();
                        cx.defer(move |cx| {
                            if let Some(this) = weak.upgrade() {
                                this.update(cx, |chat, cx| chat.send_text(text.clone(), cx));
                            }
                        });
                    }
                })
                .on_paste_image(move |image, _, cx| {
                    if let Some(this) = weak.upgrade() {
                        this.update(cx, |chat, cx| chat.paste_image(image, cx));
                    }
                })
        });
        cx.observe(&composer, |this, composer, cx| {
            this.composer_changed(&composer, cx);
        })
        .detach();
        self.composer = Some(composer);
    }

    /// The composer text moved: refresh the derived state the render path
    /// reads (send button enablement, slash palette rows).
    fn composer_changed(&mut self, composer: &Entity<TextInput>, cx: &mut Context<Self>) {
        let input = composer.read(cx);
        let text = input.text_ref();
        let has_text = !text.trim().is_empty();
        let entries = match text.strip_prefix('/') {
            Some(token) if !token.contains(char::is_whitespace) && !token.contains('/') => {
                slash_entries_for(token, &self.slash_commands)
            }
            _ => Vec::new(),
        };
        let entries_changed = entries.len() != self.slash_entries.len()
            || entries
                .iter()
                .zip(&self.slash_entries)
                .any(|(next, previous)| next.name != previous.name);
        // The palette just opened: re-read the skills, so one installed
        // since the last scan (a toggle in Settings) is in the list.
        let palette_opened = self.slash_entries.is_empty() && !entries.is_empty();
        if has_text != self.composer_has_text || entries_changed {
            self.composer_has_text = has_text;
            self.slash_entries = entries;
            cx.notify();
        }
        if palette_opened {
            self.refresh_slash_commands(cx);
        }
    }

    /// Test seam: pure state without composer wiring or runtime start.
    pub(crate) fn new_inner(
        backend: Arc<AgentBackend>,
        user_id: String,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_inner_with_placeholder(backend, user_id, cx)
    }

    fn new_inner_with_placeholder(
        backend: Arc<AgentBackend>,
        user_id: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = crate::settings::load_settings();
        let weak = cx.entity().downgrade();
        let sidebar =
            cx.new(|cx| Sidebar::new(backend.clone(), user_id.clone(), weak, &settings, cx));
        cx.subscribe(&sidebar, Self::on_sidebar_event).detach();
        Self {
            backend,
            user_id,
            sidebar,
            computers: Arc::new(Vec::new()),
            navigation_ready: false,
            navigation_only: false,
            visible: true,
            next_navigation_refresh: std::time::Instant::now() + std::time::Duration::from_secs(30),
            pending_navigation: None,
            sessions: Vec::new(),
            selected_session: None,
            timeline: Vec::new(),
            pages: paging::TranscriptPages::default(),
            active_runs: HashMap::new(),
            completed_unread_sessions: HashSet::new(),
            failed_unread_sessions: HashSet::new(),
            finished_runs: std::collections::VecDeque::new(),
            session_setup_pending: false,
            first_send_waiting_for_mcp: false,
            draft: false,
            pending_first_send: None,
            first_send_preview: None,
            sending_previews: HashMap::new(),
            draft_mcp_defaults: None,
            draft_mcp_changes: Vec::new(),
            draft_mcp_generation: 0,
            project_mcp_saves: std::collections::VecDeque::new(),
            project_mcp_save_pending: false,
            pending_questions: Vec::new(),
            pending_question_input: None,
            context_fraction: None,
            usage_poller_session: None,
            usage_poller_generation: 0,
            ledger_context_tokens: 0,
            context_limit: 0,
            composer: None,
            composer_vim_enabled: settings.composer_vim_enabled,
            application_vim_enabled: settings.application_vim_enabled,
            application_vim: ApplicationVimState::default(),
            application_focus: None,
            screen_focus_pending: true,
            composer_has_text: false,
            slash_entries: Vec::new(),
            models: Vec::new(),
            selected_model: None,
            default_model: None,
            thinking: None,
            thinking_cache: thinking::ThinkingCache::default(),
            thinking_loading: false,
            thinking_generation: 0,
            thinking_saving: false,
            notify_enabled: settings.desktop_notifications,
            notify_details: settings.notification_details_enabled(),
            window_active: true,
            sidebar_plan: None,
            runtime_error: None,
            persistence_status: maple_agent::store::PersistenceStatus::default(),
            host_recovery: HostRecovery::default(),
            notice: None,
            booting: true,
            loading_session: None,
            loading_timeline: HashMap::new(),
            snapshot_revision: 0,
            list_state: transcript_list_state(),
            painted_rows: std::rc::Rc::default(),
            tool_details: settings.tool_details,
            project_root: None,
            recent_roots: Vec::new(),
            popup: Popup::new(|this| &mut this.popup, cx),
            dialog_focus: None,
            dialog_focus_pending: false,
            feedback: None,
            trust_prompts: true,
            sidebar_collapsed: false,
            sidebar_transition: None,
            draft_attachments: Vec::new(),
            unresolved_sends: HashMap::new(),
            attachment_uploads: HashMap::new(),
            accept_send_results: true,
            sign_out_pending: false,
            discarded_sends: HashSet::new(),
            checking_sends: false,
            draft_counter: 0,
            attachment_picking: false,
            model_catalog: HashMap::new(),
            model_labels: HashMap::new(),
            models_loading: false,
            controls_held: false,
            controls_revealed: None,
            session_mcp: Vec::new(),
            mcp_enabled_count: 0,
            composer_expanded: false,
            web_enabled: true,
            default_web_enabled: settings.default_web_enabled,
            markdown_cache: MarkdownCache::default(),
            notice_dismiss_pending: std::cell::Cell::new(false),
            derived: DerivedCache::default(),
            markdown_warmup: None,
            timeline_index: HashMap::new(),
            selected_title: DEFAULT_TASK_TITLE.into(),
            project_folder_dialog: None,
            root_picker_open: false,
            root_selecting: false,
            project_label: SharedString::from("Choose folder"),
            project_branch: None,
            branch_label: None,
            branch_watcher: None,
            watched_git_dir: None,
            selection_generation: 0,
            reload_generation: 0,
            attachment_images: HashMap::new(),
            attachment_requests: HashSet::new(),
            attachment_failures: HashSet::new(),
            attachment_queue: std::collections::VecDeque::new(),
            pending_attachments: 0,
            attachment_viewport: 0..0,
            timeline_revisions: HashMap::new(),
            selection: None,
            transcript_focus: None,
            awaiting_first_token: false,
            toggled_tools: HashSet::new(),
            question_selected: HashMap::new(),
            question_step: 0,
            question_step_answers: HashMap::new(),
            question_collapsed: false,
            lightbox: None,
            prompt_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            confirm: None,
            trust_prompt: None,
            trust_saving: false,
            queue: Vec::new(),
            queue_previews: Vec::new(),
            queue_busy: false,
            queue_edit: None,
            update: crate::update::Snapshot::initial(),
            slash_commands: Vec::new(),
            slash_selected: None,
            question_focus_pending: false,
            tool_summaries: HashMap::new(),
            summary_requests: HashSet::new(),
            unsaved_summaries: HashSet::new(),
            summary_inflight: HashMap::new(),
            next_summary_token: 0,
            output_summaries: HashSet::new(),
            #[cfg(test)]
            summary_probe: None,
            summary_queue: std::collections::VecDeque::new(),
            summary_generation: 0,
            summaries_enabled: settings.tool_summaries,
            plan: Vec::new(),
            plan_done: 0,
            plan_hold: None,
            btw: None,
            btw_sequence: 0,
            plan_collapsed: HashSet::new(),
            subagents: Vec::new(),
            subagent_tick_pending: std::cell::Cell::new(false),
            runtime_running_seen: false,
            subagent_epoch: 0,
            audio: Arc::new(crate::audio::AudioEngine::new()),
            audio_caps: maple_agent::agent::AudioCapabilities::default(),
            recording: false,
            recording_starting: false,
            transcribing: false,
            speech: None,
            bridged_tasks: std::cell::RefCell::new(Vec::new()),
            speech_generation: 0,
            tts_voice: settings.tts_voice,
            tts_speed: settings.tts_speed,
        }
    }

    fn call<T, F>(
        &self,
        future: F,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
    ) where
        T: Send + 'static,
        F: std::future::Future<Output = Result<T, String>> + Send + 'static,
    {
        let task = self.backend.spawn(future);
        let bridge = cx.spawn(async move |this, cx| {
            let result = task.await.unwrap_or_else(|error| {
                log::debug!("agent task failed: {error:?}");
                Err("The agent task was cancelled".to_string())
            });
            this.update(cx, |this, cx| then(this, result, cx)).ok();
        });
        // Retain the bridge: the backend's sender holds its waker, and
        // this gpui revision asserts a task is dropped only by the thread
        // that spawned it. ChatScreen lives on one thread, so the RefCell
        // cannot race.
        crate::ui::task::retain(&self.bridged_tasks, bridge);
    }

    /// Sign-in finished: boot the runtime, then load the workspace state.
    /// Boot in two phases. Phase one reads everything local in one backend
    /// call — task list, roots, and the newest transcript — so the screen
    /// fills from disk at once. Phase two starts the runtime, which holds
    /// the lifecycle lock across network round trips; issuing any local
    /// read after it would queue behind that lock.
    fn start(&self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let user = self.user_id.clone();
        self.call(
            async move { backend.navigation_bootstrap(&user).await },
            cx,
            |this, result, cx| {
                match result {
                    Ok(boot) => {
                        this.persistence_status = boot.persistence_status.clone();
                        this.active_runs = boot.runtime_status.active_runs;
                        this.apply_bootstrap(
                            crate::backend::LocalBootstrap {
                                project_root: boot.project_root,
                                sessions: Vec::new(),
                                recent_roots: sidebar::bootstrap_projects(
                                    boot.recent_projects,
                                    boot.default_project,
                                ),
                                latest: None,
                                questions: boot.pending_questions,
                            },
                            HashMap::new(),
                            cx,
                        );
                    }
                    Err(error) => this.notice = Some(error.into()),
                }
                this.refresh_sessions(cx);
                this.finish_navigation_bootstrap(cx);
                this.refresh_slash_commands(cx);
                this.start_runtime(cx);
            },
        );
    }

    /// Take what the local bootstrap read from disk. The newest transcript
    /// opens only while nothing is on screen: a click that landed before
    /// this callback wins, and so does a draft the user started while the
    /// app was still loading, whose text belongs to the task it creates.
    fn apply_bootstrap(
        &mut self,
        boot: crate::backend::LocalBootstrap,
        summaries: HashMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        let workspace = boot
            .recent_roots
            .iter()
            .find(|root| root.is_default_workspace)
            .cloned();
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.reconcile_default_project(workspace, cx)
        });
        self.handle_service_events(boot.questions, cx);
        if !self.set_project_context(boot.project_root, cx) {
            self.project_root_changed(cx);
            self.check_project_trust(cx);
        }
        self.apply_recent_roots(boot.recent_roots, cx);
        self.sessions = boot.sessions;
        self.sync_sidebar(cx);
        if let Some(detail) = boot
            .latest
            .filter(|_| self.selected_session.is_none() && !self.draft)
        {
            let summaries = summaries
                .into_iter()
                .map(|(id, summary)| (id, SharedString::from(summary)))
                .collect();
            self.upsert_session(detail.session.clone(), cx);
            self.set_active_session(detail.session, detail.timeline, summaries, cx);
            self.queue = detail.queue.items;
        }
        cx.notify();
    }

    /// Phase two of `start`: bring the agent runtime up and fill in what
    /// needs the network (models, plan, audio, trust of a changed root).
    fn start_runtime(&self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let request = backend.default_start_request();
        // A task or project selected while the start is in flight owns the
        // visible project context.
        let generation = self.selection_generation;
        let recovery_epoch = self.host_recovery.epoch;
        self.call(
            async move { backend.start_runtime(&user_id, Some(request)).await },
            cx,
            move |this, result, cx| {
                if this.host_recovery.epoch != recovery_epoch {
                    return;
                }
                match result {
                    Ok(status) => {
                        log::debug!(
                            "startup: runtime started at {} ms",
                            crate::startup_elapsed()
                        );
                        this.runtime_error = None;
                        // Runtime root is only the fallback used at startup.
                        // A draft may have selected its folder before this
                        // start, including a catalog's first remote activation.
                        if !this.draft
                            && this.selected_session.is_none()
                            && this.selection_generation == generation
                        {
                            this.set_project_context(status.project_root, cx);
                        }
                    }
                    Err(message) => {
                        this.runtime_error =
                            Some(format!("Runtime failed to start: {message}").into())
                    }
                }
                this.booting = false;
                cx.emit(NavigationChanged);
                this.dispatch_prepared_uploads(cx);
                cx.notify();
                this.refresh_models(cx);
                this.refresh_roots(cx);
                let generation = this.selection_generation;
                this.sidebar.update(cx, |sidebar, cx| {
                    sidebar.refresh_visible_pages_for_generation(generation, cx)
                });
                this.refresh_sidebar_plan(cx);
                this.refresh_audio_capabilities(cx);
                // Summary requests issued before the runtime was up failed
                // and unregistered themselves; ask again for what is still
                // missing.
                this.summarize_loaded_tools(cx);
            },
        );
    }

    fn refresh_audio_capabilities(&self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.audio_capabilities(&user_id).await },
            cx,
            |this, result, cx| match result {
                Ok(caps) => {
                    if caps != this.audio_caps {
                        this.audio_caps = caps;
                        cx.notify();
                    }
                }
                Err(message) => log::debug!("audio capabilities unavailable: {message}"),
            },
        );
    }

    fn refresh_roots(&self, cx: &mut Context<Self>) {
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.refresh_project_roots(cx));
    }

    /// Replace the currently loaded project page membership, retaining newer
    /// owner-issued names when a late read arrives after a rename event.
    fn apply_recent_roots(&mut self, mut roots: Vec<RecentProjectRoot>, cx: &mut Context<Self>) {
        self.sidebar
            .read(cx)
            .preserve_newer_project_names(&mut roots);
        if self.recent_roots != roots {
            self.recent_roots = roots;
            self.sync_sidebar(cx);
            self.refresh_project_caption(cx);
        }
    }

    /// Project events and mutation replies carry point updates, not a catalog.
    fn merge_project_metadata(
        &mut self,
        mut roots: Vec<RecentProjectRoot>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.sidebar
            .read(cx)
            .preserve_newer_project_names(&mut roots);
        let mut changed = false;
        for root in &roots {
            if let Some(existing) = self
                .recent_roots
                .iter_mut()
                .find(|row| row.path == root.path)
                && existing != root
            {
                *existing = root.clone();
                changed = true;
            }
        }
        changed |= self
            .sidebar
            .update(cx, |sidebar, cx| sidebar.merge_project_metadata(roots, cx));
        self.refresh_project_caption(cx);
        if changed {
            cx.emit(NavigationChanged);
        }
        changed
    }

    fn restore_selected_project(&mut self, project: RecentProjectRoot, cx: &mut Context<Self>) {
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.restore_project(project.clone(), cx);
        });
        if !self
            .recent_roots
            .iter()
            .any(|root| root.path == project.path)
        {
            self.recent_roots.push(project.clone());
        }
        self.merge_project_metadata(vec![project], cx);
        cx.emit(NavigationChanged);
    }

    /// Select the project context for new tasks without disturbing work that
    /// is already running in any session.
    fn select_project_root(&mut self, path: String, cx: &mut Context<Self>) {
        if self.root_selecting {
            return;
        }
        // The new-task screen's first send is creating its task in the
        // visible project. Switching now would move the draft away from
        // the task its message is going to; the create is short, so wait.
        if self.selected_session.is_none() && self.session_setup_pending {
            self.notice = Some("Wait for the new task to be created, then switch projects".into());
            cx.notify();
            return;
        }
        let path = path.trim().to_string();
        if path.is_empty()
            || (self.backend.remote_host().is_none() && !std::path::Path::new(&path).is_absolute())
        {
            self.notice = Some("Enter an absolute directory path".into());
            cx.notify();
            return;
        }
        self.root_selecting = true;
        // Project selection is navigation just like selecting a task. The
        // generation moves only when the selection lands, so a registration
        // that fails leaves loads in flight alive, while a task clicked after
        // this point advances the generation and wins over the callback.
        let selection_generation = self.selection_generation;
        self.popup.close(cx);
        self.project_folder_dialog = None;
        self.notice = None;
        cx.notify();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.select_project_root(&user_id, path).await },
            cx,
            move |this, result, cx| {
                this.root_selecting = false;
                match result {
                    Ok(registration) => {
                        this.restore_selected_project(registration.project, cx);
                        this.refresh_roots(cx);
                        this.land_project_selection(
                            registration.project_root,
                            selection_generation,
                            cx,
                        );
                    }
                    Err(message) => {
                        this.notice = Some(message.into());
                    }
                }
                cx.notify();
            },
        );
    }

    /// The project registration landed. A task or project selected since
    /// wins; otherwise the project becomes the visible context. A draft
    /// moves with it: its text and chips stay, and the list refresh must
    /// not open that project's latest task over the message being typed.
    fn land_project_selection(
        &mut self,
        project_root: String,
        selection_generation: u64,
        cx: &mut Context<Self>,
    ) {
        if self.selection_generation != selection_generation {
            // Registration still succeeded, so the project stays listed
            // without overriding the newer navigation intent.
            return;
        }
        self.begin_navigation();
        if !self.draft {
            self.clear_selected_session_presentation(cx);
        }
        self.set_project_context(Some(project_root), cx);
        // Starting a draft changes no task inventory. Reloading every host's
        // sidebar here discards its prepared rows on every workspace button.
        if !self.draft {
            let generation = self.selection_generation;
            self.sidebar.update(cx, |sidebar, cx| {
                sidebar.refresh_visible_pages_for_generation(generation, cx)
            });
        }
    }

    /// Persist `root` as the default for new tasks and the next launch
    /// without changing what is on screen. Used when a task under another
    /// project is opened and when a project is archived.
    fn persist_project_root(&mut self, root: String, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.select_project_root(&user_id, root).await },
            cx,
            |this, result, cx| match result {
                Ok(registration) => {
                    this.restore_selected_project(registration.project, cx);
                    this.refresh_roots(cx);
                    cx.notify();
                }
                Err(error) => log::warn!("Cannot persist project root: {error}"),
            },
        );
    }

    /// Start a navigation that supersedes every task load in flight.
    fn begin_navigation(&mut self) {
        self.project_folder_dialog = None;
        self.selection_generation += 1;
        self.reload_generation += 1;
    }

    /// Adopt a project as the visible task context. This changes only UI
    /// state; the account runtime and every active run remain untouched.
    /// Returns whether the context changed.
    fn set_project_context(
        &mut self,
        project_root: Option<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.project_root == project_root {
            return false;
        }
        self.project_root = project_root;
        if self.draft {
            self.draft_mcp_defaults = None;
            self.draft_mcp_changes
                .retain(|change| !is_custom_mcp(&change.name, change.kind));
            let rows = self
                .session_mcp
                .iter()
                .filter(|row| !is_custom_mcp(&row.name, row.kind))
                .cloned()
                .collect();
            self.set_session_mcp(rows);
            self.refresh_draft_mcp(cx);
        }
        self.project_root_changed(cx);
        self.check_project_trust(cx);
        self.refresh_slash_commands(cx);
        self.sync_sidebar(cx);
        true
    }

    fn choose_root_dialog(&mut self, cx: &mut Context<Self>) {
        self.choose_root_dialog_with_intent(self.navigation_only, cx);
    }

    fn choose_root_dialog_with_intent(&mut self, draft_on_confirm: bool, cx: &mut Context<Self>) {
        if self.root_picker_open || self.project_folder_dialog.is_some() || self.root_selecting {
            return;
        }
        if self.selected_session.is_none() && self.session_setup_pending {
            self.notice = Some("Wait for the new task to be created, then switch projects".into());
            cx.notify();
            return;
        }
        self.popup.close(cx);
        if self.backend.remote_host().is_none() {
            self.choose_native_root_dialog(draft_on_confirm, cx);
        } else {
            self.show_folder_browser(draft_on_confirm, cx);
        }
    }

    fn choose_native_root_dialog(&mut self, draft_on_confirm: bool, cx: &mut Context<Self>) {
        // Use the platform picker: NSOpenPanel, Windows common dialog, or
        // Linux portal. It is asynchronous and closes with the application.
        self.root_picker_open = true;
        let generation = self.selection_generation;
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        let bridge = cx.spawn(async move |this, cx| {
            let picked = receiver.await;
            this.update(cx, |this, cx| {
                this.root_picker_open = false;
                if this.selection_generation != generation || !this.visible {
                    cx.notify();
                    return;
                }
                match picked {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            let Some(path) = path.to_str().map(str::to_owned) else {
                                this.notice = Some("This folder path is not valid Unicode".into());
                                cx.notify();
                                return;
                            };
                            let backend = this.backend.clone();
                            let user_id = this.user_id.clone();
                            this.root_selecting = true;
                            this.call(
                                async move { backend.select_project_root(&user_id, path).await },
                                cx,
                                move |this, result, cx| {
                                    this.root_selecting = false;
                                    if this.selection_generation == generation && this.visible {
                                        match result {
                                            Ok(registration) => this.finish_folder_selection(
                                                registration,
                                                draft_on_confirm,
                                                cx,
                                            ),
                                            Err(error) => this.notice = Some(error.into()),
                                        }
                                    }
                                    cx.notify();
                                },
                            );
                        }
                    }
                    // Cancellation leaves the task, project and draft untouched.
                    Ok(Ok(None)) | Err(_) => {}
                    // A Linux desktop without a portal still has a usable chooser.
                    Ok(Err(_)) => this.show_folder_browser(draft_on_confirm, cx),
                }
                cx.notify();
            })
            .ok();
        });
        crate::ui::task::retain(&self.bridged_tasks, bridge);
        cx.notify();
    }

    fn finish_folder_selection(
        &mut self,
        registration: crate::backend::SelectedProject,
        draft_on_confirm: bool,
        cx: &mut Context<Self>,
    ) {
        if draft_on_confirm && !self.draft {
            self.new_session(cx);
        }
        self.restore_selected_project(registration.project, cx);
        self.refresh_roots(cx);
        self.land_project_selection(registration.project_root, self.selection_generation, cx);
        self.leave_catalog(cx);
        self.screen_focus_pending = true;
    }

    fn show_folder_browser(&mut self, draft_on_confirm: bool, cx: &mut Context<Self>) {
        if self.project_folder_dialog.is_some() {
            return;
        }
        self.popup.close(cx);
        let computer = self
            .computers
            .iter()
            .find(|computer| computer.key.as_deref() == self.backend.remote_host())
            .map(|computer| computer.label.clone())
            .unwrap_or_else(|| {
                if self.backend.remote_host().is_some() {
                    "Remote computer".into()
                } else {
                    "This Computer".into()
                }
            });
        let dialog = cx.new(|cx| {
            project_folder::ProjectFolderDialog::new(
                self.backend.clone(),
                self.user_id.clone(),
                computer,
                self.application_vim_enabled,
                cx,
            )
        });
        let generation = self.selection_generation;
        cx.subscribe(&dialog, move |this, dialog, event, cx| {
            if this.project_folder_dialog.as_ref() != Some(&dialog) {
                return;
            }
            this.project_folder_dialog = None;
            if this.selection_generation != generation {
                this.screen_focus_pending = true;
                cx.notify();
                return;
            }
            if let project_folder::FolderDialogEvent::Selected(registration) = event {
                this.finish_folder_selection(registration.clone(), draft_on_confirm, cx);
            }
            cx.notify();
        })
        .detach();
        dialog.update(cx, |dialog, cx| dialog.open_home(cx));
        self.project_folder_dialog = Some(dialog);
        cx.notify();
    }

    /// The root changed: update the header label, then read its branch
    /// and watch its git dir.
    fn project_root_changed(&mut self, cx: &mut Context<Self>) {
        self.refresh_project_caption(cx);
        self.refresh_branch(cx);
    }

    /// Read the branch for the current root, or clear it when there is
    /// no root. The read also resolves the git dir, and the watcher is
    /// replaced when that dir changed.
    fn refresh_branch(&mut self, cx: &mut Context<Self>) {
        if self.project_root.is_some() {
            self.read_branch(cx);
            return;
        }
        self.branch_watcher = None;
        self.watched_git_dir = None;
        self.set_branch(None, cx);
    }

    fn set_branch(&mut self, branch: Option<String>, cx: &mut Context<Self>) {
        if self.project_branch == branch {
            return;
        }
        self.branch_label = branch
            .as_deref()
            .map(|branch| SharedString::from(format!("({branch})")));
        self.project_branch = branch;
        cx.notify();
    }

    /// Watch `git_dir` and re-read the branch when `HEAD` changes. The
    /// watch is on the directory, not the file: git replaces `HEAD` by
    /// rename, so a watch on the file itself is lost after the first
    /// checkout. Non-recursive, so a busy `objects/` tree costs nothing.
    /// Events arrive on the watcher's own thread and cross to the UI
    /// through a channel, like backend events. Dropping the watcher
    /// closes the channel, which ends the receiver task.
    fn watch_branch(&mut self, git_dir: &std::path::Path, cx: &mut Context<Self>) {
        use notify::Watcher as _;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher =
            match notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else { return };
                if head_change_event(&event) {
                    tx.send(()).ok();
                }
            }) {
                Ok(watcher) => watcher,
                Err(error) => {
                    log::debug!("branch watcher unavailable: {error}");
                    return;
                }
            };
        if let Err(error) = watcher.watch(git_dir, notify::RecursiveMode::NonRecursive) {
            log::debug!("cannot watch {}: {error}", git_dir.display());
            return;
        }
        self.branch_watcher = Some(watcher);
        cx.spawn(async move |this, cx| {
            while rx.recv().await.is_some() {
                // A rebase or a checkout touches HEAD several times in a
                // row; one read per burst is enough.
                while rx.try_recv().is_ok() {}
                if this.update(cx, |this, cx| this.read_branch(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Read the branch for the current root off the UI thread. The git
    /// dir comes back with it so the watcher follows a root change without
    /// a file stat on the UI thread.
    fn read_branch(&mut self, cx: &mut Context<Self>) {
        if self.backend.remote_host().is_some() {
            self.branch_watcher = None;
            self.set_branch(None, cx);
            return;
        }
        let Some(root) = self.project_root.clone() else {
            return;
        };
        self.call(
            async move {
                tokio::task::spawn_blocking(move || {
                    let git_dir = git_dir(std::path::Path::new(&root));
                    let branch = git_dir.as_deref().and_then(git_branch);
                    Ok((root, git_dir, branch))
                })
                .await
                .map_err(|error| format!("Branch lookup failed: {error}"))?
            },
            cx,
            |this, result, cx| {
                // Drop a late answer for a root that is no longer current.
                let Ok((root, git_dir, branch)) = result else {
                    return;
                };
                if this.project_root.as_deref() != Some(root.as_str()) {
                    return;
                }
                if this.watched_git_dir != git_dir {
                    this.branch_watcher = None;
                    if let Some(dir) = &git_dir {
                        this.watch_branch(dir, cx);
                    }
                    this.watched_git_dir = git_dir;
                }
                this.set_branch(branch, cx);
            },
        );
    }

    /// Open or close the project menu from the header chip.
    pub fn toggle_root_menu(&mut self, cx: &mut Context<Self>) {
        self.popup.toggle(ChatPopup::Project, cx);
    }

    /// Open or close one of the composer chips' menus. Opening the
    /// integrations menu refreshes the task's servers.
    pub(super) fn toggle_popup(&mut self, popup: ChatPopup, cx: &mut Context<Self>) {
        if self.popup.toggle(popup, cx) && popup == ChatPopup::Integrations {
            self.refresh_session_mcp(cx);
        }
    }

    /// Text for the header project chip.
    pub fn project_label(&self, cx: &gpui::App) -> String {
        self.project_root
            .as_deref()
            .map(|root| self.sidebar.read(cx).root_name(root))
            .unwrap_or_else(|| "Choose folder".to_string())
    }

    fn refresh_models(&mut self, cx: &mut Context<Self>) {
        self.models_loading = true;
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let env_model = backend.configured_model();
        self.call(
            async move {
                // Ids, labels and the saved pick land in one update, so the
                // chip never shows a raw id before its label.
                let catalog = backend.model_catalog(&user_id).await.ok();
                let saved = backend.saved_model(&user_id).await;
                let models = backend.available_model_ids(&user_id).await?;
                Ok((models, saved, catalog))
            },
            cx,
            move |this, result, cx| {
                this.models_loading = false;
                if let Ok((mut models, saved, catalog)) = result {
                    if let Some(catalog) = &catalog {
                        this.model_labels = composer::catalog_model_labels(catalog);
                        // An older execution computer lists every API model,
                        // or every catalog model; offer only what the catalog
                        // describes and does not keep out of Maple Work.
                        let described = |model: &String| {
                            catalog.data.iter().any(|entry| {
                                &entry.id == model && entry.available_in_agent != Some(false)
                            })
                        };
                        if models.iter().any(described) {
                            models.retain(described);
                        }
                    }
                    if this.default_model.is_none() {
                        // Env override wins, then the user's last pick while
                        // it is still offered, then the host's default, which
                        // it lists first.
                        let offered = |model: &String| models.is_empty() || models.contains(model);
                        this.default_model = env_model
                            .or(saved.filter(offered))
                            .or_else(|| models.first().cloned());
                    }
                    if this.selected_model.is_none() {
                        this.selected_model = this.default_model.clone();
                    }
                    this.models = models;
                    this.refresh_model_catalog(cx);
                    this.refresh_thinking(cx);
                }
                cx.notify();
            },
        );
    }

    fn refresh_sessions(&self, cx: &mut Context<Self>) {
        let generation = self.selection_generation;
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.refresh_pages_for_generation(generation, cx)
        });
    }

    /// Take a fresh session list. With nothing on screen, open the latest
    /// task of the visible project or show the new-task screen, but only
    /// when no task or project was selected since the list was requested:
    /// a click whose load is still in flight leaves the selection empty
    /// too, and the auto-select would supersede it. A draft is not
    /// "nothing on screen": the list that lands after "New Task" (the boot
    /// refresh, a project switch) must not open a task under the text
    /// being typed.
    fn apply_session_list(
        &mut self,
        sessions: Vec<AgentSessionSummary>,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        let selected = self.selected_session.clone();
        let retained = self
            .sessions
            .iter()
            .find(|s| Some(&s.id) == selected.as_ref())
            .cloned();
        self.sessions = sessions;
        if let Some(retained) = retained
            && !self.sessions.iter().any(|s| s.id == retained.id)
        {
            self.sessions.push(retained);
        }
        self.sync_sidebar(cx);
        if self.selected_session.is_some()
            || self.draft
            || self.selection_generation != generation
            || self.project_folder_dialog.is_some()
            || self.root_picker_open
        {
            return;
        }
        let root = self.project_root.clone();
        let latest = self
            .sessions
            .iter()
            .find(|session| {
                // An empty task (a draft an older build persisted) is not
                // worth opening; the new-task screen is the same thing.
                session.state != AgentTaskState::Archived
                    && session.message_count > 0
                    && Some(&session.project_root) == root.as_ref()
            })
            .map(|session| session.id.clone());
        match latest {
            Some(id) => self.select_session(&id, cx),
            None => self.new_session(cx),
        }
    }

    /// "New Task": show the empty screen for the visible project and
    /// create nothing. The task is created the moment the first message
    /// is sent, so a project switched before then moves it, and a click
    /// that sends nothing leaves no empty row behind.
    pub(super) fn new_session(&mut self, cx: &mut Context<Self>) {
        // The first send is creating its task; a click now would
        // supersede it and lose the message.
        if self.session_setup_pending {
            return;
        }
        if self.root_selecting {
            // The visible project is about to change; a draft started now
            // would supersede the selection landing.
            self.notice = Some("Wait for the project selection to finish, then try again".into());
            cx.notify();
            return;
        }
        // Starting a draft is navigation: a task load in flight must not
        // land on top of the empty screen. It works while the runtime
        // boots too (no task is needed); the boot's own auto-select then
        // yields to it.
        self.begin_navigation();
        self.loading_session = None;
        self.clear_selected_session_presentation(cx);
        self.draft = true;
        // The draft starts from this project's defaults.
        self.web_enabled = self.default_web_enabled;
        self.refresh_draft_mcp(cx);
        self.refresh_selected_title();
        cx.notify();
    }

    /// Create the task the draft describes, then run `action` against it.
    /// The text stays in the composer, which shows the create in
    /// progress, until the send goes out; a create that does not happen
    /// leaves it there.
    fn create_for_first_send(&mut self, action: FirstSend, cx: &mut Context<Self>) {
        self.slash_selected = None;
        // A command path cleared the composer before it got here; put the
        // message back on show.
        self.fill_empty_composer(&action.text(), cx);
        if self.root_selecting {
            self.notice = Some("Wait for the project selection to finish, then try again".into());
            cx.notify();
            return;
        }
        let Some(request) = self.new_session_request() else {
            self.notice = Some("Choose a project before creating a task".into());
            cx.notify();
            return;
        };
        self.first_send_preview = matches!(action, FirstSend::Message { .. }).then(|| {
            sending::SendPreview::new(None, &action.text(), &self.draft_attachments).status_only()
        });
        self.pending_first_send = Some(action);
        cx.notify();
        if self.draft
            && self.draft_mcp_defaults.is_none()
            && (!self.draft_mcp_changes.is_empty()
                || self
                    .project_mcp_saves
                    .iter()
                    .any(|save| self.project_root.as_deref() == Some(save.root.as_str())))
        {
            // A fresh or recovering draft must include local choices even
            // when their save is pending or failed during a connection loss.
            self.session_setup_pending = true;
            self.first_send_waiting_for_mcp = true;
            self.refresh_draft_mcp(cx);
            return;
        }
        self.create_session(request, cx);
    }

    fn composer_text(&self, cx: &Context<Self>) -> Option<String> {
        self.composer
            .as_ref()
            .map(|composer| composer.read(cx).text())
    }

    /// Show `text` in the composer unless it already holds something.
    /// Returns whether it was shown.
    fn fill_empty_composer(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        let Some(composer) = self.composer.clone() else {
            return false;
        };
        if !composer.read(cx).text_ref().trim().is_empty() {
            return false;
        }
        composer.update(cx, |input, cx| input.set_text(text, cx));
        true
    }

    /// A first send that will not happen: the message goes to the prompt
    /// history, and back into the composer when that is empty. Returns
    /// whether the composer shows it. The composer belongs to whatever is
    /// on screen; a message for a draft the user left does not replace
    /// what they typed since, Up recalls it instead.
    fn give_back_first_send(&mut self, action: FirstSend, cx: &mut Context<Self>) -> bool {
        self.first_send_preview = None;
        let text = action.text();
        self.remember_prompt(&text);
        self.fill_empty_composer(&text, cx)
    }

    /// Another task is opened while the first send is still creating its
    /// task. The message is not sent, and it does not follow the user: it
    /// leaves the composer for the prompt history at once, so the task
    /// opened never shows it and the late create has nothing to put back
    /// (its task is deleted when it lands).
    fn withdraw_first_send(&mut self, cx: &mut Context<Self>) {
        self.first_send_preview = None;
        if self.first_send_waiting_for_mcp {
            self.first_send_waiting_for_mcp = false;
            self.session_setup_pending = false;
        }
        let Some(action) = self.pending_first_send.take() else {
            return;
        };
        let text = self
            .composer_text(cx)
            .filter(|shown| !shown.trim().is_empty())
            .unwrap_or_else(|| action.text());
        self.remember_prompt(&text);
        if let Some(composer) = self.composer.clone() {
            composer.update(cx, |input, cx| input.clear(cx));
        }
        self.notice = Some(
            "Message not sent: another task was opened first. Press Up in the composer to recall it"
                .into(),
        );
        cx.notify();
    }

    /// Run the send the task was created for, against the task now on
    /// screen. The composer showed the message while the task was being
    /// created, so what it holds now goes out, edits included; only an
    /// emptied composer falls back to the message as typed.
    fn run_first_send(&mut self, action: FirstSend, cx: &mut Context<Self>) {
        self.first_send_preview = None;
        self.notice = None;
        let text = self
            .composer_text(cx)
            .filter(|shown| !shown.trim().is_empty())
            .unwrap_or_else(|| action.text());
        self.send_text_with(text, action.steer(), cx);
    }

    /// Create a task from `request`. What the composer shows and the
    /// request cannot carry is applied once the task exists, before the
    /// first send (`settle_new_session`).
    fn create_session(&mut self, request: AgentCreateSessionRequest, cx: &mut Context<Self>) {
        // Creating a task is a navigation intent, but the generation moves
        // only when the task lands: a failed create leaves loads in flight
        // alive, and a task or project selected meanwhile supersedes the
        // eventual callback.
        let selection_generation = self.selection_generation;
        self.session_setup_pending = true;
        let backend = self.backend.clone();
        // Reserve this before create can be delayed. A newer task selection
        // must keep its subscription even if this create finishes afterward.
        let subscription_intent = backend.next_subscription_intent();
        let user_id = self.user_id.clone();
        let task_mcp = self.created_task_mcp(request.mcp_server_names.as_deref());
        self.call(
            async move {
                backend
                    .create_session(&user_id, Some(request))
                    .await
                    .map(|detail| detail.session)
            },
            cx,
            move |this, result, cx| match result {
                Ok(session) => {
                    this.subscribe_new_session(
                        session,
                        task_mcp,
                        selection_generation,
                        subscription_intent,
                        cx,
                    );
                }
                Err(message) => this.fail_new_session(message, cx),
            },
        );
    }

    /// A create does not select the connection's transcript subscription. Wait
    /// for that acknowledgement before any first send can emit message rows.
    fn subscribe_new_session(
        &mut self,
        session: AgentSessionSummary,
        task_mcp: Vec<DraftMcpChange>,
        selection_generation: u64,
        subscription_intent: u64,
        cx: &mut Context<Self>,
    ) {
        if self.selection_generation != selection_generation {
            self.abandon_new_session(session, cx);
            return;
        }
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let session_id = session.id.clone();
        self.call(
            async move {
                backend
                    .subscribe_task(&user_id, &session_id, subscription_intent)
                    .await
            },
            cx,
            move |this, result, cx| match result {
                Ok(()) => {
                    this.settle_new_session(session, task_mcp, selection_generation, cx);
                }
                Err(message) => this.fail_new_session_setup(
                    session,
                    format!("Could not subscribe to task history: {message}"),
                    selection_generation,
                    cx,
                ),
            },
        );
    }

    /// What a task just created from `requested` has for each row of the
    /// draft's integrations chip: the servers named in the request (or
    /// the defaults when it named none), and external agents off, which
    /// no request can switch on.
    fn created_task_mcp(&self, requested: Option<&[String]>) -> Vec<DraftMcpChange> {
        let defaults = self.draft_mcp_defaults.as_deref().unwrap_or(&[]);
        self.session_mcp
            .iter()
            .map(|row| {
                let enabled = match row.kind {
                    AgentSessionIntegrationKind::Mcp => match requested {
                        Some(names) => names
                            .iter()
                            .any(|name| same_integration(name, row.kind, &row.name, row.kind)),
                        None => defaults
                            .iter()
                            .find(|default| {
                                same_integration(&default.name, default.kind, &row.name, row.kind)
                            })
                            .is_some_and(|default| default.enabled),
                    },
                    AgentSessionIntegrationKind::ExternalAgent => false,
                };
                DraftMcpChange {
                    name: row.name.clone(),
                    kind: row.kind,
                    enabled,
                }
            })
            .collect()
    }

    /// The settings the composer shows that `session`, just created, does
    /// not have: web access and the integration rows whose switch differs
    /// from `task_mcp`. Chips flipped while the create was in flight show
    /// up here like any other.
    fn new_session_changes(
        &self,
        session: &AgentSessionSummary,
        task_mcp: &[DraftMcpChange],
    ) -> NewSessionChanges {
        NewSessionChanges {
            web: (self.web_enabled != session.web_enabled).then_some(self.web_enabled),
            mcp: self
                .session_mcp
                .iter()
                .filter(|row| {
                    let has = task_mcp
                        .iter()
                        .find(|state| {
                            same_integration(&state.name, state.kind, &row.name, row.kind)
                        })
                        .is_some_and(|state| state.enabled);
                    row.enabled != has
                })
                .map(|row| DraftMcpChange {
                    name: row.name.clone(),
                    kind: row.kind,
                    enabled: row.enabled,
                })
                .collect(),
        }
    }

    /// The task exists. Apply what the composer shows and the task does
    /// not have yet, then re-check (a chip may have moved meanwhile), and
    /// only once everything has applied select the task and run the first
    /// send. Nothing is sent on a failure: the task is kept and opened,
    /// the text stays in the composer, and the error is shown, so a task
    /// meant to run without web access or a server never runs with it.
    fn settle_new_session(
        &mut self,
        session: AgentSessionSummary,
        task_mcp: Vec<DraftMcpChange>,
        selection_generation: u64,
        cx: &mut Context<Self>,
    ) {
        if self.selection_generation != selection_generation {
            // The user left; the draft's chips no longer describe this task.
            self.finish_new_session(session, selection_generation, cx);
            return;
        }
        let changes = self.new_session_changes(&session, &task_mcp);
        if changes.is_empty() {
            self.finish_new_session(session, selection_generation, cx);
            return;
        }
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let fallback = session.clone();
        let applied = changes.mcp.clone();
        self.call(
            async move {
                let mut session = session;
                if let Some(enabled) = changes.web {
                    session = backend
                        .set_session_web_enabled(&user_id, &session.id, enabled)
                        .await
                        .map_err(|message| format!("Could not change web access: {message}"))?;
                }
                for change in &changes.mcp {
                    backend
                        .set_session_mcp_server_enabled(
                            &user_id,
                            &session.id,
                            &change.name,
                            change.kind,
                            change.enabled,
                        )
                        .await
                        .map_err(|message| {
                            format!("Could not change {}: {message}", change.name)
                        })?;
                }
                Ok(session)
            },
            cx,
            move |this, result, cx| match result {
                Ok(session) => {
                    let mut task_mcp = task_mcp;
                    for change in applied {
                        task_mcp.retain(|state| {
                            !same_integration(&state.name, state.kind, &change.name, change.kind)
                        });
                        task_mcp.push(change);
                    }
                    this.settle_new_session(session, task_mcp, selection_generation, cx);
                }
                Err(message) => {
                    this.fail_new_session_setup(fallback, message, selection_generation, cx)
                }
            },
        );
    }

    /// The create failed: nothing exists, so the draft stays as it was,
    /// with the message still in the composer (or, if the user left the
    /// draft meanwhile, in the history).
    fn fail_new_session(&mut self, message: String, cx: &mut Context<Self>) {
        self.session_setup_pending = false;
        self.first_send_waiting_for_mcp = false;
        self.notice = Some(message.into());
        if let Some(action) = self.pending_first_send.take() {
            self.give_back_first_send(action, cx);
        }
        cx.notify();
    }

    /// A composer setting did not apply to the task just created. The
    /// task exists, so it opens (its chips then show what it really has),
    /// but nothing is sent: the text goes back to the composer with the
    /// error, for the user to fix the setting and send again.
    fn fail_new_session_setup(
        &mut self,
        session: AgentSessionSummary,
        message: String,
        selection_generation: u64,
        cx: &mut Context<Self>,
    ) {
        if self.selection_generation != selection_generation {
            // The user left meanwhile: the task is as unwanted as one
            // whose create lands late.
            self.abandon_new_session(session, cx);
            return;
        }
        self.session_setup_pending = false;
        self.upsert_session(session.clone(), cx);
        self.begin_navigation();
        self.set_active_session(session, Vec::new(), HashMap::new(), cx);
        if let Some(action) = self.pending_first_send.take() {
            self.give_back_first_send(action, cx);
        }
        self.notice = Some(message.into());
        cx.notify();
    }

    fn finish_new_session(
        &mut self,
        session: AgentSessionSummary,
        selection_generation: u64,
        cx: &mut Context<Self>,
    ) {
        if self.selection_generation != selection_generation {
            // Creation still succeeded, but an older callback must never
            // override a newer task or project choice.
            self.abandon_new_session(session, cx);
            return;
        }
        self.session_setup_pending = false;
        // The SessionCreated event may arrive before this callback; upsert
        // so the sidebar never shows the task twice.
        self.upsert_session(session.clone(), cx);
        self.begin_navigation();
        self.set_active_session(session, Vec::new(), HashMap::new(), cx);
        if let Some(action) = self.pending_first_send.take() {
            self.run_first_send(action, cx);
        }
    }

    /// The task landed after the user left the draft: a task or project
    /// was opened meanwhile, so the message has no screen to go to and
    /// is not sent. It goes back to the composer when that is empty and
    /// to the history either way, and the empty task is deleted rather
    /// than left in the list.
    fn abandon_new_session(&mut self, session: AgentSessionSummary, cx: &mut Context<Self>) {
        self.session_setup_pending = false;
        if let Some(action) = self.pending_first_send.take() {
            let shown = self.give_back_first_send(action, cx);
            self.notice = Some(
                if shown {
                    "Message not sent: another task was opened first. It is back in the composer"
                } else {
                    "Message not sent: another task was opened first. Press Up in the composer to recall it"
                }
                .into(),
            );
        }
        self.discard_new_session(session, cx);
        // If the project choice left the view empty while the
        // one-create-at-a-time fence was held, let it settle now.
        self.sync_sidebar(cx);
        if self.selected_session.is_none() && !self.draft {
            self.refresh_sessions(cx);
        }
        cx.notify();
    }

    /// Delete a task created for a message that will not be sent. Its row
    /// goes when the runtime confirms; a delete the runtime refuses leaves
    /// the task listed as what it is rather than hidden.
    fn discard_new_session(&mut self, session: AgentSessionSummary, cx: &mut Context<Self>) {
        // The SessionCreated event may have listed it already; one row
        // until the delete confirms.
        self.upsert_session(session.clone(), cx);
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let session_id = session.id;
        let deleted = session_id.clone();
        self.call(
            async move { backend.delete_session(&user_id, &session_id).await },
            cx,
            move |this, result, cx| {
                match result {
                    Ok(()) => this.remove_session(&deleted, cx),
                    Err(error) => log::warn!("Cannot delete the unused task {deleted}: {error}"),
                }
                cx.notify();
            },
        );
    }

    /// Build an explicit request so new-task placement never depends on the
    /// runtime's startup fallback.
    fn new_session_request(&self) -> Option<AgentCreateSessionRequest> {
        Some(AgentCreateSessionRequest {
            project_root: Some(self.project_root.clone()?),
            title: None,
            model: self.selected_model.clone(),
            context_limit: None,
            // The draft's switches decide which servers start with the
            // task; the runtime reads curated MCP integrations (CUA) from
            // this list too. External agents are not servers and are
            // applied after creation.
            mcp_server_names: self.draft_mcp_names(),
        })
    }

    /// Load a task and make it current when its snapshot lands; its
    /// persisted root then becomes the visible project context.
    pub(crate) fn select_session(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if self.host_recovery.unavailable {
            self.notice =
                Some("Task history is reconnecting. Try again when it has recovered.".into());
            cx.notify();
            return;
        }
        // The side thread belongs to the task it forked.
        if self.btw.is_some() && self.selected_session.as_deref() != Some(session_id) {
            self.close_side_thread(cx);
        }
        if self.selected_session.is_none() {
            self.withdraw_first_send(cx);
        }
        self.load_session(session_id, LoadMode::Select, LOAD_RETRIES, cx);
    }

    /// Replace the timeline after a mid-run history compaction without
    /// disturbing the active run or a pending question.
    fn reload_timeline(&mut self, session_id: &str, cx: &mut Context<Self>) {
        self.load_session(session_id, LoadMode::Reload, LOAD_RETRIES, cx);
    }

    /// Fetch a session snapshot and apply it. Events that land while the
    /// snapshot is in flight make it stale (the revision moved); the load
    /// is then repeated up to `retries` times and finally applied as is,
    /// so a session that streams while it is opened never stays blank.
    fn load_session(
        &mut self,
        session_id: &str,
        mode: LoadMode,
        retries: u8,
        cx: &mut Context<Self>,
    ) {
        // A selection supersedes any reload; a reload only supersedes
        // earlier reloads.
        self.reload_generation += 1;
        let generation = match mode {
            LoadMode::Select => {
                self.project_folder_dialog = None;
                self.selection_generation += 1;
                self.selection_generation
            }
            LoadMode::Reload => self.reload_generation,
        };
        let revision = *self.timeline_revisions.get(session_id).unwrap_or(&0);
        if matches!(mode, LoadMode::Select) && !self.is_selected(session_id) {
            if self.loading_session.as_deref() != Some(session_id) {
                self.loading_timeline.clear();
            }
            self.loading_session = Some(session_id.to_string());
            cx.notify();
        }
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let session_id = session_id.to_string();
        let target = session_id.clone();
        let subscription_intent = backend.next_subscription_intent();
        self.call(
            async move {
                backend
                    .open_task(
                        &user_id,
                        crate::backend::OpenTaskPageRequest {
                            task: target,
                            limit_rows: None,
                            limit_bytes: None,
                        },
                        subscription_intent,
                    )
                    .await
            },
            cx,
            move |this, result, cx| {
                // A newer selection (or reload) superseded this load.
                let current_generation = match mode {
                    LoadMode::Select => this.selection_generation,
                    LoadMode::Reload => this.reload_generation,
                };
                if current_generation != generation {
                    return;
                }
                // A reload swaps the transcript of the task on screen only.
                if matches!(mode, LoadMode::Reload) && !this.is_selected(&session_id) {
                    return;
                }
                let mut detail = match result {
                    Ok(detail) => detail,
                    Err(message) => {
                        this.finish_loading(&session_id);
                        this.notice = Some(message.into());
                        cx.notify();
                        return;
                    }
                };
                detail.timeline = detail.timeline.into_iter().map(paging::normalize).collect();
                let current = *this
                    .timeline_revisions
                    .get(&detail.session.id)
                    .unwrap_or(&0);
                if current != revision && retries > 0 {
                    this.load_session(&session_id, mode, retries - 1, cx);
                    return;
                }
                // The response's cutoff includes live partials. Keep newer
                // rows already delivered while the snapshot crossed IPC.
                let mut newer: Vec<_> = if this.is_selected(&session_id) {
                    this.timeline.clone()
                } else {
                    this.loading_timeline.values().cloned().collect()
                };
                newer.sort_by_key(|item| item.revision);
                for item in newer
                    .into_iter()
                    .filter(|item| item.revision > detail.timeline_revision)
                {
                    if let Some(row) = detail
                        .timeline
                        .iter_mut()
                        .find(|row| same_timeline_occurrence(row, &item))
                    {
                        *row = item;
                    } else {
                        detail.timeline.push(item);
                    }
                }
                this.finish_loading(&session_id);
                this.snapshot_revision = detail.timeline_revision;
                this.loading_timeline.clear();
                // Stored summaries stand in for the model calls the
                // timeline would otherwise request again.
                let summaries: HashMap<String, SharedString> = detail
                    .summaries
                    .into_iter()
                    .map(|(id, summary)| (id, SharedString::from(summary)))
                    .collect();
                let before = detail.before.is_some();
                let epoch = detail.rows_epoch;
                let plan = detail.plan;
                let live_has_more = detail.live_has_more;
                match mode {
                    LoadMode::Select => {
                        this.upsert_session(detail.session.clone(), cx);
                        this.set_active_session(detail.session, detail.timeline, summaries, cx);
                        this.set_queue(detail.queue.items);
                    }
                    LoadMode::Reload => {
                        this.tool_summaries.extend(summaries);
                        this.replace_timeline(detail.timeline);
                        this.warm_markdown(cx);
                        this.load_attachment_images(cx);
                        this.summarize_loaded_tools(cx);
                        cx.notify();
                    }
                }
                this.pages.generation += 1;
                this.pages.before = before;
                this.pages.after = false;
                this.pages.epoch = Some(epoch);
                this.pages.loading = false;
                this.pages.live_has_more = live_has_more;
                if let Some(plan) = plan.and_then(|item| plan_entries(&item)) {
                    this.set_plan(plan);
                }
            },
        );
    }

    /// The open of `session_id` finished (either way), so the pane stops
    /// saying it is loading. A newer selection keeps its own marker.
    fn finish_loading(&mut self, session_id: &str) {
        if self.loading_session.as_deref() == Some(session_id) {
            self.loading_session = None;
            self.refresh_selected_title();
        }
    }

    /// Install a new timeline and reset every per-item structure that is
    /// keyed by its contents.
    fn replace_timeline(&mut self, timeline: Vec<AgentTimelineItem>) {
        if let Some(session) = self.selected_session.clone() {
            self.reconcile_send_snapshot(&session, &timeline);
        }
        let old_navigation_order = self
            .application_vim_enabled
            .then(|| self.navigable_timeline_ids());
        self.timeline = timeline.into_iter().map(paging::normalize).collect();
        self.pages.weights.clear();
        self.bound_transcript_window(true);
        self.timeline_index = self
            .timeline
            .iter()
            .enumerate()
            .map(|(index, item)| (item.id.clone(), (index, 0)))
            .collect();
        self.markdown_cache.clear();
        self.derived.clear();
        self.list_state
            .reset_with_uniform_height(self.timeline.len(), TRANSCRIPT_ROW_ESTIMATE);
        if let Some(old_navigation_order) = old_navigation_order {
            self.reconcile_timeline_application_selection(&old_navigation_order);
        }
        let plan = self
            .timeline
            .iter()
            .rev()
            .find_map(plan_entries)
            .unwrap_or_default();
        self.set_plan(plan);
        self.refresh_selected_title();
    }

    /// Apply settings changes when returning from the settings screen:
    /// every default the composer and transcript read updates live.
    pub fn apply_defaults(
        &mut self,
        settings: &crate::settings::AppSettings,
        cx: &mut Context<Self>,
    ) {
        self.tool_details = settings.tool_details;
        self.default_web_enabled = settings.default_web_enabled;
        if self.selected_session.is_none() {
            // The draft has no record of its own; the chip shows the
            // default the task will be created with.
            self.web_enabled = settings.default_web_enabled;
        }
        self.apply_notification_settings(settings);
        self.summaries_enabled = settings.tool_summaries;
        self.composer_vim_enabled = settings.composer_vim_enabled;
        if let Some(composer) = self.composer.clone() {
            composer.update(cx, |input, cx| {
                input.set_vim_enabled(settings.composer_vim_enabled, cx)
            });
        }
        self.set_application_vim_enabled(settings.application_vim_enabled, cx);
        crate::ui::typography::apply(
            crate::ui::typography::ChatFontFamily::parse(&settings.chat_font_family),
            settings.chat_font_size,
        );
        if !self.timeline.is_empty() {
            self.list_state.remeasure_items(0..self.timeline.len());
        }
        // Settings is a different mounted entity, so its focus handle becomes
        // stale when Chat is restored even when no preference changed. This
        // handoff is intentionally cross-mode: Application Vim returns to its
        // proxy, while Standard mode returns to the composer.
        self.screen_focus_pending = true;
        self.tts_voice.clone_from(&settings.tts_voice);
        self.tts_speed = settings.tts_speed;
        // Servers may have been added or removed in settings.
        self.refresh_session_mcp(cx);
        cx.notify();
    }

    fn refresh_context_usage(&self, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let model = self.selected_model.clone();
        self.call(
            async move {
                backend
                    .context_usage(&user_id, &session_id, model.as_deref())
                    .await
            },
            cx,
            |this, result, cx| match result {
                Ok(Some((tokens, limit))) => this.apply_context_usage(tokens, limit, cx),
                Ok(None) => this.apply_context_usage(0, 0, cx),
                Err(_) => {}
            },
        );
    }

    /// Rebuild the subagent card for the task on screen. A background
    /// subagent outlives the turn that started it, so opening the task
    /// later must still show it.
    fn refresh_subagents(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let requested = session_id.clone();
        let epoch = self.subagent_epoch;
        self.call(
            async move { backend.session_subagents(&user_id, &requested).await },
            cx,
            move |this, result, cx| {
                let Ok(subagents) = result else {
                    return;
                };
                // The answer describes the task that was on screen when it
                // was asked for; a switch since then owns the card now.
                if this.selected_session.as_deref() != Some(session_id.as_str()) {
                    return;
                }
                this.apply_subagent_snapshot(epoch, subagents, cx);
            },
        );
    }

    /// Apply a runtime snapshot that was requested at `epoch`. An event
    /// that added or removed a row while the snapshot was in flight makes
    /// it stale: applying it would wipe the added row (later events do
    /// not re-create it) or revive the removed one. Ask again instead;
    /// the events already keep the card right in the meantime.
    fn apply_subagent_snapshot(
        &mut self,
        epoch: u64,
        subagents: Vec<AgentSubagent>,
        cx: &mut Context<Self>,
    ) {
        if self.subagent_epoch != epoch {
            self.refresh_subagents(cx);
            return;
        }
        self.set_subagents(subagents, cx);
    }

    /// Replace the card's rows with a runtime snapshot, keeping the rows
    /// that are already on screen so their elapsed time does not jump.
    fn set_subagents(&mut self, subagents: Vec<AgentSubagent>, cx: &mut Context<Self>) {
        if subagents.is_empty() && self.subagents.is_empty() {
            return;
        }
        let now = std::time::Instant::now();
        self.subagents = subagents
            .into_iter()
            .map(|subagent| {
                let known = self
                    .subagents
                    .iter()
                    .find(|known| known.id == subagent.id)
                    .map(|known| known.started);
                ActiveSubagent {
                    started: known.unwrap_or_else(|| {
                        now.checked_sub(std::time::Duration::from_millis(subagent.elapsed_ms))
                            .unwrap_or(now)
                    }),
                    id: subagent.id,
                    task: subagent.task.into(),
                    background: subagent.background,
                    activity: subagent.activity.map(SharedString::from),
                    external: subagent.external,
                }
            })
            .collect();
        self.schedule_subagent_tick(cx);
        cx.notify();
    }

    fn apply_context_usage(&mut self, tokens: i64, limit: i64, cx: &mut Context<Self>) {
        self.ledger_context_tokens = tokens;
        self.context_limit = limit;
        let fraction = if limit > 0 {
            tokens as f32 / limit as f32
        } else {
            0.0
        };
        if self.context_fraction != Some(fraction) {
            self.context_fraction = Some(fraction);
            cx.notify();
        }
    }

    /// Fallback refresh of context usage while the selected session runs.
    /// Turn boundaries refresh it directly (completed items, Finished);
    /// this timer only covers a missed event, so it is slow and skips
    /// ticks where the timeline did not move.
    fn start_usage_poller(&mut self, session_id: String, cx: &mut Context<Self>) {
        if !self.is_selected(&session_id) {
            return;
        }
        if self.usage_poller_session.as_deref() == Some(session_id.as_str()) {
            return;
        }
        // A poller for another session may still be alive; it exits at
        // its next tick because the generation moved.
        self.usage_poller_generation += 1;
        let generation = self.usage_poller_generation;
        self.usage_poller_session = Some(session_id.clone());
        let target = session_id;
        let mut last_revision = *self.timeline_revisions.get(&target).unwrap_or(&0);
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(5000))
                    .await;
                let keep_going = this
                    .update(cx, |this: &mut ChatScreen, cx| {
                        if this.usage_poller_generation != generation {
                            return false;
                        }
                        let running =
                            this.is_selected(&target) && this.active_runs.contains_key(&target);
                        let revision = *this.timeline_revisions.get(&target).unwrap_or(&0);
                        if running && revision != last_revision {
                            last_revision = revision;
                            this.refresh_context_usage(cx);
                        }
                        if !running {
                            this.usage_poller_session = None;
                        }
                        running
                    })
                    .unwrap_or(false);
                if !keep_going {
                    return;
                }
            }
        })
        .detach();
    }

    fn compact_now(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let compacted = session_id.clone();
        self.notice = Some("Compacting…".into());
        cx.notify();
        self.call(
            async move { backend.compact_session(&user_id, &session_id).await },
            cx,
            move |this, result, cx| match result {
                Ok(()) => {
                    this.notice = Some("Conversation compacted".into());
                    // Reload the task that was compacted, not whatever is
                    // selected by the time the call returns.
                    if this.is_selected(&compacted) {
                        this.reload_timeline(&compacted, cx);
                        this.refresh_context_usage(cx);
                    }
                    cx.notify();
                }
                Err(message) => {
                    this.notice = Some(format!("Compaction failed: {message}").into());
                    cx.notify();
                }
            },
        );
    }

    /// Leave the selected task's presentation without discarding anything
    /// the account runtime still needs for background work. Pending
    /// questions remain keyed by session and reappear when that task is
    /// opened again.
    pub(super) fn clear_selected_session_presentation(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        self.pages.reset_for_navigation();
        if self.first_send_waiting_for_mcp {
            self.withdraw_first_send(cx);
        }
        let left = self.selected_session.clone();
        if self.btw.is_some() {
            self.close_side_thread(cx);
        }
        self.subagent_epoch += 1;
        self.subagents.clear();
        self.tool_summaries.clear();
        self.attachment_images.clear();
        self.attachment_requests.clear();
        self.attachment_failures.clear();
        self.attachment_queue.clear();
        self.attachment_viewport = 0..0;
        self.toggled_tools.clear();
        if let Some(composer) = self.composer.clone() {
            composer.update(cx, |input, cx| input.reset_vim_context(cx));
        }
        // Release the hold while the previous session id is still selected.
        self.abandon_queue_edit(cx);
        self.selected_session = None;
        // Leaving a draft ends it; "New Task" starts the next one itself.
        self.draft = false;
        self.draft_mcp_defaults = None;
        self.draft_mcp_changes.clear();
        // The empty screen uses the default, not the last task's model.
        self.restore_default_model(cx);
        self.sync_sidebar_selection(cx);
        self.set_queue(Vec::new());
        self.replace_timeline(Vec::new());
        self.reset_transcript_view(cx);
        self.reset_question_card(cx);
        self.refresh_session_mcp(cx);
        left
    }

    /// Restart the state that belongs to the transcript on screen: the
    /// text selection, the summary slots, scrolling, and open overlays.
    fn reset_transcript_view(&mut self, cx: &mut Context<Self>) {
        // Ordinals restart with the transcript; drop any stale selection.
        if let Some(selection) = &self.selection {
            selection.update(cx, |selection, _| selection.clear());
        }
        // Summaries in flight belong to the previous screen state; their
        // results are dropped and the queue restarts.
        self.summary_generation += 1;
        self.summary_inflight.clear();
        self.summary_queue.clear();
        self.summary_requests.clear();
        self.unsaved_summaries.clear();
        self.output_summaries.clear();
        self.list_state.scroll_to_end();
        self.awaiting_first_token = false;
        self.lightbox = None;
        self.popup.close(cx);
    }

    /// Make `session` the one on screen. `stored_summaries` are the tool
    /// summaries loaded with its snapshot; they land after the per-task
    /// caches of the previous task are dropped.
    fn set_active_session(
        &mut self,
        session: AgentSessionSummary,
        timeline: Vec<AgentTimelineItem>,
        stored_summaries: HashMap<String, SharedString>,
        cx: &mut Context<Self>,
    ) {
        // Every snapshot installation completes the matching open, including
        // recovery and a newly created task. Keep a newer pending open intact.
        self.finish_loading(&session.id);
        let changed = self.selected_session.as_deref() != Some(session.id.as_str());
        if changed {
            // Everything the previous task showed goes; the new task's own
            // subagents and summaries arrive with the snapshot below.
            self.clear_selected_session_presentation(cx);
        } else {
            // Same task, fresh snapshot: keep its caches, restart the view.
            self.abandon_queue_edit(cx);
            self.reset_transcript_view(cx);
        }
        self.tool_summaries.extend(stored_summaries);
        let project_root = session.project_root.clone();
        // Opening the task is what reads how its last run ended, so the
        // unread or failed marker goes. Only an explicit settle ever moves
        // a task out of the active inbox.
        if self.visible {
            self.mark_seen(&session.id);
        }
        self.selected_session = Some(session.id.clone());
        self.draft = false;
        self.sync_sidebar(cx);
        let previous_root = self.project_root.clone();
        if self.set_project_context(Some(project_root.clone()), cx) && previous_root.is_some() {
            // Working in another project makes it the default for new
            // tasks and the next launch, as the project selector does. An
            // archived task under a removed project restores that project.
            self.persist_project_root(project_root, cx);
        }
        self.refresh_selected_title();
        self.set_queue(Vec::new());
        self.web_enabled = session.web_enabled;
        self.apply_session_model(&session, cx);
        self.replace_timeline(timeline);
        self.load_attachment_images(cx);
        self.refresh_subagents(cx);
        self.summarize_loaded_tools(cx);
        // Questions stay queued per session; the card for this session
        // starts on its first step with nothing picked.
        self.reset_question_card(cx);
        self.refresh_session_mcp(cx);
        // A run already going on this task got no poller while it was off
        // screen (Started fired for an unselected task).
        if let Some(session_id) = self
            .selected_session
            .clone()
            .filter(|id| self.active_runs.contains_key(id))
        {
            self.start_usage_poller(session_id, cx);
        }
        self.check_unresolved_sends(cx);
        cx.notify();
    }

    /// The question shown for the selected session, if any. Other
    /// sessions' questions stay queued until the user switches to them.
    fn current_question(&self) -> Option<&PendingQuestion> {
        let selected = self.selected_session.as_deref()?;
        self.pending_questions
            .iter()
            .find(|question| question.session_id == selected)
    }

    /// Start the displayed card over: drop the step state and the answer
    /// input, then recreate the input when a question is showing.
    fn reset_question_card(&mut self, cx: &mut Context<Self>) {
        self.question_selected.clear();
        self.question_step = 0;
        self.question_step_answers.clear();
        self.question_collapsed = false;
        if let Some(input) = self.pending_question_input.take() {
            input.update(cx, |input, cx| input.clear(cx));
        }
        if self.current_question().is_some() {
            self.ensure_question_input(cx);
            self.question_focus_pending = true;
        }
    }

    /// Look up what the catalog says about the selected model once.
    fn refresh_model_catalog(&mut self, cx: &mut Context<Self>) {
        let Some(model) = self.selected_model.clone() else {
            return;
        };
        if self.model_catalog.contains_key(&model) {
            return;
        }
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let lookup = model.clone();
        self.call(
            async move { backend.model_catalog_entry(&user_id, &lookup).await },
            cx,
            move |this, result, cx| {
                if let Ok(Some(entry)) = result {
                    this.model_catalog.insert(model, entry);
                    cx.notify();
                }
            },
        );
    }

    fn selected_model_entry(&self) -> Option<&CatalogEntry> {
        self.selected_model
            .as_ref()
            .and_then(|model| self.model_catalog.get(model))
    }

    fn selected_model_supports_vision(&self) -> bool {
        self.selected_model_entry()
            .and_then(|entry| entry.vision)
            .unwrap_or(false)
    }

    /// The selected model's context window, which its runs compact at. An
    /// unknown one leaves the runtime's default.
    fn selected_model_context_window(&self) -> Option<usize> {
        self.selected_model_entry()
            .and_then(|entry| entry.context_window)
            .and_then(|window| usize::try_from(window).ok())
    }

    /// Reload the skill slash commands for the current project root.
    pub fn refresh_slash_commands(&mut self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let working_dir = self.project_root.clone();
        let requested_root = working_dir.clone();
        self.call(
            async move { backend.list_slash_commands(&user_id, working_dir).await },
            cx,
            move |this, result, cx| {
                if this.project_root == requested_root
                    && let Ok(commands) = result
                {
                    this.slash_commands = commands;
                    if let Some(composer) = this.composer.clone() {
                        this.composer_changed(&composer, cx);
                    }
                    cx.notify();
                }
            },
        );
    }

    /// Reload the integrations chip: the selected task's servers, or on
    /// the draft the rows a new task would get. Before, opening the chip
    /// on the draft emptied it.
    pub fn refresh_session_mcp(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            if self.draft {
                self.refresh_draft_mcp(cx);
            } else {
                self.set_session_mcp(Vec::new());
            }
            return;
        };
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let target = session_id.clone();
        self.call(
            async move { backend.list_session_mcp_servers(&user_id, &target).await },
            cx,
            move |this, result, cx| {
                if this.selected_session.as_deref() != Some(session_id.as_str()) {
                    return;
                }
                match result {
                    Ok(servers) => this.set_session_mcp(servers),
                    Err(message) => log::debug!("mcp list failed: {message}"),
                }
                cx.notify();
            },
        );
    }

    /// Load the integrations a new task would start with, for the draft's
    /// chip: the configured MCP servers with their defaults and the
    /// curated integrations (external agents, CUA), so they can be
    /// switched on for the first turn. The rows on screen stay until the
    /// fresh ones land, and the user's toggles carry over.
    fn refresh_draft_mcp(&mut self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let root = self.project_root.clone();
        let requested_root = root.clone();
        self.draft_mcp_generation += 1;
        let generation = self.draft_mcp_generation;
        let selection_generation = self.selection_generation;
        self.call(
            async move {
                let (servers, integrations) = tokio::join!(
                    async {
                        match root {
                            Some(root) => backend.list_project_mcp_servers(&user_id, &root).await,
                            None => backend.list_mcp_servers(&user_id).await,
                        }
                    },
                    backend.list_integrations(&user_id)
                );
                // The configured servers are the chip's floor; a catalog
                // that cannot be read costs only the curated rows.
                let integrations = integrations.unwrap_or_else(|message| {
                    log::debug!("integration list failed: {message}");
                    Vec::new()
                });
                Ok(draft_mcp_rows(servers?, &integrations))
            },
            cx,
            move |this, result, cx| {
                if !this.draft
                    || this.project_root != requested_root
                    || this.draft_mcp_generation != generation
                    || this.selection_generation != selection_generation
                {
                    return;
                }
                match result {
                    Ok(rows) => {
                        this.set_draft_mcp_defaults(rows);
                        if this.first_send_waiting_for_mcp {
                            this.first_send_waiting_for_mcp = false;
                            if let Some(action) = this.pending_first_send.take() {
                                this.create_for_first_send(action, cx);
                            }
                        }
                    }
                    Err(message) if this.first_send_waiting_for_mcp => this.fail_new_session(
                        format!("Could not load project integrations: {message}"),
                        cx,
                    ),
                    Err(message) => log::debug!("mcp list failed: {message}"),
                }
                cx.notify();
            },
        );
    }

    /// Install the rows a new task would start with. Toggles the user made
    /// on rows that still exist are applied over them; the others go.
    fn set_draft_mcp_defaults(&mut self, defaults: Vec<AgentSessionMcpServer>) {
        self.draft_mcp_changes.retain(|change| {
            defaults
                .iter()
                .any(|row| same_integration(&row.name, row.kind, &change.name, change.kind))
        });
        let mut rows = defaults.clone();
        for save in &self.project_mcp_saves {
            if self.project_root.as_deref() == Some(save.root.as_str())
                && let Some(row) = rows.iter_mut().find(|row| {
                    same_integration(
                        &row.name,
                        row.kind,
                        &save.name,
                        AgentSessionIntegrationKind::Mcp,
                    )
                })
            {
                row.enabled = save.enabled;
            }
        }
        for row in &mut rows {
            if let Some(change) = self
                .draft_mcp_changes
                .iter()
                .find(|change| same_integration(&change.name, change.kind, &row.name, row.kind))
            {
                row.enabled = change.enabled;
            }
        }
        self.draft_mcp_defaults = Some(defaults);
        self.set_session_mcp(rows);
    }

    /// The MCP servers the create request starts: every configured server
    /// and curated MCP integration switched on in the draft, so one the
    /// user switched off never starts. `None` while the rows have not
    /// loaded, so the task takes the defaults rather than nothing.
    fn draft_mcp_names(&self) -> Option<Vec<String>> {
        self.draft_mcp_defaults.as_ref()?;
        Some(
            self.session_mcp
                .iter()
                .filter(|row| row.kind == AgentSessionIntegrationKind::Mcp && row.enabled)
                .map(|row| row.name.clone())
                .collect(),
        )
    }

    fn toggle_session_mcp(
        &mut self,
        name: String,
        kind: AgentSessionIntegrationKind,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(session_id) = self.selected_session.clone() else {
            // The draft: custom choices also become this project's defaults.
            if let Some(server) = self
                .session_mcp
                .iter_mut()
                .find(|server| same_integration(&server.name, server.kind, &name, kind))
            {
                server.enabled = enabled;
            }
            self.mcp_enabled_count = self
                .session_mcp
                .iter()
                .filter(|server| server.enabled)
                .count();
            self.draft_mcp_changes
                .retain(|change| !same_integration(&change.name, change.kind, &name, kind));
            self.draft_mcp_changes.push(DraftMcpChange {
                name: name.clone(),
                kind,
                enabled,
            });
            if self.draft
                && is_custom_mcp(&name, kind)
                && let Some(root) = self.project_root.clone()
            {
                let pending = usize::from(self.project_mcp_save_pending);
                if let Some(save) = self
                    .project_mcp_saves
                    .iter_mut()
                    .skip(pending)
                    .find(|save| {
                        save.root == root
                            && same_integration(
                                &save.name,
                                AgentSessionIntegrationKind::Mcp,
                                &name,
                                kind,
                            )
                    })
                {
                    save.enabled = enabled;
                } else {
                    self.project_mcp_saves.push_back(ProjectMcpSave {
                        root,
                        name,
                        enabled,
                    });
                }
                self.save_next_project_mcp(cx);
            }
            cx.notify();
            return;
        };
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let target = session_id.clone();
        self.call(
            async move {
                backend
                    .set_session_mcp_server_enabled(&user_id, &target, &name, kind, enabled)
                    .await
            },
            cx,
            move |this, result, cx| {
                if this.selected_session.as_deref() != Some(session_id.as_str()) {
                    return;
                }
                match result {
                    Ok(servers) => this.set_session_mcp(servers),
                    Err(message) => this.notice = Some(message.into()),
                }
                cx.notify();
            },
        );
    }

    fn save_next_project_mcp(&mut self, cx: &mut Context<Self>) {
        if self.project_mcp_save_pending {
            return;
        }
        let Some(save) = self.project_mcp_saves.front() else {
            return;
        };
        let root = save.root.clone();
        let name = save.name.clone();
        let enabled = save.enabled;
        self.project_mcp_save_pending = true;
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let target = root.clone();
        let server = name.clone();
        self.call(
            async move {
                backend
                    .set_project_mcp_server_enabled(&user_id, &target, &server, enabled)
                    .await
            },
            cx,
            move |this, result, cx| {
                this.project_mcp_saves.pop_front();
                this.project_mcp_save_pending = false;
                match result {
                    Err(message) => {
                        this.notice = Some(
                            format!(
                                "Could not save {name} for {}: {message}",
                                root_display_name(&root)
                            )
                            .into(),
                        );
                        cx.notify();
                    }
                    Ok(()) if this.draft && this.project_root.as_deref() == Some(root.as_str()) => {
                        // A read begun before this save must not restore its old default.
                        this.refresh_draft_mcp(cx);
                    }
                    Ok(()) => {}
                }
                this.save_next_project_mcp(cx);
            },
        );
    }

    /// Persist the web flag for the selected task; the runtime applies it
    /// on the next turn. On the draft it is only screen state, applied
    /// when the task is created.
    fn set_web_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            self.web_enabled = enabled;
            cx.notify();
            return;
        };
        let previous = self.web_enabled;
        self.web_enabled = enabled;
        cx.notify();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let target = session_id.clone();
        self.call(
            async move {
                backend
                    .set_session_web_enabled(&user_id, &target, enabled)
                    .await
            },
            cx,
            move |this, result, cx| {
                match result {
                    Ok(session) => {
                        this.upsert_session(session, cx);
                    }
                    Err(message) => {
                        if this.selected_session.as_deref() == Some(session_id.as_str()) {
                            this.web_enabled = previous;
                        }
                        this.notice =
                            Some(format!("Could not change web access: {message}").into());
                    }
                }
                cx.notify();
            },
        );
    }

    fn toggle_composer_expanded(&mut self, cx: &mut Context<Self>) {
        self.composer_expanded = !self.composer_expanded;
        let expanded = self.composer_expanded;
        if let Some(composer) = self.composer.clone() {
            composer.update(cx, |input, cx| input.set_fill_height(expanded, cx));
        }
        cx.notify();
    }

    pub fn set_update(&mut self, snapshot: crate::update::Snapshot, cx: &mut Context<Self>) {
        if self.update != snapshot {
            self.update = snapshot;
            cx.notify();
        }
    }

    /// What the Settings menu says about the updater, when it has
    /// something for the user: the row label and whether it failed.
    fn update_notice(&self) -> Option<(&'static str, bool)> {
        let snapshot = &self.update;
        if !snapshot.show_banner {
            return None;
        }
        Some(match snapshot.state {
            crate::update::State::Error => ("Update failed", true),
            crate::update::State::RestartRequired => ("Restart to finish update", false),
            _ => ("Update available", false),
        })
    }

    /// A ready update opens the restart confirmation; anything else
    /// (manual download, a failure) opens About, which shows the full
    /// message and actions.
    fn open_update(&mut self, cx: &mut Context<Self>) {
        if self.update.state == crate::update::State::Ready {
            cx.emit(crate::update::UpdateRequested(
                crate::update::UpdateAction::Restart,
            ));
        } else {
            cx.emit(OpenSettingsSection(Section::About));
        }
    }

    /// Keep a sent prompt for Up/Down recall. Repeats move to the end.
    fn remember_prompt(&mut self, text: &str) {
        let text = text.trim();
        self.history_index = None;
        self.history_draft.clear();
        if text.is_empty() {
            return;
        }
        self.prompt_history.retain(|entry| entry != text);
        self.prompt_history.push(text.to_string());
        if self.prompt_history.len() > PROMPT_HISTORY_LIMIT {
            let excess = self.prompt_history.len() - PROMPT_HISTORY_LIMIT;
            self.prompt_history.drain(..excess);
        }
    }

    /// Up/Down in the composer walks sent prompts when the composer is
    /// empty or still shows a recalled prompt. Returns the text to show,
    /// or `None` when the key should move the caret instead.
    fn recall_prompt(&mut self, key: &str, current: &str) -> Option<String> {
        let browsing = self
            .history_index
            .and_then(|index| self.prompt_history.get(index))
            .is_some_and(|entry| entry == current);
        if !browsing && !current.trim().is_empty() {
            return None;
        }
        if self.prompt_history.is_empty() {
            return None;
        }
        match key {
            "up" => {
                let next = match self.history_index {
                    Some(0) => return None,
                    Some(index) if browsing => index - 1,
                    _ => {
                        self.history_draft = current.to_string();
                        self.prompt_history.len() - 1
                    }
                };
                self.history_index = Some(next);
                Some(self.prompt_history[next].clone())
            }
            "down" => {
                let index = self.history_index?;
                if !browsing {
                    return None;
                }
                if index + 1 < self.prompt_history.len() {
                    self.history_index = Some(index + 1);
                    Some(self.prompt_history[index + 1].clone())
                } else {
                    self.history_index = None;
                    Some(std::mem::take(&mut self.history_draft))
                }
            }
            _ => None,
        }
    }

    /// Pin a subagent above the composer and start its elapsed time.
    fn start_subagent(
        &mut self,
        id: String,
        task: String,
        background: bool,
        external: Option<maple_agent::agent::ExternalAgentRef>,
        cx: &mut Context<Self>,
    ) {
        if self.subagents.iter().any(|subagent| subagent.id == id) {
            return;
        }
        self.subagent_epoch += 1;
        self.subagents.push(ActiveSubagent {
            id,
            task: task.into(),
            background,
            started: std::time::Instant::now(),
            activity: None,
            external,
        });
        self.schedule_subagent_tick(cx);
    }

    /// Stop an external agent from its row. The row closes when the
    /// runtime reports the turn's end.
    fn stop_external_agent(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let agent_id = agent_id.to_string();
        self.call(
            async move {
                backend
                    .cancel_external_agent(&user_id, &session_id, &agent_id)
                    .await
            },
            cx,
            |this, result, cx| {
                if let Err(message) = result {
                    this.notice = Some(message.into());
                }
                cx.notify();
            },
        );
    }

    /// Paint once a second while a subagent works, so the elapsed time on
    /// its row stays true. The last subagent to end stops the timer.
    fn schedule_subagent_tick(&self, cx: &mut Context<Self>) {
        if self.subagents.is_empty() || self.subagent_tick_pending.replace(true) {
            return;
        }
        cx.spawn(async move |entity, cx| {
            cx.background_executor().timer(SUBAGENT_TICK).await;
            entity
                .update(cx, |this, cx| {
                    this.subagent_tick_pending.set(false);
                    if this.subagents.is_empty() {
                        return;
                    }
                    this.schedule_subagent_tick(cx);
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// The current notice as a dismissable row, or nothing. Showing it
    /// arms a timer that clears it after `NOTICE_TTL` unless it changed.
    pub(super) fn render_notice(&self, cx: &mut Context<Self>) -> Option<Div> {
        let notice = self.notice.clone()?;
        self.schedule_notice_dismiss(notice.clone(), cx);
        Some(div().child(motion::fade_in(
            widgets::notice(
                "notice-close",
                notice,
                cx.listener(|this, _event, _window, cx| {
                    this.notice = None;
                    cx.notify();
                }),
            ),
            "notice-reveal",
        )))
    }

    fn schedule_notice_dismiss(&self, shown: SharedString, cx: &mut Context<Self>) {
        if self.notice_dismiss_pending.replace(true) {
            return;
        }
        cx.spawn(async move |entity, cx| {
            cx.background_executor().timer(NOTICE_TTL).await;
            entity
                .update(cx, |this, cx| {
                    this.notice_dismiss_pending.set(false);
                    if this.notice.as_ref() == Some(&shown) {
                        this.notice = None;
                        cx.notify();
                    } else if this.notice.is_some() {
                        // A newer notice replaced it: give that one its own
                        // full time on screen.
                        cx.notify();
                    }
                })
                .ok();
        })
        .detach();
    }

    /// A markdown parse landed: show it, and let the row measure again
    /// since its height changed.
    fn markdown_parsed(
        &mut self,
        kind: MarkdownKind,
        id: &str,
        revision: u64,
        len: usize,
        document: markdown::Document,
        cx: &mut Context<Self>,
    ) {
        self.markdown_cache
            .install(kind, id, revision, len, document);
        if let Some((ix, _)) = self.timeline_index.get(id) {
            self.list_state.remeasure_items(*ix..*ix + 1);
        }
        cx.notify();
    }

    /// Parse the newest long messages in the background after a timeline
    /// load, newest first, so opening a task does not parse on the first
    /// scroll. Short ones parse inline when shown, which is cheaper than
    /// a round trip.
    fn warm_markdown(&mut self, cx: &mut Context<Self>) {
        const WARM_ROWS: usize = 32;
        let sources: Vec<(String, u64, String)> = self
            .timeline
            .iter()
            .rev()
            .take(WARM_ROWS)
            .filter(|item| item.item_type == "message")
            .filter_map(|item| {
                let text = item.text.as_deref()?;
                let revision = self.timeline_index.get(&item.id).map_or(0, |(_, rev)| *rev);
                let wanted = text.len() > INLINE_PARSE_LIMIT
                    && !self.markdown_cache.is_current(
                        &item.id,
                        MarkdownKind::Body,
                        revision,
                        text.len(),
                    );
                wanted.then(|| (item.id.clone(), revision, text.to_string()))
            })
            .collect();
        if sources.is_empty() {
            self.markdown_warmup = None;
            return;
        }
        self.markdown_warmup = Some(cx.spawn(async move |this, cx| {
            for (id, revision, source) in sources {
                let len = source.len();
                let document =
                    cx.background_executor()
                        .spawn_with_priority(gpui::Priority::Low, async move {
                            markdown::parse(&source)
                        })
                        .await;
                let landed = this.update(cx, |this, cx| {
                    this.markdown_parsed(MarkdownKind::Body, &id, revision, len, document, cx);
                });
                if landed.is_err() {
                    break;
                }
            }
        }));
    }

    fn is_run_active(&self) -> bool {
        self.selected_session
            .as_ref()
            .is_some_and(|session| self.active_runs.contains_key(session))
    }

    #[cfg(test)]
    fn session_activity(&self, session_id: &str) -> Option<SessionActivity> {
        self.task_activity().of(session_id)
    }

    /// Open the task before or after the selected one, in sidebar order.
    /// Defaults are Command-Option-Up/Down on macOS and Alt-Up/Down elsewhere.
    fn step_task(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some((row, request)) = self.task_step_target(delta, cx) else {
            return;
        };
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.reveal_row(row, cx));
        if request.computer.as_deref() == self.backend.remote_host() {
            if let Destination::Task(id) = request.destination {
                self.select_session(&id, cx);
            }
        } else {
            cx.emit(request);
        }
    }

    /// The task `delta` rows away from the selected one, as the sidebar
    /// row it sits on and its computer-scoped destination.
    fn task_step_target(&self, delta: isize, cx: &gpui::App) -> Option<(usize, Navigate)> {
        self.sidebar.read(cx).step_target(delta)
    }

    /// Ctrl-1 to Ctrl-9 submits a single choice or toggles a multi-select option.
    fn pick_and_submit_question_option(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(question) = self.current_question() else {
            return;
        };
        if self.question_collapsed {
            self.toggle_question_collapsed(cx);
            return;
        }
        let step = self
            .question_step
            .min(question.questions.len().saturating_sub(1));
        let options = question
            .questions
            .get(step)
            .map(|entry| entry.options.len())
            .unwrap_or(0);
        if index >= options {
            return;
        }
        if question.questions[step].multi_select {
            self.toggle_question_option(step, index, cx);
        } else {
            self.select_question_option(step, index, cx);
            self.submit_question(cx);
        }
    }

    fn escape(&mut self, cx: &mut Context<Self>) {
        if self.feedback.is_some() {
            self.close_feedback(cx);
            return;
        }
        // A rename ends first. A project's rename field sits in the
        // switcher, which passes Escape on and stays open.
        if self
            .sidebar
            .update(cx, |sidebar, cx| sidebar.cancel_rename(cx))
        {
            return;
        }
        // An open menu closes next. It normally has focus and closes on its
        // own Escape binding; this covers a menu that has not taken focus
        // yet, and a text field inside a menu.
        let chat_menu = self.popup.close(cx);
        let sidebar_menu = self
            .sidebar
            .update(cx, |sidebar, cx| sidebar.close_popups(cx));
        if chat_menu || sidebar_menu {
            return;
        }
        if self.queue_edit.is_some() {
            self.discard_queue_edit(cx);
            return;
        }
        if self.btw.is_some() {
            self.close_side_thread(cx);
            return;
        }
        if self
            .sidebar
            .update(cx, |sidebar, cx| sidebar.clear_search(cx))
        {
            return;
        }
        if let Some(status) = self.trust_prompt.as_ref() {
            let path = status.path.clone();
            self.set_project_trust(path, false, cx);
            return;
        }
        if self.confirm.is_some() {
            self.confirm = None;
            cx.notify();
            return;
        }
        if self.lightbox.is_some() {
            self.lightbox = None;
            cx.notify();
            return;
        }
        if self.current_question().is_some() {
            self.skip_question(cx);
            return;
        }
        if let Some(selection) = self.selection.clone()
            && selection.read(cx).has_selection()
        {
            selection.update(cx, |selection, _| selection.clear());
            cx.notify();
            return;
        }
        self.stop(cx);
    }

    fn copy_selected_text(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        let text = selection.read(cx).selected_text();
        if !text.is_empty() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    /// Plain typing with no text input focused routes to the composer:
    /// focus it and insert the character so the first keystroke lands
    /// instead of being lost waiting for the next frame's input handler.
    fn type_into_composer(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.project_folder_dialog.is_some() || self.feedback.is_some() {
            return;
        }
        let Some(composer) = self.composer.clone() else {
            return;
        };
        // The platforms deliver a keystroke to a text input only when it
        // is unmodified text (shift alone allowed); apply the same rule
        // here so chords like ctrl-c keep their meaning. Enter and tab are
        // excluded: macOS reports a key_char for them, but they must keep
        // their focus-navigation and send meaning.
        let Some(text) = event.keystroke.key_char.clone() else {
            return;
        };
        if matches!(event.keystroke.key.as_str(), "enter" | "tab")
            || !event
                .keystroke
                .modifiers
                .is_subset_of(&gpui::Modifiers::shift())
        {
            return;
        }
        if self.application_vim_owns_unfocused_typing(window, cx) {
            self.application_vim.count.clear();
            cx.stop_propagation();
            return;
        }
        // A modal dialog owns the keyboard; nothing types past it.
        if self.trust_prompt.is_some() || self.confirm.is_some() {
            return;
        }
        // A focused text field already receives typing: the composer, the
        // question card, the sidebar's search or rename, or the path field
        // in the project menu.
        if window
            .context_stack()
            .iter()
            .any(|context| context.contains("TextInput"))
        {
            return;
        }
        let handle = composer.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        composer.update(cx, |input, cx| {
            input.prepare_for_typing(cx);
            input.replace_text_in_range(None, &text, window, cx)
        });
        cx.stop_propagation();
    }

    /// Select every message in the transcript. Paragraphs that are not on
    /// screen have never registered their text, so register them here;
    /// the ordinals mirror the ones `render_message` assigns.
    fn select_all_text(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        selection.update(cx, |selection, cx| {
            for item in &self.timeline {
                if item.item_type != "message" {
                    continue;
                }
                let text = item.text.as_deref().unwrap_or("");
                if text.trim().is_empty() {
                    continue;
                }
                let base = self.markdown_cache.ordinal_for(&item.id);
                let revision = self
                    .timeline_index
                    .get(&item.id)
                    .map_or(0, |(_, revision)| *revision);
                if item.role.as_deref() == Some("user") {
                    let derived = self.derived.get(item, revision);
                    selection.register(base + 2048, &derived.text);
                    continue;
                }
                let document =
                    self.markdown_cache
                        .get(&item.id, MarkdownKind::Body, revision, text);
                document.for_each_selectable(|offset, text| {
                    selection.register(base + offset, text);
                });
            }
            selection.select_all();
            cx.notify();
        });
        cx.notify();
    }

    /// Right-click menu over the transcript: copy a link or the selection, select all.
    fn render_transcript_menu(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let Some(&ChatPopup::Transcript(position)) = self.popup.open_key() else {
            return None;
        };
        let has_selection = self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.read(cx).has_selection());
        let command = |command: ChatCommand| {
            move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                this.execute_command(command, window, cx);
            }
        };
        let mut menu = Menu::new("transcript-menu", px(160.))
            .label("Transcript")
            .application_vim(self.application_vim_enabled);
        if let Some(url) = self
            .selection
            .as_ref()
            .and_then(|selection| selection.read(cx).context_link())
        {
            let url = url.to_owned();
            menu = menu.item(
                MenuItem::new("transcript-menu-copy-link", "Copy Link", move |_, _, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(url.clone()));
                })
                .icon("copy"),
            );
        }
        let menu = menu
            .item(
                MenuItem::new(
                    "transcript-menu-copy",
                    "Copy",
                    command(ChatCommand::CopySelection),
                )
                .icon("copy")
                .enabled(has_selection),
            )
            .item(
                MenuItem::new(
                    "transcript-menu-select-all",
                    "Select all",
                    command(ChatCommand::SelectAllTranscript),
                )
                .icon("text-select"),
            );
        Some(self.popup.render(menu, Placement::At(position), window, cx))
    }

    /// Toggle one tool card's or thinking row's expansion and re-measure
    /// its row.
    fn toggle_tool(&mut self, item_id: &str, cx: &mut Context<Self>) {
        if self.toggled_tools.contains(item_id) {
            self.toggled_tools.remove(item_id);
        } else {
            self.toggled_tools.insert(item_id.to_string());
        }
        // Resolve the row now: an index baked into the card at render
        // time goes stale once history reloads or items are inserted.
        if let Some(&(index, _)) = self.timeline_index.get(item_id) {
            self.list_state.remeasure_items(index..index + 1);
        }
        cx.notify();
    }

    /// Show a clicked attachment image full size.
    fn open_lightbox(&mut self, image: Arc<gpui::Image>, cx: &mut Context<Self>) {
        self.lightbox = Some(image);
        cx.notify();
    }
    /// Send without a Window, callable from the composer's Enter hook and
    /// from the Send button.
    /// Send without a Window, callable from the Send button. The button
    /// path owns no composer lease, so it clears the input here.
    /// The Send button. The text stays in the composer until a send path
    /// clears it, so a send that cannot go out yet (the task still being
    /// created, a question waiting) keeps it, as Enter does.
    fn send_inner(&mut self, cx: &mut Context<Self>) {
        let Some(composer) = self.composer.clone() else {
            return;
        };
        let text = composer.read(cx).text();
        self.send_text(text, cx);
    }

    /// Send the given text; used by the composer Enter hook, which hands
    /// over the text and leaves the input to the send path.
    fn send_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.send_text_with(text, false, cx);
    }

    /// Ctrl/Cmd+Enter: send into the active run instead of behind it.
    /// With no run active this is a plain send.
    fn steer_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.send_text_with(text, true, cx);
    }

    fn send_text_with(&mut self, text: String, steer: bool, cx: &mut Context<Self>) {
        // The previous task stays visible until the requested snapshot lands.
        // An inline indicator must not let Enter send into that previous task.
        if self.loading_session.is_some() {
            return;
        }
        if self.host_recovery.unavailable {
            self.notice = Some(
                "Wait for the host connection and task history to recover before sending.".into(),
            );
            cx.notify();
            return;
        }
        if self.thinking_saving {
            self.notice = Some("The thinking level is still being saved".into());
            cx.notify();
            return;
        }
        // A catalog would drop the reply's transcript rows. Leave it first;
        // the existing startup gate holds the send while its runtime warms.
        self.leave_catalog(cx);
        if self.booting {
            self.notice = Some("Agent runtime is still starting".into());
            cx.notify();
            return;
        }
        // With no task the first send creates one and runs there. One
        // create at a time: while it is in flight the composer shows it
        // and keeps the text; Enter changes nothing.
        let session_id = self.selected_session.clone();
        if session_id.is_none() && self.session_setup_pending {
            self.notice = Some("The task is still being created".into());
            cx.notify();
            return;
        }
        // A side question never touches the run, so it is allowed while the
        // run waits on a question. While the side thread is open every plain
        // message goes to it; `/btw` still works and other commands run.
        let side_question = match text.trim().strip_prefix("/btw") {
            Some(rest) if rest.is_empty() || rest.starts_with(char::is_whitespace) => {
                Some(rest.trim())
            }
            _ if self.btw.is_some() && !text.trim().starts_with('/') => Some(text.trim()),
            _ => None,
        };
        if let Some(question) = side_question {
            self.slash_selected = None;
            if let Some(composer) = self.composer.clone() {
                composer.update(cx, |input, cx| input.clear(cx));
            }
            match session_id {
                Some(session_id) => self.ask_side_question(&session_id, question, cx),
                None if question.is_empty() => {
                    self.notice = Some("Type /btw followed by a question".into());
                    cx.notify();
                }
                None => {
                    self.create_for_first_send(FirstSend::SideQuestion(question.to_string()), cx)
                }
            }
            return;
        }
        if self.current_question().is_some() {
            // The run is blocked on the question; sending here would queue a
            // message nobody reads and look like a stall.
            self.notice = Some("Answer the question above first".into());
            self.question_collapsed = false;
            self.question_focus_pending = true;
            cx.notify();
            return;
        }
        if text.trim().is_empty() && self.draft_attachments.is_empty() {
            return;
        }
        if !self.draft_attachments.iter().all(DraftAttachment::ready) {
            // The stable copy is still being prepared; keep the text for the retry.
            self.notice = Some("Files are still being prepared; try again in a moment".into());
            if let Some(composer) = self.composer.clone() {
                composer.update(cx, |input, cx| input.set_text(&text, cx));
            }
            cx.notify();
            return;
        }
        // Enter on an open palette runs the highlighted command instead of
        // sending the partial token.
        if let Some(command) = self.slash_palette_command(text.trim()) {
            self.slash_selected = None;
            if let Some(composer) = self.composer.clone() {
                composer.update(cx, |input, cx| input.clear(cx));
            }
            self.try_command(session_id.as_deref(), &format!("/{command}"), cx);
            return;
        }
        // A command that acts on a task becomes the draft's first send and,
        // like a message, stays on show until it runs (or is refused).
        // Clearing it would let whatever is typed during the create go out
        // in its place.
        let first_send = session_id.is_none()
            && Self::parse_command(text.trim())
                .is_some_and(|(name, _)| self.command_needs_task(name));
        if self.try_command(session_id.as_deref(), text.trim(), cx) {
            // Commands never echo into the transcript; drop the typed text
            // so the palette cannot survive the execution.
            self.slash_selected = None;
            if !first_send && let Some(composer) = self.composer.clone() {
                composer.update(cx, |input, cx| input.clear(cx));
            }
            return;
        }
        match session_id {
            Some(session_id) => {
                let queue_id = self.queue_edit.as_ref().map(|edit| edit.queue_id.clone());
                self.send_to_session(&session_id, text, steer, queue_id, cx);
            }
            None => self.create_for_first_send(FirstSend::Message { text, steer }, cx),
        }
    }

    /// The command Enter should run when the slash palette is open: the
    /// highlighted entry, falling back to the only or first match. `None`
    /// when the palette is closed or the text is already a full command.
    fn slash_palette_command(&self, text: &str) -> Option<String> {
        if self.current_question().is_some() {
            return None;
        }
        let entries = &self.slash_entries;
        if entries.is_empty() {
            return None;
        }
        // A full exact match runs through the normal command path so the
        // typed form (including arguments) is preserved.
        let exact = text
            .strip_prefix('/')
            .and_then(|body| body.split_whitespace().next())
            .map(|name| {
                entries
                    .iter()
                    .any(|entry| entry.name.eq_ignore_ascii_case(name))
            })
            .unwrap_or(false);
        if exact || text.contains(' ') {
            return None;
        }
        let index = self
            .slash_selected
            .filter(|index| *index < entries.len())
            .unwrap_or(0);
        Some(entries[index].name.clone())
    }

    /// Arrow/tab handling for the open slash palette; returns whether the
    /// key was consumed.
    fn navigate_slash_palette(&mut self, key: &str, token: &str, cx: &mut Context<Self>) -> bool {
        let entries = slash_entries_for(token, &self.slash_commands);
        let count = entries.len();
        if count == 0 {
            return false;
        }
        match key {
            "down" => {
                let next = self.slash_selected.map_or(0, |index| (index + 1) % count);
                self.slash_selected = Some(next);
            }
            "up" => {
                let next = self
                    .slash_selected
                    .map_or(count - 1, |index| index.max(1) - 1);
                self.slash_selected = Some(next);
            }
            _ => return false,
        }
        cx.notify();
        true
    }

    /// Replace the composer text with `/name ` and focus it.
    fn complete_slash_command(&mut self, name: &str, cx: &mut Context<Self>) {
        let completed = format!("/{name} ");
        if let Some(composer) = self.composer.clone() {
            composer.update(cx, |input, cx| input.set_text(&completed, cx));
        }
        self.slash_selected = None;
        cx.notify();
    }

    /// A `/command` as typed: its name and its arguments. `None` for text
    /// that is not one, such as a path that merely starts with a slash.
    fn parse_command(text: &str) -> Option<(&str, &str)> {
        let body = text.strip_prefix('/')?;
        let (name, args) = match body.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (body, ""),
        };
        (!name.is_empty() && !name.contains('/')).then_some((name, args))
    }

    fn is_skill_command(&self, name: &str) -> bool {
        self.slash_commands
            .iter()
            .any(|command| command.name.eq_ignore_ascii_case(name))
    }

    /// Whether the `/command` named `name` acts on a task. On the draft it
    /// creates the task first and runs as the first send.
    fn command_needs_task(&self, name: &str) -> bool {
        matches!(name, "compact" | "btw") || self.is_skill_command(name)
    }

    /// Execute a `/command` when it matches a built-in or a skill. Unknown
    /// commands fall through and are sent to the model as plain text. With
    /// no task (`session_id` is `None`) a command that acts on one creates
    /// the task first and runs once it exists.
    fn try_command(
        &mut self,
        session_id: Option<&str>,
        text: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((name, args)) = Self::parse_command(text) else {
            return false;
        };
        let is_skill = self.is_skill_command(name);
        let needs_task = self.command_needs_task(name);
        let session_id = match session_id {
            Some(session_id) => Some(session_id.to_string()),
            None if needs_task => {
                self.create_for_first_send(FirstSend::Command(text.to_string()), cx);
                return true;
            }
            None => None,
        };
        match name {
            "compact" => {
                self.compact_now(cx);
                true
            }
            "new" => {
                self.new_session(cx);
                true
            }
            "pin" => {
                if let Some(task_id) = self.selected_session.clone() {
                    self.toggle_task_pin(&task_id, cx);
                } else {
                    self.notice = Some("No task selected".into());
                    cx.notify();
                }
                true
            }
            "web" => {
                let next = !self.web_enabled;
                self.set_web_enabled(next, cx);
                true
            }
            "model" => {
                if args.is_empty() {
                    self.popup.open(ChatPopup::Model, cx);
                } else {
                    let query = args.to_string();
                    match self
                        .models
                        .iter()
                        .find(|model| model.to_lowercase().contains(&query.to_lowercase()))
                    {
                        Some(model) => {
                            let model = model.clone();
                            self.pick_model(model, cx);
                        }
                        None => {
                            self.notice = Some(format!("No model matches “{query}”").into());
                            cx.notify();
                        }
                    }
                }
                true
            }
            "btw" => {
                if let Some(session_id) = session_id {
                    self.ask_side_question(&session_id, args, cx);
                }
                true
            }
            "help" => {
                self.notice =
                    Some("Type / to list commands. Built-ins: /btw, /compact, /new, /pin, /web, /model, /help. Skills appear as /name.".into());
                cx.notify();
                true
            }
            _ => {
                // Skill commands resolve into the prompt that loads them.
                if !is_skill {
                    return false;
                }
                let Some(session_id) = session_id else {
                    return true;
                };
                let backend = self.backend.clone();
                let user_id = self.user_id.clone();
                let working_dir = self.project_root.clone();
                let command = name.to_string();
                let arguments = args.to_string();
                let invocation = text.to_string();
                self.notice = Some("Loading skill…".into());
                cx.notify();
                self.call(
                    async move {
                        backend
                            .resolve_slash_command(&user_id, working_dir, command, arguments)
                            .await
                    },
                    cx,
                    move |this, result, cx| match result {
                        Ok(Some(prompt)) => {
                            this.notice = None;
                            this.send_resolved_skill(&session_id, &invocation, prompt, cx);
                        }
                        Ok(None) => {
                            this.notice = Some("That skill is no longer installed".into());
                            cx.notify();
                        }
                        Err(message) => {
                            this.notice = Some(message.into());
                            cx.notify();
                        }
                    },
                );
                true
            }
        }
    }

    /// Skill expansion can finish after another message starts admission. Its
    /// invocation has already left the composer, so retain it explicitly when
    /// dispatch is refused, without replaying it or replacing a newer draft.
    fn send_resolved_skill(
        &mut self,
        session_id: &str,
        invocation: &str,
        prompt: String,
        cx: &mut Context<Self>,
    ) {
        if self.send_to_session(session_id, prompt, false, None, cx) {
            return;
        }
        self.remember_prompt(invocation);
        let restored = self.is_selected(session_id) && self.fill_empty_composer(invocation, cx);
        self.notice = Some(if restored {
            "Skill not sent while the previous send is settling. The command is restored; send it again when ready."
        } else {
            "Skill not sent while the previous send is settling. The command is retained; press Up in an empty composer to recall it."
        }.into());
        cx.notify();
    }

    /// Dispatch a message. While a run is active the runtime queues it
    /// behind the run, or with `steer` injects it into the run. `queue_id`
    /// names an already queued chip to send in place of new text.
    fn send_to_session(
        &mut self,
        session_id: &str,
        text: String,
        steer: bool,
        queue_id: Option<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        if queue_id.is_none()
            && !self.draft_attachments.is_empty()
            && self
                .attachment_uploads
                .values()
                .any(|upload| upload.session == session_id && !upload.fence.handed_off())
        {
            self.notice = Some("Wait for the current file upload or cancel it before sending more files. Text can still be queued or sent to the running task.".into());
            cx.notify();
            return false;
        }
        if self
            .sending_previews
            .values()
            .any(|send| send.session.as_deref() == Some(session_id) && send.admitting())
        {
            // One admission at a time. Text typed after the click stays in
            // the composer rather than racing the first run's admission.
            cx.notify();
            return false;
        }
        if self
            .unresolved_sends
            .values()
            .any(|send| send.session == session_id && send.awaiting_receipt())
        {
            self.notice =
                Some("Checking the previous send before another message can be sent.".into());
            self.check_unresolved_sends(cx);
            cx.notify();
            return false;
        }
        let send_id = uuid::Uuid::new_v4().to_string();
        let session_id = session_id.to_string();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let model = self.selected_model.clone();
        let vision_capable = self.selected_model_supports_vision();
        let context_limit = self.selected_model_context_window();
        let run_active = self.active_runs.contains_key(&session_id);
        // A queued chip keeps its own attachments; new files stay staged.
        let mut drafts = if queue_id.is_some() {
            Vec::new()
        } else {
            std::mem::take(&mut self.draft_attachments)
        };
        for draft in &mut drafts {
            draft.prepare_upload_for(&session_id);
        }
        let editing = self
            .queue_edit
            .as_ref()
            .is_some_and(|edit| Some(&edit.queue_id) == queue_id.as_ref());
        let mut request = AgentSendMessageRequest {
            session_id: session_id.clone(),
            // A steer identifies the queued item atomically. A fetched preview
            // must never overwrite another client's intervening queue edit.
            text: if queue_id.is_some() && !editing {
                String::new()
            } else {
                text.clone()
            },
            model,
            context_limit,
            vision_capable,
            steer: steer && (run_active || queue_id.is_some()),
            queue_id,
            attachments: Vec::new(),
        };
        self.notice = None;

        let from_composer = request.queue_id.is_none() || editing;
        if from_composer {
            self.remember_prompt(&text);
            // Clear the composer at dispatch so no entry path can leave the
            // sent text behind; a failed send restores it below.
            if let Some(composer) = self.composer.clone() {
                composer.update(cx, |input, cx| input.clear(cx));
            }
        }
        if editing {
            // The edit is on its way; give the stashed draft back.
            self.finish_queue_edit(cx);
        }
        // Sending closes any open menu.
        self.popup.close(cx);
        self.list_state.scroll_to_end();
        if !run_active {
            self.awaiting_first_token = true;
        }
        let preview = sending::SendPreview::new(Some(session_id.clone()), &text, &drafts)
            .capture_timeline(&self.timeline);
        let preview = if drafts.is_empty() {
            preview
        } else {
            preview.uploading()
        };
        self.sending_previews.insert(
            send_id.clone(),
            if run_active || request.queue_id.is_some() {
                preview.status_only()
            } else {
                preview
            },
        );
        cx.notify();
        let retained_send = UnresolvedSend {
            backend: backend.clone(),
            outcome: None,
            session: session_id.clone(),
            text: text.clone(),
            attachments: drafts.clone(),
            from_composer,
        };
        let (upload_fence, progress) = self.begin_uploads(&send_id, &session_id, &drafts, cx);
        if drafts.is_empty() {
            self.admit_prepared_send(
                send_id,
                uploads::PreparedSend {
                    request,
                    send: retained_send,
                    fence: upload_fence,
                },
                cx,
            );
            return true;
        }
        self.call(
            async move {
                let result = match uploads::commit_attachments(
                    &backend,
                    &user_id,
                    &session_id,
                    &mut drafts,
                    &upload_fence,
                    progress,
                )
                .await
                {
                    Ok(ids) => {
                        request.attachments = ids;
                        Ok(())
                    }
                    Err(error) => Err(error),
                };
                Ok((
                    UnresolvedSend {
                        backend: backend.clone(),
                        outcome: None,
                        session: session_id,
                        text,
                        attachments: drafts,
                        from_composer,
                    },
                    result,
                    request,
                    upload_fence,
                ))
            },
            cx,
            move |this, result, cx| {
                match result {
                    Ok((send, Ok(()), request, fence))
                        if this.accept_send_results && !this.discarded_sends.remove(&send_id) =>
                    {
                        if let Some(preview) = this.sending_previews.get_mut(&send_id) {
                            preview.prepare(uploads::PreparedSend {
                                request,
                                send,
                                fence,
                            });
                            this.dispatch_prepared_uploads(cx);
                        }
                    }
                    Ok((send, Err(error), _, _)) => this.finish_attachment_send(
                        send_id,
                        retained_send,
                        Ok((send, Err(error))),
                        cx,
                    ),
                    Err(error) => {
                        this.finish_attachment_send(send_id, retained_send, Err(error), cx)
                    }
                    Ok(_) => {}
                }
                cx.notify();
            },
        );
        true
    }

    fn pending_receipt_queries(&self) -> Vec<SendReceiptQuery> {
        self.unresolved_sends
            .iter()
            .map(|(id, send)| SendReceiptQuery {
                id: id.clone(),
                outcome: send
                    .outcome
                    .clone()
                    .filter(|o| !matches!(o, crate::host::receipts::Outcome::Pending)),
                backend: send.backend.clone(),
            })
            .collect()
    }

    fn check_unresolved_sends(&mut self, cx: &mut Context<Self>) {
        if self.checking_sends || self.unresolved_sends.is_empty() || self.host_recovery.unavailable
        {
            return;
        }
        self.checking_sends = true;
        let queries = self.pending_receipt_queries();
        self.call(
            async move {
                let mut results = Vec::new();
                for SendReceiptQuery {
                    id,
                    outcome: settled,
                    backend,
                } in queries
                {
                    if settled.is_some() {
                        results.push((id, settled));
                        continue;
                    }
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(40),
                        backend.send_outcome(id.clone()),
                    )
                    .await;
                    results.push((id, result.ok().and_then(Result::ok)));
                }
                Ok(results)
            },
            cx,
            |this, result, cx| {
                this.checking_sends = false;
                let Ok(results) = result else {
                    return;
                };
                for (id, outcome) in results {
                    this.apply_send_receipt(&id, outcome, cx);
                }
                cx.notify();
            },
        );
    }

    fn apply_send_receipt(
        &mut self,
        id: &str,
        outcome: Option<crate::host::receipts::Outcome>,
        cx: &mut Context<Self>,
    ) {
        self.apply_send_receipt_result(id, outcome, cx);
        self.dispatch_prepared_uploads(cx);
    }

    fn apply_send_receipt_result(
        &mut self,
        id: &str,
        outcome: Option<crate::host::receipts::Outcome>,
        cx: &mut Context<Self>,
    ) {
        use crate::host::receipts::Outcome;
        let Some(send) = self.unresolved_sends.get_mut(id) else {
            return;
        };
        if outcome.is_some() {
            send.outcome = outcome.clone();
        }
        match outcome {
            Some(Outcome::Finished(Ok(_))) => {
                self.unresolved_sends.remove(id);
                self.notice = Some("The host received your message. Nothing was resent.".into());
            }
            Some(Outcome::Missing | Outcome::Interrupted | Outcome::Finished(Err(_))) => {
                let Some(send) = self.unresolved_sends.get(id) else {
                    return;
                };
                // Never overwrite text typed since the uncertain send,
                // or restore a draft into another task/computer.
                if !send.from_composer {
                    self.unresolved_sends.remove(id);
                    return;
                }
                let empty = self
                    .composer
                    .as_ref()
                    .is_some_and(|c| c.read(cx).text_ref().trim().is_empty())
                    && self.draft_attachments.is_empty();
                if self.selected_session.as_deref() == Some(&send.session) && empty {
                    let send = self.unresolved_sends.remove(id).unwrap();
                    if let Some(composer) = self.composer.clone() {
                        composer.update(cx, |input, cx| input.set_text(&send.text, cx));
                    }
                    self.draft_attachments = send.attachments;
                    self.notice = Some(if matches!(outcome, Some(Outcome::Interrupted)) {
                        "The host restarted before recording this send’s result. It may have taken effect. Your draft is restored; check the task before sending again."
                    } else {
                        "The send did not complete successfully. Your draft is restored; check the task before sending again."
                    }.into());
                } else {
                    self.notice = Some(if matches!(outcome, Some(Outcome::Interrupted)) {
                        "The host restarted before recording this send’s result; it may have taken effect. You can continue. The original draft is retained; open its task with an empty composer to recover it."
                    } else {
                        "The failed draft is retained. You can continue; open its task with an empty composer to recover it."
                    }.into());
                }
            }
            _ => {
                self.notice = Some("Send outcome is still unknown. Your message and files are retained; Maple will check again after reconnecting.".into());
            }
        }
    }

    fn push_local_error(&mut self, title: &str, message: &str, cx: &mut Context<Self>) {
        let item = AgentTimelineItem {
            persisted: None,
            live: None,
            truncated: None,
            revision: 0,
            id: format!(
                "local-error-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_millis())
                    .unwrap_or(0)
            ),
            item_type: "error".to_string(),
            role: None,
            title: Some(title.to_string()),
            text: Some(message.to_string()),
            status: Some("failed".to_string()),
            input: None,
            output: None,
            created_ms: 0,
            merge: "replace".to_string(),
        };
        if let Some(session) = self.selected_session.clone() {
            self.apply_timeline_item(&session, item);
        }
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        let Some(run_id) = self.active_runs.get(&session_id).cloned() else {
            return;
        };
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.cancel_run(&user_id, &run_id).await },
            cx,
            |this, result, cx| {
                if let Err(message) = result {
                    this.notice = Some(message.into());
                }
                cx.notify();
            },
        );
    }

    /// A question card without an input is unanswerable; make sure one
    /// exists whenever a question is showing.
    fn ensure_question_input(&mut self, cx: &mut Context<Self>) {
        if self.pending_question_input.is_some() {
            return;
        }
        let chat = cx.entity().downgrade();
        let question_chat = chat.clone();
        let application_vim_enabled = self.application_vim_enabled;
        let input = cx.new(move |cx| {
            TextInput::new("Type your answer…", cx)
                .application_vim(application_vim_enabled)
                .on_application_escape(move |window, cx| {
                    if let Some(chat) = question_chat.upgrade() {
                        chat.update(cx, |chat, cx| chat.focus_application_vim(window, cx));
                    }
                })
                .on_enter(move |text, _, cx| {
                    let _ = text;
                    let chat = chat.clone();
                    // submit_question reads this input; defer out of its
                    // update first.
                    cx.defer(move |cx| {
                        if let Some(chat) = chat.upgrade() {
                            chat.update(cx, |chat, cx| chat.submit_question(cx));
                        }
                    });
                })
        });
        self.pending_question_input = Some(input);
    }

    fn submit_question(&mut self, cx: &mut Context<Self>) {
        let Some(question) = self.current_question().cloned() else {
            return;
        };
        let step = self
            .question_step
            .min(question.questions.len().saturating_sub(1));
        // Record this step's answer: the picked option, else typed text.
        let answer = self.step_answer(&question, step, cx);
        self.question_step_answers.insert(step, answer);
        if step + 1 < question.questions.len() {
            // More questions in this batch: show the next one.
            self.question_step = step + 1;
            self.question_selected.clear();
            if let Some(input) = self.pending_question_input.clone() {
                input.update(cx, |input, cx| input.clear(cx));
            }
            self.question_focus_pending = true;
            cx.notify();
            return;
        }
        let composed = self.composed_question_answer(cx);
        self.answer_question(composed, cx);
    }

    /// The answer for one step: picked option label plus any typed text
    /// as a note, else typed text alone, else an explicit placeholder so
    /// the model sees it was skipped.
    fn step_answer(
        &self,
        question: &crate::backend::PendingQuestion,
        step: usize,
        cx: &mut Context<Self>,
    ) -> Vec<String> {
        let Some(entry) = question.questions.get(step) else {
            return vec!["(no answer provided)".to_string()];
        };
        let typed = self
            .pending_question_input
            .as_ref()
            .map(|input| input.read(cx).text())
            .unwrap_or_default();
        let typed = typed.trim();
        let mut answer: Vec<_> = self
            .question_selected
            .get(&step)
            .into_iter()
            .flat_map(|selection| &selection.picked)
            .filter_map(|index| entry.options.get(*index))
            .map(|option| option.label.clone())
            .collect();
        if !answer.is_empty() {
            if !typed.is_empty() {
                // The prefix keeps the note from reading as a second
                // picked option in the echoed answers array.
                answer.push(format!("Additional note: {typed}"));
            }
            return answer;
        }
        if !typed.is_empty() {
            return vec![typed.to_string()];
        }
        vec!["(no answer provided)".to_string()]
    }

    /// Deliver `answer` (free text, a picked option, or joined options).
    fn answer_question(&mut self, answer: String, cx: &mut Context<Self>) {
        let Some(question) = self.current_question().cloned() else {
            return;
        };
        if answer.trim().is_empty() {
            return;
        }
        let request_id = question.request_id.clone();
        let callback_request_id = request_id.clone();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        // Drop the answered question and its input so the next card starts
        // fresh; a queued question's event already fired, so the input is
        // recreated right away when one is showing.
        self.pending_questions
            .retain(|queued| queued.request_id != question.request_id);
        self.reset_question_card(cx);
        self.sync_sidebar_activity(cx);
        cx.notify();
        self.call(
            async move { backend.answer_question(&user_id, &request_id, answer).await },
            cx,
            move |this, result, cx| match result {
                Ok(true) => {}
                Ok(false) => {
                    log::warn!(
                        "answer for question {} matched nothing pending",
                        callback_request_id
                    );
                    this.notice = Some("The task was no longer waiting for that answer".into());
                    cx.notify();
                }
                Err(message) => {
                    log::warn!(
                        "answer for question {} failed: {message}",
                        callback_request_id
                    );
                    this.notice = Some(format!("Could not deliver the answer: {message}").into());
                    cx.notify();
                }
            },
        );
    }
    /// Dismiss a question: cancel the run like the Stop button and
    /// unblock the tool with an empty answer if it is still waiting.
    pub(super) fn toggle_question_collapsed(&mut self, cx: &mut Context<Self>) {
        self.question_collapsed = !self.question_collapsed;
        if !self.question_collapsed {
            self.question_focus_pending = true;
        }
        cx.notify();
    }

    fn skip_question(&mut self, cx: &mut Context<Self>) {
        let Some(question) = self.current_question().cloned() else {
            return;
        };
        self.pending_questions
            .retain(|queued| queued.request_id != question.request_id);
        self.reset_question_card(cx);
        self.sync_sidebar_activity(cx);
        cx.notify();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        {
            let request_id = question.request_id.clone();
            let answer_backend = backend.clone();
            let answer_user = user_id.clone();
            self.call(
                async move {
                    answer_backend
                        .answer_question(&answer_user, &request_id, String::new())
                        .await
                },
                cx,
                |_this, _result, _cx| {},
            );
        }
        if let Some(run_id) = self.active_runs.get(&question.session_id).cloned() {
            self.call(
                async move { backend.cancel_run(&user_id, &run_id).await },
                cx,
                |this, result, cx| {
                    if let Err(message) = result {
                        this.notice = Some(message.into());
                    }
                    cx.notify();
                },
            );
        }
    }

    /// Pick an option, replacing earlier choices only for single-select questions.
    fn select_question_option(
        &mut self,
        question_index: usize,
        option_index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(question) = self
            .current_question()
            .and_then(|q| q.questions.get(question_index))
        else {
            return;
        };
        if option_index >= question.options.len() {
            return;
        }
        let multi_select = question.multi_select;
        let selection = self.question_selected.entry(question_index).or_default();
        selection.cursor = Some(option_index);
        if !multi_select {
            selection.picked.clear();
        }
        selection.picked.insert(option_index);
        cx.notify();
    }

    /// Vim navigation moves the cursor without checking multi-select options.
    fn focus_question_option(&mut self, step: usize, index: usize, cx: &mut Context<Self>) {
        // Keys never change a choice the user cannot see.
        self.question_collapsed = false;
        if self
            .current_question()
            .and_then(|q| q.questions.get(step))
            .is_some_and(|q| q.multi_select)
        {
            self.question_selected.entry(step).or_default().cursor = Some(index);
            cx.notify();
        } else {
            self.select_question_option(step, index, cx);
        }
    }

    /// Click handler for an option row: clicking the picked option again
    /// clears it so a typed answer can stand alone.
    fn toggle_question_option(
        &mut self,
        question_index: usize,
        option_index: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(selection) = self.question_selected.get_mut(&question_index)
            && selection.picked.remove(&option_index)
        {
            if selection.picked.is_empty() {
                self.question_selected.remove(&question_index);
            } else {
                selection.cursor = Some(option_index);
            }
            cx.notify();
        } else {
            self.select_question_option(question_index, option_index, cx);
        }
    }

    /// Codex response JSON assembled from the recorded step answers.
    fn composed_question_answer(&mut self, cx: &mut Context<Self>) -> String {
        let Some(question) = self.current_question() else {
            return String::new();
        };
        let mut answers = serde_json::Map::new();
        for step in 0..question.questions.len() {
            let entry = &question.questions[step];
            let answer = self
                .question_step_answers
                .get(&step)
                .cloned()
                .unwrap_or_else(|| self.step_answer(question, step, cx));
            answers.insert(entry.id.clone(), serde_json::json!({ "answers": answer }));
        }
        let composed = serde_json::json!({ "answers": answers }).to_string();
        self.question_step_answers.clear();
        self.question_step = 0;
        composed
    }

    pub(crate) fn sign_out(&mut self, cx: &mut Context<Self>) {
        if self.sign_out_pending {
            return;
        }
        self.sign_out_pending = true;
        // Pending upload completions cannot admit a prompt while saving the
        // account's final presentation edits before sign-out. Those edits are
        // best effort: a failed flush must never keep an account signed in.
        self.accept_send_results = false;
        self.call(
            async {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    crate::settings::flush_pending(),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        log::warn!("Could not save UI preferences before sign-out: {error}")
                    }
                    Err(_) => log::warn!("Saving UI preferences before sign-out timed out"),
                }
                Ok(())
            },
            cx,
            |this, _, cx| this.sign_out_after_settings_flush(cx),
        );
    }

    fn sign_out_failed(&mut self, message: String, cx: &mut Context<Self>) {
        self.sign_out_pending = false;
        self.accept_send_results = true;
        self.notice = Some(message.clone().into());
        cx.emit(SignOutFailed {
            user_id: self.user_id.clone(),
            message,
        });
        cx.notify();
    }

    fn sign_out_after_settings_flush(&mut self, cx: &mut Context<Self>) {
        // A logout during upload must win before prompt admission, even while
        // the host is still shutting down an earlier run.
        self.accept_send_results = false;
        let uploads: Vec<_> = self.attachment_uploads.keys().cloned().collect();
        for id in uploads {
            self.cancel_attachment_send(&id, cx);
        }
        self.attachment_uploads.clear();
        self.draft_attachments.clear();
        self.unresolved_sends.clear();
        self.sending_previews.clear();
        self.stop_speech(cx);
        if self.recording {
            self.recording = false;
            self.audio.cancel_recording();
        }
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move {
                backend.stop_runtime(&user_id).await?;
                backend.logout_and_clear(&user_id).await
            },
            cx,
            |this, result, cx| match result {
                Ok(()) => {
                    this.sign_out_pending = false;
                    crate::settings::retire_sidebar_account(&this.user_id);
                    cx.emit(LoggedOut);
                }
                Err(message) => this.sign_out_failed(message, cx),
            },
        );
    }

    /// Pick the model of the next message, in a task or a new one. It also
    /// becomes the account default for new tasks.
    fn pick_model(&mut self, model: String, cx: &mut Context<Self>) {
        self.selected_model = Some(model.clone());
        self.default_model = Some(model.clone());
        self.refresh_thinking(cx);
        self.popup.close(cx);
        cx.notify();
        self.refresh_model_catalog(cx);
        // Remember the choice across launches via the agent config.
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.save_default_model(&user_id, model).await },
            cx,
            |_this, _result, _cx| {},
        );
        self.refresh_context_usage(cx);
    }

    /// Point the composer at `model` without changing the account default.
    fn set_composer_model(&mut self, model: Option<String>, cx: &mut Context<Self>) {
        if self.selected_model == model {
            return;
        }
        self.selected_model = model;
        self.refresh_model_catalog(cx);
        self.refresh_context_usage(cx);
    }

    /// New Task and drafts use the account default, not the last opened
    /// task's model.
    fn restore_default_model(&mut self, cx: &mut Context<Self>) {
        self.set_composer_model(self.default_model.clone(), cx);
        self.refresh_thinking(cx);
    }

    /// A started task opens on the model it last used.
    fn apply_session_model(&mut self, session: &AgentSessionSummary, cx: &mut Context<Self>) {
        match session.model.clone().filter(|_| session.message_count > 0) {
            Some(model) => {
                self.set_composer_model(Some(model), cx);
                self.refresh_thinking(cx);
            }
            None => self.restore_default_model(cx),
        }
    }

    fn model_label<'a>(&'a self, model: &'a str) -> &'a str {
        self.model_labels.get(model).map_or(model, String::as_str)
    }

    /// Re-measure one row after it changed in place. The list keeps the
    /// row's last height as a hint and the scroll position as it was, so
    /// neither the scroll range nor the view moves before the next paint.
    fn remeasure_item(&mut self, index: usize) {
        self.list_state.remeasure_items(index..index + 1);
    }

    /// Apply a timeline item using Maple's merge contract: `append` extends
    /// message/thinking text on the item with the same id; otherwise merge
    /// fields, keeping the previous value when the incoming field is absent.
    ///
    /// Returns the changed item's index, or None for an older snapshot event.
    fn apply_timeline_item(&mut self, session_id: &str, item: AgentTimelineItem) -> Option<usize> {
        let item = paging::normalize(item);
        let alias_index = item.live.as_ref().and_then(|live| {
            self.timeline
                .iter()
                .position(|row| row.live.as_ref() == Some(live))
        });
        if item.revision > 0 && item.revision <= self.snapshot_revision {
            return None;
        }
        if self.pages.after && !self.timeline_index.contains_key(&item.id) && alias_index.is_none()
        {
            // Forward history pages restore committed rows. A paused live row
            // needs a fresh bounded open, so keep Reload latest available even
            // after paging reaches the committed tail.
            self.pages.live_has_more = true;
            return None;
        }
        if let Some(index) = alias_index
            && self.timeline[index].id != item.id
        {
            if item.revision > 0 && self.timeline[index].revision >= item.revision {
                return None;
            }
            if let (Some(old), Some(new)) = (&self.timeline[index].persisted, &item.persisted)
                && old.update_seq > new.update_seq
            {
                return None;
            }
            let old = self.timeline[index].id.clone();
            self.timeline_index.remove(&old);
            self.timeline[index].id = item.id.clone();
            self.timeline_index.insert(item.id.clone(), (index, 0));
            self.rekey_summary(&old, &item.id);
        }
        if let Some((index, _)) = self.timeline_index.get(&item.id)
            && let (Some(old), Some(new)) = (&self.timeline[*index].persisted, &item.persisted)
            && old.update_seq > new.update_seq
        {
            return None;
        }
        self.pages.weights.remove(&item.id);
        if item.revision > 0 {
            if let Some((index, _)) = self.timeline_index.get(&item.id)
                && self.timeline[*index].revision >= item.revision
            {
                return None;
            }
            if item.revision <= self.snapshot_revision {
                return None;
            }
        }
        // Streaming delivers one of these per chunk; move the payload
        // rather than copy it.
        let position = match self.timeline_index.get_mut(&item.id) {
            Some((index, revision)) => {
                *revision += 1;
                Some(*index)
            }
            None => None,
        };
        let index = match position {
            Some(index) => {
                let AgentTimelineItem {
                    persisted: incoming_persisted,
                    live: incoming_live,
                    truncated: incoming_truncated,
                    revision: incoming_revision,
                    created_ms: incoming_created,
                    item_type: incoming_type,
                    role: incoming_role,
                    title: incoming_title,
                    text: incoming_text,
                    status: incoming_status,
                    input: incoming_input,
                    output: incoming_output,
                    merge: incoming_merge,
                    ..
                } = item;
                let existing = &mut self.timeline[index];
                existing.revision = incoming_revision;
                existing.persisted = incoming_persisted.or_else(|| existing.persisted.take());
                existing.live = incoming_live.or_else(|| existing.live.take());
                existing.truncated = incoming_truncated;
                // The virtualized list caches item heights; tell it this
                // one changed. A remeasure keeps the previous height as a
                // hint and leaves the scroll position alone, so a stream
                // never collapses the scroll range or re-pins the view.
                self.list_state.remeasure_items(index..index + 1);
                let append = incoming_merge == "append"
                    && matches!(incoming_type.as_str(), "message" | "thinking")
                    && incoming_text.is_some();
                if append {
                    let addition = incoming_text.unwrap_or_default();
                    existing
                        .text
                        .get_or_insert_with(String::new)
                        .push_str(&addition);
                } else {
                    existing.created_ms = incoming_created;
                    existing.item_type = incoming_type;
                    existing.role = incoming_role.or_else(|| existing.role.take());
                    existing.title = incoming_title.or_else(|| existing.title.take());
                    existing.text = incoming_text.or_else(|| existing.text.take());
                    existing.status = incoming_status.or_else(|| existing.status.take());
                    existing.input = incoming_input.or_else(|| existing.input.take());
                    existing.output = incoming_output.or_else(|| existing.output.take());
                    existing.merge = incoming_merge;
                }
                index
            }
            None => {
                let index = self.timeline.len();
                self.timeline_index.insert(item.id.clone(), (index, 0));
                self.timeline.push(item);
                self.list_state.splice(index..index, 1);
                if index == 0 {
                    // The pane leaves the empty state; the header follows.
                    self.refresh_selected_title();
                }
                index
            }
        };
        *self
            .timeline_revisions
            .entry(session_id.to_string())
            .or_insert(0) += 1;
        let id = self.timeline[index].id.clone();
        let anchor = self.scroll_anchor();
        let count = self.timeline.len();
        self.bound_transcript_window(self.list_state.is_following_tail());
        if self.timeline.len() != count {
            self.rebuild_window(anchor);
        }
        self.timeline_index.get(&id).map(|(index, _)| *index)
    }

    /// Insert or replace a session row; returns whether anything changed.
    fn upsert_session(&mut self, session: AgentSessionSummary, cx: &mut Context<Self>) -> bool {
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.apply_live_summary(session.clone(), cx)
        });
        // An archived row shows no indicator; drop a marker it may carry.
        if session.state == AgentTaskState::Archived {
            self.mark_seen(&session.id);
        }
        if let Some(existing) = self
            .sessions
            .iter_mut()
            .find(|candidate| candidate.id == session.id)
        {
            if session_summary_eq(existing, &session) {
                return false;
            }
            let mut switched = None;
            if self.selected_session.as_deref() == Some(session.id.as_str()) {
                self.web_enabled = session.web_enabled;
                // Follow a switch the task made, here or on another surface;
                // a pick waiting for the next message stays.
                if existing.model != session.model {
                    switched = Some(session.clone());
                }
            }
            *existing = session;
            if let Some(switched) = switched {
                self.apply_session_model(&switched, cx);
            }
        } else {
            self.sessions.insert(0, session);
        }
        self.sync_sidebar(cx);
        true
    }

    /// One timeline item arrived for the selected session.
    fn apply_incoming_item(
        &mut self,
        session_id: &str,
        item: AgentTimelineItem,
        cx: &mut Context<Self>,
    ) {
        if item.role.as_deref() != Some("user") {
            self.awaiting_first_token = false;
        }
        // Cheap gates first so the common streaming chunk does no extra
        // work; the summary checks read the stored item after the merge.
        let completed = item.status.as_deref() == Some("completed");
        let is_tool = matches!(item.item_type.as_str(), "tool" | "toolCall");
        self.load_attachment_images_for(&item, cx);
        let plan = plan_entries(&item);
        let Some(index) = self.apply_timeline_item(session_id, item) else {
            return;
        };
        if let Some(plan) = plan {
            self.update_live_plan(plan, cx);
        }
        self.follow_application_stream();
        // A newer item after a thinking block finalizes the block; that is
        // when its header summary is requested (it has no status of its
        // own). Streaming chunks merge in place, so `index` stays put and
        // the request-once gate makes repeats cheap.
        if let Some(previous) = index.checked_sub(1)
            && self
                .timeline
                .get(previous)
                .is_some_and(|item| matches!(item.item_type.as_str(), "thinking" | "reasoning"))
        {
            self.maybe_summarize_thinking(previous, cx);
        }
        // A tool call is summarized from its input as soon as the call
        // starts; the request-once gate makes later updates cheap.
        if is_tool && self.summaries_enabled {
            self.maybe_summarize_tool(index, cx);
        }
        if !self.unsaved_summaries.is_empty() {
            self.save_summary(index, cx);
        }
        if completed {
            // A finished item marks a turn boundary: the usage ledger has
            // a new row.
            self.refresh_context_usage(cx);
        }
    }
    pub fn handle_service_events(
        &mut self,
        events: Vec<AgentServiceEvent>,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;
        for event in events {
            changed |= self.apply_service_event(event, cx);
        }
        if changed {
            cx.notify();
        }
    }

    /// Route one backend service event into UI state.
    #[cfg(test)]
    pub fn handle_service_event(&mut self, event: AgentServiceEvent, cx: &mut Context<Self>) {
        if self.apply_service_event(event, cx) {
            cx.notify();
        }
    }

    /// Password maintenance stops the runtime without disconnecting IPC. Keep
    /// this computer's parked chat, but stop admitting sends until the desktop
    /// has verified that the same store is Open again.
    pub(crate) fn begin_store_maintenance(&mut self, cx: &mut Context<Self>) {
        if self.host_recovery.store_maintenance || self.host_recovery.ended {
            return;
        }
        self.host_recovery.store_maintenance = true;
        self.host_recovery.epoch += 1;
        self.host_recovery.unavailable = true;
        self.host_recovery.events = None;
        self.booting = true;
        if self.first_send_waiting_for_mcp {
            self.fail_new_session(
                "Message not sent: local data is being updated. Send again when Maple is ready."
                    .into(),
                cx,
            );
        }
        if self.draft {
            self.draft_mcp_defaults = None;
        }
        self.begin_navigation();
        self.runtime_error = Some("Updating local data…".into());
        cx.emit(NavigationChanged);
        cx.notify();
    }

    /// Success, rejection and cancellation can all stop the runtime. Its readiness
    /// is independent of the password result; recover once from current host
    /// state, and never retry a prompt as part of restoring that runtime.
    pub(crate) fn end_store_maintenance(&mut self, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.host_recovery.store_maintenance) || self.host_recovery.ended {
            return;
        }
        self.recover_host(cx);
    }

    /// Rebuild presentation from the owner, preserving the composer and
    /// selection. Buffer a bounded stream while loading, then apply it after
    /// the snapshot. Overflow asks for another snapshot, never a bigger queue.
    fn recover_host(&mut self, cx: &mut Context<Self>) {
        if self.host_recovery.store_maintenance {
            return;
        }
        self.host_recovery.epoch += 1;
        let epoch = self.host_recovery.epoch;
        self.host_recovery.unavailable = true;
        self.host_recovery.events = Some(Vec::new());
        self.host_recovery.overflow = false;
        self.begin_navigation();
        let selection = self.selection_generation;
        let backend = self.backend.clone();
        let user = self.user_id.clone();
        let selected = self.selected_session.clone();
        let subscription_intent = backend.next_subscription_intent();
        self.runtime_error = None;
        self.call(
            async move {
                let boot = backend.navigation_bootstrap(&user).await?;
                let selected = match selected {
                    Some(task) => Some(
                        backend
                            .open_task(
                                &user,
                                crate::backend::OpenTaskPageRequest {
                                    task,
                                    limit_rows: None,
                                    limit_bytes: None,
                                },
                                subscription_intent,
                            )
                            .await?,
                    ),
                    None => None,
                };
                Ok::<_, String>((boot, selected))
            },
            cx,
            move |this, result, cx| {
                if this.host_recovery.epoch != epoch {
                    return;
                }
                if this.host_recovery.overflow {
                    this.recover_host(cx);
                    return;
                }
                match result {
                    Ok((boot, selected)) => {
                        this.persistence_status = boot.persistence_status.clone();
                        let status = boot.runtime_status.clone();
                        let selected_page = selected.as_ref().map(|detail| {
                            (
                                detail.before.is_some(),
                                detail.rows_epoch,
                                detail.plan.clone(),
                                detail.summaries.clone(),
                                detail.live_has_more,
                            )
                        });
                        let selected =
                            selected.map(|detail| maple_agent::agent::AgentSessionDetail {
                                timeline_revision: detail.timeline_revision,
                                session: detail.session,
                                timeline: detail
                                    .timeline
                                    .into_iter()
                                    .map(paging::normalize)
                                    .collect(),
                                queue: detail.queue,
                                mcp_errors: detail.mcp_errors,
                            });
                        let snapshot = crate::backend::RecoverySnapshot {
                            status,
                            selected,
                            bootstrap: crate::backend::LocalBootstrap {
                                project_root: boot.project_root,
                                sessions: Vec::new(),
                                recent_roots: sidebar::bootstrap_projects(
                                    boot.recent_projects,
                                    boot.default_project,
                                ),
                                latest: None,
                                questions: boot.pending_questions,
                            },
                        };
                        let events = this.host_recovery.events.take().unwrap_or_default();
                        let running = events
                            .iter()
                            .rev()
                            .find_map(|event| match event {
                                AgentServiceEvent::RuntimeStatus(status) => Some(status.running),
                                _ => None,
                            })
                            .unwrap_or(snapshot.status.running);
                        if this.selection_generation == selection
                            && let Some((_, _, _, summaries, _)) = &selected_page
                        {
                            this.tool_summaries.extend(
                                summaries
                                    .iter()
                                    .map(|(key, value)| (key.clone(), value.clone().into())),
                            );
                        }
                        // Metadata outside this bounded snapshot may have
                        // changed offline. Drop those projection caches before
                        // applying it, then replay newer buffered pushes below.
                        this.prepare_project_cache_for_recovery(
                            &snapshot.bootstrap.recent_roots,
                            cx,
                        );
                        this.apply_recovery_snapshot(snapshot, selection, cx);
                        if this.selection_generation == selection
                            && let Some((before, epoch, plan, _, live_has_more)) = selected_page
                        {
                            this.pages.generation += 1;
                            this.pages.before = before;
                            this.pages.epoch = Some(epoch);
                            this.pages.after = false;
                            this.pages.loading = false;
                            this.pages.live_has_more = live_has_more;
                            if let Some(plan) = plan.and_then(|item| plan_entries(&item)) {
                                this.set_plan(plan);
                            }
                        }
                        this.refresh_sessions(cx);
                        this.handle_service_events(events, cx);
                        this.host_recovery.unavailable = false;
                        this.runtime_error = None;
                        this.booting = !running;
                        this.check_unresolved_sends(cx);
                        this.dispatch_prepared_uploads(cx);
                        cx.emit(NavigationChanged);
                        // Warm connected computers before their first click.
                        // Catalog mode still suppresses transcript selection and
                        // execution events; warming never opens or runs a task.
                        if this.booting {
                            // A host another client restarted may not have a
                            // runtime yet. Restore catalog/tool availability;
                            // this never replays an admitted prompt. Prepared
                            // uploads still wait for this readiness result.
                            this.start_runtime(cx);
                        } else {
                            this.refresh_models(cx);
                            this.refresh_roots(cx);
                            this.refresh_slash_commands(cx);
                            this.refresh_sidebar_plan(cx);
                            this.refresh_audio_capabilities(cx);
                        }
                        this.finish_navigation_bootstrap(cx);
                    }
                    Err(error) => {
                        this.host_recovery.events = None;
                        this.runtime_error = Some(
                            format!(
                                "Could not reload host state: {error}. Reopen Maple before sending."
                            )
                            .into(),
                        );
                    }
                }
                cx.notify();
            },
        );
    }

    fn prepare_project_cache_for_recovery(
        &mut self,
        roots: &[RecentProjectRoot],
        cx: &mut Context<Self>,
    ) {
        self.sidebar.update(cx, |sidebar, _| {
            sidebar.prepare_project_cache_for_recovery(roots)
        });
    }

    fn apply_recovery_snapshot(
        &mut self,
        snapshot: crate::backend::RecoverySnapshot,
        selection: u64,
        cx: &mut Context<Self>,
    ) {
        let workspace = snapshot
            .bootstrap
            .recent_roots
            .iter()
            .find(|root| root.is_default_workspace)
            .cloned();
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.reconcile_default_project(workspace, cx)
        });
        // Revisions are scoped to a host incarnation. Even a restarted host
        // with lower revision numbers must supersede the old partial rows.
        self.snapshot_revision = 0;
        for item in &mut self.timeline {
            item.revision = 0;
        }
        self.loading_timeline.clear();
        self.finished_runs.clear();
        self.active_runs = snapshot.status.active_runs;
        self.pending_questions.clear();
        self.question_selected.clear();
        self.sessions = snapshot.bootstrap.sessions;
        self.apply_recent_roots(snapshot.bootstrap.recent_roots, cx);
        if self.navigation_only {
            self.project_root = snapshot.bootstrap.project_root;
            self.refresh_project_caption(cx);
        }
        self.handle_service_events(snapshot.bootstrap.questions, cx);
        if self.selection_generation == selection {
            if let Some(detail) = snapshot.selected {
                self.snapshot_revision = detail.timeline_revision;
                self.set_active_session(detail.session, detail.timeline, HashMap::new(), cx);
                self.set_queue(detail.queue.items);
            } else if self.selected_session.is_some() {
                // The selected task was deleted while this client was away.
                self.new_session(cx);
            }
        }
        self.sync_sidebar(cx);
        if self.draft {
            self.refresh_draft_mcp(cx);
        }
    }

    /// Apply one event; returns false when nothing visible changed.
    fn apply_service_event(&mut self, event: AgentServiceEvent, cx: &mut Context<Self>) -> bool {
        use maple_agent::agent::AgentRunEvent;
        if let AgentServiceEvent::PersistenceFailureChanged { status } = &event {
            if self.persistence_status.store_id.is_some()
                && (self.persistence_status.store_id != status.store_id
                    || self.persistence_status.revision > status.revision)
            {
                return false;
            }
            let changed = self.persistence_status != *status;
            self.persistence_status = status.clone();
            return changed;
        }
        // A computer that has not been opened needs navigation state only.
        // Do this before the recovery buffer: a busy transcript must not make
        // the summary snapshot overflow and reload forever.
        if self.navigation_only
            && match &event {
                AgentServiceEvent::TimelineItem { .. }
                | AgentServiceEvent::ThinkingChanged { .. }
                | AgentServiceEvent::SideQuestion { .. } => true,
                AgentServiceEvent::Run { event, .. } => !matches!(
                    event,
                    AgentRunEvent::Started
                        | AgentRunEvent::Finished(_)
                        | AgentRunEvent::SessionUpdated(_)
                ),
                _ => false,
            }
        {
            return false;
        }
        match event {
            AgentServiceEvent::HostReconnecting => {
                self.host_recovery.epoch += 1;
                self.host_recovery.unavailable = true;
                self.host_recovery.events = None;
                if self.first_send_waiting_for_mcp {
                    // Nothing was sent. Keep the text, but never resume this
                    // action automatically when a new connection arrives.
                    self.fail_new_session(
                        "Message not sent: the Maple host disconnected. Send again after reconnecting.".into(),
                        cx,
                    );
                }
                if self.draft {
                    self.draft_mcp_defaults = None;
                }
                self.begin_navigation();
                self.runtime_error =
                    Some("Connection lost. Reconnecting to the Maple host…".into());
                cx.emit(NavigationChanged);
                return true;
            }
            AgentServiceEvent::HostReconnected => {
                self.recover_host(cx);
                return true;
            }
            AgentServiceEvent::RemoteAccessEnded(reason) => {
                self.host_recovery.epoch += 1;
                self.host_recovery.unavailable = true;
                self.host_recovery.ended = true;
                self.host_recovery.events = None;
                self.runtime_error = Some(reason.into());
                cx.emit(NavigationChanged);
                return true;
            }
            AgentServiceEvent::HostDisconnected => {
                if self.host_recovery.ended {
                    return true;
                }
                self.host_recovery.ended = true;
                self.host_recovery.epoch += 1;
                self.host_recovery.unavailable = true;
                self.host_recovery.events = None;
                self.runtime_error = Some("The Maple host is unavailable. Reopen Maple to reload your tasks. No message was resent.".into());
                cx.emit(NavigationChanged);
                return true;
            }
            _ => {}
        }
        if let Some(events) = &mut self.host_recovery.events {
            if events.len() < 256 {
                events.push(event);
            } else {
                self.host_recovery.overflow = true;
            }
            return false;
        }
        let row = match &event {
            AgentServiceEvent::TimelineItem {
                session_id, item, ..
            } => Some((session_id, item)),
            AgentServiceEvent::Run {
                session_id,
                event:
                    maple_agent::agent::AgentRunEvent::TimelineItem(item)
                    | maple_agent::agent::AgentRunEvent::Error(item)
                    | maple_agent::agent::AgentRunEvent::QueuePromoted { item, .. },
                ..
            } => Some((session_id, item)),
            _ => None,
        };
        if let Some((session_id, item)) = row
            && self.loading_session.as_ref() == Some(session_id)
        {
            self.loading_timeline.insert(item.id.clone(), item.clone());
        }
        match event {
            AgentServiceEvent::LocalApiChanged
            | AgentServiceEvent::AccountChanged(_)
            | AgentServiceEvent::StoreStateChanged { .. }
            | AgentServiceEvent::PersistenceFailureChanged { .. } => {
                return false;
            }
            AgentServiceEvent::ProjectsChanged { user_id, roots } => {
                if user_id != self.user_id {
                    return false;
                }
                return self.merge_project_metadata(roots, cx);
            }
            AgentServiceEvent::ProjectRemoved {
                user_id,
                project_root,
            } => {
                if user_id != self.user_id {
                    return false;
                }
                self.recent_roots.retain(|root| root.path != project_root);
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.forget_project(&project_root, cx));
                cx.emit(NavigationChanged);
                return true;
            }
            AgentServiceEvent::HistoryReplaced { session_id, .. } => {
                if self.selected_session.as_deref() == Some(session_id.as_str()) {
                    self.reload_timeline(&session_id, cx);
                }
            }
            AgentServiceEvent::QuestionResolved { request_id } => {
                let showing = self
                    .current_question()
                    .is_some_and(|question| question.request_id == request_id);
                self.pending_questions
                    .retain(|question| question.request_id != request_id);
                if showing {
                    self.question_selected.clear();
                }
                self.sync_sidebar_activity(cx);
            }
            AgentServiceEvent::ThinkingChanged { session_id, state } => {
                self.apply_thinking_changed(&session_id, state, cx);
            }
            AgentServiceEvent::RemoteAccessEnded(_)
            | AgentServiceEvent::HostDisconnected
            | AgentServiceEvent::HostReconnecting
            | AgentServiceEvent::HostReconnected => return false,
            AgentServiceEvent::RuntimeStatus(mut status) => {
                // A runtime that just started points Goose at the account's
                // skills, which the first scan ran before; ask again.
                if status.running && !self.runtime_running_seen && !self.navigation_only {
                    self.runtime_running_seen = true;
                    self.refresh_slash_commands(cx);
                }
                // The status snapshot is authoritative for active runs; one
                // that repeats the known state changes nothing. A snapshot
                // raced with a terminal event must not resurrect that run.
                status
                    .active_runs
                    .retain(|_, run_id| !self.finished_runs.contains(run_id));
                if self.active_runs == status.active_runs {
                    return false;
                }
                self.active_runs = status.active_runs;
                // Run membership decides the inbox sections: a task woken
                // from elsewhere moves the moment the snapshot lands.
                self.sync_sidebar(cx);
            }
            AgentServiceEvent::SessionCreated(session) => {
                return self.upsert_session(session, cx);
            }
            AgentServiceEvent::SessionUpdated { session, .. } => {
                return self.upsert_session(session, cx);
            }
            AgentServiceEvent::TimelineItem {
                session_id,
                run_id,
                item,
            } => {
                self.observe_send_projection(&session_id, run_id.as_deref(), &item);
                if self.selected_session.as_deref() == Some(session_id.as_str()) {
                    self.apply_incoming_item(&session_id, item, cx);
                } else {
                    self.bump_timeline_revision(&session_id);
                    return false;
                }
            }
            AgentServiceEvent::Question {
                session_id,
                request_id,
                questions,
            } => {
                if self
                    .pending_questions
                    .iter()
                    .any(|queued| queued.request_id == request_id)
                {
                    return false;
                }
                self.notify_desktop(
                    &session_id,
                    "Maple has a question",
                    "Open Maple to respond.",
                    || {
                        questions
                            .first()
                            .map(|question| question.question.chars().take(140).collect())
                            .unwrap_or_default()
                    },
                    cx,
                );
                // Questions queue per session; one for a task that is not
                // on screen shows its card when the user switches there.
                let shows_now = self.current_question().is_none()
                    && self.selected_session.as_deref() == Some(session_id.as_str());
                self.pending_questions.push(PendingQuestion {
                    session_id,
                    request_id,
                    questions,
                });
                self.sync_sidebar_activity(cx);
                if shows_now {
                    // A fresh question takes the card; follow-ups queue
                    // behind it and keep the user's picks on this one.
                    self.question_selected.clear();
                    self.question_collapsed = false;
                    self.ensure_question_input(cx);
                    self.question_focus_pending = true;
                }
            }
            AgentServiceEvent::Run {
                session_id,
                run_id,
                event,
            } => return self.handle_run_event(&session_id, &run_id, event, cx),
            AgentServiceEvent::SideQuestion {
                request_id, event, ..
            } => {
                let Some(btw) = self.btw.as_mut() else {
                    return false;
                };
                if btw.request_id != request_id {
                    // A closed or replaced question; its stream is ignored.
                    return false;
                }
                match event {
                    SideQuestionEvent::Chunk(text) => {
                        if let Some(turn) = btw.turns.last_mut() {
                            turn.answer.push_str(&text);
                        }
                        btw.revision += 1;
                    }
                    SideQuestionEvent::Finished => btw.pending = false,
                    SideQuestionEvent::Error(message) => {
                        btw.pending = false;
                        btw.error = Some(message.into());
                    }
                }
            }
        }
        true
    }

    fn is_selected(&self, session_id: &str) -> bool {
        self.selected_session.as_deref() == Some(session_id)
    }

    /// Count a timeline event for a session that is not on screen. A
    /// snapshot load for that session in flight sees the moved revision
    /// and fetches again, so chunks that land while it loads are not lost.
    fn bump_timeline_revision(&mut self, session_id: &str) {
        *self
            .timeline_revisions
            .entry(session_id.to_string())
            .or_insert(0) += 1;
    }

    /// Drop every queued question of a session whose run ended.
    fn clear_session_questions(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let before = self.pending_questions.len();
        self.pending_questions
            .retain(|question| question.session_id != session_id);
        if self.pending_questions.len() != before {
            if self.is_selected(session_id) {
                self.reset_question_card(cx);
            }
            self.sync_sidebar_activity(cx);
        }
    }

    fn handle_run_event(
        &mut self,
        session_id: &str,
        run_id: &str,
        event: maple_agent::agent::AgentRunEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        use maple_agent::agent::AgentRunEvent;
        if let AgentRunEvent::TimelineItem(item) | AgentRunEvent::QueuePromoted { item, .. } =
            &event
        {
            self.observe_send_projection(session_id, Some(run_id), item);
        }
        match event {
            AgentRunEvent::SessionUpdated(session) => {
                return self.upsert_session(session, cx);
            }
            AgentRunEvent::Started => {
                self.active_runs
                    .insert(session_id.to_string(), run_id.to_string());
                // The sections read `active_runs`; keep them in step.
                self.sync_sidebar(cx);
                self.start_usage_poller(session_id.to_string(), cx);
                self.refresh_context_usage(cx);
            }
            AgentRunEvent::TimelineItem(item) => {
                if self.is_selected(session_id) {
                    self.apply_incoming_item(session_id, item, cx);
                } else {
                    self.bump_timeline_revision(session_id);
                    return false;
                }
            }
            AgentRunEvent::SubagentStarted {
                id,
                task,
                background,
                external,
            } => {
                if !self.is_selected(session_id) {
                    return false;
                }
                self.start_subagent(id, task, background, external, cx);
            }
            AgentRunEvent::SubagentActivity { id, tool } => {
                if !self.is_selected(session_id) {
                    return false;
                }
                let Some(subagent) = self.subagents.iter_mut().find(|subagent| subagent.id == id)
                else {
                    return false;
                };
                let tool = SharedString::from(tool);
                if subagent.activity.as_ref() == Some(&tool) {
                    return false;
                }
                subagent.activity = Some(tool);
            }
            AgentRunEvent::SubagentFinished { id } => {
                if !self.is_selected(session_id) {
                    return false;
                }
                let before = self.subagents.len();
                self.subagents.retain(|subagent| subagent.id != id);
                if self.subagents.len() == before {
                    return false;
                }
                self.subagent_epoch += 1;
            }
            AgentRunEvent::SetupWarning(message) => {
                if self.is_selected(session_id) {
                    self.notice = Some(message.into());
                }
            }
            // The reload that follows shows the compaction's notice.
            AgentRunEvent::Compacting | AgentRunEvent::Compacted => return false,
            AgentRunEvent::HistoryReplaced => {
                if self.is_selected(session_id) {
                    // Replace history only; the run keeps flowing.
                    let session_id = session_id.to_string();
                    self.reload_timeline(&session_id, cx);
                }
            }
            AgentRunEvent::Error(item) => {
                if self.is_selected(session_id) {
                    self.load_attachment_images_for(&item, cx);
                    self.apply_timeline_item(session_id, item);
                } else {
                    self.bump_timeline_revision(session_id);
                    return false;
                }
            }
            AgentRunEvent::Finished(terminal) => {
                self.finish_send_preview(session_id, run_id);
                if self.finished_runs.len() >= FINISHED_RUNS_KEPT {
                    self.finished_runs.pop_front();
                }
                self.finished_runs.push_back(run_id.to_string());
                // Only retire the run that actually finished; a late
                // Finished from a cancelled run must not clear a newer one.
                let owns_session = self
                    .active_runs
                    .get(session_id)
                    .is_none_or(|active| active == run_id);
                if owns_session {
                    self.active_runs.remove(session_id);
                    // The markers follow the latest terminal: a completion
                    // or a failure off screen sets one; a stop sets none.
                    // Opening the task (`set_active_session`) is the only
                    // other writer.
                    self.mark_seen(session_id);
                    if !(self.visible && self.is_selected(session_id)) {
                        match terminal {
                            maple_agent::agent::AgentRunTerminal::Completed => {
                                self.completed_unread_sessions
                                    .insert(session_id.to_string());
                            }
                            maple_agent::agent::AgentRunTerminal::Failed => {
                                self.failed_unread_sessions.insert(session_id.to_string());
                            }
                            maple_agent::agent::AgentRunTerminal::Cancelled => {}
                        }
                    }
                    // Waking or retiring a run moves a task between
                    // sections; the entries must follow the same frame.
                    self.sync_sidebar(cx);
                    // A subagent the run was waiting for ended with it. A
                    // background one works on and is collected by a later
                    // turn, so its row stays.
                    if self.is_selected(session_id) {
                        let before = self.subagents.len();
                        self.subagents.retain(|subagent| subagent.background);
                        if self.subagents.len() != before {
                            self.subagent_epoch += 1;
                        }
                    }
                    // The watcher covers checkouts; this catches a
                    // change that landed between two events.
                    self.read_branch(cx);
                    // The run that asked is gone (stopped or failed): its
                    // questions would block the composer forever.
                    self.clear_session_questions(session_id, cx);
                }
                if self.is_selected(session_id) {
                    // Another task finishing must not hide the dots for
                    // the send that is still waiting here.
                    self.awaiting_first_token = false;
                    self.refresh_context_usage(cx);
                    // A run that ended mid-thought leaves the thinking
                    // block newest; the run's end finalizes it.
                    if let Some(last) = self.timeline.len().checked_sub(1) {
                        self.maybe_summarize_thinking(last, cx);
                    }
                }
                self.notify_desktop(
                    session_id,
                    "Maple",
                    "A task finished. Open Maple to view it.",
                    || {
                        let title = self
                            .sessions
                            .iter()
                            .find(|session| session.id == session_id)
                            .map(|session| session.title.as_str())
                            .unwrap_or("Task");
                        format!("“{title}” finished")
                    },
                    cx,
                );
                self.refresh_sidebar_plan(cx);
            }
            AgentRunEvent::QueueChanged(snapshot) => {
                if !self.is_selected(session_id) {
                    return false;
                }
                self.set_queue(snapshot.items);
            }
            AgentRunEvent::QueuePromoted { snapshot, item, .. } => {
                if !self.is_selected(session_id) {
                    return false;
                }
                self.set_queue(snapshot.items);
                // The promoted message is a user turn like any other: its
                // attachments load and the plan and summaries follow it.
                self.apply_incoming_item(session_id, item, cx);
            }
        }
        true
    }
}

impl EventEmitter<crate::update::UpdateRequested> for ChatScreen {}
impl EventEmitter<LoggedOut> for ChatScreen {}
impl EventEmitter<SignOutFailed> for ChatScreen {}

impl EventEmitter<OpenSettings> for ChatScreen {}
impl EventEmitter<OpenSettingsSection> for ChatScreen {}
impl EventEmitter<crate::ui::store_gate::RetryStorePersistence> for ChatScreen {}

/// Height hint for transcript rows that have not been measured yet, so a
/// freshly opened task has a scrollbar of about the right size on its
/// first frame. Rows are measured as they scroll into view.
const TRANSCRIPT_ROW_ESTIMATE: gpui::Pixels = px(96.);

/// The transcript list: bottom-aligned, following its tail so streaming
/// content stays in view until the user scrolls up, with enough overdraw
/// that a fast wheel scroll lands on measured rows.
fn same_timeline_occurrence(a: &AgentTimelineItem, b: &AgentTimelineItem) -> bool {
    match (&a.persisted, &b.persisted) {
        (Some(a), Some(b)) => {
            a.task_id == b.task_id && a.rows_epoch == b.rows_epoch && a.row_seq == b.row_seq
        }
        _ => match (&a.live, &b.live) {
            (Some(a), Some(b)) => a == b,
            _ => {
                a.persisted.is_none()
                    && b.persisted.is_none()
                    && a.live.is_none()
                    && b.live.is_none()
                    && a.id == b.id
            }
        },
    }
}

fn transcript_list_state() -> gpui::ListState {
    let state = gpui::ListState::new(0, gpui::ListAlignment::Bottom, px(1024.));
    state.set_follow_mode(gpui::FollowMode::Tail);
    state
}

/// The update mark beside Settings and on its menu row: a download glyph
/// on the accent, or a muted alert when the update failed.
fn update_badge(size: gpui::Pixels, failed: bool) -> Div {
    let badge = div()
        .flex_none()
        .size(size)
        .rounded_full()
        .flex()
        .items_center()
        .justify_center();
    if failed {
        badge
            .border_1()
            .border_color(gpui::rgb(theme::border()))
            .child(icon("circle-alert", size * 0.67, theme::text_muted()))
    } else {
        badge
            .bg(gpui::rgb(theme::accent()))
            .child(icon("download", size * 0.6, theme::on_accent()))
    }
}

impl Render for ChatScreen {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Refreshed every frame; activation changes force a redraw, so
        // this tracks focus closely enough to gate notifications.
        self.window_active = window.is_window_active();
        let vim_in_sidebar = self.application_vim_enabled
            && self.application_vim.region == self::navigation::ChatRegion::Sidebar;
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.set_vim_active(vim_in_sidebar, cx));
        // First, so a dialog's focus request below wins over a menu handing
        // focus back.
        self.popup.sync_focus(window, cx);
        if let Some(dialog) = self.project_folder_dialog.clone() {
            self.sidebar
                .update(cx, |sidebar, cx| sidebar.sync_popup_focus(window, cx));
            dialog.update(cx, |dialog, cx| dialog.sync_focus(window, cx));
        }
        if self.dialog_focus_pending && self.project_folder_dialog.is_none() {
            self.dialog_focus_pending = false;
            if let Some(handle) = self.dialog_focus.clone() {
                window.focus(&handle, cx);
            }
        }
        // A window with no focused element has no dispatch path: menu
        // items validate as unavailable and shortcuts fall through. The
        // login screen's fields leaving, or a rename field closing, can
        // leave it that way; the screen takes focus back.
        if window.focused(cx).is_none() {
            self.screen_focus_pending = true;
        }
        if self.screen_focus_pending && self.project_folder_dialog.is_none() {
            self.screen_focus_pending = false;
            if self.application_vim_enabled {
                if let Some(handle) = self.application_focus.clone() {
                    window.focus(&handle, cx);
                }
            } else if let Some(composer) = self.composer.clone() {
                let handle = composer.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
        }
        if self.question_focus_pending && self.project_folder_dialog.is_none() {
            self.question_focus_pending = false;
            if self.application_vim_enabled {
                if let Some(handle) = self.application_focus.clone() {
                    window.focus(&handle, cx);
                }
            } else if self.current_question().is_some()
                && let Some(input) = self.pending_question_input.clone()
            {
                let handle = input.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
        }
        self.focus_feedback(window, cx);
        let feedback = self.render_feedback(cx);
        let confirm = self
            .confirm
            .clone()
            .map(|dialog| self.render_confirm(&dialog, cx));
        let trust_prompt = self
            .trust_prompt
            .clone()
            .map(|status| self.render_trust_prompt(&status, cx));
        let empty = self.shows_empty_state();
        let sidebar_expansion = self.sidebar_expansion(window, cx);
        let main = if empty {
            self.render_empty_state(window, cx)
        } else {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .h_full()
                .min_w_0()
                // Whatever the pointer is over, a wheel that nothing there
                // takes scrolls the transcript.
                .on_scroll_wheel(cx.listener(Self::scroll_transcript_from_pane))
                .child(self.render_transcript(window, cx))
                .children(self.render_send_preview())
                .when(
                    self.awaiting_first_token && self.is_run_active() && !self.has_send_feedback(),
                    |main| {
                        // Same gutter as transcript text so the dots line up
                        main.child(
                            div()
                                .w_full()
                                .max_w(CONTENT_WIDTH)
                                .mx_auto()
                                .px_6()
                                .child(render_waiting_indicator()),
                        )
                    },
                )
                .when_some(self.current_question(), |container, question| {
                    let input = self.pending_question_input.clone();
                    let step = self.question_step;
                    container.child(render_question_card(
                        question,
                        step,
                        self.question_collapsed,
                        input,
                        &self.question_selected,
                        cx,
                    ))
                })
                .child(
                    div()
                        .relative()
                        .w_full()
                        .max_w(CONTENT_WIDTH)
                        .mx_auto()
                        .px_6()
                        .pb_4()
                        .when(self.composer_expanded, |wrap| {
                            wrap.flex_none()
                                .h(gpui::relative(0.7))
                                .min_h_0()
                                .flex()
                                .flex_col()
                        })
                        // The composer, its cards and the palette keep their
                        // own wheel; the space beside them scrolls the
                        // transcript.
                        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                        .children(self.render_btw_card(cx))
                        .children(self.render_subagents_card(cx))
                        .children(self.render_plan_card(cx))
                        .child(self.render_composer(window, cx))
                        .children(self.render_slash_palette(cx))
                        .children(self.render_persistence_failure(cx)),
                )
        };
        div()
            .key_context(self.application_vim_context())
            .when_some(
                self.application_vim_enabled
                    .then(|| self.application_focus.clone())
                    .flatten(),
                |root, focus| root.track_focus(&focus),
            )
            .on_action(cx.listener(Self::chat_escape))
            .on_action(cx.listener(Self::copy_selection))
            .on_action(cx.listener(Self::new_task_action))
            .on_action(cx.listener(Self::focus_search))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_archived))
            .on_action(cx.listener(Self::open_app_settings))
            .on_action(cx.listener(Self::choose_project))
            .on_action(cx.listener(Self::previous_task))
            .on_action(cx.listener(Self::next_task))
            .on_action(cx.listener(Self::pick_question_option))
            .on_action(cx.listener(Self::select_all_transcript))
            .on_action(cx.listener(Self::app_vim_next))
            .on_action(cx.listener(Self::app_vim_previous))
            .on_action(cx.listener(Self::app_vim_first))
            .on_action(cx.listener(Self::app_vim_last))
            .on_action(cx.listener(Self::app_vim_activate))
            .on_action(cx.listener(Self::app_vim_collapse))
            .on_action(cx.listener(Self::app_vim_expand))
            .on_action(cx.listener(Self::app_vim_copy))
            .on_action(cx.listener(Self::app_vim_search))
            .on_action(cx.listener(Self::app_vim_escape))
            .on_action(cx.listener(Self::app_vim_composer))
            .on_action(cx.listener(Self::app_vim_newest_assistant))
            .on_action(cx.listener(Self::app_vim_next_assistant))
            .on_action(cx.listener(Self::app_vim_previous_assistant))
            .on_action(cx.listener(Self::app_vim_next_annotation))
            .on_action(cx.listener(Self::app_vim_previous_annotation))
            .on_action(cx.listener(Self::app_vim_count))
            .on_action(cx.listener(Self::app_vim_move_region))
            .on_key_down(cx.listener(Self::type_into_composer))
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(gpui::rgb(theme::bg_app()))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(self.render_sidebar(sidebar_expansion, window, cx))
                    .child(
                        div()
                            .relative()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .bg(gpui::rgb(theme::bg_sidebar()))
                            .child(
                                // The header is the window's title bar; a wheel
                                // over it scrolls nothing.
                                self.render_header(sidebar_expansion, window, cx)
                                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation()),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_h_0()
                                    .rounded_tl(theme::RADIUS_LG)
                                    .bg(gpui::rgb(theme::bg_app()))
                                    .child(main),
                            ),
                    ),
            )
            .when_some(self.lightbox.clone(), |root, image| {
                root.child(motion::fade_in(
                    div()
                        .id("lightbox")
                        // Covers the screen: the pointer, wheel included,
                        // reaches nothing underneath.
                        .occlude()
                        .absolute()
                        .size_full()
                        .top_0()
                        .left_0()
                        .bg(theme::scrim())
                        .flex()
                        .items_center()
                        .justify_center()
                        .hover(|style| style.cursor_pointer())
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.lightbox = None;
                            cx.notify();
                        }))
                        .child(
                            gpui::img(gpui::ImageSource::Image(image))
                                .max_w(gpui::relative(0.9))
                                .max_h(gpui::relative(0.9))
                                .rounded(theme::RADIUS_MD)
                                .border_1()
                                .border_color(gpui::rgb(theme::border()))
                                .shadow_lg(),
                        )
                        .child(
                            div().absolute().top_3().right_3().child(
                                widgets::icon_button_for_action(
                                    "lightbox-close",
                                    "x",
                                    "Close",
                                    px(16.),
                                    theme::on_accent(),
                                    &ChatEscape,
                                )
                                .size_8()
                                .rounded_full()
                                .bg(theme::overlay_hover()),
                            ),
                        ),
                    "lightbox-reveal",
                ))
            })
            .children(confirm)
            .children(feedback)
            .children(trust_prompt)
            .children(self.project_folder_dialog.clone().map(|dialog| {
                dialog.cached(
                    gpui::StyleRefinement::default()
                        .absolute()
                        .inset_0()
                        .size_full(),
                )
            }))
    }
}

impl ChatScreen {
    fn render_sidebar(
        &mut self,
        expansion: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let width = SIDEBAR_RAIL_WIDTH + (SIDEBAR_WIDTH - SIDEBAR_RAIL_WIDTH) * expansion;
        div()
            .relative()
            .flex_none()
            .w(width)
            .h_full()
            .bg(gpui::rgb(theme::bg_sidebar()))
            // Clip only during motion: settled sidebar menus may extend
            // into the content pane as before.
            .when(expansion > 0. && expansion < 1., |column| {
                column.overflow_hidden()
            })
            .when(expansion < 1., |column| {
                column.child(self.render_sidebar_rail(cx))
            })
            .when(expansion > 0., |column| {
                column.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .w(SIDEBAR_WIDTH)
                        .h_full()
                        .opacity(expansion)
                        // Fixed bounds keep labels from wrapping as the
                        // outer column narrows; settled frames stay cached.
                        .child(
                            self.sidebar.clone().cached(
                                gpui::StyleRefinement::default()
                                    .w(SIDEBAR_WIDTH)
                                    .h_full()
                                    .flex_none(),
                            ),
                        ),
                )
            })
            .child(self.render_sidebar_settings(expansion, window, cx))
    }

    fn render_sidebar_settings(
        &mut self,
        expansion: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let expanded = expansion > 0.95;
        let open = self.popup.is_open(&ChatPopup::Settings);
        let update = self.update_notice();
        let button = widgets::ghost_button("open-settings")
            .aria_label(if update.is_some() {
                "Settings menu, update available"
            } else {
                "Settings menu"
            })
            .a11y_synthetic_children(|tree| {
                tree.parent_node()
                    .set_has_popup(gpui::accesskit::HasPopup::Menu);
            })
            .debug_selector(|| "open-settings".to_string())
            .w_full()
            .h(px(40.))
            .px_2()
            .rounded(theme::RADIUS_SM)
            .tab_index(6)
            .on_key_down(Self::sidebar_button_key_down)
            .when(open, |button| {
                button.bg(gpui::rgb(theme::bg_sidebar_row_hover()))
            })
            .child(
                div()
                    .relative()
                    .flex_none()
                    .child(icon("settings", px(16.), theme::text_secondary()))
                    // Collapsed, a dot on the gear stands in for the badge.
                    .when(!expanded && update.is_some(), |gear| {
                        gear.child(
                            div()
                                .debug_selector(|| "settings-update-dot".to_string())
                                .absolute()
                                .top(px(-2.))
                                .right(px(-2.))
                                .size(px(6.))
                                .rounded_full()
                                .bg(gpui::rgb(theme::accent())),
                        )
                    }),
            )
            .when(expanded, |button| {
                button
                    .justify_start()
                    .gap_2()
                    .child("Settings")
                    .when_some(update, |button, (_, failed)| {
                        button.child(
                            update_badge(px(18.), failed)
                                .debug_selector(|| "settings-update-badge".to_string()),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        icon("chevron-down", px(12.), theme::text_muted()).when(!open, |icon| {
                            icon.with_transformation(gpui::Transformation::rotate(gpui::radians(
                                std::f32::consts::PI,
                            )))
                        }),
                    )
            });
        let button = self
            .popup
            .trigger(ChatPopup::Settings, button, cx, |this, _, cx| {
                this.popup.toggle(ChatPopup::Settings, cx);
            });
        let mut footer = div()
            .absolute()
            .bottom_0()
            .left_0()
            .w(if expanded {
                SIDEBAR_WIDTH
            } else {
                SIDEBAR_RAIL_WIDTH
            })
            .h(px(54.))
            .px_2()
            .py(px(6.))
            .border_t_1()
            .border_color(gpui::rgb(theme::border_subtle()))
            .bg(gpui::rgb(theme::bg_sidebar()))
            .child(button);
        if open {
            let usage_label = self.sidebar_plan.as_ref().map_or_else(
                || "Usage unavailable. Open Usage settings".to_string(),
                |plan| {
                    format!(
                        "Usage, {} percent used. {}, resets {}. Open Usage settings",
                        plan.percent_used, plan.plan_label, plan.resets_label
                    )
                },
            );
            let usage = div()
                .w_full()
                .py_2()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child("Usage"),
                        )
                        .children(self.sidebar_plan.as_ref().map(|plan| {
                            div()
                                .text_xs()
                                .text_color(gpui::rgb(theme::text_secondary()))
                                .child(format!("{}% used", plan.percent_used))
                        }))
                        .child(icon("chevron-right", px(12.), theme::text_muted())),
                )
                .when_some(self.sidebar_plan.as_ref(), |usage, plan| {
                    usage
                        .child(
                            div()
                                .debug_selector(|| "settings-usage-meter".to_string())
                                .h(px(6.))
                                .w_full()
                                .rounded_full()
                                .bg(gpui::rgb(theme::border()))
                                .child(
                                    div()
                                        .h_full()
                                        .w(gpui::relative(
                                            (f32::from(plan.percent_used) / 100.).clamp(0., 1.),
                                        ))
                                        .rounded_full()
                                        .bg(gpui::rgb(theme::accent())),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_between()
                                .gap_2()
                                .text_xs()
                                .text_color(gpui::rgb(theme::text_muted()))
                                .child(plan.plan_label.clone())
                                .child(format!("Resets {}", plan.resets_label)),
                        )
                })
                .when(self.sidebar_plan.is_none(), |usage| {
                    usage.child(
                        div()
                            .debug_selector(|| "settings-usage-unavailable".to_string())
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child("Usage unavailable"),
                    )
                });
            let menu = Menu::new("settings-menu", SIDEBAR_WIDTH - px(16.))
                .label("Settings")
                .application_vim(self.application_vim_enabled)
                .item(
                    MenuItem::new("settings-menu-usage", usage_label, |_: &mut Self, _, cx| {
                        cx.emit(OpenSettingsSection(Section::Usage));
                    })
                    .content(usage),
                )
                .separator()
                .items(update.map(|(label, failed)| {
                    MenuItem::new("settings-menu-update", label, |this: &mut Self, _, cx| {
                        this.open_update(cx);
                    })
                    .content(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            // The badge overhangs the icon column by 2 px a
                            // side, so its label lines up with the rows below.
                            .child(update_badge(px(18.), failed).mx(px(-2.)))
                            .child(label),
                    )
                }))
                .item(
                    MenuItem::new("settings-menu-account", "Account", |_: &mut Self, _, cx| {
                        cx.emit(OpenSettingsSection(Section::Account));
                    })
                    .icon("user"),
                )
                .item(
                    MenuItem::new(
                        "settings-menu-all",
                        "All settings",
                        |this: &mut Self, window, cx| {
                            this.execute_command(ChatCommand::OpenSettings, window, cx);
                        },
                    )
                    .icon("settings"),
                )
                .item(
                    MenuItem::new(
                        "send-feedback",
                        "Send feedback",
                        |this: &mut Self, window, cx| {
                            this.open_feedback(window, cx);
                        },
                    )
                    .icon("message-square"),
                );
            footer = footer.child(self.popup.render(menu, Placement::AboveStart, window, cx));
        }
        footer
    }

    /// Keep unsaved-work feedback beside the composer that produced it.
    /// The desktop owns retries so the local store's lifecycle guards still apply.
    fn render_persistence_failure(&self, cx: &mut Context<Self>) -> Option<Div> {
        let failure = self.persistence_status.failure.as_ref()?;
        let remote = self.backend.remote_host().is_some();
        Some(
            div()
                .debug_selector(|| "chat-persistence-feedback".to_string())
                .w_full()
                .pt_2()
                .child(
                    widgets::banner(theme::status_error())
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap_2()
                        .child(failure.message())
                        .when(remote, |panel| {
                            panel.child("Retry saving from Maple on that computer.")
                        })
                        .when(!remote, |panel| {
                            panel.child(
                                widgets::secondary_button("retry-local-store-persistence")
                                    .debug_selector(|| "retry-local-store-persistence".to_string())
                                    .child("Retry saving")
                                    .on_click(cx.listener(|_this, _event, _window, cx| {
                                        cx.emit(crate::ui::store_gate::RetryStorePersistence);
                                    })),
                            )
                        }),
                ),
        )
    }

    /// Tab moves among the rail and header buttons. Space presses the
    /// focused button on release, rather than typing into the composer.
    fn sidebar_button_key_down(
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        if event.keystroke.key == "space" {
            cx.stop_propagation();
        } else if event.keystroke.key == "tab"
            && event
                .keystroke
                .modifiers
                .is_subset_of(&gpui::Modifiers::shift())
        {
            if event.keystroke.modifiers.shift {
                window.focus_prev(cx);
            } else {
                window.focus_next(cx);
            }
            cx.stop_propagation();
        }
    }

    /// The collapsed sidebar: a slim rail in the sidebar's color whose
    /// Maple mark shows the sidebar again.
    fn render_sidebar_rail(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let mark_padding = (sidebar::TOP_ROW_HEIGHT - sidebar::WORDMARK_HEIGHT) / 2.;
        let rail = titlebar::drag_region(
            div()
                .id("sidebar-rail")
                .debug_selector(|| "sidebar-rail".to_string())
                .relative()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .bg(gpui::rgb(theme::bg_sidebar())),
        )
        .child(
            div()
                .id("show-sidebar")
                .debug_selector(|| "show-sidebar".to_string())
                .absolute()
                .left(sidebar::WORDMARK_LEFT - mark_padding)
                .top(sidebar::wordmark_row_top() - CHAT_HEADER_HEIGHT)
                .role(gpui::Role::Button)
                .aria_label("Show sidebar")
                // The search field's place in the tab order, which the rail
                // holds while the sidebar is collapsed.
                .tab_index(2)
                .flex_none()
                .size(sidebar::TOP_ROW_HEIGHT)
                .overflow_hidden()
                .flex()
                .items_center()
                .pl(mark_padding)
                .rounded(theme::RADIUS_SM)
                .focus_visible(widgets::focus_ring)
                .hover(|style| {
                    style
                        .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                        .cursor_pointer()
                })
                .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_row_selected())))
                // Inside a drag region: a press here is a click, not a drag.
                .on_mouse_down(gpui::MouseButton::Left, |_event, _window, cx| {
                    cx.stop_propagation();
                })
                .on_key_down(Self::sidebar_button_key_down)
                .tooltip_show_delay(widgets::ICON_TOOLTIP_DELAY)
                .tooltip(widgets::tooltip_for_action("Show sidebar", &ToggleSidebar))
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.execute_command(ChatCommand::ToggleSidebar, window, cx);
                }))
                .child(
                    crate::ui::icons::wordmark(sidebar::WORDMARK_HEIGHT, theme::text_sidebar())
                        .path("icons/maple-mark.svg")
                        .debug_selector(|| "sidebar-rail-mark".to_string()),
                ),
        );
        div()
            .id("sidebar-rail-column")
            .flex_none()
            .w(SIDEBAR_RAIL_WIDTH)
            .h_full()
            .flex()
            .flex_col()
            .child(titlebar::drag_region(
                div()
                    .id("sidebar-rail-titlebar")
                    .h(CHAT_HEADER_HEIGHT)
                    .flex_none()
                    .bg(gpui::rgb(theme::bg_sidebar())),
            ))
            .child(rail)
    }

    /// Hero layout for a task with no messages: display heading, composer,
    /// and privacy note centered in the pane (mirrors EmptyAgentState).
    fn render_empty_state(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let expanded = self.composer_expanded;
        let body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .items_center()
            .justify_center()
            .px_6()
            .when(expanded, |pane| pane.py_6())
            .child(
                div()
                    .w_full()
                    .max_w(CONTENT_WIDTH)
                    .px_2()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_6()
                    .when(expanded, |column| column.h_full().min_h_0())
                    .when(!expanded, |column| {
                        column.child(
                            div()
                                .mb_6()
                                .font_family(crate::assets::FONT_DISPLAY)
                                .text_size(px(36.))
                                .line_height(px(48.))
                                .text_color(gpui::rgb(theme::display_text()))
                                .child("Work on anything..."),
                        )
                    })
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .when(expanded, |wrap| wrap.flex_1().min_h_0().flex().flex_col())
                            .child(self.render_composer(window, cx))
                            .children(self.render_slash_palette(cx)),
                    )
                    .when(
                        self.awaiting_first_token && self.is_run_active(),
                        |column| column.child(render_waiting_indicator()),
                    )
                    .when(!expanded, |column| {
                        column.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_xs()
                                .text_color(gpui::rgb(theme::text_muted()))
                                .child(icon("lock", px(12.), theme::text_muted()))
                                .child("Encrypted and private at every step"),
                        )
                    })
                    .when_some(self.runtime_error.clone(), |column, error| {
                        column.child(widgets::banner(theme::status_error()).child(error))
                    })
                    .children(self.render_persistence_failure(cx))
                    .children(self.render_notice(cx)),
            );
        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(body)
    }

    /// Replace the plan and the completed count the card header shows.
    /// A finished plan from history shows no card.
    fn set_plan(&mut self, plan: Vec<PlanEntry>) {
        self.plan_done = plan
            .iter()
            .filter(|entry| entry.status == PlanStatus::Completed)
            .count();
        self.plan = plan;
        self.plan_hold = None;
    }

    /// Replace the plan from a live update. When the update finishes the
    /// plan on screen, the card keeps the final check for
    /// `PLAN_DONE_HOLD`, then goes; the finished list stays in the
    /// transcript.
    fn update_live_plan(&mut self, plan: Vec<PlanEntry>, cx: &mut Context<Self>) {
        let showing = self.plan_card_visible();
        // A repeat of the finished list keeps the running hold.
        let hold = self.plan_hold.take();
        self.set_plan(plan);
        if showing && plan_finished(&self.plan) {
            self.plan_hold = hold.or_else(|| {
                Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(PLAN_DONE_HOLD).await;
                    this.update(cx, |this, cx| {
                        this.plan_hold = None;
                        cx.notify();
                    })
                    .ok();
                }))
            });
        }
    }

    /// The pinned card shows an unfinished plan, and a finished one only
    /// while it holds its final check.
    fn plan_card_visible(&self) -> bool {
        !self.plan.is_empty() && (self.plan_done < self.plan.len() || self.plan_hold.is_some())
    }

    /// Replace the session's MCP servers and the enabled count the
    /// composer chip shows.
    fn set_session_mcp(&mut self, servers: Vec<AgentSessionMcpServer>) {
        self.mcp_enabled_count = servers.iter().filter(|server| server.enabled).count();
        self.session_mcp = servers;
    }

    /// Cache the header title for the selected task.
    /// Cache the header title for the selected task. The header names what
    /// the pane shows: with no transcript on screen it is a new task,
    /// whatever the list has selected.
    fn refresh_selected_title(&mut self) {
        if self.timeline.is_empty() && self.loading_session.is_none() {
            self.selected_title = DEFAULT_TASK_TITLE.into();
            return;
        }
        self.selected_title = self
            .selected_session
            .as_deref()
            .and_then(|selected| {
                self.sessions
                    .iter()
                    .position(|session| session.id == selected)
            })
            .map(|index| SharedString::from(self.sessions[index].title.clone()))
            .unwrap_or_else(|| DEFAULT_TASK_TITLE.into());
    }

    /// Raise a desktop notification when enabled and the window is not
    /// focused. `tag` names the task, so a later alert about the same task
    /// replaces the earlier one instead of stacking.
    fn notify_desktop(
        &self,
        tag: &str,
        title: &str,
        generic_body: &str,
        details: impl FnOnce() -> String,
        cx: &gpui::App,
    ) {
        if !self.notify_enabled {
            log::info!("desktop notification skipped (disabled): {title}");
            return;
        }
        if self.window_active {
            log::info!("desktop notification skipped (window active): {title}");
            return;
        }
        log::info!("desktop notification sent: {title}");
        let tag = match self.backend.remote_host() {
            Some(host) => format!("remote-task:{host}:{tag}"),
            None => format!("task:{tag}"),
        };
        let body = notification_body(self.notify_details, generic_body, details);
        crate::notify::notify(cx, tag, title, &body, &[]);
    }

    /// Load the usage preview from the Maple billing API. Failures keep
    /// the previous value; without one, the menu shows usage as unavailable.
    fn refresh_sidebar_plan(&mut self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.plan_usage(&user_id).await },
            cx,
            |this, result: Result<Option<crate::billing::PlanUsage>, String>, cx| match result {
                Ok(plan) => {
                    this.sidebar_plan = plan;
                    cx.notify();
                }
                Err(error) => log::warn!("plan usage unavailable: {error}"),
            },
        );
    }
}

/// The directory that holds `HEAD` for a checkout, or `None` when `root`
/// is not one. Supports worktrees, whose `.git` is a file that points at
/// the real git dir.
fn git_dir(root: &std::path::Path) -> Option<std::path::PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let pointer = std::fs::read_to_string(&dot_git).ok()?;
    let target = pointer.trim().strip_prefix("gitdir:")?.trim();
    let target = std::path::Path::new(target);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        root.join(target)
    })
}

/// Current git branch from a git dir, or the short commit id when HEAD
/// is detached. `None` when there is no readable `HEAD`.
fn git_branch(git_dir: &std::path::Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        Some(reference) => Some(
            reference
                .strip_prefix("refs/heads/")
                .unwrap_or(reference)
                .to_string(),
        ),
        // Detached: a hex id. Anything else is a corrupt HEAD.
        None => head
            .get(..7)
            .filter(|id| id.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .map(str::to_string),
    }
}

/// True when a watcher event means the branch may have changed: a semantic
/// change to a `HEAD` path (write, create, remove, or the rename pair of an
/// atomic replacement), or a rescan the backend requires. Access-only events
/// (open, read, close) are dropped: the branch read they would trigger emits
/// those same events again under Linux inotify, looping the watcher at full
/// CPU while idle (#945). Real writes still arrive as `Modify` on every
/// backend, so no true change is lost; `Any`/`Other` stay forwarded for
/// backends that cannot classify.
fn head_change_event(event: &notify::Event) -> bool {
    if event.need_rescan() {
        return true;
    }
    if matches!(event.kind, notify::EventKind::Access(_)) {
        return false;
    }
    event
        .paths
        .iter()
        .any(|path| path.file_name().is_some_and(|name| name == "HEAD"))
}

/// Section heading in the sidebar.
fn section_label(text: &'static str) -> Div {
    div()
        .text_sm()
        .text_color(gpui::rgb(theme::text_muted()))
        .child(text)
}
