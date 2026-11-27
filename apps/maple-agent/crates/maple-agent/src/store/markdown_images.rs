//! Pictures an assistant reply names in Markdown (`![alt](path)`).
//!
//! When the execution computer saves a reply, each picture it names inside
//! the task's project folder is copied into the task's attachments, so the
//! message keeps the picture it had when it was written: a later edit of the
//! file changes nothing. The reply's row lists those copies under
//! `input.markdownImages`, and a viewer reads them like any other attachment,
//! on this computer or from a paired one.
//!
//! Nothing here can fail a save. A picture that is missing, outside the
//! project, too large or not a picture stays as its alternative text.

use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use pi_ai::Message;
use pi_coding_agent::{
    SessionMessage,
    session::{EntryKind, SessionEntry},
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    AttachmentUploadMetadata, MAX_GENERATED_IMAGE_BYTES, active_task, detected_image_mime, hex,
    normalized_name,
};
use crate::store::{AccountId, Store, StoreResult};

/// What the agent is told, so its replies show pictures at all.
pub const REPLY_PICTURES_GUIDANCE: &str = "Maple shows pictures in your replies. To show the user an image or GIF, save it inside the task folder and put it on its own line as Markdown: ![short description](relative/path.png). PNG, JPEG, WebP and GIF files up to 20 MB are shown as they are when you reply; later edits to the file do not change a sent reply.";

/// Row `input` key holding the pictures of an assistant reply.
pub const MARKDOWN_IMAGES_KEY: &str = "markdownImages";
/// Distinct pictures copied for one reply; later ones keep their text.
pub const MAX_MARKDOWN_IMAGES: usize = 8;
/// Distinct picture destinations of one reply considered for those copies,
/// so web pictures or missing files ahead of a real one do not crowd it out.
pub const MAX_PICTURE_DESTINATIONS: usize = 32;
const MAX_DESTINATION_BYTES: usize = 4096;
const MAX_IMAGE_EDGE: u32 = 16_384;

/// One picture of a reply as its row shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkdownImage {
    /// The destination exactly as the reply wrote it.
    pub source: String,
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub size: u64,
}

/// Destinations of the pictures `text` shows on their own: images outside
/// table cells, headings and links, in order, each once, at most
/// [`MAX_PICTURE_DESTINATIONS`]. The desktop's Markdown renderer draws exactly
/// these as pictures; any other image keeps its alternative text inline.
pub fn image_destinations(text: &str) -> Vec<String> {
    let mut options = Options::empty();
    // The desktop renderer's options: tables change where images can sit.
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    let mut found: Vec<String> = Vec::new();
    let (mut cells, mut headings, mut links, mut images) = (0usize, 0usize, 0usize, 0usize);
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(Tag::TableCell) => cells += 1,
            Event::End(TagEnd::TableCell) => cells = cells.saturating_sub(1),
            Event::Start(Tag::Heading { .. }) => headings += 1,
            Event::End(TagEnd::Heading(_)) => headings = headings.saturating_sub(1),
            Event::Start(Tag::Link { .. }) => links += 1,
            Event::End(TagEnd::Link) => links = links.saturating_sub(1),
            Event::Start(Tag::Image { dest_url, .. }) => {
                if images == 0 && cells == 0 && headings == 0 && links == 0 {
                    let destination = dest_url.into_string();
                    if !found.contains(&destination) {
                        found.push(destination);
                    }
                }
                images += 1;
            }
            Event::End(TagEnd::Image) => images = images.saturating_sub(1),
            _ => {}
        }
    }
    found.truncate(MAX_PICTURE_DESTINATIONS);
    found
}

/// The text of an assistant reply, as its row shows it.
fn reply_text(entry: &SessionEntry) -> Option<String> {
    match &entry.kind {
        EntryKind::Message {
            message: SessionMessage::Llm(Message::Assistant(reply)),
        } => Some(reply.text()),
        _ => None,
    }
}

