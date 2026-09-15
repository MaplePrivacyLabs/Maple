//! The agent chat surface: session sidebar, streaming transcript with tool
//! calls, permission prompts, and the composer. Pure consumer of the backend
//! facade + event stream.

use crate::settings::PermissionMode;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{
    AppContext, Div, Entity, EntityInputHandler, EventEmitter, Focusable, Render, SharedString,
    Window, div, prelude::*, px,
};
use maple_agent::agent::{
    AgentCreateSessionRequest, AgentImageUpload, AgentProjectTrustStatus, AgentQueuedMessage,
    AgentSendMessageRequest, AgentServiceEvent, AgentSessionMcpServer, AgentSessionSummary,
    AgentSlashCommand, AgentSubagent, AgentTimelineItem, SideQuestionEvent,
};

use crate::backend::{AgentBackend, PendingPermission, PendingQuestion};
use crate::ui::icons::{icon, spinner, wordmark};
use crate::ui::markdown;
use crate::ui::motion;
use crate::ui::rich_text::{self, RenderCtx};
use crate::ui::settings::{OpenSettingsSection, Section};
use crate::ui::text_input::TextInput;
use crate::ui::theme;
use crate::ui::widgets;

mod cache;
mod commands;
mod composer;
mod dialogs;
mod images;
mod navigation;
mod queue;
mod sidebar;
mod speech;
mod summaries;
#[cfg(test)]
mod tests;
mod transcript;

use self::cache::{DerivedCache, INLINE_PARSE_LIMIT, MarkdownCache, MarkdownKind};
use self::commands::ChatCommand;
use self::composer::{SideQuestionPanel, SlashEntry, slash_entries_for};
use self::navigation::ApplicationVimState;
#[cfg(test)]
use self::sidebar::SessionActivity;
use self::sidebar::{Sidebar, SidebarEvent, root_display_name, session_summary_eq};
use self::transcript::{
    ActiveSubagent, PlanEntry, PlanStatus, permission_arguments, plan_entries,
    render_permission_card, render_question_card, render_waiting_indicator,
};

