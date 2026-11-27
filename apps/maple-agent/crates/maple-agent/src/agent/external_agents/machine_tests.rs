//! The lifecycle, transition by transition, with no process, task or clock.

use std::collections::BTreeMap;
use std::time::Instant;

use serde_json::{Map, json};

use super::super::super::{AgentPermissionDecision, AgentPermissionMode, AgentQuestion};
use super::super::driver::{AgentEvent, AgentItem, AgentReply, AgentRequest, TurnOutcome};
use super::*;

fn machine() -> AgentMachine {
    AgentMachine::new("grok", "Grok Build", "grok-1", false, Instant::now())
}

/// A turn as a blocking caller (or, with `background`, a background one)
/// asks for it, in Read only.
fn spec(background: bool) -> StartSpec {
    StartSpec {
        prompt: "Fix the bug".to_string(),
        background,
        model: None,
        effort: None,
        mode: AgentPermissionMode::ReadOnly,
        mode_args: vec!["--permission-mode".to_string(), "default".to_string()],
        row_id: Some("row".to_string()),
        gated: true,
    }
}

fn step(machine: &mut AgentMachine, input: Input) -> Vec<Effect> {
    machine.handle(input, Instant::now())
}

/// Each effect's kind, in order.
fn kinds(effects: &[Effect]) -> Vec<&'static str> {
    effects
        .iter()
        .map(|effect| match effect {
            Effect::Launch { .. } => "launch",
            Effect::Abort { .. } => "abort",
            Effect::Configure { .. } => "configure",
            Effect::Prompt { .. } => "prompt",
            Effect::Interrupt { .. } => "interrupt",
            Effect::Settle { .. } => "settle",
            Effect::Kill { .. } => "kill",
            Effect::Reply { .. } => "reply",
            Effect::Refuse { .. } => "refuse",
            Effect::Judge { .. } => "judge",
            Effect::TurnEnded { .. } => "turn-ended",
            Effect::Row(_) => "row",
            Effect::SubagentStarted { .. } => "started-panel",
            Effect::SubagentActivity(_) => "activity",
            Effect::SubagentFinished => "finished",
            Effect::Record(_) => "record",
            Effect::ReportEnd { .. } => "report-end",
            Effect::ReportQuestion { .. } => "report-question",
            Effect::Started { .. } => "started",
            Effect::Answered { .. } => "answered",
            Effect::Stopped { .. } => "stopped",
            Effect::Wake { .. } => "wake",
            Effect::Exit => "exit",
        })
        .collect()
}

fn has(effects: &[Effect], kind: &str) -> bool {
    kinds(effects).contains(&kind)
}

fn count(effects: &[Effect], kind: &str) -> usize {
    kinds(effects).iter().filter(|seen| **seen == kind).count()
}

fn opened() -> Result<Opened, String> {
    Ok(Opened {
        thread_id: "thread-1".to_string(),
        confirmed: true,
    })
}

/// Take call 1's turn up to the point its prompt is on its way.
fn prompting(machine: &mut AgentMachine, background: bool) {
    let effects = step(
        machine,
        Input::Start {
            call: 1,
            spec: spec(background),
        },
    );
    assert!(matches!(effects[..], [Effect::Launch { process: 1, .. }]));
    let effects = step(
        machine,
        Input::Launched {
            process: 1,
            result: opened(),
        },
    );
    assert!(matches!(
        effects[..],
        [Effect::Configure { process: 1, .. }]
    ));
    let effects = step(
        machine,
        Input::Configured {
            process: 1,
            result: Ok(()),
        },
    );
    assert_eq!(
        kinds(&effects),
        ["started-panel", "row", "record", "prompt"],
        "a turn shows, is recorded, then its prompt goes"
    );
}

/// Take call 1's turn until it runs.
fn running(machine: &mut AgentMachine, background: bool) {
    prompting(machine, background);
    let effects = step(
        machine,
        Input::Prompted {
            turn: 1,
            result: Ok(()),
        },
    );
    assert!(matches!(
        effects[..],
        [Effect::Started {
            call: 1,
            result: Ok(1)
        }]
    ));
}

fn completed() -> Input {
    Input::Event(AgentEvent::TurnCompleted {
        outcome: TurnOutcome::Completed,
        error: None,
    })
}

