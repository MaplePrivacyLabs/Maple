//! Directory completion for a typed project root.
//!
//! The host owns the filesystem, so it answers "what directories match
//! what I typed so far". Clients show the answer as-is and never parse or
//! filter paths themselves.

use std::path::{Path, PathBuf};

use crate::host::DirectorySuggestion;

/// Most suggestions one answer carries.
pub const SUGGESTION_LIMIT: usize = 50;

/// Directories that complete `query`, absolute, sorted by name.
///
/// An empty query lists the home directory. A query ending in a
/// separator lists that directory. Otherwise the last component is a
/// prefix filter on its parent. Hidden directories appear only when the
/// prefix starts with a dot. A leading `~` means the home directory.
/// Blocking: call from a blocking thread.
pub fn suggest(query: &str, home: Option<&Path>) -> Vec<DirectorySuggestion> {
    let query = query.trim();
    let expanded = match query.strip_prefix('~') {
        Some(rest) => match home {
            Some(home) => format!("{}{}", home.display(), rest),
            None => return Vec::new(),
        },
        None if query.is_empty() => match home {
            Some(home) => format!("{}{}", home.display(), std::path::MAIN_SEPARATOR),
            None => return Vec::new(),
        },
        None => query.to_string(),
    };
    let (parent, prefix) = split_query(&expanded);
    if !parent.is_absolute() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&parent) else {
        return Vec::new();
    };
    let show_hidden = prefix.starts_with('.');
    let mut matches: Vec<DirectorySuggestion> = entries
        .flatten()
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !show_hidden && name.starts_with('.') {
                return None;
            }
            if !name.to_lowercase().starts_with(&prefix.to_lowercase()) {
                return None;
            }
            Some(DirectorySuggestion {
                path: entry.path().to_string_lossy().into_owned(),
                name,
            })
        })
        .collect();
    matches.sort_by_key(|suggestion| suggestion.name.to_lowercase());
    matches.truncate(SUGGESTION_LIMIT);
    matches
}

/// The directory to list and the name prefix to match in it.
fn split_query(query: &str) -> (PathBuf, String) {
    if query.ends_with(std::path::MAIN_SEPARATOR) || query.ends_with('/') {
        return (PathBuf::from(query), String::new());
    }
    let path = Path::new(query);
    let prefix = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(query));
    (parent, prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "maple-dirs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for name in ["projects", "Photos", ".hidden", "plain"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::write(root.join("pfile"), "x").unwrap();
        root
    }

    #[test]
    fn prefix_filters_case_insensitively_and_skips_files_and_hidden() {
        let root = fixture();
        let query = format!("{}/p", root.display());
        let names: Vec<String> = suggest(&query, None).into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["Photos", "plain", "projects"]);
        let hidden: Vec<String> = suggest(&format!("{}/.h", root.display()), None)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(hidden, vec![".hidden"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn trailing_separator_lists_the_directory_and_tilde_means_home() {
        let root = fixture();
        let all = suggest(&format!("{}/", root.display()), None);
        assert_eq!(all.len(), 3, "hidden stays out without a dot prefix");
        assert!(all.iter().all(|s| Path::new(&s.path).is_absolute()));
        let via_home = suggest("~/pr", Some(&root));
        assert_eq!(via_home.len(), 1);
        assert_eq!(via_home[0].name, "projects");
        let empty = suggest("", Some(&root));
        assert_eq!(empty.len(), 3);
        assert!(suggest("relative/path", None).is_empty());
        assert!(suggest("", None).is_empty());
        let _ = std::fs::remove_dir_all(root);
    }
}
