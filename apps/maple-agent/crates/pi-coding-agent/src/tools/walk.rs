//! The folder walk `grep` and `find` share, with the rules Pi's ripgrep and fd follow:
//! hidden files are included, ignore files are respected, and `.git` is not entered.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use globset::GlobSet;
use ignore::WalkBuilder;

/// A walk of `root` in name order. `.gitignore` rules apply inside a repository, as
/// they do for git, and outside one too, as Pi asks fd to; inside, a nested
/// repository's files are not matched against its parent's rules. `.ignore` files and
/// `ignore_file` (`.rgignore` for `grep`, `.fdignore` for `find`) apply everywhere.
/// Entries under `root` that match `skip`, by their path from `root`, are left out,
/// and so are the folders' contents.
pub(crate) fn walker(root: &Path, ignore_file: &str, skip: Option<GlobSet>) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .require_git(inside_git_repository(root))
        .add_custom_ignore_filename(ignore_file)
        .sort_by_file_name(|a, b| a.cmp(b));
    let root = root.to_path_buf();
    builder.filter_entry(move |entry| {
        if entry.file_name() == ".git" {
            return false;
        }
        let (Some(skip), Some(relative)) = (&skip, relative_slash_path(entry.path(), &root)) else {
            return true;
        };
        let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
        !(skip.is_match(&relative) || (is_dir && skip.is_match(format!("{relative}/"))))
    });
    builder
}

/// Whether `path` or a folder above it holds a `.git`.
fn inside_git_repository(path: &Path) -> bool {
    path.ancestors().any(|dir| dir.join(".git").exists())
}

/// `path` under `root`, with `/` between its parts; `None` for a path outside it.
pub(crate) fn relative_slash_path(path: &Path, root: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let parts: Vec<_> = relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect();
    Some(parts.join("/"))
}

/// Runs `work` on a blocking thread. If the caller stops waiting, as a cancelled tool
/// call does, the flag `work` is given is set so it can stop early.
pub(crate) async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce(&AtomicBool) -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    struct StopOnDrop(Arc<AtomicBool>);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    let stop = StopOnDrop(Arc::new(AtomicBool::new(false)));
    let flag = stop.0.clone();
    let result = tokio::task::spawn_blocking(move || work(&flag)).await;
    drop(stop);
    result
}
