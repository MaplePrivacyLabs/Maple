//! Pi's built-in tools: `read`, `bash`, `edit` and `write`, `grep`, `find` and `ls`, and
//! `powershell` for Windows.
//!
//! As in Pi, an [`AgentSession`](crate::AgentSession) creates them for its folder and
//! gives the model `read`, `bash`, `edit` and `write`; the others stay registered and can
//! be turned on. [`ToolsOptions`] configures them, and a host or extension tool with the
//! same name replaces one. Each tool takes operations that can be swapped to run it
//! somewhere else, and `bash` takes a spawn hook to adjust a command's environment.

mod bash;
mod edit;
mod edit_diff;
mod find;
mod grep;
mod image;
mod ls;
mod mutation_queue;
mod output;
mod path_utils;
mod read;
mod shell;
mod truncate;
mod walk;
mod write;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pi_ai::Model;

use crate::extensions::{ExtensionContext, RegisteredTool, ToolPrompt};

pub use bash::{
    BASH_SNIPPET, BashOperations, BashSpawnContext, BashSpawnHook, BashToolOptions, ExecError,
    ExecOptions, LocalShellOperations, OnData, POWERSHELL_SNIPPET, PowerShellToolOptions,
    ShellTool,
};
pub use edit::{
    EDIT_GUIDELINES, EDIT_SNIPPET, EditOperations, EditTool, EditToolOptions, LocalEditOperations,
};
pub use edit_diff::{
    Edit, apply_edits_to_normalized_content, generate_diff_string, generate_unified_patch,
};
pub use find::{
    FIND_SNIPPET, FindGlobOptions, FindOperations, FindTool, FindToolOptions, LocalFindOperations,
};
pub use grep::{GREP_SNIPPET, GrepOperations, GrepTool, GrepToolOptions, LocalGrepOperations};
pub use image::{
    ImageResizeOptions, ProcessedImage, ResizedImage, detect_supported_image_mime_type,
    process_image, resize_image,
};
pub use ls::{LS_SNIPPET, LocalLsOperations, LsOperations, LsTool, LsToolOptions};
pub use mutation_queue::with_file_mutation_queue;
pub use output::{OutputAccumulator, OutputSnapshot};
pub use path_utils::{expand_path, resolve_read_path, resolve_to_cwd};
pub use read::{
    LocalReadOperations, READ_GUIDELINES, READ_SNIPPET, ReadOperations, ReadTool, ReadToolOptions,
};
pub use shell::{
    CommandTransport, ShellConfig, env_var, kill_process_tree, kill_tracked_children,
    powershell_config, set_env_var, shell_config, shell_env,
};
pub use truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, GREP_MAX_LINE_LENGTH, TruncatedBy, TruncationResult,
    format_size, truncate_head, truncate_line, truncate_tail,
};
pub use write::{
    LocalWriteOperations, WRITE_GUIDELINES, WRITE_SNIPPET, WriteOperations, WriteTool,
    WriteToolOptions,
};

/// The built-in tools the model gets unless the host chooses others.
pub const DEFAULT_TOOL_NAMES: [&str; 4] = ["read", "bash", "edit", "write"];

/// The built-in tools that only read: Pi's read-only set.
pub const READ_ONLY_TOOL_NAMES: [&str; 4] = ["read", "grep", "find", "ls"];

/// Every built-in tool, in Pi's order.
pub const ALL_TOOL_NAMES: [&str; 8] = [
    "read",
    "bash",
    "powershell",
    "edit",
    "write",
    "grep",
    "find",
    "ls",
];

/// How the built-in tools are set up.
#[derive(Clone, Default)]
pub struct ToolsOptions {
    pub read: ReadToolOptions,
    pub bash: BashToolOptions,
    pub powershell: PowerShellToolOptions,
    pub edit: EditToolOptions,
    pub write: WriteToolOptions,
    pub grep: GrepToolOptions,
    pub find: FindToolOptions,
    pub ls: LsToolOptions,
}

/// What a built-in tool reads from the session that runs it, as Pi's tools read their
/// extension context: the session's folder, model, thinking level and ids. Outside a
/// session it has only the app's name, which names the tools' environment variables and
/// output files.
#[derive(Clone)]
pub struct ToolContext {
    app_name: Arc<str>,
    session: Option<ExtensionContext>,
}

impl ToolContext {
    /// A context for tools that run outside a session.
    pub fn new(app_name: &str) -> Self {
        Self {
            app_name: Arc::from(app_name),
            session: None,
        }
    }

    pub(crate) fn for_session(app_name: &str, session: ExtensionContext) -> Self {
        Self {
            app_name: Arc::from(app_name),
            session: Some(session),
        }
    }

    pub fn app_name(&self) -> &str {
        &self.app_name
    }

