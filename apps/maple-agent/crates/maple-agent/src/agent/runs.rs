//! Runs: one at a time per task, the desktop queue, steering and Stop.
//!
//! A run is Maple's unit of work on a task: it starts with a prompt and ends
//! when Pi's session settles and no queued message is left to send. Its
//! events reach the host as [`AgentRunEvent`]s tagged with the run's id.
//!
//! Messages sent while a desktop run works wait in the task's queue as
//! chips the user can edit, cancel or steer in. When the session settles,
//! the run sends the waiting chips together, as one turn, before it ends.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use pi_ai::{Content, ImageContent, Message, StopReason};
use pi_coding_agent::Delivery;
use pi_coding_agent::extensions::CustomMessageDraft;
use pi_coding_agent::{
    AgentSession, AgentSessionError, AgentSessionEvent, PromptOptions, PromptOutcome,
    SessionMessage,
};
use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::attachments::{
    AgentAttachment, AgentAttachmentStore, attachment_prompt, prompt_has_inline_images,
    split_attachment_prompt,
};
use super::config::unix_ms;
use super::runtime::AgentRuntime;
use super::store::SessionFacts;
use super::surface::{AGENT_SURFACE_INACTIVE_ERROR, AgentSurfaceAccess};
use super::timeline::{
    LiveTimeline, MAPLE_NOTICE_ENTRY, STOPPED_NOTICE_TEXT, error_item, notice_entry_data,
    notice_item,
};
use super::{
    AgentDesktopQueueSnapshot, AgentEventDispatcher, AgentQueueControlRequest, AgentQueuedMessage,
    AgentRunEvent, AgentRunTerminal, AgentRunUsage, AgentRuntimeHandle, AgentSendMessageRequest,
    AgentServiceEvent, AgentTaskState, AgentTimelineItem, emit_agent_event,
};

mod live;
use live::LiveSnapshot;

const AGENT_RUN_EVENT_CAPACITY: usize = 256;
const MAX_DESKTOP_QUEUE_ITEMS: usize = 16;
const MAX_DESKTOP_QUEUE_TEXT_BYTES: usize = 32 * 1024;
const QUEUED_MESSAGE_ATTACHMENTS_ERROR: &str =
    "New attachments cannot be added while sending a queued message";
const EMPTY_PROMPT_ERROR: &str = "Prompt cannot be empty";
/// How often Stop repeats its abort until the run's prompt returns.
const STOP_REPEAT_INTERVAL: Duration = Duration::from_millis(100);
/// How long Stop and shutdown wait for a run to end before aborting it.
pub(super) const RUN_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

static NEXT_TIMELINE_REVISION: AtomicU64 = AtomicU64::new(1);

pub(super) fn next_timeline_revision() -> u64 {
    NEXT_TIMELINE_REVISION.fetch_add(1, Ordering::SeqCst)
}

static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_QUEUE_ID: AtomicU64 = AtomicU64::new(1);

fn next_run_id() -> String {
    let sequence = NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed);
    format!("run_{}_{sequence}", unix_ms())
}

fn next_queue_id() -> String {
    let sequence = NEXT_QUEUE_ID.fetch_add(1, Ordering::Relaxed);
    format!("queue_{}_{sequence}", unix_ms())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A message on its way to the model: what the user typed, the images they
/// attached, and, for a model that sees images, the images themselves.
#[derive(Clone, Debug, Default)]
struct Outgoing {
    text: String,
    attachments: Vec<AgentAttachment>,
    images: Vec<ImageContent>,
    /// Set for a message to the model alone, such as a background agent's
    /// result: Pi gets it as a custom message of this type, which the
    /// timeline does not show. It is never a chip the user sees, and goes
    /// to the model on its own.
    hidden: Option<&'static str>,
}

impl Outgoing {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }

    fn hidden(custom_type: &'static str, text: String) -> Self {
        Self {
            text,
            hidden: Some(custom_type),
            ..Self::default()
        }
    }

    /// A message read back from the text Pi was given, as when steering
    /// goes back to the queue. Its images are then referred to by source.
    fn from_prompt(prompt: String) -> Self {
        match split_attachment_prompt(&prompt) {
            Some((text, attachments)) => Self {
                text,
                images: attachments
                    .iter()
                    .filter(|attachment| attachment.is_image() && prompt_has_inline_images(&prompt))
                    .map(|attachment| ImageContent {
                        data: attachment.source.clone(),
                        mime_type: attachment.mime_type.clone(),
                    })
                    .collect(),
                attachments,
                ..Self::default()
            },
            None => Self::text(prompt),
        }
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty() && self.attachments.is_empty()
    }

    /// Resolve only this message's selected images just before it enters Pi.
    /// The queue keeps references, while Pi still receives ordinary pixels for
    /// its normal image normalization and context-token accounting.
    async fn prompt(
        &self,
        store: &AgentAttachmentStore,
        session_id: &str,
    ) -> Result<(String, Vec<ImageContent>), AgentSessionError> {
        let text = attachment_prompt(&self.text, &self.attachments, !self.images.is_empty());
        if self.images.is_empty() {
            return Ok((text, Vec::new()));
        }
        let store = store.clone();
        let session_id = session_id.to_string();
        let images = self.images.clone();
        tokio::task::spawn_blocking(move || {
            let mut messages = [Message::User(pi_ai::UserMessage {
                content: std::iter::once(Content::text(text))
                    .chain(images.into_iter().map(Content::Image))
                    .collect(),
                timestamp: 0,
            })];
            store.prepare_model_context(&session_id, &mut messages)?;
            let Message::User(user) = &mut messages[0] else {
                unreachable!()
            };
            let mut text = Vec::new();
            let mut images = Vec::new();
            for block in std::mem::take(&mut user.content) {
                match block {
                    Content::Text(block) => text.push(block.text),
                    Content::Image(image) => images.push(image),
                }
            }
            Ok::<_, String>((text.join("\n\n"), images))
        })
        .await
        .map_err(|error| {
            AgentSessionError::Failed(format!("Failed to prepare image attachments: {error}"))
        })?
        .map_err(AgentSessionError::Failed)
    }

    /// A hidden message as the custom message Pi gets; `None` for a
    /// message the user sent.
    fn custom_draft(&self) -> Option<CustomMessageDraft> {
        Some(CustomMessageDraft {
            custom_type: self.hidden?.to_string(),
            content: vec![Content::text(self.text.clone())],
            display: false,
            details: None,
        })
    }

    /// Steer a message the user sent into the session's turn. A hidden
    /// message never is: it goes to the model as a turn of its own.
    async fn steer(&self, session: &AgentSession, store: &AgentAttachmentStore, session_id: &str) {
        debug_assert!(self.hidden.is_none(), "a hidden message is not steered");
        let prepared = self.prompt(store, session_id).await;
        let result = match prepared {
            Ok((text, images)) => session.steer(&text, images).await,
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            log::warn!("Failed to steer the Agent task: {error}");
        }
    }
}

/// What a send started, staged or steered.
pub struct AgentRunHandle {
    /// The run that took the message: a new one, or the run a staged or
    /// steered message joined.
    pub run_id: String,
    /// The run's events, for a caller that follows the run itself. Empty
    /// for a message that joined a run.
    pub events: mpsc::Receiver<AgentRunEvent>,
    /// How the run ended, once it has.
    pub terminal: watch::Receiver<Option<AgentRunTerminal>>,
    /// The tokens the run used, once it has ended.
    pub usage: watch::Receiver<Option<AgentRunUsage>>,
    /// Whether `events` dropped events because its reader fell behind.
    pub event_overflowed: Arc<AtomicBool>,
    /// The chip a staged message became.
    pub queued: Option<AgentQueuedMessage>,
    pub queue: AgentDesktopQueueSnapshot,
}

impl AgentRunHandle {
    /// A handle for a message that joined `run_id` rather than starting it.
    fn joined(
        run_id: String,
        queued: Option<AgentQueuedMessage>,
        queue: AgentDesktopQueueSnapshot,
    ) -> Self {
        let (_events_tx, events) = mpsc::channel(1);
        let (_terminal_tx, terminal) = watch::channel(None);
        let (_usage_tx, usage) = watch::channel(None);
        Self {
            run_id,
            events,
            terminal,
            usage,
            event_overflowed: Arc::new(AtomicBool::new(false)),
            queued,
            queue,
        }
    }
}

/// Publishes one run's events, in order, to the host and to the run's own
/// bounded channel. A slow reader of the channel never holds the run back:
/// events that do not fit are dropped and the overflow is flagged.
#[derive(Clone)]
pub(super) struct RunEvents {
    dispatcher: AgentEventDispatcher,
    session_id: Arc<str>,
    run_id: Arc<str>,
    sender: mpsc::Sender<AgentRunEvent>,
    overflowed: Arc<AtomicBool>,
    order: Arc<Mutex<LiveSnapshot>>,
}

