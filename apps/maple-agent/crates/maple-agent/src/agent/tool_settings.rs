//! Settings → Tools: which tools the account's tasks on this host offer the
//! model. Every switch has a default, and the account's `agent.tools`
//! setting in the host-owned store keeps only the switches the user changed.
//!
//! A task's session is built with the switches as they are, whether the task
//! is new or reopened. A change reaches the loaded tasks from their next
//! model request, as Pi's `setActiveTools` does within a session, and their
//! transcripts record it.
//!
//! The web tools follow each task's web switch; external agents, MCP servers
//! and computer use have their switches in Integrations.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::config::account_store;
use super::{
    AgentPathLayout, AgentRuntimeHandle, AgentSetToolEnabledRequest, AgentToolSwitch, tools,
};

const TOOLS_SETTING: &str = "agent.tools";
const TOOLS_SETTING_VERSION: u32 = 1;

/// A switch of Settings → Tools, in the order the page lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolSwitch {
    Read,
    /// Whichever shell tool the task uses.
    Shell,
    Edit,
    Write,
    Grep,
    Find,
    Ls,
    ReadImage,
    TodoWrite,
    RequestUserInput,
    Python,
}

impl ToolSwitch {
    pub(crate) const ALL: [Self; 11] = [
        Self::Read,
        Self::Shell,
        Self::Edit,
        Self::Write,
        Self::Grep,
        Self::Find,
        Self::Ls,
        Self::ReadImage,
        Self::TodoWrite,
        Self::RequestUserInput,
        Self::Python,
    ];

    /// The switches of Pi's built-in tools; the others are Maple's tools.
    pub(crate) const BUILTIN: [Self; 7] = [
        Self::Read,
        Self::Shell,
        Self::Edit,
        Self::Write,
        Self::Grep,
        Self::Find,
        Self::Ls,
    ];

    /// The switch's id, in the host API and the saved setting: its tool's
    /// name, or `shell`.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Shell => "shell",
            Self::Edit => "edit",
            Self::Write => "write",
            Self::Grep => "grep",
            Self::Find => "find",
            Self::Ls => "ls",
            Self::ReadImage => "read_image",
            Self::TodoWrite => "todo_write",
            Self::RequestUserInput => "request_user_input",
            Self::Python => "python",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|switch| switch.id() == id)
    }

    /// The switch of one of Maple's own tools, by the tool's name.
    pub(crate) fn for_maple_tool(name: &str) -> Option<Self> {
        Self::from_id(name).filter(|switch| !Self::BUILTIN.contains(switch))
    }

    /// Whether the switch is on for an account that has not changed it. The
    /// search tools are off because the shell does what they do, and
    /// `python` until the user turns it on.
    pub(crate) fn enabled_by_default(self) -> bool {
        !matches!(self, Self::Grep | Self::Find | Self::Ls | Self::Python)
    }

    /// The tool this switch turns on in a task whose shell tool is `shell`.
    pub(crate) fn tool_name(self, shell: &'static str) -> &'static str {
        match self {
            Self::Shell => shell,
            switch => switch.id(),
        }
    }
}

/// The account's switches: the ones changed from their defaults, by id.
/// Ids this build does not know are ignored, and kept for the build that
/// saved them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolSwitches {
    changed: BTreeMap<String, bool>,
}

impl ToolSwitches {
    pub(crate) fn enabled(&self, switch: ToolSwitch) -> bool {
        self.changed
            .get(switch.id())
            .copied()
            .unwrap_or_else(|| switch.enabled_by_default())
    }

