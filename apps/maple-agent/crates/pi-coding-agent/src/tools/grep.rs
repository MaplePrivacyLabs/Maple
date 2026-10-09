//! `grep`: search file contents for a pattern, natively, where Pi runs ripgrep.
//!
//! The search follows ripgrep's rules as Pi runs it: patterns are Rust regular
//! expressions (ripgrep's syntax), hidden files are searched, ignore files are
//! respected, a file's search stops at a NUL byte, as it does for binary files, and
//! `glob` picks files the way ripgrep's `--glob` does. Matches come in path order.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use ignore::overrides::OverrideBuilder;
use pi_agent_core::{AgentTool, AgentToolResult, ToolError, ToolInvocation};
use pi_ai::{Content, Tool};
use regex::bytes::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::ToolContext;
use super::path_utils::resolve_to_cwd;
use super::truncate::{
    DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH, format_size, truncate_head, truncate_line,
};
use super::walk::{relative_slash_path, run_blocking, walker};

pub const GREP_SNIPPET: &str = "Search file contents for patterns (respects .gitignore)";
const DEFAULT_LIMIT: usize = 100;
/// The most of a matching line kept, far more than is shown.
const MAX_KEPT_LINE_BYTES: usize = 64 * 1024;

/// How `grep` checks its path and reads the lines around a match. Replace it to read
/// them somewhere else; as in Pi, the search itself runs on this machine.
#[async_trait]
pub trait GrepOperations: Send + Sync {
    /// Whether `path` is a folder. Fails when it does not exist.
    async fn is_directory(&self, path: &Path) -> std::io::Result<bool>;
    /// A file's text, for the lines around a match.
    async fn read_file(&self, path: &Path) -> std::io::Result<String>;
}

/// Files on this machine.
pub struct LocalGrepOperations;

#[async_trait]
impl GrepOperations for LocalGrepOperations {
    async fn is_directory(&self, path: &Path) -> std::io::Result<bool> {
        Ok(tokio::fs::metadata(path).await?.is_dir())
    }

