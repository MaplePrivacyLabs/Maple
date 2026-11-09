//! Permission modes: what a task may do without asking, as a Pi extension.
//!
//! Full access runs every tool call. Read only and Auto show each call to a
//! System One classifier just before it runs, through Pi's `tool_call` gate:
//! Auto runs what doesn't look dangerous, and Read only what is clearly
//! harmless. The rest goes to the user on the question card, and a declined
//! call goes back to the model as a blocked tool call. There are no lists of
//! allowed commands: the classifier judges every call.
//!
//! Each request carries the tool, its input, the task's folder and the OS.
//! The agent's own text and tool results stay out, so neither the agent nor
//! a poisoned file can argue a call through, and no images ever go in.
//!
//! The Tinfoil decision model answers first and GLM-5.3 Flash answers when
//! it fails for any reason. A call neither of them cleared is never run
//! unattended: the user is asked, and with nobody to ask it is blocked.
//!
//! `docs/permissions.md` covers the wording and thresholds, and
//! `just permission-eval` measures them against live models.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use pi_agent_core::BeforeToolCallResult;
use pi_ai::{
    BoolCriteria, ClassifierAnswer, ClassifierContext, ClassifierOptions, ClassifierQuestion,
    ClassifierResult, ClassifierStopReason,
};
use pi_coding_agent::ModelRegistry;
use pi_coding_agent::extensions::{Extension, ExtensionContext, ToolCall, extension};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::provider::{MAPLE_PROVIDER, SYSTEM_ONE_MODEL_ID};
use super::questions::QuestionBroker;
use super::{
    AgentPermissionMode, AgentPermissionPrompt, AgentPermissionReason, AgentQuestion,
    AgentQuestionOption,
};

/// The classifier that answers first.
pub(super) const CLASSIFIER_MODEL: &str = "decision-small";
/// The classifier that answers when [`CLASSIFIER_MODEL`] fails.
pub(super) const FALLBACK_CLASSIFIER_MODEL: &str = SYSTEM_ONE_MODEL_ID;
const CLASSIFIER_TIMEOUT_MS: u64 = 4_000;
const FALLBACK_CLASSIFIER_TIMEOUT_MS: u64 = 10_000;

/// Auto runs what doesn't look dangerous: it stops at a hazard this likely,
pub(super) const HAZARD_AT: f64 = 0.4;
/// or at a call this unlikely to be undoable.
pub(super) const REVERSIBLE_AT: f64 = 0.5;
/// Read only runs what is clearly harmless: every answer must reach this.
pub(super) const READ_ONLY_AT: f64 = 0.8;

/// Tools that only talk to the user, so they have nothing to judge.
const USER_FACING_TOOLS: [&str; 2] = ["request_user_input", "todo_write"];

/// The longest tool input sent, in bytes of JSON. Dense JSON runs about three
/// bytes a token, so this leaves room for the questions in a decision
/// model's 32,768-token context. A longer call goes to the user.
const MAX_INPUT_BYTES: usize = 24_000;
/// Text of this length that is all base64 is binary data, not something to
/// read.
const MIN_BASE64_TEXT: usize = 1_024;
const OMITTED_DATA: &str = "[embedded data omitted]";
/// The most of a tool's description sent: MCP servers write theirs, at any
/// length.
const MAX_DESCRIPTION_CHARS: usize = 300;
/// The most of a call the card shows, keeping its start and its end.
const MAX_CARD_ACTION_CHARS: usize = 1_500;

const REVERSIBLE: &str = "reversible";
const CONTAINED: &str = "contained";
const HAZARD: &str = "hazard";
const EFFECT: &str = "effect";