    /// Keep only a change from the default.
    pub(crate) fn set(&mut self, switch: ToolSwitch, enabled: bool) {
        if enabled == switch.enabled_by_default() {
            self.changed.remove(switch.id());
        } else {
            self.changed.insert(switch.id().to_string(), enabled);
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredTools {
    version: u32,
    #[serde(default)]
    tools: BTreeMap<String, bool>,
}

fn load_stored_tools(paths: &AgentPathLayout, user_id: &str) -> Result<ToolSwitches, String> {
    let (store, account) = account_store(paths, user_id).map_err(|error| error.to_string())?;
    let Some(value) = store
        .account_setting(account, TOOLS_SETTING)
        .map_err(|error| error.to_string())?
    else {
        return Ok(ToolSwitches::default());
    };
    let version = value
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| "Tool settings have no version".to_string())?;
    if version != u64::from(TOOLS_SETTING_VERSION) {
        return Err(format!("Unsupported tool settings version {version}"));
    }
    let stored: StoredTools = serde_json::from_value(value)
        .map_err(|error| format!("Failed to parse tool settings: {error}"))?;
    Ok(ToolSwitches {
        changed: stored.tools,
    })
}

fn save_stored_tools(
    paths: &AgentPathLayout,
    user_id: &str,
    switches: &ToolSwitches,
) -> Result<(), String> {
    let (store, account) = account_store(paths, user_id).map_err(|error| error.to_string())?;
    let value = serde_json::to_value(StoredTools {
        version: TOOLS_SETTING_VERSION,
        tools: switches.changed.clone(),
    })
    .map_err(|error| error.to_string())?;
    store
        .set_account_setting(account, TOOLS_SETTING, &value, pi_ai::now_ms())
        .map_err(|error| format!("Failed to save tool settings: {error}"))
}

/// The account's switches. Read where a task must keep working, so a setting
/// that cannot be read reads as the defaults, and the next change replaces it.
pub(super) fn tool_switches(paths: &AgentPathLayout, user_id: &str) -> ToolSwitches {
    load_stored_tools(paths, user_id).unwrap_or_else(|error| {
        log::warn!("{error}; tools read as their defaults");
        ToolSwitches::default()
    })
}

/// Whether this host can offer the switch's tool: `python` needs the CPython
/// bundled with the build.
fn available(switch: ToolSwitch, python: bool) -> bool {
    switch != ToolSwitch::Python || python
}

fn project_tools(switches: &ToolSwitches, python: bool) -> Vec<AgentToolSwitch> {
    ToolSwitch::ALL
        .into_iter()
        .map(|switch| AgentToolSwitch {
            id: switch.id().to_string(),
            enabled: switches.enabled(switch),
            enabled_by_default: switch.enabled_by_default(),
            available: available(switch, python),
        })
        .collect()
}

impl AgentRuntimeHandle {
    /// The switches of Settings → Tools.
    pub async fn list_tools(&self) -> Result<Vec<AgentToolSwitch>, String> {
        let _settings = self.lock_settings().await;
        self.verify_generation().await?;
        Ok(project_tools(
            &tool_switches(self.paths(), &self.user_id),
            tools::PythonWorkers::is_bundled(),
        ))
    }

    /// Switch a tool on or off for the account's tasks. New and reopened
    /// tasks are built with it, and the loaded ones get it from their next
    /// model request. Enabling needs a tool this host can offer; disabling
    /// always works.
    pub async fn set_tool_enabled(
        &self,
        request: AgentSetToolEnabledRequest,
    ) -> Result<Vec<AgentToolSwitch>, String> {
        let id = request.id.trim();
        let switch = ToolSwitch::from_id(id).ok_or_else(|| format!("Unknown tool '{id}'"))?;
        let python = tools::PythonWorkers::is_bundled();
        let (switches, runtime) = {
            let _settings = self.lock_settings().await;
            self.verify_generation().await?;
            self.ensure_accepting_new_work()?;
            if request.enabled && !available(switch, python) {
                return Err(format!("The {id} tool is not available on this computer"));
            }
            let mut switches = tool_switches(self.paths(), &self.user_id);
            switches.set(switch, request.enabled);
            save_stored_tools(self.paths(), &self.user_id, &switches)?;
            (switches, self.current_runtime().await?)
        };
        if let Some(runtime) = runtime {
            runtime.apply_tool_switch(switch).await;
        }
        Ok(project_tools(&switches, python))
    }
}

#[cfg(test)]
mod tests;
