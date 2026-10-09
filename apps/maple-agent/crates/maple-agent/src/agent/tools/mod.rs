//! A task's tools: Pi's built-in tools, set up for Maple, and Maple's own.
//!
//! Pi's session gives every task `read`, `bash`, `edit` and `write`, or
//! `powershell` in place of `bash` on Windows without Git Bash. Maple sets up
//! how their commands start: from the login shell's PATH, with the task's id,
//! and with the task's tool context, so the ACP bridge's variables stay out.
//! `MAPLE_SHELL` names the bash to run. Maple's own tools join these as they
//! move to the Pi runtime.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use pi_coding_agent::extensions::RegisteredTool;
use pi_coding_agent::tools::{
    BashSpawnContext, BashSpawnHook, PowerShellToolOptions, ToolsOptions, set_env_var,
};

use super::tool_context::SharedAgentToolContext;

/// The developer's choice of bash.
pub(crate) const SHELL_ENV: &str = "MAPLE_SHELL";

/// How a task's tools are set up.
pub(crate) struct TaskTools {
    pub(crate) options: ToolsOptions,
    /// The built-in tools the model gets.
    pub(crate) builtin: Vec<String>,
    /// Maple's own tools.
    pub(crate) maple: Vec<RegisteredTool>,
}

/// The tools of the task `session_id`. `login_path` is the login shell's
/// PATH, when it could be read.
pub(crate) fn task_tools(
    session_id: &str,
    tool_context: SharedAgentToolContext,
    login_path: Option<String>,
) -> TaskTools {
    let shell_path = std::env::var_os(SHELL_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let hook = spawn_hook(session_id.to_string(), tool_context, login_path);
    let mut options = ToolsOptions::default();
    options.bash.shell_path = shell_path.clone();
    options.bash.spawn_hook = Some(hook.clone());
    options.powershell = PowerShellToolOptions {
        spawn_hook: Some(hook),
        ..PowerShellToolOptions::default()
    };
    let shell =
        if cfg!(windows) && pi_coding_agent::tools::shell_config(shell_path.as_deref()).is_err() {
            "powershell"
        } else {
            "bash"
        };
    TaskTools {
        options,
        builtin: ["read", shell, "edit", "write"]
            .iter()
            .map(|name| name.to_string())
            .collect(),
        maple: Vec::new(),
    }
}

/// How every command of the task starts: the login PATH, the task's id, and
/// the task's tool context, read when the command starts.
fn spawn_hook(
    session_id: String,
    tool_context: SharedAgentToolContext,
    login_path: Option<String>,
) -> BashSpawnHook {
    Arc::new(move |mut spawn: BashSpawnContext| {
        if let Some(path) = &login_path {
            set_env_var(&mut spawn.env, "PATH", path.clone());
        }
        set_env_var(&mut spawn.env, "AGENT_SESSION_ID", session_id.clone());
        let context = tool_context.snapshot();
        for key in &context.scrub_from_parent {
            remove_env_var(&mut spawn.env, key);
        }
        for (key, value) in &context.values {
            set_env_var(&mut spawn.env, key, value.clone());
        }
        spawn
    })
}

/// Remove `key`, ignoring case on Windows, where variable names do.
fn remove_env_var(env: &mut BTreeMap<String, String>, key: &str) {
    env.retain(|existing, _| {
        if cfg!(windows) {
            !existing.eq_ignore_ascii_case(key)
        } else {
            existing != key
        }
    });
}

#[cfg(test)]
mod tests;