#[test]
fn a_turn_ends_once_recorded_before_its_caller_hears() {
    let mut machine = machine();
    running(&mut machine, false);
    assert!(machine.working());
    let effects = step(&mut machine, completed());
    assert_eq!(
        kinds(&effects),
        ["turn-ended", "row", "record", "wake", "finished"],
        "the record is written before the waiting caller wakes, and nobody is sent a notice"
    );
    assert!(matches!(
        &effects[3],
        Effect::Wake { calls, why: Wake::Ended } if calls == &[1]
    ));
    assert!(!machine.working());
    assert_eq!(machine.view().activity.status, "completed");
}

#[test]
fn a_background_turn_reports_its_end_to_the_model() {
    let mut machine = machine();
    running(&mut machine, true);
    let effects = step(&mut machine, completed());
    assert!(!has(&effects, "wake"));
    assert!(has(&effects, "report-end"));
}

#[test]
fn a_caller_that_leaves_the_turn_hears_of_its_end_by_notice() {
    let mut machine = machine();
    running(&mut machine, false);
    assert!(step(&mut machine, Input::Unwait { call: 1 }).is_empty());
    let effects = step(&mut machine, completed());
    assert!(!has(&effects, "wake"));
    assert!(has(&effects, "report-end"));
}

#[test]
fn a_refused_option_starts_no_turn() {
    let mut machine = machine();
    step(
        &mut machine,
        Input::Start {
            call: 1,
            spec: spec(false),
        },
    );
    step(
        &mut machine,
        Input::Launched {
            process: 1,
            result: opened(),
        },
    );
    let effects = step(
        &mut machine,
        Input::Configured {
            process: 1,
            result: Err("Unknown model".to_string()),
        },
    );
    assert!(matches!(
        &effects[..],
        [Effect::Started { call: 1, result: Err(error) }] if error == "Unknown model"
    ));
    assert_eq!(machine.view().activity.turns, 0);
    assert!(!machine.working());
    // The process stays for the next turn, which reuses it.
    let effects = step(
        &mut machine,
        Input::Start {
            call: 2,
            spec: spec(false),
        },
    );
    assert!(matches!(
        effects[..],
        [Effect::Configure { process: 1, .. }]
    ));
}

#[test]
fn a_failed_launch_leaves_the_agent_idle() {
    let mut machine = machine();
    step(
        &mut machine,
        Input::Start {
            call: 1,
            spec: spec(false),
        },
    );
    let effects = step(
        &mut machine,
        Input::Launched {
            process: 1,
            result: Err("grok is not installed".to_string()),
        },
    );
    assert!(matches!(
        effects[..],
        [Effect::Started {
            call: 1,
            result: Err(_)
        }]
    ));
    assert!(!machine.working());
}

#[test]
fn a_second_start_while_working_is_refused() {
    let mut machine = machine();
    step(
        &mut machine,
        Input::Start {
            call: 1,
            spec: spec(false),
        },
    );
    let effects = step(
        &mut machine,
        Input::Start {
            call: 2,
            spec: spec(false),
        },
    );
    assert!(matches!(
        &effects[..],
        [Effect::Started { call: 2, result: Err(error) }] if error.contains("still working")
    ));
    assert!(machine.is_starting(1));
}

#[test]
fn another_mode_replaces_the_process_and_resumes_the_thread() {
    let mut machine = machine();
    running(&mut machine, false);
    step(&mut machine, completed());
    let mut full_access = spec(false);
    full_access.mode = AgentPermissionMode::FullAccess;
    full_access.mode_args = vec!["--always-approve".to_string()];
    let effects = step(
        &mut machine,
        Input::Start {
            call: 2,
            spec: full_access,
        },
    );
    assert!(matches!(
        &effects[..],
        [Effect::Launch { process: 2, replace: Some(1), thread_id: Some(thread), .. }]
            if thread == "thread-1"
    ));
}

#[test]
fn stop_waits_for_the_prompt_so_the_interrupt_cannot_overtake_it() {
    let mut machine = machine();
    prompting(&mut machine, false);
    let effects = step(&mut machine, Input::Stop { call: 2 });
    assert_eq!(kinds(&effects), ["settle"], "nothing goes to the agent yet");
    let effects = step(
        &mut machine,
        Input::Prompted {
            turn: 1,
            result: Ok(()),
        },
    );
    assert_eq!(kinds(&effects), ["started", "interrupt"]);
}

