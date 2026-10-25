//! Generic composer attachments, staged once on the controller before import.
//! Copies and image decoding stay off the UI thread; only metadata reaches it.

use super::{ChatScreen, staging};
use gpui::{Context, SharedString};
use std::{path::Path, sync::Arc};

pub(super) const MAX_DRAFT_ATTACHMENTS: usize = 10;
const MAX_DRAFT_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct DraftAttachment {
    pub id: u64,
    pub name: String,
    pub mime_type: String,
    pub detail: SharedString,
    pub size: u64,
    pub staged: Option<Arc<staging::StagedFile>>,
    pub operation_id: String,
    pub upload_session: Option<String>,
    /// Reused only for the same execution task. A failed send never uploads a
    /// second copy merely because its admission response was lost.
    pub committed: Option<(String, String)>,
    pub thumbnail: Option<Arc<gpui::Image>>,
}

impl DraftAttachment {
    pub(super) fn preparing(id: u64, name: String) -> Self {
        Self {
            id,
            name,
            mime_type: String::new(),
            detail: "Preparing…".into(),
            size: 0,
            staged: None,
            operation_id: uuid::Uuid::new_v4().to_string(),
            upload_session: None,
            committed: None,
            thumbnail: None,
        }
    }
    pub(super) fn reset_upload(&mut self) {
        self.operation_id = uuid::Uuid::new_v4().to_string();
        self.committed = None;
    }

    pub(super) fn prepare_upload_for(&mut self, session: &str) {
        if self
            .upload_session
            .as_deref()
            .is_some_and(|previous| previous != session)
        {
            self.reset_upload();
        }
        self.upload_session = Some(session.to_owned());
    }

    pub(super) fn ready(&self) -> bool {
        self.staged.is_some()
    }

    #[cfg(test)]
    pub(super) fn for_test(id: u64, name: &str, ready: bool) -> Self {
        let mut draft = Self::preparing(id, name.to_string());
        if ready {
            draft.staged = Some(staging::copy_bytes(&staging::root(), b"test attachment").unwrap());
            draft.mime_type = "application/octet-stream".into();
            draft.size = 15;
            draft.detail = attachment_detail(&draft.mime_type, draft.size).into();
        }
        draft
    }
}

impl ChatScreen {
    fn remaining_attachment_slots(&mut self, cx: &mut Context<Self>) -> Option<usize> {
        let remaining = MAX_DRAFT_ATTACHMENTS.saturating_sub(self.draft_attachments.len());
        if remaining == 0 {
            self.notice =
                Some(format!("Attach at most {MAX_DRAFT_ATTACHMENTS} files at a time").into());
            cx.notify();
            return None;
        }
        Some(remaining)
    }

    fn image_plan_available(&self) -> bool {
        self.sidebar_plan.as_ref().is_none_or(|plan| {
            let label = plan.plan_label.to_lowercase();
            label.contains("pro") || label.contains("max") || label.contains("team")
        })
    }