    async fn read_file(&self, path: &Path) -> std::io::Result<String> {
        let bytes = tokio::fs::read(path).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

#[derive(Clone, Default)]
pub struct GrepToolOptions {
    /// How the path is checked and context lines are read; this machine when `None`.
    pub operations: Option<Arc<dyn GrepOperations>>,
}

pub struct GrepTool {
    declaration: Tool,
    cwd: PathBuf,
    operations: Arc<dyn GrepOperations>,
    context: ToolContext,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrepParams {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    ignore_case: Option<bool>,
    #[serde(default)]
    literal: Option<bool>,
    #[serde(default)]
    context: Option<f64>,
    #[serde(default)]
    limit: Option<f64>,
}

/// A matching line.
struct Match {
    path: PathBuf,
    line_number: usize,
    line: String,
}

/// One search: a file, or a folder's files, up to `limit` matches.
struct Search {
    root: PathBuf,
    is_directory: bool,
    regex: Regex,
    glob: Option<String>,
    limit: usize,
}

impl Search {
    fn run(&self, stop: &AtomicBool) -> Result<Vec<Match>, String> {
        let mut matches = Vec::new();
        if !self.is_directory {
            // A pipe or device is not read, so the search cannot hang on one.
            if std::fs::metadata(&self.root).is_ok_and(|metadata| metadata.is_file()) {
                self.search_file(&self.root, &mut matches, stop);
            }
            return Ok(matches);
        }
        let mut builder = walker(&self.root, ".rgignore", None);
        if let Some(glob) = &self.glob {
            let mut overrides = OverrideBuilder::new(&self.root);
            overrides.add(glob).map_err(|error| error.to_string())?;
            builder.overrides(overrides.build().map_err(|error| error.to_string())?);
        }
        for entry in builder.build() {
            if matches.len() >= self.limit || stop.load(Ordering::Relaxed) {
                break;
            }
            // Entries that cannot be read are passed over.
            let Ok(entry) = entry else { continue };
            if entry.file_type().is_some_and(|kind| kind.is_file()) {
                self.search_file(entry.path(), &mut matches, stop);
            }
        }
        Ok(matches)
    }

    /// Adds `path`'s matching lines. As in ripgrep, a NUL byte marks a binary file and
    /// ends its search.
    fn search_file(&self, path: &Path, matches: &mut Vec<Match>, stop: &AtomicBool) {
        let Ok(file) = File::open(path) else { return };
        let mut reader = BufReader::with_capacity(64 * 1024, file);
        let mut line = Vec::new();
        let mut line_number = 0;
        while matches.len() < self.limit {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            line_number += 1;
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.contains(&0) {
                return;
            }
            if self.regex.is_match(&line) {
                let kept = &line[..line.len().min(MAX_KEPT_LINE_BYTES)];
                matches.push(Match {
                    path: path.to_path_buf(),
                    line_number,
                    line: String::from_utf8_lossy(kept).into_owned(),
                });
            }
            if line_number % 4096 == 0 && stop.load(Ordering::Relaxed) {
                return;
            }
        }
    }
}

/// How a match's file is named: from the searched folder, or by its name when one
/// file was searched.
fn format_path(path: &Path, root: &Path, is_directory: bool) -> String {
    if is_directory
        && let Some(relative) = relative_slash_path(path, root).filter(|path| !path.is_empty())
    {
        return relative;
    }
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// A line as shown: without carriage returns, and cut when long.
fn shown(line: &str, truncated: &mut bool) -> String {
    let (text, cut) = truncate_line(&line.replace('\r', ""), GREP_MAX_LINE_LENGTH);
    *truncated |= cut;
    text
}

impl GrepTool {
    pub fn new(cwd: impl Into<PathBuf>, options: GrepToolOptions, context: ToolContext) -> Self {
        Self {
            declaration: Tool::new(
                "grep",
                format!(
                    "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} matches or {}KB (whichever is hit first). Long lines are truncated to {GREP_MAX_LINE_LENGTH} chars.",
                    DEFAULT_MAX_BYTES / 1024
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Search pattern (regex or literal string)"
                        },
                        "path": {
                            "type": "string",
                            "description": "Directory or file to search (default: current directory)"
                        },
                        "glob": {
                            "type": "string",
                            "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'"
                        },
                        "ignoreCase": {
                            "type": "boolean",
                            "description": "Case-insensitive search (default: false)"
                        },
                        "literal": {
                            "type": "boolean",
                            "description": "Treat pattern as literal string instead of regex (default: false)"
                        },
                        "context": {
                            "type": "number",
                            "description": "Number of lines to show before and after each match (default: 0)"
                        },
                        "limit": {
                            "type": "number",
                            "description": "Maximum number of matches to return (default: 100)"
                        }
                    },
                    "required": ["pattern"]
                }),
            ),
            cwd: cwd.into(),
            operations: options
                .operations
                .unwrap_or_else(|| Arc::new(LocalGrepOperations)),
            context,
        }
    }

    async fn grep(&self, params: GrepParams) -> Result<AgentToolResult, ToolError> {
        let cwd = self.context.cwd().unwrap_or_else(|| self.cwd.clone());
        let path = params.path.as_deref().filter(|path| !path.is_empty());
        let root = resolve_to_cwd(path.unwrap_or("."), &cwd);
        let is_directory = self
            .operations
            .is_directory(&root)
            .await
            .map_err(|_| format!("Path not found: {}", root.display()))?;
        let context = params
            .context
            .filter(|lines| *lines > 0.0)
            .map_or(0, |lines| lines as usize);
        let limit = params
            .limit
            .map_or(DEFAULT_LIMIT, |limit| limit.max(1.0) as usize);
        let pattern = if params.literal == Some(true) {
            regex::escape(&params.pattern)
        } else {
            params.pattern
        };
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(params.ignore_case == Some(true))
            .build()
            .map_err(|error| error.to_string())?;
        let search = Search {
            root: root.clone(),
            is_directory,
            regex,
            glob: params.glob.filter(|glob| !glob.is_empty()),
            limit,
        };
        let matches = run_blocking(move |stop| search.run(stop)).await??;
        if matches.is_empty() {
            return Ok(AgentToolResult::text("No matches found"));
        }

        let mut lines_truncated = false;
        let mut output_lines = Vec::new();
        let mut files: HashMap<PathBuf, Vec<String>> = HashMap::new();
        for found in &matches {
            let name = format_path(&found.path, &root, is_directory);
            if context == 0 {
                let text = shown(&found.line, &mut lines_truncated);
                output_lines.push(format!("{name}:{}: {text}", found.line_number));
                continue;
            }
            if let Entry::Vacant(slot) = files.entry(found.path.clone()) {
                let lines = match self.operations.read_file(&found.path).await {
                    Ok(content) => content.split('\n').map(str::to_string).collect(),
                    Err(_) => Vec::new(),
                };
                slot.insert(lines);
            }
            let lines = &files[&found.path];
            if lines.is_empty() {
                output_lines.push(format!(
                    "{name}:{}: (unable to read file)",
                    found.line_number
                ));
                continue;
            }
            let start = found.line_number.saturating_sub(context).max(1);
            let end = (found.line_number + context).min(lines.len());
            for number in start..=end {
                let text = shown(
                    lines.get(number - 1).map_or("", String::as_str),
                    &mut lines_truncated,
                );
                if number == found.line_number {
                    output_lines.push(format!("{name}:{number}: {text}"));
                } else {
                    output_lines.push(format!("{name}-{number}- {text}"));
                }
            }
        }

        // The match limit already caps the lines, so only bytes are limited here.
        let truncation = truncate_head(&output_lines.join("\n"), usize::MAX, DEFAULT_MAX_BYTES);
        let mut output = truncation.content.clone();
        let mut details = Map::new();
        let mut notices = Vec::new();
        if matches.len() >= limit {
            notices.push(format!(
                "{limit} matches limit reached. Use limit={} for more, or refine pattern",
                limit.saturating_mul(2)
            ));
            details.insert("matchLimitReached".to_string(), json!(limit));
        }
        if truncation.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
            details.insert("truncation".to_string(), json!(truncation));
        }
        if lines_truncated {
            notices.push(format!(
                "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
            ));
            details.insert("linesTruncated".to_string(), json!(true));
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
impl AgentTool for GrepTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn label(&self) -> &str {
        "grep"
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        let params: GrepParams = serde_json::from_value(invocation.args)
            .map_err(|error| format!("Invalid arguments: {error}"))?;
        if invocation.cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }
        tokio::select! {
            _ = invocation.cancel.cancelled() => Err("Operation aborted".into()),
            result = self.grep(params) => result,
        }
    }
}
