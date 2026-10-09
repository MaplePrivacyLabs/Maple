//! The small requests Maple makes beside a task, as Goose's runtime made
//! them: a generated title for a new task, one-line summaries of tool calls
//! and thinking for the activity feed, and `/btw` side questions.
//!
//! Titles and summaries ask one fixed, cheap model a bounded question,
//! without thinking or tools. A side question asks the task's own model
//! with the task's context, so the provider can reuse its cached prefix,
//! and is told not to call tools; nothing it says is saved to the task.

use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use pi_ai::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, Message, Model, StopReason,
    StreamOptions, UserMessage,
};
use pi_coding_agent::ModelRegistry;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::provider::maple_model;
use super::tasks::session_title_from_prompt;
use super::{
    AgentEventDispatcher, AgentRuntimeHandle, AgentServiceEvent, SideQuestionEvent,
    SideQuestionTurn, emit_agent_event,
};

/// The model titles and summaries ask.
pub(super) const SIDE_MODEL: &str = "llama3-3-70b";

const TITLE_TIMEOUT: Duration = Duration::from_secs(10);
const TITLE_TEMPERATURE: f64 = 0.7;
const TITLE_MAX_TOKENS: u64 = 15;
const TITLE_MAX_INPUT_CHARS: usize = 500;
const TITLE_SYSTEM_PROMPT: &str = "You are a helpful assistant that generates concise, meaningful titles (3-5 words) for chat conversations based on the user's first message. Return only the title without quotes or explanations.";

const SUMMARY_TIMEOUT: Duration = Duration::from_secs(15);
const SUMMARY_TEMPERATURE: f64 = 0.2;
const SUMMARY_MAX_TOKENS: u64 = 48;
const SUMMARY_MAX_INPUT_CHARS: usize = 4000;
const SUMMARY_MAX_CHARS: usize = 100;
const TOOL_SUMMARY_SYSTEM_PROMPT: &str = "You summarize one tool call for a coding agent's activity feed. Reply with ONE short line of at most 12 words that says what the call did. No prefix, no quotes, no explanations.";
const THINKING_SUMMARY_SYSTEM_PROMPT: &str = "You summarize a coding agent's reasoning for its activity feed. Reply with ONE short line of at most 12 words that says what the agent thought about or decided. No prefix, no quotes, no explanations.";

const SIDE_QUESTION_TIMEOUT: Duration = Duration::from_secs(120);
/// The framing of a `/btw` question. It goes in the user message, not the
/// system prompt, so the request keeps the task's cached prefix.
const SIDE_QUESTION_PREFIX: &str = "The user asks a quick side question about the task so far. Answer it directly and briefly in plain prose. Do not call tools and do not continue the task; the task carries on separately and this exchange is not part of it.";

/// One bounded question to the side model.
struct SideRequest<'a> {
    system_prompt: &'a str,
    text: String,
    temperature: f64,
    max_tokens: u64,
    timeout: Duration,
}

/// The side model's answer, or why there is none. A timeout cancels the
/// request and waits for it to end, so nothing it started outlives it.
async fn ask_side_model(
    models: &ModelRegistry,
    session_id: &str,
    request: SideRequest<'_>,
    cancel: CancellationToken,
) -> Result<String, String> {
    let model = maple_model(SIDE_MODEL, None, None);
    let context = Context::new(
        request.system_prompt,
        Vec::new(),
        vec![Message::User(UserMessage::text(request.text))],
    );
    let options = StreamOptions {
        max_tokens: Some(request.max_tokens),
        temperature: Some(request.temperature),
        session_id: Some(session_id.to_string()),
        cancel: cancel.clone(),
        ..StreamOptions::default()
    };
    let answer = models.stream_fn().stream(&model, context, options).result();
    tokio::pin!(answer);
    let message = tokio::select! {
        message = &mut answer => message,
        () = tokio::time::sleep(request.timeout) => {
            cancel.cancel();
            let _ = answer.await;
            return Err("timed out".to_string());
        }
    };
    match message.stop_reason {
        StopReason::Error | StopReason::Aborted => Err(message
            .error_message
            .unwrap_or_else(|| "the request failed".to_string())),
        _ => Ok(message.text()),
    }
}

