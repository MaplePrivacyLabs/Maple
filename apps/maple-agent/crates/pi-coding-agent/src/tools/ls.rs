//! `ls`: a folder's entries, sorted, with `/` after folders.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use pi_agent_core::{AgentTool, AgentToolResult, ToolError, ToolInvocation};
use pi_ai::{Content, Tool};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::ToolContext;
use super::path_utils::resolve_to_cwd;
use super::truncate::{DEFAULT_MAX_BYTES, format_size, truncate_head};

pub const LS_SNIPPET: &str = "List directory contents";
const DEFAULT_LIMIT: usize = 500;

/// Where folders are listed. Replace it to list them somewhere else, for example over
/// SSH.
#[async_trait]
pub trait LsOperations: Send + Sync {
    async fn exists(&self, path: &Path) -> bool;
    /// Whether `path`, or what a link there points to, is a folder. Fails when it
    /// cannot be read.
    async fn is_directory(&self, path: &Path) -> std::io::Result<bool>;
    /// The names of a folder's entries.
    async fn read_dir(&self, path: &Path) -> std::io::Result<Vec<String>>;
}

/// Folders on this machine.
pub struct LocalLsOperations;

#[async_trait]
impl LsOperations for LocalLsOperations {
    async fn exists(&self, path: &Path) -> bool {
        tokio::fs::try_exists(path).await.unwrap_or(false)
    }

    async fn is_directory(&self, path: &Path) -> std::io::Result<bool> {
        Ok(tokio::fs::metadata(path).await?.is_dir())
    }

    async fn read_dir(&self, path: &Path) -> std::io::Result<Vec<String>> {
        let mut entries = tokio::fs::read_dir(path).await?;
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        Ok(names)
    }
}

#[derive(Clone, Default)]
pub struct LsToolOptions {
    /// Where folders are listed; this machine when `None`.
    pub operations: Option<Arc<dyn LsOperations>>,
}

pub struct LsTool {
    declaration: Tool,
    cwd: PathBuf,
    operations: Arc<dyn LsOperations>,
    context: ToolContext,
}

#[derive(Deserialize)]
struct LsParams {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<f64>,
}

impl LsTool {
    pub fn new(cwd: impl Into<PathBuf>, options: LsToolOptions, context: ToolContext) -> Self {
        Self {
            declaration: Tool::new(
                "ls",
                format!(
                    "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to {DEFAULT_LIMIT} entries or {}KB (whichever is hit first).",
                    DEFAULT_MAX_BYTES / 1024
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory to list (default: current directory)"
                        },
                        "limit": {
                            "type": "number",
                            "description": "Maximum number of entries to return (default: 500)"
                        }
                    }
                }),
            ),
            cwd: cwd.into(),
            operations: options
                .operations
                .unwrap_or_else(|| Arc::new(LocalLsOperations)),
            context,
        }
    }

    async fn list(&self, params: LsParams) -> Result<AgentToolResult, ToolError> {
        let cwd = self.context.cwd().unwrap_or_else(|| self.cwd.clone());
        let path = params.path.as_deref().filter(|path| !path.is_empty());
        let dir = resolve_to_cwd(path.unwrap_or("."), &cwd);
        let limit = params
            .limit
            .map_or(DEFAULT_LIMIT, |limit| limit.max(1.0) as usize);
        if !self.operations.exists(&dir).await {
            return Err(format!("Path not found: {}", dir.display()).into());
        }
        if !self.operations.is_directory(&dir).await? {
            return Err(format!("Not a directory: {}", dir.display()).into());
        }
        let mut entries = self
            .operations
            .read_dir(&dir)
            .await
            .map_err(|error| format!("Cannot read directory: {error}"))?;
        entries.sort_by(|a, b| {
            a.to_lowercase()
                .cmp(&b.to_lowercase())
                .then_with(|| a.cmp(b))
        });

        let mut results = Vec::new();
        let mut limit_reached = false;
        for entry in entries {
            if results.len() >= limit {
                limit_reached = true;
                break;
            }
            // Entries that cannot be read, such as broken links, are left out.
            match self.operations.is_directory(&dir.join(&entry)).await {
                Ok(true) => results.push(format!("{entry}/")),
                Ok(false) => results.push(entry),
                Err(_) => {}
            }
        }
        if results.is_empty() {
            return Ok(AgentToolResult::text("(empty directory)"));
        }

        // The entry limit already caps the lines, so only bytes are limited here.
        let truncation = truncate_head(&results.join("\n"), usize::MAX, DEFAULT_MAX_BYTES);
        let mut output = truncation.content.clone();
        let mut details = Map::new();
        let mut notices = Vec::new();
        if limit_reached {
            notices.push(format!(
                "{limit} entries limit reached. Use limit={} for more",
                limit.saturating_mul(2)
            ));
            details.insert("entryLimitReached".to_string(), json!(limit));
        }
        if truncation.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
            details.insert("truncation".to_string(), json!(truncation));
        }
        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }
        Ok(AgentToolResult {
            content: vec![Content::text(output)],
            details: (!details.is_empty()).then_some(Value::Object(details)),
            ..AgentToolResult::default()
        })
    }
}

#[async_trait]
impl AgentTool for LsTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn label(&self) -> &str {
        "ls"
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        let params: LsParams = serde_json::from_value(invocation.args)
            .map_err(|error| format!("Invalid arguments: {error}"))?;
        if invocation.cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }
        tokio::select! {
            _ = invocation.cancel.cancelled() => Err("Operation aborted".into()),
            result = self.list(params) => result,
        }
    }
}
