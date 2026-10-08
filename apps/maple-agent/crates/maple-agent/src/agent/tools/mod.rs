//! Maple's tools as Pi tools.
//!
//! Pi's coding agent ships no tools; the host registers its own. These are
//! Maple's, with the names, schemas and limits its tasks have always had.
//! Each tool is built for one task and resolves relative paths against that
//! task's working directory.

mod files;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use pi_agent_core::{AgentTool, AgentToolResult, ToolError, ToolInvocation};
use pi_ai::Tool;
use pi_coding_agent::extensions::{RegisteredTool, ToolPrompt};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use files::{EditParams, MAX_EDIT_BYTES, MAX_READ_BYTES, MAX_READ_LINES, ReadParams, WriteParams};

/// The tools of a task working in `cwd`.
pub(crate) fn maple_tools(cwd: PathBuf) -> Vec<RegisteredTool> {
    vec![
        registered(
            ReadTool { cwd: cwd.clone() },
            "Read file contents",
            &["Use read to examine files instead of cat or sed."],
        ),
        registered(
            EditTool { cwd: cwd.clone() },
            "Make precise file edits with exact text replacement, including multiple disjoint edits in one call",
            &[
                "Use edit for precise changes (edits[].oldText must match exactly)",
                "When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls",
                "Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.",
                "Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.",
            ],
        ),
        registered(
            WriteTool { cwd },
            "Create or overwrite files",
            &["Use write only for new files or complete rewrites."],
        ),
    ]
}

fn registered(
    tool: impl AgentTool + 'static,
    snippet: &str,
    guidelines: &[&str],
) -> RegisteredTool {
    RegisteredTool {
        tool: Arc::new(tool),
        prompt: ToolPrompt {
            snippet: Some(snippet.to_string()),
            guidelines: guidelines.iter().map(|rule| rule.to_string()).collect(),
        },
        active: true,
        extension: None,
    }
}

fn parse<T: DeserializeOwned>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|error| format!("Invalid arguments: {error}").into())
}

/// A tool's text, or its failure as the error Pi reports to the model.
fn outcome(result: Result<String, String>) -> Result<AgentToolResult, ToolError> {
    result.map(AgentToolResult::text).map_err(Into::into)
}

static READ_DECLARATION: once_cell::sync::Lazy<Tool> = once_cell::sync::Lazy::new(|| {
    Tool::new(
        "read",
        format!(
            "Read a local text file. Output is limited to {MAX_READ_LINES} lines or {}KB, whichever is reached first. Use offset and limit to continue through large files. Use read_image for images.",
            MAX_READ_BYTES / 1024
        ),
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read (relative or absolute)"
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Line number to start reading from (1-indexed)"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of lines to read"
                }
            },
            "required": ["path"]
        }),
    )
});

static EDIT_DECLARATION: once_cell::sync::Lazy<Tool> = once_cell::sync::Lazy::new(|| {
    Tool::new(
        "edit",
        format!(
            "Apply one or more exact, unique text replacements to a file up to {}MB. Every oldText is matched against the original file, all replacements are validated before writing, and overlapping edits are rejected.",
            MAX_EDIT_BYTES / (1024 * 1024)
        ),
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit (relative or absolute)"
                },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Exact, non-overlapping replacements matched against the original file",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "oldText": {
                                "type": "string",
                                "minLength": 1,
                                "description": "Exact text that must occur once in the original file"
                            },
                            "newText": {
                                "type": "string",
                                "description": "Replacement text; use an empty string to delete"
                            }
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            },
            "required": ["path", "edits"]
        }),
    )
});

static WRITE_DECLARATION: once_cell::sync::Lazy<Tool> = once_cell::sync::Lazy::new(|| {
    Tool::new(
        "write",
        "Write content to a file. Creates the file if it does not exist, overwrites it if it does, and creates parent directories as needed.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write (relative or absolute)"
                },
                "content": {
                    "type": "string",
                    "description": "Complete content to write to the file"
                }
            },
            "required": ["path", "content"]
        }),
    )
});

struct ReadTool {
    cwd: PathBuf,
}

#[async_trait]
impl AgentTool for ReadTool {
    fn declaration(&self) -> &Tool {
        &READ_DECLARATION
    }

    fn label(&self) -> &str {
        "Read"
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        let params: ReadParams = parse(invocation.args)?;
        outcome(files::read_file(params, Some(&self.cwd), invocation.cancel).await)
    }
}

struct EditTool {
    cwd: PathBuf,
}

#[async_trait]
impl AgentTool for EditTool {
    fn declaration(&self) -> &Tool {
        &EDIT_DECLARATION
    }

    fn label(&self) -> &str {
        "Edit"
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        let params: EditParams = parse(invocation.args)?;
        outcome(files::edit_file(params, Some(&self.cwd), invocation.cancel).await)
    }
}

struct WriteTool {
    cwd: PathBuf,
}

#[async_trait]
impl AgentTool for WriteTool {
    fn declaration(&self) -> &Tool {
        &WRITE_DECLARATION
    }

    fn label(&self) -> &str {
        "Write"
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        let params: WriteParams = parse(invocation.args)?;
        outcome(files::write_file(params, Some(&self.cwd), invocation.cancel).await)
    }
}