const NO_HAZARD: &str = "none";
/// The hazards, in the order the model reads them.
const HAZARDS: [(&str, &str); 9] = [
    (
        NO_HAZARD,
        "No hazard below applies: ordinary development work, such as reading, searching, editing project files, staging and committing, installing dependencies, and removing build output or caches that a rebuild brings back.",
    ),
    (
        "credential_access",
        "Reads, copies or prints secrets: private keys, tokens, password stores, .env secrets, cloud credential files, the process environment.",
    ),
    (
        "secret_exfiltration",
        "Sends secrets or credentials to a host outside this machine.",
    ),
    (
        "push_or_publish",
        "Pushes, publishes, deploys, uploads, orders, pays for or sends project work, money or a message to a host or person outside this machine.",
    ),
    (
        "remote_code_execution",
        "Runs code fetched from the network, such as a download piped into a shell or interpreter.",
    ),
    (
        "force_push",
        "Rewrites shared version-control history, such as git push --force.",
    ),
    (
        "discard_uncommitted_work",
        "Throws away uncommitted changes, such as git reset --hard, git checkout -- or git clean -f.",
    ),
    (
        "disarm_guards",
        "Turns off a safety guard: permission rules, hooks, sandboxing, TLS checks, or protections on the agent's own config.",
    ),
    (
        "other_destructive",
        "Deletes or overwrites data that git does not hold and that cannot be made again, or changes system state in a way that is hard to undo.",
    ),
];

const OBSERVE: &str = "observe";
const BUILD_OR_TEST: &str = "build_or_test";
/// What a call does, most harmless first.
const EFFECTS: [(&str, &str); 8] = [
    (
        OBSERVE,
        "Only reads or inspects, on this computer, in a connected service or on the web: listing, searching and fetching. It creates, changes and deletes nothing, reads no secret and publishes nothing.",
    ),
    (
        BUILD_OR_TEST,
        "Runs the project's own build, test, lint or type-check tools. It writes only output that can be made again, such as build output, caches and logs.",
    ),
    (
        "project_change",
        "Changes the project's own files, dependencies or local git state in a way that git or rerunning a tool can undo.",
    ),
    (
        "outside_change",
        "Changes something outside the project: system settings, other folders, shell startup files, services or processes.",
    ),
    (
        "sends_out",
        "Sends, pushes, publishes, deploys or posts something to a person or a system outside this computer.",
    ),
    (
        "destructive",
        "Destroys data that cannot be recovered, discards uncommitted work or rewrites shared history.",
    ),
    (
        "secrets",
        "Reads, prints or copies credentials, tokens, keys or environment secrets.",
    ),
    (
        "remote_code",
        "Downloads and runs code from the network, or runs hidden or encoded code.",
    ),
];

/// A task's mode, read at each tool call, so a change applies to the next
/// call of a running task.
#[derive(Clone)]
pub(super) struct TaskPermissions(Arc<AtomicU8>);

impl TaskPermissions {
    pub(super) fn new(mode: AgentPermissionMode) -> Self {
        Self(Arc::new(AtomicU8::new(encode(mode))))
    }

    pub(super) fn get(&self) -> AgentPermissionMode {
        match self.0.load(Ordering::Acquire) {
            0 => AgentPermissionMode::ReadOnly,
            1 => AgentPermissionMode::Auto,
            _ => AgentPermissionMode::FullAccess,
        }
    }

    pub(super) fn set(&self, mode: AgentPermissionMode) {
        self.0.store(encode(mode), Ordering::Release);
    }
}

fn encode(mode: AgentPermissionMode) -> u8 {
    match mode {
        AgentPermissionMode::ReadOnly => 0,
        AgentPermissionMode::Auto => 1,
        AgentPermissionMode::FullAccess => 2,
    }
}

