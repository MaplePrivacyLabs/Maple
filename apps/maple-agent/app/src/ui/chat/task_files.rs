//! Links in a task's replies that name its files.
//!
//! On the computer that runs the task, a link opens the file where it is,
//! or shows it in the file manager when opening it could run something, and
//! always for a folder. A viewer on another computer downloads the file into
//! its Downloads folder instead: it checks the whole file against the digest
//! the execution computer sent, marks it as downloaded for the system's own
//! checks, and then opens or shows it by the same rule. Only web links reach
//! the browser.

use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use gpui::{App, Context};
use maple_agent::store::{MAX_TASK_FILE_BYTES, MAX_TASK_FILE_RANGE_BYTES, TaskFileRange};
use sha2::{Digest, Sha256};

use super::ChatScreen;
use crate::backend::{ReadTaskFileRequest, TaskFileRequest};
use crate::ui::i18n::UiText;
use crate::ui::rich_text;
use crate::ui::task::Failure;

/// Kinds of file that open in a viewer or editor rather than running
/// anything: no scripts (web pages) and no embedded macros (OpenDocument).
/// Any other file is shown in the file manager.
const OPENS_SAFELY: &[&str] = &[
    "pdf", "png", "jpg", "jpeg", "gif", "webp", "heic", "heif", "bmp", "tif", "tiff", "txt", "md",
    "markdown", "csv", "tsv", "json", "log", "mp3", "m4a", "wav", "aac", "flac", "mp4", "m4v",
    "mov", "webm", "docx", "xlsx", "pptx", "pages", "numbers",
];
const MAX_SAVED_NAME_BYTES: usize = 180;

const DAMAGED: Failure = Failure::Local(
    "chat-file-damaged",
    "The file did not arrive intact. Try again.",
);
const NOT_SAVED: Failure = Failure::Local(
    "chat-file-not-saved",
    "The file could not be saved to Downloads.",
);
const CHANGED: Failure = Failure::Local(
    "runtime-error-task-file-changed",
    "The file changed while it downloaded. Try again.",
);

impl ChatScreen {
    /// Open a link of the task on screen: a web link in the browser, any
    /// other as a file of the task.
    pub(super) fn open_task_link(
        &mut self,
        task: &str,
        generation: u64,
        link: String,
        cx: &mut Context<Self>,
    ) {
        if rich_text::is_web_link(&link) {
            rich_text::open_link(&link);
            return;
        }
        // A place in the reply itself, or nothing at all.
        if link.is_empty() || link.starts_with('#') {
            return;
        }
        if self.pages.generation != generation || !self.is_selected(task) {
            return;
        }
        if self.file_link_pending {
            self.notice = Some(UiText::key("chat-file-busy"));
            cx.notify();
            return;
        }
        self.file_link_pending = true;
        let backend = self.backend.clone();
        let user = self.user_id.clone();
        let task = task.to_owned();
        if backend.remote_host().is_none() {
            self.call_semantic(
                async move {
                    backend
                        .locate_task_file(&user, TaskFileRequest { task, link })
                        .await
                },
                cx,
                move |this, result, cx| {
                    this.file_link_pending = false;
                    match result {
                        Ok(location) => open_located(&location.path, location.folder, cx),
                        Err(error) => this.notice = Some(error.into()),
                    }
                    cx.notify();
                },
            );
            return;
        }
        // A download from another computer can take a while; say it began.
        self.notice = Some(UiText::message(
            "chat-file-downloading",
            [("name", link_name(&link).into())],
        ));
        cx.notify();
        self.call_semantic(
            async move {
                let directory = downloads_directory().ok_or(NOT_SAVED)?;
                let read = |offset: u64, version: Option<String>| {
                    let backend = backend.clone();
                    let user = user.clone();
                    let request = ReadTaskFileRequest {
                        task: task.clone(),
                        link: link.clone(),
                        offset,
                        len: MAX_TASK_FILE_RANGE_BYTES,
                        version,
                    };
                    async move {
                        backend
                            .read_task_file(&user, request)
                            .await
                            .map_err(Failure::from)
                    }
                };
                download(&directory, MAX_TASK_FILE_RANGE_BYTES, read).await
            },
            cx,
            move |this, result, cx| {
                this.file_link_pending = false;
                match result {
                    Ok(saved) => {
                        let name = saved
                            .path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        this.notice =
                            Some(UiText::message("chat-file-saved", [("name", name.into())]));
                        open_downloaded(&saved, cx);
                    }
                    Err(error) => this.notice = Some(error.into()),
                }
                cx.notify();
            },
        );
    }
}