#[test]
fn stop_interrupts_then_takes_the_process_once_the_turn_ends() {
    let mut machine = machine();
    running(&mut machine, false);
    let effects = step(&mut machine, Input::Stop { call: 2 });
    assert!(matches!(
        effects[..],
        [Effect::Interrupt { process: 1, .. }]
    ));
    let effects = step(&mut machine, Input::Interrupted { process: 1 });
    assert_eq!(kinds(&effects), ["settle"]);
    let effects = step(
        &mut machine,
        Input::Event(AgentEvent::TurnCompleted {
            outcome: TurnOutcome::Cancelled,
            error: None,
        }),
    );
    assert_eq!(count(&effects, "record"), 1);
    assert_eq!(count(&effects, "finished"), 1);
    assert!(matches!(
        effects.last(),
        Some(Effect::Kill {
            process: 1,
            flush: false
        })
    ));
    assert!(machine.working(), "Stop is not done until the process is");
    let effects = step(&mut machine, Input::Killed { process: 1 });
    assert!(matches!(
        &effects[..],
        [Effect::Stopped { calls }] if calls == &[2]
    ));
    assert_eq!(machine.view().activity.status, "cancelled");
    assert!(!machine.working());
}

#[test]
fn a_stop_that_lands_as_the_turn_ends_cannot_cut_the_end_short() {
    let mut machine = machine();
    running(&mut machine, false);
    // The end comes first: it is taken in whole, record and all.
    let ended = step(&mut machine, completed());
    assert_eq!(count(&ended, "record"), 1);
    assert_eq!(count(&ended, "finished"), 1);
    // Stop then finds nothing to stop.
    let effects = step(&mut machine, Input::Stop { call: 2 });
    assert!(matches!(&effects[..], [Effect::Stopped { calls }] if calls == &[2]));
    assert_eq!(machine.view().activity.status, "completed");
}

#[test]
fn an_agent_that_never_ends_its_turn_is_killed_and_the_turn_cancelled() {
    let mut machine = machine();
    running(&mut machine, false);
    step(&mut machine, Input::Stop { call: 2 });
    step(&mut machine, Input::Interrupted { process: 1 });
    let effects = step(&mut machine, Input::SettleExpired { timer: 1 });
    assert_eq!(kinds(&effects), ["kill"]);
    assert!(
        machine.view().turn.is_some(),
        "the turn ends once its process is gone"
    );
    let effects = step(&mut machine, Input::Killed { process: 1 });
    assert_eq!(
        kinds(&effects),
        ["turn-ended", "row", "record", "wake", "finished", "stopped"]
    );
    assert_eq!(machine.view().activity.status, "cancelled");
}

#[test]
fn a_settle_timer_from_an_earlier_stop_is_ignored() {
    let mut machine = machine();
    prompting(&mut machine, false);
    step(&mut machine, Input::Stop { call: 2 });
    step(
        &mut machine,
        Input::Prompted {
            turn: 1,
            result: Ok(()),
        },
    );
    step(&mut machine, Input::Interrupted { process: 1 });
    // Timer 1 guarded the prompt; timer 2 is the settle that counts.
    assert!(step(&mut machine, Input::SettleExpired { timer: 1 }).is_empty());
    assert_eq!(
        kinds(&step(&mut machine, Input::SettleExpired { timer: 2 })),
        ["kill"]
    );
}

#[test]
fn stop_while_launching_gives_the_launch_up() {
    let mut machine = machine();
    step(
        &mut machine,
        Input::Start {
            call: 1,
            spec: spec(false),
        },
    );
    let effects = step(&mut machine, Input::Stop { call: 2 });
    assert!(matches!(
        &effects[..],
        [
            Effect::Started { call: 1, result: Err(error) },
            Effect::Abort { process: 1 },
            Effect::Stopped { calls },
        ] if error == LAUNCH_CANCELLED && calls == &[2]
    ));
    assert!(!machine.working());
    // A launch that finished as it was given up goes too.
    let effects = step(
        &mut machine,
        Input::Launched {
            process: 1,
            result: opened(),
        },
    );
    assert!(matches!(effects[..], [Effect::Kill { process: 1, .. }]));
}

