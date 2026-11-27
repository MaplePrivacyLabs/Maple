//! One external agent's lifecycle as a plain state machine. It is told what
//! happened (a caller's request, a message from the agent, the end of
//! something its owner started) and answers with what to do about it, as
//! [`Effect`]s for the agent's owner task to carry out. Nothing here waits,
//! locks, or touches a process, so every transition is tested directly.
//!
//! The agent is in one [`Phase`] at a time, and that phase says what it may
//! do next: a turn starts only from `Idle`, a request is judged only while a
//! turn runs, and every way out of a turn passes through `finish_turn`.
//! Processes, callers and timers are named by numbers, so news about one
//! that is gone is recognised and dropped.

use std::collections::{BTreeMap, VecDeque};
use std::mem;
use std::time::Instant;

use serde_json::Value;

use super::super::config::unix_ms;
use super::super::permissions::{Refusal, card_action};
use super::super::runs::next_timeline_revision;
use super::super::{
    AgentLiveRowIdentity, AgentPermissionDecision, AgentPermissionMode, AgentQuestion,
    AgentTimelineItem,
};
use super::driver::{self, AgentEvent, AgentItem, AgentReply, AgentRequest, TurnOutcome};
use super::{
    AGENT_STATUS_TOOL, ActivityCommand, ActivityFileChange, ActivityQuestion, ActivityTodo,
    ExternalAgentActivity, FileChangeEntry, LIVE_ROW_INTERVAL, MAX_ACTIVITY_COMMAND_CHARS,
    MAX_ACTIVITY_COMMANDS, MAX_ACTIVITY_FILE_CHANGES, MAX_ACTIVITY_TODOS, MAX_RESULT_TEXT_CHARS,
    TodoEntry, activity_row_item, bounded_timeline_text, caller_answers, external_run_id,
    latest_activity_label, question_text, tail_bounded,
};

/// One of the agent's processes. A new process gets a new number, so news
/// from one that is gone is told apart.
pub(super) type ProcessId = u64;
/// One caller's request, and its wait on the turn it starts or answers.
pub(super) type CallId = u64;
/// One arming of the settle timer; a later arming replaces it.
pub(super) type TimerId = u64;

pub(super) const SHUT_DOWN: &str = "This external agent has been shut down.";
pub(super) const LAUNCH_CANCELLED: &str = "The external agent launch was cancelled.";

/// What a caller asks a turn for.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct StartSpec {
    pub(super) prompt: String,
    pub(super) background: bool,
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
    /// The task's permission mode when the turn starts.
    pub(super) mode: AgentPermissionMode,
    /// The arguments that put the agent in its own mode for `mode`. A
    /// process launched with others is replaced.
    pub(super) mode_args: Vec<String>,
    /// The caller's transcript row, which the turn's progress joins.
    pub(super) row_id: Option<String>,
    /// Whether the agent's requests go to the task's permission gate, as its
    /// catalog entry's `approvals` say. Otherwise they are accepted once and
    /// the agent's own settings govern.
    pub(super) gated: bool,
}

/// What a turn's options and prompt go to the agent with.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct TurnSpec {
    pub(super) thread_id: String,
    pub(super) prompt: String,
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
    pub(super) mode: AgentPermissionMode,
}

/// The thread a launched process opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Opened {
    pub(super) thread_id: String,
    /// See [`driver::OpenedThread::confirmed`].
    pub(super) confirmed: bool,
}

/// Why a caller waiting on a turn stops waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Wake {
    /// The turn ended.
    Ended,
    /// The agent asked a question, which the caller answers with its next
    /// call. The turn goes on.
    Asked,
}

/// Something that happened to the agent.
#[derive(Debug)]
pub(super) enum Input {
    /// A caller asks for a turn.
    Start { call: CallId, spec: StartSpec },
    /// A caller answers the question that holds the turn.
    Answer {
        call: CallId,
        prompt: String,
        answers: BTreeMap<String, String>,
        background: bool,
    },
    /// A caller stops what the agent is doing.
    Stop { call: CallId },
    /// A caller waiting on the turn went away.
    Unwait { call: CallId },
    /// The agent's lifetime ended, with its task or the runtime.
    Shutdown,
    /// The current process reported something.
    Event(AgentEvent),
    /// The current process asks something of Maple.
    Request { id: Value, request: AgentRequest },
    /// The current process's messages ended: it exited.
    Closed,
    Launched {
        process: ProcessId,
        result: Result<Opened, String>,
    },
    /// The turn's options were applied, or refused.
    Configured {
        process: ProcessId,
        result: Result<(), String>,
    },
    /// The turn's prompt reached the agent, or could not.
    Prompted {
        turn: u32,
        result: Result<(), String>,
    },
    /// The request to stop the turn was sent, answered or timed out.
    Interrupted { process: ProcessId },
    /// The agent did not end its turn in time after Stop.
    SettleExpired { timer: TimerId },
    /// A process ended and was reaped.
    Killed { process: ProcessId },
    /// The task's gate judged what `process` asked to do in turn `turn`.
    Judged {
        process: ProcessId,
        turn: u32,
        id: Value,
        item_id: String,
        action: String,
        verdict: Result<(), Refusal>,
    },
}

