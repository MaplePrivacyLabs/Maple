//! The task that owns one external agent: its state machine, its processes
//! and every wait on them. Callers reach it only through its
//! [`ExternalAgent`] handle, by message, and read what it publishes without
//! waiting on it.
//!
//! Because one task does everything, what happens to the agent happens in
//! one order, and nothing the agent owns outlives it: dropping the task
//! drops its processes, which take their process groups with them. Slow
//! work (starting a process, applying options, the user's approval) runs
//! beside the task and reports back to it, so Stop and a shutdown are
//! never stuck behind it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use pi_agent_core::AgentToolResult;
use pi_coding_agent::Delivery;
use pi_coding_agent::extensions::{CustomMessageDraft, ExtensionContext};
use serde_json::json;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{AbortHandle, JoinError, JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use super::super::config::unix_ms;
use super::super::permissions::{PermissionGate, Refusal};
use super::super::runtime::AgentRuntime;
use super::super::timeline::{MAPLE_NOTICE_ENTRY, notice_item};
use super::super::{
    AgentLiveRowIdentity, AgentPermissionMode, AgentPersistedRowIdentity, AgentRunEvent,
    AgentServiceEvent, AgentSubagent, AgentTimelineItem, AgentTimelineLabel, ExternalAgentRef,
    emit_agent_event,
};
use super::app_server::AgentClient;
use super::catalog::{Approvals, CatalogEntry};
use super::driver::{DriverMessage, OpenedThread, TurnOutcome, TurnRequest};
use super::machine::{
    self, AgentMachine, AgentView, CallId, Effect, Input, LAUNCH_CANCELLED, NOT_RUNNING, Opened,
    ProcessId, SHUT_DOWN, StartSpec, StopScope, TimerId, TurnSpec, Wake,
};
use super::{
    ACTIVITY_KEY, AGENT_SEND_TOOL, AGENT_STATUS_TOOL, BACKGROUND_QUESTION_MESSAGE,
    BACKGROUND_RESULT_MESSAGE, ExternalAgentActivity, ExternalAgentCall, ExternalAgentHost,
    INTERRUPT_REQUEST_TIMEOUT, INTERRUPT_SETTLE_TIMEOUT, LAUNCH_TIMEOUT, NOTICE_RESULT_KEY,
    NOTICE_ROW_KEY, TURN_END_ENTRY, TurnInput, activity_result, activity_row_item, agent_command,
    answered_prompt, background_guidance, background_result_message, completion_guidance,
    external_run_id, latest_activity_label, lock, records, render_activity, subagent_row_id,
};

/// How long a process that finished its turn gets to exit by itself and
/// flush its session before it is killed.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// What an agent is, which never changes.
pub(super) struct AgentInfo {
    /// The agent's catalog entry.
    pub(super) entry: &'static CatalogEntry,
    /// Its provider's id, as tools and results name it.
    pub(super) provider: String,
    pub(super) agent_id: String,
    pub(super) session_id: String,
    /// The first line of its first prompt.
    pub(super) task: String,
    pub(super) cwd: PathBuf,
    started: Instant,
    /// Ends the agent. Derived from the runtime's lifetime, so logout and
    /// runtime stop end it too.
    pub(super) cancel: CancellationToken,
    host: ExternalAgentHost,
}

/// The tasks whose agents are working: starting a turn, in one, or
/// stopping. Maintenance that must not run under a working agent holds this
/// while it runs, and no agent starts work meanwhile.
#[derive(Default)]
pub(super) struct Admission {
    working: StdMutex<HashMap<String, usize>>,
}

impl Admission {
    /// Count one agent of the task as working until the guard goes.
    fn enter(self: &Arc<Self>, session_id: &str) -> Working {
        *lock(&self.working)
            .entry(session_id.to_string())
            .or_default() += 1;
        Working {
            admission: Arc::clone(self),
            session_id: session_id.to_string(),
        }
    }

    /// Run `action` while no agent of the task, or of any task with `None`,
    /// works, and none can start.
    pub(super) fn while_idle<T>(
        &self,
        session_id: Option<&str>,
        action: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let working = lock(&self.working);
        let busy = match session_id {
            Some(session_id) => working.get(session_id).is_some_and(|count| *count > 0),
            None => working.values().any(|count| *count > 0),
        };
        if busy {
            return Err("An external agent is working".to_string());
        }
        action()
    }
}

/// One agent counted as working.
struct Working {
    admission: Arc<Admission>,
    session_id: String,
}

impl Drop for Working {
    fn drop(&mut self) {
        let mut working = lock(&self.admission.working);
        if let Some(count) = working.get_mut(&self.session_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                working.remove(&self.session_id);
            }
        }
    }
}

/// What a caller asks of the agent's owner. Each request carries the id its
/// caller gave it, so a later Stop can say which start or turn it is about.
enum Command {
    Start(Box<StartCommand>),
    Answer {
        call: CallId,
        prompt: String,
        answers: BTreeMap<String, String>,
        background: bool,
        reply: oneshot::Sender<Result<Option<TurnReply>, String>>,
    },
    Stop {
        call: CallId,
        scope: StopScope,
        reply: oneshot::Sender<()>,
    },
    Unwait {
        call: CallId,
    },
}

struct StartCommand {
    call: CallId,
    spec: StartSpec,
    gate: PermissionGate,
    login_path: Option<String>,
    reply: oneshot::Sender<Result<TurnReply, String>>,
}

/// A turn a caller started or answered: its number, and the caller's wait
/// on it if the caller waits.
struct TurnReply {
    number: u32,
    wait: Option<oneshot::Receiver<Wake>>,
}

/// A caller's wait on a turn. One dropped before the turn wakes it leaves
/// the turn, which then reports its end to the model as a notice.
pub(super) struct Wait {
    call: CallId,
    turn: u32,
    woken: oneshot::Receiver<Wake>,
    inbox: mpsc::UnboundedSender<Command>,
    settled: bool,
}

impl Wait {
    async fn woken(&mut self) {
        let _ = (&mut self.woken).await;
        self.settled = true;
    }

    /// Keep the caller on the turn: it stops the turn rather than leave it.
    fn stay(&mut self) {
        self.settled = true;
    }
}