/// The saved copy's operation: one per task, reply and destination, so a
/// retried save finds the copy it already made instead of reading the file
/// again, and a row rebuild finds it without touching the file at all.
fn operation_id(task_uuid: &str, entry_id: &str, destination: &str) -> String {
    let mut identity = Sha256::new();
    for part in [task_uuid, entry_id, destination] {
        identity.update((part.len() as u64).to_le_bytes());
        identity.update(part.as_bytes());
    }
    format!("markdown-{}", hex(&identity.finalize()))
}

/// Pictures of a stored reply, for its row. Reads metadata only.
pub(in crate::store) fn entry_markdown_images_tx(
    tx: &Transaction<'_>,
    task: i64,
    entry: &SessionEntry,
) -> StoreResult<Vec<MarkdownImage>> {
    let Some(text) = reply_text(entry) else {
        return Ok(Vec::new());
    };
    let destinations = image_destinations(&text);
    if destinations.is_empty() {
        return Ok(Vec::new());
    }
    let uuid: String = tx.query_row("SELECT uuid FROM tasks WHERE id=?1", [task], |row| {
        row.get(0)
    })?;
    let mut images = Vec::new();
    for source in destinations {
        if images.len() == MAX_MARKDOWN_IMAGES {
            break;
        }
        let operation = operation_id(&uuid, &entry.id, &source);
        let found = tx
            .query_row(
                "SELECT public_id,name,mime,size FROM file_attachments WHERE task_id=?1 AND operation_id=?2 AND state='ready'",
                params![task, operation],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        if let Some((id, name, mime_type, size)) = found {
            images.push(MarkdownImage {
                source,
                id,
                name,
                mime_type,
                size: u64::try_from(size).unwrap_or(0),
            });
        }
    }
    Ok(images)
}

/// Put a reply's pictures on its text row.
pub(in crate::store) fn apply_markdown_images<'a>(
    rows: impl IntoIterator<Item = &'a mut crate::agent::AgentTimelineItem>,
    images: &[MarkdownImage],
) {
    if images.is_empty() {
        return;
    }
    if let Some(row) = rows
        .into_iter()
        .find(|row| row.item_type == "message" && row.role.as_deref() == Some("assistant"))
    {
        row.input = Some(serde_json::json!({ MARKDOWN_IMAGES_KEY: images }));
    }
}

impl Store {
    /// Copy the pictures a new reply names into its task. Returns the
    /// attachments the save pins together with the reply. Never fails: a
    /// picture that cannot be copied keeps its alternative text.
    pub(in crate::store) fn import_markdown_images(
        &self,
        account: AccountId,
        task_uuid: &str,
        entry: &SessionEntry,
    ) -> Vec<String> {
        let Some(text) = reply_text(entry) else {
            return Vec::new();
        };
        let destinations = image_destinations(&text);
        if destinations.is_empty() {
            return Vec::new();
        }
        let mut root = None;
        let mut ids = Vec::new();
        for destination in destinations {
            if ids.len() == MAX_MARKDOWN_IMAGES {
                break;
            }
            let operation = operation_id(task_uuid, &entry.id, &destination);
            match self.import_markdown_image(
                account,
                task_uuid,
                &operation,
                &destination,
                &mut root,
            ) {
                Ok(Some(id)) => ids.push(id),
                Ok(None) => {}
                Err(error) => log::debug!("A reply picture was not copied: {error}"),
            }
        }
        ids
    }

