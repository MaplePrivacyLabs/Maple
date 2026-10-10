//! The tools a desktop task delegates with. Every desktop task has all
//! five; the model sees them while the task may use a provider, and a call
//! must name a provider the task may use.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use pi_agent_core::{AgentTool, AgentToolResult, ToolError, ToolInvocation};
use pi_ai::Tool;
use pi_coding_agent::AgentSession;
use pi_coding_agent::extensions::{RegisteredTool, ToolPrompt};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{
    AGENT_CANCEL_TOOL, AGENT_SEND_TOOL, AGENT_START_TOOL, AGENT_STATUS_TOOL, AgentRefParams,
    AgentSendParams, AgentStartParams, EXTERNAL_AGENT_TOOLS, ExternalAgentCall,
    ExternalAgentRegistry, LIST_AGENT_PROVIDERS_TOOL,
};
use crate::agent::tool_context::SharedAgentToolContext;

/// The providers a task may use now: switched on in Settings and for the
/// task. Its tools read them at each call; each run brings them up to date.
#[derive(Clone, Default)]
pub(crate) struct TaskProviders(Arc<RwLock<Vec<String>>>);

impl TaskProviders {
    pub(crate) fn get(&self) -> Vec<String> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set(&self, providers: Vec<String>) {
        *self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = providers;
    }
}

/// What a task's external agent tools are set up from.
pub(crate) struct ExternalAgentToolsFor {
    pub(crate) registry: Arc<ExternalAgentRegistry>,
    pub(crate) session_id: String,
    /// The task's folder, where its agents work.
    pub(crate) working_dir: PathBuf,
    pub(crate) login_path: Option<String>,
    pub(crate) tool_context: SharedAgentToolContext,
    pub(crate) providers: TaskProviders,
}

#[derive(Clone, Copy)]
enum Action {
    Start,
    Send,
    Status,
    Cancel,
    ListProviders,
}

/// A task's external agent tools, registered but not declared until
/// [`sync_external_agent_tools`] finds a provider the task may use.
pub(crate) fn external_agent_tools(task: ExternalAgentToolsFor) -> Vec<RegisteredTool> {
    let task = Arc::new(task);
    declarations()
        .into_iter()
        .map(|(action, declaration, snippet)| RegisteredTool {
            tool: Arc::new(ExternalAgentTool {
                declaration,
                action,
                task: Arc::clone(&task),
            }),
            prompt: ToolPrompt {
                snippet: Some(snippet.to_string()),
                guidelines: Vec::new(),
            },
            active: false,
            extension: None,
        })
        .collect()
}

/// Give the task's tools the providers it may use now, and declare the
/// tools to the model while there is one. The change is declared with the
/// session's next request.
pub(crate) fn sync_external_agent_tools(
    session: &AgentSession,
    task: &TaskProviders,
    providers: Vec<String>,
) {
    let enabled = !providers.is_empty();
    task.set(providers);
    let active = session.active_tools();
    let declared = EXTERNAL_AGENT_TOOLS
        .iter()
        .all(|name| active.iter().any(|active| active == name));
    if declared == enabled {
        return;
    }
    let mut next: Vec<String> = active
        .into_iter()
        .filter(|name| !EXTERNAL_AGENT_TOOLS.contains(&name.as_str()))
        .collect();
    if enabled {
        next.extend(EXTERNAL_AGENT_TOOLS.map(str::to_string));
    }
    session.set_active_tools(&next);
}

struct ExternalAgentTool {
    declaration: Tool,
    action: Action,
    task: Arc<ExternalAgentToolsFor>,
}

#[async_trait]
impl AgentTool for ExternalAgentTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        Ok(self
            .run(invocation)
            .await
            .unwrap_or_else(AgentToolResult::error))
    }
}

impl ExternalAgentTool {
    async fn run(&self, invocation: ToolInvocation) -> Result<AgentToolResult, String> {
        let providers = self.task.providers.get();
        if providers.is_empty() {
            return Err("External agents are not enabled for this task.".to_string());
        }
        let registry = &self.task.registry;
        match self.action {
            Action::ListProviders => {
                let call = self.call(&invocation, None);
                Ok(registry.list_providers(&call, &providers).await)
            }
            Action::Start => {
                let params: AgentStartParams = arguments(&invocation.args)?;
                enabled(&providers, &params.provider)?;
                let call = self.turn_call(&invocation).await;
                registry.start(call, params).await
            }
            Action::Send => {
                let params: AgentSendParams = arguments(&invocation.args)?;
                enabled(&providers, &params.provider)?;
                let call = self.turn_call(&invocation).await;
                registry.send(call, params).await
            }
            Action::Status => {
                let params: AgentRefParams = arguments(&invocation.args)?;
                enabled(&providers, &params.provider)?;
                registry.status(&self.call(&invocation, None), params).await
            }
            Action::Cancel => {
                let params: AgentRefParams = arguments(&invocation.args)?;
                enabled(&providers, &params.provider)?;
                registry
                    .cancel_tool(&self.call(&invocation, None), params)
                    .await
            }
        }
    }

