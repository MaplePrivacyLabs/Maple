//! Entry points of features that have not moved to the Pi runtime yet.
//!
//! Maple's features move onto Pi one at a time. Until a feature has moved,
//! its entry point here keeps the host's calls compiling and answers with
//! nothing, or with an error that says the feature is not available yet.
//! Each one is replaced by the real feature, and this module goes once it
//! is empty.

use super::{AgentRuntimeHandle, AgentSubagent};

const UNAVAILABLE: &str = "This feature is not available in this build of Maple yet";

fn unavailable<T>() -> Result<T, String> {
    Err(UNAVAILABLE.to_string())
}

impl AgentRuntimeHandle {
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
}