/// A title for a new task from its first prompt, or `None` when the model
/// gave nothing usable.
pub(super) async fn generate_title(
    models: &ModelRegistry,
    session_id: &str,
    first_prompt: &str,
    cancel: CancellationToken,
) -> Result<Option<String>, String> {
    let bounded: String = first_prompt.chars().take(TITLE_MAX_INPUT_CHARS).collect();
    let answer = ask_side_model(
        models,
        session_id,
        SideRequest {
            system_prompt: TITLE_SYSTEM_PROMPT,
            text: format!(
                "Generate a concise, contextual title (3-5 words) for a chat that starts with this message: \"{bounded}\""
            ),
            temperature: TITLE_TEMPERATURE,
            max_tokens: TITLE_MAX_TOKENS,
            timeout: TITLE_TIMEOUT,
        },
        cancel,
    )
    .await
    .map_err(|error| format!("Failed to generate Agent task title: {error}"))?;
    Ok(normalize_generated_title(&answer))
}

/// `raw` without `<think>` and `<analysis>` blocks; an unclosed block runs
/// to the end.
fn strip_reasoning_blocks(raw: &str) -> String {
    let mut value = raw.to_string();
    for tag in ["think", "analysis"] {
        loop {
            let lowercase = value.to_ascii_lowercase();
            let Some(start) = lowercase.find(&format!("<{tag}")) else {
                break;
            };
            let Some(open_end_offset) = lowercase[start..].find('>') else {
                value.truncate(start);
                break;
            };
            let content_start = start + open_end_offset + 1;
            let close = format!("</{tag}>");
            let Some(close_offset) = lowercase[content_start..].find(&close) else {
                value.truncate(start);
                break;
            };
            let end = content_start + close_offset + close.len();
            value.replace_range(start..end, "");
        }
    }
    value
}

/// The first line of an answer with text, on one line, without reasoning
/// blocks, control characters or surrounding double quotes.
fn first_answer_line(raw: &str) -> Option<String> {
    let printable: String = strip_reasoning_blocks(raw)
        .chars()
        .filter(|character| !character.is_control() || character.is_whitespace())
        .collect();
    let line = printable
        .lines()
        .find(|line| !line.trim().is_empty())?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    Some(line)
}

fn normalize_generated_title(raw: &str) -> Option<String> {
    let line = first_answer_line(raw)?;
    let title = line
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            line.strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(&line)
        .trim();
    (!title.is_empty()).then(|| session_title_from_prompt(title))
}

fn normalize_summary(raw: &str) -> Option<String> {
    let line = first_answer_line(raw)?;
    let line = line
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(&line)
        .trim();
    let summary: String = line.chars().take(SUMMARY_MAX_CHARS).collect();
    (!summary.is_empty()).then_some(summary)
}

fn bounded_input(text: &str) -> String {
    text.chars().take(SUMMARY_MAX_INPUT_CHARS).collect()
}

/// The `tool_choice` a side question's request carries: the task's tools
/// stay in the request, for its cached prefix, but cannot be called.
fn without_tool_calls(mut payload: Value) -> Value {
    if let Some(body) = payload.as_object_mut()
        && body.contains_key("tools")
    {
        body.insert("tool_choice".to_string(), json!("none"));
    }
    payload
}

/// A side question's messages after the task's context: the framed first
/// question, then each earlier answer and the question after it.
fn side_question_messages(
    model: &Model,
    prior: &[SideQuestionTurn],
    question: &str,
) -> Vec<Message> {
    let mut questions = prior
        .iter()
        .map(|turn| turn.question.as_str())
        .chain(std::iter::once(question));
    let first = questions.next().unwrap_or_default();
    let mut messages = vec![Message::User(UserMessage::text(format!(
        "{SIDE_QUESTION_PREFIX}\n\n{first}"
    )))];
    for (turn, next) in prior.iter().zip(questions) {
        messages.push(Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::text(turn.answer.clone())],
            ..AssistantMessage::empty(model)
        }));
        messages.push(Message::User(UserMessage::text(next)));
    }
    messages
}