/// What the owner does about it.
#[derive(Debug)]
pub(super) enum Effect {
    /// Start a process, after ending the one it replaces.
    Launch {
        process: ProcessId,
        replace: Option<ProcessId>,
        thread_id: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        mode: AgentPermissionMode,
    },
    /// Give up starting a process; the half-started process goes.
    Abort {
        process: ProcessId,
    },
    /// Apply the turn's options. [`Input::Configured`] follows.
    Configure {
        process: ProcessId,
        turn: TurnSpec,
    },
    /// Send the turn's prompt. [`Input::Prompted`] follows.
    Prompt {
        process: ProcessId,
        turn: u32,
        spec: TurnSpec,
    },
    /// Ask the agent to end its turn. [`Input::Interrupted`] follows.
    Interrupt {
        process: ProcessId,
        thread_id: String,
        turn_id: Option<String>,
    },
    /// Arm the settle timer. [`Input::SettleExpired`] follows unless a
    /// later arming replaces it.
    Settle {
        timer: TimerId,
    },
    /// Stop reading a process and end it, after up to two seconds for its
    /// own exit when `flush`. [`Input::Killed`] follows.
    Kill {
        process: ProcessId,
        flush: bool,
    },
    /// Answer what a process asked.
    Reply {
        process: ProcessId,
        id: Value,
        reply: AgentReply,
    },
    /// Refuse a request Maple does not serve.
    Refuse {
        process: ProcessId,
        id: Value,
        message: String,
    },
    /// Put what the agent asks to do to the task's gate. [`Input::Judged`]
    /// follows.
    Judge {
        process: ProcessId,
        turn: u32,
        id: Value,
        item_id: String,
        action: String,
        tool_name: String,
        input: Value,
    },
    /// A turn ended: what still waits on it, as the gate, stops waiting.
    TurnEnded {
        turn: u32,
    },
    /// Show the turn's row as it stands.
    Row(AgentTimelineItem),
    SubagentStarted {
        background: bool,
    },
    SubagentActivity(String),
    SubagentFinished,
    /// Leave the agent's record in its task, so a restart can reopen it.
    Record(ExternalAgentActivity),
    /// A turn nobody waited on ended: tell the model.
    ReportEnd {
        activity: ExternalAgentActivity,
        row_id: String,
        outcome: TurnOutcome,
    },
    /// The agent asked a question nobody waited on: tell the model.
    ReportQuestion {
        activity: ExternalAgentActivity,
    },
    /// Answer a caller's [`Input::Start`] with the turn's number.
    Started {
        call: CallId,
        result: Result<u32, String>,
    },
    /// Answer a caller's [`Input::Answer`]: the turn the answer went to,
    /// or none when no question holds the turn.
    Answered {
        call: CallId,
        result: Result<Option<u32>, String>,
    },
    /// Stop is done for these callers.
    Stopped {
        calls: Vec<CallId>,
    },
    /// These callers stop waiting on the turn.
    Wake {
        calls: Vec<CallId>,
        why: Wake,
    },
    /// The agent is gone; its owner exits.
    Exit,
}

