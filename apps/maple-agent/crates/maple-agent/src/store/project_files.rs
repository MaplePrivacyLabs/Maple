//! Files a task's replies name inside its project folder.
//!
//! A reply names a file with a Markdown link or picture: a relative or
//! absolute path, a `file:` URL, or either percent-encoded. Everything here
//! resolves on the execution computer and only inside the task's project
//! folder; a symbolic link counts only if it really leads inside.
//!
//! On the execution computer the desktop opens the file where it is
//! ([`Store::locate_task_file`]). A paired viewer has no such path, so it
//! downloads the file in pieces ([`Store::read_task_file`]); each piece names
//! the version of the file it was read from, and the whole file's digest
//! comes with the first piece, so a file that changes midway is refused.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{active_task, hex, normalized_name};
use crate::store::{AccountId, Store, StoreError, StoreResult};

/// What the agent is told, so its replies link the files it makes.
pub const REPLY_FILE_LINKS_GUIDANCE: &str = "To give the user a file, save it inside the task folder and link it in Markdown, such as [report.pdf](out/report.pdf). The user opens it with a click, or downloads it when they are on another computer.";

/// Largest file a viewer on another computer may download.
pub const MAX_TASK_FILE_BYTES: u64 = 100 * 1024 * 1024;
/// Largest piece of a task file one read returns.
pub const MAX_TASK_FILE_RANGE_BYTES: usize = 1024 * 1024;
pub(super) const MAX_LINK_BYTES: usize = 4096;

/// A file or folder a link names, on the execution computer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskFileLocation {
    pub path: PathBuf,
    pub folder: bool,
}

/// A piece of a file a link names, for a viewer on another computer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskFileRange {
    /// The file's own name, made safe to save under.
    pub name: String,
    pub size: u64,
    /// This version of the file. Later reads pass it back and are refused
    /// once the file changes.
    pub version: String,
    /// SHA-256 of the whole file, with a read that named no version.
    pub sha256: Option<String>,
    /// Base64 of the bytes from the requested offset.
    pub data: String,
}

/// Why a link opens nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskFileRefusal {
    /// Nothing is there, or it is not a file or folder.
    Missing,
    /// It lies outside the task's project folder.
    Outside,
    /// A folder, which only the execution computer opens.
    Folder,
    /// Larger than [`MAX_TASK_FILE_BYTES`].
    TooLarge,
    /// The file changed since the version the viewer is reading.
    Changed,
}

impl Store {
    /// The project folder of a task this account owns, if it still exists.
    pub(super) fn task_project_root(
        &self,
        account: AccountId,
        task_uuid: &str,
    ) -> StoreResult<Option<ProjectRoot>> {
        let root = self.read(|tx| {
            let task = active_task(tx, account, task_uuid)?;
            Ok(tx.query_row(
                "SELECT p.root FROM projects p JOIN tasks t ON t.project_id=p.id WHERE t.id=?1",
                [task],
                |row| row.get::<_, String>(0),
            )?)
        })?;
        Ok(ProjectRoot::new(&root).filter(|root| root.canonical.is_dir()))
    }

    /// Where a link of a task's reply leads on this computer. Only for the
    /// desktop of the execution computer: a paired viewer has no use for
    /// this path, and opening a lookalike path of its own would be wrong.
    pub fn locate_task_file(
        &self,
        account: AccountId,
        task_uuid: &str,
        link: &str,
    ) -> StoreResult<Result<TaskFileLocation, TaskFileRefusal>> {
        let Some(root) = self.task_project_root(account, task_uuid)? else {
            return Ok(Err(TaskFileRefusal::Missing));
        };
        Ok(resolve(&root, link).and_then(|path| {
            let metadata = std::fs::metadata(&path).map_err(|_| TaskFileRefusal::Missing)?;
            if metadata.is_dir() || metadata.is_file() {
                Ok(TaskFileLocation {
                    folder: metadata.is_dir(),
                    path,
                })
            } else {
                Err(TaskFileRefusal::Missing)
            }
        }))
    }

