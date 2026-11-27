//! Open-file and folder dialogs through a credential-free instance of this
//! executable.
//!
//! GPUI's Linux dialog is an xdg-desktop-portal request, and the portal
//! identifies every caller by opening /proc/<pid>/root. The GUI is
//! non-dumpable from startup, so the kernel denies that and the portal refuses
//! the request. Like the theme watcher in `portal.rs`, a child of this
//! executable that never opens Maple state makes the request instead. Its only
//! output is the `file:` URIs the person chose; the GUI reads the files itself.

use std::{io, io::Write as _, sync::Arc};

use super::PickedPaths;

use tokio::io::AsyncReadExt as _;

/// The startup branch in `main.rs`, followed by the four request fields.
const ARGUMENT: &str = "--internal-file-chooser";
/// Thousands of paths; far more than an attachment draft or a folder needs.
const MAX_RESPONSE: usize = 1 << 20;
const MAX_MESSAGE: usize = 512;
// GPUI's wording for the same failure, so it reads the same on every path.
const PORTAL_MISSING: &str =
    "Couldn't open file picker due to missing xdg-desktop-portal implementation.";
const HELPER_FAILED: &str = "The desktop file chooser stopped unexpectedly.";
const INVALID_RESPONSE: &str = "The desktop file chooser returned an invalid response.";
const TOO_MANY: &str = "Too many files were selected.";

/// Whether the portal can identify this process itself. It needs
/// /proc/<pid>/root, which the kernel opens to other processes only while
/// the process is dumpable by its user. Hardening clears that at startup;
/// only tests and a debug build with `MAPLE_ALLOW_DEBUG_ATTACH=1` keep it.
pub(crate) fn portal_can_identify_this_process() -> bool {
    // 1 is SUID_DUMP_USER; 0 and the root-only 2 both hide /proc/<pid>/root.
    // SAFETY: PR_GET_DUMPABLE has no pointer arguments.
    unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) == 1 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Files,
    Folder,
    Folders,
}

impl Kind {
    fn new(options: &gpui::PathPromptOptions) -> Self {
        // GPUI's portal request offers folders whenever they are allowed.
        match (options.directories, options.multiple) {
            (false, false) => Self::File,
            (false, true) => Self::Files,
            (true, false) => Self::Folder,
            (true, true) => Self::Folders,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Files => "files",
            Self::Folder => "folder",
            Self::Folders => "folders",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        [Self::File, Self::Files, Self::Folder, Self::Folders]
            .into_iter()
            .find(|kind| kind.as_str() == value)
    }

    fn directories(self) -> bool {
        matches!(self, Self::Folder | Self::Folders)
    }

    fn multiple(self) -> bool {
        matches!(self, Self::Files | Self::Folders)
    }
}

/// One dialog, passed to the helper as arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Request {
    kind: Kind,
    /// The portal's parent window identifier, `x11:<xid>` or
    /// `wayland:<exported handle>`, or empty for no parent.
    parent: String,
    title: String,
    /// Empty for the dialog's own default.
    accept_label: String,
}

impl Request {
    fn arguments(&self) -> [&str; 5] {
        [
            ARGUMENT,
            self.kind.as_str(),
            &self.parent,
            &self.title,
            &self.accept_label,
        ]
    }

    fn from_arguments(arguments: &[String]) -> Option<Self> {
        let [kind, parent, title, accept_label] = arguments else {
            return None;
        };
        let request = Self {
            kind: Kind::parse(kind)?,
            parent: parent.clone(),
            title: title.clone(),
            accept_label: accept_label.clone(),
        };
        if !request.parent.is_empty() {
            request.identifier()?;
        }
        Some(request)
    }

    fn identifier(&self) -> Option<ashpd::WindowIdentifier> {
        parent_identifier(&self.parent)
    }

