//! Install the GNOME Shell helper that computer use needs on Mutter.
//!
//! Mutter advertises none of the Wayland protocols that would let an ordinary
//! client read window geometry or capture the screen, so the Cua Driver SDK
//! routes both through a small Shell extension. The extension is embedded in
//! this binary rather than fetched, because a user who installs only the
//! executable has no source checkout to run an installer from.
//!
//! The driver and the extension negotiate an API version at run time, so the
//! embedded copy moves with the SDK pin. See `resources/gnome-helper/README.md`.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;

pub(super) const EXTENSION_UUID: &str = "winrects@cua";
/// The Cua Driver revision these files were copied from. Kept next to them so
/// a stale vendor is visible in a diff rather than only at run time.
pub(super) const UPSTREAM_SOURCE_REVISION: &str = "e7156658562eea3cb1721f3435ea317edb87acbd";

const METADATA_JSON: &str = include_str!("../../resources/gnome-helper/metadata.json");
const EXTENSION_JS: &str = include_str!("../../resources/gnome-helper/extension.js");

const SHELL_SCHEMA: &str = "org.gnome.shell";
const ENABLED_EXTENSIONS_KEY: &str = "enabled-extensions";

/// How far along the helper is on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GnomeHelperState {
    /// The extension owns its bus name, so the driver can call it.
    Loaded,
    /// The files are in place but GNOME has not loaded them. GNOME reads
    /// extensions only when the session starts.
    NeedsSessionRestart,
    Missing,
}

pub(super) fn state(loaded: bool) -> GnomeHelperState {
    if loaded {
        GnomeHelperState::Loaded
    } else if installed() {
        GnomeHelperState::NeedsSessionRestart
    } else {
        GnomeHelperState::Missing
    }
}

fn extensions_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
        })?;
    Some(base.join("gnome-shell/extensions").join(EXTENSION_UUID))
}

fn installed() -> bool {
    extensions_dir().is_some_and(|dir| dir.join("metadata.json").is_file())
}

/// Write the embedded extension and add it to the session's enabled set.
///
/// This is blocking file and subprocess work; call it off the async runtime.
/// It is idempotent, so re-running it after an upgrade refreshes the files.
pub(super) fn install() -> Result<GnomeHelperState, String> {
    let dir = extensions_dir()
        .ok_or_else(|| "Could not locate the GNOME extensions directory".to_string())?;
    write_extension(&dir)?;
    enable_extension()?;
    log::info!(
        "Installed the {EXTENSION_UUID} GNOME extension from Cua Driver {UPSTREAM_SOURCE_REVISION}"
    );
    Ok(GnomeHelperState::NeedsSessionRestart)
}

/// Place the embedded files, replacing any earlier copy.
fn write_extension(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("Could not create {}: {error}", dir.display()))?;
    write_file(&dir.join("metadata.json"), METADATA_JSON)?;
    write_file(&dir.join("extension.js"), EXTENSION_JS)
}

fn write_file(path: &Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents)
        .map_err(|error| format!("Could not write {}: {error}", path.display()))
}

/// Add the extension to `org.gnome.shell enabled-extensions`, preserving the
/// entries already there.
fn enable_extension() -> Result<(), String> {
    let current = read_enabled_extensions()?;
    if current.iter().any(|entry| entry == EXTENSION_UUID) {
        return Ok(());
    }
    let mut next = current;
    next.push(EXTENSION_UUID.to_string());

    let status = Command::new("gsettings")
        .args([
            "set",
            SHELL_SCHEMA,
            ENABLED_EXTENSIONS_KEY,
            &format_string_array(&next),
        ])
        .status()
        .map_err(|error| format!("Could not run gsettings: {error}"))?;
    if !status.success() {
        return Err(format!(
            "gsettings could not record the extension (exit status {:?})",
            status.code()
        ));
    }
    Ok(())
}