gpui::actions!(
    chat,
    [
        AllowPermission,
        ChatEscape,
        ChooseProject,
        CopySelection,
        FocusSearch,
        NewTask,
        NextTask,
        OpenAppSettings,
        PreviousTask,
        RootMenuConfirm,
        RootMenuNext,
        RootMenuPrevious,
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
/// Left inset that keeps the header and its menus clear of the floating
/// sidebar toggle while the sidebar is collapsed.
const SIDEBAR_COLLAPSED_INSET: gpui::Pixels = px(220.);
/// Reading width shared by the transcript and the composer; they must
/// stay in one column.
const CONTENT_WIDTH: gpui::Pixels = px(900.);
/// Header title when no task is selected.
const DEFAULT_TASK_TITLE: &str = "New Task";

/// Recent projects the project menu lists above "New project…".
pub(super) const ROOT_MENU_RECENTS: usize = 6;

/// How long a notice stays before it clears itself.
const NOTICE_TTL: std::time::Duration = std::time::Duration::from_secs(8);

const COMPOSER_PLACEHOLDER: &str = "Ask Maple to work in this folder…";
const SIDE_THREAD_PLACEHOLDER: &str = "Ask a side question (Esc to leave the thread)…";
const QUEUE_EDIT_PLACEHOLDER: &str =
    "Edit the queued message, then send to keep its place. Escape discards.";
/// Sent prompts kept for Up/Down recall.
const PROMPT_HISTORY_LIMIT: usize = 50;
/// Gap between two repaints of the subagent card, which shows a live
/// elapsed time. It runs only while a subagent works.
const SUBAGENT_TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// An image staged in the composer: the data URL the runtime stores with
/// the message and a square thumbnail, both built off the UI thread.
#[derive(Clone)]
struct DraftImage {
    /// Unique per staged draft, so a late thumbnail finds its owner even
    /// after other drafts were removed.
    id: u64,
    name: String,
    /// `None` while the base64 encode is still running; the send waits
    /// for it. Shared so the send does not copy up to 13 MB per image.
    data_url: Option<Arc<str>>,
    /// `None` until the crop finishes (or if the image did not decode).
    thumbnail: Option<Arc<gpui::Image>>,
}

impl DraftImage {
    fn ready(&self) -> bool {
        self.data_url.is_some()
    }
}

/// Shared read-only state the transcript rows render from.
struct TranscriptCtx<'a> {
    markdown_cache: &'a MarkdownCache,
    derived: &'a DerivedCache,
    attachment_images: &'a HashMap<String, Arc<gpui::Image>>,
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

pub struct ChatScreen {
    backend: Arc<AgentBackend>,
    user_id: String,
    /// The task list; its own entity so it renders only when it changes.
    sidebar: Entity<Sidebar>,
    sessions: Vec<AgentSessionSummary>,
    selected_session: Option<String>,
    timeline: Vec<AgentTimelineItem>,
    /// Active run per session id, kept across selection changes.
    active_runs: HashMap<String, String>,
    /// Successfully completed tasks that have not been opened since their
    /// terminal event. This is intentionally independent of selection and
    /// project grouping; the sidebar derives presentation from it.
    completed_unread_sessions: HashSet<String>,
    /// Ids of runs whose Finished event already arrived, newest last and
    /// capped at `FINISHED_RUNS_KEPT`. A send that returns after its run
    /// finished must not mark that run active again.
    finished_runs: std::collections::VecDeque<String>,
    /// Permission requests waiting for a decision, one per request,
    /// across every session. The card shows the first one for the
    /// selected session; the others wait until their session is opened.
    pending_permissions: Vec<PendingPermission>,
    permission_responding: bool,
    /// Suppresses duplicate session creation while one is in flight.
    session_setup_pending: bool,
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
    models_menu_open: bool,
    /// Approval-mode dropdown open state, anchored under the composer.
    mode_menu_open: bool,
    /// Desktop notifications enabled (settings).
    notify_enabled: bool,
    /// Mirrors `window.is_window_active()` from the last render; refreshed
    /// on activation changes because they force a redraw.
    window_active: bool,
    sidebar_plan: Option<crate::billing::PlanUsage>,
    runtime_error: Option<SharedString>,
    notice: Option<SharedString>,
    booting: bool,
    /// Task being opened whose snapshot has not landed yet, so the pane
    /// can say so instead of showing the previous task.
    loading_session: Option<String>,
    /// Virtualized transcript state; bottom-aligned like a chat log.
    list_state: gpui::ListState,
    /// Set when the transcript should jump to its newest content on the
    /// next render (session switch or send); streaming follows only while
    /// the view is already at the bottom.
    /// Whether tool cards show their input/output payloads. Toggled from
    /// the header; off gives a one-line card per tool call.
    tool_details: bool,
    /// Permission policy for new runs: ask per gated tool, or approve
    /// every call.
    permission_mode: PermissionMode,
    /// False once the user picks a mode for this specific session; the
    /// settings default then no longer overrides it.
    uses_default_permission_mode: bool,
    /// Project context shown by the UI and used explicitly for new tasks.
    /// Existing tasks always execute in their own persisted project root.
    project_root: Option<String>,
    recent_roots: Vec<String>,
    root_menu_open: bool,
    /// Row the project menu highlights for the keyboard, if any.
    root_menu_selected: Option<usize>,
    /// Focus for the open project menu, so plain arrow keys reach it
    /// instead of the composer's text handling.
    root_menu_focus: Option<gpui::FocusHandle>,
    /// Focus for whichever modal dialog is open, so Enter and Escape
    /// reach it instead of the composer. Created the first time a dialog
    /// opens: creating it up front shifts the window's focus-id order,
    /// which the typing-focus test showed gpui is sensitive to.
    dialog_focus: Option<gpui::FocusHandle>,
    /// A dialog just opened; the next render moves focus into it.
    dialog_focus_pending: bool,
    /// Whether the project-trust question may open its dialog. Tests that
    /// drive typing turn it off, since the dialog rightly takes focus and
    /// the machine's home directory decides whether it appears.
    trust_prompts: bool,
    /// The menu was just opened and still needs the focus.
    root_menu_focus_pending: bool,
    /// Manual path entry for the project selector.
    root_input: Option<Entity<TextInput>>,
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
    /// A native folder picker is open; more clicks must not open another.
    root_picker_open: bool,
    /// Sidebar hidden; a toggle in the main pane brings it back.
    sidebar_collapsed: bool,
    /// Images staged for the next message.
    draft_images: Vec<DraftImage>,
    /// Source of `DraftImage::id`.
    draft_counter: u64,
    /// True while the native file dialog is open.
    image_picking: bool,
    /// Catalog vision flag per model id, looked up when a model is chosen.
    model_vision: HashMap<String, bool>,
    /// MCP servers with their enabled state for the selected task.
    session_mcp: Vec<AgentSessionMcpServer>,
    /// Count of enabled servers in `session_mcp`, for the composer chip.
    mcp_enabled_count: usize,
    mcp_menu_open: bool,
    /// Composer fills the pane (fullscreen editing).
    composer_expanded: bool,
    /// Latest todo list from the selected task, pinned above the composer.
    plan: Vec<PlanEntry>,
    /// Count of completed entries in `plan`, for the card header.
    plan_done: usize,
    /// Open `/btw` side question, if any. Never part of the transcript.
    btw: Option<SideQuestionPanel>,
    /// Counter for side question request ids.
    btw_sequence: u64,
    /// The pinned plan card shows only its header.
    plan_collapsed: bool,
    /// Subagents working for the selected task, pinned above the
    /// composer. Empty unless a `delegate` call is in flight.
    subagents: Vec<ActiveSubagent>,
    /// A repaint that keeps the subagent elapsed times moving is already
    /// on its way; one at a time is enough.
    subagent_tick_pending: std::cell::Cell<bool>,
    /// Bumped whenever an event adds or removes a subagent row. A
    /// snapshot from `refresh_subagents` is applied only if no event
    /// landed while it was in flight; a stale one would wipe a row the
    /// events already know about (or revive one they removed).
    subagent_epoch: u64,
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
    /// Shared drag-selection state for transcript text.
    selection: Option<Entity<rich_text::TextSelection>>,
    /// Window position of the transcript's right-click menu while open.
    transcript_menu: Option<gpui::Point<gpui::Pixels>>,
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
    question_selected: HashMap<usize, usize>,
    /// Index of the question being shown within the current card; a batch
    /// iterates one question at a time instead of listing them all.
    question_step: usize,
    /// Answers recorded for earlier steps of the current card, by index.
    question_step_answers: HashMap<usize, Vec<String>>,
    /// Full-size image shown over the chat until dismissed.
    lightbox: Option<Arc<gpui::Image>>,
    /// Sent prompts, oldest first, for Up/Down recall in the composer.
    prompt_history: Vec<String>,
    /// Position in `prompt_history` while browsing; `None` when typing.
    history_index: Option<usize>,
    /// The unsent draft saved when browsing started.
    history_draft: String,
    /// Root waiting for the user to confirm removal.
    confirm_remove_root: Option<String>,
    /// Project that has skills or guidance and no trust decision yet.
    trust_prompt: Option<AgentProjectTrustStatus>,
    trust_saving: bool,
    /// Messages waiting behind the selected session's active run.
    queue: Vec<AgentQueuedMessage>,
    /// First line of each queued message, for the chips.
    queue_previews: Vec<SharedString>,
    /// A newer release, until the banner is dismissed.
    update: Option<crate::update::UpdateInfo>,
    queue_busy: bool,
    queue_edit: Option<QueueEdit>,
    /// Slash commands from the installed skills of the current project root.
    slash_commands: Vec<AgentSlashCommand>,
    /// Highlighted row in the open slash palette, if any.
    slash_selected: Option<usize>,
    /// Set when a pending question should steal focus at the next render.
    question_focus_pending: bool,
    /// One-line model summaries per completed tool item id.
    tool_summaries: HashMap<String, SharedString>,
    /// Item ids with a summary request sent; never asked twice.
    summary_requests: HashSet<String>,
    /// Summary requests in flight; caps how many ride at once.
    pending_summaries: usize,
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

/// How often a stale snapshot is fetched again before it is applied.
const LOAD_RETRIES: u8 = 2;

/// Finished run ids remembered for a late send acknowledgement.
const FINISHED_RUNS_KEPT: usize = 64;

impl ChatScreen {
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
        this.root_menu_focus = Some(cx.focus_handle());
        this.application_focus = Some(cx.focus_handle());
        this.initialize_application_vim_surface();
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
            SidebarEvent::Select(id) => self.select_session(&id, cx),
            SidebarEvent::SessionChanged(session) => {
                self.upsert_session(session, cx);
                cx.notify();
            }
            SidebarEvent::SetArchived {
                session_id,
                archived,
            } => self.set_session_archived(&session_id, archived, cx),
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
    /// tasks run or finished unread, and the recent roots. Called when
    /// any of them changed; the sidebar rebuilds only what moved.
    fn sync_sidebar(&mut self, cx: &mut Context<Self>) {
        let sessions = self.sessions.clone();
        let selected = self.selected_session.clone();
        let running: HashSet<String> = self.active_runs.keys().cloned().collect();
        let unread = self.completed_unread_sessions.clone();
        let roots = self.recent_roots.clone();
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.set_sessions(sessions, cx);
            sidebar.set_selected(selected, cx);
            sidebar.set_activity(running, unread, cx);
            sidebar.set_recent_roots(roots, cx);
        });
        self.refresh_selected_title();
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
        self.sidebar_collapsed = collapsed;
        cx.notify();
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
    fn switcher_root_paths(&self, cx: &gpui::App) -> Vec<String> {
        self.sidebar
            .read(cx)
            .switcher_root_paths()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[cfg(test)]
    fn set_sidebar_filter(&mut self, filter: &str, cx: &mut Context<Self>) {
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.set_filter_for_test(filter);
            cx.notify();
        });
    }

    #[cfg(test)]
    fn settle_task(&mut self, session_id: &str, cx: &mut Context<Self>) {
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.settle_task(session_id, cx));
    }

    #[cfg(test)]
    fn unsettle_task(&mut self, session_id: &str, cx: &mut Context<Self>) {
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.unsettle_task(session_id, cx));
    }

    #[cfg(test)]
    fn step_sidebar_popup(&mut self, delta: isize, count: usize, cx: &mut Context<Self>) -> bool {
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.step_popup(delta, count, cx))
    }

    #[cfg(test)]
    fn activate_sidebar_popup(&mut self, cx: &mut Context<Self>) -> bool {
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.activate_popup(cx))
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
        if has_text != self.composer_has_text || entries_changed {
            self.composer_has_text = has_text;
            self.slash_entries = entries;
            cx.notify();
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
            sessions: Vec::new(),
            selected_session: None,
            timeline: Vec::new(),
            active_runs: HashMap::new(),
            completed_unread_sessions: HashSet::new(),
            finished_runs: std::collections::VecDeque::new(),
            pending_permissions: Vec::new(),
            permission_responding: false,
            session_setup_pending: false,
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
            models_menu_open: false,
            mode_menu_open: false,
            notify_enabled: settings.desktop_notifications,
            window_active: true,
            sidebar_plan: None,
            runtime_error: None,
            notice: None,
            booting: true,
            loading_session: None,
            list_state: transcript_list_state(),
            tool_details: settings.tool_details,
            // An unset or unknown value in either place means "use the
            // saved default", so only a known mode counts as an override.
            permission_mode: std::env::var("MAPLE_PERMISSION_MODE")
                .ok()
                .and_then(|mode| PermissionMode::from_str(&mode))
                .or(Some(settings.default_permission_mode))
                .unwrap_or_default(),
            uses_default_permission_mode: std::env::var("MAPLE_PERMISSION_MODE").is_err(),
            project_root: None,
            recent_roots: Vec::new(),
            root_menu_open: false,
            root_menu_selected: None,
            root_menu_focus: None,
            dialog_focus: None,
            dialog_focus_pending: false,
            trust_prompts: true,
            root_menu_focus_pending: false,
            sidebar_collapsed: false,
            draft_images: Vec::new(),
            draft_counter: 0,
            image_picking: false,
            model_vision: HashMap::new(),
            session_mcp: Vec::new(),
            mcp_enabled_count: 0,
            mcp_menu_open: false,
            composer_expanded: false,
            web_enabled: true,
            default_web_enabled: settings.default_web_enabled,
            markdown_cache: MarkdownCache::default(),
            notice_dismiss_pending: std::cell::Cell::new(false),
            derived: DerivedCache::default(),
            markdown_warmup: None,
            timeline_index: HashMap::new(),
            selected_title: DEFAULT_TASK_TITLE.into(),
            root_input: None,
            root_selecting: false,
            project_label: SharedString::from("Choose folder"),
            project_branch: None,
            branch_label: None,
            branch_watcher: None,
            watched_git_dir: None,
            root_picker_open: false,
            selection_generation: 0,
            reload_generation: 0,
            attachment_images: HashMap::new(),
            attachment_requests: HashSet::new(),
            timeline_revisions: HashMap::new(),
            selection: None,
            transcript_menu: None,
            transcript_focus: None,
            awaiting_first_token: false,
            toggled_tools: HashSet::new(),
            question_selected: HashMap::new(),
            question_step: 0,
            question_step_answers: HashMap::new(),
            lightbox: None,
            prompt_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            confirm_remove_root: None,
            trust_prompt: None,
            trust_saving: false,
            queue: Vec::new(),
            queue_previews: Vec::new(),
            queue_busy: false,
            queue_edit: None,
            update: None,
            slash_commands: Vec::new(),
            slash_selected: None,
            question_focus_pending: false,
            tool_summaries: HashMap::new(),
            summary_requests: HashSet::new(),
            pending_summaries: 0,
            summary_queue: std::collections::VecDeque::new(),
            summary_generation: 0,
            summaries_enabled: settings.tool_summaries,
            plan: Vec::new(),
            plan_done: 0,
            btw: None,
            btw_sequence: 0,
            plan_collapsed: false,
            subagents: Vec::new(),
            subagent_tick_pending: std::cell::Cell::new(false),
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
        let user_id = self.user_id.clone();
        self.call(
            async move {
                let boot = backend.local_bootstrap(&user_id).await?;
                let summaries = match &boot.latest {
                    Some(detail) => {
                        let store = backend.clone();
                        let target = detail.session.id.clone();
                        tokio::task::spawn_blocking(move || {
                            store.load_tool_summaries_blocking(&user_id, &target)
                        })
                        .await
                        .map_err(|error| error.to_string())?
                        .unwrap_or_else(|error| {
                            log::warn!("Cannot load tool summaries: {error}");
                            HashMap::new()
                        })
                    }
                    None => HashMap::new(),
                };
                Ok::<_, String>((boot, summaries))
            },
            cx,
            |this, result, cx| {
                match result {
                    Ok((boot, summaries)) => {
                        log::debug!(
                            "startup: local bootstrap applied at {} ms ({} tasks)",
                            crate::startup_elapsed(),
                            boot.sessions.len()
                        );
                        this.project_root = boot.project_root;
                        this.project_root_changed(cx);
                        this.check_project_trust(cx);
                        this.recent_roots = boot.recent_roots;
                        this.sessions = boot.sessions;
                        this.sync_sidebar(cx);
                        // A click that landed before this callback wins.
                        if let Some(detail) =
                            boot.latest.filter(|_| this.selected_session.is_none())
                        {
                            let summaries = summaries
                                .into_iter()
                                .map(|(id, summary)| (id, SharedString::from(summary)))
                                .collect();
                            this.upsert_session(detail.session.clone(), cx);
                            this.set_active_session(detail.session, detail.timeline, summaries, cx);
                            this.queue = detail.queue.items;
                        }
                        cx.notify();
                    }
                    // Not fatal: the runtime start below retreads all of it.
                    Err(message) => log::debug!("local bootstrap unavailable: {message}"),
                }
                this.refresh_slash_commands(cx);
                this.start_runtime(cx);
            },
        );
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
        self.call(
            async move { backend.start_runtime(&user_id, Some(request)).await },
            cx,
            move |this, result, cx| {
                match result {
                    Ok(status) => {
                        log::debug!(
                            "startup: runtime started at {} ms",
                            crate::startup_elapsed()
                        );
                        this.runtime_error = None;
                        // Runtime root is only the fallback used at startup.
                        if this.selected_session.is_none()
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
                cx.notify();
                this.refresh_models(cx);
                this.refresh_roots(cx);
                this.refresh_sessions(cx);
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
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.recent_project_roots(&user_id).await },
            cx,
            |this, result, cx| {
                if let Ok(roots) = result {
                    this.apply_recent_roots(roots, cx);
                }
                cx.notify();
            },
        );
    }

    /// Take the recent-root list the service returned, newest first.
    fn apply_recent_roots(
        &mut self,
        roots: Vec<maple_agent::agent::RecentProjectRoot>,
        cx: &mut Context<Self>,
    ) {
        self.recent_roots = roots.into_iter().map(|root| root.path).collect();
        self.sync_sidebar(cx);
    }

    /// Select the project context for new tasks without disturbing work that
    /// is already running in any session.
    fn select_project_root(&mut self, path: String, cx: &mut Context<Self>) {
        if self.root_selecting {
            return;
        }
        let path = path.trim().to_string();
        if path.is_empty() || !std::path::Path::new(&path).is_absolute() {
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
        self.root_menu_open = false;
        self.root_input = None;
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
                        this.apply_recent_roots(registration.roots, cx);
                        if this.selection_generation != selection_generation {
                            // Registration still succeeded, so the project
                            // stays listed without overriding the newer
                            // navigation intent.
                            cx.notify();
                            return;
                        }
                        this.begin_navigation();
                        this.clear_selected_session_presentation(cx);
                        this.set_project_context(Some(registration.project_root), cx);
                        this.refresh_sessions(cx);
                    }
                    Err(message) => {
                        this.notice = Some(message.into());
                    }
                }
                cx.notify();
            },
        );
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
                    this.apply_recent_roots(registration.roots, cx);
                    cx.notify();
                }
                Err(error) => log::warn!("Cannot persist project root: {error}"),
            },
        );
    }

    /// Start a navigation that supersedes every task load in flight.
    fn begin_navigation(&mut self) {
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
        self.project_root_changed(cx);
        self.check_project_trust(cx);
        self.refresh_slash_commands(cx);
        self.sync_sidebar(cx);
        true
    }

    fn choose_root_dialog(&mut self, cx: &mut Context<Self>) {
        // The platform folder picker through gpui: NSOpenPanel on macOS,
        // the common file dialog on Windows, the XDG portal on Linux.
        // The panel closes with the app, so quit is never blocked on it.
        // Manual entry only when the picker cannot open (e.g. a Linux
        // desktop with no portal); a cancel just closes.
        if !self.begin_root_picker(cx) {
            return;
        }
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
                match picked {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            this.select_project_root(path.to_string_lossy().into_owned(), cx);
                        }
                    }
                    // Cancelled, or the picker dropped its channel.
                    Ok(Ok(None)) | Err(_) => {}
                    Ok(Err(_)) => this.show_root_input(cx),
                }
                cx.notify();
            })
            .ok();
        });
        // The portal dialog completes on its own thread; retained so the
        // bridge dies here (see ChatScreen::call).
        crate::ui::task::retain(&self.bridged_tasks, bridge);
    }

    /// The root changed: update the header label, then read its branch
    /// and watch its git dir.
    fn project_root_changed(&mut self, cx: &mut Context<Self>) {
        self.project_label = SharedString::from(self.project_label());
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
                let touches_head = event
                    .paths
                    .iter()
                    .any(|path| path.file_name().is_some_and(|name| name == "HEAD"));
                if touches_head || event.need_rescan() {
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

    /// Claim the folder picker. One at a time: several at once each
    /// applied their own result and stalled the app. Returns `false` when
    /// a picker or a project selection is already in progress.
    fn begin_root_picker(&mut self, cx: &mut Context<Self>) -> bool {
        if self.root_picker_open || self.root_selecting {
            return false;
        }
        self.root_picker_open = true;
        self.root_menu_open = false;
        cx.notify();
        true
    }

    /// Manual path entry when the native picker is unavailable.
    fn show_root_input(&mut self, cx: &mut Context<Self>) {
        // The native picker could not open: offer manual entry.
        if self.root_input.is_none() {
            let chat = cx.entity().downgrade();
            let application_vim_enabled = self.application_vim_enabled;
            let input = cx.new(move |cx| {
                TextInput::new("/absolute/path/to/project", cx)
                    .with_tab_index(0)
                    .application_vim(application_vim_enabled)
                    .on_application_escape(move |window, cx| {
                        if let Some(chat) = chat.upgrade() {
                            chat.update(cx, |chat, cx| chat.focus_application_vim(window, cx));
                        }
                    })
            });
            self.root_input = Some(input);
        }
        self.root_menu_open = true;
        cx.notify();
    }

    /// Open or close the project menu from the header chip.
    pub fn toggle_root_menu(&mut self, cx: &mut Context<Self>) {
        self.models_menu_open = false;
        self.mode_menu_open = false;
        self.mcp_menu_open = false;
        self.root_menu_open = !self.root_menu_open;
        self.root_menu_selected = None;
        // The next frame moves the focus; from there the arrow keys and
        // Enter reach the menu instead of the composer.
        self.root_menu_focus_pending = self.root_menu_open;
        cx.notify();
    }

    /// Text for the header project chip.
    pub fn project_label(&self) -> String {
        self.project_root
            .as_deref()
            .map(root_display_name)
            .unwrap_or_else(|| "Choose folder".to_string())
    }

    fn refresh_models(&self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let env_model = backend.configured_model();
        self.call(
            async move {
                let saved = backend.saved_model(&user_id).await;
                let models = backend.available_model_ids(&user_id).await?;
                Ok((models, saved))
            },
            cx,
            move |this, result, cx| {
                if let Ok((models, saved)) = result {
                    if this.selected_model.is_none() {
                        // Env override wins, then the account's saved
                        // default, then the first catalog entry.
                        this.selected_model =
                            env_model.or(saved).or_else(|| models.first().cloned());
                    }
                    this.models = models;
                    this.refresh_vision(cx);
                }
                cx.notify();
            },
        );
    }

    fn refresh_sessions(&self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        // The sidebar groups tasks by project, so list every root. Each task's
        // stored root remains authoritative when it is opened or run.
        let generation = self.selection_generation;
        self.call(
            async move { backend.list_sessions(&user_id, None).await },
            cx,
            move |this, result, cx| {
                match result {
                    Ok(sessions) => this.apply_session_list(sessions, generation, cx),
                    Err(message) => this.notice = Some(message.into()),
                }
                cx.notify();
            },
        );
    }

    /// Take a fresh session list. With nothing on screen, open the latest
    /// task of the visible project or create one, but only when no task or
    /// project was selected since the list was requested: a click whose
    /// load is still in flight leaves the selection empty too, and the
    /// auto-select would supersede it.
    fn apply_session_list(
        &mut self,
        sessions: Vec<AgentSessionSummary>,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        self.sessions = sessions;
        self.sync_sidebar(cx);
        if self.selected_session.is_some() || self.selection_generation != generation {
            return;
        }
        let root = self.project_root.clone();
        let latest = self
            .sessions
            .iter()
            .find(|session| !session.archived && Some(&session.project_root) == root.as_ref())
            .map(|session| session.id.clone());
        match latest {
            Some(id) => self.select_session(&id, cx),
            None => self.new_session(cx),
        }
    }

    fn new_session(&mut self, cx: &mut Context<Self>) {
        // A boot-time auto-create and a user click can race; one only.
        if self.session_setup_pending {
            return;
        }
        if self.root_selecting {
            // The visible project is about to change; a task created now
            // would land under the old one.
            self.notice = Some("Wait for the project selection to finish, then try again".into());
            cx.notify();
            return;
        }
        let Some(request) = self.new_session_request() else {
            self.notice = Some("Choose a project before creating a task".into());
            cx.notify();
            return;
        };
        // Creating a task is a navigation intent, but the generation moves
        // only when the task lands: a failed create leaves loads in flight
        // alive, and a task or project selected meanwhile supersedes the
        // eventual callback.
        let selection_generation = self.selection_generation;
        self.session_setup_pending = true;
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move {
                backend
                    .create_session(&user_id, Some(request))
                    .await
                    .map(|detail| detail.session)
            },
            cx,
            move |this, result, cx| match result {
                Ok(session) => this.finish_new_session(session, selection_generation, cx),
                Err(message) => {
                    this.session_setup_pending = false;
                    this.notice = Some(message.into());
                }
            },
        );
    }

    fn finish_new_session(
        &mut self,
        session: AgentSessionSummary,
        selection_generation: u64,
        cx: &mut Context<Self>,
    ) {
        self.session_setup_pending = false;
        // The SessionCreated event may arrive before this callback; upsert so
        // the sidebar never shows the task twice, even when its navigation is
        // no longer current.
        self.upsert_session(session.clone(), cx);
        if self.selection_generation == selection_generation {
            self.begin_navigation();
            self.set_active_session(session, Vec::new(), HashMap::new(), cx);
            if !self.default_web_enabled {
                self.set_web_enabled(false, cx);
            }
            return;
        }

        // Creation still succeeded, but an older callback must never override
        // a newer task or project choice. If the project choice left the view
        // empty while the one-create-at-a-time fence was held, let it settle
        // now that another task may be created.
        self.sync_sidebar(cx);
        if self.selected_session.is_none() {
            self.refresh_sessions(cx);
        }
        cx.notify();
    }

    /// Build an explicit request so new-task placement never depends on the
    /// runtime's startup fallback.
    fn new_session_request(&self) -> Option<AgentCreateSessionRequest> {
        Some(AgentCreateSessionRequest {
            project_root: Some(self.project_root.clone()?),
            title: None,
            model: None,
            context_limit: None,
            mode: None,
            mcp_server_names: None,
            system_prompt: None,
        })
    }

    /// Load a task and make it current when its snapshot lands; its
    /// persisted root then becomes the visible project context.
    pub(crate) fn select_session(&mut self, session_id: &str, cx: &mut Context<Self>) {
        // The side thread belongs to the task it forked.
        if self.btw.is_some() && self.selected_session.as_deref() != Some(session_id) {
            self.close_side_thread(cx);
        }
        self.load_session(session_id, LoadMode::Select, LOAD_RETRIES, cx);
    }

    /// Replace the timeline after a mid-run history compaction without
    /// disturbing the active run or a pending permission.
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
                self.selection_generation += 1;
                self.selection_generation
            }
            LoadMode::Reload => self.reload_generation,
        };
        let revision = *self.timeline_revisions.get(session_id).unwrap_or(&0);
        if matches!(mode, LoadMode::Select) && !self.is_selected(session_id) {
            self.loading_session = Some(session_id.to_string());
            cx.notify();
        }
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let session_id = session_id.to_string();
        let target = session_id.clone();
        self.call(
            async move {
                // The stored summaries load off-thread while the runtime
                // builds the session detail.
                let store = backend.clone();
                let store_user = user_id.clone();
                let store_target = target.clone();
                let summaries = tokio::task::spawn_blocking(move || {
                    store.load_tool_summaries_blocking(&store_user, &store_target)
                });
                let (detail, summaries) =
                    tokio::join!(backend.load_session(&user_id, &target), summaries);
                let summaries = summaries
                    .map_err(|error| error.to_string())?
                    .unwrap_or_else(|error| {
                        log::warn!("Cannot load tool summaries: {error}");
                        HashMap::new()
                    });
                Ok::<_, String>((detail?, summaries))
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
                let (detail, summaries) = match result {
                    Ok(detail) => detail,
                    Err(message) => {
                        this.finish_loading(&session_id);
                        this.notice = Some(message.into());
                        cx.notify();
                        return;
                    }
                };
                let current = *this
                    .timeline_revisions
                    .get(&detail.session.id)
                    .unwrap_or(&0);
                if current != revision && retries > 0 {
                    this.load_session(&session_id, mode, retries - 1, cx);
                    return;
                }
                this.finish_loading(&session_id);
                // Stored summaries stand in for the model calls the
                // timeline would otherwise request again.
                let summaries: HashMap<String, SharedString> = summaries
                    .into_iter()
                    .map(|(id, summary)| (id, SharedString::from(summary)))
                    .collect();
                match mode {
                    LoadMode::Select => {
                        this.upsert_session(detail.session.clone(), cx);
                        this.set_active_session(detail.session, detail.timeline, summaries, cx);
                        this.queue = detail.queue.items;
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
            },
        );
    }

    /// The open of `session_id` finished (either way), so the pane stops
    /// saying it is loading. A newer selection keeps its own marker.
    fn finish_loading(&mut self, session_id: &str) {
        if self.loading_session.as_deref() == Some(session_id) {
            self.loading_session = None;
        }
    }

    /// Overlay shown over the pane while a task snapshot is in flight.
    fn render_loading_overlay(&self) -> Option<Div> {
        self.loading_session.as_ref()?;
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme::loading_veil())
                .child(motion::fade_in(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_4()
                        .py_2()
                        .rounded_full()
                        .bg(gpui::rgb(theme::bg_elevated()))
                        .border_1()
                        .border_color(gpui::rgb(theme::border()))
                        .shadow_md()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_secondary()))
                        .child(spinner("open-task", px(14.), theme::accent()))
                        .child("Opening task…"),
                    "loading-overlay-reveal",
                )),
        )
    }

    /// Install a new timeline and reset every per-item structure that is
    /// keyed by its contents.
    fn replace_timeline(&mut self, timeline: Vec<AgentTimelineItem>) {
        let old_navigation_order = self
            .application_vim_enabled
            .then(|| self.navigable_timeline_ids());
        self.timeline = timeline;
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
    }

    /// Apply settings-default changes when returning from the settings
    /// screen: tool verbosity updates live; the permission default only
    /// affects sessions that still follow the default.
    pub fn apply_defaults(
        &mut self,
        settings: &crate::settings::AppSettings,
        cx: &mut Context<Self>,
    ) {
        self.tool_details = settings.tool_details;
        self.default_web_enabled = settings.default_web_enabled;
        self.notify_enabled = settings.desktop_notifications;
        self.summaries_enabled = settings.tool_summaries;
        self.composer_vim_enabled = settings.composer_vim_enabled;
        if let Some(composer) = self.composer.clone() {
            composer.update(cx, |input, cx| {
                input.set_vim_enabled(settings.composer_vim_enabled, cx)
            });
        }
        self.set_application_vim_enabled(settings.application_vim_enabled, cx);
        // Settings is a different mounted entity, so its focus handle becomes
        // stale when Chat is restored even when no preference changed. This
        // handoff is intentionally cross-mode: Application Vim returns to its
        // proxy, while Standard mode returns to the composer.
        self.screen_focus_pending = true;
        self.tts_voice.clone_from(&settings.tts_voice);
        self.tts_speed = settings.tts_speed;
        if self.uses_default_permission_mode {
            self.permission_mode = settings.default_permission_mode;
            self.apply_permission_mode(cx);
        }
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
            |this, result, cx| {
                if let Ok(Some((tokens, limit))) = result {
                    this.apply_context_usage(tokens, limit, cx);
                }
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

    fn apply_permission_mode(&self, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let mode = self.permission_mode.as_str().to_string();
        self.call(
            async move {
                backend
                    .set_permission_mode(&user_id, &session_id, &mode)
                    .await
            },
            cx,
            |this, result, cx| {
                if let Err(message) = result {
                    this.notice = Some(format!("Could not set permission mode: {message}").into());
                    cx.notify();
                }
            },
        );
    }

    /// Leave the selected task's presentation without discarding anything
    /// the account runtime still needs for background work. Pending
    /// permissions and questions remain keyed by session and reappear when
    /// that task is opened again.
    pub(super) fn clear_selected_session_presentation(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let left = self.selected_session.clone();
        if self.btw.is_some() {
            self.close_side_thread(cx);
        }
        self.subagent_epoch += 1;
        self.subagents.clear();
        self.tool_summaries.clear();
        self.attachment_images.clear();
        self.attachment_requests.clear();
        self.toggled_tools.clear();
        if let Some(composer) = self.composer.clone() {
            composer.update(cx, |input, cx| input.reset_vim_context(cx));
        }
        // Release the hold while the previous session id is still selected.
        self.abandon_queue_edit(cx);
        self.selected_session = None;
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
        self.pending_summaries = 0;
        self.summary_queue.clear();
        self.summary_requests.clear();
        self.list_state.scroll_to_end();
        self.awaiting_first_token = false;
        self.lightbox = None;
        self.permission_responding = false;
        self.models_menu_open = false;
        self.root_menu_open = false;
        self.mcp_menu_open = false;
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
        // Opening the task is what reads its completion, so the unread
        // marker goes. Only an explicit settle ever moves a task out of
        // the active inbox.
        self.completed_unread_sessions.remove(&session.id);
        self.selected_session = Some(session.id);
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
        // Adopt the session's stored policy; it persists per session in the
        // runtime.
        if let Some(mode) = PermissionMode::from_str(&session.mode) {
            self.permission_mode = mode;
        }
        self.web_enabled = session.web_enabled;
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
        if let Some(input) = self.pending_question_input.take() {
            input.update(cx, |input, cx| input.clear(cx));
        }
        if self.current_question().is_some() {
            self.ensure_question_input(cx);
            self.question_focus_pending = true;
        }
    }

    /// Look up the catalog vision flag for the selected model once.
    fn refresh_vision(&mut self, cx: &mut Context<Self>) {
        let Some(model) = self.selected_model.clone() else {
            return;
        };
        if self.model_vision.contains_key(&model) {
            return;
        }
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let lookup = model.clone();
        self.call(
            async move { backend.model_supports_vision(&user_id, &lookup).await },
            cx,
            move |this, result, cx| {
                if let Ok(Some(vision)) = result {
                    this.model_vision.insert(model, vision);
                    cx.notify();
                }
            },
        );
    }

    fn selected_model_supports_vision(&self) -> bool {
        self.selected_model
            .as_ref()
            .and_then(|model| self.model_vision.get(model))
            .copied()
            .unwrap_or(false)
    }
    /// Reload the skill slash commands for the current project root.
    pub fn refresh_slash_commands(&mut self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let working_dir = self.project_root.clone();
        let requested_root = working_dir.clone();
        self.call(
            async move { backend.list_slash_commands(working_dir).await },
            cx,
            move |this, result, cx| {
                if this.project_root == requested_root
                    && let Ok(commands) = result
                {
                    this.slash_commands = commands;
                    cx.notify();
                }
            },
        );
    }

    /// Reload the MCP server list for the selected task.
    pub fn refresh_session_mcp(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            self.set_session_mcp(Vec::new());
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

    fn toggle_session_mcp(&mut self, name: String, enabled: bool, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let target = session_id.clone();
        self.call(
            async move {
                backend
                    .set_session_mcp_server_enabled(&user_id, &target, &name, enabled)
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

    /// Persist the web flag for the selected task; the runtime applies it
    /// on the next turn.
    fn set_web_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
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

    pub fn set_update(&mut self, info: crate::update::UpdateInfo, cx: &mut Context<Self>) {
        self.update = Some(info);
        cx.notify();
    }

    /// Banner for a newer release, with a link to its page.
    fn render_update_banner(&self, cx: &mut Context<Self>) -> Option<Div> {
        let info = self.update.as_ref()?;
        let url = info.url.clone();
        Some(
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .py_2()
                .rounded(theme::RADIUS_SM)
                .bg(gpui::rgb(theme::bg_elevated()))
                .border_1()
                .border_color(gpui::rgb(theme::border()))
                .text_sm()
                .text_color(gpui::rgb(theme::text_primary()))
                .child(icon("arrow-up", px(14.), theme::accent()))
                .child(
                    div()
                        .flex_1()
                        .child(format!("Maple v{} is available", info.version)),
                )
                .child(
                    div()
                        .id("update-open")
                        .px_2()
                        .py_0p5()
                        .rounded(theme::RADIUS_SM)
                        .text_color(gpui::rgb(theme::accent()))
                        .hover(|style| style.bg(theme::overlay_hover()).cursor_pointer())
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            if let Err(error) = webbrowser::open(&url) {
                                this.notice =
                                    Some(format!("Could not open browser: {error}").into());
                            }
                            cx.notify();
                        }))
                        .child("Download"),
                )
                .child(
                    div()
                        .id("update-dismiss")
                        .size_5()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(theme::RADIUS_SM)
                        .hover(|style| style.bg(theme::overlay_hover()).cursor_pointer())
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.update = None;
                            cx.notify();
                        }))
                        .child(icon("x", px(12.), theme::text_secondary())),
                ),
        )
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
        });
        self.schedule_subagent_tick(cx);
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
        if self.active_runs.contains_key(session_id) {
            Some(SessionActivity::Running)
        } else if self.completed_unread_sessions.contains(session_id) {
            Some(SessionActivity::CompletedUnread)
        } else {
            None
        }
    }

    /// Rows the project menu offers: the recent roots it lists, then
    /// "New project…".
    fn root_menu_rows(&self) -> usize {
        self.recent_roots.len().min(ROOT_MENU_RECENTS) + 1
    }

    /// Move the menu highlight. A menu is short, so it wraps at both
    /// ends instead of stopping.
    fn step_root_menu(&mut self, delta: isize, cx: &mut Context<Self>) {
        if !self.root_menu_open {
            return;
        }
        let rows = self.root_menu_rows() as isize;
        let next = match self.root_menu_selected {
            Some(current) => (current as isize + delta).rem_euclid(rows),
            // Nothing highlighted: enter the menu from the end the key
            // comes from.
            None if delta < 0 => rows - 1,
            None => 0,
        };
        self.root_menu_selected = Some(next as usize);
        cx.notify();
    }

    /// Enter on the highlighted menu row: switch to that project, or
    /// open the folder picker on the last row.
    fn confirm_root_menu(&mut self, cx: &mut Context<Self>) {
        if !self.root_menu_open {
            return;
        }
        let Some(index) = self.root_menu_selected else {
            return;
        };
        let recent = self
            .recent_roots
            .iter()
            .take(ROOT_MENU_RECENTS)
            .nth(index)
            .cloned();
        match recent {
            Some(path) => self.select_project_root(path, cx),
            None => self.choose_root_dialog(cx),
        }
    }

    /// Alt-Up / Alt-Down: open the task before or after the selected one
    /// in the order the sidebar shows them.
    fn step_task(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some((row, id)) = self.task_step_target(delta, cx) else {
            return;
        };
        self.sidebar.read(cx).reveal_row(row);
        self.select_session(&id, cx);
    }

    /// The task `delta` rows away from the selected one, as the sidebar
    /// row it sits on and its id.
    fn task_step_target(&self, delta: isize, cx: &gpui::App) -> Option<(usize, String)> {
        self.sidebar.read(cx).step_target(delta)
    }

    /// Ctrl-1 to Ctrl-9: answer the question card with the numbered
    /// option, the same as picking it and pressing Answer.
    fn pick_and_submit_question_option(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(question) = self.current_question() else {
            return;
        };
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
        self.select_question_option(step, index, cx);
        self.submit_question(cx);
    }

    fn escape(&mut self, cx: &mut Context<Self>) {
        if self
            .sidebar
            .update(cx, |sidebar, cx| sidebar.cancel_rename(cx))
        {
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
        let popups = self
            .sidebar
            .update(cx, |sidebar, cx| sidebar.close_popups(cx));
        if self.confirm_remove_root.is_some() || popups {
            self.confirm_remove_root = None;
            cx.notify();
            return;
        }
        if self.lightbox.is_some() {
            self.lightbox = None;
            cx.notify();
            return;
        }
        if self.transcript_menu.is_some() {
            self.transcript_menu = None;
            cx.notify();
            return;
        }
        if self.current_question().is_some() {
            self.skip_question(cx);
            return;
        }
        if self.current_permission().is_some() {
            self.respond_permission(false, cx);
            return;
        }
        if let Some(selection) = self.selection.clone()
            && selection.read(cx).has_selection()
        {
            selection.update(cx, |selection, _| selection.clear());
            cx.notify();
            return;
        }
        if self.close_menus_on_escape(cx) {
            return;
        }
        self.stop(cx);
    }

    /// Escape with nothing else to dismiss: close whichever chip menu is
    /// open (including the folder picker). Reports whether one was open.
    fn close_menus_on_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.models_menu_open || self.mode_menu_open || self.mcp_menu_open || self.root_menu_open
        {
            self.models_menu_open = false;
            self.mode_menu_open = false;
            self.mcp_menu_open = false;
            self.root_menu_open = false;
            cx.notify();
            return true;
        }
        false
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
        if self.trust_prompt.is_some() || self.confirm_remove_root.is_some() {
            return;
        }
        // A focused text input already receives typing.
        let focused = window.focused(cx);
        let typing_here = [self.composer.clone(), self.pending_question_input.clone()]
            .into_iter()
            .flatten()
            .chain(self.sidebar.read(cx).inputs())
            .any(|input| Some(input.read(cx).focus_handle(cx)) == focused);
        if typing_here {
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

    /// Right-click menu over the transcript: copy the selection, select all.
    fn render_transcript_menu(&self, cx: &mut Context<Self>) -> Option<gpui::Deferred> {
        let position = self.transcript_menu?;
        let has_selection = self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.read(cx).has_selection());
        let item = |id: &'static str,
                    icon_name: &'static str,
                    label: &'static str,
                    command: ChatCommand| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1p5()
                .text_sm()
                .text_color(gpui::rgb(theme::text_primary()))
                .hover(|style| {
                    style
                        .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                        .cursor_pointer()
                })
                .on_click(cx.listener(move |this, _event, window, cx| {
                    cx.stop_propagation();
                    this.transcript_menu = None;
                    this.execute_command(command, window, cx);
                    cx.notify();
                }))
                .child(icon(icon_name, px(14.), theme::text_secondary()))
                .child(label)
        };
        Some(gpui::deferred(
            gpui::anchored()
                .position(position)
                .snap_to_window_with_margin(px(8.))
                .child(
                    div()
                        .id("transcript-menu")
                        .occlude()
                        .w(px(160.))
                        .py_1()
                        .rounded(theme::RADIUS_SM)
                        .bg(gpui::rgb(theme::bg_elevated()))
                        .border_1()
                        .border_color(gpui::rgb(theme::border()))
                        .shadow_md()
                        .flex()
                        .flex_col()
                        .on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
                            this.transcript_menu = None;
                            cx.notify();
                        }))
                        .when(has_selection, |menu| {
                            menu.child(item(
                                "transcript-menu-copy",
                                "copy",
                                "Copy",
                                ChatCommand::CopySelection,
                            ))
                        })
                        .child(item(
                            "transcript-menu-select-all",
                            "text-select",
                            "Select all",
                            ChatCommand::SelectAllTranscript,
                        )),
                ),
        ))
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
    fn send_inner(&mut self, cx: &mut Context<Self>) {
        let Some(composer) = self.composer.clone() else {
            return;
        };
        let text = composer.read(cx).text();
        composer.update(cx, |input, cx| input.clear(cx));
        self.send_text(text, cx);
    }

    /// Send the given text; used by the composer Enter hook, which already
    /// holds the text and clears the input itself.
    fn send_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.send_text_with(text, false, cx);
    }

    /// Ctrl/Cmd+Enter: send into the active run instead of behind it.
    /// With no run active this is a plain send.
    fn steer_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.send_text_with(text, true, cx);
    }

    fn send_text_with(&mut self, text: String, steer: bool, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            self.notice = Some("Create a task first".into());
            cx.notify();
            return;
        };
        if self.booting {
            self.notice = Some("Agent runtime is still starting".into());
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
            self.ask_side_question(&session_id, question, cx);
            return;
        }
        if self.current_question().is_some() {
            // The run is blocked on the question; sending here would queue a
            // message nobody reads and look like a stall.
            self.notice = Some("Answer the question above first".into());
            self.question_focus_pending = true;
            cx.notify();
            return;
        }
        if text.trim().is_empty() && self.draft_images.is_empty() {
            return;
        }
        if !self.draft_images.iter().all(DraftImage::ready) {
            // The encode is still on its way; keep the text for the retry.
            self.notice = Some("Images are still being prepared; try again in a moment".into());
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
            self.try_command(&session_id, &format!("/{command}"), cx);
            return;
        }
        if self.try_command(&session_id, text.trim(), cx) {
            // Commands never echo into the transcript; drop the typed text
            // so the palette cannot survive the execution.
            self.slash_selected = None;
            if let Some(composer) = self.composer.clone() {
                composer.update(cx, |input, cx| input.clear(cx));
            }
            return;
        }
        let queue_id = self.queue_edit.as_ref().map(|edit| edit.queue_id.clone());
        self.send_to_session(&session_id, text, steer, queue_id, cx);
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

    /// Execute a `/command` when it matches a built-in or a skill. Unknown
    /// commands fall through and are sent to the model as plain text.
    fn try_command(&mut self, session_id: &str, text: &str, cx: &mut Context<Self>) -> bool {
        let Some(body) = text.strip_prefix('/') else {
            return false;
        };
        let (name, args) = match body.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (body, ""),
        };
        if name.is_empty() || name.contains('/') {
            return false;
        }
        let session_id = session_id.to_string();
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
                    self.models_menu_open = true;
                    self.mode_menu_open = false;
                    self.mcp_menu_open = false;
                    self.root_menu_open = false;
                    cx.notify();
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
                self.ask_side_question(&session_id, args, cx);
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
                if !self
                    .slash_commands
                    .iter()
                    .any(|command| command.name.eq_ignore_ascii_case(name))
                {
                    return false;
                }
                let backend = self.backend.clone();
                let working_dir = self.project_root.clone();
                let command = name.to_string();
                let arguments = args.to_string();
                self.notice = Some("Loading skill…".into());
                cx.notify();
                self.call(
                    async move {
                        backend
                            .resolve_slash_command(working_dir, command, arguments)
                            .await
                    },
                    cx,
                    move |this, result, cx| match result {
                        Ok(Some(prompt)) => {
                            this.notice = None;
                            this.send_to_session(&session_id, prompt, false, None, cx);
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
    ) {
        let session_id = session_id.to_string();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let model = self.selected_model.clone();
        let vision_capable = self.selected_model_supports_vision();
        let run_active = self.active_runs.contains_key(&session_id);
        // A queued chip keeps its own attachments; new images stay staged.
        let drafts = if queue_id.is_some() {
            Vec::new()
        } else {
            std::mem::take(&mut self.draft_images)
        };
        let request = AgentSendMessageRequest {
            session_id: session_id.clone(),
            text: text.clone(),
            model,
            context_limit: None,
            mode: Some(self.permission_mode.as_str().to_string()),
            vision_capable,
            steer: steer && run_active,
            queue_id,
            attachments: drafts
                .iter()
                .filter_map(|image| {
                    let data_url = image.data_url.as_deref()?;
                    Some(AgentImageUpload {
                        name: image.name.clone(),
                        data_url: data_url.to_string(),
                    })
                })
                .collect(),
        };
        self.notice = None;
        let editing = self
            .queue_edit
            .as_ref()
            .is_some_and(|edit| Some(&edit.queue_id) == request.queue_id.as_ref());
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
        // Sending closes the chip menus anchored under the composer.
        self.models_menu_open = false;
        self.mode_menu_open = false;
        self.mcp_menu_open = false;
        self.root_menu_open = false;
        self.list_state.scroll_to_end();
        if !run_active {
            self.awaiting_first_token = true;
        }
        cx.notify();
        self.call(
            async move { backend.send_message(&user_id, request).await },
            cx,
            move |this, result, cx| match result {
                Ok(run_id) => {
                    // A short run can finish before the send returns; its
                    // Finished event already retired it.
                    if !this.finished_runs.contains(&run_id) {
                        this.active_runs.insert(session_id.clone(), run_id);
                        this.sync_sidebar(cx);
                    }
                }
                Err(message) => {
                    // Show the failure in the transcript and give the draft
                    // back instead of silently dropping it. The composer
                    // belongs to the selected session: a draft from another
                    // task must not land in it.
                    this.push_local_error("Send failed", &message, cx);
                    if from_composer
                        && this.selected_session.as_deref() == Some(session_id.as_str())
                    {
                        if let Some(composer) = this.composer.clone() {
                            composer.update(cx, |input, cx| input.set_text(&text, cx));
                        }
                        let mut restored = drafts;
                        restored.append(&mut this.draft_images);
                        this.draft_images = restored;
                    } else {
                        this.notice = Some(format!("Send failed: {message}").into());
                    }
                    this.awaiting_first_token = false;
                }
            },
        );
    }

    fn push_local_error(&mut self, title: &str, message: &str, cx: &mut Context<Self>) {
        let item = AgentTimelineItem {
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
        if let Some(option_index) = self.question_selected.get(&step)
            && let Some(option) = entry.options.get(*option_index)
        {
            let mut answer = vec![option.label.clone()];
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
    fn skip_question(&mut self, cx: &mut Context<Self>) {
        let Some(question) = self.current_question().cloned() else {
            return;
        };
        self.pending_questions
            .retain(|queued| queued.request_id != question.request_id);
        self.reset_question_card(cx);
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

    /// Pick one option of one question (single-select, codex shape).
    fn select_question_option(
        &mut self,
        question_index: usize,
        option_index: usize,
        cx: &mut Context<Self>,
    ) {
        self.question_selected.insert(question_index, option_index);
        cx.notify();
    }

    /// Click handler for an option row: clicking the picked option again
    /// clears it so a typed answer can stand alone. Ctrl-N keeps plain
    /// select semantics because it submits immediately.
    fn toggle_question_option(
        &mut self,
        question_index: usize,
        option_index: usize,
        cx: &mut Context<Self>,
    ) {
        if self.question_selected.get(&question_index) == Some(&option_index) {
            self.question_selected.remove(&question_index);
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

    fn respond_permission(&mut self, allow: bool, cx: &mut Context<Self>) {
        let Some(permission) = self.current_permission().cloned() else {
            return;
        };
        if self.permission_responding {
            return;
        }
        self.permission_responding = true;
        cx.notify();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let request_id = permission.request_id.clone();
        self.call(
            async move {
                backend
                    .permission_respond(
                        &user_id,
                        &permission.session_id,
                        &permission.request_id,
                        allow,
                    )
                    .await
            },
            cx,
            move |this, result, cx| {
                this.permission_responding = false;
                match result {
                    Ok(()) => {
                        this.pending_permissions
                            .retain(|pending| pending.request_id != request_id);
                    }
                    Err(message) => {
                        // Keep the card so the decision can be retried.
                        this.notice = Some(format!("Permission response failed: {message}").into());
                    }
                }
                cx.notify();
            },
        );
    }

    pub(crate) fn sign_out(&mut self, cx: &mut Context<Self>) {
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
            |_this, _result, cx| {
                cx.emit(LoggedOut);
            },
        );
    }

    fn pick_model(&mut self, model: String, cx: &mut Context<Self>) {
        self.selected_model = Some(model.clone());
        self.models_menu_open = false;
        self.root_menu_open = false;
        cx.notify();
        self.refresh_vision(cx);
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
    /// Returns the item's index in the timeline.
    fn apply_timeline_item(&mut self, session_id: &str, item: AgentTimelineItem) -> usize {
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
                index
            }
        };
        *self
            .timeline_revisions
            .entry(session_id.to_string())
            .or_insert(0) += 1;
        index
    }

    /// Insert or replace a session row; returns whether anything changed.
    fn upsert_session(&mut self, session: AgentSessionSummary, cx: &mut Context<Self>) -> bool {
        // An archived row shows no indicator; drop a marker it may carry.
        if session.archived {
            self.completed_unread_sessions.remove(&session.id);
        }
        if let Some(existing) = self
            .sessions
            .iter_mut()
            .find(|candidate| candidate.id == session.id)
        {
            if session_summary_eq(existing, &session) {
                return false;
            }
            if self.selected_session.as_deref() == Some(session.id.as_str()) {
                self.web_enabled = session.web_enabled;
            }
            *existing = session;
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
        // work; the summary check reads the stored item after the merge.
        let completed = item.status.as_deref() == Some("completed");
        let is_tool = matches!(item.item_type.as_str(), "tool" | "toolCall");
        self.retire_decided_permission(&item);
        self.load_attachment_images_for(&item, cx);
        if let Some(plan) = plan_entries(&item) {
            self.set_plan(plan);
        }
        let index = self.apply_timeline_item(session_id, item);
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
        if completed {
            if is_tool && self.summaries_enabled {
                self.maybe_summarize_tool(index, cx);
            }
            // A finished item marks a turn boundary: the usage ledger has
            // a new row.
            self.refresh_context_usage(cx);
        }
    }
    /// Drop the permission card when the runtime decided its request
    /// without the card: switching the session to "Allow all" approves
    /// every pending request and replaces the permission row with a
    /// status.
    fn retire_decided_permission(&mut self, item: &AgentTimelineItem) {
        if item.status.is_none() {
            return;
        }
        let Some(request_id) = item.id.strip_prefix("permission-") else {
            return;
        };
        let showing = self
            .current_permission()
            .is_some_and(|permission| permission.request_id == request_id);
        let before = self.pending_permissions.len();
        self.pending_permissions
            .retain(|pending| pending.request_id != request_id);
        if showing && self.pending_permissions.len() != before {
            self.permission_responding = false;
        }
    }

    /// The permission card shown for the selected session, if any.
    fn current_permission(&self) -> Option<&PendingPermission> {
        let selected = self.selected_session.as_deref()?;
        self.pending_permissions
            .iter()
            .find(|permission| permission.session_id == selected)
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

    /// Apply one event; returns false when nothing visible changed.
    fn apply_service_event(&mut self, event: AgentServiceEvent, cx: &mut Context<Self>) -> bool {
        match event {
            AgentServiceEvent::RuntimeStatus(mut status) => {
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
                session_id, item, ..
            } => {
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
                let preview: String = questions
                    .first()
                    .map(|question| question.question.chars().take(140).collect())
                    .unwrap_or_default();
                self.notify_desktop(&session_id, "Maple has a question", &preview, cx);
                // Questions queue per session; one for a task that is not
                // on screen shows its card when the user switches there.
                let shows_now = self.current_question().is_none()
                    && self.selected_session.as_deref() == Some(session_id.as_str());
                self.pending_questions.push(PendingQuestion {
                    session_id,
                    request_id,
                    questions,
                });
                if shows_now {
                    // A fresh question takes the card; follow-ups queue
                    // behind it and keep the user's picks on this one.
                    self.question_selected.clear();
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
        if self.pending_questions.len() != before && self.is_selected(session_id) {
            self.reset_question_card(cx);
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
            AgentRunEvent::PermissionRequested { request, item } => {
                let selected = self.is_selected(session_id);
                if selected {
                    // The permission row stays in the transcript so the
                    // decision is visible after the card is answered.
                    self.load_attachment_images_for(&item, cx);
                    self.apply_timeline_item(session_id, item);
                }
                let arguments = serde_json::Value::Object(request.arguments);
                let arguments = permission_arguments(&request.tool_name, &arguments);
                let prompt = request
                    .prompt
                    .clone()
                    .unwrap_or_else(|| format!("Run tool {}?", request.tool_name));
                self.notify_desktop(session_id, "Maple needs permission", &prompt, cx);
                // The request is kept even when its session is not on
                // screen: the run blocks until it is answered, so the card
                // must appear when the user opens that session.
                self.pending_permissions
                    .retain(|pending| pending.request_id != request.request_id);
                self.pending_permissions.push(PendingPermission {
                    session_id: session_id.to_string(),
                    run_id: run_id.to_string(),
                    request_id: request.request_id,
                    tool_name: request.tool_name,
                    prompt: request.prompt,
                    arguments,
                });
                if selected {
                    self.permission_responding = false;
                } else {
                    self.bump_timeline_revision(session_id);
                    return false;
                }
            }
            AgentRunEvent::SubagentStarted {
                id,
                task,
                background,
            } => {
                if !self.is_selected(session_id) {
                    return false;
                }
                self.start_subagent(id, task, background, cx);
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
            AgentRunEvent::HistoryReplaced => {
                if self.is_selected(session_id) {
                    // Replace history only; the run and any pending
                    // permission keep flowing.
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
                    // The marker follows the latest terminal: only a
                    // successful completion off screen sets it. Opening the
                    // task (`set_active_session`) is the only other writer.
                    self.completed_unread_sessions.remove(session_id);
                    if matches!(terminal, maple_agent::agent::AgentRunTerminal::Completed)
                        && !self.is_selected(session_id)
                    {
                        self.completed_unread_sessions
                            .insert(session_id.to_string());
                        // A fresh completion is new activity: it wakes a
                        // task the user settled away earlier.
                        self.sidebar
                            .update(cx, |sidebar, cx| sidebar.wake_task(session_id, cx));
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
                let showing = self
                    .current_permission()
                    .is_some_and(|permission| permission.run_id == run_id);
                self.pending_permissions
                    .retain(|permission| permission.run_id != run_id);
                if showing {
                    self.permission_responding = false;
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
                let title = self
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .map(|session| session.title.clone())
                    .unwrap_or_else(|| "Task".to_string());
                self.notify_desktop(session_id, "Maple", &format!("“{title}” finished"), cx);
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

impl EventEmitter<LoggedOut> for ChatScreen {}

impl EventEmitter<OpenSettings> for ChatScreen {}
impl EventEmitter<OpenSettingsSection> for ChatScreen {}

/// Height hint for transcript rows that have not been measured yet, so a
/// freshly opened task has a scrollbar of about the right size on its
/// first frame. Rows are measured as they scroll into view.
const TRANSCRIPT_ROW_ESTIMATE: gpui::Pixels = px(96.);

/// The transcript list: bottom-aligned, following its tail so streaming
/// content stays in view until the user scrolls up, with enough overdraw
/// that a fast wheel scroll lands on measured rows.
fn transcript_list_state() -> gpui::ListState {
    let state = gpui::ListState::new(0, gpui::ListAlignment::Bottom, px(1024.));
    state.set_follow_mode(gpui::FollowMode::Tail);
    state
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
        if self.dialog_focus_pending {
            self.dialog_focus_pending = false;
            if let Some(handle) = self.dialog_focus.clone() {
                window.focus(&handle, cx);
            }
        }
        if self.root_menu_focus_pending {
            self.root_menu_focus_pending = false;
            if let Some(handle) = self.root_menu_focus.clone() {
                window.focus(&handle, cx);
            }
        } else if !self.root_menu_open
            && let Some(handle) = self.root_menu_focus.clone()
            && handle.is_focused(window)
        {
            // The menu closed while it held focus. Application Vim returns
            // to its proxy; Standard mode keeps the legacy composer return.
            if self.application_vim_enabled {
                if let Some(handle) = self.application_focus.clone() {
                    window.focus(&handle, cx);
                }
            } else if let Some(composer) = self.composer.clone() {
                let handle = composer.read(cx).focus_handle(cx);
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
        if self.screen_focus_pending {
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
        if self.question_focus_pending {
            self.question_focus_pending = false;
            if self.application_vim_enabled {
                self.application_vim.permission_choice = 0;
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
        let confirm_remove = self
            .confirm_remove_root
            .clone()
            .map(|root| self.render_confirm_remove(&root, cx));
        let trust_prompt = self
            .trust_prompt
            .clone()
            .map(|status| self.render_trust_prompt(&status, cx));
        let empty = self.timeline.is_empty();
        let collapsed = self.sidebar_collapsed;
        let loading_overlay = self.render_loading_overlay();
        let main = if empty {
            self.render_empty_state(cx)
        } else {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .h_full()
                .min_w_0()
                .child(self.render_header(cx))
                .child(self.render_transcript(cx))
                .when(self.awaiting_first_token && self.is_run_active(), |main| {
                    // Same gutter as transcript text so the dots line up
                    main.child(
                        div()
                            .w_full()
                            .max_w(CONTENT_WIDTH)
                            .mx_auto()
                            .px_6()
                            .child(render_waiting_indicator()),
                    )
                })
                .when_some(self.current_question(), |container, question| {
                    let input = self.pending_question_input.clone();
                    let step = self.question_step;
                    container.child(render_question_card(
                        question,
                        step,
                        input,
                        &self.question_selected,
                        cx,
                    ))
                })
                .when_some(self.current_permission(), |container, permission| {
                    container.child(render_permission_card(
                        permission,
                        self.permission_responding,
                        self.application_permission_choice(),
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
                        .children(self.render_btw_card(cx))
                        .children(self.render_subagents_card())
                        .children(self.render_plan_card(cx))
                        .child(self.render_composer(cx))
                        .children(self.render_slash_palette(cx))
                        .children(self.render_menu_panel(cx)),
                )
        };
        let main = match loading_overlay {
            Some(overlay) => div()
                .relative()
                .flex()
                .flex_col()
                .flex_1()
                .h_full()
                .min_w_0()
                .child(main)
                .child(overlay),
            None => main,
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
            .on_action(cx.listener(Self::root_menu_previous))
            .on_action(cx.listener(Self::root_menu_next))
            .on_action(cx.listener(Self::root_menu_confirm))
            .on_action(cx.listener(Self::previous_task))
            .on_action(cx.listener(Self::next_task))
            .on_action(cx.listener(Self::allow_permission))
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
                    .when(!collapsed, |row| {
                        // Cached: the sidebar's subtree is reused until the
                        // sidebar entity is notified, whatever this screen does.
                        row.child(
                            self.sidebar.clone().cached(
                                gpui::StyleRefinement::default()
                                    .w(SIDEBAR_WIDTH)
                                    .h_full()
                                    .flex_none(),
                            ),
                        )
                    })
                    .child(
                        div()
                            .relative()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .when(collapsed, |pane| {
                                pane.child(
                                    div()
                                        .absolute()
                                        .top_2()
                                        .left(crate::ui::titlebar::top_row_inset(px(12.)))
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(self.render_sidebar_toggle(cx))
                                        .child(wordmark(px(14.), theme::text_primary())),
                                )
                            })
                            .child(main)
                            .children(self.render_root_menu(cx)),
                    ),
            )
            .when_some(self.lightbox.clone(), |root, image| {
                root.child(motion::fade_in(
                    div()
                        .id("lightbox")
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
                                widgets::icon_button(
                                    "lightbox-close",
                                    "x",
                                    "Close",
                                    px(16.),
                                    theme::on_accent(),
                                )
                                .size_8()
                                .rounded_full()
                                .bg(theme::overlay_hover())
                                .tooltip(widgets::tooltip_for_action("Close", &ChatEscape)),
                            ),
                        ),
                    "lightbox-reveal",
                ))
            })
            .children(confirm_remove)
            .children(trust_prompt)
    }
}

impl ChatScreen {
    /// Panel icon that hides or shows the sidebar.
    fn render_sidebar_toggle(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        div()
            .id("sidebar-toggle")
            .flex_none()
            .size_7()
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::RADIUS_SM)
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
            .tooltip(widgets::tooltip_for_action(
                "Toggle sidebar",
                &ToggleSidebar,
            ))
            .on_click(cx.listener(|this, _event, window, cx| {
                this.execute_command(ChatCommand::ToggleSidebar, window, cx);
            }))
            .child(icon("panel-left", px(16.), theme::text_secondary()))
    }

    /// Hero layout for a task with no messages: display heading, composer,
    /// and privacy note centered in the pane (mirrors EmptyAgentState).
    fn render_empty_state(&mut self, cx: &mut Context<Self>) -> Div {
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
                            .child(self.render_composer(cx))
                            .children(self.render_slash_palette(cx))
                            .children(self.render_menu_panel(cx)),
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
                    .children(self.render_update_banner(cx))
                    .children(self.render_notice(cx)),
            );
        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(self.render_header(cx))
            .child(body)
    }

    /// Replace the plan and the completed count the card header shows.
    fn set_plan(&mut self, plan: Vec<PlanEntry>) {
        self.plan_done = plan
            .iter()
            .filter(|entry| entry.status == PlanStatus::Completed)
            .count();
        self.plan = plan;
    }

    /// Replace the session's MCP servers and the enabled count the
    /// composer chip shows.
    fn set_session_mcp(&mut self, servers: Vec<AgentSessionMcpServer>) {
        self.mcp_enabled_count = servers.iter().filter(|server| server.enabled).count();
        self.session_mcp = servers;
    }

    /// Cache the header title for the selected task.
    fn refresh_selected_title(&mut self) {
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
    fn notify_desktop(&self, tag: &str, title: &str, body: &str, cx: &gpui::App) {
        if !self.notify_enabled {
            log::info!("desktop notification skipped (disabled): {title}");
            return;
        }
        if self.window_active {
            log::info!("desktop notification skipped (window active): {title}");
            return;
        }
        log::info!("desktop notification sent: {title}");
        crate::notify::notify(cx, format!("task:{tag}"), title, body, &[]);
    }

    /// Load the plan card from the Maple billing API. Failures keep the
    /// previous card; the token-total fallback covers the first load.
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

/// Last path component of a project root, for chips and the sidebar.
/// Upper-case section heading in the sidebar.
fn section_label(text: &'static str) -> Div {
    div()
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(gpui::rgb(theme::text_muted()))
        .child(text)
}
