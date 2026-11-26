//! Explicit assistant-file activation and viewer-local downloads.
//!
//! A remote execution path is not a local path. Resolve and snapshot it on
//! its host, transfer through ordinary account/task-authorized range reads,
//! and open only the complete, verified local copy chosen by the viewer.

use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use gpui::{Context, Window};
use maple_agent::store::{
    AttachmentRangeReply, MAX_TASK_FILE_BYTES, TaskFileKind, TaskFileSnapshot,
};
use sha2::{Digest, Sha256};

use super::ChatScreen;
use crate::backend::{ImportTaskFileRequest, ReadAttachmentRequest};
use crate::ui::i18n::UiText;
use crate::ui::rich_text;

const TASK_FILE_CHUNK_BYTES: usize = 1024 * 1024;

impl ChatScreen {
    pub(super) fn open_task_link(
        &mut self,
        task: String,
        generation: u64,
        source: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !is_task_file_link(&source) {
            rich_text::open_link(&source);
            return;
        }
        // An old virtualized row must not activate a newly selected task.
        if self.selection_generation != generation || !self.is_selected(&task) {
            return;
        }
        if self.file_operation_pending {
            self.notice = Some(UiText::key("chat-file-download-busy"));
            cx.notify();
            return;
        }
        self.file_operation_pending = true;
        let backend = self.backend.clone();
        let user = self.user_id.clone();
        let request = ImportTaskFileRequest {
            task,
            source: source.clone(),
            kind: TaskFileKind::Download,
        };
        if backend.remote_host().is_none() {
            // This native-only resolver canonicalizes and checks the project
            // root in the daemon. It is deliberately not a remote wire call.
            self.call(
                async move { backend.task_file_source(&user, request).await },
                cx,
                move |this, result, cx| {
                    this.file_operation_pending = false;
                    if this.selection_generation == generation {
                        match result {
                            Ok(file) => cx.open_with_system(&file.path),
                            Err(_) => this.notice = Some(UiText::key("chat-file-open-failed")),
                        }
                        cx.notify();
                    }
                },
            );
            return;
        }

        let directory = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(std::env::temp_dir);
        let response =
            cx.prompt_for_new_path(&directory, Some(suggested_filename(&source).as_str()));
        cx.spawn(async move |this, cx| {
            let destination = match response.await {
                Ok(Ok(Some(destination))) => destination,
                _ => {
                    this.update(cx, |this, cx| {
                        this.file_operation_pending = false;
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            this.update(cx, |this, cx| {
                this.call(
                    async move {
                        let task = request.task.clone();
                        let snapshot = backend.import_task_file(&user, request).await?;
                        let attachment = snapshot.attachment.clone();
                        let read_user = user.clone();
                        let read_backend = backend.clone();
                        persist_snapshot(snapshot, destination, move |offset, len| {
                            let backend = read_backend.clone();
                            let user = read_user.clone();
                            let request = ReadAttachmentRequest {
                                task: task.clone(),
                                attachment: attachment.id.clone(),
                                offset,
                                len,
                            };
                            async move { backend.read_attachment(&user, request).await }
                        })
                        .await
                        .map_err(|error| error.to_string())
                    },
                    cx,
                    move |this, result, cx| {
                        this.file_operation_pending = false;
                        if this.selection_generation == generation {
                            match result {
                                Ok(path) => {
                                    this.notice = Some(UiText::key("chat-file-download-open"));
                                    cx.open_with_system(&path);
                                }
                                Err(_) => {
                                    this.notice = Some(UiText::key("chat-file-download-failed"));
                                }
                            }
                            cx.notify();
                        }
                    },
                );
            })
            .ok();
        })
        .detach();
    }
}

/// Exclude ordinary links without treating a remote-looking string as a
/// filesystem instruction. The native store still authorizes every path.
fn is_task_file_link(source: &str) -> bool {
    if source.is_empty()
        || source.len() > 4096
        || source.trim() != source
        || source.starts_with('#')
        || source.bytes().any(|byte| byte.is_ascii_control())
    {
        return false;
    }
    if let Some((scheme, rest)) = source.split_once(':')
        && scheme
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "+-.".contains(character))
    {
        // Windows absolute paths have a drive prefix, not a URL scheme.
        let drive_path = scheme.len() == 1 && (rest.starts_with('/') || rest.starts_with('\\'));
        if !drive_path {
            return false;
        }
    }
    true
}

fn suggested_filename(source: &str) -> String {
    let leaf = source.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = leaf
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(96)
        .collect();
    let cleaned = cleaned.trim_matches(|character| matches!(character, '.' | ' '));
    if cleaned.is_empty() {
        "download".to_owned()
    } else {
        cleaned.to_owned()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DownloadError {
    InvalidSnapshot,
    Transfer,
    Integrity,
    Destination,
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("The task file could not be downloaded")
    }
}

/// Transfer and write a bounded immutable snapshot. The range transport is
/// shared with sent attachments; unlike an image, the whole file must match
/// its host snapshot digest before it is opened.
async fn persist_snapshot<F, Fut>(
    snapshot: TaskFileSnapshot,
    destination: PathBuf,
    mut read_range: F,
) -> Result<PathBuf, DownloadError>
where
    F: FnMut(u64, usize) -> Fut,
    Fut: Future<Output = Result<AttachmentRangeReply, String>>,
{
    let attachment = snapshot.attachment;
    if snapshot.sha256.len() != 64
        || !snapshot
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || attachment.size > MAX_TASK_FILE_BYTES as u64
    {
        return Err(DownloadError::InvalidSnapshot);
    }
    if destination.file_name().is_none() {
        return Err(DownloadError::Destination);
    }
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let mut file =
        tempfile::NamedTempFile::new_in(parent).map_err(|_| DownloadError::Destination)?;
    let mut hasher = Sha256::new();
    let mut offset = 0u64;
    while offset < attachment.size {
        let remaining = attachment.size - offset;
        let len = usize::try_from(remaining.min(TASK_FILE_CHUNK_BYTES as u64))
            .map_err(|_| DownloadError::InvalidSnapshot)?;
        let reply = read_range(offset, len)
            .await
            .map_err(|_| DownloadError::Transfer)?;
        if reply.mime != attachment.mime_type
            || reply.size != attachment.size
            || reply.data.len() > (len + 2).saturating_mul(4)
        {
            return Err(DownloadError::Integrity);
        }
        let bytes = STANDARD
            .decode(reply.data.as_bytes())
            .map_err(|_| DownloadError::Integrity)?;
        if bytes.len() != len {
            return Err(DownloadError::Integrity);
        }
        file.write_all(&bytes)
            .map_err(|_| DownloadError::Destination)?;
        hasher.update(&bytes);
        offset = offset
            .checked_add(bytes.len() as u64)
            .ok_or(DownloadError::Integrity)?;
    }
    let digest = hasher.finalize();
    if snapshot.sha256 != hex_digest(&digest) {
        return Err(DownloadError::Integrity);
    }
    file.as_file()
        .sync_all()
        .map_err(|_| DownloadError::Destination)?;
    // Atomic no-clobber persistence avoids replacing a file that appeared
    // after the picker, or while a remote transfer was in progress.
    file.persist_noclobber(&destination)
        .map_err(|_| DownloadError::Destination)?;
    Ok(destination)
}

fn hex_digest(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use maple_agent::store::StoredAttachment;
    use std::sync::Arc;

    fn snapshot(data: &[u8]) -> TaskFileSnapshot {
        TaskFileSnapshot {
            attachment: StoredAttachment {
                id: "0123456789abcdef0123456789abcdef".into(),
                name: "notes.txt".into(),
                mime_type: "text/plain".into(),
                source: "maple-attachment://0123456789abcdef0123456789abcdef".into(),
                size: data.len() as u64,
                execution_path: String::new(),
            },
            sha256: hex_digest(&Sha256::digest(data)),
        }
    }

    fn ranges(
        data: &[u8],
    ) -> impl FnMut(
        u64,
        usize,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<AttachmentRangeReply, String>> + Send>,
    > {
        let data = Arc::new(data.to_vec());
        let snapshot = snapshot(&data);
        move |offset, len| {
            let data = data.clone();
            let snapshot = snapshot.clone();
            Box::pin(async move {
                let start = usize::try_from(offset).unwrap();
                let end = start.saturating_add(len).min(data.len());
                let Some(bytes) = data.get(start..end) else {
                    return Err("bad offset".into());
                };
                Ok(AttachmentRangeReply {
                    mime: snapshot.attachment.mime_type,
                    size: snapshot.attachment.size,
                    data: STANDARD.encode(bytes),
                })
            })
        }
    }

    #[tokio::test]
    async fn download_verifies_the_whole_file_and_never_clobbers_a_destination() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = vec![41u8; TASK_FILE_CHUNK_BYTES + 37];
        let destination = directory.path().join("notes.txt");
        persist_snapshot(snapshot(&bytes), destination.clone(), ranges(&bytes))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Nothing sensitive or project-private is exposed to other users.
            assert_eq!(
                std::fs::metadata(&destination)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        let mut wrong_hash = snapshot(&bytes);
        wrong_hash.sha256 = hex_digest(&Sha256::digest(b"different"));
        let bad = directory.path().join("wrong-hash.txt");
        assert_eq!(
            persist_snapshot(wrong_hash, bad.clone(), ranges(&bytes))
                .await
                .unwrap_err(),
            DownloadError::Integrity
        );
        assert!(!bad.exists());

        let existing = directory.path().join("existing.txt");
        std::fs::write(&existing, b"keep this").unwrap();
        assert_eq!(
            persist_snapshot(snapshot(b"new"), existing.clone(), ranges(b"new"))
                .await
                .unwrap_err(),
            DownloadError::Destination
        );
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep this");

        let empty = directory.path().join("empty.txt");
        persist_snapshot(snapshot(b""), empty.clone(), ranges(b""))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&empty).unwrap(), b"");
    }

    #[tokio::test]
    async fn download_rejects_short_malformed_and_changed_ranges() {
        let directory = tempfile::tempdir().unwrap();
        let good = snapshot(b"abcdef");
        let replies = [
            ("short", STANDARD.encode(b"abcde")),
            ("oversized", STANDARD.encode(b"abcdefg")),
            ("malformed", "not base64".into()),
        ];
        for (name, data) in replies {
            let reply = AttachmentRangeReply {
                mime: good.attachment.mime_type.clone(),
                size: good.attachment.size,
                data,
            };
            let destination = directory.path().join(name);
            assert_eq!(
                persist_snapshot(good.clone(), destination.clone(), move |_, _| {
                    let reply = reply.clone();
                    async move { Ok(reply) }
                })
                .await
                .unwrap_err(),
                DownloadError::Integrity
            );
            assert!(!destination.exists());
        }
        for (name, mime) in [
            ("mime", "application/octet-stream"),
            ("original-mime", "text/plain"),
        ] {
            let reply = AttachmentRangeReply {
                mime: mime.into(),
                size: if name == "original-mime" { 7 } else { 6 },
                data: STANDARD.encode(b"abcdef"),
            };
            let destination = directory.path().join(name);
            assert!(
                persist_snapshot(good.clone(), destination, move |_, _| {
                    let reply = reply.clone();
                    async move { Ok(reply) }
                })
                .await
                .is_err()
            );
        }
    }

    #[test]
    fn file_links_do_not_capture_web_or_other_url_schemes() {
        for source in [
            "/workspace/report.pdf",
            "artifacts/report.pdf",
            "../artifacts/report.pdf",
            r"C:\work\report.pdf",
            "report.pdf",
        ] {
            assert_eq!(suggested_filename(source), "report.pdf");
            assert!(is_task_file_link(source));
        }
        for source in [
            "https://maple.ai/docs",
            "mailto:someone@example.com",
            "file:///tmp/report.txt",
            "data:text/plain,hello",
            "maple-attachment://0123456789abcdef0123456789abcdef",
            "#section",
            " bad path.txt ",
        ] {
            assert!(!is_task_file_link(source));
        }
        assert_eq!(
            suggested_filename(r"\bad:file?.txt\report .pdf"),
            "report_.pdf"
        );
        assert_eq!(suggested_filename(".."), "download");
    }
}
