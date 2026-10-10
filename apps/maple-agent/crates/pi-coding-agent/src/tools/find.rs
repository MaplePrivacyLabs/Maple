//! `find`: files whose names match a glob, natively, where Pi runs fd.
//!
//! The search follows fd's rules as Pi runs it: hidden files are found, ignore files
//! are respected, a pattern without `/` matches names and one with `/` matches paths
//! from the search folder, at any depth, and a pattern without capitals ignores case.
//! Folders end with `/`. Results come in path order.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use globset::{GlobBuilder, GlobMatcher, GlobSetBuilder};
use pi_agent_core::{AgentTool, AgentToolResult, ToolError, ToolInvocation};
use pi_ai::{Content, Tool};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::ToolContext;
use super::path_utils::resolve_to_cwd;
use super::truncate::{DEFAULT_MAX_BYTES, format_size, truncate_head};
use super::walk::{relative_slash_path, run_blocking, walker};

pub const FIND_SNIPPET: &str = "Find files by glob pattern (respects .gitignore)";
const DEFAULT_LIMIT: usize = 1000;
/// Left out of every search, as Pi asks of custom operations.
const IGNORED: [&str; 2] = ["**/node_modules/**", "**/.git/**"];

/// What a search leaves out and how much it returns.
#[derive(Clone, Debug)]
pub struct FindGlobOptions {
    /// Globs of paths, from the search folder, to leave out.
    pub ignore: Vec<String>,
    /// The most paths to return.
    pub limit: usize,
}

/// Where `find` searches. Replace it to search somewhere else, for example over SSH.
#[async_trait]
pub trait FindOperations: Send + Sync {
    async fn exists(&self, path: &Path) -> bool;
    /// Paths under `search_path` that match `pattern`, from it or absolute, at most
    /// `options.limit`. Folders end with `/`.
    async fn glob(
        &self,
        pattern: &str,
        search_path: &Path,
        options: &FindGlobOptions,
    ) -> io::Result<Vec<String>>;
}

/// Files on this machine.
pub struct LocalFindOperations;

#[async_trait]
impl FindOperations for LocalFindOperations {
    async fn exists(&self, path: &Path) -> bool {
        tokio::fs::try_exists(path).await.unwrap_or(false)
    }

    async fn glob(
        &self,
        pattern: &str,
        search_path: &Path,
        options: &FindGlobOptions,
    ) -> io::Result<Vec<String>> {
        let pattern = pattern.to_string();
        let root = search_path.to_path_buf();
        let options = options.clone();
        run_blocking(move |stop| find_files(&pattern, &root, &options, stop)).await?
    }
}

fn invalid_glob(error: globset::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
}

fn find_files(
    pattern: &str,
    root: &Path,
    options: &FindGlobOptions,
    stop: &AtomicBool,
) -> io::Result<Vec<String>> {
    if !root.is_dir() {
        return Err(io::Error::other(format!(
            "Not a directory: {}",
            root.display()
        )));
    }
    let matcher = PatternMatcher::new(pattern)?;
    let mut skip = GlobSetBuilder::new();
    for glob in &options.ignore {
        skip.add(
            GlobBuilder::new(glob)
                .literal_separator(true)
                .build()
                .map_err(invalid_glob)?,
        );
    }
    let skip = skip.build().map_err(invalid_glob)?;
    let mut found = Vec::new();
    for entry in walker(root, ".fdignore", Some(skip)).build() {
        if found.len() >= options.limit || stop.load(Ordering::Relaxed) {
            break;
        }
        // Entries that cannot be read are passed over.
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 {
            continue;
        }
        let Some(relative) = relative_slash_path(entry.path(), root) else {
            continue;
        };
        if matcher.is_match(entry.path(), &relative) {
            if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                found.push(format!("{relative}/"));
            } else {
                found.push(relative);
            }
        }
    }
    Ok(found)
}

/// What a pattern is matched against.
enum Target {
    Name,
    PathFromRoot,
    AbsolutePath,
}

/// fd's matching of a glob.
struct PatternMatcher {
    glob: GlobMatcher,
    target: Target,
}

