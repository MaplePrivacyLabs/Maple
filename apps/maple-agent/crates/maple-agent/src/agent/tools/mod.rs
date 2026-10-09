//! A task's tools: Pi's built-in tools, set up for Maple, and Maple's own.
//!
//! Pi's session gives every task `read`, `bash`, `edit` and `write`, or
//! `powershell` in place of `bash` on Windows without Git Bash. Maple sets up
//! how their commands start: from the login shell's PATH, with the task's id,
//! and with the task's tool context, so the ACP bridge's variables reach only
//! the tasks of its sessions. A command that gets the bridge's credentials
//! ends everything it started when it ends. `MAPLE_SHELL` names the bash to
//! run.
//!
//! Maple's own tools join these: every task gets `read_image`, desktop tasks
//! get the plan the user watches (`todo_write`) and questions the user
//! answers (`request_user_input`), and every task gets `web_search` and
//! `open_url` while its web switch is on.

mod desktop;
mod read_image;
pub(crate) mod web;

pub(super) use desktop::parse_user_questions;
pub(crate) use read_image::ReadImageFor;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use pi_coding_agent::AgentSession;
use pi_coding_agent::extensions::RegisteredTool;
use pi_coding_agent::tools::{
    BashSpawnContext, BashSpawnHook, PowerShellToolOptions, ToolsOptions, set_env_var,
};

use super::questions::QuestionBroker;
use super::store::TaskKind;
use super::tool_context::SharedAgentToolContext;
use crate::maple_api::MapleWebTransport;

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

/// What a task's tools are set up from.
pub(crate) struct TaskToolsFor {
    pub(crate) session_id: String,
    pub(crate) kind: TaskKind,
    pub(crate) tool_context: SharedAgentToolContext,
    /// The login shell's PATH, when it could be read.
    pub(crate) login_path: Option<String>,
    /// Takes the questions the user answers.
    pub(crate) questions: QuestionBroker,
    /// Maple's web provider.
    pub(crate) web: Arc<dyn MapleWebTransport>,
    /// The task's web switch.
    pub(crate) web_enabled: bool,
    pub(crate) read_image: ReadImageFor,
}

/// A task's tools.
pub(crate) fn task_tools(task: TaskToolsFor) -> TaskTools {
    let shell_path = std::env::var_os(SHELL_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let hook = spawn_hook(task.session_id.clone(), task.tool_context, task.login_path);
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
        maple: std::iter::once(read_image::read_image_tool(task.read_image))
            .chain(match task.kind {
                TaskKind::Desktop => desktop::desktop_tools(&task.session_id, task.questions),
                TaskKind::Acp => Vec::new(),
            })
            .chain(web::web_tools(task.web, task.web_enabled))
            .collect(),
    }
}

/// Declare the web tools to the model while the task's web switch is on.
/// The change is declared with the session's next request.
pub(crate) fn sync_web_tools(session: &AgentSession, web_enabled: bool) {
    let active = session.active_tools();
    let has_web = web::WEB_TOOL_NAMES
        .iter()
        .all(|name| active.iter().any(|active| active == name));
    if has_web == web_enabled {
        return;
    }
    let mut next: Vec<String> = active
        .into_iter()
        .filter(|name| !web::WEB_TOOL_NAMES.contains(&name.as_str()))
        .collect();
    if web_enabled {
        next.extend(web::WEB_TOOL_NAMES.map(str::to_string));
    }
    session.set_active_tools(&next);
}

/// How every command of the task starts: the login PATH, the task's id, and
/// the task's tool context, read when the command starts. Nothing a command
/// started outlives it with the context's credentials.
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
        spawn.contain = context.ephemeral;
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
