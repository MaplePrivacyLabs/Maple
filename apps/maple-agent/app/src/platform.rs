//! Small desktop integrations that differ per operating system.

use std::process::Command;

#[cfg(target_os = "linux")]
pub(crate) mod file_chooser;
#[cfg(target_os = "linux")]
pub(crate) mod portal;

/// Chosen paths, `Ok(None)` when the person cancels; the error is an
/// untranslated diagnostic.
pub(crate) type PickedPaths = Result<Option<Vec<std::path::PathBuf>>, String>;

/// Choose files or folders with the system dialog: NSOpenPanel, the Windows
/// common dialog, or the desktop portal on Linux. Use this, not
/// `cx.prompt_for_paths`: on Linux the hardened GUI cannot reach the portal
/// itself. Dropping the task closes a Linux dialog.
pub(crate) fn prompt_for_paths(
    backend: &std::sync::Arc<crate::backend::AgentBackend>,
    options: gpui::PathPromptOptions,
    cx: &mut gpui::App,
) -> gpui::Task<PickedPaths> {
    #[cfg(target_os = "linux")]
    if !file_chooser::portal_can_identify_this_process() {
        return file_chooser::prompt_for_paths(backend, options, cx);
    }
    let _ = backend;
    let receiver = cx.prompt_for_paths(options);
    cx.spawn(async move |_| match receiver.await {
        Ok(Ok(paths)) => Ok(paths),
        Ok(Err(error)) => Err(error.to_string()),
        // GPUI dropped the request without an answer.
        Err(_) => Ok(None),
    })
}

/// Open a folder in the platform file manager. Runs the launcher without
/// waiting for it; call from a blocking task, not the UI thread.
pub fn reveal_folder(path: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(path);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("explorer");
        command.arg(path);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(path);
        command
    };
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not open the folder: {error}"))
}
