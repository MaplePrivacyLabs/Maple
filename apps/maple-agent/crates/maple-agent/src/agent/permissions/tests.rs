use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use pi_ai::{Calibration, ClassifierModel, ClassifyFn};
use serde_json::json;

use super::*;
use crate::agent::provider::{MAPLE_PROVIDER, NoKeys, system_one_model};
use crate::agent::{AgentEventDispatcher, AgentEventSink, AgentServiceEvent};

const READ_ONLY: AgentPermissionMode = AgentPermissionMode::ReadOnly;
const AUTO: AgentPermissionMode = AgentPermissionMode::Auto;

fn choice_answer(options: &[(&str, f64)]) -> ClassifierAnswer {
    ClassifierAnswer::Choice {
        choice: options[0].0.to_string(),
        probabilities: options
            .iter()
            .map(|(option, probability)| (option.to_string(), *probability))
            .collect(),
        confidence: 0.5,
        calibration: Calibration::default(),
    }
}

fn bool_answer(probability: f64) -> ClassifierAnswer {
    ClassifierAnswer::Bool {
        probability,
        calibration: Calibration::default(),
    }
}

fn no_answers() -> ClassifierResult {
    ClassifierResult::empty(&system_one_model())
}

/// Answers to every question, sure of `hazard` and `effect`.
fn answers(hazard: &str, effect: &str) -> ClassifierResult {
    let mut result = no_answers();
    let answers = &mut result.answers;
    answers.insert(REVERSIBLE.to_string(), bool_answer(0.99));
    answers.insert(CONTAINED.to_string(), bool_answer(0.99));
    let mut hazards = vec![(hazard, 0.95)];
    hazards.extend(
        HAZARDS
            .iter()
            .filter(|(option, _)| *option != hazard)
            .map(|(option, _)| (*option, 0.05 / 8.0)),
    );
    answers.insert(HAZARD.to_string(), choice_answer(&hazards));
    let mut effects = vec![(effect, 0.95)];
    effects.extend(
        EFFECTS
            .iter()
            .filter(|(option, _)| *option != effect)
            .map(|(option, _)| (*option, 0.05 / 7.0)),
    );
    answers.insert(EFFECT.to_string(), choice_answer(&effects));
    result
}

#[test]
fn auto_runs_what_does_not_look_dangerous_and_names_what_it_stops() {
    assert_eq!(
        decide(AUTO, &answers(NO_HAZARD, "project_change")),
        Some(Verdict::Allow)
    );
    assert_eq!(
        decide(AUTO, &answers("push_or_publish", "sends_out")),
        Some(Verdict::Ask(AgentPermissionReason::SendsOut))
    );
    // Not sure it is safe is not the same as looking dangerous.
    let mut unsure = answers(NO_HAZARD, OBSERVE);
    unsure.answers[HAZARD] = choice_answer(&[
        (NO_HAZARD, 0.5),
        ("other_destructive", HAZARD_AT - 0.01),
        ("force_push", 0.11),
    ]);
    assert_eq!(decide(AUTO, &unsure), Some(Verdict::Allow));
    unsure.answers[HAZARD] = choice_answer(&[(NO_HAZARD, 0.6), ("other_destructive", HAZARD_AT)]);
    assert_eq!(
        decide(AUTO, &unsure),
        Some(Verdict::Ask(AgentPermissionReason::Destructive))
    );
    let mut lasting = answers(NO_HAZARD, "project_change");
    lasting.answers[REVERSIBLE] = bool_answer(REVERSIBLE_AT - 0.01);
    assert_eq!(
        decide(AUTO, &lasting),
        Some(Verdict::Ask(AgentPermissionReason::ChangesProject))
    );
}

