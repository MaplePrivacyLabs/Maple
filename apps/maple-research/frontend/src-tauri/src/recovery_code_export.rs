use std::{io::Write, path::Path};

use tauri_plugin_dialog::DialogExt;

static SAVE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const SAVE_ERROR: &str = "Couldn't save the recovery code file";

// The SDK owns the recovery-code grammar. This boundary only permits a small,
// single-line ASCII value in a fixed text document, never arbitrary file content.
fn export_text(code: &str) -> Result<String, String> {
    if code.is_empty()
        || code.len() > 128
        || !code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("Invalid recovery code export".into());
    }
    Ok(format!(
        "Maple recovery code\n\n{code}\n\nKeep this code private. Replacing or disabling it makes this copy invalid.\n"
    ))
}

fn write_export(path: &Path, content: &str) -> Result<(), String> {
    if !path.is_absolute()
        || !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("txt"))
    {
        return Err("Choose a text (.txt) file".into());
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => return Err(SAVE_ERROR.into()),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(SAVE_ERROR.into());
        }
        _ => {}
    }
    let parent = path.parent().ok_or(SAVE_ERROR)?;
    // A private temporary file (0600 on Unix), then an atomic replacement, keeps
    // partial writes and an existing destination's permissive mode from exposing the code.
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|_| SAVE_ERROR)?;
    file.write_all(content.as_bytes()).map_err(|_| SAVE_ERROR)?;
    file.as_file().sync_all().map_err(|_| SAVE_ERROR)?;
    file.persist(path).map_err(|_| SAVE_ERROR)?;
    Ok(())
}

/// The renderer supplies only the code. The user chooses the destination in a
/// native dialog; no renderer-selected path or filesystem permission is exposed.
#[tauri::command]
pub async fn save_recovery_code(
    window: tauri::WebviewWindow,
    recovery_code: String,
) -> Result<bool, String> {
    if window.label() != "main" {
        return Err(SAVE_ERROR.into());
    }
    let content = export_text(&recovery_code)?;
    let _guard = SAVE_LOCK
        .try_lock()
        .map_err(|_| "A save dialog is already open")?;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    window
        .dialog()
        .file()
        .set_parent(&window)
        .set_title("Save your recovery code")
        .set_file_name("maple-recovery-code.txt")
        .add_filter("Text file", &["txt"])
        .save_file(move |path| {
            let _ = sender.send(path);
        });
    let Some(path) = receiver.await.map_err(|_| SAVE_ERROR)? else {
        return Ok(false);
    };
    let path = path.into_path().map_err(|_| SAVE_ERROR)?;
    tauri::async_runtime::spawn_blocking(move || write_export(&path, &content))
        .await
        .map_err(|_| SAVE_ERROR)??;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unbounded_or_multiline_renderer_content() {
        for code in ["", "code\nprose", "code\0", "é", &"A".repeat(129)] {
            assert!(export_text(code).is_err());
        }
    }

    #[test]
    fn saves_complete_text_and_safely_replaces_an_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recovery.txt");
        std::fs::write(&path, "old content").unwrap();
        let content = export_text("RECOVERY-CODE-FIXTURE").unwrap();
        write_export(&path, &content).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        assert!(content.starts_with("Maple recovery code\n\nRECOVERY-CODE-FIXTURE\n\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn rejects_non_text_paths_and_write_failures_without_leaking_paths_or_content() {
        let directory = tempfile::tempdir().unwrap();
        assert!(write_export(&directory.path().join("file.sh"), "secret").is_err());
        assert!(write_export(Path::new("relative.txt"), "secret").is_err());
        assert_eq!(
            write_export(&directory.path().join("missing/file.txt"), "secret"),
            Err(SAVE_ERROR.into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinks_without_modifying_the_target() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.txt");
        let link = directory.path().join("link.txt");
        std::fs::write(&target, "unchanged").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(write_export(&link, "secret").is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "unchanged");
    }
}