    async fn open(&self) -> Outcome {
        let request = ashpd::desktop::file_chooser::OpenFileRequest::default()
            .identifier(self.identifier())
            .modal(true)
            .title(self.title.as_str())
            .accept_label((!self.accept_label.is_empty()).then_some(self.accept_label.as_str()))
            .multiple(self.kind.multiple())
            .directory(self.kind.directories());
        match request.send().await.and_then(|request| request.response()) {
            Ok(selection) => Outcome::Selected(
                selection
                    .uris()
                    .iter()
                    .filter_map(|uri| url::Url::parse(uri.as_str()).ok())
                    .filter(|uri| uri.scheme() == "file")
                    .collect(),
            ),
            Err(ashpd::Error::Response(_)) => Outcome::Cancelled,
            Err(ashpd::Error::PortalNotFound(_)) => Outcome::Missing,
            Err(error) => Outcome::Failed(error.to_string()),
        }
    }
}

/// The portal's parent identifier, or `None` for one it cannot use.
fn parent_identifier(parent: &str) -> Option<ashpd::WindowIdentifier> {
    match parent.parse().ok()? {
        ashpd::WindowIdentifierType::Wayland(handle) if handle.is_empty() => None,
        parent => Some(ashpd::WindowIdentifier::from(parent)),
    }
}

/// The helper's whole answer: one status line, then its fields, one per line.
/// A serialized URL never contains a line break.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Selected(Vec<url::Url>),
    Cancelled,
    Missing,
    Failed(String),
}

impl Outcome {
    fn encode(&self) -> Vec<u8> {
        let mut output = String::new();
        match self {
            Self::Selected(uris) => {
                output.push_str("selected\n");
                for uri in uris {
                    output.push_str(uri.as_str());
                    output.push('\n');
                }
            }
            Self::Cancelled => output.push_str("cancelled\n"),
            Self::Missing => output.push_str("missing\n"),
            Self::Failed(message) => {
                output.push_str("failed\n");
                output.extend(
                    message
                        .chars()
                        .map(|c| if c.is_control() { ' ' } else { c })
                        .take(MAX_MESSAGE),
                );
                output.push('\n');
            }
        }
        output.into_bytes()
    }
}

/// Paths in the order chosen. Like GPUI, URIs that name no local path are
/// skipped rather than failing the whole selection.
fn decode(response: &[u8]) -> PickedPaths {
    let invalid = || INVALID_RESPONSE.to_owned();
    let text = std::str::from_utf8(response).map_err(|_| invalid())?;
    let mut lines = text.strip_suffix('\n').ok_or_else(invalid)?.split('\n');
    match (lines.next(), lines.clone().count()) {
        (Some("selected"), _) => Ok(Some(
            lines
                .filter_map(|line| url::Url::parse(line).ok())
                .filter(|uri| uri.scheme() == "file")
                .filter_map(|uri| uri.to_file_path().ok())
                .collect(),
        )),
        (Some("cancelled"), 0) => Ok(None),
        (Some("missing"), 0) => Err(PORTAL_MISSING.to_owned()),
        (Some("failed"), 1) => Err(lines.next().unwrap_or_default().to_owned()),
        _ => Err(invalid()),
    }
}

/// This is a terminal startup branch: no logging, store, backend, or GUI.
pub(crate) fn run_helper(arguments: &[String]) -> io::Result<()> {
    crate::host::hardening::disable_core_dumps().map_err(io::Error::other)?;
    let request = Request::from_arguments(arguments).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "Invalid file chooser request")
    })?;
    // The GUI closes its private stdin pipe when it stops waiting: the view
    // closed, Maple quit or crashed. Exiting drops this bus connection, and
    // the portal closes the dialog of a caller that has gone.
    std::thread::Builder::new()
        .name("chooser-parent".into())
        .spawn(|| {
            use std::io::Read as _;
            let _ = io::stdin().read(&mut [0_u8]);
            std::process::exit(0);
        })?;
    let outcome = futures_lite::future::block_on(request.open());
    let mut output = io::stdout().lock();
    output
        .write_all(&outcome.encode())
        .and_then(|()| output.flush())
}