    /// Read a piece of a file a link of a task's reply names. Without
    /// `version` the reply also carries the digest of the whole file.
    pub fn read_task_file(
        &self,
        account: AccountId,
        task_uuid: &str,
        link: &str,
        offset: u64,
        len: usize,
        version: Option<&str>,
    ) -> StoreResult<Result<TaskFileRange, TaskFileRefusal>> {
        if len > MAX_TASK_FILE_RANGE_BYTES {
            return Err(StoreError::InvalidInput);
        }
        let Some(root) = self.task_project_root(account, task_uuid)? else {
            return Ok(Err(TaskFileRefusal::Missing));
        };
        let path = match resolve(&root, link) {
            Ok(path) => path,
            Err(refusal) => return Ok(Err(refusal)),
        };
        if path.is_dir() {
            return Ok(Err(TaskFileRefusal::Folder));
        }
        let Some((path, mut file)) = open_confined(&root, &path) else {
            return Ok(Err(TaskFileRefusal::Missing));
        };
        let metadata = file.metadata()?;
        let size = metadata.len();
        if size > MAX_TASK_FILE_BYTES {
            return Ok(Err(TaskFileRefusal::TooLarge));
        }
        let current = file_version(&metadata);
        if version.is_some_and(|version| version != current) {
            return Ok(Err(TaskFileRefusal::Changed));
        }
        // The digest comes with the first piece, and only with it.
        if offset > size || (version.is_none() && offset != 0) {
            return Err(StoreError::InvalidInput);
        }
        let sha256 = match version {
            Some(_) => None,
            None => {
                let mut hash = Sha256::new();
                let copied = std::io::copy(&mut (&mut file).take(size + 1), &mut hash)?;
                // A file written while it was hashed has no one version.
                if copied != size || file_version(&file.metadata()?) != current {
                    return Ok(Err(TaskFileRefusal::Changed));
                }
                Some(hex(&hash.finalize()))
            }
        };
        file.seek(SeekFrom::Start(offset))?;
        let mut data = vec![0; len.min((size - offset) as usize)];
        // A file cut short or rewritten while it is read has changed.
        match file.read_exact(&mut data) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(Err(TaskFileRefusal::Changed));
            }
            Err(error) => return Err(error.into()),
        }
        if file_version(&file.metadata()?) != current {
            return Ok(Err(TaskFileRefusal::Changed));
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return Ok(Err(TaskFileRefusal::Missing));
        };
        Ok(Ok(TaskFileRange {
            name: normalized_name(name),
            size,
            version: current,
            sha256,
            data: STANDARD.encode(data),
        }))
    }
}

/// The canonical path inside the project a link leads to.
fn resolve(root: &ProjectRoot, link: &str) -> Result<PathBuf, TaskFileRefusal> {
    let mut refusal = TaskFileRefusal::Missing;
    for candidate in local_candidates(link) {
        match confined_path(root, &candidate) {
            Ok(path) => return Ok(path),
            Err(TaskFileRefusal::Outside) => refusal = TaskFileRefusal::Outside,
            Err(_) => {}
        }
    }
    Err(refusal)
}

/// Identifies one version of a file by where it lives, its size and its
/// times. Opaque to the viewer.
fn file_version(metadata: &std::fs::Metadata) -> String {
    let mut hash = Sha256::new();
    hash.update(b"maple-task-file-version-v1\0");
    hash.update(metadata.len().to_le_bytes());
    for time in [metadata.modified(), metadata.created()] {
        let nanos = time
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |time| time.as_nanos());
        hash.update(nanos.to_le_bytes());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        hash.update(metadata.dev().to_le_bytes());
        hash.update(metadata.ino().to_le_bytes());
        hash.update(metadata.ctime().to_le_bytes());
        hash.update(metadata.ctime_nsec().to_le_bytes());
    }
    hex(&hash.finalize()[..16])
}

/// Filesystem paths a link or picture may name. URLs other than `file:`
/// name nothing here; a percent-encoded path is tried as written, then
/// decoded.
pub(super) fn local_candidates(destination: &str) -> Vec<PathBuf> {
    if destination.is_empty()
        || destination.len() > MAX_LINK_BYTES
        || destination.chars().any(char::is_control)
    {
        return Vec::new();
    }
    if let Some(rest) = strip_prefix_ignore_case(destination, "file://") {
        let rest = strip_prefix_ignore_case(rest, "localhost").unwrap_or(rest);
        // A URL's query and fragment name no part of the file.
        let rest = rest.split(['?', '#']).next().unwrap_or_default();
        if !rest.starts_with('/') {
            return Vec::new();
        }
        let Some(decoded) = percent_decode(rest) else {
            return Vec::new();
        };
        // `file:///C:/work/a.png` names `C:/work/a.png` on Windows.
        let path = match decoded.as_bytes() {
            [b'/', drive, b':', ..] if cfg!(windows) && drive.is_ascii_alphabetic() => {
                decoded[1..].to_owned()
            }
            _ => decoded,
        };
        return vec![PathBuf::from(path)];
    }
    if has_url_scheme(destination) {
        return Vec::new();
    }
    let mut candidates = vec![PathBuf::from(destination)];
    if destination.contains('%')
        && let Some(decoded) = percent_decode(destination)
        && decoded != destination
    {
        candidates.push(PathBuf::from(decoded));
    }
    candidates
}