#[test]
fn read_only_runs_reads_and_builds_and_asks_about_changes() {
    for effect in [OBSERVE, BUILD_OR_TEST] {
        assert_eq!(
            decide(READ_ONLY, &answers(NO_HAZARD, effect)),
            Some(Verdict::Allow)
        );
    }
    assert_eq!(
        decide(READ_ONLY, &answers(NO_HAZARD, "project_change")),
        Some(Verdict::Ask(AgentPermissionReason::ChangesProject))
    );
    assert_eq!(
        decide(READ_ONLY, &answers("credential_access", OBSERVE)),
        Some(Verdict::Ask(AgentPermissionReason::Secrets))
    );
    // Each answer counts alone, and read and build count together.
    let mut split = answers(NO_HAZARD, OBSERVE);
    split.answers[EFFECT] =
        choice_answer(&[(OBSERVE, 0.45), (BUILD_OR_TEST, 0.45), ("secrets", 0.1)]);
    assert_eq!(decide(READ_ONLY, &split), Some(Verdict::Allow));
    split.answers[CONTAINED] = bool_answer(READ_ONLY_AT - 0.01);
    assert_eq!(
        decide(READ_ONLY, &split),
        Some(Verdict::Ask(AgentPermissionReason::Uncertain))
    );
}

#[test]
fn a_missing_answer_is_no_verdict() {
    assert_eq!(decide(AUTO, &no_answers()), None);
    let mut partial = answers(NO_HAZARD, OBSERVE);
    partial.answers.shift_remove(EFFECT);
    assert_eq!(decide(READ_ONLY, &partial), None);
    let mut partial = answers(NO_HAZARD, OBSERVE);
    partial.answers.shift_remove(REVERSIBLE);
    assert_eq!(decide(AUTO, &partial), None);
    let mut wrong_kind = answers(NO_HAZARD, OBSERVE);
    wrong_kind.answers[HAZARD] = bool_answer(1.0);
    assert_eq!(decide(AUTO, &wrong_kind), None);
}

#[test]
fn the_classifier_sees_the_call_and_nothing_else() {
    let context = classifier_context(
        "bash",
        Some("Runs a command."),
        &json!({"command": "ls"}),
        "/work",
    )
    .unwrap();
    assert_eq!(
        context.state,
        json!({
            "tool": "bash",
            "tool_description": "Runs a command.",
            "input": {"command": "ls"},
            "cwd": "/work",
            "os": std::env::consts::OS,
        })
    );
    assert!(context.images.is_empty());
    assert_eq!(
        context.questions.keys().collect::<Vec<_>>(),
        [REVERSIBLE, CONTAINED, HAZARD, EFFECT]
    );
}

#[test]
fn images_and_encoded_data_never_reach_the_classifier() {
    let base64 = "QUJD".repeat(512);
    let input = json!({
        "image": "data:image/png;base64,iVBORw0KGgo=",
        "nested": [{"data": base64}],
        "command": "ls",
    });
    let context = classifier_context("mcp__draw", None, &input, "/work").unwrap();
    assert_eq!(
        context.state["input"],
        json!({
            "image": OMITTED_DATA,
            "nested": [{"data": OMITTED_DATA}],
            "command": "ls",
        })
    );
    // A long command is read, however plain its words.
    let long_words = json!({"command": "a ".repeat(4_000)});
    let context = classifier_context("bash", None, &long_words, "/work").unwrap();
    assert_eq!(context.state["input"], long_words);
}

#[test]
fn a_call_too_long_to_show_gets_no_context() {
    let input = json!({"command": "a ".repeat(MAX_INPUT_BYTES)});
    assert!(classifier_context("bash", None, &input, "/work").is_none());
}

#[test]
fn only_the_allow_label_allows() {
    let card =
        |answers: Value| json!({"answers": {"permission": {"answers": answers}}}).to_string();
    assert_eq!(read_answer(&card(json!(["Allow once"]))), Ok(()));
    assert_eq!(
        read_answer(&card(json!(["Allow once", "Additional note: go on"]))),
        Ok(())
    );
    assert_eq!(read_answer("Allow once"), Ok(()));
    assert_eq!(read_answer(&card(json!(["Deny"]))), Err(DENIED.to_string()));
    assert_eq!(read_answer(""), Err(DENIED.to_string()));
    assert_eq!(
        read_answer(&card(json!(["(no answer provided)"]))),
        Err(DENIED.to_string())
    );
    let noted = read_answer(&card(json!([
        "Deny",
        "Additional note: use the staging DB"
    ])));
    assert_eq!(
        noted,
        Err(format!("{DENIED} The user said: use the staging DB"))
    );
    let typed = read_answer(&card(json!(["run the tests first"])));
    assert_eq!(
        typed,
        Err(format!("{DENIED} The user said: run the tests first"))
    );
    // Words near the label do not count, in either kind of answer.
    assert!(read_answer("allow").is_err());
    assert!(read_answer("Allow once, but not really").is_err());
}