/// Show the dialog for the active window. Dropping the task closes it.
pub(crate) fn prompt_for_paths(
    backend: &Arc<crate::backend::AgentBackend>,
    options: gpui::PathPromptOptions,
    cx: &mut gpui::App,
) -> gpui::Task<PickedPaths> {
    use gpui::AppContext as _;

    let window = cx.active_window();
    let kind = Kind::new(&options);
    let title = crate::ui::i18n::tr(if kind.directories() {
        "file-dialog-open-folder"
    } else {
        "file-dialog-open-files"
    });
    let accept_label = options
        .prompt
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    let backend = backend.clone();
    cx.spawn(async move |cx| {
        // Outside the click's own update, so the window is free to borrow.
        let parent = window
            .and_then(|window| {
                cx.update_window(window, |_, window, _| Parent::of(window))
                    .ok()
            })
            .unwrap_or_default();
        let request = Request {
            kind,
            parent: parent.argument(),
            title: title.to_string(),
            accept_label,
        };
        let mut helper = AbortOnDrop(backend.spawn(async move {
            // Package resolution reads filesystem metadata; keep it and the
            // subprocess off the UI thread.
            let command = super::portal::helper_command(request.arguments())
                .map_err(|error| failed("Could not prepare the desktop file chooser", error))?;
            run(command).await
        }));
        let picked = (&mut helper.0)
            .await
            .unwrap_or_else(|_| Err(HELPER_FAILED.to_owned()));
        parent.release();
        picked
    })
}

struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        // The helper's Child is killed on drop, which closes its dialog.
        self.0.abort();
    }
}

/// Log the cause; the person sees only the generic notice.
fn failed(context: &str, error: impl std::fmt::Display) -> String {
    log::warn!("{context}: {error}");
    HELPER_FAILED.to_owned()
}

async fn run(command: std::process::Command) -> PickedPaths {
    let mut child = tokio::process::Command::from(command)
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| failed("Could not start the desktop file chooser", error))?;
    // Held open until the answer is read: closing it cancels the dialog.
    let lifetime = child.stdin.take();
    let output = child.stdout.take().ok_or(HELPER_FAILED)?;
    let mut response = Vec::new();
    output
        .take(MAX_RESPONSE as u64 + 1)
        .read_to_end(&mut response)
        .await
        .map_err(|error| failed("Could not read the desktop file chooser's answer", error))?;
    if response.len() > MAX_RESPONSE {
        return Err(TOO_MANY.to_owned());
    }
    let status = child
        .wait()
        .await
        .map_err(|error| failed("Could not wait for the desktop file chooser", error))?;
    drop(lifetime);
    if !status.success() {
        return Err(failed("The desktop file chooser failed", status));
    }
    decode(&response)
}

/// The dialog's parent window, so the dialog stays attached and modal.
#[derive(Default)]
struct Parent(Option<ashpd::WindowIdentifier>);

impl Parent {
    fn of(window: &gpui::Window) -> Self {
        use raw_window_handle::{
            HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
        };

        // `Window` has its own `window_handle`, so name the trait's.
        let Ok(handle) = HasWindowHandle::window_handle(window) else {
            return Self::default();
        };
        Self::new(match handle.as_raw() {
            RawWindowHandle::Xcb(handle) => Some(ashpd::WindowIdentifier::from_xid(
                handle.window.get().into(),
            )),
            RawWindowHandle::Xlib(handle) => Some(ashpd::WindowIdentifier::from_xid(handle.window)),
            RawWindowHandle::Wayland(surface) => {
                match HasDisplayHandle::display_handle(window).map(|d| d.as_raw()) {
                    Ok(RawDisplayHandle::Wayland(display)) => {
                        // SAFETY: the window stays borrowed for this whole call,
                        // and GPUI destroys surfaces only on this thread, so the
                        // surface outlives the export. GPUI's display connection
                        // lives as long as the app; `release` and `Drop` below
                        // keep the export from outliving it. ashpd exports on its
                        // own thread and event queue. GPUI holds no Wayland read
                        // guard while app code runs (calloop-wayland-source
                        // releases it before dispatch), so this waits only for
                        // two compositor round trips.
                        futures_lite::future::block_on(unsafe {
                            ashpd::WindowIdentifier::from_wayland_raw(
                                surface.surface.as_ptr(),
                                display.display.as_ptr(),
                            )
                        })
                    }
                    _ => None,
                }
            }
            _ => None,
        })
    }