    fn in_session(&self) -> bool {
        self.session.is_some()
    }

    fn cwd(&self) -> Option<PathBuf> {
        self.session.as_ref()?.cwd()
    }

    fn model(&self) -> Option<Model> {
        self.session.as_ref()?.model()
    }

    fn session_id(&self) -> Option<String> {
        self.session.as_ref()?.session_id()
    }

    fn session_file(&self) -> Option<PathBuf> {
        self.session.as_ref()?.session_file()
    }

    fn thinking_level(&self) -> Option<String> {
        let level = self.session.as_ref()?.thinking_level();
        serde_json::to_value(level)
            .ok()?
            .as_str()
            .map(str::to_string)
    }

    /// The prefix of the session variables commands get: `MAPLE` for Maple.
    fn env_prefix(&self) -> String {
        let prefix: String = self
            .app_name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect();
        if prefix.is_empty() {
            "AGENT".to_string()
        } else {
            prefix
        }
    }

    /// The name output files of `tool` start with: `maple-bash` for Maple's `bash`.
    fn output_file_prefix(&self, tool: &str) -> String {
        let app: String = self
            .app_name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        if app.is_empty() {
            tool.to_string()
        } else {
            format!("{app}-{tool}")
        }
    }
}

fn registered(
    tool: impl pi_agent_core::AgentTool + 'static,
    snippet: &str,
    guidelines: Vec<String>,
    active: bool,
) -> RegisteredTool {
    RegisteredTool {
        tool: Arc::new(tool),
        prompt: ToolPrompt {
            snippet: Some(snippet.to_string()),
            guidelines,
        },
        active,
        extension: None,
    }
}

fn strings(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| line.to_string()).collect()
}

/// The built-in tool `name` for `cwd`, or `None` for a name that is not one.
pub fn create_tool(
    name: &str,
    cwd: &Path,
    options: &ToolsOptions,
    context: &ToolContext,
    active: bool,
) -> Option<RegisteredTool> {
    let context = context.clone();
    Some(match name {
        "read" => registered(
            ReadTool::new(cwd, options.read.clone(), context),
            READ_SNIPPET,
            strings(&READ_GUIDELINES),
            active,
        ),
        "bash" => {
            let tool = ShellTool::bash(cwd, options.bash.clone(), context);
            let guidelines = tool.guidelines();
            registered(tool, BASH_SNIPPET, guidelines, active)
        }
        "powershell" => {
            let tool = ShellTool::powershell(cwd, options.powershell.clone(), context);
            let guidelines = tool.guidelines();
            registered(tool, POWERSHELL_SNIPPET, guidelines, active)
        }
        "edit" => registered(
            EditTool::new(cwd, options.edit.clone(), context),
            EDIT_SNIPPET,
            strings(&EDIT_GUIDELINES),
            active,
        ),
        "write" => registered(
            WriteTool::new(cwd, options.write.clone(), context),
            WRITE_SNIPPET,
            strings(&WRITE_GUIDELINES),
            active,
        ),
        "grep" => registered(
            GrepTool::new(cwd, options.grep.clone(), context),
            GREP_SNIPPET,
            Vec::new(),
            active,
        ),
        "find" => registered(
            FindTool::new(cwd, options.find.clone(), context),
            FIND_SNIPPET,
            Vec::new(),
            active,
        ),
        "ls" => registered(
            LsTool::new(cwd, options.ls.clone(), context),
            LS_SNIPPET,
            Vec::new(),
            active,
        ),
        _ => return None,
    })
}

/// Every built-in tool for `cwd`; those named in `active` are given to the model.
pub fn create_all_tools(
    cwd: &Path,
    options: &ToolsOptions,
    context: &ToolContext,
    active: &[String],
) -> Vec<RegisteredTool> {
    ALL_TOOL_NAMES
        .iter()
        .filter_map(|name| {
            create_tool(
                name,
                cwd,
                options,
                context,
                active.iter().any(|on| on == name),
            )
        })
        .collect()
}

/// `read`, `bash`, `edit` and `write` for `cwd`, all active.
pub fn create_coding_tools(
    cwd: &Path,
    options: &ToolsOptions,
    context: &ToolContext,
) -> Vec<RegisteredTool> {
    DEFAULT_TOOL_NAMES
        .iter()
        .filter_map(|name| create_tool(name, cwd, options, context, true))
        .collect()
}

/// `read`, `grep`, `find` and `ls` for `cwd`, all active.
pub fn create_read_only_tools(
    cwd: &Path,
    options: &ToolsOptions,
    context: &ToolContext,
) -> Vec<RegisteredTool> {
    READ_ONLY_TOOL_NAMES
        .iter()
        .filter_map(|name| create_tool(name, cwd, options, context, true))
        .collect()
}

#[cfg(test)]
mod tests;