#[test]
fn stop_while_applying_options_takes_the_process() {
    let mut machine = machine();
    step(
        &mut machine,
        Input::Start {
            call: 1,
            spec: spec(false),
        },
    );
    step(
        &mut machine,
        Input::Launched {
            process: 1,
            result: opened(),
        },
    );
    let effects = step(&mut machine, Input::Stop { call: 2 });
    assert_eq!(kinds(&effects), ["started", "kill"]);
    // The options' late answer changes nothing.
    assert!(
        step(
            &mut machine,
            Input::Configured {
                process: 1,
                result: Ok(())
            }
        )
        .is_empty()
    );
    let effects = step(&mut machine, Input::Killed { process: 1 });
    assert_eq!(kinds(&effects), ["stopped"]);
    assert_eq!(machine.view().activity.turns, 0);
}

#[test]
fn shutting_down_an_idle_agent_ends_its_process_then_exits() {
    let mut machine = machine();
    running(&mut machine, false);
    step(&mut machine, completed());
    let effects = step(&mut machine, Input::Shutdown);
    assert_eq!(kinds(&effects), ["kill"]);
    let effects = step(&mut machine, Input::Killed { process: 1 });
    assert_eq!(kinds(&effects), ["exit"]);
    assert!(machine.view().gone);
    let effects = step(
        &mut machine,
        Input::Start {
            call: 9,
            spec: spec(false),
        },
    );
    assert!(matches!(
        &effects[..],
        [Effect::Started { result: Err(error), .. }] if error == SHUT_DOWN
    ));
}

#[test]
fn shutting_down_mid_turn_stops_it_and_records_nothing_new() {
    let mut machine = machine();
    running(&mut machine, true);
    let effects = step(&mut machine, Input::Shutdown);
    assert_eq!(kinds(&effects), ["interrupt"]);
    step(&mut machine, Input::Interrupted { process: 1 });
    step(&mut machine, Input::SettleExpired { timer: 1 });
    let effects = step(&mut machine, Input::Killed { process: 1 });
    assert!(
        !has(&effects, "record"),
        "the last record stands: cut short"
    );
    assert!(!has(&effects, "report-end"), "nobody is left to tell");
    assert_eq!(
        effects
            .last()
            .map(|effect| kinds(std::slice::from_ref(effect))[0]),
        Some("exit")
    );
}

#[test]
fn a_process_that_exits_mid_turn_fails_it() {
    let mut machine = machine();
    running(&mut machine, false);
    let effects = step(&mut machine, Input::Closed);
    assert!(matches!(effects[0], Effect::Kill { process: 1, .. }));
    assert!(has(&effects, "wake"));
    let activity = machine.view().activity;
    assert_eq!(activity.status, "failed");
    assert_eq!(
        activity.error.as_deref(),
        Some("The Grok Build process exited before the turn finished.")
    );
    // The next turn starts a new process on the same thread.
    let effects = step(
        &mut machine,
        Input::Start {
            call: 2,
            spec: spec(false),
        },
    );
    assert!(matches!(
        &effects[..],
        [Effect::Launch {
            process: 2,
            replace: None,
            thread_id: Some(_),
            ..
        }]
    ));
}

#[test]
fn a_prompt_that_cannot_be_sent_fails_the_turn() {
    let mut machine = machine();
    prompting(&mut machine, false);
    let effects = step(
        &mut machine,
        Input::Prompted {
            turn: 1,
            result: Err("broken pipe".to_string()),
        },
    );
    assert!(has(&effects, "turn-ended"));
    assert!(matches!(
        effects.last(),
        Some(Effect::Started {
            call: 1,
            result: Err(_)
        })
    ));
    assert_eq!(machine.view().activity.status, "failed");
}

fn approval(item_id: &str, command: &str) -> Input {
    let mut arguments = Map::new();
    arguments.insert("command".to_string(), json!(command));
    Input::Request {
        id: json!(item_id),
        request: AgentRequest::Approval {
            item_id: item_id.to_string(),
            tool_name: "grok execute".to_string(),
            arguments,
            prompt: None,
        },
    }
}