fn read_enabled_extensions() -> Result<Vec<String>, String> {
    let output = Command::new("gsettings")
        .args(["get", SHELL_SCHEMA, ENABLED_EXTENSIONS_KEY])
        .output()
        .map_err(|error| format!("Could not run gsettings: {error}"))?;
    if !output.status.success() {
        return Err("gsettings could not read the enabled extensions".to_string());
    }
    let value = String::from_utf8_lossy(&output.stdout);
    parse_string_array(value.trim()).ok_or_else(|| {
        // Refuse to guess. Overwriting this key with a bad parse would
        // silently disable every extension the user has enabled.
        "The enabled-extensions setting is not in a form Maple can safely edit".to_string()
    })
}

/// Parse GVariant's array-of-string form, as `gsettings get` prints it.
///
/// Returns `None` for anything that is not a plain list of single-quoted
/// entries, so an unexpected shape is never rewritten.
fn parse_string_array(value: &str) -> Option<Vec<String>> {
    if value == "@as []" || value == "[]" {
        return Some(Vec::new());
    }
    let inner = value.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    inner
        .split(',')
        .map(|entry| {
            let entry = entry.trim();
            let entry = entry.strip_prefix('\'')?.strip_suffix('\'')?;
            // An escape would need real GVariant unquoting; extension IDs
            // never contain one, so treat it as a shape Maple will not touch.
            (!entry.contains('\\') && !entry.contains('\'')).then(|| entry.to_string())
        })
        .collect()
}

fn format_string_array(values: &[String]) -> String {
    let entries = values
        .iter()
        .map(|value| format!("'{value}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{entries}]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_extension_is_the_one_the_driver_expects() {
        let metadata: serde_json::Value =
            serde_json::from_str(METADATA_JSON).expect("embedded metadata is valid JSON");
        assert_eq!(metadata["uuid"], EXTENSION_UUID);
        assert!(EXTENSION_JS.contains("org.cua.WinRects"));
        // Recorded so a stale vendor shows up in a diff. The driver and the
        // extension negotiate an API version, so the two move together.
        assert_eq!(UPSTREAM_SOURCE_REVISION.len(), 40);
        assert!(
            UPSTREAM_SOURCE_REVISION
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
    }

    #[test]
    fn writing_the_extension_is_idempotent() {
        let temporary = tempfile::tempdir().unwrap();
        let dir = temporary.path().join("winrects@cua");

        write_extension(&dir).expect("first install");
        let metadata = std::fs::read_to_string(dir.join("metadata.json")).unwrap();
        assert_eq!(metadata, METADATA_JSON);
        assert_eq!(
            std::fs::read_to_string(dir.join("extension.js")).unwrap(),
            EXTENSION_JS
        );

        // Re-running after an upgrade refreshes a stale copy rather than
        // failing because the directory already exists.
        std::fs::write(dir.join("extension.js"), "stale").unwrap();
        write_extension(&dir).expect("second install");
        assert_eq!(
            std::fs::read_to_string(dir.join("extension.js")).unwrap(),
            EXTENSION_JS
        );
    }

    #[test]
    fn enabled_extensions_round_trip_and_refuse_unknown_shapes() {
        assert_eq!(parse_string_array("@as []"), Some(Vec::new()));
        assert_eq!(parse_string_array("[]"), Some(Vec::new()));
        assert_eq!(
            parse_string_array("['ding@rastersoft.com', 'winrects@cua']"),
            Some(vec![
                "ding@rastersoft.com".to_string(),
                "winrects@cua".to_string()
            ])
        );
        assert_eq!(
            format_string_array(&["a@b".to_string(), "c@d".to_string()]),
            "['a@b', 'c@d']"
        );

        // Anything Maple cannot read back exactly is left alone, because
        // rewriting this key wrongly disables every extension the user has.
        assert_eq!(parse_string_array("['need\\'s escaping']"), None);
        assert_eq!(parse_string_array("not-an-array"), None);
    }
}
