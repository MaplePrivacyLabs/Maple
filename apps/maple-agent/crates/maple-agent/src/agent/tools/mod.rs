//! A task's tools: Pi's built-in tools, set up for Maple, and Maple's own.
//!
//! Pi's session registers its built-in tools in every task: `read`, `bash`
//! (or `powershell` in its place on Windows without Git Bash), `edit` and
//! `write`, on by default, and `grep`, `find` and `ls`, off by default.
//! Maple sets up how their commands start: from the login shell's PATH, with
//! the task's id. `MAPLE_SHELL` names the bash to run.
//!
//! Maple's own tools join these: every task gets `read_image`, desktop tasks
//! get the plan the user watches (`todo_write`) and questions the user
//! answers (`request_user_input`), and every task gets `web_search` and
//! `open_url`. For an image, `read` sends a model that cannot see images to
//! `read_image`, which describes it. A build that bundles CPython also
//! registers `python`, a persistent scratchpad, off by default. Settings →
//! Tools switches each of these, with one switch for both web tools.

mod desktop;
mod python;
mod read_image;
pub(super) mod shell;
pub(crate) mod web;

pub(super) use desktop::parse_user_questions;
pub(crate) use python::{
    ARCHIVED_REASON, PROJECT_REMOVED_REASON, PythonWorkers, SWITCHED_OFF_REASON,
    check_bundled as check_bundled_python, python_extension, python_tool,
};
pub(crate) use read_image::ReadImageFor;

use std::path::PathBuf;
use std::sync::Arc;

use pi_coding_agent::AgentSession;
use pi_coding_agent::extensions::{Extension, RegisteredTool};
use pi_coding_agent::tools::{
    BashSpawnContext, BashSpawnHook, LocalShellOperations, PowerShellToolOptions, ToolsOptions,
    set_env_var,
};

use super::questions::QuestionBroker;
use super::store::TaskKind;
use super::tool_settings::{ToolSwitch, ToolSwitches};
use crate::maple_api::MapleWebTransport;

/// The developer's choice of bash.
pub(crate) const SHELL_ENV: &str = "MAPLE_SHELL";

/// How a task's tools are set up.
pub(crate) struct TaskTools {
    pub(crate) options: ToolsOptions,
    /// The built-in tools the model gets.
    pub(crate) builtin: Vec<String>,
    /// The task's shell tool: `bash`, or `powershell` on Windows without Git
    /// Bash.
    pub(crate) shell: &'static str,
    /// Maple's own tools.
    pub(crate) maple: Vec<RegisteredTool>,
    /// Keeps the images the model meets consistent with what it can see.
    pub(crate) extension: Arc<dyn Extension>,
}

/// What a task's tools are set up from.
pub(crate) struct TaskToolsFor {
    pub(crate) session_id: String,
    pub(crate) kind: TaskKind,
    /// The login shell's PATH, when it could be read.
    pub(crate) login_path: Option<String>,
    /// Takes the questions the user answers.
    pub(crate) questions: QuestionBroker,
    /// Maple's web provider.
    pub(crate) web: Arc<dyn MapleWebTransport>,
    pub(crate) read_image: ReadImageFor,
    /// The account's switches in Settings → Tools.
    pub(crate) switches: ToolSwitches,
}

/// A task's tools.
pub(crate) fn task_tools(task: TaskToolsFor) -> TaskTools {
    let switches = task.switches;
    let shell_path = std::env::var_os(SHELL_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let hook = spawn_hook(task.session_id.clone(), task.login_path);
    let mut options = ToolsOptions::default();
    options.bash.operations = Some(shell::bounded(LocalShellOperations::bash(
        shell_path.clone(),
    )));
    options.bash.shell_path = shell_path.clone();
    options.bash.spawn_hook = Some(hook.clone());
    options.powershell = PowerShellToolOptions {
        operations: Some(shell::bounded(LocalShellOperations::powershell())),
        spawn_hook: Some(hook),
        ..PowerShellToolOptions::default()
    };
    let shell =
        if cfg!(windows) && pi_coding_agent::tools::shell_config(shell_path.as_deref()).is_err() {
            "powershell"
        } else {
            "bash"
        };
    let mut maple: Vec<RegisteredTool> =
        std::iter::once(read_image::read_image_tool(task.read_image.clone()))
            .chain(match task.kind {
                TaskKind::Desktop => desktop::desktop_tools(&task.session_id, task.questions),
                TaskKind::Acp => Vec::new(),
            })
            .chain(web::web_tools(task.web))
            .collect();
    for tool in &mut maple {
        if let Some(switch) = ToolSwitch::for_tool(tool.tool.name(), shell) {
            tool.active = switches.enabled(switch);
        }
    }
    TaskTools {
        options,
        builtin: ToolSwitch::BUILTIN
            .into_iter()
            .filter(|switch| switches.enabled(*switch))
            .flat_map(|switch| switch.tool_names(shell))
            .map(str::to_string)
            .collect(),
        shell,
        extension: read_image::read_image_extension(task.read_image),
        maple,
    }
}

/// Switch the tools `names` on or off together in a session that registered
/// them, as Pi does within a session: the change is declared with the
/// session's next request, even during a run, and its transcript records it.
/// The other tools stay as they are.
pub(crate) fn set_tools_active(session: &AgentSession, names: &[&str], active: bool) {
    let current = session.active_tools();
    let mut next: Vec<String> = current
        .iter()
        .filter(|tool| active || !names.contains(&tool.as_str()))
        .cloned()
        .collect();
    if active {
        let registered = session.all_tools();
        next.extend(
            names
                .iter()
                .filter(|name| !current.iter().any(|tool| tool == **name))
                .filter(|name| registered.iter().any(|tool| tool.name == **name))
                .map(|name| name.to_string()),
        );
    }
    if next != current {
        session.set_active_tools(&next);
    }
}

/// How every command of the task starts: on the login PATH, with the task's
/// id.
fn spawn_hook(session_id: String, login_path: Option<String>) -> BashSpawnHook {
    Arc::new(move |mut spawn: BashSpawnContext| {
        if let Some(path) = &login_path {
            set_env_var(&mut spawn.env, "PATH", path.clone());
        }
        set_env_var(&mut spawn.env, "AGENT_SESSION_ID", session_id.clone());
        spawn
    })
}

#[cfg(test)]
mod tests;