    pub(super) fn paste_image(&mut self, image: gpui::Image, cx: &mut Context<Self>) {
        if self.remaining_attachment_slots(cx).is_none() {
            return;
        }
        if !self.image_plan_available() {
            self.notice = Some("Image attachments need a Pro, Max, or Team plan".into());
            cx.notify();
            return;
        }
        let extension = match image.format {
            gpui::ImageFormat::Jpeg => "jpg",
            gpui::ImageFormat::Webp => "webp",
            _ => "png",
        };
        let name = format!("pasted-{}.{extension}", self.draft_attachments.len() + 1);
        if let Err(message) = draft_image_mime(&name, &image.bytes) {
            self.notice = Some(message.into());
            cx.notify();
            return;
        }
        let id = self.next_draft_id();
        self.draft_attachments
            .push(DraftAttachment::preparing(id, name.clone()));
        self.notice = None;
        self.call(
            async move {
                let permit = preparation_slots()
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| "Attachment preparation stopped")?;
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let mime = draft_image_mime(&name, &image.bytes)?;
                    let staged = staging::copy_bytes(&staging::root(), &image.bytes)?;
                    let thumbnail = square_thumbnail(&image.bytes).ok().map(Arc::new);
                    Ok((
                        staged,
                        mime.to_string(),
                        image.bytes.len() as u64,
                        thumbnail,
                    ))
                })
                .await
                .map_err(|e| format!("Attachment preparation failed: {e}"))?
            },
            cx,
            move |this, result, cx| this.finish_attachment_preparation(id, result, cx),
        );
        cx.notify();
    }

    fn next_draft_id(&mut self) -> u64 {
        self.draft_counter += 1;
        self.draft_counter
    }

    pub(super) fn finish_attachment_preparation(
        &mut self,
        id: u64,
        result: Result<PreparedAttachment, String>,
        cx: &mut Context<Self>,
    ) {
        let images_allowed = self.image_plan_available();
        let Some(draft) = self
            .draft_attachments
            .iter_mut()
            .find(|draft| draft.id == id)
        else {
            return;
        };
        match result {
            Ok((staged, mime_type, size, thumbnail))
                if !mime_type.starts_with("image/") || images_allowed =>
            {
                draft.detail = attachment_detail(&mime_type, size).into();
                draft.staged = Some(staged);
                draft.mime_type = mime_type;
                draft.size = size;
                draft.thumbnail = thumbnail;
            }
            result => {
                let message = result
                    .err()
                    .unwrap_or_else(|| "Image attachments need a Pro, Max, or Team plan".into());
                self.draft_attachments.retain(|draft| draft.id != id);
                self.notice = Some(message.into());
            }
        }
        cx.notify();
    }

    pub(super) fn remove_attachment(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(index) = self
            .draft_attachments
            .iter()
            .position(|draft| draft.id == id)
        else {
            return;
        };
        let draft = self.draft_attachments.remove(index);
        if let Some(session_id) = draft.upload_session {
            let backend = self.backend.clone();
            let user = self.user_id.clone();
            let operation_id = draft.operation_id;
            // A restored draft can still have a pending/ready upload. Removing
            // its chip explicitly cancels that operation; accepted-message
            // references remain protected by the execution store.
            self.call(
                async move {
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(3),
                        backend.cancel_attachment_upload(
                            &user,
                            crate::backend::AttachmentUploadRequest {
                                session_id,
                                operation_id,
                            },
                        ),
                    )
                    .await;
                    Ok(())
                },
                cx,
                |_, _, _| {},
            );
        }
        cx.notify();
    }

    pub(super) fn pick_attachments(&mut self, cx: &mut Context<Self>) {
        if self.attachment_picking || self.remaining_attachment_slots(cx).is_none() {
            return;
        }
        self.attachment_picking = true;
        self.notice = None;
        cx.notify();
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        let bridge = cx.spawn(async move |this, cx| {
            let picked = receiver.await;
            this.update(cx, |this, cx| {
                this.attachment_picking = false;
                match picked {
                    Ok(Ok(Some(paths))) => this.add_attachment_paths(paths, cx),
                    Ok(Ok(None)) | Err(_) => {}
                    Ok(Err(error)) => {
                        this.notice =
                            Some(format!("Could not open the file dialog: {error}").into())
                    }
                }
                cx.notify();
            })
            .ok();
        });
        crate::ui::task::retain(&self.bridged_tasks, bridge);
    }

    pub(super) fn add_attachment_paths(
        &mut self,
        paths: Vec<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(remaining) = self.remaining_attachment_slots(cx) else {
            return;
        };
        self.notice = if paths.len() > remaining {
            Some(format!("Attach at most {MAX_DRAFT_ATTACHMENTS} files at a time").into())
        } else {
            None
        };
        // Reserve every slot immediately, before asynchronous work can race a
        // second drop. Dropping a preparing chip also discards its late result.
        let work: Vec<_> = paths
            .into_iter()
            .take(remaining)
            .map(|path| {
                let id = self.next_draft_id();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "attachment".into());
                self.draft_attachments
                    .push(DraftAttachment::preparing(id, name));
                (id, path)
            })
            .collect();
        for (id, path) in work {
            self.call(
                async move {
                    let permit = preparation_slots()
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|_| "Attachment preparation stopped")?;
                    tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        prepare_file(&path)
                    })
                    .await
                    .map_err(|e| format!("Attachment preparation failed: {e}"))?
                },
                cx,
                move |this, result, cx| this.finish_attachment_preparation(id, result, cx),
            );
        }
        cx.notify();
    }
}

fn preparation_slots() -> &'static Arc<tokio::sync::Semaphore> {
    static SLOTS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
}

type PreparedAttachment = (
    Arc<staging::StagedFile>,
    String,
    u64,
    Option<Arc<gpui::Image>>,
);

fn prepare_file(path: &Path) -> Result<PreparedAttachment, String> {
    use std::io::Read;
    let (staged, size) = staging::copy_file(&staging::root(), path)?;
    let mut signature = [0; 16];
    let mut file = std::fs::File::open(&staged.path).map_err(|e| e.to_string())?;
    let read = file.read(&mut signature).map_err(|e| e.to_string())?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if image_format_from_bytes(&signature[..read]).is_some() {
        if size > MAX_DRAFT_IMAGE_BYTES {
            return Err("Images must be 10 MiB or smaller".into());
        }
        let bytes = std::fs::read(&staged.path).map_err(|e| e.to_string())?;
        let mime = draft_image_mime(&name, &bytes)?.to_string();
        Ok((
            staged,
            mime,
            size,
            square_thumbnail(&bytes).ok().map(Arc::new),
        ))
    } else {
        let mime = match path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "zip" => "application/zip",
            "pdf" => "application/pdf",
            "txt" | "md" | "rs" | "py" | "js" | "ts" | "log" => "text/plain",
            "json" => "application/json",
            "csv" => "text/csv",
            _ => "application/octet-stream",
        };
        Ok((staged, mime.to_string(), size, None))
    }
}

