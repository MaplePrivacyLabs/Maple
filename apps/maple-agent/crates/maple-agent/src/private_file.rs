//! Crash-safe writes for small private files (settings, credentials,
//! per-account configuration).
//!
//! The content goes to a temporary file in the same directory, is synced,
//! and is then renamed over the target. A crash mid-write leaves the old
//! file intact instead of a truncated one. The temporary file is created
//! with owner-only permissions, so the content is never world-readable, not
//! even for a moment.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// Restrict `path` to its owner: mode 0600 for a file.
///
/// A no-op off unix, where the containing profile already carries the
/// access control.
pub(crate) fn set_owner_only_file(path: &Path) -> io::Result<()> {
    set_owner_only_mode(path, 0o600)
}

/// Restrict directory `path` to its owner: mode 0700.
///
/// A no-op off unix, where the containing profile already carries the
/// access control.
pub(crate) fn set_owner_only_dir(path: &Path) -> io::Result<()> {
    set_owner_only_mode(path, 0o700)
}

#[cfg(unix)]
fn set_owner_only_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_owner_only_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// Write `bytes` to `path` atomically with mode 0600 (owner read/write).
/// The parent directory is created when it is missing.
pub fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", path.display()),
        )
    })?;
    fs::create_dir_all(parent)?;

    let mut builder = tempfile::Builder::new();
    builder.prefix(".").suffix(".tmp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o600));
    }
    let mut temporary = builder.tempfile_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    set_owner_only_file(path)?;
    #[cfg(unix)]
    {
        // The rename is durable only after the directory entry is synced.
        // The file is already replaced, so a failure here is not an error
        // for the caller; log it and keep the committed value.
        if let Err(error) = fs::File::open(parent).and_then(|dir| dir.sync_all()) {
            log::warn!(
                "{} was written, but its directory could not be synced: {error}",
                path.display()
            );
        }
    }
    Ok(())
}

/// Serialize `value` as pretty JSON and write it with [`write_private_file`].
pub fn write_private_json<T: serde::Serialize + ?Sized>(path: &Path, value: &T) -> io::Result<()> {
    let json = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
    write_private_file(path, json.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_content_and_creates_parent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/file.json");
        write_private_json(&path, &serde_json::json!({"a": 1})).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, "{\n  \"a\": 1\n}");
        assert!(!dir.path().join("nested").read_dir().unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[test]
    fn replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        write_private_file(&path, b"old").unwrap();
        write_private_file(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        write_private_file(&path, b"x").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