/// Stream a side question's answer as `SideQuestion` events, ending with
/// `Finished` or one `Error`.
async fn stream_side_question(
    stream_fn: Arc<dyn pi_ai::StreamFn>,
    model: Model,
    context: Context,
    session_id: String,
    request_id: String,
    events: AgentEventDispatcher,
    cancel: CancellationToken,
) {
    let emit = |event: SideQuestionEvent| {
        emit_agent_event(
            &events,
            AgentServiceEvent::SideQuestion {
                session_id: session_id.clone(),
                request_id: request_id.clone(),
                event,
            },
        );
    };
    let options = StreamOptions {
        session_id: Some(session_id.clone()),
        cancel: cancel.clone(),
        on_payload: Some(Arc::new(|payload| {
            futures_util::future::ready(without_tool_calls(payload)).boxed()
        })),
        ..StreamOptions::default()
    };
    let mut stream = stream_fn.stream(&model, context, options);
    let answer = async {
        let mut ending = None;
        while let Some(event) = stream.next().await {
            match event {
                AssistantMessageEvent::TextDelta { delta, .. } if !delta.is_empty() => {
                    emit(SideQuestionEvent::Chunk(delta));
                }
                AssistantMessageEvent::Done { message }
                | AssistantMessageEvent::Error { message } => ending = Some(message),
                _ => {}
            }
        }
        ending
    };
    tokio::pin!(answer);
    let ending = tokio::select! {
        ending = &mut answer => ending,
        () = tokio::time::sleep(SIDE_QUESTION_TIMEOUT) => {
            cancel.cancel();
            let _ = answer.await;
            emit(SideQuestionEvent::Error("Side question timed out".to_string()));
            return;
        }
    };
    match ending {
        Some(message) if message.stop_reason == StopReason::Aborted => {
            emit(SideQuestionEvent::Error(
                "Side question cancelled".to_string(),
            ));
        }
        Some(message) if message.stop_reason == StopReason::Error => {
            emit(SideQuestionEvent::Error(
                message
                    .error_message
                    .unwrap_or_else(|| "Side question failed".to_string()),
            ));
        }
        Some(_) => emit(SideQuestionEvent::Finished),
        None => emit(SideQuestionEvent::Error("Side question failed".to_string())),
    }
}

impl AgentRuntimeHandle {
    /// A one-line summary of a finished tool call for the activity feed, or
    /// `None` when the model gave nothing usable.
    pub async fn summarize_tool_call(
        &self,
        session_id: &str,
        tool_name: &str,
        input: Option<&Value>,
        output_text: &str,
    ) -> Result<Option<String>, String> {
        let input_line = match input {
            Some(value) if !value.is_null() => {
                bounded_input(&serde_json::to_string(value).unwrap_or_default())
            }
            _ => String::new(),
        };
        let text = format!(
            "Tool: {tool_name}\nInput: {input_line}\nOutput: {}",
            bounded_input(output_text)
        );
        self.summarize(session_id, TOOL_SUMMARY_SYSTEM_PROMPT, text)
            .await
    }

    /// A one-line summary of a finished thinking block, for the header of
    /// its row.
    pub async fn summarize_thinking(
        &self,
        session_id: &str,
        thinking_text: &str,
    ) -> Result<Option<String>, String> {
        self.summarize(
            session_id,
            THINKING_SUMMARY_SYSTEM_PROMPT,
            bounded_input(thinking_text),
        )
        .await
    }

    async fn summarize(
        &self,
        session_id: &str,
        system_prompt: &str,
        text: String,
    ) -> Result<Option<String>, String> {
        self.ensure_accepting_new_work()?;
        let runtime = self.runtime().await?;
        let answer = ask_side_model(
            runtime.models(),
            session_id,
            SideRequest {
                system_prompt,
                text,
                temperature: SUMMARY_TEMPERATURE,
                max_tokens: SUMMARY_MAX_TOKENS,
                timeout: SUMMARY_TIMEOUT,
            },
            runtime.lifetime.child_token(),
        )
        .await
        .map_err(|error| format!("Failed to summarize: {error}"))?;
        Ok(normalize_summary(&answer))
    }

    /// Answer a `/btw` side question about a task. The task's model gets
    /// the task's context, then the earlier turns of the side thread
    /// (`prior`) and the new question; its tools stay in the request but
    /// cannot be called. Nothing is saved to the task: the answer streams
    /// as `SideQuestion` events tagged with `request_id`, and ends when the
    /// runtime stops.
    pub async fn ask_side_question(
        &self,
        session_id: &str,
        request_id: String,
        prior: Vec<SideQuestionTurn>,
        question: String,
    ) -> Result<(), String> {
        let question = question.trim().to_string();
        if question.is_empty() {
            return Err("Question cannot be empty".to_string());
        }
        self.ensure_accepting_new_work()?;
        let runtime = self.runtime().await?;
        let session = runtime.side_question_session(session_id).await?;
        let model = session
            .model()
            .ok_or_else(|| "The Agent task has no model".to_string())?;
        let mut messages = pi_coding_agent::messages::convert_to_llm(&session.messages());
        if !messages
            .iter()
            .any(|message| matches!(message, Message::System(_)))
        {
            // A task that has not run has no system message in its history.
            messages = Context::new(&session.system_prompt(), Vec::new(), messages).messages;
        }
        messages.extend(side_question_messages(&model, &prior, &question));
        tokio::spawn(stream_side_question(
            runtime.models().stream_fn(),
            model,
            Context::from_messages(messages),
            session_id.to_string(),
            request_id,
            self.service.host.events.clone(),
            runtime.lifetime.child_token(),
        ));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