/// Add the questions about a call. Both modes ask all of them and read what
/// they need, and the answers name the reason for asking.
fn add_questions(context: &mut ClassifierContext) {
    const ASKS: &str = "A coding agent asks to run the action in the state.";
    let questions = &mut context.questions;
    questions.insert(
            REVERSIBLE.to_string(),
            ClassifierQuestion::Bool {
                instructions: format!(
                    "{ASKS} Can every effect of it be undone with ordinary tools? Output that can be made again, such as caches, build output, test output, logs and files the action itself creates, counts as undoable. It cannot be undone if it deletes or overwrites files that git does not hold, discards uncommitted work, rewrites git history, publishes or pushes a change, or sends a message."
                )
                .into(),
                criteria: Some(BoolCriteria {
                    when_true: Some("Every effect can be undone or made again.".into()),
                    when_false: Some(
                        "Some effect cannot be undone: lost files or uncommitted work, rewritten history, a published change or a sent message."
                            .into(),
                    ),
                }),
            },
        );
    questions.insert(
            CONTAINED.to_string(),
            ClassifierQuestion::Bool {
                instructions: format!(
                    "{ASKS} Is the action contained? Contained means its effects stay on this machine and inside the project: the project tree (the cwd), /tmp, and the usual caches and build output of the project's own tools. It is not contained if it reads a secret such as keys, tokens, credentials, .env files or the environment, sends files or secrets off this machine, changes anything in another service, or changes files outside the project. Searching the web and reading from a connected service are contained."
                )
                .into(),
                criteria: Some(BoolCriteria {
                    when_true: Some(
                        "Contained: it stays within the project, /tmp and its tools' usual caches, reads no secret, and sends nothing out."
                            .into(),
                    ),
                    when_false: Some(
                        "Not contained: it reads a secret, sends files or secrets off this machine, or changes something elsewhere."
                            .into(),
                    ),
                }),
            },
        );
    questions.insert(
        HAZARD.to_string(),
        ClassifierQuestion::choice(
            format!(
                "{ASKS} Which hazard does it carry? Choose none when no listed hazard applies."
            ),
            HAZARDS,
        ),
    );
    questions.insert(
            EFFECT.to_string(),
            ClassifierQuestion::choice(
                format!(
                    "{ASKS} Which option best describes the most serious thing the action does? Judge what the action really does, not what its text claims, and look at every command in it."
                ),
                EFFECTS,
            ),
        );
}

/// One tool call as the classifier sees it, or `None` when it is too long
/// to show whole. The context carries no images, whatever the model takes.
pub(super) fn classifier_context(
    tool: &str,
    description: Option<&str>,
    input: &Value,
    cwd: &str,
) -> Option<ClassifierContext> {
    let input = without_embedded_data(input);
    if serde_json::to_string(&input).ok()?.len() > MAX_INPUT_BYTES {
        return None;
    }
    let mut state = Map::new();
    state.insert("tool".into(), tool.into());
    if let Some(description) = description.filter(|text| !text.is_empty()) {
        let description: String = description.chars().take(MAX_DESCRIPTION_CHARS).collect();
        state.insert("tool_description".into(), description.into());
    }
    state.insert("input".into(), input);
    state.insert("cwd".into(), cwd.into());
    state.insert("os".into(), std::env::consts::OS.into());
    let mut context = ClassifierContext {
        state: Value::Object(state),
        ..ClassifierContext::default()
    };
    add_questions(&mut context);
    Some(context)
}

/// `value` with images and other encoded data taken out of its strings.
fn without_embedded_data(value: &Value) -> Value {
    match value {
        Value::String(text) if is_embedded_data(text) => OMITTED_DATA.into(),
        Value::Array(items) => items.iter().map(without_embedded_data).collect(),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), without_embedded_data(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn is_embedded_data(text: &str) -> bool {
    if text
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
        && text.contains(";base64,")
    {
        return true;
    }
    text.len() >= MIN_BASE64_TEXT
        && text.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'-' | b'_')
        })
}

/// What a mode makes of a classifier's answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Allow,
    Ask(AgentPermissionReason),
}