    fn import_markdown_image(
        &self,
        account: AccountId,
        task_uuid: &str,
        operation: &str,
        destination: &str,
        root: &mut Option<ProjectRoot>,
    ) -> StoreResult<Option<String>> {
        let existing = self.read(|tx| {
            let task = active_task(tx, account, task_uuid)?;
            Ok(tx
                .query_row(
                    "SELECT public_id FROM file_attachments WHERE task_id=?1 AND operation_id=?2 AND state='ready'",
                    params![task, operation],
                    |row| row.get::<_, String>(0),
                )
                .optional()?)
        })?;
        if existing.is_some() {
            return Ok(existing);
        }
        let candidates = local_candidates(destination);
        if candidates.is_empty() {
            return Ok(None);
        }
        let root = match root {
            Some(root) => root.clone(),
            None => {
                let found = self.read(|tx| {
                    let task = active_task(tx, account, task_uuid)?;
                    Ok(tx.query_row(
                        "SELECT p.root FROM projects p JOIN tasks t ON t.project_id=p.id WHERE t.id=?1",
                        [task],
                        |row| row.get::<_, String>(0),
                    )?)
                })?;
                let Some(found) = ProjectRoot::new(&found) else {
                    return Ok(None);
                };
                *root = Some(found.clone());
                found
            }
        };
        let Some((path, file)) = candidates
            .iter()
            .find_map(|candidate| open_confined(&root, candidate))
        else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        file.take(MAX_GENERATED_IMAGE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.is_empty() || bytes.len() > MAX_GENERATED_IMAGE_BYTES {
            return Ok(None);
        }
        let Some(mime_type) = detected_image_mime(&bytes) else {
            return Ok(None);
        };
        let Ok((width, height)) = image::ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(drop)
            .and_then(|reader| reader.into_dimensions().map_err(drop))
        else {
            return Ok(None);
        };
        if width == 0 || height == 0 || width > MAX_IMAGE_EDGE || height > MAX_IMAGE_EDGE {
            return Ok(None);
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return Ok(None);
        };
        let metadata = AttachmentUploadMetadata {
            name: normalized_name(name),
            mime_type: mime_type.to_owned(),
            size: bytes.len() as u64,
            sha256: hex(&Sha256::digest(&bytes)),
        };
        let stored =
            self.import_attachment_bytes(account, task_uuid, operation, metadata, &bytes, true)?;
        Ok(Some(stored.id))
    }
}

/// Filesystem paths a destination may name. URLs other than `file:` name
/// nothing here; a percent-encoded path is tried as written, then decoded.
fn local_candidates(destination: &str) -> Vec<PathBuf> {
    if destination.is_empty()
        || destination.len() > MAX_DESTINATION_BYTES
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
    pub(super) fn new(recorded: &str) -> Option<Self> {
        let canonical = std::fs::canonicalize(recorded).ok()?;
        Some(Self {
            recorded: lexical(Path::new(recorded)).unwrap_or_else(|| canonical.clone()),
            canonical,
        })
    }

    pub(super) fn canonical(&self) -> &Path {
        &self.canonical
    }

    /// Where `candidate` leads, worked out from the text alone, if that is
    /// inside the folder. Nothing else is ever touched, even to look: a
    /// path on another computer's share must not make this one connect.
    pub(super) fn within(&self, candidate: &Path) -> Option<PathBuf> {
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

/// Open a regular file inside the project. Symbolic links are followed only
/// to where they really lead, which must also be inside.
fn open_confined(root: &ProjectRoot, candidate: &Path) -> Option<(PathBuf, File)> {
    let requested = root.within(candidate)?;
    let path = std::fs::canonicalize(&requested).ok()?;
    if !path.starts_with(root.canonical()) || path == root.canonical() {
        return None;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A FIFO must not hold the saving thread. The path is canonical, so
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
mod tests {
    use super::*;

    #[test]
    fn destinations_are_standalone_pictures_only() {
        let text = "Before ![chart](out/chart.png) after.\n\n\
            ![chart again](out/chart.png)\n\n\
            # Title ![logo](logo.png)\n\n\
            [![badge](badge.svg)](https://ci.example)\n\n\
            | a | b |\n|---|---|\n| ![cell](cell.png) | x |\n\n\
            > ![quoted](quote.gif)\n\n\
            - ![listed](list.webp)\n";
        assert_eq!(
            image_destinations(text),
            ["out/chart.png", "quote.gif", "list.webp"]
        );
        let many: String = (0..40).map(|n| format!("![{n}](p{n}.png)\n\n")).collect();
        assert_eq!(image_destinations(&many).len(), MAX_PICTURE_DESTINATIONS);
    }

    #[test]
    fn candidates_admit_paths_and_file_urls_only() {
        for (destination, expected) in [
            ("shot.png", vec!["shot.png"]),
            (
                "out/My%20Shot.png",
                vec!["out/My%20Shot.png", "out/My Shot.png"],
            ),
            ("/work/a.gif", vec!["/work/a.gif"]),
            ("file:///work/a%20b.png", vec!["/work/a b.png"]),
            ("file://localhost/work/a.png", vec!["/work/a.png"]),
            ("file:///work/a.png?v=2#top", vec!["/work/a.png"]),
            (r"C:\work\a.png", vec![r"C:\work\a.png"]),
        ] {
            assert_eq!(
                local_candidates(destination),
                expected.into_iter().map(PathBuf::from).collect::<Vec<_>>(),
                "{destination}"
            );
        }
        for destination in [
            "",
            "https://example.com/a.png",
            "data:image/png;base64,AAAA",
            "maple-attachment://0123456789abcdef0123456789abcdef",
            "file://server/share/a.png",
            "javascript:alert(1)",
        ] {
            assert!(local_candidates(destination).is_empty(), "{destination}");
        }
        assert!(local_candidates(&"a".repeat(MAX_DESTINATION_BYTES + 1)).is_empty());
    }

    fn check_within(root: &ProjectRoot, cases: &[(&str, Option<&str>)]) {
        for (candidate, expected) in cases {
            assert_eq!(
                root.within(Path::new(candidate)),
                expected.map(PathBuf::from),
                "{candidate}"
            );
        }
    }

    /// Only the text decides; nothing outside the project is looked up, so
    /// a share on another computer is never contacted.
    #[cfg(unix)]
    #[test]
    fn only_paths_inside_the_project_are_looked_up() {
        let root = ProjectRoot {
            recorded: PathBuf::from("/work/project"),
            canonical: PathBuf::from("/private/work/project"),
        };
        check_within(
            &root,
            &[
                ("out/a.png", Some("/private/work/project/out/a.png")),
                ("./out/../b.png", Some("/private/work/project/b.png")),
                ("/work/project/c.png", Some("/work/project/c.png")),
                (
                    "/private/work/project/d.png",
                    Some("/private/work/project/d.png"),
                ),
                ("../e.png", None),
                ("out/../../f.png", None),
                ("/etc/passwd", None),
                ("/work/projectx/g.png", None),
                ("//attacker.example/share/a.png", None),
                ("/work/project", None),
                ("/work/project/..", None),
            ],
        );
    }

    #[cfg(windows)]
    #[test]
    fn only_paths_inside_the_project_are_looked_up() {
        let root = ProjectRoot {
            recorded: PathBuf::from(r"C:\work\project"),
            canonical: PathBuf::from(r"\\?\C:\work\project"),
        };
        check_within(
            &root,
            &[
                (r"out\a.png", Some(r"\\?\C:\work\project\out\a.png")),
                // Joining onto a `\\?\` path splits `/` and works out `.`
                // and `..` too.
                ("out/a.png", Some(r"\\?\C:\work\project\out\a.png")),
                ("./out/../b.png", Some(r"\\?\C:\work\project\b.png")),
                (r"C:\work\project\c.png", Some(r"C:\work\project\c.png")),
                ("C:/work/project/f.png", Some(r"C:\work\project\f.png")),
                // Drive letters compare in either case.
                (r"c:\work\project\d.png", Some(r"C:\work\project\d.png")),
                (r"..\e.png", None),
                ("../e.png", None),
                (r"C:\Windows\win.ini", None),
                (r"\\attacker.example\share\a.png", None),
                ("//attacker.example/share/a.png", None),
                (r"\\?\UNC\attacker.example\share\a.png", None),
                (r"\\.\pipe\x", None),
                (r"C:a.png", None),
            ],
        );
    }
}