impl Drop for Wait {
    fn drop(&mut self) {
        if !self.settled {
            let _ = self.inbox.send(Command::Unwait { call: self.call });
        }
    }
}

/// A handle on an agent and its owner task.
pub(super) struct ExternalAgent {
    info: Arc<AgentInfo>,
    inbox: mpsc::UnboundedSender<Command>,
    next_call: AtomicU64,
    view: watch::Receiver<AgentView>,
    owner: StdMutex<Option<JoinHandle<()>>>,
}

impl Deref for ExternalAgent {
    type Target = AgentInfo;

    fn deref(&self) -> &AgentInfo {
        &self.info
    }
}

impl ExternalAgent {
    pub(super) fn new(
        entry: &'static CatalogEntry,
        agent_id: String,
        session_id: String,
        task: String,
        cwd: PathBuf,
        host: ExternalAgentHost,
        admission: Arc<Admission>,
    ) -> Self {
        let started = Instant::now();
        let machine = AgentMachine::new(
            &entry.id,
            &entry.name,
            &agent_id,
            entry.transport.one_turn_per_process(),
            started,
        );
        Self::spawn(
            entry, agent_id, session_id, task, cwd, host, admission, started, machine,
        )
    }

    /// An agent of the task opened again from its record, as after Maple
    /// restarted: its process is gone, its session is not, and the next
    /// turn resumes it.
    pub(super) fn from_record(
        entry: &'static CatalogEntry,
        session_id: String,
        record: records::AgentRecord,
        host: ExternalAgentHost,
        admission: Arc<Admission>,
    ) -> Self {
        let started = Instant::now();
        let activity = records::after_restart(record.activity);
        let agent_id = activity.agent_id.clone();
        let machine = AgentMachine::restored(
            &entry.name,
            entry.transport.one_turn_per_process(),
            started,
            activity,
        );
        Self::spawn(
            entry,
            agent_id,
            session_id,
            record.task,
            record.cwd,
            host,
            admission,
            started,
            machine,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn(
        entry: &'static CatalogEntry,
        agent_id: String,
        session_id: String,
        task: String,
        cwd: PathBuf,
        host: ExternalAgentHost,
        admission: Arc<Admission>,
        started: Instant,
        machine: AgentMachine,
    ) -> Self {
        let info = Arc::new(AgentInfo {
            entry,
            provider: entry.id.clone(),
            agent_id,
            session_id,
            task,
            cwd,
            started,
            cancel: host.lifetime.child_token(),
            host,
        });
        let (view_sender, view) = watch::channel(machine.view());
        let (inbox, commands) = mpsc::unbounded_channel();
        let (recorder, recordings) = mpsc::unbounded_channel();
        let (written, writes) = mpsc::unbounded_channel();
        let recording = tokio::spawn(record_agent(Arc::clone(&info), recordings, written));
        let owner = Owner {
            info: Arc::clone(&info),
            machine,
            inbox: commands,
            inbox_open: true,
            shutdown_seen: false,
            view: view_sender,
            admission,
            working: None,
            processes: HashMap::new(),
            current: None,
            ops: JoinSet::new(),
            launching: HashMap::new(),
            settle: None,
            gate: None,
            login_path: None,
            start_context: HashMap::new(),
            turns: HashMap::new(),
            starts: HashMap::new(),
            answers: HashMap::new(),
            stops: HashMap::new(),
            waits: HashMap::new(),
            woken: HashMap::new(),
            inputs: VecDeque::new(),
            recorder: Some(recorder),
            recording: AbortOnDrop(Some(recording)),
            recorded: 0,
            written: 0,
            writes,
            held: VecDeque::new(),
        };
        let owner = tokio::spawn(owner.run());
        Self {
            info,
            inbox,
            next_call: AtomicU64::new(1),
            view,
            owner: StdMutex::new(Some(owner)),
        }
    }

    fn next_call(&self) -> CallId {
        self.next_call.fetch_add(1, Ordering::Relaxed)
    }

    /// What the agent last published, with its age as of now.
    fn view(&self) -> AgentView {
        let mut view = self.view.borrow().clone();
        view.activity.elapsed_ms = machine::elapsed_ms(self.started, Instant::now());
        view
    }

    pub(super) fn activity(&self) -> ExternalAgentActivity {
        self.view().activity
    }

    /// Whether no turn of the agent ever started.
    pub(super) fn never_ran(&self) -> bool {
        let view = self.view.borrow();
        view.activity.turns == 0 && view.turn.is_none()
    }

    /// The agent's id for the turn in progress, once it gave one, for the
    /// fake agents' tests, which run on Unix.
    #[cfg(all(test, unix))]
    pub(super) fn turn_id(&self) -> Option<String> {
        self.view.borrow().turn.as_ref()?.turn_id.clone()
    }

    pub(super) fn last_row_revision(&self) -> u64 {
        self.view.borrow().last_row_revision
    }

    /// The agent's line in the agents panel, while a turn runs.
    pub(super) fn subagent_row(&self) -> Option<AgentSubagent> {
        let view = self.view();
        let turn = view.turn?;
        Some(AgentSubagent {
            id: subagent_row_id(&self.agent_id),
            task: self.task.clone(),
            background: turn.background,
            elapsed_ms: view.activity.elapsed_ms,
            activity: latest_activity_label(&view.activity),
            external: Some(ExternalAgentRef {
                provider: self.provider.clone(),
                agent_id: self.agent_id.clone(),
            }),
        })
    }

    /// The row of the turn in progress, as it stands.
    pub(super) fn live_row(&self) -> Option<AgentTimelineItem> {
        let view = self.view();
        let turn = view.turn?;
        let mut item = activity_row_item(&turn.row_id, &view.activity, unix_ms());
        item.revision = view.last_row_revision;
        item.live = Some(AgentLiveRowIdentity {
            run_id: external_run_id(&self.agent_id),
            occurrence: u64::from(view.activity.turns),
        });
        self.decorate_live_item(&mut item);
        Some(item)
    }

    /// The next turn's input. Answers to questions the agent ended its last
    /// turn with lead it.
    pub(super) fn next_prompt(
        &self,
        prompt: String,
        answers: &BTreeMap<String, String>,
    ) -> Result<String, String> {
        if answers.is_empty() {
            return Ok(prompt);
        }
        let view = self.view.borrow();
        if view.activity.questions.is_empty() {
            return Err(
                "This agent has no open question. Send instructions in prompt instead of answers."
                    .to_string(),
            );
        }
        answered_prompt(&view.activity.questions, &prompt, answers)
    }

    /// Run one turn: the owner starts the process if needed, applies the
    /// turn's options and sends the prompt; then wait for the end, or, in
    /// the background, return at once.
    pub(super) async fn run_turn(
        &self,
        call: &ExternalAgentCall,
        input: TurnInput,
    ) -> Result<AgentToolResult, String> {
        if let Some(runtime) = self.host.runtime.upgrade() {
            runtime.host.cancel_integrity_check();
        }
        let mode = call.permissions.mode.get();
        let spec = StartSpec {
            prompt: input.prompt,
            background: input.background,
            model: input.model,
            effort: input.effort,
            mode,
            mode_args: self.entry.mode_args(mode).to_vec(),
            row_id: call.row_id.clone(),
            gated: self.entry.approvals == Approvals::Task,
        };
        let id = self.next_call();
        let (reply, started) = oneshot::channel();
        self.inbox
            .send(Command::Start(Box::new(StartCommand {
                call: id,
                spec,
                gate: call.permissions.clone(),
                login_path: call.login_path.clone(),
                reply,
            })))
            .map_err(|_| SHUT_DOWN.to_string())?;
        let started = tokio::select! {
            biased;
            _ = call.cancel_token.cancelled() => {
                // Only this call's own start, or the turn it became: a start
                // refused as busy leaves the other turn alone.
                self.stop_scoped(StopScope::Start(id)).await;
                return Err(LAUNCH_CANCELLED.to_string());
            }
            started = started => started.unwrap_or_else(|_| Err(SHUT_DOWN.to_string()))?,
        };
        let wait = self.wait(id, started);
        if input.background {
            let activity = self.activity();
            // A short turn can be over before this returns; say so rather
            // than promising a notice that already went out.
            if activity.status == "running" {
                return Ok(AgentToolResult::text(render_activity(
                    &activity,
                    &background_guidance(),
                )));
            }
            return Ok(activity_result(&activity, &completion_guidance(&activity)));
        }
        Ok(self.wait_for_turn(call, wait).await)
    }

    /// Answer the question that holds the agent's turn, if one does. The
    /// result carries the caller's wait on the turn when it waits.
    pub(super) async fn answer_question(
        &self,
        prompt: &str,
        answers: &BTreeMap<String, String>,
        background: bool,
    ) -> Result<Option<Option<Wait>>, String> {
        let id = self.next_call();
        let (reply, answered) = oneshot::channel();
        self.inbox
            .send(Command::Answer {
                call: id,
                prompt: prompt.to_string(),
                answers: answers.clone(),
                background,
                reply,
            })
            .map_err(|_| SHUT_DOWN.to_string())?;
        let answered = answered
            .await
            .unwrap_or_else(|_| Err(SHUT_DOWN.to_string()))?;
        Ok(answered.map(|turn| self.wait(id, turn)))
    }

    fn wait(&self, call: CallId, turn: TurnReply) -> Option<Wait> {
        turn.wait.map(|woken| Wait {
            call,
            turn: turn.number,
            woken,
            inbox: self.inbox.clone(),
            settled: false,
        })
    }

    /// Wait, for a blocking call, until the turn ends, the agent asks a
    /// question, or the call's run is cancelled; then say where the agent
    /// stands.
    pub(super) async fn wait_for_turn(
        &self,
        call: &ExternalAgentCall,
        wait: Option<Wait>,
    ) -> AgentToolResult {
        if let Some(mut wait) = wait {
            tokio::select! {
                biased;
                _ = call.cancel_token.cancelled() => {
                    // The Maple turn ended; the agent keeps its thread but
                    // stops working on this prompt, and its process goes so
                    // a sandboxed command cannot outlive the stop.
                    wait.stay();
                    self.stop_scoped(StopScope::Turn(wait.turn)).await;
                }
                () = wait.woken() => {}
            }
        }
        let activity = self.activity();
        activity_result(&activity, &completion_guidance(&activity))
    }

    /// Stop the agent's turn and reclaim its process. The thread stays, and
    /// the next send resumes it in a fresh process.
    pub(super) async fn stop(&self) {
        self.stop_scoped(StopScope::Any).await;
    }

    /// Stop what `scope` names, if the agent is still on it.
    async fn stop_scoped(&self, scope: StopScope) {
        let (reply, stopped) = oneshot::channel();
        let call = self.next_call();
        if self
            .inbox
            .send(Command::Stop { call, scope, reply })
            .is_ok()
        {
            let _ = stopped.await;
        }
    }

    /// The owner task, for a caller that bounds how long it waits for it.
    pub(super) fn take_owner(&self) -> Option<JoinHandle<()>> {
        lock(&self.owner).take()
    }

    /// End the agent: its turn is stopped as Stop does, letting the agent
    /// end its own commands (Grok runs each in a process group of its own),
    /// then its process goes.
    pub(super) async fn shutdown(&self) {
        self.cancel.cancel();
        match self.take_owner() {
            Some(owner) => {
                let _ = owner.await;
            }
            None => {
                let mut view = self.view.clone();
                let _ = view.wait_for(|view| view.gone).await;
            }
        }
    }
}

impl AgentInfo {
    fn emit_subagent_started(&self, background: bool) {
        self.emit_run_event(AgentRunEvent::SubagentStarted {
            id: subagent_row_id(&self.agent_id),
            task: self.task.clone(),
            background,
            external: Some(ExternalAgentRef {
                provider: self.provider.clone(),
                agent_id: self.agent_id.clone(),
            }),
        });
    }

    fn emit_subagent_activity(&self, tool: String) {
        self.emit_run_event(AgentRunEvent::SubagentActivity {
            id: subagent_row_id(&self.agent_id),
            tool,
        });
    }

    fn emit_subagent_finished(&self) {
        self.emit_run_event(AgentRunEvent::SubagentFinished {
            id: subagent_row_id(&self.agent_id),
        });
    }

    fn emit_run_event(&self, event: AgentRunEvent) {
        emit_agent_event(
            &self.host.events,
            AgentServiceEvent::Run {
                session_id: self.session_id.clone(),
                run_id: external_run_id(&self.agent_id),
                event,
            },
        );
    }

    fn emit_item(&self, mut item: AgentTimelineItem) {
        if item.revision == 0 {
            item.revision = super::super::runs::next_timeline_revision();
        }
        self.decorate_live_item(&mut item);
        emit_agent_event(
            &self.host.events,
            AgentServiceEvent::TimelineItem {
                session_id: self.session_id.clone(),
                run_id: None,
                item,
            },
        );
    }

    fn decorate_live_item(&self, item: &mut AgentTimelineItem) {
        if item.live.is_none() {
            return;
        }
        let Some(runtime) = self.host.runtime.upgrade() else {
            return;
        };
        match runtime.store.store().external_repaint_target(
            runtime.store.account(),
            &self.session_id,
            &item.id,
        ) {
            Ok(Some(row)) => {
                item.persisted = Some(AgentPersistedRowIdentity {
                    task_id: self.session_id.clone(),
                    rows_epoch: row.epoch,
                    row_seq: row.row_seq,
                    update_seq: row.update_seq,
                })
            }
            Ok(None) => {}
            Err(error) => {
                log::warn!("Could not resolve the external agent's committed row: {error}")
            }
        }
    }

    /// Leave a notice in the task's transcript.
    fn notice(
        &self,
        context: &ExtensionContext,
        kind: &str,
        text: &str,
        label: AgentTimelineLabel,
    ) {
        let id = format!("external-{}-{kind}-{}", self.agent_id, unix_ms());
        context.append_entry(
            MAPLE_NOTICE_ENTRY,
            Some(json!({"id": id, "text": text, "label": label})),
        );
        let mut item = notice_item(id, "Agent notice", text, pi_ai::now_ms());
        item.label = Some(label);
        self.emit_item(item);
    }

    /// Hand the model a message it did not ask for, behind the task's run
    /// or in a run of its own. A task that cannot be resumed reads it with
    /// its next prompt.
    async fn deliver(
        &self,
        runtime: &Arc<AgentRuntime>,
        context: &ExtensionContext,
        custom_type: &'static str,
        for_model: String,
    ) {
        let Err(error) = runtime
            .deliver_hidden(&self.session_id, custom_type, for_model.clone())
            .await
        else {
            return;
        };
        log::warn!(
            "External agent {} could not reach its task: {error}",
            self.agent_id
        );
        context.send_message(
            CustomMessageDraft {
                custom_type: custom_type.to_string(),
                content: vec![pi_ai::Content::text(for_model)],
                display: false,
                details: None,
            },
            Delivery::Append,
        );
        self.notice(
            context,
            "undelivered",
            "The task could not be resumed. Send a message to read the agent's result.",
            super::super::timeline::label("external-undelivered"),
        );
    }
}

/// A background turn ended: leave the result in the transcript and tell the
/// model, behind the task's run or in a run of its own.
async fn report_turn_end(
    info: Arc<AgentInfo>,
    activity: ExternalAgentActivity,
    row_id: String,
    outcome: TurnOutcome,
) {
    // An agent shut down went with its task or the runtime: nobody is left
    // to tell.
    if info.cancel.is_cancelled() {
        return;
    }
    let Some(runtime) = info.host.runtime.upgrade() else {
        return;
    };
    let result_text = render_activity(&activity, &completion_guidance(&activity));
    let for_model = background_result_message(
        "A background agent has finished. Its result is included below; continue the task using this result without fetching it again.",
        &format!("external agent {} ({})", info.agent_id, info.provider),
        outcome.status(),
        &result_text,
        &format!(
            "Use {AGENT_STATUS_TOOL}(provider: \"{}\", agent_id: \"{}\") only if you need to inspect its current state again.",
            info.provider, info.agent_id
        ),
    );
    // The task's session takes the notices; a task closed meanwhile is
    // opened again, one deleted is gone with its agents.
    let session = match runtime.side_question_session(&info.session_id).await {
        Ok(session) => session,
        Err(error) => {
            log::warn!(
                "External agent {} could not report to its task: {error}",
                info.agent_id
            );
            return;
        }
    };
    // The notices first, so a run the result starts follows them.
    let context = session.extension_context();
    context.append_entry(
        TURN_END_ENTRY,
        Some(json!({
            ACTIVITY_KEY: activity,
            NOTICE_ROW_KEY: row_id,
            NOTICE_RESULT_KEY: result_text,
        })),
    );
    let mut row = activity_row_item(&row_id, &activity, unix_ms());
    row.live = Some(AgentLiveRowIdentity {
        run_id: external_run_id(&info.agent_id),
        occurrence: u64::from(activity.turns),
    });
    row.output = Some(json!({
        "text": result_text,
        "structuredContent": { ACTIVITY_KEY: activity },
        "content": [],
    }));
    info.emit_item(row);
    info.notice(
        &context,
        "end",
        &format!(
            "External agent {} ({}) {}.",
            info.agent_id,
            info.entry.name,
            outcome.verb()
        ),
        AgentTimelineLabel {
            kind: format!("external-{}", outcome.status()),
            name: Some(info.agent_id.clone()),
            detail: Some(info.entry.name.clone()),
        },
    );
    info.deliver(&runtime, &context, BACKGROUND_RESULT_MESSAGE, for_model)
        .await;
}

/// A background agent asked a question: hand it to the model that called
/// the agent, which answers with its next send.
async fn report_question(info: Arc<AgentInfo>, activity: ExternalAgentActivity) {
    if info.cancel.is_cancelled() || activity.questions.is_empty() {
        return;
    }
    let Some(runtime) = info.host.runtime.upgrade() else {
        return;
    };
    let for_model = background_result_message(
        "A background agent asked a question and is waiting for your answer. Its questions are included below.",
        &format!("external agent {} ({})", info.agent_id, info.provider),
        &activity.status,
        &render_activity(&activity, &completion_guidance(&activity)),
        &format!(
            "Answer with {AGENT_SEND_TOOL}(provider: \"{}\", agent_id: \"{}\").",
            info.provider, info.agent_id
        ),
    );
    let session = match runtime.side_question_session(&info.session_id).await {
        Ok(session) => session,
        Err(error) => {
            log::warn!(
                "External agent {} could not ask its task: {error}",
                info.agent_id
            );
            return;
        }
    };
    let context = session.extension_context();
    info.deliver(&runtime, &context, BACKGROUND_QUESTION_MESSAGE, for_model)
        .await;
}

/// Write the agent's records into its task's session, in the order they
/// come, and say how far it got. Only the latest counts, so one a newer one
/// replaces is skipped.
async fn record_agent(
    info: Arc<AgentInfo>,
    mut recordings: mpsc::UnboundedReceiver<(u64, ExternalAgentActivity)>,
    written: mpsc::UnboundedSender<u64>,
) {
    while let Some((mut count, mut activity)) = recordings.recv().await {
        while let Ok((newer_count, newer)) = recordings.try_recv() {
            count = newer_count;
            activity = newer;
        }
        write_record(&info, activity).await;
        let _ = written.send(count);
    }
}

async fn write_record(info: &AgentInfo, activity: ExternalAgentActivity) {
    let Some(runtime) = info.host.runtime.upgrade() else {
        return;
    };
    let record = records::AgentRecord {
        task: info.task.clone(),
        cwd: info.cwd.clone(),
        activity,
    };
    let Ok(data) = serde_json::to_value(&record) else {
        return;
    };
    // An agent going away writes only into a task still open: one being
    // deleted was closed first and must not be opened again.
    let session = if info.cancel.is_cancelled() {
        runtime
            .loaded_session(&info.session_id)
            .await
            .ok_or_else(|| "the task is closed".to_string())
    } else {
        runtime.side_question_session(&info.session_id).await
    };
    match session {
        Ok(session) => session
            .extension_context()
            .append_entry(records::AGENT_RECORD_ENTRY, Some(data)),
        Err(error) => log::warn!(
            "External agent {} could not record itself in its task: {error}",
            info.agent_id
        ),
    }
}

/// An agent's process, in its own process group, which goes with it.
pub(super) struct ContainedChild {
    pub(super) child: tokio::process::Child,
}

impl ContainedChild {
    /// Signal the process and everything it started to end, now.
    fn signal(&mut self) {
        if let Some(pid) = self.child.id() {
            pi_coding_agent::tools::kill_process_tree(pid);
        }
        let _ = self.child.start_kill();
    }

    /// End the process and everything it started, and reap it.
    async fn kill_and_wait(&mut self) {
        self.signal();
        let _ = self.child.wait().await;
    }
}

impl Drop for ContainedChild {
    /// A process dropped while it runs takes everything it started with it.
    /// One that exited is left alone: its id may name another process by now.
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None))
            && let Some(pid) = self.child.id()
        {
            pi_coding_agent::tools::kill_process_tree(pid);
        }
    }
}