#[test]
fn an_approval_goes_to_the_gate_and_its_answer_back_to_the_agent() {
    let mut machine = machine();
    running(&mut machine, false);
    let effects = step(&mut machine, approval("call-1", "rm -rf build"));
    assert!(matches!(
        effects.last(),
        Some(Effect::Judge { process: 1, turn: 1, action, .. }) if action.contains("rm -rf build")
    ));
    assert!(machine.view().activity.pending_permission.is_some());
    let effects = step(
        &mut machine,
        Input::Judged {
            process: 1,
            turn: 1,
            id: json!("call-1"),
            item_id: "call-1".to_string(),
            action: "rm -rf build".to_string(),
            verdict: Ok(()),
        },
    );
    assert!(matches!(
        effects.last(),
        Some(Effect::Reply {
            reply: AgentReply::Decision(AgentPermissionDecision::AllowOnce),
            ..
        })
    ));
    assert!(machine.view().activity.pending_permission.is_none());
}

#[test]
fn a_refusal_is_listed_and_its_command_did_not_run() {
    let mut machine = machine();
    running(&mut machine, false);
    step(
        &mut machine,
        Input::Event(AgentEvent::ItemStarted(AgentItem::CommandExecution {
            id: "call-1".to_string(),
            command: "rm -rf build".to_string(),
            exit_code: None,
            status: None,
        })),
    );
    step(&mut machine, approval("call-1", "rm -rf build"));
    let effects = step(
        &mut machine,
        Input::Judged {
            process: 1,
            turn: 1,
            id: json!("call-1"),
            item_id: "call-1".to_string(),
            action: "rm -rf build".to_string(),
            verdict: Err(Refusal::Declined(Some("not that".to_string()))),
        },
    );
    assert!(matches!(
        effects.last(),
        Some(Effect::Reply {
            reply: AgentReply::Decision(AgentPermissionDecision::Cancel),
            ..
        })
    ));
    let activity = machine.view().activity;
    assert_eq!(
        activity.declined,
        ["rm -rf build: the user declined it and said: not that"]
    );
    assert_eq!(activity.commands[0].status, "failed");
}

#[test]
fn a_judgement_from_an_earlier_turn_only_answers_the_agent() {
    let mut machine = machine();
    running(&mut machine, true);
    step(&mut machine, approval("call-1", "make"));
    step(&mut machine, completed());
    step(
        &mut machine,
        Input::Start {
            call: 2,
            spec: spec(true),
        },
    );
    step(
        &mut machine,
        Input::Configured {
            process: 1,
            result: Ok(()),
        },
    );
    let effects = step(
        &mut machine,
        Input::Judged {
            process: 1,
            turn: 1,
            id: json!("call-1"),
            item_id: "call-1".to_string(),
            action: "make".to_string(),
            verdict: Err(Refusal::Declined(None)),
        },
    );
    assert_eq!(kinds(&effects), ["reply"]);
    assert!(machine.view().activity.declined.is_empty());
}

#[test]
fn two_approvals_at_once_are_each_answered() {
    let mut machine = machine();
    running(&mut machine, false);
    step(&mut machine, approval("a", "make"));
    step(&mut machine, approval("b", "make test"));
    for (id, verdict) in [("b", Err(Refusal::NobodyToAsk)), ("a", Ok(()))] {
        let allowed = verdict.is_ok();
        let effects = step(
            &mut machine,
            Input::Judged {
                process: 1,
                turn: 1,
                id: json!(id),
                item_id: id.to_string(),
                action: id.to_string(),
                verdict,
            },
        );
        let Some(Effect::Reply {
            id: replied,
            reply: AgentReply::Decision(decision),
            ..
        }) = effects.last()
        else {
            panic!("no reply for {id}");
        };
        assert_eq!(replied, &json!(id));
        assert_eq!(
            *decision == AgentPermissionDecision::AllowOnce,
            allowed,
            "{id}"
        );
    }
}

#[test]
fn an_agent_maple_does_not_set_the_mode_of_has_its_requests_accepted() {
    let mut machine = machine();
    let mut codex = spec(false);
    codex.gated = false;
    codex.mode_args.clear();
    step(
        &mut machine,
        Input::Start {
            call: 1,
            spec: codex,
        },
    );
    step(
        &mut machine,
        Input::Launched {
            process: 1,
            result: opened(),
        },
    );
    step(
        &mut machine,
        Input::Configured {
            process: 1,
            result: Ok(()),
        },
    );
    let effects = step(&mut machine, approval("call-1", "make"));
    assert!(matches!(
        &effects[..],
        [Effect::Reply {
            reply: AgentReply::Decision(AgentPermissionDecision::AllowOnce),
            ..
        }]
    ));
}