/// Open a file of the task where it is, if that cannot run anything; show
/// anything else in the file manager. A folder is always shown: one may be a
/// bundle, such as an app, whatever its name.
fn open_located(path: &Path, folder: bool, cx: &mut App) {
    // Still the very file the execution computer found: no link along the
    // path was swapped in since.
    let unchanged = std::fs::canonicalize(path).is_ok_and(|now| now == path);
    if !folder && unchanged && opens_in_place(path) {
        cx.open_with_system(&shell_path(path));
    } else {
        cx.reveal_path(&shell_path(path));
    }
}

/// Open a download by the same rule, and only once it carries the system's
/// mark for downloads.
fn open_downloaded(saved: &Saved, cx: &mut App) {
    if saved.marked && opens_in_place(&saved.path) {
        cx.open_with_system(&saved.path);
    } else {
        cx.reveal_path(&saved.path);
    }
}

fn opens_in_place(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file()) && opens_safely(path)
}

/// `path` as other programs expect it: Windows apps may not accept the
/// `\\?\` form a canonical path has there.
fn shell_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        if let Some(Component::Prefix(prefix)) = path.components().next() {
            let rest: PathBuf = path.components().skip(1).collect();
            match prefix.kind() {
                Prefix::VerbatimDisk(drive) => {
                    return PathBuf::from(format!("{}:\\", char::from(drive))).join(rest);
                }
                Prefix::VerbatimUNC(server, share) => {
                    let mut root = std::ffi::OsString::from(r"\\");
                    root.push(server);
                    root.push(r"\");
                    root.push(share);
                    root.push(r"\");
                    return PathBuf::from(root).join(rest);
                }
                _ => {}
            }
        }
    }
    path.to_path_buf()
}

fn opens_safely(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| OPENS_SAFELY.contains(&extension.to_ascii_lowercase().as_str()))
}

fn downloads_directory() -> Option<PathBuf> {
    dirs::download_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Downloads")))
        .filter(|directory| directory.is_dir())
}

/// A checked download in the Downloads folder.
#[derive(Debug)]
struct Saved {
    path: PathBuf,
    /// It carries the system's mark for downloads.
    marked: bool,
}

/// Download a file into `directory` in pieces of `piece` bytes and return
/// where it was saved. The first read names no version and carries the
/// whole file's digest; every later read is of that same version.
async fn download<F, Fut>(directory: &Path, piece: usize, mut read: F) -> Result<Saved, Failure>
where
    F: FnMut(u64, Option<String>) -> Fut,
    Fut: Future<Output = Result<TaskFileRange, Failure>>,
{
    let first = read(0, None).await?;
    let size = first.size;
    let Some(expected) = first.sha256.clone().filter(|digest| is_digest(digest)) else {
        return Err(DAMAGED);
    };
    if size > MAX_TASK_FILE_BYTES {
        return Err(DAMAGED);
    }
    let name = saved_name(&first.name);
    let version = first.version.clone();
    let mut part = Part::create(directory)?;
    let mut hash = Sha256::new();
    let mut offset = 0;
    let mut range = first;
    loop {
        if range.size != size || range.version != version {
            return Err(CHANGED);
        }
        let data = STANDARD.decode(&range.data).map_err(|_| DAMAGED)?;
        let end = offset + data.len() as u64;
        // Every piece but the last is whole, so a host cannot stretch a
        // download into endless tiny reads.
        if data.len() > piece || end > size || (end < size && data.len() != piece) {
            return Err(DAMAGED);
        }
        part.write(&data)?;
        hash.update(&data);
        offset = end;
        if offset == size {
            break;
        }
        range = read(offset, Some(version.clone())).await?;
    }
    let digest: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if digest != expected {
        return Err(DAMAGED);
    }
    part.place(directory, &name)
}