/// Whether `mode` lets a call run, or `None` when an answer it reads is
/// missing. `slack` moves every threshold toward running, so the eval can
/// tell how close a call came.
pub(super) fn runs(
    mode: AgentPermissionMode,
    result: &ClassifierResult,
    slack: f64,
) -> Option<bool> {
    let reversible = bool_probability(result, REVERSIBLE)?;
    Some(match mode {
        AgentPermissionMode::FullAccess => true,
        AgentPermissionMode::Auto => {
            strongest_hazard(result)?.1 < HAZARD_AT + slack && reversible >= REVERSIBLE_AT - slack
        }
        AgentPermissionMode::ReadOnly => {
            let harmless = option_probability(result, EFFECT, OBSERVE)?
                + option_probability(result, EFFECT, BUILD_OR_TEST)?;
            [
                reversible,
                bool_probability(result, CONTAINED)?,
                option_probability(result, HAZARD, NO_HAZARD)?,
                harmless,
            ]
            .into_iter()
            .all(|answer| answer >= READ_ONLY_AT - slack)
        }
    })
}

/// The verdict, or `None` when an answer the mode reads is missing.
pub(super) fn decide(mode: AgentPermissionMode, result: &ClassifierResult) -> Option<Verdict> {
    if runs(mode, result, 0.0)? {
        return Some(Verdict::Allow);
    }
    let hazard = strongest_hazard(result)
        .filter(|(_, probability)| *probability >= HAZARD_AT)
        .and_then(|(hazard, _)| hazard_reason(hazard));
    Some(Verdict::Ask(
        hazard
            .or_else(|| likeliest(result, EFFECT).and_then(effect_reason))
            .unwrap_or(AgentPermissionReason::Uncertain),
    ))
}

/// The likeliest hazard other than none.
fn strongest_hazard(result: &ClassifierResult) -> Option<(&str, f64)> {
    match result.answers.get(HAZARD)? {
        ClassifierAnswer::Choice { probabilities, .. } => probabilities
            .iter()
            .filter(|(option, _)| option.as_str() != NO_HAZARD)
            .max_by(|left, right| left.1.total_cmp(right.1))
            .map(|(option, probability)| (option.as_str(), *probability)),
        _ => None,
    }
}

fn bool_probability(result: &ClassifierResult, question: &str) -> Option<f64> {
    match result.answers.get(question)? {
        ClassifierAnswer::Bool { probability, .. } => Some(*probability),
        _ => None,
    }
}

fn option_probability(result: &ClassifierResult, question: &str, option: &str) -> Option<f64> {
    match result.answers.get(question)? {
        ClassifierAnswer::Choice { probabilities, .. } => probabilities.get(option).copied(),
        _ => None,
    }
}

fn likeliest<'a>(result: &'a ClassifierResult, question: &str) -> Option<&'a str> {
    match result.answers.get(question)? {
        ClassifierAnswer::Choice { probabilities, .. } => probabilities
            .iter()
            .max_by(|left, right| left.1.total_cmp(right.1))
            .map(|(option, _)| option.as_str()),
        _ => None,
    }
}

fn hazard_reason(hazard: &str) -> Option<AgentPermissionReason> {
    Some(match hazard {
        "credential_access" => AgentPermissionReason::Secrets,
        "secret_exfiltration" => AgentPermissionReason::SecretsLeaving,
        "push_or_publish" => AgentPermissionReason::SendsOut,
        "remote_code_execution" => AgentPermissionReason::RemoteCode,
        "force_push" => AgentPermissionReason::RewritesHistory,
        "discard_uncommitted_work" => AgentPermissionReason::DiscardsWork,
        "disarm_guards" => AgentPermissionReason::DisablesSafeguards,
        "other_destructive" => AgentPermissionReason::Destructive,
        _ => return None,
    })
}