#[test]
fn a_request_with_no_turn_is_refused() {
    let mut machine = machine();
    running(&mut machine, false);
    step(&mut machine, completed());
    let effects = step(&mut machine, approval("late", "make"));
    assert!(matches!(
        &effects[..],
        [Effect::Reply {
            reply: AgentReply::Decision(AgentPermissionDecision::Cancel),
            ..
        }]
    ));
}

#[test]
fn an_agent_asking_to_change_its_own_mode_is_refused_and_told() {
    let mut machine = machine();
    running(&mut machine, false);
    let effects = step(
        &mut machine,
        Input::Request {
            id: json!(7),
            request: AgentRequest::ModeChange {
                item_id: "mode".to_string(),
            },
        },
    );
    assert!(matches!(
        effects.last(),
        Some(Effect::Reply {
            reply: AgentReply::Decision(AgentPermissionDecision::Cancel),
            ..
        })
    ));
    assert_eq!(machine.view().activity.declined.len(), 1);
}

fn question(id: &str) -> AgentQuestion {
    serde_json::from_value(json!({
        "id": id,
        "header": id,
        "question": format!("Which {id}?"),
        "options": [{"label": "red", "description": ""}, {"label": "blue", "description": ""}],
        "multiSelect": false,
    }))
    .unwrap()
}

fn asks(id: &str) -> Input {
    Input::Request {
        id: json!(id),
        request: AgentRequest::UserInput {
            questions: vec![question(id)],
        },
    }
}

#[test]
fn a_question_wakes_the_waiting_caller_and_its_answer_goes_back() {
    let mut machine = machine();
    running(&mut machine, false);
    let effects = step(&mut machine, asks("colour"));
    assert!(matches!(
        &effects[0],
        Effect::Wake { calls, why: Wake::Asked } if calls == &[1]
    ));
    assert!(!has(&effects, "report-question"));
    assert_eq!(machine.view().activity.status, "waiting");
    let effects = step(
        &mut machine,
        Input::Answer {
            call: 2,
            prompt: "blue".to_string(),
            answers: BTreeMap::new(),
            background: false,
        },
    );
    assert!(matches!(
        &effects[0],
        Effect::Reply { id, reply: AgentReply::Answers(answers), .. }
            if id == &json!("colour") && answers.contains("blue")
    ));
    assert!(matches!(
        effects.last(),
        Some(Effect::Answered {
            call: 2,
            result: Ok(Some(1))
        })
    ));
    // The answering caller now waits on the turn.
    let effects = step(&mut machine, completed());
    assert!(matches!(
        effects.iter().find(|effect| matches!(effect, Effect::Wake { .. })),
        Some(Effect::Wake { calls, why: Wake::Ended }) if calls == &[2]
    ));
}

#[test]
fn a_question_nobody_waits_for_reaches_the_model_and_questions_queue() {
    let mut machine = machine();
    running(&mut machine, true);
    let effects = step(&mut machine, asks("first"));
    assert!(has(&effects, "report-question"));
    // A second question waits for the first's answer.
    assert!(step(&mut machine, asks("second")).is_empty());
    let effects = step(
        &mut machine,
        Input::Answer {
            call: 2,
            prompt: "red".to_string(),
            answers: BTreeMap::new(),
            background: true,
        },
    );
    assert!(
        has(&effects, "report-question"),
        "the next question is asked"
    );
    assert_eq!(machine.view().activity.questions[0].id, "second");
    // The turn ends with the question open: the agent is answered empty.
    let effects = step(&mut machine, completed());
    assert!(matches!(
        &effects[0],
        Effect::Reply { id, reply: AgentReply::Answers(answer), .. }
            if id == &json!("second") && answer.is_empty()
    ));
}

#[test]
fn an_answer_with_no_question_starts_a_turn_instead() {
    let mut machine = machine();
    running(&mut machine, false);
    let effects = step(
        &mut machine,
        Input::Answer {
            call: 2,
            prompt: "more".to_string(),
            answers: BTreeMap::new(),
            background: false,
        },
    );
    assert!(matches!(
        effects[..],
        [Effect::Answered {
            call: 2,
            result: Ok(None)
        }]
    ));
}

