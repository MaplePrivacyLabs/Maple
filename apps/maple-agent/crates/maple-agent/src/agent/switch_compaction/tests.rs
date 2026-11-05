//! The extension's decisions, on a Pi session of its own with a scripted
//! provider: which compactions it leaves to Pi, which model summarizes the
//! others, and what the trimmed history keeps.

use std::sync::{Arc, Mutex};

use pi_ai::faux::{FauxProvider, FauxRequest, faux_message};
use pi_ai::{AssistantContent, Message, Model, UserMessage};
use pi_coding_agent::compaction::CompactionSettings;
use pi_coding_agent::session::SessionManager;
use pi_coding_agent::{AgentSession, AgentSessionOptions, ModelRegistry, StaticKeys};

use super::*;

const OLDEST: &str = "oldest-marker";
const KEPT: &str = "kept-marker";

fn model(id: &str, context_window: u64) -> Model {
    Model {
        id: id.to_string(),
        context_window,
        ..FauxProvider::default_model()
    }
}

fn big() -> Model {
    model("big", 200_000)
}

fn small() -> Model {
    model("small", 40_000)
}

/// A user message of about `tokens` tokens, as compaction estimates them.
fn user(marker: &str, tokens: usize) -> SessionMessage {
    SessionMessage::Llm(Message::User(UserMessage::text(format!(
        "{marker} {}",
        "x".repeat(tokens * 4)
    ))))
}

fn reply(model: &Model, text: &str) -> SessionMessage {
    let mut reply = faux_message(vec![AssistantContent::text(text)]);
    reply.provider = model.provider.clone();
    reply.model = model.id.clone();
    SessionMessage::Llm(Message::Assistant(reply))
}

/// A history the previous model wrote: its oldest message is about 30k
/// tokens, which the small window cannot summarize, and the newest 25k
/// stay after the cut.
fn history(writer: &Model) -> Vec<SessionMessage> {
    vec![
        user(OLDEST, 30_000),
        reply(writer, KEPT),
        user("recent", 25_000),
        reply(writer, "Recent answer."),
    ]
}

/// What the catalog offers: `offered`, whatever the id asked for.
fn offering(offered: Option<Model>) -> OfferedModel {
    Arc::new(move |_id| {
        let offered = offered.clone();
        Box::pin(async move { offered })
    })
}

async fn session_on(
    current: Model,
    history: Vec<SessionMessage>,
    offered: OfferedModel,
    faux: &FauxProvider,
) -> AgentSession {
    let registry = ModelRegistry::new(Arc::new(StaticKeys::default()));
    registry.register_api("faux", Arc::new(faux.clone()));
    let mut manager = SessionManager::in_memory("/project");
    for message in history {
        manager.append_message(message);
    }
    let mut options = AgentSessionOptions::new("/project", "Maple", manager, registry);
    options.model = Some(current);
    options.settings.retry.base_delay_ms = 1;
    options
        .extensions
        .push(switch_compaction_extension(offered));
    AgentSession::new(options).await.unwrap()
}

/// Compact the session by hand, as `/compact` does. Returns whether the
/// extension wrote the summary, and the summary requests in order.
async fn compact_now(session: &AgentSession, faux: &FauxProvider) -> (bool, Vec<FauxRequest>) {
    session.compact(None).await.unwrap();
    let from_extension = session.with_session(|manager| {
        manager
            .branch()
            .iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                EntryKind::Compaction { from_extension, .. } => Some(*from_extension),
                _ => None,
            })
    });
    (from_extension.expect("compacted"), faux.requests())
}

fn request_text(request: &FauxRequest) -> String {
    request
        .context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(pi_ai::content_text(&user.content)),
            _ => None,
        })
        .collect()
}

fn models(requests: &[FauxRequest]) -> Vec<&str> {
    requests
        .iter()
        .map(|request| request.model.id.as_str())
        .collect()
}

#[tokio::test]
async fn a_compaction_on_the_same_model_is_pis_own() {
    let faux = FauxProvider::new();
    faux.push_text("Summary.");
    let session = session_on(small(), history(&small()), offering(Some(big())), &faux).await;
    let (from_extension, requests) = compact_now(&session, &faux).await;
    assert!(!from_extension);
    assert_eq!(models(&requests), ["small"]);
}

#[tokio::test]
async fn a_switch_to_a_larger_window_is_pis_own() {
    let faux = FauxProvider::new();
    faux.push_text("Summary.");
    let session = session_on(big(), history(&small()), offering(Some(small())), &faux).await;
    let (from_extension, requests) = compact_now(&session, &faux).await;
    assert!(!from_extension);
    assert_eq!(models(&requests), ["big"]);
}

#[tokio::test]
async fn a_switch_to_a_smaller_window_is_summarized_by_the_previous_model() {
    let faux = FauxProvider::new();
    faux.push_text("Summary by the previous model.");
    let asked = Arc::new(Mutex::new(Vec::new()));
    let offered: OfferedModel = {
        let asked = Arc::clone(&asked);
        Arc::new(move |id| {
            asked.lock().unwrap().push(id);
            Box::pin(async { Some(big()) })
        })
    };
    let session = session_on(small(), history(&big()), offered, &faux).await;
    let (from_extension, requests) = compact_now(&session, &faux).await;
    assert!(from_extension);
    assert_eq!(*asked.lock().unwrap(), ["big"]);
    assert_eq!(models(&requests), ["big"]);
    assert!(request_text(&requests[0]).contains(OLDEST));
    let summary = session.with_session(|manager| {
        manager.branch().iter().find_map(|entry| match &entry.kind {
            EntryKind::Compaction { summary, .. } => Some(summary.clone()),
            _ => None,
        })
    });
    assert!(
        summary
            .unwrap()
            .starts_with("Summary by the previous model."),
        "the previous model's summary is recorded"
    );
}