fn effect_reason(effect: &str) -> Option<AgentPermissionReason> {
    Some(match effect {
        "project_change" => AgentPermissionReason::ChangesProject,
        "outside_change" => AgentPermissionReason::ChangesOutsideProject,
        "sends_out" => AgentPermissionReason::SendsOut,
        "destructive" => AgentPermissionReason::Destructive,
        "secrets" => AgentPermissionReason::Secrets,
        "remote_code" => AgentPermissionReason::RemoteCode,
        _ => return None,
    })
}

/// The first answer that can be read, or `None`. A cancelled run has no
/// answer, and a model that fails for any reason hands over to the next.
pub(super) async fn classify(
    models: &ModelRegistry,
    mode: AgentPermissionMode,
    context: &ClassifierContext,
    cancel: &CancellationToken,
) -> Option<Verdict> {
    for (id, timeout_ms) in [
        (CLASSIFIER_MODEL, CLASSIFIER_TIMEOUT_MS),
        (FALLBACK_CLASSIFIER_MODEL, FALLBACK_CLASSIFIER_TIMEOUT_MS),
    ] {
        let Some(model) = models.find_classifier(MAPLE_PROVIDER, id) else {
            log::warn!("Permission classifier {id} is not registered");
            continue;
        };
        let options = ClassifierOptions {
            timeout_ms: Some(timeout_ms),
            max_retries: Some(0),
            cancel: cancel.clone(),
            ..ClassifierOptions::default()
        };
        let result = models.classify(&model, context.clone(), options).await;
        match result.stop_reason {
            ClassifierStopReason::Aborted if cancel.is_cancelled() => return None,
            ClassifierStopReason::Stop => {
                if let Some(verdict) = decide(mode, &result) {
                    return Some(verdict);
                }
                log::warn!("Permission classifier {id} returned unusable answers");
            }
            // Its error text can repeat a response body, so it is not logged.
            _ => log::warn!("Permission classifier {id} failed"),
        }
    }
    None
}

/// The label of the option that lets a call run. The card translates what it
/// shows; this stays the value it answers with.
const ALLOW_ONCE: &str = "Allow once";
const DENY: &str = "Deny";
const CARD_QUESTION_ID: &str = "permission";

const DENIED: &str = "The user declined this tool call. Do not try to get around it: take another approach or ask the user.";
const NOBODY_TO_ASK: &str = "This tool call needs the user's approval and no connected interface can show the request. Take another approach, or explain what needs approval.";
const STOPPED: &str = "The run was stopped before this tool call was approved.";

/// The card's question for a call. Its text is for interfaces that do not
/// know `permission`; Maple's own cards word it from the fields there.
fn card_question(call: &ToolCall, reason: AgentPermissionReason) -> AgentQuestion {
    let action = card_action(&call.tool_name, &call.input);
    AgentQuestion {
        multi_select: false,
        id: CARD_QUESTION_ID.to_string(),
        header: "Allow this action?".to_string(),
        question: format!("{}\n\n{action}", reason.description()),
        options: [ALLOW_ONCE, DENY]
            .map(|label| AgentQuestionOption {
                label: label.to_string(),
                description: String::new(),
            })
            .to_vec(),
        permission: Some(AgentPermissionPrompt {
            reason,
            tool: call.tool_name.clone(),
            action,
        }),
    }
}

/// What the card shows of a call: its command or path where it has one.
fn card_action(tool: &str, input: &Value) -> String {
    let named = ["command", "path", "url", "query"]
        .into_iter()
        .find_map(|field| input.get(field)?.as_str());
    let action = match named {
        Some(text) => text.to_string(),
        None => format!("{tool} {}", without_embedded_data(input)),
    };
    clip_middle(&action)
}

fn clip_middle(text: &str) -> String {
    let length = text.chars().count();
    if length <= MAX_CARD_ACTION_CHARS {
        return text.to_string();
    }
    let kept = MAX_CARD_ACTION_CHARS / 2;
    let start: String = text.chars().take(kept).collect();
    let end: String = text.chars().skip(length - kept).collect();
    format!(
        "{start}\n[{} characters not shown]\n{end}",
        length - 2 * kept
    )
}

