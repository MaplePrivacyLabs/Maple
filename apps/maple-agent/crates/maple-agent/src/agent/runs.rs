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

use pi_ai::{ImageContent, Message, StopReason};
use pi_coding_agent::{AgentSession, AgentSessionEvent, PromptOptions, SessionMessage};
use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::attachments::{AgentImageAttachment, image_prompt, split_image_prompt};
use super::config::{account_attachment_store, unix_ms};
use super::runtime::AgentRuntime;
use super::store::SessionFacts;
use super::timeline::{
    LiveTimeline, MAPLE_NOTICE_ENTRY, STOPPED_NOTICE_TEXT, error_item, notice_entry_data,
    notice_item,
};
use super::{
    AgentDesktopQueueSnapshot, AgentEventDispatcher, AgentQueueControlRequest, AgentQueuedMessage,
    AgentRunEvent, AgentRunSurface, AgentRunTerminal, AgentRunUsage, AgentRuntimeHandle,
    AgentSendMessageRequest, AgentServiceEvent, AgentTaskState, AgentTimelineItem,
    emit_agent_event,
};

const AGENT_RUN_EVENT_CAPACITY: usize = 256;
const MAX_DESKTOP_QUEUE_ITEMS: usize = 16;
const MAX_DESKTOP_QUEUE_TEXT_BYTES: usize = 32 * 1024;
const QUEUED_MESSAGE_ATTACHMENTS_ERROR: &str =
    "New images cannot be added while sending a queued message";
const EMPTY_PROMPT_ERROR: &str = "Prompt cannot be empty";
/// How often Stop repeats its abort until the run's prompt returns.
const STOP_REPEAT_INTERVAL: Duration = Duration::from_millis(100);
/// How long Stop and shutdown wait for a run to end before aborting it.
pub(super) const RUN_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

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
    attachments: Vec<AgentImageAttachment>,
    images: Vec<ImageContent>,
}

impl Outgoing {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }

    /// A message read back from the text Pi was given, as when steering
    /// goes back to the queue. Its images are then referred to by source.
    fn from_prompt(prompt: String) -> Self {
        match split_image_prompt(&prompt) {
            Some((text, attachments)) => Self {
                text,
                attachments,
                images: Vec::new(),
            },
            None => Self::text(prompt),
        }
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty() && self.attachments.is_empty()
    }

    /// The text and images Pi gets: the text names the attachments, and a
    /// model without the images is told to look with `read_image`.
    fn prompt(&self) -> (String, Vec<ImageContent>) {
        (
            image_prompt(&self.text, &self.attachments, !self.images.is_empty()),
            self.images.clone(),
        )
    }

    fn steer(&self, session: &AgentSession) {
        let (text, images) = self.prompt();
        session.steer(&text, images);
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

/// Whether a run's events also go to the host's event sink. A calling
/// surface that follows its runs through [`AgentRunHandle::events`] keeps
/// them from the desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostEvents {
    Publish,
    Suppress,
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
    host: HostEvents,
    overflowed: Arc<AtomicBool>,
    order: Arc<Mutex<()>>,
}

impl RunEvents {
    fn new(
        dispatcher: AgentEventDispatcher,
        session_id: &str,
        run_id: &str,
        host: HostEvents,
    ) -> (Self, mpsc::Receiver<AgentRunEvent>) {
        let (sender, receiver) = mpsc::channel(AGENT_RUN_EVENT_CAPACITY);
        (
            Self {
                dispatcher,
                session_id: Arc::from(session_id),
                run_id: Arc::from(run_id),
                sender,
                host,
                overflowed: Arc::new(AtomicBool::new(false)),
                order: Arc::new(Mutex::new(())),
            },
            receiver,
        )
    }

    pub(super) fn publish(&self, event: AgentRunEvent) {
        let _order = lock(&self.order);
        if self.host == HostEvents::Publish {
            emit_agent_event(
                &self.dispatcher,
                AgentServiceEvent::Run {
                    session_id: self.session_id.to_string(),
                    run_id: self.run_id.to_string(),
                    event: event.clone(),
                },
            );
        }
        if let Err(mpsc::error::TrySendError::Full(_)) = self.sender.try_send(event) {
            self.overflowed.store(true, Ordering::Release);
        }
    }
}