    fn call(&self, invocation: &ToolInvocation, row_id: Option<String>) -> ExternalAgentCall {
        ExternalAgentCall {
            session_id: self.task.session_id.clone(),
            working_dir: self.task.working_dir.clone(),
            row_id,
            login_path: self
                .task
                .registry
                .search_path(self.task.login_path.as_deref()),
            tool_context: self.task.tool_context.snapshot(),
            cancel_token: invocation.cancel.clone(),
        }
    }

    /// A call that starts a turn, whose progress joins the call's row.
    async fn turn_call(&self, invocation: &ToolInvocation) -> ExternalAgentCall {
        let row_id = self
            .task
            .registry
            .tool_row_id(&self.task.session_id, &invocation.call_id)
            .await;
        self.call(invocation, Some(row_id))
    }
}

fn arguments<T: DeserializeOwned>(args: &Value) -> Result<T, String> {
    let args = match args {
        Value::Null => json!({}),
        args => args.clone(),
    };
    serde_json::from_value(args).map_err(|error| format!("Invalid arguments: {error}"))
}

fn enabled(providers: &[String], provider: &str) -> Result<(), String> {
    if providers.iter().any(|enabled| enabled == provider.trim()) {
        Ok(())
    } else {
        Err("That external agent is not enabled for this task.".to_string())
    }
}

fn declarations() -> [(Action, Tool, &'static str); 5] {
    let reference = || {
        json!({
            "type": "object",
            "properties": {
                "provider": { "type": "string", "description": "The agent's provider" },
                "agent_id": { "type": "string", "description": "The agent ID that agent_start returned" }
            },
            "required": ["provider", "agent_id"]
        })
    };
    [
        (
            Action::Start,
            Tool::new(
                AGENT_START_TOOL,
                format!(
                    "Hand a self-contained piece of work to an external coding agent (an installed harness such as Codex or Claude Code) that runs in the project with its own context and its own account. \
The new agent knows nothing about this conversation: write a complete briefing with the task, relevant files, current state, what was tried, decisions made, acceptance criteria, and constraints. \
It runs under its own sandbox and approval settings; Maple accepts its approval requests, and its questions still come to the user through Maple. \
Blocking by default: the call returns the agent's result. With background=true the call returns at once and Maple tells you when the agent finishes; do not poll. \
Call {LIST_AGENT_PROVIDERS_TOOL} first when unsure what is installed."
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "provider": {
                            "type": "string",
                            "description": "Which external agent to use, from list_agent_providers (for example \"codex\" or \"claude\")"
                        },
                        "prompt": {
                            "type": "string",
                            "description": "The complete, self-contained briefing for the agent"
                        },
                        "background": {
                            "type": "boolean",
                            "description": "Return at once and let the agent work on; Maple reports when it finishes (default false)"
                        },
                        "model": {
                            "type": "string",
                            "description": "Optional model override for the provider; omit to use its own default"
                        },
                        "effort": {
                            "type": "string",
                            "description": "Optional reasoning effort for the provider (for example \"low\", \"medium\", \"high\"); omit to use its default"
                        },
                        "cwd": {
                            "type": "string",
                            "description": "Optional subdirectory of the project for the agent to work in; must stay inside the project"
                        }
                    },
                    "required": ["provider", "prompt"]
                }),
            ),
            "Hand a self-contained piece of work to an external coding agent such as Codex or Claude Code",
        ),
        (
            Action::Send,
            Tool::new(
                AGENT_SEND_TOOL,
                format!(
                    "Give an external agent you started with {AGENT_START_TOOL} more instructions in the same thread, with its context intact. Use it to follow up, correct, or continue that agent's work."
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "provider": { "type": "string", "description": "The agent's provider" },
                        "agent_id": { "type": "string", "description": "The agent ID that agent_start returned" },
                        "prompt": { "type": "string", "description": "The next instructions for the agent" },
                        "background": {
                            "type": "boolean",
                            "description": "Return at once and let the agent work on; Maple reports when it finishes (default false)"
                        },
                        "model": { "type": "string", "description": "Optional model override" },
                        "effort": { "type": "string", "description": "Optional reasoning effort" }
                    },
                    "required": ["provider", "agent_id", "prompt"]
                }),
            ),
            "Continue an external agent's thread with more instructions",
        ),
        (
            Action::Status,
            Tool::new(
                AGENT_STATUS_TOOL,
                "Read the state and latest result of an external agent: its status, last message, files it changed, and commands it ran. Maple tells you when a background agent finishes, so call this when you need the result or when the user asks, not in a loop.",
                reference(),
            ),
            "Read an external agent's state and latest result",
        ),
        (
            Action::Cancel,
            Tool::new(
                AGENT_CANCEL_TOOL,
                format!(
                    "Interrupt what an external agent is doing now. The agent keeps its thread, so {AGENT_SEND_TOOL} can continue it later."
                ),
                reference(),
            ),
            "Interrupt what an external agent is doing now",
        ),
        (
            Action::ListProviders,
            Tool::new(
                LIST_AGENT_PROVIDERS_TOOL,
                "List the external coding agents installed on this computer that a task may delegate to, with their version and sign-in state.",
                json!({
                    "type": "object",
                    "properties": {}
                }),
            ),
            "List the external coding agents this task may delegate to",
        ),
    ]
}