/// What the agent's callers and Maple's interface read of it.
#[derive(Debug, Clone, Default)]
pub(super) struct AgentView {
    /// The agent's activity, as of its owner's last step.
    pub(super) activity: ExternalAgentActivity,
    pub(super) turn: Option<TurnView>,
    pub(super) last_row_revision: u64,
    /// Shut down: its owner is exiting.
    pub(super) gone: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TurnView {
    pub(super) row_id: String,
    pub(super) background: bool,
    /// The agent's id for the turn, once it gave one.
    pub(super) turn_id: Option<String>,
}

#[derive(Debug)]
enum Phase {
    Idle,
    /// Setting up a turn for `call`: launching a process if it needs one,
    /// then applying the turn's options. No turn exists yet.
    Starting {
        call: CallId,
        spec: StartSpec,
        step: Step,
    },
    /// A turn runs.
    Running,
    /// Ending the turn for Stop or a shutdown.
    Stopping(Stopping),
    /// A turn's one-turn process is finishing; the turn ends once it has,
    /// so the next process resumes a session it finished writing.
    Reaping {
        process: ProcessId,
        outcome: TurnOutcome,
        error: Option<String>,
    },
    /// Shut down.
    Gone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Launching(ProcessId),
    Configuring(ProcessId),
}

#[derive(Debug, PartialEq)]
enum Stopping {
    /// The prompt is still on its way; the interrupt follows it, so it
    /// cannot overtake it. The timer ends a wait on an agent that never
    /// takes the prompt.
    AwaitingPrompt {
        timer: TimerId,
    },
    Interrupting {
        process: ProcessId,
    },
    /// Waiting for the agent to end its turn.
    Settling {
        process: ProcessId,
        timer: TimerId,
    },
    /// The process is ending. `finish` ends the turn once it has.
    Killing {
        process: ProcessId,
        finish: Option<(TurnOutcome, Option<String>)>,
    },
}

#[derive(Debug)]
struct Process {
    id: ProcessId,
    thread_id: String,
    mode_args: Vec<String>,
}

#[derive(Debug)]
struct Turn {
    number: u32,
    row_id: String,
    turn_id: Option<String>,
    background: bool,
    /// Callers waiting on the turn. A turn nobody waits on reports its end
    /// to the model as a notice.
    waiters: Vec<CallId>,
}

/// A question the agent asked, which its reply answers.
#[derive(Debug)]
struct Asked {
    process: ProcessId,
    id: Value,
    questions: Vec<AgentQuestion>,
}

pub(super) struct AgentMachine {
    agent_id: String,
    /// The agent's name, for messages.
    name: String,
    one_turn_per_process: bool,
    started: Instant,
    /// When the input being handled happened.
    now: Instant,
    phase: Phase,
    process: Option<Process>,
    next_process: ProcessId,
    next_timer: TimerId,
    thread_id: Option<String>,
    turn: Option<Turn>,
    /// The turn whose prompt is on its way, and the call that started it,
    /// answered once it is.
    prompting: Option<(u32, CallId)>,
    activity: ExternalAgentActivity,
    /// Streamed agent messages of the current turn, in order.
    messages: Vec<(String, String)>,
    /// The question that holds the turn; the agent asks one at a time.
    question: Option<Asked>,
    /// Questions asked while another waits.
    queued: VecDeque<Asked>,
    gated: bool,
    shutting_down: bool,
    /// Callers whose Stop is not done yet.
    stop_calls: Vec<CallId>,
    last_row_emit: Option<Instant>,
    last_row_revision: u64,
    effects: Vec<Effect>,
}

impl AgentMachine {
    pub(super) fn new(
        provider: &str,
        name: &str,
        agent_id: &str,
        one_turn_per_process: bool,
        started: Instant,
    ) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            name: name.to_string(),
            one_turn_per_process,
            started,
            now: started,
            phase: Phase::Idle,
            process: None,
            next_process: 1,
            next_timer: 1,
            thread_id: None,
            turn: None,
            prompting: None,
            activity: ExternalAgentActivity {
                provider: provider.to_string(),
                agent_id: agent_id.to_string(),
                status: "idle".to_string(),
                ..Default::default()
            },
            messages: Vec::new(),
            question: None,
            queued: VecDeque::new(),
            gated: false,
            shutting_down: false,
            stop_calls: Vec::new(),
            last_row_emit: None,
            last_row_revision: 0,
            effects: Vec::new(),
        }
    }

    /// An agent opened again from its record: its process is gone, its
    /// session is not, and the next turn resumes it.
    pub(super) fn restored(
        name: &str,
        one_turn_per_process: bool,
        started: Instant,
        activity: ExternalAgentActivity,
    ) -> Self {
        let mut machine = Self::new(
            &activity.provider,
            name,
            &activity.agent_id,
            one_turn_per_process,
            started,
        );
        machine.thread_id = activity.thread_id.clone();
        machine.activity = activity;
        machine
    }

    /// Whether the agent is starting a turn, in one, or stopping.
    pub(super) fn working(&self) -> bool {
        !matches!(self.phase, Phase::Idle | Phase::Gone)
    }

    /// Whether `call`'s start was taken up, rather than refused.
    pub(super) fn is_starting(&self, call: CallId) -> bool {
        matches!(&self.phase, Phase::Starting { call: starting, .. } if *starting == call)
    }

    pub(super) fn view(&self) -> AgentView {
        AgentView {
            activity: self.activity.clone(),
            turn: self.turn.as_ref().map(|turn| TurnView {
                row_id: turn.row_id.clone(),
                background: turn.background,
                turn_id: turn.turn_id.clone(),
            }),
            last_row_revision: self.last_row_revision,
            gone: matches!(self.phase, Phase::Gone),
        }
    }

    /// Take in what happened at `now` and say what to do about it.
    pub(super) fn handle(&mut self, input: Input, now: Instant) -> Vec<Effect> {
        self.now = now;
        match input {
            Input::Start { call, spec } => self.start(call, spec),
            Input::Answer {
                call,
                prompt,
                answers,
                background,
            } => self.answer(call, &prompt, &answers, background),
            Input::Stop { call } => self.stop(Some(call)),
            Input::Unwait { call } => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.waiters.retain(|waiter| *waiter != call);
                }
            }
            Input::Shutdown => self.shutdown(),
            Input::Event(event) => self.event(event),
            Input::Request { id, request } => self.request(id, request),
            Input::Closed => self.closed(),
            Input::Launched { process, result } => self.launched(process, result),
            Input::Configured { process, result } => self.configured(process, result),
            Input::Prompted { turn, result } => self.prompted(turn, result),
            Input::Interrupted { process } => self.interrupted(process),
            Input::SettleExpired { timer } => self.settle_expired(timer),
            Input::Killed { process } => self.killed(process),
            Input::Judged {
                process,
                turn,
                id,
                item_id,
                action,
                verdict,
            } => self.judged(process, turn, id, &item_id, &action, verdict),
        }
        mem::take(&mut self.effects)
    }

    fn push(&mut self, effect: Effect) {
        self.effects.push(effect);
    }

    fn busy_message(&self) -> String {
        format!(
            "Agent {} is still working on its previous turn. Wait for Maple's notice, or check with {AGENT_STATUS_TOOL}.",
            self.agent_id
        )
    }

    fn start(&mut self, call: CallId, spec: StartSpec) {
        if self.shutting_down || matches!(self.phase, Phase::Gone) {
            return self.push(Effect::Started {
                call,
                result: Err(SHUT_DOWN.to_string()),
            });
        }
        if !matches!(self.phase, Phase::Idle) {
            let busy = self.busy_message();
            return self.push(Effect::Started {
                call,
                result: Err(busy),
            });
        }
        self.gated = spec.gated;
        let step = match &self.process {
            Some(process) if process.mode_args == spec.mode_args => {
                let turn = turn_spec(&spec, &process.thread_id);
                let id = process.id;
                self.push(Effect::Configure { process: id, turn });
                Step::Configuring(id)
            }
            _ => {
                // The agent takes its own mode only at launch: a process in
                // another mode goes, and a new one resumes the session.
                let replace = self.process.take().map(|process| process.id);
                if replace.is_some() {
                    log::info!("External agent {} restarts in another mode", self.agent_id);
                }
                let id = self.next_process;
                self.next_process += 1;
                self.push(Effect::Launch {
                    process: id,
                    replace,
                    thread_id: self.thread_id.clone(),
                    model: spec.model.clone(),
                    effort: spec.effort.clone(),
                    mode: spec.mode,
                });
                Step::Launching(id)
            }
        };
        self.phase = Phase::Starting { call, spec, step };
    }

    fn launched(&mut self, process: ProcessId, result: Result<Opened, String>) {
        let current = matches!(
            &self.phase,
            Phase::Starting { step: Step::Launching(id), .. } if *id == process
        );
        if !current {
            // A launch that was given up on: what it started goes.
            if result.is_ok() {
                self.push(Effect::Kill {
                    process,
                    flush: false,
                });
            }
            return;
        }
        let Phase::Starting { call, spec, .. } = mem::replace(&mut self.phase, Phase::Idle) else {
            return;
        };
        match result {
            Err(error) => {
                self.push(Effect::Started {
                    call,
                    result: Err(error),
                });
                self.rest();
            }
            Ok(opened) => {
                // A thread the agent has not confirmed is provisional: a
                // launch that fails before the confirmation starts fresh on
                // retry.
                if opened.confirmed {
                    self.thread_id = Some(opened.thread_id.clone());
                    self.activity.thread_id = Some(opened.thread_id.clone());
                }
                let turn = turn_spec(&spec, &opened.thread_id);
                self.process = Some(Process {
                    id: process,
                    thread_id: opened.thread_id,
                    mode_args: spec.mode_args.clone(),
                });
                self.push(Effect::Configure { process, turn });
                self.phase = Phase::Starting {
                    call,
                    spec,
                    step: Step::Configuring(process),
                };
            }
        }
    }

    fn configured(&mut self, process: ProcessId, result: Result<(), String>) {
        let current = matches!(
            &self.phase,
            Phase::Starting { step: Step::Configuring(id), .. } if *id == process
        );
        if !current {
            return;
        }
        let Phase::Starting { call, spec, .. } = mem::replace(&mut self.phase, Phase::Idle) else {
            return;
        };
        match result {
            // A model or effort the agent refuses starts no turn: the
            // agent's last turn stands as it was.
            Err(error) => {
                self.push(Effect::Started {
                    call,
                    result: Err(error),
                });
                self.rest();
            }
            Ok(()) => self.begin_turn(call, spec, process),
        }
    }

    fn begin_turn(&mut self, call: CallId, spec: StartSpec, process: ProcessId) {
        let Some(thread_id) = self
            .process
            .as_ref()
            .map(|process| process.thread_id.clone())
        else {
            return;
        };
        self.messages.clear();
        self.activity.begin_turn();
        let number = self.activity.turns;
        let row_id = spec
            .row_id
            .clone()
            .unwrap_or_else(|| format!("external-{}-turn-{number}", self.agent_id));
        self.turn = Some(Turn {
            number,
            row_id,
            turn_id: None,
            background: spec.background,
            // A blocking call waits from the start, so a turn that ends at
            // once is not also reported as a notice.
            waiters: if spec.background {
                Vec::new()
            } else {
                vec![call]
            },
        });
        self.phase = Phase::Running;
        self.prompting = Some((number, call));
        self.last_row_revision = next_timeline_revision();
        log::info!("External agent {} turn starts", self.agent_id);
        self.push(Effect::SubagentStarted {
            background: spec.background,
        });
        self.emit_row(true);
        self.record();
        let spec = turn_spec(&spec, &thread_id);
        self.push(Effect::Prompt {
            process,
            turn: number,
            spec,
        });
    }

    fn prompted(&mut self, turn: u32, result: Result<(), String>) {
        let Some((_, call)) = self.prompting.filter(|(number, _)| *number == turn) else {
            return;
        };
        self.prompting = None;
        match result {
            Ok(()) => {
                self.push(Effect::Started {
                    call,
                    result: Ok(turn),
                });
                if matches!(self.phase, Phase::Stopping(Stopping::AwaitingPrompt { .. })) {
                    self.interrupt();
                }
            }
            Err(error) => {
                let ends_turn = self.turn.as_ref().is_some_and(|open| open.number == turn)
                    && matches!(
                        self.phase,
                        Phase::Running | Phase::Stopping(Stopping::AwaitingPrompt { .. })
                    );
                if ends_turn {
                    self.finish_turn(TurnOutcome::Failed, Some(error.clone()));
                    self.after_turn_end();
                }
                self.push(Effect::Started {
                    call,
                    result: Err(error),
                });
            }
        }
    }

    fn answer(
        &mut self,
        call: CallId,
        prompt: &str,
        answers: &BTreeMap<String, String>,
        background: bool,
    ) {
        if !matches!(self.phase, Phase::Running) || self.question.is_none() {
            return self.push(Effect::Answered {
                call,
                result: Ok(None),
            });
        }
        let response = match caller_answers(&self.activity.questions, prompt, answers) {
            Ok(response) => response,
            Err(error) => {
                return self.push(Effect::Answered {
                    call,
                    result: Err(error),
                });
            }
        };
        let (Some(asked), Some(turn)) = (self.question.take(), self.turn.as_mut()) else {
            return;
        };
        if !background {
            turn.waiters.push(call);
        }
        turn.background = background;
        let number = turn.number;
        self.activity.questions.clear();
        self.activity.status = "running".to_string();
        log::info!("External agent {} has its answer", self.agent_id);
        self.push(Effect::Reply {
            process: asked.process,
            id: asked.id,
            reply: AgentReply::Answers(response),
        });
        self.push(Effect::SubagentActivity("Working".to_string()));
        self.emit_row(true);
        self.push(Effect::Answered {
            call,
            result: Ok(Some(number)),
        });
        if let Some(next) = self.queued.pop_front() {
            self.ask(next);
        }
    }

    /// Stop what the agent is doing, for `call` or for a shutdown. A turn is
    /// asked to end, given time to, and its process goes either way: Codex
    /// does not always end a sandboxed command on interrupt. The thread
    /// stays, and the next turn resumes it in a fresh process.
    fn stop(&mut self, call: Option<CallId>) {
        self.stop_calls.extend(call);
        match mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Idle => self.rest(),
            Phase::Gone => {
                self.phase = Phase::Gone;
                let calls = mem::take(&mut self.stop_calls);
                if !calls.is_empty() {
                    self.push(Effect::Stopped { calls });
                }
            }
            Phase::Starting {
                call: start_call,
                step,
                ..
            } => {
                let why = if self.shutting_down {
                    SHUT_DOWN
                } else {
                    LAUNCH_CANCELLED
                };
                self.push(Effect::Started {
                    call: start_call,
                    result: Err(why.to_string()),
                });
                match step {
                    Step::Launching(process) => {
                        self.push(Effect::Abort { process });
                        self.rest();
                    }
                    // Options may be half sent: the process goes rather
                    // than take a prompt after them.
                    Step::Configuring(_) => match self.retire(false) {
                        Some(process) => {
                            self.phase = Phase::Stopping(Stopping::Killing {
                                process,
                                finish: None,
                            });
                        }
                        None => self.rest(),
                    },
                }
            }
            Phase::Running => {
                self.phase = Phase::Running;
                if self.prompting.is_some() {
                    let timer = self.arm_timer();
                    self.phase = Phase::Stopping(Stopping::AwaitingPrompt { timer });
                } else {
                    self.interrupt();
                }
            }
            phase @ (Phase::Stopping(_) | Phase::Reaping { .. }) => self.phase = phase,
        }
    }

    /// Ask the agent to end its turn, as Stop does.
    fn interrupt(&mut self) {
        let target = match (&self.process, &self.turn) {
            (Some(process), Some(turn)) => {
                Some((process.id, process.thread_id.clone(), turn.turn_id.clone()))
            }
            _ => None,
        };
        match target {
            Some((process, thread_id, turn_id)) => {
                self.push(Effect::Interrupt {
                    process,
                    thread_id,
                    turn_id,
                });
                self.phase = Phase::Stopping(Stopping::Interrupting { process });
            }
            None => {
                self.finish_turn(TurnOutcome::Cancelled, None);
                self.rest();
            }
        }
    }

    fn interrupted(&mut self, process: ProcessId) {
        let current = matches!(
            self.phase,
            Phase::Stopping(Stopping::Interrupting { process: interrupting }) if interrupting == process
        );
        if !current {
            return;
        }
        let timer = self.arm_timer();
        self.phase = Phase::Stopping(Stopping::Settling { process, timer });
    }

    fn settle_expired(&mut self, timer: TimerId) {
        let expired = matches!(
            self.phase,
            Phase::Stopping(
                Stopping::AwaitingPrompt { timer: armed } | Stopping::Settling { timer: armed, .. }
            ) if armed == timer
        );
        if !expired {
            return;
        }
        // The agent did not end its turn in time: its process goes, and the
        // turn ends as stopped.
        match self.retire(false) {
            Some(process) => {
                self.phase = Phase::Stopping(Stopping::Killing {
                    process,
                    finish: Some((TurnOutcome::Cancelled, None)),
                });
            }
            None => {
                self.finish_turn(TurnOutcome::Cancelled, None);
                self.rest();
            }
        }
    }

    fn killed(&mut self, process: ProcessId) {
        match mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Reaping {
                process: reaped,
                outcome,
                error,
            } if reaped == process => {
                self.finish_turn(outcome, error);
                self.rest();
            }
            Phase::Stopping(Stopping::Killing {
                process: killed,
                finish,
            }) if killed == process => {
                if let Some((outcome, error)) = finish {
                    self.finish_turn(outcome, error);
                }
                self.rest();
            }
            phase => self.phase = phase,
        }
    }

    fn shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        match self.phase {
            Phase::Idle => self.rest(),
            Phase::Starting { .. } | Phase::Running => self.stop(None),
            Phase::Stopping(_) | Phase::Reaping { .. } | Phase::Gone => {}
        }
    }

    /// The agent is done with a turn or a stop: idle again, or, shutting
    /// down, gone once its process is.
    fn rest(&mut self) {
        self.phase = Phase::Idle;
        if self.shutting_down {
            if let Some(process) = self.retire(false) {
                self.phase = Phase::Stopping(Stopping::Killing {
                    process,
                    finish: None,
                });
                return;
            }
            self.phase = Phase::Gone;
        }
        let calls = mem::take(&mut self.stop_calls);
        if !calls.is_empty() {
            self.push(Effect::Stopped { calls });
        }
        if matches!(self.phase, Phase::Gone) {
            self.push(Effect::Exit);
        }
    }

    /// End the current process; [`Input::Killed`] follows.
    fn retire(&mut self, flush: bool) -> Option<ProcessId> {
        let process = self.process.take()?;
        self.push(Effect::Kill {
            process: process.id,
            flush,
        });
        Some(process.id)
    }

    fn arm_timer(&mut self) -> TimerId {
        let timer = self.next_timer;
        self.next_timer += 1;
        self.push(Effect::Settle { timer });
        timer
    }

    /// The current process's messages ended: whatever it was doing is over.
    fn closed(&mut self) {
        let Some(process) = self.process.take() else {
            return;
        };
        self.push(Effect::Kill {
            process: process.id,
            flush: false,
        });
        if self.turn.is_none() {
            return;
        }
        let error = format!("The {} process exited before the turn finished.", self.name);
        match self.phase {
            Phase::Running => {
                self.finish_turn(TurnOutcome::Failed, Some(error));
                self.rest();
            }
            Phase::Stopping(
                Stopping::AwaitingPrompt { .. }
                | Stopping::Interrupting { .. }
                | Stopping::Settling { .. },
            ) => {
                self.finish_turn(TurnOutcome::Failed, Some(error));
                self.phase = Phase::Stopping(Stopping::Killing {
                    process: process.id,
                    finish: None,
                });
            }
            _ => {}
        }
    }

    /// After a turn ended by itself or failed: idle again, or, while
    /// stopping, its process still goes.
    fn after_turn_end(&mut self) {
        match self.phase {
            Phase::Running => self.rest(),
            Phase::Stopping(
                Stopping::AwaitingPrompt { .. }
                | Stopping::Interrupting { .. }
                | Stopping::Settling { .. },
            ) => match self.retire(false) {
                Some(process) => {
                    self.phase = Phase::Stopping(Stopping::Killing {
                        process,
                        finish: None,
                    });
                }
                None => self.rest(),
            },
            _ => {}
        }
    }

    fn event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::ThreadStarted { thread_id } => {
                let first = self.thread_id.is_none();
                self.thread_id.get_or_insert(thread_id.clone());
                self.activity.thread_id.get_or_insert(thread_id);
                // An agent that names its session only once its turn runs,
                // as Claude Code does, is recorded once it has.
                if first {
                    self.record();
                }
            }
            AgentEvent::TurnStarted { turn_id } => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.turn_id = turn_id;
                }
            }
            AgentEvent::TurnCompleted { outcome, error } => self.turn_completed(outcome, error),
            AgentEvent::AgentMessageDelta { item_id, delta } => {
                match self.messages.iter_mut().find(|(id, _)| *id == item_id) {
                    Some((_, text)) => text.push_str(&delta),
                    None => self.messages.push((item_id, delta)),
                }
                self.refresh_text();
                self.emit_row(false);
            }
            AgentEvent::ItemStarted(item) => {
                let label = match item {
                    AgentItem::CommandExecution { id, command, .. } => {
                        let label = format!("Running: {command}");
                        self.activity.record_command_started(id, command);
                        Some(label)
                    }
                    AgentItem::TodoList { items, .. } => {
                        self.activity.record_todos(items);
                        None
                    }
                    AgentItem::FileChange { changes, .. } => {
                        let label = changes
                            .first()
                            .map(|change| format!("Editing: {}", change.path));
                        self.activity.record_file_changes(changes);
                        label
                    }
                    _ => None,
                };
                if let Some(label) = label {
                    self.push(Effect::SubagentActivity(label));
                }
                self.emit_row(true);
            }
            AgentEvent::ItemCompleted(item) => {
                let label = match item {
                    AgentItem::AgentMessage { id, text } => {
                        match self.messages.iter_mut().find(|(item_id, _)| *item_id == id) {
                            Some((_, existing)) if !text.is_empty() => *existing = text,
                            Some(_) => {}
                            None => self.messages.push((id, text)),
                        }
                        self.refresh_text();
                        None
                    }
                    AgentItem::CommandExecution {
                        id,
                        command,
                        exit_code,
                        status,
                    } => {
                        self.activity
                            .record_command_finished(&id, command, exit_code, status);
                        None
                    }
                    AgentItem::FileChange { changes, .. } => {
                        let label = changes
                            .first()
                            .map(|change| format!("Edited: {}", change.path));
                        self.activity.record_file_changes(changes);
                        label
                    }
                    AgentItem::TodoList { items, .. } => {
                        self.activity.record_todos(items);
                        None
                    }
                    AgentItem::AsyncQuestion { questions, .. } => {
                        // The question rides the agent's message and does not
                        // hold the turn: the turn's result carries it, and the
                        // caller's next send answers it.
                        self.activity.questions = driver::async_question_prompts(&questions)
                            .iter()
                            .map(ActivityQuestion::from)
                            .collect();
                        Some("Asked a question".to_string())
                    }
                    AgentItem::Other { .. } => None,
                };
                if let Some(label) = label {
                    self.push(Effect::SubagentActivity(label));
                }
                self.emit_row(true);
            }
            AgentEvent::Other => {}
        }
    }

    fn turn_completed(&mut self, outcome: TurnOutcome, error: Option<String>) {
        if self.one_turn_per_process
            && let Some(process) = self.retire(outcome == TurnOutcome::Completed)
        {
            // The process served its turn and goes before the end counts,
            // giving a finished one time to flush its session.
            match self.phase {
                Phase::Running if self.turn.is_some() => {
                    self.phase = Phase::Reaping {
                        process,
                        outcome,
                        error,
                    };
                }
                Phase::Stopping(_) => {
                    let finish = self.turn.as_ref().map(|_| (outcome, error));
                    self.phase = Phase::Stopping(Stopping::Killing { process, finish });
                }
                _ => {}
            }
            return;
        }
        self.finish_turn(outcome, error);
        self.after_turn_end();
    }

    fn request(&mut self, id: Value, request: AgentRequest) {
        let Some(process) = self.process.as_ref().map(|process| process.id) else {
            return;
        };
        let open = matches!(self.phase, Phase::Running) && self.turn.is_some();
        match request {
            AgentRequest::Approval {
                item_id,
                tool_name,
                arguments,
                ..
            } => {
                // A request after its turn, or while it stops, is refused.
                let Some(number) = self.turn.as_ref().map(|turn| turn.number).filter(|_| open)
                else {
                    return self.push(Effect::Reply {
                        process,
                        id,
                        reply: AgentReply::Decision(AgentPermissionDecision::Cancel),
                    });
                };
                if !self.gated {
                    log::info!(
                        "External agent {} approval accepted for {tool_name}",
                        self.agent_id
                    );
                    return self.push(Effect::Reply {
                        process,
                        id,
                        reply: AgentReply::Decision(AgentPermissionDecision::AllowOnce),
                    });
                }
                // Only the tool, its input and the folder reach the
                // classifier: the agent's own title for the action could
                // argue it through.
                let input = Value::Object(arguments);
                let action = card_action(&tool_name, &input);
                self.activity.pending_permission = Some(format!("approve {action}"));
                self.emit_row(true);
                self.push_activity_label();
                self.push(Effect::Judge {
                    process,
                    turn: number,
                    id,
                    item_id,
                    action,
                    tool_name,
                    input,
                });
            }
            AgentRequest::ModeChange { .. } => {
                // Maple sets the agent's mode from the task's; the caller
                // hears that the agent asked.
                self.activity.declined.push(
                    "Changing its own mode: Maple keeps the agent in the task's permission mode."
                        .to_string(),
                );
                self.emit_row(true);
                self.push(Effect::Reply {
                    process,
                    id,
                    reply: AgentReply::Decision(AgentPermissionDecision::Cancel),
                });
            }
            AgentRequest::UserInput { questions } => {
                if questions.is_empty() || !open {
                    return self.push(Effect::Reply {
                        process,
                        id,
                        reply: AgentReply::Answers(String::new()),
                    });
                }
                let asked = Asked {
                    process,
                    id,
                    questions,
                };
                // One question at a time; another waits for the first's
                // answer.
                if self.question.is_some() {
                    self.queued.push_back(asked);
                } else {
                    self.ask(asked);
                }
            }
            AgentRequest::Unknown { method } => self.push(Effect::Refuse {
                process,
                id,
                message: format!("{method} is not supported by this client"),
            }),
        }
    }

    /// Put the agent's question to the model that called it. A caller
    /// waiting on the turn returns with it; otherwise the model gets a
    /// notice. The answer comes with the caller's next send.
    fn ask(&mut self, asked: Asked) {
        let Some(turn) = self.turn.as_mut() else {
            return self.push(Effect::Reply {
                process: asked.process,
                id: asked.id,
                reply: AgentReply::Answers(String::new()),
            });
        };
        let waiters = mem::take(&mut turn.waiters);
        self.activity.status = "waiting".to_string();
        self.activity.questions = asked.questions.iter().map(ActivityQuestion::from).collect();
        // The row shows what was asked, as the agent's own words.
        let id = format!("maple-question-{}", self.messages.len());
        self.messages.push((id, question_text(&asked.questions)));
        self.refresh_text();
        log::info!(
            "External agent {} asks its caller {} question(s)",
            self.agent_id,
            asked.questions.len()
        );
        self.question = Some(asked);
        let report = waiters.is_empty();
        if !report {
            self.push(Effect::Wake {
                calls: waiters,
                why: Wake::Asked,
            });
        }
        self.push(Effect::SubagentActivity("Asked a question".to_string()));
        self.emit_row(true);
        if report && !self.shutting_down {
            let activity = self.activity_now();
            self.push(Effect::ReportQuestion { activity });
        }
    }

    fn judged(
        &mut self,
        process: ProcessId,
        turn: u32,
        id: Value,
        item_id: &str,
        action: &str,
        verdict: Result<(), Refusal>,
    ) {
        if self.turn.as_ref().is_some_and(|open| open.number == turn) {
            let declined = match &verdict {
                Ok(()) | Err(Refusal::Stopped) => None,
                Err(Refusal::Declined(None)) => Some(format!("{action}: the user declined it.")),
                Err(Refusal::Declined(Some(note))) => {
                    Some(format!("{action}: the user declined it and said: {note}"))
                }
                Err(Refusal::NobodyToAsk) => Some(format!(
                    "{action}: it needed the user's approval and no connected interface could ask."
                )),
            };
            self.activity.declined.extend(declined);
            // A command that was not allowed did not run, whether or not the
            // agent says so.
            if verdict.is_err() {
                for command in &mut self.activity.commands {
                    if command.id == item_id && command.status == "running" {
                        command.status = "failed".to_string();
                    }
                }
            }
            self.activity.pending_permission = None;
            self.emit_row(true);
            self.push_activity_label();
        }
        log::info!(
            "External agent {} approval for {action}: {}",
            self.agent_id,
            if verdict.is_ok() {
                "allowed"
            } else {
                "refused"
            }
        );
        let decision = match verdict {
            Ok(()) => AgentPermissionDecision::AllowOnce,
            Err(_) => AgentPermissionDecision::Cancel,
        };
        self.push(Effect::Reply {
            process,
            id,
            reply: AgentReply::Decision(decision),
        });
    }

    /// End the turn with `outcome`: its last row, its record, and the word
    /// to whoever waits on it or, with nobody waiting, to the model.
    fn finish_turn(&mut self, outcome: TurnOutcome, error: Option<String>) {
        let Some(turn) = self.turn.take() else {
            return;
        };
        self.activity.status = outcome.status().to_string();
        self.activity.pending_permission = None;
        // A command a stopped or failed turn left running did not finish;
        // some agents never say so.
        if outcome != TurnOutcome::Completed {
            for command in &mut self.activity.commands {
                if command.status == "running" {
                    command.status = "failed".to_string();
                }
            }
        }
        // A question that held the turn went with it, as did any behind it.
        if let Some(asked) = self.question.take() {
            self.activity.questions.clear();
            self.push(Effect::Reply {
                process: asked.process,
                id: asked.id,
                reply: AgentReply::Answers(String::new()),
            });
        }
        for asked in mem::take(&mut self.queued) {
            self.push(Effect::Reply {
                process: asked.process,
                id: asked.id,
                reply: AgentReply::Answers(String::new()),
            });
        }
        if error.is_some() {
            self.activity.error = error;
        }
        self.refresh_elapsed();
        let mut row = activity_row_item(&turn.row_id, &self.activity, unix_ms());
        self.last_row_revision = next_timeline_revision();
        row.revision = self.last_row_revision;
        row.live = Some(self.live_identity());
        self.last_row_emit = Some(self.now);
        log::info!(
            "External agent {} turn ended: {}",
            self.agent_id,
            outcome.status()
        );
        self.push(Effect::TurnEnded { turn: turn.number });
        self.push(Effect::Row(row));
        // Recorded before anyone waiting on the turn hears it ended.
        self.record();
        let report = turn.waiters.is_empty();
        if !report {
            self.push(Effect::Wake {
                calls: turn.waiters,
                why: Wake::Ended,
            });
        }
        self.push(Effect::SubagentFinished);
        // An agent shut down went with its task or the runtime: nobody is
        // left to tell.
        if report && !self.shutting_down {
            let activity = self.activity_now();
            self.push(Effect::ReportEnd {
                activity,
                row_id: turn.row_id,
                outcome,
            });
        }
    }

    /// Leave the agent's record in its task. An agent shutting down leaves
    /// none: its last record stands, and one left mid-turn reads as cut
    /// short.
    fn record(&mut self) {
        if !self.shutting_down {
            let activity = self.activity_now();
            self.push(Effect::Record(activity));
        }
    }

    fn push_activity_label(&mut self) {
        let label = latest_activity_label(&self.activity).unwrap_or_else(|| "Working".to_string());
        self.push(Effect::SubagentActivity(label));
    }

    fn refresh_elapsed(&mut self) {
        self.activity.elapsed_ms = elapsed_ms(self.started, self.now);
    }

    fn activity_now(&mut self) -> ExternalAgentActivity {
        self.refresh_elapsed();
        self.activity.clone()
    }

    fn live_identity(&self) -> AgentLiveRowIdentity {
        AgentLiveRowIdentity {
            run_id: external_run_id(&self.agent_id),
            occurrence: u64::from(self.activity.turns),
        }
    }

    fn refresh_text(&mut self) {
        let joined = self
            .messages
            .iter()
            .map(|(_, text)| text.as_str())
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        self.activity.text = tail_bounded(&joined, MAX_RESULT_TEXT_CHARS);
    }

    /// Repaint the turn's row with the activity so far. Streamed text is
    /// throttled; structural changes are not.
    fn emit_row(&mut self, force: bool) {
        let Some(row_id) = self.turn.as_ref().map(|turn| turn.row_id.clone()) else {
            return;
        };
        if !force
            && self
                .last_row_emit
                .is_some_and(|last| self.now.duration_since(last) < LIVE_ROW_INTERVAL)
        {
            return;
        }
        self.last_row_emit = Some(self.now);
        self.last_row_revision = next_timeline_revision();
        self.refresh_elapsed();
        let mut item = activity_row_item(&row_id, &self.activity, unix_ms());
        item.revision = self.last_row_revision;
        item.live = Some(self.live_identity());
        self.push(Effect::Row(item));
    }
}