impl PatternMatcher {
    fn new(pattern: &str) -> io::Result<Self> {
        // `./` is the search folder.
        let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
        let (glob, target) = if !pattern.contains('/') {
            (pattern.to_string(), Target::Name)
        } else if pattern.starts_with('/') || Path::new(pattern).is_absolute() {
            (pattern.to_string(), Target::AbsolutePath)
        } else if pattern.starts_with("**/") {
            (pattern.to_string(), Target::PathFromRoot)
        } else {
            // As Pi has fd do, a path pattern matches at any depth.
            (format!("**/{pattern}"), Target::PathFromRoot)
        };
        let glob = GlobBuilder::new(&glob)
            .literal_separator(true)
            // fd's smart case: a pattern without capitals ignores case.
            .case_insensitive(!pattern.chars().any(char::is_uppercase))
            .build()
            .map_err(invalid_glob)?;
        Ok(Self {
            glob: glob.compile_matcher(),
            target,
        })
    }

    fn is_match(&self, path: &Path, from_root: &str) -> bool {
        match self.target {
            Target::Name => path
                .file_name()
                .is_some_and(|name| self.glob.is_match(name)),
            Target::PathFromRoot => self.glob.is_match(from_root),
            Target::AbsolutePath => self.glob.is_match(path),
        }
    }
}

/// A result as a path from the search folder, with `/` between its parts. A folder
/// keeps its trailing `/`.
fn relativize(result: &str, search_path: &Path) -> String {
    let folder = result.ends_with('/') || (cfg!(windows) && result.ends_with('\\'));
    let path = Path::new(result);
    let relative = if path.is_absolute() {
        relative_slash_path(path, search_path).unwrap_or_else(|| result.to_string())
    } else if cfg!(windows) {
        result.replace('\\', "/")
    } else {
        result.to_string()
    };
    if folder && !relative.ends_with('/') {
        format!("{relative}/")
    } else {
        relative
    }
}

#[derive(Clone, Default)]
pub struct FindToolOptions {
    /// Where to search; this machine when `None`.
    pub operations: Option<Arc<dyn FindOperations>>,
}

pub struct FindTool {
    declaration: Tool,
    cwd: PathBuf,
    operations: Arc<dyn FindOperations>,
    context: ToolContext,
}

#[derive(Deserialize)]
struct FindParams {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<f64>,
}

impl FindTool {
    pub fn new(cwd: impl Into<PathBuf>, options: FindToolOptions, context: ToolContext) -> Self {
        Self {
            declaration: Tool::new(
                "find",
                format!(
                    "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} results or {}KB (whichever is hit first).",
                    DEFAULT_MAX_BYTES / 1024
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"
                        },
                        "path": {
                            "type": "string",
                            "description": "Directory to search in (default: current directory)"
                        },
                        "limit": {
                            "type": "number",
                            "description": "Maximum number of results (default: 1000)"
                        }
                    },
                    "required": ["pattern"]
                }),
            ),
            cwd: cwd.into(),
            operations: options
                .operations
                .unwrap_or_else(|| Arc::new(LocalFindOperations)),
            context,
        }
    }

    async fn find(&self, params: FindParams) -> Result<AgentToolResult, ToolError> {
        let cwd = self.context.cwd().unwrap_or_else(|| self.cwd.clone());
        let path = params.path.as_deref().filter(|path| !path.is_empty());
        let root = resolve_to_cwd(path.unwrap_or("."), &cwd);
        let limit = params
            .limit
            .map_or(DEFAULT_LIMIT, |limit| limit.max(1.0) as usize);
        if !self.operations.exists(&root).await {
            return Err(format!("Path not found: {}", root.display()).into());
        }
        let options = FindGlobOptions {
            ignore: IGNORED.iter().map(|glob| glob.to_string()).collect(),
            limit,
        };
        let results = self
            .operations
            .glob(&params.pattern, &root, &options)
            .await?;
        if results.is_empty() {
            return Ok(AgentToolResult::text("No files found matching pattern"));
        }

        let relativized: Vec<String> = results
            .iter()
            .map(|result| relativize(result, &root))
            .collect();
        // The result limit already caps the lines, so only bytes are limited here.
        let truncation = truncate_head(&relativized.join("\n"), usize::MAX, DEFAULT_MAX_BYTES);
        let mut output = truncation.content.clone();
        let mut details = Map::new();
        let mut notices = Vec::new();
        if relativized.len() >= limit {
            notices.push(format!(
                "{limit} results limit reached. Use limit={} for more, or refine pattern",
                limit.saturating_mul(2)
            ));
            details.insert("resultLimitReached".to_string(), json!(limit));
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
impl AgentTool for FindTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn label(&self) -> &str {
        "find"
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        let params: FindParams = serde_json::from_value(invocation.args)
            .map_err(|error| format!("Invalid arguments: {error}"))?;
        if invocation.cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }
        tokio::select! {
            _ = invocation.cancel.cancelled() => Err("Operation aborted".into()),
            result = self.find(params) => result,
        }
    }
}
