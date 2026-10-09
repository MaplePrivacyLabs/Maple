//! Tools that need Maple's desktop interface, as Goose's runtime offered
//! them to desktop tasks: the plan the user watches (`todo_write`) and
//! questions the user answers (`request_user_input`).

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use pi_agent_core::{AgentTool, AgentToolResult, FnTool, ToolError, ToolInvocation};
use pi_ai::Tool;
use pi_coding_agent::extensions::{RegisteredTool, ToolPrompt};
use serde_json::{Map, Value, json};

use crate::agent::questions::QuestionBroker;
use crate::agent::{AgentQuestion, AgentQuestionOption};

/// At most this many questions go in one card, so the answers have one slot
/// per id.
const MAX_QUESTIONS: usize = 3;
/// At most this many options per question.
const MAX_OPTIONS: usize = 5;

/// The desktop tools of the task `session_id`. Questions go through
/// `questions`, the broker of the service the interface answers.
pub(super) fn desktop_tools(session_id: &str, questions: QuestionBroker) -> Vec<RegisteredTool> {
    vec![
        registered(
            Arc::new(FnTool::new(todo_declaration(), |invocation| async move {
                // The list is state for the interface; it is echoed back so
                // the model sees its own plan confirmed.
                Ok(AgentToolResult::text(
                    serde_json::to_string_pretty(&invocation.args)
                        .unwrap_or_else(|_| "[]".to_string()),
                ))
            })),
            "Record or update the plan for the current task",
        ),
        registered(
            Arc::new(RequestUserInput {
                declaration: request_user_input_declaration(),
                session_id: session_id.to_string(),
                questions,
            }),
            "Ask the user one to three short questions and wait for the answers",
        ),
    ]
}

fn registered(tool: Arc<dyn AgentTool>, snippet: &str) -> RegisteredTool {
    RegisteredTool {
        tool,
        prompt: ToolPrompt {
            snippet: Some(snippet.to_string()),
            guidelines: Vec::new(),
        },
        active: true,
        extension: None,
    }
}

fn todo_declaration() -> Tool {
    Tool::new(
        "todo_write",
        "Record or update the plan for the current task. Call this whenever the plan changes so the user sees live progress.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["todos"],
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The full todo list; replaces any previous list",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["content", "status"],
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "Short task description"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Current state of this item"
                            }
                        }
                    }
                }
            }
        }),
    )
}

fn request_user_input_declaration() -> Tool {
    Tool::new(
        "request_user_input",
        "Request user input for one to three short questions and wait for the response.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["questions"],
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "Questions to show the user. Prefer 1 and do not exceed 3",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["id", "header", "question", "options"],
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "Stable identifier for mapping answers (snake_case)."
                            },
                            "header": {
                                "type": "string",
                                "description": "Short header label shown in the UI (12 or fewer chars)."
                            },
                            "question": {
                                "type": "string",
                                "description": "Single-sentence prompt shown to the user."
                            },
                            "options": {
                                "type": "array",
                                "description": "Provide 2-3 mutually exclusive choices. Put the recommended option first and suffix its label with \"(Recommended)\". Do not include an \"Other\" option in this list; the client will add a free-form \"Other\" option automatically.",
                                "items": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "required": ["label", "description"],
                                    "properties": {
                                        "label": {
                                            "type": "string",
                                            "description": "User-facing label (1-5 words)."
                                        },
                                        "description": {
                                            "type": "string",
                                            "description": "One short sentence explaining impact/tradeoff if selected."
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }),
    )
}

struct RequestUserInput {
    declaration: Tool,
    session_id: String,
    questions: QuestionBroker,
}

#[async_trait]
impl AgentTool for RequestUserInput {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    /// Take what Goose's runtime took, so a near miss does not dead-end the
    /// model: `questions` as one object, or one question at the top level,
    /// with fields left out or of other types. Each question is rebuilt in
    /// the schema's shape, and [`parse_user_questions`] applies the rules.
    fn prepare_arguments(&self, mut arguments: Map<String, Value>) -> Map<String, Value> {
        let entries = match arguments.remove("questions") {
            Some(Value::Array(entries)) => entries,
            Some(single @ Value::Object(_)) => vec![single],
            Some(other) => {
                arguments.insert("questions".to_string(), other);
                return arguments;
            }
            None if arguments.contains_key("question") => vec![Value::Object(arguments)],
            None => return arguments,
        };
        let questions = entries.iter().filter_map(schema_question).collect();
        Map::from_iter([("questions".to_string(), Value::Array(questions))])
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        let entries = invocation
            .args
            .get("questions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let questions = parse_user_questions(&entries);
        if questions.is_empty() {
            return Ok(AgentToolResult::error("questions must not be empty"));
        }
        // A stopped task must not leave the tool waiting on a card the
        // interface has taken down; dropping the ask takes the question back.
        let answer = tokio::select! {
            biased;
            () = invocation.cancel.cancelled() => {
                return Ok(AgentToolResult::error("request_user_input cancelled"));
            }
            answer = self.questions.ask(&self.session_id, questions) => answer,
        };
        Ok(AgentToolResult::text(if answer.trim().is_empty() {
            "(no answer provided)".to_string()
        } else {
            answer
        }))
    }
}

/// One question in the schema's shape, its text fields empty where the
/// model sent none, for [`parse_user_questions`] to default or drop.
fn schema_question(entry: &Value) -> Option<Value> {
    let entry = entry.as_object()?;
    let options: Vec<Value> = entry
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .map(|option| {
            json!({
                "label": text_field(option, "label"),
                "description": text_field(option, "description"),
            })
        })
        .collect();
    Some(json!({
        "id": text_field(entry, "id"),
        "header": text_field(entry, "header"),
        "question": text_field(entry, "question"),
        "options": options,
    }))
}

fn text_field(object: &Map<String, Value>, key: &str) -> String {
    object
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// The questions the card shows: blank ones dropped, ids made unique, a
/// header for each, at most [`MAX_QUESTIONS`] with at most [`MAX_OPTIONS`]
/// options each. A `multiSelect` flag, which external agents' questions
/// carry, lets the user pick several options.
pub(super) fn parse_user_questions(entries: &[Value]) -> Vec<AgentQuestion> {
    let mut questions = Vec::new();
    let mut seen_ids = HashSet::new();
    for (index, entry) in entries.iter().enumerate() {
        if questions.len() >= MAX_QUESTIONS {
            break;
        }
        let text = |key: &str| entry.get(key).and_then(Value::as_str).unwrap_or("");
        let question = text("question").trim().to_string();
        if question.is_empty() {
            continue;
        }
        let base_id = Some(text("id").trim())
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("question_{index}"));
        let mut id = base_id.clone();
        let mut suffix = 2;
        while !seen_ids.insert(id.clone()) {
            id = format!("{base_id}_{suffix}");
            suffix += 1;
        }
        let header = Some(text("header"))
            .filter(|header| !header.trim().is_empty())
            .unwrap_or("Question")
            .to_string();
        let options = entry
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|option| {
                let label = option.get("label").and_then(Value::as_str)?.trim();
                (!label.is_empty()).then(|| AgentQuestionOption {
                    label: label.to_string(),
                    description: option
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                })
            })
            .take(MAX_OPTIONS)
            .collect();
        questions.push(AgentQuestion {
            multi_select: entry
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            id,
            header,
            question,
            options,
        });
    }
    questions
}