impl RunEvents {
    fn new(
        dispatcher: AgentEventDispatcher,
        session_id: &str,
        run_id: &str,
    ) -> (Self, mpsc::Receiver<AgentRunEvent>) {
        let (sender, receiver) = mpsc::channel(AGENT_RUN_EVENT_CAPACITY);
        (
            Self {
                dispatcher,
                session_id: Arc::from(session_id),
                run_id: Arc::from(run_id),
                sender,
                overflowed: Arc::new(AtomicBool::new(false)),
                order: Arc::new(Mutex::new(LiveSnapshot::default())),
            },
            receiver,
        )
    }

    pub(super) fn publish(&self, event: AgentRunEvent) {
        self.publish_row(event, None, false);
    }

    fn publish_row(&self, mut event: AgentRunEvent, tool: Option<&str>, completed: bool) {
        let mut live = lock(&self.order);
        live.drain_changes(&self.session_id);
        let mut shared = event.clone();
        match &mut shared {
            AgentRunEvent::TimelineItem(item)
            | AgentRunEvent::Error(item)
            | AgentRunEvent::QueuePromoted { item, .. } => {
                item.revision = next_timeline_revision();
                *item = live.row(item.clone(), &self.run_id, tool, completed);
                if let AgentRunEvent::TimelineItem(original)
                | AgentRunEvent::Error(original)
                | AgentRunEvent::QueuePromoted { item: original, .. } = &mut event
                {
                    original.live = item.live.clone();
                    original.persisted = item.persisted.clone();
                    original.revision = item.revision;
                }
            }
            AgentRunEvent::HistoryReplaced => {
                live.epoch += 1;
            }
            _ => {}
        }
        {
            emit_agent_event(
                &self.dispatcher,
                AgentServiceEvent::Run {
                    session_id: self.session_id.to_string(),
                    run_id: self.run_id.to_string(),
                    event: shared,
                },
            );
        }
        if let Err(mpsc::error::TrySendError::Full(_)) = self.sender.try_send(event) {
            self.overflowed.store(true, Ordering::Release);
        }
    }

    fn watch_store(&self, store: &Arc<crate::store::Store>, account: crate::store::AccountId) {
        let mut live = lock(&self.order);
        live.changes = Some(store.subscribe_changes());
        live.resolver = Some((Arc::clone(store), account));
    }

    fn watch_sources(&self, receipts: Option<crate::store::SourceReceipts>) {
        lock(&self.order).source_receipts = receipts;
    }

    fn drain_sources(&self) {
        lock(&self.order).drain_sources();
    }

    fn begin_message(&self) {
        lock(&self.order).begin_message();
    }

    fn note_source(&self, entry: &pi_coding_agent::session::SessionEntry) {
        let mut live = lock(&self.order);
        if let Some(receipt) = crate::store::SourceReceipt::of(entry) {
            live.note_source(receipt);
        }
        live.drain_changes(&self.session_id);
    }

    fn end_message(&self, message: &SessionMessage) -> Vec<AgentTimelineItem> {
        let mut live = lock(&self.order);
        let rows = live.end_message(message);
        live.drain_changes(&self.session_id);
        rows
    }
}

/// A run that has started and not yet finished.
struct ActiveRun {
    session_id: String,
    /// Cancelled by Stop.
    stopped: CancellationToken,
    /// Whether staged messages may still join the run. False once the run
    /// has sent its last queued messages, or was stopped.
    accepting: bool,
    events: RunEvents,
    /// The task's session, once the run has it.
    session: Option<AgentSession>,
    /// One ordered inbox, including messages sent before the session is ready.
    steers: mpsc::UnboundedSender<Outgoing>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ActiveRun {
    fn stageable(&self) -> bool {
        self.accepting && !self.stopped.is_cancelled()
    }

    fn steer(&mut self, message: Outgoing) {
        if self.steers.send(message).is_err() {
            log::warn!("The Agent run ended before it could accept steering");
        }
    }
}

/// A message waiting behind a run, with the images it shows a model that
/// sees them. A hidden message waits the same way, but the user never sees
/// it in the queue.
#[derive(Clone)]
struct Chip {
    message: AgentQueuedMessage,
    images: Vec<ImageContent>,
    hidden: Option<&'static str>,
}

impl Chip {
    fn new(session_id: &str, message: Outgoing) -> Self {
        let queue_id = next_queue_id();
        Self {
            message: AgentQueuedMessage {
                message_id: queue_id.clone(),
                queue_id,
                session_id: session_id.to_string(),
                text: message.text,
                attachments: message.attachments,
                created_ms: unix_ms(),
            },
            images: message.images,
            hidden: message.hidden,
        }
    }

    fn outgoing(self) -> Outgoing {
        Outgoing {
            text: self.message.text,
            attachments: self.message.attachments,
            images: self.images,
            hidden: self.hidden,
        }
    }
}

#[derive(Default)]
struct DesktopQueue {
    revision: u64,
    items: VecDeque<Chip>,
    /// A chip the user is editing; nothing is sent while one is.
    editing: Option<String>,
}

impl DesktopQueue {
    fn snapshot(&self) -> AgentDesktopQueueSnapshot {
        AgentDesktopQueueSnapshot {
            revision: self.revision,
            items: self
                .items
                .iter()
                .filter(|chip| chip.hidden.is_none())
                .map(|chip| chip.message.clone())
                .collect(),
        }
    }

    fn changed(&mut self) -> AgentDesktopQueueSnapshot {
        self.revision = self.revision.saturating_add(1);
        self.snapshot()
    }
}

fn empty_snapshot() -> AgentDesktopQueueSnapshot {
    AgentDesktopQueueSnapshot {
        revision: 0,
        items: Vec::new(),
    }
}

/// A runtime's active runs and its tasks' desktop queues.
#[derive(Default)]
pub(super) struct Runs {
    state: Mutex<RunsState>,
    /// Notified whenever a run ends.
    finished: Notify,
}

#[derive(Default)]
struct RunsState {
    runs: HashMap<String, ActiveRun>,
    queues: HashMap<String, DesktopQueue>,
}

impl RunsState {
    fn run_of(&self, session_id: &str) -> Option<(&String, &ActiveRun)> {
        self.runs
            .iter()
            .find(|(_, run)| run.session_id == session_id)
    }

    fn queue(&mut self, session_id: &str) -> &mut DesktopQueue {
        self.queues.entry(session_id.to_string()).or_default()
    }

    fn snapshot(&self, session_id: &str) -> AgentDesktopQueueSnapshot {
        self.queues
            .get(session_id)
            .map(DesktopQueue::snapshot)
            .unwrap_or_else(empty_snapshot)
    }

    fn remove_chip(
        &mut self,
        session_id: &str,
        queue_id: &str,
    ) -> Result<(Chip, AgentDesktopQueueSnapshot), String> {
        let Some(queue) = self.queues.get_mut(session_id) else {
            return Err("Queued Agent message is no longer available".to_string());
        };
        let Some(index) = queue
            .items
            .iter()
            .position(|item| item.message.queue_id == queue_id)
        else {
            return Err("Queued Agent message has already been sent".to_string());
        };
        let removed = queue.items.remove(index).expect("index was just found");
        if queue.editing.as_deref() == Some(queue_id) {
            queue.editing = None;
        }
        Ok((removed, queue.changed()))
    }

    /// Every chip, unless one is being edited.
    fn take_chips(&mut self, session_id: &str) -> Option<(Vec<Chip>, AgentDesktopQueueSnapshot)> {
        let queue = self.queues.get_mut(session_id)?;
        if queue.items.is_empty() || queue.editing.is_some() {
            return None;
        }
        let items = queue.items.drain(..).collect();
        Some((items, queue.changed()))
    }

    /// The hidden messages waiting behind a run, which leave the queue.
    fn take_hidden_chips(&mut self, session_id: &str) -> Vec<Chip> {
        let Some(queue) = self.queues.get_mut(session_id) else {
            return Vec::new();
        };
        let (hidden, visible): (VecDeque<Chip>, VecDeque<Chip>) = queue
            .items
            .drain(..)
            .partition(|chip| chip.hidden.is_some());
        queue.items = visible;
        hidden.into()
    }

