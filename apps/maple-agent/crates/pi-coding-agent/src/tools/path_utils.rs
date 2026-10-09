//! Paths the model writes, resolved against the session's folder.
//!
//! Models paste paths from many places, so `~` is the home folder, a leading `@` is
//! dropped, `file://` URLs are paths, unicode spaces are plain spaces, and on Windows a
//! Git Bash path such as `/c/Users` is a drive path. A file that is not found under
//! the exact name is also looked for under the names macOS gives screenshots.

use std::path::{Component, Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

const NARROW_NO_BREAK_SPACE: char = '\u{202F}';

fn is_unicode_space(c: char) -> bool {
    matches!(
        c,
        '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

/// `path` as the model meant it, before it is joined to a folder.
pub fn expand_path(path: &str) -> PathBuf {
    let mut normalized: String = path
        .chars()
        .map(|c| if is_unicode_space(c) { ' ' } else { c })
        .collect();
    if let Some(rest) = normalized.strip_prefix('@') {
        normalized = rest.to_string();
    }
    if cfg!(windows) {
        normalized = windows_shell_path(&normalized);
    }
    if let Some(home) = std::env::home_dir() {
        if normalized == "~" {
            return home;
        }
        if let Some(rest) = normalized
            .strip_prefix("~/")
            .or_else(|| normalized.strip_prefix("~\\").filter(|_| cfg!(windows)))
        {
            return home.join(rest);
        }
    }
    if let Some(path) = file_url_path(&normalized) {
        return path;
    }
    PathBuf::from(normalized)
}

/// `path` resolved against `cwd`, with `.` and `..` folded away.
pub fn resolve_to_cwd(path: &str, cwd: &Path) -> PathBuf {
    let expanded = expand_path(path);
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    normalize_lexically(&joined)
}

/// Like [`resolve_to_cwd`], then the names macOS gives screenshots when the exact
/// name does not exist: a narrow no-break space before AM/PM, decomposed accents, and a
/// curly apostrophe.
pub async fn resolve_read_path(path: &str, cwd: &Path) -> PathBuf {
    let resolved = resolve_to_cwd(path, cwd);
    if exists(&resolved).await {
        return resolved;
    }
    let text = resolved.to_string_lossy().into_owned();
    let nfd: String = text.nfd().collect();
    let variants = [
        am_pm_variant(&text),
        nfd.clone(),
        text.replace('\'', "\u{2019}"),
        nfd.replace('\'', "\u{2019}"),
    ];
    for variant in variants {
        if variant != text && exists(Path::new(&variant)).await {
            return PathBuf::from(variant);
        }
    }
    resolved
}

async fn exists(path: &Path) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

/// macOS writes "Screenshot 2024-01-01 at 10.00.00 AM.png" with a narrow no-break
/// space before AM or PM.
fn am_pm_variant(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(at) = rest.find(' ') {
        let after = &rest[at + 1..];
        let marker = after.get(..3).map(str::to_ascii_uppercase);
        out.push_str(&rest[..at]);
        if matches!(marker.as_deref(), Some("AM." | "PM.")) {
            out.push(NARROW_NO_BREAK_SPACE);
        } else {
            out.push(' ');
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Fold `.` and `..` without touching the disk, as Node's `path.resolve` does.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// `/c/Users`, `/mnt/c/Users` and `/cygdrive/c/Users` as `C:\Users`.
fn windows_shell_path(path: &str) -> String {
    if !path.starts_with('/') || path.starts_with("//") || path.contains('\\') {
        return path.to_string();
    }
    let rest = path
        .strip_prefix("/mnt/")
        .or_else(|| path.strip_prefix("/cygdrive/"))
        .unwrap_or(&path[1..]);
    let mut parts = rest.splitn(2, '/');
    let drive = parts.next().unwrap_or_default();
    let mut letters = drive.chars();
    match (letters.next(), letters.next()) {
        (Some(letter), None) if letter.is_ascii_alphabetic() => {
            let suffix = parts.next().unwrap_or_default().replace('/', "\\");
            format!("{}:\\{suffix}", letter.to_ascii_uppercase())
        }
        _ => path.to_string(),
    }
}

/// The path of a `file://` URL, percent-decoded.
fn file_url_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let decoded = percent_decode(rest)?;
    if cfg!(windows) {
        // file:///C:/Users becomes C:\Users.
        let trimmed = decoded.strip_prefix('/').unwrap_or(&decoded);
        return Some(PathBuf::from(trimmed.replace('/', "\\")));
    }
    Some(PathBuf::from(decoded))
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_join_the_folder_and_fold_dots() {
        let cwd = Path::new("/work/project");
        assert_eq!(
            resolve_to_cwd("src/../lib/./a.rs", cwd),
            PathBuf::from("/work/project/lib/a.rs")
        );
        assert_eq!(
            resolve_to_cwd("/etc/hosts", cwd),
            PathBuf::from("/etc/hosts")
        );
        assert_eq!(
            resolve_to_cwd("@notes.md", cwd),
            PathBuf::from("/work/project/notes.md")
        );
        assert_eq!(
            resolve_to_cwd("my\u{00A0}file.txt", cwd),
            PathBuf::from("/work/project/my file.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn home_and_file_urls_expand() {
        let home = std::env::home_dir().unwrap();
        assert_eq!(expand_path("~"), home);
        assert_eq!(expand_path("~/notes"), home.join("notes"));
        assert_eq!(
            expand_path("file:///tmp/a%20b.txt"),
            PathBuf::from("/tmp/a b.txt")
        );
    }

    #[test]
    fn git_bash_paths_read_as_drive_paths() {
        assert_eq!(windows_shell_path("/c/Users/me"), "C:\\Users\\me");
        assert_eq!(windows_shell_path("/mnt/d/data"), "D:\\data");
        assert_eq!(windows_shell_path("/cygdrive/e"), "E:\\");
        assert_eq!(windows_shell_path("/usr/bin"), "/usr/bin");
    }

    #[tokio::test]
    async fn screenshots_are_found_under_the_names_macos_gives_them() {
        let dir = tempfile::tempdir().unwrap();
        let real = "Screenshot 2024-01-01 at 10.00.00\u{202F}AM.png";
        std::fs::write(dir.path().join(real), b"png").unwrap();
        let typed = "Screenshot 2024-01-01 at 10.00.00 AM.png";
        assert_eq!(
            resolve_read_path(typed, dir.path()).await,
            dir.path().join(real)
        );

        // Decomposed, with a curly apostrophe. Some file systems match either form of
        // the accent, so only the apostrophe is checked.
        let curly = "Capture d\u{2019}e\u{0301}cran.png";
        std::fs::write(dir.path().join(curly), b"png").unwrap();
        let found = resolve_read_path("Capture d'\u{e9}cran.png", dir.path()).await;
        assert!(std::fs::metadata(&found).is_ok(), "{}", found.display());
        assert!(found.to_string_lossy().contains('\u{2019}'));
    }
}
