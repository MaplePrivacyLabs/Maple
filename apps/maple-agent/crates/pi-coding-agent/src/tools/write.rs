//! `write`: create or overwrite a file, with any missing folders.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use pi_agent_core::{AgentTool, AgentToolResult, ToolError, ToolInvocation};
use pi_ai::Tool;
use serde::Deserialize;
use serde_json::json;

use super::mutation_queue::with_file_mutation_queue;
use super::path_utils::resolve_to_cwd;
use super::{PREFER_STRICT, ToolContext};

pub const WRITE_SNIPPET: &str = "Create or overwrite files";
pub const WRITE_GUIDELINES: [&str; 1] = ["Use write only for new files or complete rewrites."];

/// Where files are written. Replace it to write them somewhere else, for example over
/// SSH.
#[async_trait]
pub trait WriteOperations: Send + Sync {
    async fn write_file(&self, path: &Path, content: &str) -> std::io::Result<()>;
    /// Create a folder and any missing parents.
    async fn mkdir(&self, dir: &Path) -> std::io::Result<()>;
}

/// Files on this machine.
pub struct LocalWriteOperations;

#[async_trait]
impl WriteOperations for LocalWriteOperations {
    async fn write_file(&self, path: &Path, content: &str) -> std::io::Result<()> {
        tokio::fs::write(path, content).await
    }

    async fn mkdir(&self, dir: &Path) -> std::io::Result<()> {
        tokio::fs::create_dir_all(dir).await
    }
}

#[derive(Clone, Default)]
pub struct WriteToolOptions {
    /// Where files are written; this machine when `None`.
    pub operations: Option<Arc<dyn WriteOperations>>,
}

pub struct WriteTool {
    declaration: Tool,
    cwd: PathBuf,
    operations: Arc<dyn WriteOperations>,
    context: ToolContext,
}

#[derive(Deserialize)]
struct WriteParams {
    path: String,
    content: String,
}

impl WriteTool {
    pub fn new(cwd: impl Into<PathBuf>, options: WriteToolOptions, context: ToolContext) -> Self {
        Self {
            declaration: Tool::new(
                "write",
                "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to write (relative or absolute)"
                        },
                        "content": { "type": "string", "description": "Content to write to the file" }
                    },
                    "required": ["path", "content"]
                }),
            )
            .with_constrained_sampling(PREFER_STRICT),
            cwd: cwd.into(),
            operations: options
                .operations
                .unwrap_or_else(|| Arc::new(LocalWriteOperations)),
            context,
        }
    }
}

#[async_trait]
impl AgentTool for WriteTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn label(&self) -> &str {
        "write"
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        let params: WriteParams = serde_json::from_value(invocation.args)
            .map_err(|error| format!("Invalid arguments: {error}"))?;
        let cwd = self.context.cwd().unwrap_or_else(|| self.cwd.clone());
        let absolute = resolve_to_cwd(&params.path, &cwd);
        let cancel = invocation.cancel;
        // An abort is noticed between steps, so a write that started finishes before
        // the next change to the file begins.
        let check = || -> Result<(), ToolError> {
            if cancel.is_cancelled() {
                Err("Operation aborted".into())
            } else {
                Ok(())
            }
        };
        with_file_mutation_queue(&absolute, async {
            check()?;
            if let Some(dir) = absolute.parent() {
                self.operations.mkdir(dir).await?;
            }
            check()?;
            self.operations
                .write_file(&absolute, &params.content)
                .await?;
            check()?;
            Ok(AgentToolResult::text(format!(
                "Successfully wrote to {}",
                params.path
            )))
        })
        .await
    }
}