    /// Every chip the user sees, unless one is being edited; hidden
    /// messages stay queued.
    fn take_visible_chips(
        &mut self,
        session_id: &str,
    ) -> Option<(Vec<Chip>, AgentDesktopQueueSnapshot)> {
        let queue = self.queues.get_mut(session_id)?;
        if queue.editing.is_some() || queue.items.iter().all(|chip| chip.hidden.is_some()) {
            return None;
        }
        let (hidden, visible): (VecDeque<Chip>, VecDeque<Chip>) = queue
            .items
            .drain(..)
            .partition(|chip| chip.hidden.is_some());
        queue.items = hidden;
        Some((visible.into(), queue.changed()))
    }
}

impl Runs {
    pub(super) fn while_all_idle<T>(
        &self,
        action: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let state = self.state();
        if !state.runs.is_empty() {
            return Err("Agent work is active".into());
        }
        action()
    }

    pub(super) fn while_idle<T>(
        &self,
        session_id: &str,
        action: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let state = self.state();
        if state.run_of(session_id).is_some() {
            return Err("The task is running; its model cannot change".into());
        }
        action()
    }

    pub(super) fn snapshot_marker(&self, session_id: &str) -> Option<(String, u64)> {
        self.state().run_of(session_id).map(|(id, run)| {
            let mut live = lock(&run.events.order);
            live.drain_changes(session_id);
            (id.clone(), live.epoch)
        })
    }

    pub(super) fn live_snapshot_page(
        &self,
        session_id: &str,
        max_rows: usize,
        max_bytes: usize,
    ) -> (Option<(String, u64)>, u64, Vec<AgentTimelineItem>, bool) {
        let state = self.state();
        if let Some((id, run)) = state.run_of(session_id) {
            let mut live = lock(&run.events.order);
            live.drain_changes(session_id);
            let (items, more) = live::bounded_tail(&live.items, max_rows, max_bytes);
            return (
                Some((id.clone(), live.epoch)),
                NEXT_TIMELINE_REVISION.load(Ordering::SeqCst) - 1,
                items,
                more || live.resync_required,
            );
        }
        (
            None,
            NEXT_TIMELINE_REVISION.load(Ordering::SeqCst) - 1,
            Vec::new(),
            false,
        )
    }

    fn state(&self) -> MutexGuard<'_, RunsState> {
        lock(&self.state)
    }

    /// Desktop runs by task id, as the status reports them.
    pub(super) fn desktop_runs(&self) -> HashMap<String, String> {
        self.state()
            .runs
            .iter()
            .map(|(run_id, run)| (run.session_id.clone(), run_id.clone()))
            .collect()
    }

    /// The tasks with a run.
    pub(super) fn running_task_ids(&self) -> HashSet<String> {
        self.state()
            .runs
            .values()
            .map(|run| run.session_id.clone())
            .collect()
    }

    pub(super) fn is_running(&self, session_id: &str) -> bool {
        self.state().run_of(session_id).is_some()
    }

    pub(super) fn queue_snapshot(&self, session_id: &str) -> AgentDesktopQueueSnapshot {
        self.state().snapshot(session_id)
    }

    pub(super) fn clear_queue(&self, session_id: &str) {
        self.state().queues.remove(session_id);
    }

    /// Stop every run and wait for them to end, aborting those that take
    /// longer than [`RUN_SHUTDOWN_TIMEOUT`].
    pub(super) async fn stop_all(&self) {
        let tasks: Vec<tokio::task::JoinHandle<()>> = {
            let mut state = self.state();
            state.queues.clear();
            state
                .runs
                .values_mut()
                .filter_map(|run| {
                    run.accepting = false;
                    run.stopped.cancel();
                    if let Some(session) = &run.session {
                        session.abort();
                    }
                    run.task.take()
                })
                .collect()
        };
        join_tasks(tasks, RUN_SHUTDOWN_TIMEOUT).await;
        self.state().runs.clear();
        self.finished.notify_waiters();
    }
}

/// Wait for `tasks`, aborting those still running after `timeout`. Every
/// task is joined either way, so none outlives the call.
async fn join_tasks(mut tasks: Vec<tokio::task::JoinHandle<()>>, timeout: Duration) {
    let graceful = futures_util::future::join_all(tasks.iter_mut());
    if tokio::time::timeout(timeout, graceful).await.is_err() {
        for task in &tasks {
            task.abort();
        }
        let _ = futures_util::future::join_all(tasks).await;
    }
}

/// A run registered by a send and not set up yet.
struct NewRun {
    run_id: String,
    prompts: Vec<Outgoing>,
    /// Chips the run sends; they leave the queue once it starts.
    consumed: Vec<String>,
    stopped: CancellationToken,
    receiver: mpsc::Receiver<AgentRunEvent>,
    events: RunEvents,
    steers: mpsc::UnboundedReceiver<Outgoing>,
}

/// What a send asks for once the queue and the task's run are considered.
enum Plan {
    /// The message joined a run.
    Joined(AgentRunHandle),
    /// A new run starts with these prompts, after taking these chips.
    Start(NewRun),
    /// The task's run is ending; try again once it has.
    Wait,
}

fn combined_marker(
    main: Option<(String, u64)>,
    external_revision: u64,
) -> Option<super::AgentRunMarker> {
    if main.is_none() && external_revision == 0 {
        return None;
    }
    let (run_id, epoch) = main.unwrap_or_default();
    Some(super::AgentRunMarker {
        run_id,
        epoch,
        external_revision,
    })
}

async fn runtime_marker(runtime: &AgentRuntime, task: &str) -> Option<super::AgentRunMarker> {
    combined_marker(
        runtime.runs.snapshot_marker(task),
        runtime.external_agents.live_revision(task).await,
    )
}

impl AgentRuntimeHandle {
    /// A run marker without loading Pi or reading transcript payloads.
    pub async fn live_task_marker(
        &self,
        task: &str,
    ) -> Result<Option<super::AgentRunMarker>, String> {
        self.verify_generation().await?;
        Ok(match self.current_runtime().await? {
            Some(runtime) => runtime_marker(&runtime, task).await,
            None => None,
        })
    }

    /// Capture only the bounded active overlay between stable committed/source
    /// markers. Whole-history replay remains in history_forward_page.
    pub async fn live_task_page(
        &self,
        task: &str,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<super::AgentLivePage, String> {
        self.verify_generation().await?;
        if max_rows == 0 || max_rows > 500 || !(256..=4 * 1024 * 1024).contains(&max_bytes) {
            return Err("Invalid live page limits".into());
        }
        let tasks = self.store()?;
        let runtime = self.current_runtime().await?;
        for _ in 0..16 {
            let before_run = match &runtime {
                Some(runtime) => runtime_marker(runtime, task).await,
                None => None,
            };
            let (stamp_store, stamp_task) = (Arc::clone(tasks.store()), task.to_string());
            let account = tasks.account();
            let before = tokio::task::spawn_blocking(move || {
                stamp_store.task_history_stamp(account, &stamp_task)
            })
            .await
            .map_err(|_| "Live page worker failed".to_string())?
            .map_err(|error| error.to_string())?;
            let (main_marker, revision, mut items, mut has_more) = runtime
                .as_ref()
                .map(|runtime| runtime.runs.live_snapshot_page(task, max_rows, max_bytes))
                .unwrap_or((
                    None,
                    NEXT_TIMELINE_REVISION.load(Ordering::SeqCst) - 1,
                    Vec::new(),
                    false,
                ));
            let external_revision = match &runtime {
                Some(runtime) => runtime.external_agents.live_revision(task).await,
                None => 0,
            };
            let marker = combined_marker(main_marker, external_revision);
            if let Some(runtime) = &runtime {
                for external in runtime.external_agents.live_rows(task).await {
                    let previous = items.iter_mut().find(|item| {
                        match (&item.persisted, &external.persisted) {
                            (Some(a), Some(b)) => {
                                a.task_id == b.task_id
                                    && a.rows_epoch == b.rows_epoch
                                    && a.row_seq == b.row_seq
                            }
                            _ => item.live.is_some() && item.live == external.live,
                        }
                    });
                    if let Some(previous) = previous {
                        let persisted = external
                            .persisted
                            .clone()
                            .or_else(|| previous.persisted.clone());
                        let alias = external.live.clone().or_else(|| previous.live.clone());
                        super::timeline::merge_item(previous, external);
                        previous.persisted = persisted;
                        previous.live = alias;
                    } else {
                        items.push(external);
                    }
                }
                if let Some(failure) = runtime.failures.get(task) {
                    items.push(failure);
                }
            }
            let (items, omitted) = live::bounded_tail(&items, max_rows, max_bytes);
            has_more |= omitted;
            let (stamp_store, stamp_task) = (Arc::clone(tasks.store()), task.to_string());
            let after = tokio::task::spawn_blocking(move || {
                stamp_store.task_history_stamp(account, &stamp_task)
            })
            .await
            .map_err(|_| "Live page worker failed".to_string())?
            .map_err(|error| error.to_string())?;
            let after_run = match &runtime {
                Some(runtime) => runtime_marker(runtime, task).await,
                None => None,
            };
            if before != after || before_run != marker || marker != after_run {
                continue;
            }
            return Ok(super::AgentLivePage {
                marker,
                stored_marker: (after.rows_epoch, after.last_seq),
                timeline_revision: revision,
                items,
                has_more,
                queue: runtime
                    .as_ref()
                    .map(|runtime| runtime.runs.queue_snapshot(task))
                    .unwrap_or(super::AgentDesktopQueueSnapshot {
                        revision: 0,
                        items: Vec::new(),
                    }),
            });
        }
        Err("The task changed while its live page was read; reload and try again".into())
    }

    /// Send a message to a task: start a run, or join the task's run as a
    /// queued chip or, with `steer`, as steering.
    pub async fn send_message(
        &self,
        request: AgentSendMessageRequest,
    ) -> Result<AgentRunHandle, String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let runtime = self.runtime().await?;
        runtime.send_desktop(request).await
    }