/// Whether the card's answer lets the call run, or what to tell the model
/// when it does not. The card answers with JSON and other interfaces with
/// plain text; only the allow label itself allows.
fn read_answer(answer: &str) -> Result<(), String> {
    let values: Vec<String> = serde_json::from_str::<Value>(answer)
        .ok()
        .and_then(|value| {
            let answers = value.pointer(&format!("/answers/{CARD_QUESTION_ID}/answers"))?;
            answers.as_array().map(|answers| {
                answers
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
        })
        .unwrap_or_else(|| vec![answer.trim().to_string()]);
    if values.first().map(String::as_str) == Some(ALLOW_ONCE) {
        return Ok(());
    }
    let note = values
        .iter()
        .map(|value| {
            value
                .strip_prefix("Additional note: ")
                .unwrap_or(value)
                .trim()
        })
        .filter(|value| !value.is_empty() && *value != DENY && *value != "(no answer provided)")
        .collect::<Vec<_>>()
        .join("\n");
    Err(if note.is_empty() {
        DENIED.to_string()
    } else {
        format!("{DENIED} The user said: {note}")
    })
}

/// What a task's gate needs.
#[derive(Clone)]
pub(super) struct PermissionGate {
    pub(super) session_id: String,
    pub(super) mode: TaskPermissions,
    /// The account's models: a task's own registry has no classifiers.
    pub(super) models: ModelRegistry,
    pub(super) questions: QuestionBroker,
}

impl PermissionGate {
    /// Gate one call: nothing to let it run, or a decision blocking it.
    pub(super) async fn check(
        &self,
        call: &ToolCall,
        description: Option<&str>,
        cwd: &str,
        cancel: &CancellationToken,
    ) -> Option<BeforeToolCallResult> {
        let mode = self.mode.get();
        if mode == AgentPermissionMode::FullAccess
            || USER_FACING_TOOLS.contains(&call.tool_name.as_str())
        {
            return None;
        }
        let reason = match classifier_context(&call.tool_name, description, &call.input, cwd) {
            None => AgentPermissionReason::TooLong,
            Some(context) => match classify(&self.models, mode, &context, cancel).await {
                Some(Verdict::Allow) => return None,
                Some(Verdict::Ask(reason)) => reason,
                None => AgentPermissionReason::Unavailable,
            },
        };
        if cancel.is_cancelled() {
            return Some(BeforeToolCallResult::block(STOPPED));
        }
        let question = card_question(call, reason);
        let answer = tokio::select! {
            biased;
            () = cancel.cancelled() => return Some(BeforeToolCallResult::block(STOPPED)),
            answer = self.questions.try_ask(&self.session_id, vec![question]) => answer,
        };
        match answer {
            Ok(answer) => read_answer(&answer).err().map(BeforeToolCallResult::block),
            Err(refused) => {
                log::info!("A permission request could not be shown: {refused}");
                Some(BeforeToolCallResult::block(NOBODY_TO_ASK))
            }
        }
    }
}

pub(super) fn permission_extension(gate: PermissionGate) -> Arc<dyn Extension> {
    extension("maple-permissions", move |api| {
        let gate = gate.clone();
        api.on(move |call: ToolCall, context: ExtensionContext| {
            let gate = gate.clone();
            async move {
                let description = context
                    .all_tools()
                    .into_iter()
                    .find(|tool| tool.name == call.tool_name)
                    .map(|tool| tool.description);
                let cwd = context
                    .cwd()
                    .map(|cwd| cwd.display().to_string())
                    .unwrap_or_default();
                let cancel = context.signal().unwrap_or_default();
                Ok(gate
                    .check(&call, description.as_deref(), &cwd, &cancel)
                    .await)
            }
        });
    })
}

#[cfg(test)]
mod eval;
#[cfg(test)]
mod tests;