pub(super) fn attachment_detail(mime: &str, size: u64) -> String {
    let kind = match mime {
        "application/zip" => "ZIP",
        "application/pdf" => "PDF",
        "application/octet-stream" => "File",
        mime if mime.starts_with("image/") => "Image",
        mime if mime.starts_with("text/") => "Text",
        _ => "File",
    };
    let size = if size >= 1024 * 1024 {
        format!("{:.1} MiB", size as f64 / (1024. * 1024.))
    } else if size >= 1024 {
        format!("{:.0} KiB", size as f64 / 1024.)
    } else {
        format!("{size} B")
    };
    format!("{kind} · {size}")
}

fn draft_image_mime(name: &str, bytes: &[u8]) -> Result<&'static str, String> {
    if bytes.len() as u64 > MAX_DRAFT_IMAGE_BYTES {
        return Err(format!("{name} is larger than 10 MiB"));
    }
    match image_format_from_bytes(bytes) {
        Some(gpui::ImageFormat::Png) => Ok("image/png"),
        Some(gpui::ImageFormat::Jpeg) => Ok("image/jpeg"),
        Some(gpui::ImageFormat::Webp) => Ok("image/webp"),
        _ => Err(format!("{name} is not a PNG, JPEG, or WebP image")),
    }
}

/// Thumbnail edge in physical pixels: 2x the 64pt box so it stays sharp
/// on HiDPI screens.
const DRAFT_THUMBNAIL_PX: u32 = 128;

/// Center-crop to a square and scale down, so the composer can show a
/// rounded square that is the picture itself. gpui clips with rectangular
/// masks only, so cropping the pixels is the one way to get round corners
/// on a cover-fit thumbnail.
fn square_thumbnail(bytes: &[u8]) -> Result<gpui::Image, String> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|error| error.to_string())?;
    let (width, height) = (decoded.width(), decoded.height());
    let edge = width.min(height);
    if edge == 0 {
        return Err("empty image".to_string());
    }
    let cropped = decoded.crop_imm((width - edge) / 2, (height - edge) / 2, edge, edge);
    let scaled = if edge > DRAFT_THUMBNAIL_PX {
        cropped.resize_exact(
            DRAFT_THUMBNAIL_PX,
            DRAFT_THUMBNAIL_PX,
            image::imageops::FilterType::Triangle,
        )
    } else {
        cropped
    };
    let mut png = std::io::Cursor::new(Vec::new());
    scaled
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok(gpui::Image::from_bytes(
        gpui::ImageFormat::Png,
        png.into_inner(),
    ))
}

/// Image format from the file signature; `None` for unsupported data.
pub(super) fn image_format_from_bytes(bytes: &[u8]) -> Option<gpui::ImageFormat> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some(gpui::ImageFormat::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(gpui::ImageFormat::Jpeg)
    } else if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(gpui::ImageFormat::Webp)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changing_the_draft_task_uses_a_new_upload_operation() {
        let mut draft = DraftAttachment::preparing(1, "report.zip".into());
        draft.prepare_upload_for("original-task");
        let original = draft.operation_id.clone();
        draft.prepare_upload_for("original-task");
        assert_eq!(draft.operation_id, original);
        draft.committed = Some(("original-task".into(), "old-file".into()));
        draft.prepare_upload_for("another-task");
        assert_ne!(draft.operation_id, original);
        assert!(draft.committed.is_none());
    }

    #[test]
    fn zip_attachment_is_a_stable_generic_file() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("report.zip");
        std::fs::write(&original, b"PK\x03\x04an archive").unwrap();
        let (staged, mime, size, thumbnail) = prepare_file(&original).unwrap();
        std::fs::remove_file(original).unwrap();
        assert_eq!(mime, "application/zip");
        assert_eq!(size, 14);
        assert!(thumbnail.is_none());
        assert_eq!(
            std::fs::read(&staged.path).unwrap(),
            b"PK\x03\x04an archive"
        );
    }

    #[test]
    fn generic_file_limit_does_not_raise_the_image_limit() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("large.png");
        let mut bytes = vec![0; MAX_DRAFT_IMAGE_BYTES as usize + 1];
        bytes[..8].copy_from_slice(&[0x89, b'P', b'N', b'G', 13, 10, 26, 10]);
        std::fs::write(&original, bytes).unwrap();
        assert!(prepare_file(&original).err().unwrap().contains("10 MiB"));
    }
}