    /// Drop a message that waits behind the task's run.
    pub async fn cancel_queued_message(
        &self,
        request: AgentQueueControlRequest,
    ) -> Result<AgentDesktopQueueSnapshot, String> {
        let runtime = self.runtime().await?;
        let snapshot = {
            let mut state = runtime.runs.state();
            let (_, snapshot) = state.remove_chip(&request.session_id, &request.queue_id)?;
            snapshot
        };
        runtime.publish_queue(&request.session_id, snapshot.clone());
        Ok(snapshot)
    }

    /// Take a chip out of the queue to edit it in the composer.
    pub async fn unqueue_message_for_edit(
        &self,
        request: AgentQueueControlRequest,
    ) -> Result<AgentQueuedMessage, String> {
        let runtime = self.runtime().await?;
        let (removed, snapshot) = runtime
            .runs
            .state()
            .remove_chip(&request.session_id, &request.queue_id)?;
        runtime.publish_queue(&request.session_id, snapshot);
        Ok(removed.message)
    }

    /// Hold the queue while the user edits a chip: nothing is sent until
    /// the edit ends.
    pub async fn begin_queued_message_edit(
        &self,
        request: AgentQueueControlRequest,
    ) -> Result<(), String> {
        let runtime = self.runtime().await?;
        let mut state = runtime.runs.state();
        let Some(queue) = state.queues.get_mut(&request.session_id) else {
            return Err("Queued Agent message is no longer available".to_string());
        };
        if !queue
            .items
            .iter()
            .any(|item| item.message.queue_id == request.queue_id)
        {
            return Err("Queued Agent message has already been sent".to_string());
        }
        queue.editing = Some(request.queue_id);
        Ok(())
    }

    /// Release the hold of [`Self::begin_queued_message_edit`].
    pub async fn end_queued_message_edit(
        &self,
        request: AgentQueueControlRequest,
    ) -> Result<(), String> {
        let runtime = self.runtime().await?;
        let mut state = runtime.runs.state();
        if let Some(queue) = state.queues.get_mut(&request.session_id)
            && queue.editing.as_deref() == Some(request.queue_id.as_str())
        {
            queue.editing = None;
        }
        Ok(())
    }

    /// Stop a desktop run. The reply so far is kept, and the task gets a
    /// "Stopped by user" notice.
    pub async fn cancel_desktop_run(&self, run_id: String) -> Result<(), String> {
        let Some(runtime) = self.current_runtime().await? else {
            return Ok(());
        };
        runtime.stop_run(&run_id)
    }

    /// Stop the shared task regardless of the interface that admitted it.
    pub async fn cancel_session(&self, session_id: &str) -> Result<(), String> {
        let runtime = self.runtime().await?;
        runtime
            .stop_task_run(session_id, RUN_SHUTDOWN_TIMEOUT)
            .await;
        Ok(())
    }

    /// Inject at Pi's next steering boundary without starting a new turn.
    /// False means the run settled before admission and consumed no message.
    pub async fn steer_surface_message(
        &self,
        access: &AgentSurfaceAccess,
        request: AgentSendMessageRequest,
    ) -> Result<bool, String> {
        let runtime = self.runtime().await?;
        if request.session_id != access.session_id() || !runtime.surface_holds(access) {
            return Err(AGENT_SURFACE_INACTIVE_ERROR.into());
        }
        let message = runtime.outgoing(&request).await?;
        if message.is_empty() {
            return Err(EMPTY_PROMPT_ERROR.into());
        }
        let mut state = runtime.runs.state();
        let Some(run) = state
            .runs
            .values_mut()
            .find(|run| run.session_id == request.session_id && run.stageable())
        else {
            return Ok(false);
        };
        run.steers
            .send(message)
            .map_err(|_| "The task stopped before accepting steering".to_string())?;
        Ok(true)
    }

    /// Send a calling surface's prompt to a task it holds: a run of its own,
    /// once the task's last run has ended, whose events go to the surface
    /// alone. Nothing is queued or steered. Cancelling `cancellation` stops
    /// the run, as Stop does, or the send while it waits.
    pub async fn send_surface_message(
        &self,
        access: &AgentSurfaceAccess,
        request: AgentSendMessageRequest,
        cancellation: CancellationToken,
    ) -> Result<AgentRunHandle, String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let runtime = self.runtime().await?;
        runtime.send_surface(access, request, cancellation).await
    }
}

impl AgentRuntime {
    pub(super) fn publish_queue(&self, session_id: &str, snapshot: AgentDesktopQueueSnapshot) {
        let events = self
            .runs
            .state()
            .run_of(session_id)
            .map(|(_, run)| run.events.clone());
        if let Some(events) = events {
            events.publish(AgentRunEvent::QueueChanged(snapshot));
        }
    }

    async fn send_surface(
        self: &Arc<Self>,
        access: &AgentSurfaceAccess,
        request: AgentSendMessageRequest,
        cancellation: CancellationToken,
    ) -> Result<AgentRunHandle, String> {
        let session_id = request.session_id.clone();
        if session_id != access.session_id() {
            return Err("Agent surface access does not match this task".to_string());
        }
        if request.steer || request.queue_id.is_some() {
            return Err("A calling surface's message is never queued".to_string());
        }
        if !self.surface_holds(access) || self.store.get(&session_id)?.is_none() {
            return Err(AGENT_SURFACE_INACTIVE_ERROR.to_string());
        }
        let message = self.outgoing(&request).await?;
        if message.is_empty() {
            return Err(EMPTY_PROMPT_ERROR.to_string());
        }
        let run = {
            let mut state = self.runs.state();
            if cancellation.is_cancelled() {
                return Err("The prompt was cancelled before admission".into());
            }
            if state.run_of(&session_id).is_some() {
                return Err("This task already has an active run".into());
            }
            self.register_run(&mut state, &session_id, vec![message], Vec::new())
        };
        // Once admitted, work belongs to the host. Transport cancellation or
        // EOF affects only the projection; explicit Stop uses cancel_session.
        let handle = self.start_run(&request, run).await?;
        self.settle_provisional(&session_id);
        Ok(handle)
    }

