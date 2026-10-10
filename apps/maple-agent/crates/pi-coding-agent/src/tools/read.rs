//! `read`: a text file, a page at a time, or an image as an attachment.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use pi_agent_core::{AgentTool, AgentToolResult, ToolError, ToolInvocation};
use pi_ai::{Content, Tool};
use serde::Deserialize;
use serde_json::json;

use super::image::{ImageResizeOptions, detect_supported_image_mime_type_from_file, process_image};
use super::path_utils::resolve_read_path;
use super::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, format_size, truncate_head,
};
use super::{PREFER_STRICT, ToolContext};

pub const READ_SNIPPET: &str = "Read file contents";
pub const READ_GUIDELINES: [&str; 1] = ["Use read to examine files instead of cat or sed."];

/// Where files are read from. Replace it to read them somewhere else, for example over
/// SSH.
#[async_trait]
pub trait ReadOperations: Send + Sync {
    async fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>>;
    /// Fails when the file cannot be read.
    async fn access(&self, path: &Path) -> std::io::Result<()>;
    /// The file's image type, or `None` for a file to read as text. Operations that
    /// cannot tell read every file as text.
    async fn detect_image_mime_type(&self, _path: &Path) -> std::io::Result<Option<String>> {
        Ok(None)
    }
}

/// Files on this machine.
pub struct LocalReadOperations;

#[async_trait]
impl ReadOperations for LocalReadOperations {
    async fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        tokio::fs::read(path).await
    }

    async fn access(&self, path: &Path) -> std::io::Result<()> {
        tokio::fs::File::open(path).await.map(drop)
    }

    async fn detect_image_mime_type(&self, path: &Path) -> std::io::Result<Option<String>> {
        Ok(detect_supported_image_mime_type_from_file(path)
            .await?
            .map(str::to_string))
    }
}

#[derive(Clone)]
pub struct ReadToolOptions {
    /// Resize images to fit inline image limits.
    pub auto_resize_images: bool,
    pub resize: ImageResizeOptions,
    /// Where files are read from; this machine when `None`.
    pub operations: Option<Arc<dyn ReadOperations>>,
}

impl Default for ReadToolOptions {
    fn default() -> Self {
        Self {
            auto_resize_images: true,
            resize: ImageResizeOptions::default(),
            operations: None,
        }
    }
}

pub struct ReadTool {
    declaration: Tool,
    cwd: PathBuf,
    options: ReadToolOptions,
    operations: Arc<dyn ReadOperations>,
    context: ToolContext,
}

#[derive(Deserialize)]
struct ReadParams {
    path: String,
    #[serde(default)]
    offset: Option<f64>,
    #[serde(default)]
    limit: Option<f64>,
}