#[test]
fn the_card_shows_the_command_keeping_both_ends_of_a_long_one() {
    assert_eq!(
        card_action("bash", &json!({"command": "git push"})),
        "git push"
    );
    assert_eq!(
        card_action("write", &json!({"path": "a.txt", "content": "x"})),
        "a.txt"
    );
    assert_eq!(
        card_action("linear__get_issue", &json!({"id": "ENG-1"})),
        "linear__get_issue {\"id\":\"ENG-1\"}"
    );
    let long = format!("{}rm -rf /", "x".repeat(10_000));
    let shown = card_action("bash", &json!({"command": long}));
    assert!(shown.ends_with("rm -rf /"));
    assert!(shown.chars().count() < MAX_CARD_ACTION_CHARS + 64);
}

/// A model's context, as it was asked.
type Asked = (String, ClassifierContext);

/// Classifies from a script, one result per request, and records the models
/// and contexts it was given.
#[derive(Default)]
struct Scripted {
    replies: Mutex<VecDeque<Reply>>,
    asked: Mutex<Vec<Asked>>,
}

enum Reply {
    Answers(Box<ClassifierResult>),
    Fails,
}

#[async_trait]
impl ClassifyFn for Scripted {
    async fn classify(
        &self,
        model: &ClassifierModel,
        context: ClassifierContext,
        _options: ClassifierOptions,
    ) -> ClassifierResult {
        self.asked.lock().unwrap().push((model.id.clone(), context));
        match self.replies.lock().unwrap().pop_front() {
            Some(Reply::Answers(result)) => *result,
            _ => ClassifierResult::failed(model, ClassifierStopReason::Error, "nope"),
        }
    }
}

fn reply(result: ClassifierResult) -> Reply {
    Reply::Answers(Box::new(result))
}

/// Answers every question it sees with `answer`, and records them.
#[derive(Default)]
struct Answering {
    broker: Mutex<Option<QuestionBroker>>,
    answer: String,
    shown: Mutex<Vec<AgentQuestion>>,
}

impl AgentEventSink for Answering {
    fn emit(&self, event: &AgentServiceEvent) {
        if let AgentServiceEvent::Question {
            request_id,
            questions,
            ..
        } = event
        {
            self.shown.lock().unwrap().extend(questions.iter().cloned());
            let broker = self.broker.lock().unwrap().clone().unwrap();
            let (id, answer) = (request_id.clone(), self.answer.clone());
            tokio::spawn(async move { broker.answer(&id, answer).await });
        }
    }
}

fn gate(
    mode: AgentPermissionMode,
    replies: Vec<Reply>,
    answer: &str,
) -> (PermissionGate, Arc<Scripted>, Arc<Answering>) {
    let classifier = Arc::new(Scripted {
        replies: Mutex::new(replies.into()),
        ..Scripted::default()
    });
    let models = ModelRegistry::new(Arc::new(NoKeys));
    models.register_classifier_api(pi_ai::system_one::API, classifier.clone());
    let mut decision = system_one_model();
    decision.id = CLASSIFIER_MODEL.to_string();
    models.register_classifier_models([decision, system_one_model()]);
    let sink = Arc::new(Answering {
        answer: answer.to_string(),
        ..Answering::default()
    });
    let questions = QuestionBroker::new(AgentEventDispatcher::new(sink.clone()));
    *sink.broker.lock().unwrap() = Some(questions.clone());
    (
        PermissionGate {
            session_id: "task".to_string(),
            mode: TaskPermissions::new(mode),
            models,
            questions,
        },
        classifier,
        sink,
    )
}