/// A running agent process and the transport that speaks to it.
struct AgentProcess {
    child: ContainedChild,
    client: Arc<AgentClient>,
    reader: JoinHandle<()>,
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

struct Launched {
    process: AgentProcess,
    messages: mpsc::Receiver<DriverMessage>,
    opened: OpenedThread,
}

/// What work beside the owner reports back.
enum Done {
    Launched {
        process: ProcessId,
        result: Result<Launched, String>,
    },
    Input(Input),
    Nothing,
}

/// What the owner waits on next.
enum Next {
    Shutdown,
    /// The record writer wrote records up to this count.
    Written(Option<u64>),
    Done(Result<Done, JoinError>),
    Settle,
    Message(Option<DriverMessage>),
    Command(Option<Command>),
    /// Nothing is left to wait on, which the machine never allows.
    Stalled,
}

struct Owner {
    info: Arc<AgentInfo>,
    machine: AgentMachine,
    inbox: mpsc::UnboundedReceiver<Command>,
    inbox_open: bool,
    shutdown_seen: bool,
    view: watch::Sender<AgentView>,
    admission: Arc<Admission>,
    working: Option<Working>,
    processes: HashMap<ProcessId, AgentProcess>,
    /// The current process's messages.
    current: Option<(ProcessId, mpsc::Receiver<DriverMessage>)>,
    /// Work beside the owner. Dropping the owner aborts it.
    ops: JoinSet<Done>,
    launching: HashMap<ProcessId, AbortHandle>,
    settle: Option<(TimerId, tokio::time::Instant)>,
    /// The task's permission gate, from the latest turn's call.
    gate: Option<PermissionGate>,
    /// The latest turn's login PATH, which processes are found and run on.
    login_path: Option<String>,
    /// A start's gate and PATH, until the machine takes the start up.
    start_context: HashMap<CallId, (PermissionGate, Option<String>)>,
    /// Fires when its turn ends, so an approval still waiting stops.
    turns: HashMap<u32, CancellationToken>,
    starts: HashMap<CallId, oneshot::Sender<Result<TurnReply, String>>>,
    answers: HashMap<CallId, oneshot::Sender<Result<Option<TurnReply>, String>>>,
    stops: HashMap<CallId, oneshot::Sender<()>>,
    waits: HashMap<CallId, oneshot::Sender<Wake>>,
    /// Waits not handed to their caller yet.
    woken: HashMap<CallId, oneshot::Receiver<Wake>>,
    /// Inputs the owner raised itself, taken before anything new.
    inputs: VecDeque<Input>,
    recorder: Option<mpsc::UnboundedSender<(u64, ExternalAgentActivity)>>,
    /// The record writer, aborted with the owner.
    recording: AbortOnDrop,
    /// How many records were taken, and how many of them are written.
    recorded: u64,
    written: u64,
    writes: mpsc::UnboundedReceiver<u64>,
    /// What callers hear, held until the records taken before it are
    /// written: each batch with the count it waits for.
    held: VecDeque<(u64, Vec<Effect>)>,
}

/// A task aborted when this is dropped.
struct AbortOnDrop(Option<JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

impl Owner {
    async fn run(mut self) {
        loop {
            let input = match self.inputs.pop_front() {
                Some(input) => input,
                None => match self.next().await {
                    Ok(Some(input)) => input,
                    Ok(None) => continue,
                    Err(()) => {
                        log::warn!(
                            "External agent {} had nothing left to wait on; ending it",
                            self.info.agent_id
                        );
                        break;
                    }
                },
            };
            if self.step(input) {
                break;
            }
        }
        // The records taken before the agent went are written; nothing
        // after its shutdown is added. Then callers still held hear back.
        self.recorder = None;
        if let Some(recording) = self.recording.0.take() {
            let _ = recording.await;
        }
        self.written = self.recorded;
        self.release();
    }

    /// What happens next; an error when nothing can.
    async fn next(&mut self) -> Result<Option<Input>, ()> {
        let next = {
            let Self {
                info,
                inbox,
                inbox_open,
                shutdown_seen,
                ops,
                settle,
                current,
                writes,
                recorded,
                written,
                ..
            } = self;
            let unwritten = *written < *recorded;
            let deadline = settle.map(|(_, deadline)| deadline);
            tokio::select! {
                biased;
                () = info.cancel.cancelled(), if !*shutdown_seen => Next::Shutdown,
                Some(done) = ops.join_next(), if !ops.is_empty() => Next::Done(done),
                () = sleep_until(deadline), if deadline.is_some() => Next::Settle,
                write = writes.recv(), if unwritten => Next::Written(write),
                message = recv(current), if current.is_some() => Next::Message(message),
                command = inbox.recv(), if *inbox_open => Next::Command(command),
                else => Next::Stalled,
            }
        };
        Ok(match next {
            Next::Stalled => return Err(()),
            Next::Written(write) => {
                // A writer that is gone wrote all it will.
                self.written = write.unwrap_or(self.recorded).max(self.written);
                self.release();
                self.update_working();
                None
            }
            Next::Shutdown => {
                self.shutdown_seen = true;
                Some(Input::Shutdown)
            }
            Next::Done(Ok(done)) => self.done(done),
            Next::Done(Err(error)) => {
                if error.is_panic() {
                    log::warn!(
                        "Work for external agent {} panicked: {error}",
                        self.info.agent_id
                    );
                }
                None
            }
            Next::Settle => self
                .settle
                .take()
                .map(|(timer, _)| Input::SettleExpired { timer }),
            Next::Message(Some(DriverMessage::Event(event))) => Some(Input::Event(event)),
            Next::Message(Some(DriverMessage::Request { id, request })) => {
                log::info!(
                    "External agent {} asks: {}",
                    self.info.agent_id,
                    request.kind()
                );
                Some(Input::Request { id, request })
            }
            Next::Message(None) => {
                self.current = None;
                Some(Input::Closed)
            }
            Next::Command(Some(command)) => self.command(command),
            // Every handle is gone: so is the agent.
            Next::Command(None) => {
                self.inbox_open = false;
                (!std::mem::replace(&mut self.shutdown_seen, true)).then_some(Input::Shutdown)
            }
        })
    }

    fn command(&mut self, command: Command) -> Option<Input> {
        match command {
            Command::Unwait { call } => {
                self.waits.remove(&call);
                Some(Input::Unwait { call })
            }
            Command::Start(start) => {
                let StartCommand {
                    call,
                    spec,
                    gate,
                    login_path,
                    reply,
                } = *start;
                if !spec.background {
                    self.register_wait(call);
                }
                self.starts.insert(call, reply);
                self.start_context.insert(call, (gate, login_path));
                // A process that exited goes before the turn could use it.
                self.notice_exit();
                self.inputs.push_back(Input::Start { call, spec });
                None
            }
            Command::Answer {
                call,
                prompt,
                answers,
                background,
                reply,
            } => {
                if !background {
                    self.register_wait(call);
                }
                self.answers.insert(call, reply);
                Some(Input::Answer {
                    call,
                    prompt,
                    answers,
                    background,
                })
            }
            Command::Stop { call, scope, reply } => {
                self.stops.insert(call, reply);
                Some(Input::Stop { call, scope })
            }
        }
    }

    fn register_wait(&mut self, call: CallId) {
        let (wake, woken) = oneshot::channel();
        self.waits.insert(call, wake);
        self.woken.insert(call, woken);
    }

    /// The current process's transport closed: take in what it sent before
    /// it went, then that it went.
    fn notice_exit(&mut self) {
        let Some((process, messages)) = self.current.as_mut() else {
            return;
        };
        let exited = self
            .processes
            .get(process)
            .is_none_or(|agent| agent.client.closed().is_cancelled());
        if !exited {
            return;
        }
        while let Ok(message) = messages.try_recv() {
            self.inputs.push_back(match message {
                DriverMessage::Event(event) => Input::Event(event),
                DriverMessage::Request { id, request } => Input::Request { id, request },
            });
        }
        self.current = None;
        self.inputs.push_back(Input::Closed);
    }

    fn done(&mut self, done: Done) -> Option<Input> {
        match done {
            Done::Launched { process, result } => {
                // A launch given up on that finished anyway: what it
                // started goes with it, never in place of a newer process.
                self.launching.remove(&process)?;
                let result = result.map(|launched| {
                    let opened = Opened {
                        thread_id: launched.opened.thread_id.clone(),
                        confirmed: launched.opened.confirmed,
                    };
                    self.processes.insert(process, launched.process);
                    self.current = Some((process, launched.messages));
                    opened
                });
                Some(Input::Launched { process, result })
            }
            Done::Input(input) => Some(input),
            Done::Nothing => None,
        }
    }

    /// Hand `input` to the machine and carry out what it says. True once
    /// the agent is gone.
    fn step(&mut self, input: Input) -> bool {
        let start = match &input {
            Input::Start { call, .. } => Some(*call),
            _ => None,
        };
        let effects = self.machine.handle(input, Instant::now());
        if let Some(call) = start
            && let Some((gate, login_path)) = self.start_context.remove(&call)
            && self.machine.is_starting(call)
        {
            self.gate = Some(gate);
            self.login_path = login_path;
        }
        // Counted as working before any work starts, so maintenance that
        // holds the admission keeps it from starting.
        self.update_working();
        let mut replies = Vec::new();
        let mut exit = false;
        for effect in effects {
            match effect {
                Effect::Started { .. }
                | Effect::Answered { .. }
                | Effect::Stopped { .. }
                | Effect::Wake { .. }
                | Effect::ReportEnd { .. }
                | Effect::ReportQuestion { .. } => replies.push(effect),
                Effect::Exit => exit = true,
                effect => self.apply(effect),
            }
        }
        // A settle timer the machine no longer waits on is dropped.
        if self
            .settle
            .is_some_and(|(timer, _)| Some(timer) != self.machine.armed_timer())
        {
            self.settle = None;
        }
        // Published before any caller hears back, so what it reads next is
        // at least as new as what it heard.
        self.view.send_replace(self.machine.view());
        // Callers and the model hear back once the records taken so far are
        // written, so a record never trails what they act on.
        if !replies.is_empty() {
            self.held.push_back((self.recorded, replies));
            self.release();
        }
        self.update_working();
        exit
    }

    /// Working, for maintenance, while the machine works or its records
    /// and what waits on them are not through yet.
    fn update_working(&mut self) {
        let working =
            self.machine.working() || self.written < self.recorded || !self.held.is_empty();
        match (working, self.working.is_some()) {
            (true, false) => self.working = Some(self.admission.enter(&self.info.session_id)),
            (false, true) => self.working = None,
            _ => {}
        }
    }

    /// Deliver what is held, in order, as far as the records allow.
    fn release(&mut self) {
        while self
            .held
            .front()
            .is_some_and(|(needs, _)| *needs <= self.written)
        {
            let Some((_, replies)) = self.held.pop_front() else {
                break;
            };
            for reply in replies {
                self.reply(reply);
            }
        }
    }

    fn client(&self, process: ProcessId) -> Option<Arc<AgentClient>> {
        self.processes
            .get(&process)
            .map(|agent| Arc::clone(&agent.client))
    }

    /// Stop reading a process and take it out of the owner's hands.
    fn forget(&mut self, process: ProcessId) -> Option<AgentProcess> {
        if self
            .current
            .as_ref()
            .is_some_and(|(current, _)| *current == process)
        {
            self.current = None;
        }
        self.processes.remove(&process)
    }

    fn apply(&mut self, effect: Effect) {
        match effect {
            Effect::Launch {
                process,
                replace,
                thread_id,
                model,
                effort,
                mode,
            } => {
                let old = replace.and_then(|old| self.forget(old));
                let launch = launch(
                    Arc::clone(&self.info),
                    self.login_path.clone(),
                    old,
                    thread_id,
                    model,
                    effort,
                    mode,
                );
                let handle = self.ops.spawn(async move {
                    Done::Launched {
                        process,
                        result: launch.await,
                    }
                });
                self.launching.insert(process, handle);
            }
            Effect::Abort { process } => {
                if let Some(handle) = self.launching.remove(&process) {
                    handle.abort();
                }
            }
            Effect::Configure { process, turn } => {
                let Some(client) = self.client(process) else {
                    return self.inputs.push_back(Input::Configured {
                        process,
                        result: Err(NOT_RUNNING.to_string()),
                    });
                };
                let timeout = self.info.entry.startup_timeout().unwrap_or(LAUNCH_TIMEOUT);
                let cwd = self.info.cwd.clone();
                self.ops.spawn(async move {
                    let request = turn_request(&turn, &cwd);
                    let result = tokio::time::timeout(timeout, client.configure_turn(&request))
                        .await
                        .unwrap_or_else(|_| {
                            Err("The external agent did not apply its options in time.".to_string())
                        });
                    Done::Input(Input::Configured { process, result })
                });
            }
            Effect::Prompt {
                process,
                turn,
                spec,
            } => {
                let Some(client) = self.client(process) else {
                    return self.inputs.push_back(Input::Prompted {
                        turn,
                        result: Err(NOT_RUNNING.to_string()),
                    });
                };
                let cwd = self.info.cwd.clone();
                self.ops.spawn(async move {
                    let result = client.start_turn(&turn_request(&spec, &cwd)).await;
                    Done::Input(Input::Prompted { turn, result })
                });
            }
            Effect::Interrupt {
                process,
                thread_id,
                turn_id,
            } => {
                let Some(client) = self.client(process) else {
                    return self.inputs.push_back(Input::Interrupted { process });
                };
                self.ops.spawn(async move {
                    // Claude and ACP agents stop without the turn's id, which
                    // can still be on its way when Stop comes.
                    let request = client.interrupt(&thread_id, turn_id.as_deref());
                    if let Ok(Err(error)) =
                        tokio::time::timeout(INTERRUPT_REQUEST_TIMEOUT, request).await
                    {
                        log::debug!("External agent interrupt was refused: {error}");
                    }
                    Done::Input(Input::Interrupted { process })
                });
            }
            Effect::Settle { timer } => {
                self.settle = Some((
                    timer,
                    tokio::time::Instant::now() + INTERRUPT_SETTLE_TIMEOUT,
                ));
            }
            Effect::Kill { process, flush } => {
                let Some(mut agent) = self.forget(process) else {
                    return self.inputs.push_back(Input::Killed { process });
                };
                // Signalled now, so nothing outlives an abort of the wait
                // that follows. A process given time to flush is not yet.
                if !flush {
                    agent.child.signal();
                }
                self.ops.spawn(async move {
                    if flush {
                        let _ = tokio::time::timeout(FLUSH_TIMEOUT, agent.child.child.wait()).await;
                    }
                    agent.child.kill_and_wait().await;
                    drop(agent);
                    Done::Input(Input::Killed { process })
                });
            }
            Effect::Reply { process, id, reply } => {
                let Some(client) = self.client(process) else {
                    return;
                };
                let agent_id = self.info.agent_id.clone();
                let kind = reply.label();
                self.ops.spawn(async move {
                    log::info!("External agent {agent_id} answered: {kind}");
                    if let Err(error) = client.reply(id, reply).await {
                        log::warn!("Failed to answer an external agent request: {error}");
                    }
                    Done::Nothing
                });
            }
            Effect::Refuse {
                process,
                id,
                message,
            } => {
                let Some(client) = self.client(process) else {
                    return;
                };
                self.ops.spawn(async move {
                    client.refuse(id, &message).await;
                    Done::Nothing
                });
            }
            Effect::Judge {
                process,
                turn,
                id,
                item_id,
                action,
                tool_name,
                input,
            } => {
                let ended = self
                    .turns
                    .entry(turn)
                    .or_insert_with(|| self.info.cancel.child_token())
                    .clone();
                let stop = self.info.cancel.child_token();
                let cwd = self.info.cwd.display().to_string();
                let gate = self.gate.clone();
                self.ops.spawn(async move {
                    let verdict = match gate {
                        Some(gate) => tokio::select! {
                            biased;
                            () = ended.cancelled() => Err(Refusal::Stopped),
                            verdict = gate.check_request(&tool_name, None, &input, &cwd, &stop) => verdict,
                        },
                        None => Err(Refusal::Stopped),
                    };
                    Done::Input(Input::Judged {
                        process,
                        turn,
                        id,
                        item_id,
                        action,
                        verdict,
                    })
                });
            }
            Effect::TurnEnded { turn } => {
                if let Some(ended) = self.turns.remove(&turn) {
                    ended.cancel();
                }
            }
            Effect::Row(item) => self.info.emit_item(item),
            Effect::SubagentStarted { background } => self.info.emit_subagent_started(background),
            Effect::SubagentActivity(label) => self.info.emit_subagent_activity(label),
            Effect::SubagentFinished => self.info.emit_subagent_finished(),
            Effect::Record(activity) => {
                if let Some(recorder) = &self.recorder
                    && recorder.send((self.recorded + 1, activity)).is_ok()
                {
                    self.recorded += 1;
                }
            }
            Effect::Started { .. }
            | Effect::Answered { .. }
            | Effect::Stopped { .. }
            | Effect::Wake { .. }
            | Effect::ReportEnd { .. }
            | Effect::ReportQuestion { .. }
            | Effect::Exit => {}
        }
    }

    fn reply(&mut self, effect: Effect) {
        match effect {
            Effect::Started { call, result } => {
                let wait = self.woken.remove(&call);
                let joined = result.is_ok();
                let reply = result.map(|number| TurnReply { number, wait });
                let heard = self
                    .starts
                    .remove(&call)
                    .is_some_and(|sender| sender.send(reply).is_ok());
                self.leave_unless(heard && joined, call);
            }
            Effect::Answered { call, result } => {
                let wait = self.woken.remove(&call);
                let joined = matches!(result, Ok(Some(_)));
                let reply = result.map(|turn| turn.map(|number| TurnReply { number, wait }));
                let heard = self
                    .answers
                    .remove(&call)
                    .is_some_and(|sender| sender.send(reply).is_ok());
                self.leave_unless(heard && joined, call);
            }
            Effect::Stopped { calls } => {
                for call in calls {
                    if let Some(sender) = self.stops.remove(&call) {
                        let _ = sender.send(());
                    }
                }
            }
            Effect::Wake {
                calls,
                why,
                unheard,
            } => {
                let mut heard = false;
                for call in calls {
                    if let Some(sender) = self.waits.remove(&call) {
                        heard |= sender.send(why).is_ok();
                    }
                }
                // Every caller went away: the model hears it as a notice.
                if !heard && let Some(report) = unheard {
                    self.reply(*report);
                }
            }
            Effect::ReportEnd {
                activity,
                row_id,
                outcome,
            } => {
                tokio::spawn(report_turn_end(
                    Arc::clone(&self.info),
                    activity,
                    row_id,
                    outcome,
                ));
            }
            Effect::ReportQuestion { activity } => {
                tokio::spawn(report_question(Arc::clone(&self.info), activity));
            }
            _ => {}
        }
    }

    /// A caller that went away before hearing it joined the turn leaves it,
    /// so the turn tells the model instead.
    fn leave_unless(&mut self, stays: bool, call: CallId) {
        if !stays && self.waits.remove(&call).is_some() {
            self.inputs.push_back(Input::Unwait { call });
        }
    }
}

/// Start a process for the agent, after ending the one it replaces, and
/// open its thread: within the entry's start-up time.
async fn launch(
    info: Arc<AgentInfo>,
    login_path: Option<String>,
    old: Option<AgentProcess>,
    thread_id: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    mode: AgentPermissionMode,
) -> Result<Launched, String> {
    let timeout = info.entry.startup_timeout().unwrap_or(LAUNCH_TIMEOUT);
    let launch = async {
        if let Some(mut old) = old {
            old.child.kill_and_wait().await;
        }
        let entry = info.entry;
        let executable =
            super::super::integrations::find_executable(&entry.executable, login_path.as_deref())
                .ok_or_else(|| entry.text.launch_missing.clone())?;
        let launch = entry.launch(
            thread_id.as_deref(),
            model.as_deref(),
            effort.as_deref(),
            mode,
        );
        let mut command = agent_command(
            &executable,
            &launch.args,
            &info.cwd,
            login_path.as_deref(),
            &info.session_id,
        )?;
        entry.apply_environment(&mut command);
        let mut child = ContainedChild {
            child: command
                .spawn()
                .map_err(|error| format!("Failed to start {}: {error}", entry.name))?,
        };
        let stdin = child
            .child
            .stdin
            .take()
            .ok_or_else(|| "Failed to open the agent stdin".to_string())?;
        let stdout = child
            .child
            .stdout
            .take()
            .ok_or_else(|| "Failed to open the agent stdout".to_string())?;
        let (client, messages, reader) = entry.connect(stdin, stdout, launch);
        let process = AgentProcess {
            child,
            client,
            reader,
        };
        process.client.initialize().await?;
        let opened = process
            .client
            .open_thread(thread_id.as_deref(), &info.cwd, model.as_deref())
            .await?;
        Ok(Launched {
            process,
            messages,
            opened,
        })
    };
    tokio::time::timeout(timeout, launch)
        .await
        .unwrap_or_else(|_| Err("The external agent did not initialize in time.".to_string()))
}

fn turn_request<'a>(spec: &'a TurnSpec, cwd: &'a std::path::Path) -> TurnRequest<'a> {
    TurnRequest {
        thread_id: &spec.thread_id,
        prompt: &spec.prompt,
        cwd,
        model: spec.model.as_deref(),
        effort: spec.effort.as_deref(),
        mode: spec.mode,
    }
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn recv(
    current: &mut Option<(ProcessId, mpsc::Receiver<DriverMessage>)>,
) -> Option<DriverMessage> {
    match current {
        Some((_, messages)) => messages.recv().await,
        None => std::future::pending().await,
    }
}