/// The file name a link shows, before the execution computer names it.
fn link_name(link: &str) -> String {
    // A `file:` URL's query and fragment name no part of the file; in a
    // plain path they may be part of a name.
    let path = if link
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("file:"))
    {
        link.split(['?', '#']).next().unwrap_or_default()
    } else {
        link
    };
    let leaf = path.rsplit(['/', '\\']).next().unwrap_or_default();
    saved_name(&percent_encoding::percent_decode_str(leaf).decode_utf8_lossy())
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A file name from another computer, safe to save here: no folders, no
/// characters a system refuses or that hide what follows, not hidden.
fn saved_name(name: &str) -> String {
    let leaf = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut saved = String::new();
    for character in leaf.chars().filter(|character| {
        !character.is_control() && !is_invisible(*character) && !"<>:\"|?*".contains(*character)
    }) {
        if saved.len() + character.len_utf8() > MAX_SAVED_NAME_BYTES {
            break;
        }
        saved.push(character);
    }
    let saved = saved
        .trim_start_matches(|character: char| character == '.' || character.is_whitespace())
        .trim_end_matches(|character: char| character == '.' || character.is_whitespace());
    let reserved = saved.split('.').next().is_some_and(|stem| {
        let stem = stem.trim().to_ascii_uppercase();
        matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit())
    });
    if saved.is_empty() || reserved {
        format!("download {saved}").trim().to_owned()
    } else {
        saved.to_owned()
    }
}

/// Direction and zero-width marks: `invoice\u{202E}fdp.exe` would show as
/// `invoiceexe.pdf`.
fn is_invisible(character: char) -> bool {
    matches!(
        character,
        '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
    )
}

/// The name for the `attempt`th file of that name: `report (2).pdf`.
fn numbered(name: &str, attempt: u32) -> String {
    if attempt == 0 {
        return name.to_owned();
    }
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => format!("{stem} ({attempt}).{extension}"),
        _ => format!("{name} ({attempt})"),
    }
}

/// A download in progress: hidden until it is whole and checked.
struct Part {
    path: PathBuf,
    /// Closed before the file is renamed or removed, which Windows needs.
    file: Option<std::fs::File>,
    placed: bool,
}

impl Part {
    fn create(directory: &Path) -> Result<Self, Failure> {
        for attempt in 0..16u32 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |time| time.subsec_nanos());
            let path = directory.join(format!(
                ".maple-download-{}-{nanos:08x}-{attempt}.part",
                std::process::id()
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        placed: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(NOT_SAVED),
            }
        }
        Err(NOT_SAVED)
    }

    fn write(&mut self, data: &[u8]) -> Result<(), Failure> {
        self.file
            .as_mut()
            .ok_or(NOT_SAVED)?
            .write_all(data)
            .map_err(|_| NOT_SAVED)
    }

    /// Mark the checked file as downloaded and close it, then give it the
    /// first free name: claim the name with an empty file nobody else can
    /// take, and move the download over it. It is marked before it has its
    /// name, so it is never in Downloads unmarked.
    fn place(mut self, directory: &Path, name: &str) -> Result<Saved, Failure> {
        let file = self.file.take().ok_or(NOT_SAVED)?;
        file.sync_all().map_err(|_| NOT_SAVED)?;
        let marked = match mark_downloaded(file, &self.path) {
            Ok(()) => true,
            Err(error) => {
                log::debug!("could not mark a downloaded file: {error}");
                false
            }
        };
        for attempt in 0..1000 {
            let path = directory.join(numbered(name, attempt));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(claim) => {
                    drop(claim);
                    if std::fs::rename(&self.path, &path).is_err() {
                        let _ = std::fs::remove_file(&path);
                        return Err(NOT_SAVED);
                    }
                    self.placed = true;
                    return Ok(Saved { path, marked });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(NOT_SAVED),
            }
        }
        Err(NOT_SAVED)
    }
}

impl Drop for Part {
    fn drop(&mut self) {
        self.file.take();
        if !self.placed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Mark a file as downloaded from elsewhere, so the system checks it before
/// it runs: quarantine on macOS, the mark of the web on Windows. Both belong
/// to the file itself and move with it. Closes `file`.
#[cfg(target_os = "macos")]
fn mark_downloaded(file: std::fs::File, _path: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_secs());
    let value = format!("0081;{seconds:08x};Maple;");
    // SAFETY: the descriptor is open, the name is NUL-terminated and
    // `value` outlives the call.
    let result = unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            c"com.apple.quarantine".as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            0,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn mark_downloaded(file: std::fs::File, path: &Path) -> std::io::Result<()> {
    drop(file);
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    std::fs::write(PathBuf::from(stream), b"[ZoneTransfer]\r\nZoneId=3\r\n")
}

#[cfg(not(any(target_os = "macos", windows)))]
fn mark_downloaded(file: std::fs::File, _path: &Path) -> std::io::Result<()> {
    drop(file);
    Ok(())
}

#[cfg(test)]
#[path = "task_file_tests.rs"]
mod tests;