fn bash(command: &str) -> ToolCall {
    ToolCall {
        tool_call_id: "call".to_string(),
        tool_name: "bash".to_string(),
        input: json!({ "command": command }),
    }
}

async fn check(gate: &PermissionGate, call: &ToolCall) -> Option<BeforeToolCallResult> {
    gate.check(call, None, "/work", &CancellationToken::new())
        .await
}

const CARD_ALLOW: &str = r#"{"answers":{"permission":{"answers":["Allow once"]}}}"#;
const CARD_DENY: &str = r#"{"answers":{"permission":{"answers":["Deny"]}}}"#;

#[tokio::test]
async fn full_access_and_user_facing_tools_never_reach_the_classifier() {
    let (full, classifier, _) = gate(AgentPermissionMode::FullAccess, Vec::new(), CARD_DENY);
    assert_eq!(check(&full, &bash("rm -rf /")).await, None);
    let (auto, _, _) = gate(AUTO, Vec::new(), CARD_DENY);
    for tool_name in USER_FACING_TOOLS {
        let call = ToolCall {
            tool_call_id: "call".to_string(),
            tool_name: tool_name.to_string(),
            input: json!({}),
        };
        assert_eq!(check(&auto, &call).await, None);
    }
    assert!(classifier.asked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_cleared_call_runs_without_a_card() {
    let (gate, classifier, sink) = gate(AUTO, vec![reply(answers(NO_HAZARD, OBSERVE))], CARD_DENY);
    assert_eq!(check(&gate, &bash("ls")).await, None);
    assert!(sink.shown.lock().unwrap().is_empty());
    let asked = classifier.asked.lock().unwrap();
    assert_eq!(asked[0].0, CLASSIFIER_MODEL);
    assert!(asked[0].1.images.is_empty());
}

#[tokio::test]
async fn the_user_decides_what_the_classifier_does_not_clear() {
    let hazard = || vec![reply(answers("force_push", "destructive"))];
    let (allowing, _, sink) = gate(AUTO, hazard(), CARD_ALLOW);
    assert_eq!(check(&allowing, &bash("git push -f")).await, None);
    {
        let shown = sink.shown.lock().unwrap();
        let prompt = shown[0].permission.as_ref().unwrap();
        assert_eq!(prompt.reason, AgentPermissionReason::RewritesHistory);
        assert_eq!(prompt.action, "git push -f");
        assert_eq!(prompt.tool, "bash");
    }

    let (denying, _, _) = gate(AUTO, hazard(), CARD_DENY);
    let decision = check(&denying, &bash("git push -f")).await.unwrap();
    assert!(decision.block);
    assert_eq!(decision.reason.as_deref(), Some(DENIED));
    assert!(!decision.terminate);
}

#[tokio::test]
async fn glm_answers_when_the_decision_model_fails_for_any_reason() {
    let (failing, classifier, _) = gate(
        AUTO,
        vec![Reply::Fails, reply(answers(NO_HAZARD, OBSERVE))],
        CARD_DENY,
    );
    assert_eq!(check(&failing, &bash("ls")).await, None);
    let asked: Vec<String> = classifier
        .asked
        .lock()
        .unwrap()
        .iter()
        .map(|(model, _)| model.clone())
        .collect();
    assert_eq!(asked, [CLASSIFIER_MODEL, FALLBACK_CLASSIFIER_MODEL]);

    let (unreadable, classifier, _) = gate(
        AUTO,
        vec![reply(no_answers()), reply(answers(NO_HAZARD, OBSERVE))],
        CARD_DENY,
    );
    assert_eq!(check(&unreadable, &bash("ls")).await, None);
    assert_eq!(classifier.asked.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn no_classifier_answer_asks_and_never_allows() {
    let (denying, _classifier, sink) = gate(AUTO, vec![Reply::Fails, Reply::Fails], CARD_DENY);
    let decision = check(&denying, &bash("ls")).await.unwrap();
    assert!(decision.block);
    assert_eq!(
        sink.shown.lock().unwrap()[0]
            .permission
            .as_ref()
            .unwrap()
            .reason,
        AgentPermissionReason::Unavailable
    );
}

#[tokio::test]
async fn a_call_too_long_to_show_is_asked_about_without_a_classifier() {
    let (denying, classifier, sink) = gate(AUTO, Vec::new(), CARD_DENY);
    let long = bash(&"a ".repeat(MAX_INPUT_BYTES));
    assert!(check(&denying, &long).await.unwrap().block);
    assert!(classifier.asked.lock().unwrap().is_empty());
    assert_eq!(
        sink.shown.lock().unwrap()[0]
            .permission
            .as_ref()
            .unwrap()
            .reason,
        AgentPermissionReason::TooLong
    );
}

#[tokio::test]
async fn with_nobody_to_ask_the_call_is_blocked() {
    let (gate, _, _) = gate(
        AUTO,
        vec![reply(answers("force_push", "destructive"))],
        CARD_ALLOW,
    );
    gate.questions.manage_availability();
    let decision = check(&gate, &bash("git push -f")).await.unwrap();
    assert!(decision.block);
    assert_eq!(decision.reason.as_deref(), Some(NOBODY_TO_ASK));
}

#[tokio::test]
async fn stopping_the_run_blocks_a_call_waiting_on_the_card() {
    let classifier = Arc::new(Scripted {
        replies: Mutex::new(vec![reply(answers("force_push", "destructive"))].into()),
        ..Scripted::default()
    });
    let models = ModelRegistry::new(Arc::new(NoKeys));
    models.register_classifier_api(pi_ai::system_one::API, classifier);
    let mut decision = system_one_model();
    decision.id = CLASSIFIER_MODEL.to_string();
    models.register_classifier_models([decision]);
    struct Silent;
    impl AgentEventSink for Silent {
        fn emit(&self, _event: &AgentServiceEvent) {}
    }
    let questions = QuestionBroker::new(AgentEventDispatcher::new(Arc::new(Silent)));
    let gate = PermissionGate {
        session_id: "task".to_string(),
        mode: TaskPermissions::new(AUTO),
        models,
        questions: questions.clone(),
    };
    let cancel = CancellationToken::new();
    let waiting = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            gate.check(&bash("git push -f"), None, "/work", &cancel)
                .await
        })
    };
    while questions.pending_count().await == 0 {
        tokio::task::yield_now().await;
    }
    cancel.cancel();
    let decision = waiting.await.unwrap().unwrap();
    assert!(decision.block);
    assert_eq!(decision.reason.as_deref(), Some(STOPPED));
    assert_eq!(questions.pending_count().await, 0);
}

#[tokio::test]
async fn a_new_mode_applies_to_the_next_call() {
    let (gate, classifier, _) = gate(AgentPermissionMode::FullAccess, Vec::new(), CARD_DENY);
    assert_eq!(check(&gate, &bash("ls")).await, None);
    gate.mode.set(AUTO);
    assert!(check(&gate, &bash("ls")).await.unwrap().block);
    assert_eq!(gate.mode.get(), AUTO);
    assert!(!classifier.asked.lock().unwrap().is_empty());
}

#[test]
fn saved_modes_read_back_and_old_tasks_keep_full_access() {
    for mode in [READ_ONLY, AUTO, AgentPermissionMode::FullAccess] {
        assert_eq!(AgentPermissionMode::from_saved(Some(mode.as_str())), mode);
    }
    assert_eq!(
        AgentPermissionMode::from_saved(None),
        AgentPermissionMode::FullAccess
    );
    assert_eq!(AgentPermissionMode::from_saved(Some("nonsense")), AUTO);
}

#[test]
fn the_maple_provider_serves_both_classifiers() {
    assert_eq!(system_one_model().provider, MAPLE_PROVIDER);
    assert_eq!(FALLBACK_CLASSIFIER_MODEL, system_one_model().id);
}