/// A run that has started and not yet finished.
struct ActiveRun {
    session_id: String,
    surface: AgentRunSurface,
    /// Cancelled by Stop.
    stopped: CancellationToken,
    /// Whether staged messages may still join the run. False once the run
    /// has sent its last queued messages, or was stopped.
    accepting: bool,
    events: RunEvents,
    /// The task's session, once the run has it.
    session: Option<AgentSession>,
    /// Messages steered in before the session was ready.
    pending_steers: Vec<Outgoing>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ActiveRun {
    fn stageable(&self) -> bool {
        self.surface == AgentRunSurface::Desktop && self.accepting && !self.stopped.is_cancelled()
    }

    fn steer(&mut self, message: Outgoing) {
        match &self.session {
            Some(session) => message.steer(session),
            None => self.pending_steers.push(message),
        }
    }
}

/// A message waiting behind a run, with the images it shows a model that
/// sees them.
#[derive(Clone)]
struct Chip {
    message: AgentQueuedMessage,
    images: Vec<ImageContent>,
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
        }
    }

    fn outgoing(self) -> Outgoing {
        Outgoing {
            text: self.message.text,
            attachments: self.message.attachments,
            images: self.images,
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
            items: self.items.iter().map(|chip| chip.message.clone()).collect(),
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
}

impl Runs {
    fn state(&self) -> MutexGuard<'_, RunsState> {
        lock(&self.state)
    }

    /// Desktop runs by task id, as the status reports them.
    pub(super) fn desktop_runs(&self) -> HashMap<String, String> {
        self.state()
            .runs
            .iter()
            .filter(|(_, run)| run.surface == AgentRunSurface::Desktop)
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

impl AgentRuntimeHandle {
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
        runtime.stop_run(&run_id, AgentRunSurface::Desktop)
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

    pub(super) fn stop_run(&self, run_id: &str, surface: AgentRunSurface) -> Result<(), String> {
        let mut state = self.runs.state();
        let Some(run) = state.runs.get_mut(run_id) else {
            return Ok(());
        };
        if run.surface != surface {
            return Err("Agent run is controlled by another Agent surface".to_string());
        }
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

    /// The message a send carries. Its images are stored with the task
    /// first; a model that sees images gets them beside the text.
    async fn outgoing(&self, request: &AgentSendMessageRequest) -> Result<Outgoing, String> {
        let text = request.text.trim().to_string();
        if request.attachments.is_empty() {
            return Ok(Outgoing::text(text));
        }
        let session_id = request.session_id.clone();
        if self.store.get(&session_id)?.is_none() {
            return Err(format!("Failed to find Agent task {session_id}"));
        }
        let store = account_attachment_store(&self.host.paths, &self.user_id)?;
        let uploads = request.attachments.clone();
        let stored =
            tokio::task::spawn_blocking(move || store.store_uploads(&session_id, &uploads))
                .await
                .map_err(|error| format!("Failed to store the images: {error}"))??;
        let images = if request.vision_capable {
            stored
                .iter()
                .map(|image| ImageContent {
                    data: image.base64_data.clone(),
                    mime_type: image.attachment.mime_type.clone(),
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(Outgoing {
            text,
            attachments: stored.into_iter().map(|image| image.attachment).collect(),
            images,
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
            Some((_, run)) if run.surface != AgentRunSurface::Desktop => {
                return Err("This Agent task is controlled by another Agent surface".to_string());
            }
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
                    let Some((items, snapshot)) = state.take_chips(session_id) else {
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
        let run_id = next_run_id();
        let (events, receiver) = RunEvents::new(
            self.host.events.clone(),
            session_id,
            &run_id,
            HostEvents::Publish,
        );
        // Stopping the runtime stops the run, also before it has a task.
        let stopped = self.lifetime.child_token();
        state.runs.insert(
            run_id.clone(),
            ActiveRun {
                session_id: session_id.to_string(),
                surface: AgentRunSurface::Desktop,
                stopped: stopped.clone(),
                accepting: true,
                events: events.clone(),
                session: None,
                pending_steers: Vec::new(),
                task: None,
            },
        );
        Ok(Plan::Start(NewRun {
            run_id,
            prompts,
            consumed,
            stopped,
            receiver,
            events,
        }))
    }

    /// Set up a registered run and start it.
    async fn start_run(
        self: &Arc<Self>,
        request: &AgentSendMessageRequest,
        run: NewRun,
    ) -> Result<AgentRunHandle, String> {
        let NewRun {
            run_id,
            prompts,
            consumed,
            stopped,
            receiver,
            events,
        } = run;
        let session_id = request.session_id.as_str();
        let session = match self.prepare_run_session(request, &prompts, &events).await {
            Ok(session) => session,
            Err(error) => {
                self.runs.state().runs.remove(&run_id);
                self.runs.finished.notify_waiters();
                return Err(error);
            }
        };

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
        for message in std::mem::take(&mut run.pending_steers) {
            message.steer(&session);
        }
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
        let model = self.pi_model(
            &model_id,
            request.context_limit.map(|limit| limit as u64),
            request.vision_capable,
        );
        let session = self.task_session(&row, model).await?;
        self.failures.clear(session_id);

        // A run is new activity: it wakes a settled task, which stays
        // active until it is settled again by hand.
        if row.state == AgentTaskState::Settled
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
        // Named from the first message with text: images alone say nothing.
        if super::tasks::names_from_prompt(&row)
            && let Some(first) = prompts
                .iter()
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
        } = run;
        let watch = RunWatch::default();
        let listener = session.subscribe({
            let events = events.clone();
            let watch = watch.clone();
            let live = Mutex::new(LiveTimeline::default());
            move |event| watch.observe(event, &live, &events)
        });

        // The task's MCP servers start with its run, which waits a little
        // for them, as Pi's first prompt does.
        if let Some(notice) = self.start_task_mcp(&session_id, &stopped).await {
            events.publish(AgentRunEvent::SetupWarning(notice));
        }

        let mut failure = None;
        let mut prompted = false;
        while !stopped.is_cancelled() {
            let Some((first, rest)) = prompts.split_first() else {
                break;
            };
            // Steering queued before the prompt joins its first turn, so
            // several messages make one turn.
            for message in rest {
                message.steer(&session);
            }
            prompted = true;
            let (text, images) = first.prompt();
            let options = PromptOptions {
                images,
                expand: false,
                ..PromptOptions::default()
            };
            if let Err(error) = prompt_until_stopped(&session, &text, options, &stopped).await {
                failure = Some(error.to_string());
                break;
            }
            if stopped.is_cancelled() {
                break;
            }
            // A message steered in after Pi's last look would wait for the
            // task's next prompt; send it now with the queued chips.
            let (stranded, _) = session.clear_queue();
            prompts = stranded.into_iter().map(Outgoing::from_prompt).collect();
            match self.take_chips_or_close(&run_id, &session_id) {
                Some((chips, snapshot)) => {
                    events.publish(AgentRunEvent::QueueChanged(snapshot));
                    prompts.extend(chips.into_iter().map(Chip::outgoing));
                }
                None if prompts.is_empty() => break,
                None => {}
            }
        }
        session.unsubscribe(listener);

        let cancelled = stopped.is_cancelled();
        if cancelled {
            // Steering the session never took goes back to the queue.
            let (stranded, _) = session.clear_queue();
            let unsent = if prompted {
                stranded.into_iter().map(Outgoing::from_prompt).collect()
            } else {
                prompts
            };
            self.requeue(&session_id, unsent, &events);
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
    fn requeue(&self, session_id: &str, messages: Vec<Outgoing>, events: &RunEvents) {
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

/// Run one prompt. Stop aborts it, and keeps aborting until the prompt
/// returns: an abort that lands while Pi is still setting the run up would
/// otherwise be forgotten when the run starts.
async fn prompt_until_stopped(
    session: &AgentSession,
    text: &str,
    options: PromptOptions,
    stopped: &CancellationToken,
) -> Result<pi_coding_agent::PromptOutcome, pi_coding_agent::AgentSessionError> {
    let prompt = session.prompt(text, options);
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
        match event {
            AgentSessionEvent::Agent(event) => {
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
                let rows: Vec<AgentTimelineItem> = lock(live).rows(event);
                for row in rows {
                    events.publish(AgentRunEvent::TimelineItem(row));
                }
            }
            AgentSessionEvent::RetryStart { .. } => lock(&self.state).retried = true,
            AgentSessionEvent::CompactionEnd {
                result: Some(_), ..
            } => events.publish(AgentRunEvent::HistoryReplaced),
            AgentSessionEvent::CompactionEnd {
                error: Some(error), ..
            } => log::warn!("Agent compaction failed: {error}"),
            AgentSessionEvent::PersistenceError(error) => {
                log::warn!("Failed to save the Agent task: {error}");
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