/// How long the agent has existed at `now`, as its activity says it.
pub(super) fn elapsed_ms(started: Instant, now: Instant) -> u64 {
    now.duration_since(started)
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn turn_spec(spec: &StartSpec, thread_id: &str) -> TurnSpec {
    TurnSpec {
        thread_id: thread_id.to_string(),
        prompt: spec.prompt.clone(),
        model: spec.model.clone(),
        effort: spec.effort.clone(),
        mode: spec.mode,
    }
}

impl From<&AgentQuestion> for ActivityQuestion {
    fn from(question: &AgentQuestion) -> Self {
        Self {
            id: question.id.clone(),
            question: question.question.clone(),
            options: question
                .options
                .iter()
                .map(|option| option.label.clone())
                .collect(),
            multi_select: question.multi_select,
        }
    }
}

impl ExternalAgentActivity {
    pub(super) fn record_command_started(&mut self, id: String, command: String) {
        let command = bounded_timeline_text(&command, MAX_ACTIVITY_COMMAND_CHARS);
        if let Some(existing) = self.commands.iter_mut().find(|entry| entry.id == id) {
            existing.command = command;
            return;
        }
        self.commands.push(ActivityCommand {
            id,
            command,
            exit_code: None,
            status: "running".to_string(),
        });
        let overflow = self.commands.len().saturating_sub(MAX_ACTIVITY_COMMANDS);
        if overflow > 0 {
            self.commands.drain(..overflow);
        }
    }

    pub(super) fn record_command_finished(
        &mut self,
        id: &str,
        command: String,
        exit_code: Option<i64>,
        status: Option<String>,
    ) {
        let status = match (status.as_deref(), exit_code) {
            (Some("failed" | "error"), _) => "failed",
            (_, Some(code)) if code != 0 => "failed",
            _ => "completed",
        }
        .to_string();
        match self.commands.iter_mut().find(|entry| entry.id == id) {
            Some(existing) => {
                existing.exit_code = exit_code;
                existing.status = status;
            }
            None => {
                self.record_command_started(id.to_string(), command);
                if let Some(existing) = self.commands.last_mut() {
                    existing.exit_code = exit_code;
                    existing.status = status;
                }
            }
        }
    }

    pub(super) fn record_file_changes(&mut self, changes: Vec<FileChangeEntry>) {
        for change in changes {
            if let Some(existing) = self
                .file_changes
                .iter_mut()
                .find(|entry| entry.path == change.path)
            {
                existing.kind = change.kind;
                continue;
            }
            self.file_changes.push(ActivityFileChange {
                path: change.path,
                kind: change.kind,
            });
        }
        let overflow = self
            .file_changes
            .len()
            .saturating_sub(MAX_ACTIVITY_FILE_CHANGES);
        if overflow > 0 {
            self.file_changes.drain(..overflow);
        }
    }

    pub(super) fn record_todos(&mut self, items: Vec<TodoEntry>) {
        self.todos = items
            .into_iter()
            .take(MAX_ACTIVITY_TODOS)
            .map(|item| ActivityTodo {
                text: item.text,
                completed: item.completed,
            })
            .collect();
    }

    fn begin_turn(&mut self) {
        self.status = "running".to_string();
        self.text.clear();
        self.questions.clear();
        self.error = None;
        self.pending_permission = None;
        self.declined.clear();
        self.turns = self.turns.saturating_add(1);
    }
}

#[cfg(test)]
#[path = "machine_tests.rs"]
mod tests;
