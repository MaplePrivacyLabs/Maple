//! `edit`: exact text replacements in one file, several disjoint ones per call.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use pi_agent_core::{AgentTool, AgentToolResult, ToolError, ToolInvocation};
use pi_ai::{Content, Tool};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::edit_diff::{
    Edit, apply_edits_to_normalized_content, detect_line_ending, generate_diff_string,
    generate_unified_patch, normalize_to_lf, restore_line_endings, split_bom,
};
use super::mutation_queue::with_file_mutation_queue;
use super::path_utils::resolve_to_cwd;
use super::{PREFER_STRICT, ToolContext};

pub const EDIT_SNIPPET: &str = "Make precise file edits with exact text replacement, including multiple disjoint edits in one call";
pub const EDIT_GUIDELINES: [&str; 4] = [
    "Use edit for precise changes (edits[].oldText must match exactly)",
    "When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls",
    "Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.",
    "Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.",
];

/// Where files are edited. Replace it to edit them somewhere else, for example over
/// SSH.
#[async_trait]
pub trait EditOperations: Send + Sync {
    async fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>>;
    async fn write_file(&self, path: &Path, content: &str) -> std::io::Result<()>;
    /// Fails when the file cannot be both read and written.
    async fn access(&self, path: &Path) -> std::io::Result<()>;
}

/// Files on this machine.
pub struct LocalEditOperations;

#[async_trait]
impl EditOperations for LocalEditOperations {
    async fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        tokio::fs::read(path).await
    }

    async fn write_file(&self, path: &Path, content: &str) -> std::io::Result<()> {
        tokio::fs::write(path, content).await
    }

    async fn access(&self, path: &Path) -> std::io::Result<()> {
        tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .await
            .map(drop)
    }
}

#[derive(Clone, Default)]
pub struct EditToolOptions {
    /// Where files are edited; this machine when `None`.
    pub operations: Option<Arc<dyn EditOperations>>,
}

pub struct EditTool {
    declaration: Tool,
    cwd: PathBuf,
    operations: Arc<dyn EditOperations>,
    context: ToolContext,
}

#[derive(Deserialize)]
struct EditParams {
    path: String,
    edits: Vec<EditEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditEntry {
    old_text: String,
    new_text: String,
}

fn is_single_edit(value: &Value) -> bool {
    value.as_object().is_some_and(|edit| {
        edit.get("oldText").is_some_and(Value::is_string)
            && edit.get("newText").is_some_and(Value::is_string)
    })
}

impl EditTool {
    pub fn new(cwd: impl Into<PathBuf>, options: EditToolOptions, context: ToolContext) -> Self {
        Self {
            declaration: Tool::new(
                "edit",
                "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to edit (relative or absolute)"
                        },
                        "edits": {
                            "type": "array",
                            "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "oldText": {
                                        "type": "string",
                                        "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."
                                    },
                                    "newText": {
                                        "type": "string",
                                        "description": "Replacement text for this targeted edit."
                                    }
                                },
                                "required": ["oldText", "newText"]
                            }
                        }
                    },
                    "required": ["path", "edits"]
                }),
            )
            .with_constrained_sampling(PREFER_STRICT),
            cwd: cwd.into(),
            operations: options
                .operations
                .unwrap_or_else(|| Arc::new(LocalEditOperations)),
            context,
        }
    }

    async fn edit(
        &self,
        path: &str,
        absolute: &Path,
        edits: &[Edit],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<AgentToolResult, ToolError> {
        // An abort is noticed between steps, never in the middle of one, so the queue
        // stays held until a write that started has finished.
        let check = || -> Result<(), ToolError> {
            if cancel.is_cancelled() {
                Err("Operation aborted".into())
            } else {
                Ok(())
            }
        };
        check()?;
        if let Err(error) = self.operations.access(absolute).await {
            check()?;
            return Err(format!("Could not edit file: {path}. {error}.").into());
        }
        check()?;
        let raw = String::from_utf8_lossy(&self.operations.read_file(absolute).await?).into_owned();
        check()?;
        let (bom, content) = split_bom(&raw);
        let ending = detect_line_ending(content);
        let normalized = normalize_to_lf(content);
        let (base, new) = apply_edits_to_normalized_content(&normalized, edits, path)?;
        check()?;
        let final_content = format!("{bom}{}", restore_line_endings(&new, ending));
        self.operations.write_file(absolute, &final_content).await?;
        check()?;
        let (diff, first_changed_line) = generate_diff_string(&base, &new, 4);
        let patch = generate_unified_patch(path, &base, &new, 4);
        Ok(AgentToolResult {
            content: vec![Content::text(format!(
                "Successfully replaced {} block(s) in {path}.",
                edits.len()
            ))],
            details: Some(json!({
                "diff": diff,
                "patch": patch,
                "firstChangedLine": first_changed_line,
            })),
            ..AgentToolResult::default()
        })
    }
}

#[async_trait]
impl AgentTool for EditTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn label(&self) -> &str {
        "edit"
    }

    /// Accept the shapes models send besides an array: `edits` as a JSON string, one
    /// edit object, or the older top-level `oldText` and `newText`.
    fn prepare_arguments(&self, mut arguments: Map<String, Value>) -> Map<String, Value> {
        match arguments.get("edits") {
            Some(Value::String(text)) => {
                if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                    if parsed.is_array() {
                        arguments.insert("edits".to_string(), parsed);
                    } else if is_single_edit(&parsed) {
                        arguments.insert("edits".to_string(), Value::Array(vec![parsed]));
                    }
                }
            }
            Some(single) if is_single_edit(single) => {
                let single = single.clone();
                arguments.insert("edits".to_string(), Value::Array(vec![single]));
            }
            _ => {}
        }
        if let (Some(Value::String(_)), Some(Value::String(_))) =
            (arguments.get("oldText"), arguments.get("newText"))
        {
            let old_text = arguments.remove("oldText").unwrap_or_default();
            let new_text = arguments.remove("newText").unwrap_or_default();
            let mut edits = match arguments.remove("edits") {
                Some(Value::Array(edits)) => edits,
                _ => Vec::new(),
            };
            edits.push(json!({ "oldText": old_text, "newText": new_text }));
            arguments.insert("edits".to_string(), Value::Array(edits));
        }
        arguments
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        let params: EditParams = serde_json::from_value(invocation.args)
            .map_err(|error| format!("Invalid arguments: {error}"))?;
        if params.edits.is_empty() {
            return Err(
                "Edit tool input is invalid. edits must contain at least one replacement.".into(),
            );
        }
        let edits: Vec<Edit> = params
            .edits
            .into_iter()
            .map(|entry| Edit {
                old_text: entry.old_text,
                new_text: entry.new_text,
            })
            .collect();
        let cwd = self.context.cwd().unwrap_or_else(|| self.cwd.clone());
        let absolute = resolve_to_cwd(&params.path, &cwd);
        with_file_mutation_queue(
            &absolute,
            self.edit(&params.path, &absolute, &edits, &invocation.cancel),
        )
        .await
    }
}