    /// Keep only a parent the helper accepts. ashpd reports an empty Wayland
    /// handle when the compositor sends none; that dialog opens unparented
    /// rather than failing. A dropped export is unexported here, on GPUI's
    /// thread.
    fn new(identifier: Option<ashpd::WindowIdentifier>) -> Self {
        Self(identifier.filter(|identifier| parent_identifier(&identifier.to_string()).is_some()))
    }

    fn argument(&self) -> String {
        self.0.as_ref().map(ToString::to_string).unwrap_or_default()
    }

    /// Unexport a Wayland parent. Call on GPUI's thread while the app runs,
    /// so its display connection still exists.
    fn release(mut self) {
        drop(self.0.take());
    }
}

impl Drop for Parent {
    fn drop(&mut self) {
        // Only a dropped task gets here, as at Quit, when GPUI's display
        // connection may already be gone. Leave the export to the compositor,
        // which discards it with the surface or the connection.
        if let Some(identifier) = self.0.take() {
            std::mem::forget(identifier);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn arguments(values: [&str; 4]) -> Vec<String> {
        values.map(str::to_owned).to_vec()
    }

    #[test]
    fn request_round_trips_through_helper_arguments() {
        for (directories, multiple, kind) in [
            (false, false, Kind::File),
            (false, true, Kind::Files),
            (true, false, Kind::Folder),
            (true, true, Kind::Folders),
        ] {
            let options = gpui::PathPromptOptions {
                files: !directories,
                directories,
                multiple,
                prompt: None,
            };
            assert_eq!(Kind::new(&options), kind);
            assert_eq!(kind.directories(), directories);
            assert_eq!(kind.multiple(), multiple);
            for parent in ["", "x11:3a00004", "wayland:2c5e8f1a-handle"] {
                let request = Request {
                    kind,
                    parent: parent.into(),
                    title: "Open Folder".into(),
                    accept_label: String::new(),
                };
                let arguments = request.arguments();
                assert_eq!(arguments[0], ARGUMENT);
                let parsed = arguments[1..]
                    .iter()
                    .map(|value| value.to_string())
                    .collect::<Vec<_>>();
                assert_eq!(Request::from_arguments(&parsed), Some(request));
            }
        }
    }

    #[test]
    fn helper_rejects_malformed_requests() {
        for values in [
            ["directory", "", "Open", ""],
            ["", "", "Open", ""],
            ["files", "x11:not-hex", "Open", ""],
            ["files", "wayland:", "Open", ""],
            ["files", "mir:1", "Open", ""],
            ["files", "3a00004", "Open", ""],
        ] {
            assert_eq!(
                Request::from_arguments(&arguments(values)),
                None,
                "{values:?}"
            );
        }
        assert_eq!(Request::from_arguments(&[]), None);
        assert_eq!(
            Request::from_arguments(&arguments(["files", "", "Open", ""])[..3]),
            None
        );
    }

    #[test]
    fn unusable_parents_open_the_dialog_unparented() {
        let empty =
            ashpd::WindowIdentifier::from(ashpd::WindowIdentifierType::Wayland(String::new()));
        assert_eq!(Parent::new(Some(empty)).argument(), "");
        assert_eq!(Parent::new(None).argument(), "");
        for parent in ["x11:3a00004", "wayland:2c5e8f1a-handle"] {
            let kept = Parent::new(parent_identifier(parent));
            assert_eq!(kept.argument(), parent);
            kept.release();
        }
    }

    #[test]
    fn parent_identifiers_reach_the_portal_unchanged() {
        for parent in ["x11:3a00004", "wayland:2c5e8f1a-handle"] {
            let request =
                Request::from_arguments(&arguments(["files", parent, "Open", ""])).unwrap();
            assert_eq!(request.identifier().unwrap().to_string(), parent);
        }
        let request = Request::from_arguments(&arguments(["files", "", "Open", ""])).unwrap();
        assert!(request.identifier().is_none());
    }

    #[test]
    fn responses_round_trip() {
        let uris = [
            "file:///home/a/report.pdf",
            "file:///home/a/two%0Alines.txt",
        ]
        .map(|uri| url::Url::parse(uri).unwrap());
        assert_eq!(
            decode(&Outcome::Selected(uris.to_vec()).encode()),
            Ok(Some(vec![
                PathBuf::from("/home/a/report.pdf"),
                PathBuf::from("/home/a/two\nlines.txt"),
            ]))
        );
        assert_eq!(
            decode(&Outcome::Selected(Vec::new()).encode()),
            Ok(Some(Vec::new()))
        );
        assert_eq!(decode(&Outcome::Cancelled.encode()), Ok(None));
        assert_eq!(
            decode(&Outcome::Missing.encode()),
            Err(PORTAL_MISSING.to_owned())
        );
        assert_eq!(
            decode(&Outcome::Failed("Portal request failed:\nAccess denied".into()).encode()),
            Err("Portal request failed: Access denied".to_owned())
        );
        let long = Outcome::Failed("x".repeat(MAX_MESSAGE * 2)).encode();
        assert_eq!(decode(&long).unwrap_err().len(), MAX_MESSAGE);
    }

    #[test]
    fn selections_keep_only_local_paths() {
        assert_eq!(
            decode(
                b"selected\nfile:///home/a/x\nhttps://example.com/y\nnot a uri\nfile://host/z\n"
            ),
            Ok(Some(vec![PathBuf::from("/home/a/x")]))
        );
    }

    #[test]
    fn malformed_responses_are_errors() {
        for response in [
            b"".as_slice(),
            b"selected",
            b"cancelled\nfile:///x\n",
            b"missing\nextra\n",
            b"failed\n",
            b"failed\none\ntwo\n",
            b"unknown\n",
            b"\xff\n",
        ] {
            assert_eq!(
                decode(response),
                Err(INVALID_RESPONSE.to_owned()),
                "{response:?}"
            );
        }
    }

    fn shell(script: &str) -> std::process::Command {
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        command
    }

    #[tokio::test]
    async fn run_reads_one_answer_from_the_helper() {
        assert_eq!(
            run(shell("printf 'selected\\nfile:///tmp/a%%20b\\n'")).await,
            Ok(Some(vec![PathBuf::from("/tmp/a b")]))
        );
        assert_eq!(run(shell("printf 'cancelled\\n'")).await, Ok(None));
    }

    #[tokio::test]
    async fn run_rejects_failed_or_oversized_helpers() {
        assert_eq!(
            run(shell("printf 'selected\\n'; exit 1")).await,
            Err(HELPER_FAILED.to_owned())
        );
        assert_eq!(
            run(shell("head -c 2000000 /dev/zero")).await,
            Err(TOO_MANY.to_owned())
        );
    }

    #[tokio::test]
    async fn dropping_the_request_kills_the_helper() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let script = format!("echo $$ > '{}'; exec sleep 60", pid_file.display());
        let pending = tokio::spawn(run(shell(&script)));
        let pid: u32 = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = pid.trim().parse()
                {
                    return pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        // Killed at once; tokio reaps it in the background.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                    Err(_) => return,
                    Ok(stat)
                        if stat
                            .rsplit(')')
                            .next()
                            .unwrap()
                            .trim_start()
                            .starts_with('Z') =>
                    {
                        return;
                    }
                    Ok(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .unwrap();
    }
}