#[test]
fn a_one_turn_process_finishes_before_its_turn_ends() {
    let mut machine = AgentMachine::new("claude", "Claude Code", "claude-1", true, Instant::now());
    running(&mut machine, false);
    let effects = step(&mut machine, completed());
    assert!(matches!(
        &effects[..],
        [Effect::Kill {
            process: 1,
            flush: true
        }]
    ));
    assert!(machine.view().turn.is_some());
    // Stop now only waits for it.
    assert!(step(&mut machine, Input::Stop { call: 2 }).is_empty());
    let effects = step(&mut machine, Input::Killed { process: 1 });
    assert_eq!(
        kinds(&effects),
        ["turn-ended", "row", "record", "wake", "finished", "stopped"]
    );
    assert_eq!(machine.view().activity.status, "completed");
    // The next turn resumes the thread in a new process.
    let effects = step(
        &mut machine,
        Input::Start {
            call: 3,
            spec: spec(false),
        },
    );
    assert!(matches!(
        &effects[..],
        [Effect::Launch {
            process: 2,
            replace: None,
            thread_id: Some(_),
            ..
        }]
    ));
}

#[test]
fn an_unconfirmed_thread_is_recorded_once_the_agent_names_it() {
    let mut machine = AgentMachine::new("claude", "Claude Code", "claude-1", true, Instant::now());
    step(
        &mut machine,
        Input::Start {
            call: 1,
            spec: spec(false),
        },
    );
    step(
        &mut machine,
        Input::Launched {
            process: 1,
            result: Ok(Opened {
                thread_id: "provisional".to_string(),
                confirmed: false,
            }),
        },
    );
    assert!(machine.view().activity.thread_id.is_none());
    step(
        &mut machine,
        Input::Configured {
            process: 1,
            result: Ok(()),
        },
    );
    let named = AgentEvent::ThreadStarted {
        thread_id: "provisional".to_string(),
    };
    let effects = step(&mut machine, Input::Event(named.clone()));
    assert_eq!(kinds(&effects), ["record"]);
    assert!(step(&mut machine, Input::Event(named)).is_empty());
    assert_eq!(
        machine.view().activity.thread_id.as_deref(),
        Some("provisional")
    );
}

#[test]
fn streamed_text_is_throttled_but_structure_is_not() {
    let mut machine = machine();
    running(&mut machine, false);
    let now = Instant::now();
    let delta = |text: &str| {
        Input::Event(AgentEvent::AgentMessageDelta {
            item_id: "m".to_string(),
            delta: text.to_string(),
        })
    };
    // The turn's start just drew the row: text right after waits.
    assert!(machine.handle(delta("Hel"), now).is_empty());
    let later = now + LIVE_ROW_INTERVAL;
    assert_eq!(kinds(&machine.handle(delta("lo"), later)), ["row"]);
    assert_eq!(machine.view().activity.text, "Hello");
    let effects = machine.handle(
        Input::Event(AgentEvent::ItemStarted(AgentItem::CommandExecution {
            id: "c".to_string(),
            command: "ls".to_string(),
            exit_code: None,
            status: None,
        })),
        later,
    );
    assert_eq!(kinds(&effects), ["activity", "row"]);
}

#[test]
fn a_restored_agent_resumes_its_thread_on_its_next_turn() {
    let mut activity = machine().view().activity;
    activity.thread_id = Some("thread-9".to_string());
    activity.turns = 3;
    activity.status = "completed".to_string();
    let mut machine = AgentMachine::restored("Grok Build", false, Instant::now(), activity);
    assert!(!machine.working());
    let effects = step(
        &mut machine,
        Input::Start {
            call: 1,
            spec: spec(false),
        },
    );
    assert!(matches!(
        &effects[..],
        [Effect::Launch { thread_id: Some(thread), .. }] if thread == "thread-9"
    ));
    step(
        &mut machine,
        Input::Launched {
            process: 1,
            result: opened(),
        },
    );
    step(
        &mut machine,
        Input::Configured {
            process: 1,
            result: Ok(()),
        },
    );
    assert_eq!(machine.view().activity.turns, 4, "numbering goes on");
}