#[tokio::test]
async fn the_new_model_summarizes_trimmed_history_when_the_previous_one_fails() {
    let faux = FauxProvider::new();
    faux.push_error("The Maple inference endpoint was not found");
    faux.push_text("Summary of what fits.");
    let session = session_on(small(), history(&big()), offering(Some(big())), &faux).await;
    let (from_extension, requests) = compact_now(&session, &faux).await;
    assert!(from_extension);
    assert_eq!(models(&requests), ["big", "small"]);
    let trimmed = request_text(&requests[1]);
    assert!(!trimmed.contains(OLDEST), "the oldest message is left out");
    assert!(trimmed.contains(KEPT), "newer messages stay");
}

#[tokio::test]
async fn the_new_model_summarizes_trimmed_history_when_the_previous_one_is_gone() {
    let faux = FauxProvider::new();
    faux.push_text("Summary of what fits.");
    let session = session_on(small(), history(&big()), offering(None), &faux).await;
    let (from_extension, requests) = compact_now(&session, &faux).await;
    assert!(from_extension);
    assert_eq!(models(&requests), ["small"]);
    assert!(!request_text(&requests[0]).contains(OLDEST));
}

/// A history the new model takes whole needs nothing from the extension.
#[tokio::test]
async fn pi_summarizes_when_the_new_model_takes_the_whole_history() {
    let faux = FauxProvider::new();
    faux.push_text("Pi's own summary.");
    let history = vec![
        user("short", 1_000),
        reply(&big(), KEPT),
        user("recent", 25_000),
        reply(&big(), "Recent answer."),
    ];
    let session = session_on(small(), history, offering(None), &faux).await;
    let (from_extension, requests) = compact_now(&session, &faux).await;
    assert!(!from_extension);
    assert_eq!(models(&requests), ["small"]);
}

#[tokio::test]
async fn pi_summarizes_when_both_attempts_fail() {
    let faux = FauxProvider::new();
    faux.push_error("The Maple inference endpoint was not found");
    faux.push_error("The Maple inference endpoint was not found");
    faux.push_text("Pi's own summary.");
    let session = session_on(small(), history(&big()), offering(Some(big())), &faux).await;
    let (from_extension, requests) = compact_now(&session, &faux).await;
    assert!(!from_extension);
    assert_eq!(models(&requests), ["big", "small", "small"]);
    assert!(
        request_text(&requests[2]).contains(OLDEST),
        "Pi sends it all"
    );
}

#[test]
fn the_switch_is_read_from_the_last_complete_reply() {
    let mut manager = SessionManager::in_memory("/project");
    let current = small();
    manager.append_message(user("first", 1));
    assert_eq!(switched_from(&branch(&manager), &current), None);
    manager.append_message(reply(&big(), "From the previous model."));
    assert_eq!(
        switched_from(&branch(&manager), &current).as_deref(),
        Some("big")
    );
    // A failed reply on the new model wrote nothing.
    let mut failed = faux_message(Vec::new());
    failed.provider = current.provider.clone();
    failed.model = current.id.clone();
    failed.stop_reason = pi_ai::StopReason::Error;
    manager.append_message(SessionMessage::Llm(Message::Assistant(failed)));
    assert_eq!(
        switched_from(&branch(&manager), &current).as_deref(),
        Some("big")
    );
    manager.append_message(reply(&current, "From the new model."));
    assert_eq!(switched_from(&branch(&manager), &current), None);
}

fn branch(manager: &SessionManager) -> Vec<SessionEntry> {
    manager.branch().into_iter().cloned().collect()
}

#[test]
fn trimmed_history_keeps_the_newest_messages_that_fit() {
    let preparation = CompactionPreparation {
        first_kept_entry_id: "kept".into(),
        messages_to_summarize: vec![
            user(OLDEST, 30_000),
            reply(&big(), KEPT),
            user("newer", 5_000),
        ],
        turn_prefix_messages: vec![user("prefix", 1_000)],
        tokens_before: 60_000,
        previous_summary: Some("y".repeat(4 * 1_000)),
        file_operations: Default::default(),
        settings: CompactionSettings::default(),
    };
    let window = small().context_window;
    let fitted = fit_preparation(&preparation, &small(), None).unwrap();
    assert_eq!(fitted.messages_to_summarize.len(), 2);
    assert_eq!(
        fitted.turn_prefix_messages,
        preparation.turn_prefix_messages
    );
    assert_eq!(fitted.first_kept_entry_id, "kept");
    assert_eq!(fitted.previous_summary, preparation.previous_summary);
    let request: u64 = fitted
        .messages_to_summarize
        .iter()
        .map(request_tokens)
        .sum::<u64>()
        + text_tokens(fitted.previous_summary.as_deref().unwrap())
        + PROMPT_TOKENS;
    assert!(
        request <= window - CompactionSettings::default().reserve_tokens,
        "{request}"
    );
    // Everything fits a window large enough.
    assert_eq!(fit_preparation(&preparation, &big(), None), None);
}