fn strip_prefix_ignore_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value
        .get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &value[prefix.len()..])
}

/// A URL scheme, as opposed to a Windows drive letter (`C:\` or `C:/`).
fn has_url_scheme(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once(':') else {
        return false;
    };
    let drive = scheme.len() == 1 && (rest.starts_with('/') || rest.starts_with('\\'));
    !drive
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3)?;
            let text = std::str::from_utf8(hex).ok()?;
            decoded.push(u8::from_str_radix(text, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// A task's project folder as the store records it and as it really is. A
/// path may name the folder either way.
#[derive(Clone, Debug)]
pub(super) struct ProjectRoot {
    recorded: PathBuf,
    canonical: PathBuf,
}

impl ProjectRoot {
    fn new(recorded: &str) -> Option<Self> {
        let canonical = std::fs::canonicalize(recorded).ok()?;
        Some(Self {
            recorded: lexical(Path::new(recorded)).unwrap_or_else(|| canonical.clone()),
            canonical,
        })
    }

    /// Where `candidate` leads, worked out from the text alone, if that is
    /// inside the folder. Nothing else is ever touched, even to look: a
    /// path on another computer's share must not make this one connect.
    fn within(&self, candidate: &Path) -> Option<PathBuf> {
        let joined = if candidate.is_absolute() {
            candidate.to_owned()
        } else {
            self.canonical.join(candidate)
        };
        let path = lexical(&joined)?;
        [&self.canonical, &self.recorded]
            .into_iter()
            .any(|root| path.starts_with(root) && path != *root)
            .then_some(path)
    }
}

/// `path` with `.` and `..` worked out from the text; `None` if `..` would
/// climb above its root.
fn lexical(path: &Path) -> Option<PathBuf> {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    resolved.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    return None;
                }
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    Some(resolved)
}

/// Where `candidate` really leads, if that is inside the project and not
/// the folder itself. The text is checked before anything is looked up.
fn confined_path(root: &ProjectRoot, candidate: &Path) -> Result<PathBuf, TaskFileRefusal> {
    let requested = root.within(candidate).ok_or(TaskFileRefusal::Outside)?;
    resolved(root, &requested)
}

/// Where `requested`, already inside the folder by its text, really leads.
fn resolved(root: &ProjectRoot, requested: &Path) -> Result<PathBuf, TaskFileRefusal> {
    let path = std::fs::canonicalize(requested).map_err(|_| TaskFileRefusal::Missing)?;
    if !path.starts_with(&root.canonical) || path == root.canonical {
        return Err(TaskFileRefusal::Outside);
    }
    Ok(path)
}

/// Open a regular file inside the project. Symbolic links are followed only
/// to where they really lead, which must also be inside.
pub(super) fn open_confined(root: &ProjectRoot, candidate: &Path) -> Option<(PathBuf, File)> {
    let requested = root.within(candidate)?;
    let path = resolved(root, &requested).ok()?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A FIFO must not hold the reading thread. The path is canonical, so
        // a link here was swapped in after the check: refuse to follow it.
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options.open(&path).ok()?;
    let opened = file.metadata().ok()?;
    if !opened.is_file() {
        return None;
    }
    // The path must still lead inside the project to the file just opened,
    // so swapping a link between the check and the open reads nothing else.
    let again = std::fs::canonicalize(&requested).ok()?;
    if again != path {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let current = std::fs::metadata(&again).ok()?;
        if (current.dev(), current.ino()) != (opened.dev(), opened.ino()) {
            return None;
        }
    }
    #[cfg(windows)]
    {
        let current = File::open(&again).ok()?;
        if file_identity(&current)? != file_identity(&file)? {
            return None;
        }
    }
    Some((path, file))
}

/// The volume and file index of an open file.
#[cfg(windows)]
fn file_identity(file: &File) -> Option<(u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle stays open for the call and `information` is a
    // valid out-pointer.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return None;
    }
    Some((
        information.dwVolumeSerialNumber,
        information.nFileIndexHigh,
        information.nFileIndexLow,
    ))
}

#[cfg(test)]
#[path = "project_file_tests.rs"]
mod tests;