    /// Stop the task's run, whoever drives it, and wait at most `timeout`
    /// for it to end.
    pub(super) async fn stop_task_run(&self, session_id: &str, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let finished = self.runs.finished.notified();
            tokio::pin!(finished);
            finished.as_mut().enable();
            {
                let mut state = self.runs.state();
                let Some(run) = state
                    .runs
                    .values_mut()
                    .find(|run| run.session_id == session_id)
                else {
                    return;
                };
                run.accepting = false;
                run.stopped.cancel();
                if let Some(session) = &run.session {
                    session.abort();
                }
            }
            if tokio::time::timeout_at(deadline, finished).await.is_err() {
                return;
            }
        }
    }

    pub(super) fn stop_run(&self, run_id: &str) -> Result<(), String> {
        let mut state = self.runs.state();
        let Some(run) = state.runs.get_mut(run_id) else {
            return Ok(());
        };
        run.accepting = false;
        run.stopped.cancel();
        if let Some(session) = &run.session {
            session.abort();
        }
        Ok(())
    }

    async fn send_desktop(
        self: &Arc<Self>,
        request: AgentSendMessageRequest,
    ) -> Result<AgentRunHandle, String> {
        if request.queue_id.is_some() && !request.attachments.is_empty() {
            return Err(QUEUED_MESSAGE_ATTACHMENTS_ERROR.to_string());
        }
        let message = self.outgoing(&request).await?;
        loop {
            let finished = self.runs.finished.notified();
            tokio::pin!(finished);
            finished.as_mut().enable();
            match self.plan_desktop_send(&request, &message)? {
                Plan::Joined(handle) => return Ok(handle),
                Plan::Wait => {
                    tokio::select! {
                        _ = &mut finished => {}
                        _ = self.lifetime.cancelled() => {
                            return Err(super::RUNTIME_NOT_RUNNING_ERROR.to_string());
                        }
                    }
                }
                Plan::Start(run) => return self.start_run(&request, run).await,
            }
        }
    }

    /// Resolve committed IDs as one batch before accepting the message. The
    /// transcript and queue carry file references; only selected model context
    /// materializes image pixels at the provider boundary.
    async fn outgoing(&self, request: &AgentSendMessageRequest) -> Result<Outgoing, String> {
        let text = request.text.trim().to_string();
        if request.attachments.is_empty() {
            return Ok(Outgoing::text(text));
        }
        let session_id = request.session_id.clone();
        if self.store.get(&session_id)?.is_none() {
            return Err(format!("Failed to find Agent task {session_id}"));
        }
        let store = AgentAttachmentStore::new(Arc::clone(self.store.store()), self.store.account());
        let ids = request.attachments.clone();
        let attachments =
            tokio::task::spawn_blocking(move || store.resolve_and_pin(&session_id, &ids))
                .await
                .map_err(|error| format!("Failed to prepare attachments: {error}"))??;
        let images = if request.vision_capable {
            attachments
                .iter()
                .filter(|attachment| attachment.is_image())
                .map(|attachment| ImageContent {
                    data: attachment.source.clone(),
                    mime_type: attachment.mime_type.clone(),
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(Outgoing {
            text,
            attachments,
            images,
            hidden: None,
        })
    }

    /// Decide what a desktop send does, and register a new run atomically
    /// with that decision.
    fn plan_desktop_send(
        &self,
        request: &AgentSendMessageRequest,
        message: &Outgoing,
    ) -> Result<Plan, String> {
        let text = message.text.as_str();
        let session_id = request.session_id.as_str();
        let mut state = self.runs.state();
        let joinable = match state.run_of(session_id) {
            Some((run_id, run)) if run.stageable() => Some(run_id.clone()),
            Some(_) => return Ok(Plan::Wait),
            None => None,
        };

        if let Some(run_id) = joinable {
            if request.steer {
                let mut steered = Vec::new();
                let snapshot = if let Some(queue_id) = request.queue_id.as_deref() {
                    if !text.is_empty() {
                        update_chip_text(&mut state, session_id, queue_id, text)?;
                    }
                    let (removed, snapshot) = state.remove_chip(session_id, queue_id)?;
                    steered.push(removed.outgoing());
                    Some(snapshot)
                } else if message.is_empty() {
                    let Some((items, snapshot)) = state.take_visible_chips(session_id) else {
                        return Err(if state.snapshot(session_id).items.is_empty() {
                            EMPTY_PROMPT_ERROR.to_string()
                        } else {
                            "Queued messages cannot be steered while one is being edited"
                                .to_string()
                        });
                    };
                    steered.extend(items.into_iter().map(Chip::outgoing));
                    Some(snapshot)
                } else {
                    steered.push(message.clone());
                    None
                };
                let run = state.runs.get_mut(&run_id).expect("the run was just found");
                for message in steered {
                    run.steer(message);
                }
                let events = run.events.clone();
                let queue = state.snapshot(session_id);
                drop(state);
                if let Some(snapshot) = snapshot {
                    events.publish(AgentRunEvent::QueueChanged(snapshot));
                }
                return Ok(Plan::Joined(AgentRunHandle::joined(run_id, None, queue)));
            }
            if message.is_empty() {
                return Err(EMPTY_PROMPT_ERROR.to_string());
            }
            if text.len() > MAX_DESKTOP_QUEUE_TEXT_BYTES {
                return Err("Queued Agent message is too large".to_string());
            }
            let queue = state.queue(session_id);
            if queue.items.len() >= MAX_DESKTOP_QUEUE_ITEMS {
                return Err("Agent task already has too many queued messages".to_string());
            }
            let chip = Chip::new(session_id, message.clone());
            let queued = chip.message.clone();
            queue.items.push_back(chip);
            let snapshot = queue.changed();
            let events = state.runs[&run_id].events.clone();
            drop(state);
            events.publish(AgentRunEvent::QueueChanged(snapshot.clone()));
            return Ok(Plan::Joined(AgentRunHandle::joined(
                run_id,
                Some(queued),
                snapshot,
            )));
        }

        // No run: start one. A chip the user picks goes alone; otherwise
        // the chips left from an earlier run go first, and the draft after
        // them. Chips are taken only once the run has started, so a setup
        // failure leaves them in place.
        let (prompts, consumed) = match request.queue_id.as_deref() {
            Some(queue_id) if request.steer => {
                if !text.is_empty() {
                    update_chip_text(&mut state, session_id, queue_id, text)?;
                }
                let chip = state
                    .queues
                    .get(session_id)
                    .and_then(|queue| {
                        queue
                            .items
                            .iter()
                            .find(|item| item.message.queue_id == queue_id)
                    })
                    .ok_or_else(|| "Queued Agent message has already been sent".to_string())?;
                (vec![chip.clone().outgoing()], vec![queue_id.to_string()])
            }
            _ => {
                let leftover: Vec<Chip> = state
                    .queues
                    .get(session_id)
                    .map(|queue| queue.items.iter().cloned().collect())
                    .unwrap_or_default();
                let consumed = leftover
                    .iter()
                    .map(|chip| chip.message.queue_id.clone())
                    .collect();
                let mut prompts: Vec<Outgoing> = leftover.into_iter().map(Chip::outgoing).collect();
                if !message.is_empty() {
                    prompts.push(message.clone());
                }
                if prompts.is_empty() {
                    return Err(EMPTY_PROMPT_ERROR.to_string());
                }
                (prompts, consumed)
            }
        };
        Ok(Plan::Start(
            self.register_run(&mut state, session_id, prompts, consumed),
        ))
    }

    /// Register a run of `surface` for `session_id`, to start with
    /// `prompts`. A desktop run's events also go to the host.
    fn register_run(
        &self,
        state: &mut RunsState,
        session_id: &str,
        prompts: Vec<Outgoing>,
        consumed: Vec<String>,
    ) -> NewRun {
        self.host.cancel_integrity_check();
        let run_id = next_run_id();
        let (events, receiver) = RunEvents::new(self.host.events.clone(), session_id, &run_id);
        events.watch_store(self.store.store(), self.store.account());
        // Stopping the runtime stops the run, also before it has a task.
        let stopped = self.lifetime.child_token();
        let (steers, steering) = mpsc::unbounded_channel();
        state.runs.insert(
            run_id.clone(),
            ActiveRun {
                session_id: session_id.to_string(),
                stopped: stopped.clone(),
                accepting: true,
                events: events.clone(),
                session: None,
                steers,
                task: None,
            },
        );
        NewRun {
            run_id,
            prompts,
            consumed,
            stopped,
            receiver,
            events,
            steers: steering,
        }
    }

    /// Set up a registered run and start it.
    async fn start_run(
        self: &Arc<Self>,
        request: &AgentSendMessageRequest,
        run: NewRun,
    ) -> Result<AgentRunHandle, String> {
        match self
            .prepare_run_session(request, &run.prompts, &run.events)
            .await
        {
            Ok(session) => self.launch_run(&request.session_id, run, session),
            Err(error) => {
                self.forget_run(&run.run_id);
                Err(error)
            }
        }
    }

    /// Drop a registered run that could not start.
    fn forget_run(&self, run_id: &str) {
        self.runs.state().runs.remove(run_id);
        self.runs.finished.notify_waiters();
    }

    /// Hand the model a message the user does not see, such as a background
    /// agent's result: behind the task's run, as its next turn, or in a run
    /// of its own. The task must be loaded.
    pub(super) async fn deliver_hidden(
        self: &Arc<Self>,
        session_id: &str,
        custom_type: &'static str,
        text: String,
    ) -> Result<(), String> {
        let message = Outgoing::hidden(custom_type, text);
        loop {
            let finished = self.runs.finished.notified();
            tokio::pin!(finished);
            finished.as_mut().enable();
            let plan = {
                let mut state = self.runs.state();
                match state.run_of(session_id) {
                    Some((run_id, run)) if run.stageable() => {
                        let run_id = run_id.clone();
                        state
                            .queue(session_id)
                            .items
                            .push_back(Chip::new(session_id, message.clone()));
                        let queue = state.snapshot(session_id);
                        Plan::Joined(AgentRunHandle::joined(run_id, None, queue))
                    }
                    Some(_) => Plan::Wait,
                    // Chips left from an earlier run wait for the user's
                    // next send; the message goes alone.
                    None => Plan::Start(self.register_run(
                        &mut state,
                        session_id,
                        vec![message.clone()],
                        Vec::new(),
                    )),
                }
            };
            match plan {
                Plan::Joined(_) => return Ok(()),
                Plan::Wait => {
                    tokio::select! {
                        _ = &mut finished => {}
                        _ = self.lifetime.cancelled() => {
                            return Err(super::RUNTIME_NOT_RUNNING_ERROR.to_string());
                        }
                    }
                }
                Plan::Start(run) => {
                    let session = match self.loaded_session(session_id).await {
                        Some(session) => session,
                        None => {
                            self.forget_run(&run.run_id);
                            return Err(format!("Agent task {session_id} is not open"));
                        }
                    };
                    if let Err(error) = self.wake(session_id) {
                        self.forget_run(&run.run_id);
                        return Err(error);
                    }
                    return self.launch_run(session_id, run, session).map(|_| ());
                }
            }
        }
    }

    /// A run is new activity: it wakes a settled task, which stays active
    /// until it is settled again by hand.
    fn wake(&self, session_id: &str) -> Result<(), String> {
        let settled = self
            .store
            .get(session_id)?
            .is_some_and(|row| row.state == AgentTaskState::Settled);
        if settled
            && let Some(woken) = self
                .store
                .update(session_id, |row| row.state = AgentTaskState::Active)?
        {
            emit_agent_event(
                &self.host.events,
                AgentServiceEvent::SessionUpdated {
                    session_id: session_id.to_string(),
                    run_id: None,
                    session: woken.summary(),
                },
            );
        }
        Ok(())
    }

    /// Start a registered run on the task's session.
    fn launch_run(
        self: &Arc<Self>,
        session_id: &str,
        run: NewRun,
        session: AgentSession,
    ) -> Result<AgentRunHandle, String> {
        let NewRun {
            run_id,
            prompts,
            consumed,
            stopped,
            receiver,
            events,
            steers,
        } = run;

        // Desktop sends and background hidden deliveries share this path.
        events.watch_sources(self.store.source_receipts(session_id));
        let (terminal_tx, terminal) = watch::channel(None);
        let (usage_tx, usage) = watch::channel(None);
        let overflowed = Arc::clone(&events.overflowed);
        // Spawned under the registry's lock, and only while the runtime
        // lives: shutdown cancels the lifetime before it collects the runs'
        // tasks, so it joins every task there is.
        let mut state = self.runs.state();
        if self.lifetime.is_cancelled() || !state.runs.contains_key(&run_id) {
            state.runs.remove(&run_id);
            drop(state);
            self.runs.finished.notify_waiters();
            return Err(super::RUNTIME_NOT_RUNNING_ERROR.to_string());
        }
        let mut changed = None;
        for queue_id in &consumed {
            if let Ok((_, snapshot)) = state.remove_chip(session_id, queue_id) {
                changed = Some(snapshot);
            }
        }
        let queue = state.snapshot(session_id);
        let run = state
            .runs
            .get_mut(&run_id)
            .expect("checked above, under the same lock");
        run.session = Some(session.clone());
        if let Some(snapshot) = changed {
            events.publish(AgentRunEvent::QueueChanged(snapshot));
        }
        events.publish(AgentRunEvent::Started);
        run.task = Some(tokio::spawn(Arc::clone(self).drive_run(DrivenRun {
            run_id: run_id.clone(),
            session_id: session_id.to_string(),
            session,
            events,
            prompts,
            stopped,
            terminal: terminal_tx,
            usage: usage_tx,
            steers,
        })));
        drop(state);
        Ok(AgentRunHandle {
            run_id,
            events: receiver,
            terminal,
            usage,
            event_overflowed: overflowed,
            queued: None,
            queue,
        })
    }

    /// The task's session for a run, with the task woken and named.
    async fn prepare_run_session(
        &self,
        request: &AgentSendMessageRequest,
        prompts: &[Outgoing],
        events: &RunEvents,
    ) -> Result<AgentSession, String> {
        let session_id = request.session_id.as_str();
        let row = self
            .store
            .get(session_id)?
            .ok_or_else(|| format!("Failed to find Agent task {session_id}"))?;
        let model_id = request.model.clone().unwrap_or_else(|| self.model.clone());
        if row.message_count > 0
            && let Some(locked) = row.model.as_deref()
            && locked != model_id
        {
            return Err(format!(
                "This task is locked to model {locked}. Start a new task to use {model_id}."
            ));
        }
        let model = self
            .pi_model(
                &model_id,
                request.context_limit.map(|limit| limit as u64),
                Some(request.vision_capable),
            )
            .await;
        let session = self.task_session(&row, model).await?;
        self.failures.clear(session_id);
        self.wake(session_id)?;
        // Named from the first message with text: images alone say nothing,
        // and a hidden message is not the user's.
        if super::tasks::names_from_prompt(&row)
            && let Some(first) = prompts
                .iter()
                .filter(|message| message.hidden.is_none())
                .map(|message| message.text.as_str())
                .find(|text| !text.is_empty())
        {
            let title = super::tasks::session_title_from_prompt(first);
            if let Some(named) = self
                .store
                .update(session_id, |row| row.title = title.clone())?
            {
                events.publish(AgentRunEvent::SessionUpdated(named.summary()));
                self.generate_title(session_id, first, title);
            }
        }
        Ok(session)
    }

    async fn drive_run(self: Arc<Self>, run: DrivenRun) {
        let DrivenRun {
            run_id,
            session_id,
            session,
            events,
            mut prompts,
            stopped,
            terminal,
            usage,
            mut steers,
        } = run;
        let attachments =
            AgentAttachmentStore::new(Arc::clone(self.store.store()), self.store.account());
        let watch = RunWatch::default();
        let listener = session.subscribe({
            let events = events.clone();
            let watch = watch.clone();
            let live = Mutex::new(LiveTimeline::default());
            move |event| watch.observe(event, &live, &events)
        });

        // The task's MCP servers and built-in CUA start with its run, which
        // waits a little for them, as Pi's first prompt does.
        let vision = session
            .model()
            .is_some_and(|model| model.input.contains(&pi_ai::InputModality::Image));
        if let Some(notice) = self
            .start_task_integrations(&session_id, vision, &stopped)
            .await
        {
            events.publish(AgentRunEvent::SetupWarning(notice));
        }

        let mut failure = None;
        let mut prompted = false;
        while !stopped.is_cancelled() {
            // Messages the user sent go together, as one turn; a hidden
            // message goes on its own.
            let batch = match prompts.iter().position(|message| message.hidden.is_some()) {
                Some(0) => 1,
                Some(index) => index,
                None => prompts.len(),
            };
            if batch == 0 {
                break;
            }
            let mut sending: Vec<Outgoing> = prompts.drain(..batch).collect();
            let first = sending.remove(0);
            // Steering queued before the prompt joins its first turn, so
            // several messages make one turn.
            while let Ok(message) = steers.try_recv() {
                message.steer(&session, &attachments, &session_id).await;
            }
            for message in &sending {
                message.steer(&session, &attachments, &session_id).await;
            }
            prompted = true;
            let result = match first.custom_draft() {
                Some(draft) => {
                    until_stopped(
                        &session,
                        &stopped,
                        with_steering(
                            &session,
                            &attachments,
                            &session_id,
                            &mut steers,
                            session.send_custom_message(draft),
                        ),
                    )
                    .await
                }
                None => {
                    async {
                        let (text, images) = first.prompt(&attachments, &session_id).await?;
                        let options = PromptOptions {
                            images,
                            expand: false,
                            ..PromptOptions::default()
                        };
                        until_stopped(
                            &session,
                            &stopped,
                            with_steering(
                                &session,
                                &attachments,
                                &session_id,
                                &mut steers,
                                session.prompt(&text, options),
                            ),
                        )
                        .await
                    }
                    .await
                }
            };
            if let Err(error) = result {
                // What was not sent yet goes back to the queue below.
                failure = Some(error.to_string());
                break;
            }
            if stopped.is_cancelled() {
                break;
            }
            // A message steered in after Pi's last look would wait for the
            // task's next prompt; send it now, before the messages still
            // waiting and the queued chips.
            // Close admission under the same lock used by send, then drain
            // everything accepted before that boundary before clearing Pi's queue.
            let chips = self.take_chips_or_close(&run_id, &session_id);
            while let Ok(message) = steers.try_recv() {
                message.steer(&session, &attachments, &session_id).await;
            }
            let (stranded, _) = session.clear_queue();
            let waiting = std::mem::take(&mut prompts);
            prompts = stranded.into_iter().map(Outgoing::from_prompt).collect();
            prompts.extend(waiting);
            match chips {
                Some((chips, snapshot)) => {
                    events.publish(AgentRunEvent::QueueChanged(snapshot));
                    prompts.extend(chips.into_iter().map(Chip::outgoing));
                }
                None if prompts.is_empty() => break,
                None => {}
            }
        }
        // No accepted message may be left in the inbox when its receiver drops.
        if let Some(run) = self.runs.state().runs.get_mut(&run_id) {
            run.accepting = false;
        }
        if failure.is_some() && !stopped.is_cancelled() {
            let (stranded, _) = session.clear_queue();
            let mut unsent: Vec<_> = stranded.into_iter().map(Outgoing::from_prompt).collect();
            while let Ok(message) = steers.try_recv() {
                unsent.push(message);
            }
            unsent.append(&mut prompts);
            prompts = unsent;
        }
        session.unsubscribe(listener);

        let cancelled = stopped.is_cancelled();
        if cancelled {
            // Steering the session never took, and messages the run did not
            // send yet, go back to the queue.
            let (stranded, _) = session.clear_queue();
            let mut unsent = if prompted {
                Vec::new()
            } else {
                std::mem::take(&mut prompts)
            };
            unsent.extend(stranded.into_iter().map(Outgoing::from_prompt));
            while let Ok(message) = steers.try_recv() {
                unsent.push(message);
            }
            unsent.extend(std::mem::take(&mut prompts));
            // Hidden messages still waiting go into the history with the
            // rest; the user's chips wait for their next send.
            let waiting = self.runs.state().take_hidden_chips(&session_id);
            unsent.extend(waiting.into_iter().map(Chip::outgoing));
            self.requeue(&session, &session_id, unsent, &events);
            if prompted {
                let id = format!("stopped-{}", pi_ai::now_ms());
                session.extension_context().append_entry(
                    MAPLE_NOTICE_ENTRY,
                    Some(notice_entry_data(&id, STOPPED_NOTICE_TEXT)),
                );
                events.publish(AgentRunEvent::TimelineItem(notice_item(
                    id,
                    "Agent notice",
                    STOPPED_NOTICE_TEXT,
                    pi_ai::now_ms(),
                )));
            }
        }
        if failure.is_some() && !prompts.is_empty() {
            // A failed prompt leaves the messages after it for the next send.
            self.requeue(&session, &session_id, std::mem::take(&mut prompts), &events);
        }
        let terminal_state = if cancelled {
            AgentRunTerminal::Cancelled
        } else if let Some(error) = failure {
            let item = error_item(error);
            self.remember_failure(&session_id, item.clone());
            events.publish(AgentRunEvent::Error(item));
            AgentRunTerminal::Failed
        } else if last_reply_failed(&session) {
            AgentRunTerminal::Failed
        } else {
            AgentRunTerminal::Completed
        };
        if watch.retried() {
            // Failed attempts that Pi retried left error rows behind; the
            // stored history hides them.
            events.publish(AgentRunEvent::HistoryReplaced);
        }

        let facts = session.with_session(SessionFacts::of);
        match self.store.refresh_caches(&session_id, &facts) {
            Ok(Some(row)) => events.publish(AgentRunEvent::SessionUpdated(row.summary())),
            Ok(None) => {}
            Err(error) => log::warn!("Failed to update the Agent task index: {error}"),
        }
        let _ = usage.send(Some(watch.usage()));
        events.publish(AgentRunEvent::Finished(terminal_state));
        let _ = terminal.send(Some(terminal_state));
        self.runs.state().runs.remove(&run_id);
        self.runs.finished.notify_waiters();
    }

    /// The chips to send next, or, when there are none, stop the run from
    /// taking more: a later send starts its own run.
    fn take_chips_or_close(
        &self,
        run_id: &str,
        session_id: &str,
    ) -> Option<(Vec<Chip>, AgentDesktopQueueSnapshot)> {
        let mut state = self.runs.state();
        let taken = state.take_chips(session_id);
        if taken.is_none()
            && let Some(run) = state.runs.get_mut(run_id)
        {
            run.accepting = false;
        }
        taken
    }

    /// Put messages a stopped run never sent back at the head of the queue.
    fn requeue(
        &self,
        session: &AgentSession,
        session_id: &str,
        messages: Vec<Outgoing>,
        events: &RunEvents,
    ) {
        // A hidden message is not the user's to send again: it goes into
        // the task's history, where the model reads it with the next prompt.
        let (hidden, messages): (Vec<Outgoing>, Vec<Outgoing>) = messages
            .into_iter()
            .partition(|message| message.hidden.is_some());
        let context = session.extension_context();
        for draft in hidden.iter().filter_map(Outgoing::custom_draft) {
            context.send_message(draft, Delivery::Append);
        }
        if messages.is_empty() {
            return;
        }
        let snapshot = {
            let mut state = self.runs.state();
            let queue = state.queue(session_id);
            for message in messages.into_iter().rev() {
                queue.items.push_front(Chip::new(session_id, message));
            }
            queue.changed()
        };
        events.publish(AgentRunEvent::QueueChanged(snapshot));
    }
}

/// Poll the prompt and the run's steering inbox together. Only this future
/// drains the inbox, so async input handlers cannot reorder accepted steers.
async fn with_steering(
    session: &AgentSession,
    attachments: &AgentAttachmentStore,
    session_id: &str,
    steers: &mut mpsc::UnboundedReceiver<Outgoing>,
    prompt: impl std::future::Future<Output = Result<PromptOutcome, AgentSessionError>>,
) -> Result<PromptOutcome, AgentSessionError> {
    tokio::pin!(prompt);
    loop {
        tokio::select! {
            result = &mut prompt => return result,
            Some(message) = steers.recv() => message.steer(session, attachments, session_id).await,
        }
    }
}

/// Run one prompt. Stop aborts it, and keeps aborting until the prompt
/// returns: an abort that lands while Pi is still setting the run up would
/// otherwise be forgotten when the run starts.
async fn until_stopped(
    session: &AgentSession,
    stopped: &CancellationToken,
    prompt: impl std::future::Future<Output = Result<PromptOutcome, AgentSessionError>>,
) -> Result<PromptOutcome, AgentSessionError> {
    tokio::pin!(prompt);
    if let Some(result) = tokio::select! {
        biased;
        result = &mut prompt => Some(result),
        _ = stopped.cancelled() => None,
    } {
        return result;
    }
    let mut again = tokio::time::interval(STOP_REPEAT_INTERVAL);
    loop {
        session.abort();
        tokio::select! {
            biased;
            result = &mut prompt => return result,
            _ = again.tick() => {}
        }
    }
}

/// Change a chip's text before it is sent.
fn update_chip_text(
    state: &mut RunsState,
    session_id: &str,
    queue_id: &str,
    text: &str,
) -> Result<(), String> {
    if text.len() > MAX_DESKTOP_QUEUE_TEXT_BYTES {
        return Err("Queued Agent message is too large".to_string());
    }
    let Some(queue) = state.queues.get_mut(session_id) else {
        return Err("Queued Agent message is no longer available".to_string());
    };
    let Some(item) = queue
        .items
        .iter_mut()
        .find(|item| item.message.queue_id == queue_id)
    else {
        return Err("Queued Agent message has already been sent".to_string());
    };
    item.message.text = text.to_string();
    if queue.editing.as_deref() == Some(queue_id) {
        queue.editing = None;
    }
    queue.changed();
    Ok(())
}

/// Whether the session's last reply failed.
fn last_reply_failed(session: &AgentSession) -> bool {
    session
        .messages()
        .iter()
        .rev()
        .find_map(|message| match message {
            SessionMessage::Llm(Message::Assistant(reply)) => Some(reply.stop_reason),
            _ => None,
        })
        .is_some_and(|reason| reason == StopReason::Error)
}

struct DrivenRun {
    run_id: String,
    session_id: String,
    session: AgentSession,
    events: RunEvents,
    prompts: Vec<Outgoing>,
    stopped: CancellationToken,
    terminal: watch::Sender<Option<AgentRunTerminal>>,
    usage: watch::Sender<Option<AgentRunUsage>>,
    steers: mpsc::UnboundedReceiver<Outgoing>,
}

/// What a run's listener saw: the usage of its replies and whether Pi
/// retried.
#[derive(Clone, Default)]
struct RunWatch {
    state: Arc<Mutex<RunWatchState>>,
}

#[derive(Default)]
struct RunWatchState {
    usage: AgentRunUsage,
    retried: bool,
}

impl RunWatch {
    fn observe(&self, event: &AgentSessionEvent, live: &Mutex<LiveTimeline>, events: &RunEvents) {
        // Pi has released its session lock before forwarding these callbacks.
        // Observed identities remain useful even when the preceding SQL write failed.
        events.drain_sources();
        match event {
            AgentSessionEvent::Agent(event) => {
                if matches!(event, pi_agent_core::AgentEvent::MessageStart { message }
                    if matches!(message, SessionMessage::Llm(Message::User(_) | Message::Assistant(_)) | SessionMessage::Custom(_)))
                {
                    events.begin_message();
                }
                if let pi_agent_core::AgentEvent::MessageEnd {
                    message: SessionMessage::Llm(Message::Assistant(reply)),
                } = event
                {
                    let mut state = lock(&self.state);
                    state.usage.input_tokens += reply.usage.input;
                    state.usage.output_tokens += reply.usage.output;
                    state.usage.total_tokens += reply.usage.total_tokens;
                    state.usage.cached_read_tokens += reply.usage.cache_read;
                    state.usage.cached_write_tokens += reply.usage.cache_write;
                }
                let projected = lock(live).rows(event);
                let (rows, tool, completed) = if let pi_agent_core::AgentEvent::MessageEnd {
                    message,
                } = event
                {
                    let rows = events.end_message(message);
                    (
                        if rows.is_empty() { projected } else { rows },
                        LiveSnapshot::tool_for_message(message),
                        true,
                    )
                } else {
                    let tool = match event {
                        pi_agent_core::AgentEvent::ToolExecutionStart { tool_call_id, .. }
                        | pi_agent_core::AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                            Some(tool_call_id.as_str())
                        }
                        _ => None,
                    };
                    (projected, tool, false)
                };
                for row in rows {
                    events.publish_row(AgentRunEvent::TimelineItem(row), tool, completed);
                }
            }
            AgentSessionEvent::EntryAppended { entry } => events.note_source(entry),
            AgentSessionEvent::RetryStart { .. } => lock(&self.state).retried = true,
            AgentSessionEvent::CompactionStart { .. } => events.publish(AgentRunEvent::Compacting),
            AgentSessionEvent::CompactionEnd {
                result: Some(_), ..
            } => {
                events.publish(AgentRunEvent::Compacted);
                events.publish(AgentRunEvent::HistoryReplaced);
            }
            AgentSessionEvent::CompactionEnd {
                error: Some(error), ..
            } => log::warn!("Agent compaction failed: {error}"),
            AgentSessionEvent::PersistenceError(_) => {
                // Store adapters latch the typed failure for visible retry.
                // Pi's generic error string can contain paths or values.
                log::warn!("Agent task changes are waiting for a persistence retry");
            }
            AgentSessionEvent::ExtensionError(report) => {
                log::warn!("An Agent extension failed: {report:?}");
            }
            _ => {}
        }
    }

    fn usage(&self) -> AgentRunUsage {
        lock(&self.state).usage
    }

    fn retried(&self) -> bool {
        lock(&self.state).retried
    }
}

/// The error rows of runs that failed before Pi recorded anything, by task,
/// kept so a reload still shows why the last run ended.
#[derive(Default)]
pub(super) struct Failures {
    rows: Mutex<HashMap<String, AgentTimelineItem>>,
}

impl Failures {
    pub(super) fn get(&self, session_id: &str) -> Option<AgentTimelineItem> {
        lock(&self.rows).get(session_id).cloned()
    }

    pub(super) fn set(&self, session_id: &str, item: AgentTimelineItem) {
        lock(&self.rows).insert(session_id.to_string(), item);
    }

    pub(super) fn clear(&self, session_id: &str) {
        lock(&self.rows).remove(session_id);
    }
}

impl AgentRuntime {
    fn remember_failure(&self, session_id: &str, item: AgentTimelineItem) {
        self.failures.set(session_id, item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests::{Harness, eventually, finished};

    #[test]
    fn external_only_markers_retain_revision_after_a_turn_finishes() {
        assert!(combined_marker(None, 0).is_none());
        let running = combined_marker(None, 10).unwrap();
        let finished = combined_marker(None, 11).unwrap();
        assert!(running.run_id.is_empty());
        assert_ne!(running, finished);
        assert_eq!(Some(finished), combined_marker(None, 11));
        assert_ne!(
            combined_marker(Some(("run".into(), 3)), 10),
            combined_marker(Some(("run".into(), 3)), 11)
        );
    }

    #[tokio::test]
    async fn ordinary_pi_callbacks_reconcile_adapter_receipts_with_saved_rows() {
        let harness = Harness::new().await;
        let runtime = harness.runtime().await;
        let task = harness.create_task().await;
        let mut identities = Vec::new();
        for _ in 0..2 {
            harness.faux.push_text("Repeated answer.");
            let mut handle = harness.send(&task, "Repeated question.").await;
            assert_eq!(finished(&mut handle).await, AgentRunTerminal::Completed);
            let mut completed = HashMap::new();
            while let Ok(event) = handle.events.try_recv() {
                if let AgentRunEvent::TimelineItem(row) = event
                    && let Some(identity) = row.persisted
                {
                    assert!(row.live.is_some());
                    completed.insert(row.role, identity);
                }
            }
            let user = completed
                .remove(&Some("user".into()))
                .expect("saved user row");
            let assistant = completed
                .remove(&Some("assistant".into()))
                .expect("saved assistant row");
            assert_ne!(user.row_seq, assistant.row_seq);
            identities.extend([user, assistant]);
        }
        let history = runtime
            .store
            .store()
            .history_forward_page(
                runtime.store.account(),
                &crate::store::HistoryPageRequest {
                    task: task.clone(),
                    cursor: None,
                    limit_rows: Some(20),
                    limit_bytes: None,
                },
                crate::store::PageTransport::Local,
            )
            .unwrap();
        for identity in &identities {
            assert!(
                history
                    .rows
                    .iter()
                    .any(|row| row.rows_epoch == identity.rows_epoch
                        && row.row_seq == identity.row_seq
                        && row.update_seq == identity.update_seq)
            );
        }
        let unique: HashSet<_> = identities.iter().map(|identity| identity.row_seq).collect();
        assert_eq!(unique.len(), 4);
    }

    #[tokio::test]
    async fn background_hidden_delivery_uses_the_same_adapter_receipt_feed() {
        let harness = Harness::new().await;
        let runtime = harness.runtime().await;
        let task = harness.create_task().await;
        harness.faux.push_text("Initial answer.");
        let mut first = harness.send(&task, "Start.").await;
        assert_eq!(finished(&mut first).await, AgentRunTerminal::Completed);
        harness.faux.push_text("Background answer.");
        runtime
            .deliver_hidden(&task, "maple.test", "Background result.".into())
            .await
            .unwrap();
        eventually(|| runtime.runs.state().runs.is_empty()).await;
        let saved = harness.recorder.events().into_iter().find_map(|event| {
            if let AgentServiceEvent::Run {
                event: AgentRunEvent::TimelineItem(row),
                ..
            } = event
                && row.text.as_deref() == Some("Background answer.")
            {
                row.persisted
            } else {
                None
            }
        });
        assert!(
            saved.is_some(),
            "hidden-triggered assistant replies carry saved identities"
        );
    }

    #[tokio::test]
    async fn steering_before_session_setup_keeps_its_order_and_survives_stop() {
        for stop in [false, true] {
            let harness = Harness::new().await;
            let runtime = harness.runtime().await;
            let task = harness.create_task().await;
            let request = AgentSendMessageRequest {
                session_id: task.clone(),
                text: "First".into(),
                model: None,
                context_limit: Some(200_000),
                vision_capable: false,
                steer: false,
                queue_id: None,
                attachments: Vec::new(),
            };
            let Plan::Start(run) = runtime
                .plan_desktop_send(&request, &Outgoing::text("First"))
                .unwrap()
            else {
                panic!("the first message starts a run");
            };
            for text in ["Second", "Third"] {
                let steer = AgentSendMessageRequest {
                    text: text.into(),
                    steer: true,
                    ..request.clone()
                };
                assert!(matches!(
                    runtime
                        .plan_desktop_send(&steer, &Outgoing::text(text))
                        .unwrap(),
                    Plan::Joined(_)
                ));
            }
            if stop {
                runtime.stop_run(&run.run_id).unwrap();
            } else {
                harness.faux.push_text("Done.");
            }
            let mut handle = runtime.start_run(&request, run).await.unwrap();
            let terminal = finished(&mut handle).await;
            if stop {
                assert_eq!(terminal, AgentRunTerminal::Cancelled);
                assert!(harness.faux.requests().is_empty());
                let queue = runtime.runs.queue_snapshot(&task);
                let texts: Vec<_> = queue.items.iter().map(|item| item.text.as_str()).collect();
                assert_eq!(texts, ["First", "Second", "Third"]);
            } else {
                assert_eq!(terminal, AgentRunTerminal::Completed);
                let requests = harness.faux.requests();
                assert_eq!(requests.len(), 1);
                let texts: Vec<_> = requests[0]
                    .context
                    .messages
                    .iter()
                    .filter_map(|message| match message {
                        Message::User(user) => Some(pi_ai::content_text(&user.content)),
                        _ => None,
                    })
                    .collect();
                assert_eq!(texts, ["First", "Second", "Third"]);
            }
        }
    }
}