impl ReadTool {
    pub fn new(cwd: impl Into<PathBuf>, options: ReadToolOptions, context: ToolContext) -> Self {
        let operations = options
            .operations
            .clone()
            .unwrap_or_else(|| Arc::new(LocalReadOperations));
        Self {
            declaration: Tool::new(
                "read",
                format!(
                    "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
                    DEFAULT_MAX_BYTES / 1024
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to read (relative or absolute)"
                        },
                        "offset": {
                            "type": "number",
                            "description": "Line number to start reading from (1-indexed)"
                        },
                        "limit": { "type": "number", "description": "Maximum number of lines to read" }
                    },
                    "required": ["path"]
                }),
            )
            .with_constrained_sampling(PREFER_STRICT),
            cwd: cwd.into(),
            options,
            operations,
            context,
        }
    }

    async fn read(&self, params: ReadParams) -> Result<AgentToolResult, ToolError> {
        let cwd = self.context.cwd().unwrap_or_else(|| self.cwd.clone());
        let path = resolve_read_path(&params.path, &cwd).await;
        self.operations
            .access(&path)
            .await
            .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
        let mime_type = self.operations.detect_image_mime_type(&path).await?;
        let non_vision_note = self
            .context
            .model()
            .filter(|model| !model.supports_images())
            .map(|_| {
                "[Current model does not support images. The image will be omitted from this request.]"
            });

        if let Some(mime_type) = mime_type {
            let bytes = self.operations.read_file(&path).await?;
            let (auto_resize, resize) =
                (self.options.auto_resize_images, self.options.resize.clone());
            let processed = {
                let mime_type = mime_type.clone();
                tokio::task::spawn_blocking(move || {
                    process_image(&bytes, &mime_type, auto_resize, &resize)
                })
                .await?
            };
            let content = match processed {
                Ok(image) => {
                    let mut note = format!("Read image file [{}]", image.mime_type);
                    for line in image
                        .hints
                        .iter()
                        .map(String::as_str)
                        .chain(non_vision_note)
                    {
                        note.push('\n');
                        note.push_str(line);
                    }
                    vec![
                        Content::text(note),
                        Content::image(image.data, image.mime_type),
                    ]
                }
                Err(message) => {
                    let mut note = format!("Read image file [{mime_type}]\n{message}");
                    if let Some(line) = non_vision_note {
                        note.push('\n');
                        note.push_str(line);
                    }
                    vec![Content::text(note)]
                }
            };
            return Ok(AgentToolResult {
                content,
                ..AgentToolResult::default()
            });
        }

        let bytes = self.operations.read_file(&path).await?;
        let text = String::from_utf8_lossy(&bytes);
        let all_lines: Vec<&str> = text.split('\n').collect();
        let total_file_lines = all_lines.len();
        let start = params
            .offset
            .filter(|offset| *offset > 0.0)
            .map_or(0, |offset| (offset - 1.0).max(0.0) as usize);
        let start_display = start + 1;
        if start >= all_lines.len() {
            return Err(format!(
                "Offset {} is beyond end of file ({} lines total)",
                params.offset.unwrap_or_default(),
                all_lines.len()
            )
            .into());
        }
        let (selected, user_limited_lines) = match params.limit {
            Some(limit) => {
                let end = start
                    .saturating_add(limit.max(0.0) as usize)
                    .min(all_lines.len());
                (all_lines[start..end].join("\n"), Some(end - start))
            }
            None => (all_lines[start..].join("\n"), None),
        };
        let truncation = truncate_head(&selected, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let mut details = None;
        let output = if truncation.first_line_exceeds_limit {
            let size = format_size(all_lines[start].len());
            details = Some(json!({ "truncation": truncation }));
            format!(
                "[Line {start_display} is {size}, exceeds {} limit. Use bash: sed -n '{start_display}p' {} | head -c {DEFAULT_MAX_BYTES}]",
                format_size(DEFAULT_MAX_BYTES),
                params.path
            )
        } else if truncation.truncated {
            let end_display = start_display + truncation.output_lines - 1;
            let next = end_display + 1;
            let note = if truncation.truncated_by == Some(TruncatedBy::Lines) {
                format!(
                    "\n\n[Showing lines {start_display}-{end_display} of {total_file_lines}. Use offset={next} to continue.]"
                )
            } else {
                format!(
                    "\n\n[Showing lines {start_display}-{end_display} of {total_file_lines} ({} limit). Use offset={next} to continue.]",
                    format_size(DEFAULT_MAX_BYTES)
                )
            };
            let output = format!("{}{note}", truncation.content);
            details = Some(json!({ "truncation": truncation }));
            output
        } else if let Some(limited) =
            user_limited_lines.filter(|limited| start + limited < all_lines.len())
        {
            let remaining = all_lines.len() - (start + limited);
            format!(
                "{}\n\n[{remaining} more lines in file. Use offset={} to continue.]",
                truncation.content,
                start + limited + 1
            )
        } else {
            truncation.content
        };
        Ok(AgentToolResult {
            content: vec![Content::text(output)],
            details,
            ..AgentToolResult::default()
        })
    }
}

#[async_trait]
impl AgentTool for ReadTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn label(&self) -> &str {
        "read"
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        let params: ReadParams = serde_json::from_value(invocation.args)
            .map_err(|error| format!("Invalid arguments: {error}"))?;
        if invocation.cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }
        tokio::select! {
            _ = invocation.cancel.cancelled() => Err("Operation aborted".into()),
            result = self.read(params) => result,
        }
    }
}
