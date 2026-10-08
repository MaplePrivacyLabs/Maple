//! Entry points of features that have not moved to the Pi runtime yet.
//!
//! Maple's features move onto Pi one at a time. Until a feature has moved,
//! its entry point here keeps the host's calls compiling and answers with
//! nothing, or with an error that says the feature is not available yet.
//! Each one is replaced by the real feature, and this module goes once it
//! is empty.

use super::{
    AgentIntegration, AgentIntegrationPermissions, AgentPathLayout, AgentRuntimeHandle,
    AgentSessionMcpServer, AgentSetIntegrationEnabledRequest, AgentSetSessionMcpServerRequest,
    AgentSetupIntegrationRequest, AgentSlashCommand, AgentSubagent, SideQuestionTurn,
};

const UNAVAILABLE: &str = "This feature is not available in this build of Maple yet";

fn unavailable<T>() -> Result<T, String> {
    Err(UNAVAILABLE.to_string())
}

/// Start the host-owned setup of a built-in integration.
pub fn begin_integration_setup(
    _request: &AgentSetupIntegrationRequest,
) -> Result<AgentIntegrationPermissions, String> {
    unavailable()
}

/// Slash commands from skills.
pub(super) fn slash_commands(
    _paths: &AgentPathLayout,
    _user_id: Option<&str>,
    _working_dir: Option<&str>,
) -> Vec<AgentSlashCommand> {
    Vec::new()
}

/// The prompt of a skill's slash command.
pub(super) fn resolve_slash_command(
    _working_dir: Option<&str>,
    _command: &str,
    _args: &str,
) -> Result<Option<String>, String> {
    Ok(None)
}

impl AgentRuntimeHandle {
    /// Maple-curated integrations found on this device.
    pub async fn list_integrations(&self) -> Result<Vec<AgentIntegration>, String> {
        self.verify_generation().await?;
        Ok(Vec::new())
    }

    pub async fn set_integration_enabled(
        &self,
        _request: AgentSetIntegrationEnabledRequest,
    ) -> Result<Vec<AgentIntegration>, String> {
        unavailable()
    }

    pub async fn setup_integration(
        &self,
        _request: AgentSetupIntegrationRequest,
    ) -> Result<Vec<AgentIntegration>, String> {
        unavailable()
    }

    /// A task's MCP servers and integrations.
    pub async fn list_session_mcp_servers(
        &self,
        _session_id: String,
    ) -> Result<Vec<AgentSessionMcpServer>, String> {
        self.verify_generation().await?;
        Ok(Vec::new())
    }

    pub async fn set_session_mcp_server_enabled(
        &self,
        _request: AgentSetSessionMcpServerRequest,
    ) -> Result<Vec<AgentSessionMcpServer>, String> {
        unavailable()
    }

    /// External agents still working for a task.
    pub async fn session_subagents(&self, _session_id: &str) -> Vec<AgentSubagent> {
        Vec::new()
    }

    pub async fn cancel_external_agent(
        &self,
        _session_id: &str,
        _agent_id: &str,
    ) -> Result<(), String> {
        unavailable()
    }

    /// A one-line summary of a tool call.
    pub async fn summarize_tool_call(
        &self,
        _session_id: &str,
        _tool_name: &str,
        _input: Option<&serde_json::Value>,
        _output_text: &str,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }

    /// A one-line summary of a thinking block.
    pub async fn summarize_thinking(
        &self,
        _session_id: &str,
        _thinking_text: &str,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }

    /// A `/btw` side question.
    pub async fn ask_side_question(
        &self,
        _session_id: &str,
        _request_id: String,
        _prior: Vec<SideQuestionTurn>,
        _question: String,
    ) -> Result<(), String> {
        unavailable()
    }
}
