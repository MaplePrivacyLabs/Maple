//! Generic composer attachments, staged once on the controller before import.
//! Copies and image decoding stay off the UI thread; only metadata reaches it.

use super::{ChatScreen, staging};
use crate::ui::i18n::{MessageCache, UiText, tr};
use gpui::{Context, SharedString};
use std::{path::Path, sync::Arc};

pub(super) const MAX_DRAFT_ATTACHMENTS: usize = 10;
const MAX_DRAFT_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct DraftAttachment {
    pub id: u64,
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    pub staged: Option<Arc<staging::StagedFile>>,
    pub operation_id: String,
    pub upload_session: Option<String>,
    /// Reused only for the same execution task. A failed send never uploads a
    /// second copy merely because its admission response was lost.
    pub committed: Option<(String, String)>,
    /// The picture cropped to a square, for the composer's chip.
    pub thumbnail: Option<Arc<gpui::Image>>,
    /// The whole picture, small: what the sent message shows, so the
    /// message being sent draws the same thumbnail.
    pub picture: Option<Arc<gpui::Image>>,
}

/// The two small pictures of a draft image, made in one decode.
#[derive(Clone, Debug)]
pub(super) struct DraftPictures {
    /// Cropped to a square, for the composer's chip.
    pub square: Arc<gpui::Image>,
    /// The whole picture, scaled to fit a message thumbnail.
    pub whole: Arc<gpui::Image>,
}

#[derive(Debug)]
enum PreparationError {
    Raw(String),
    Staging(staging::Failure),
    Stopped,
    Failed(String),
    ImageTooLarge,
    NamedImageTooLarge(String),
    UnsupportedImage(String),
}

impl From<String> for PreparationError {
    fn from(error: String) -> Self {
        Self::Raw(error)
    }
}

impl From<staging::Failure> for PreparationError {
    fn from(error: staging::Failure) -> Self {
        Self::Staging(error)
    }
}

impl PreparationError {
    fn into_text(self) -> UiText {
        match self {
            Self::Raw(error) => error.into(),
            Self::Staging(error) => match error {
                staging::Failure::RetainedFileLimit => UiText::key("chat-staging-file-limit"),
                staging::Failure::RetainedByteLimit => UiText::key("chat-staging-byte-limit"),
                staging::Failure::UnsafeDirectory => UiText::key("chat-staging-unsafe-directory"),
                #[cfg(unix)]
                staging::Failure::NonPrivateDirectory => {
                    UiText::key("chat-staging-private-directory")
                }
                staging::Failure::NotAFile => UiText::key("chat-staging-not-file"),
                staging::Failure::FileTooLarge => UiText::key("chat-staging-file-too-large"),
                staging::Failure::FileGrew => UiText::key("chat-staging-file-grew"),
                staging::Failure::PrepareDirectory(error) => {
                    UiText::context("chat-staging-prepare-failed", "error", error.into())
                }
                staging::Failure::OpenAttachment(error) => {
                    UiText::context("chat-staging-open-failed", "error", error.into())
                }
                staging::Failure::Raw(error) => error.into(),
            },
            Self::Stopped => UiText::key("chat-attachment-stopped"),
            Self::Failed(error) => {
                UiText::message("chat-attachment-failed", [("error", error.into())])
            }
            Self::ImageTooLarge => UiText::key("chat-image-too-large"),
            Self::NamedImageTooLarge(name) => {
                UiText::message("chat-image-name-too-large", [("name", name.into())])
            }
            Self::UnsupportedImage(name) => {
                UiText::message("chat-image-unsupported", [("name", name.into())])
            }
        }
    }
}

impl DraftAttachment {
    pub(super) fn preparing(id: u64, name: String) -> Self {
        Self {
            id,
            name,
            mime_type: String::new(),
            size: 0,
            staged: None,
            operation_id: uuid::Uuid::new_v4().to_string(),
            upload_session: None,
            committed: None,
            thumbnail: None,
            picture: None,
        }
    }
    pub(super) fn detail(&self, messages: &MessageCache) -> SharedString {
        if self.ready() {
            cached_attachment_detail(&self.mime_type, self.size, messages)
        } else {
            tr("chat-preparing")
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
        }
        draft
    }
}

impl ChatScreen {
    fn remaining_attachment_slots(&mut self, cx: &mut Context<Self>) -> Option<usize> {
        let remaining = MAX_DRAFT_ATTACHMENTS.saturating_sub(self.draft_attachments.len());
        if remaining == 0 {
            self.notice = Some(UiText::message(
                "chat-attachment-limit",
                [("count", MAX_DRAFT_ATTACHMENTS.into())],
            ));
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
            self.notice = Some(UiText::key("chat-image-plan"));
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
            self.notice = Some(message.into_text());
            cx.notify();
            return;
        }
        let id = self.next_draft_id();
        self.draft_attachments
            .push(DraftAttachment::preparing(id, name.clone()));
        self.notice = None;
        self.call_semantic(
            async move {
                Ok::<_, String>(
                    async move {
                        let permit = preparation_slots()
                            .clone()
                            .acquire_owned()
                            .await
                            .map_err(|_| PreparationError::Stopped)?;
                        tokio::task::spawn_blocking(move || {
                            let _permit = permit;
                            let mime = draft_image_mime(&name, &image.bytes)?;
                            let staged = staging::copy_bytes(&staging::root(), &image.bytes)?;
                            let pictures = draft_pictures(&image.bytes).ok();
                            Ok((staged, mime.to_string(), image.bytes.len() as u64, pictures))
                        })
                        .await
                        .map_err(|e| PreparationError::Failed(e.to_string()))?
                    }
                    .await,
                )
            },
            cx,
            move |this, result, cx| {
                this.finish_attachment_preparation(
                    id,
                    result
                        .map_err(UiText::from)
                        .and_then(|prepared| prepared.map_err(PreparationError::into_text)),
                    cx,
                )
            },
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
        result: Result<PreparedAttachment, UiText>,
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
            Ok((staged, mime_type, size, pictures))
                if !mime_type.starts_with("image/") || images_allowed =>
            {
                draft.staged = Some(staged);
                draft.mime_type = mime_type;
                draft.size = size;
                let (square, whole) = pictures
                    .map(|pictures| (pictures.square, pictures.whole))
                    .unzip();
                draft.thumbnail = square;
                draft.picture = whole;
            }
            result => {
                let message = result
                    .err()
                    .unwrap_or_else(|| UiText::key("chat-image-plan"));
                self.draft_attachments.retain(|draft| draft.id != id);
                self.notice = Some(message);
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
            self.call_semantic(
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
                    Ok::<_, String>(())
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
                        this.notice = Some(UiText::message(
                            "chat-file-dialog-failed",
                            [("error", error.to_string().into())],
                        ))
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
            Some(UiText::message(
                "chat-attachment-limit",
                [("count", MAX_DRAFT_ATTACHMENTS.into())],
            ))
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
            self.call_semantic(
                async move {
                    Ok::<_, String>(
                        async move {
                            let permit = preparation_slots()
                                .clone()
                                .acquire_owned()
                                .await
                                .map_err(|_| PreparationError::Stopped)?;
                            tokio::task::spawn_blocking(move || {
                                let _permit = permit;
                                prepare_file(&path)
                            })
                            .await
                            .map_err(|e| PreparationError::Failed(e.to_string()))?
                        }
                        .await,
                    )
                },
                cx,
                move |this, result, cx| {
                    this.finish_attachment_preparation(
                        id,
                        result
                            .map_err(UiText::from)
                            .and_then(|prepared| prepared.map_err(PreparationError::into_text)),
                        cx,
                    )
                },
            );
        }
        cx.notify();
    }
}

fn preparation_slots() -> &'static Arc<tokio::sync::Semaphore> {
    static SLOTS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
}

type PreparedAttachment = (Arc<staging::StagedFile>, String, u64, Option<DraftPictures>);

fn prepare_file(path: &Path) -> Result<PreparedAttachment, PreparationError> {
    use std::io::Read;
    let (staged, size) = staging::copy_file(&staging::root(), path)?;
    let mut signature = [0; 16];
    let mut file = std::fs::File::open(&staged.path).map_err(|e| e.to_string())?;
    let read = file.read(&mut signature).map_err(|e| e.to_string())?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if image_format_from_bytes(&signature[..read]).is_some() {
        if size > MAX_DRAFT_IMAGE_BYTES {
            return Err(PreparationError::ImageTooLarge);
        }
        let bytes = std::fs::read(&staged.path).map_err(|e| e.to_string())?;
        let mime = draft_image_mime(&name, &bytes)?.to_string();
        Ok((staged, mime, size, draft_pictures(&bytes).ok()))
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

pub(super) fn cached_attachment_detail(
    mime: &str,
    size: u64,
    messages: &MessageCache,
) -> SharedString {
    messages.computed("chat-attachment-detail", &format!("{mime}:{size}"), || {
        attachment_detail(mime, size).into()
    })
}

pub(super) fn attachment_detail(mime: &str, size: u64) -> String {
    let kind = match mime {
        "application/zip" => "ZIP".into(),
        "application/pdf" => "PDF".into(),
        "application/octet-stream" => tr("chat-attachment-file"),
        mime if mime.starts_with("image/") => tr("chat-attachment-image"),
        mime if mime.starts_with("text/") => tr("chat-attachment-text"),
        _ => tr("chat-attachment-file"),
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

fn draft_image_mime(name: &str, bytes: &[u8]) -> Result<&'static str, PreparationError> {
    if bytes.len() as u64 > MAX_DRAFT_IMAGE_BYTES {
        return Err(PreparationError::NamedImageTooLarge(name.to_string()));
    }
    match image_format_from_bytes(bytes) {
        Some(gpui::ImageFormat::Png) => Ok("image/png"),
        Some(gpui::ImageFormat::Jpeg) => Ok("image/jpeg"),
        Some(gpui::ImageFormat::Webp) => Ok("image/webp"),
        _ => Err(PreparationError::UnsupportedImage(name.to_string())),
    }
}

/// Thumbnail edge in physical pixels: 2x the 64pt box so it stays sharp
/// on HiDPI screens.
const DRAFT_THUMBNAIL_PX: u32 = 128;

/// Longest edge of the whole picture in physical pixels: 2x the 72pt
/// thumbnail a sent message shows.
const MESSAGE_PICTURE_PX: u32 = 144;

/// The composer's chip and the message's thumbnail, from one decode.
///
/// The chip is the picture center-cropped to a square and scaled down, so
/// the composer shows a rounded square that is the picture itself. The
/// message thumbnail keeps the picture's shape, as the sent message does.
fn draft_pictures(bytes: &[u8]) -> Result<DraftPictures, String> {
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
    let filter = image::imageops::FilterType::Triangle;
    let cropped = decoded.crop_imm((width - edge) / 2, (height - edge) / 2, edge, edge);
    let square = if edge > DRAFT_THUMBNAIL_PX {
        cropped.resize_exact(DRAFT_THUMBNAIL_PX, DRAFT_THUMBNAIL_PX, filter)
    } else {
        cropped
    };
    // `resize` keeps the shape and would also enlarge a small picture.
    let whole = if width.max(height) > MESSAGE_PICTURE_PX {
        decoded.resize(MESSAGE_PICTURE_PX, MESSAGE_PICTURE_PX, filter)
    } else {
        decoded
    };
    let png = |picture: image::DynamicImage| {
        let mut png = std::io::Cursor::new(Vec::new());
        picture
            .write_to(&mut png, image::ImageFormat::Png)
            .map_err(|error| error.to_string())?;
        Ok::<_, String>(Arc::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            png.into_inner(),
        )))
    };
    Ok(DraftPictures {
        square: png(square)?,
        whole: png(whole)?,
    })
}

/// Image format from the file signature; `None` for unsupported data.
pub(super) fn image_format_from_bytes(bytes: &[u8]) -> Option<gpui::ImageFormat> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some(gpui::ImageFormat::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(gpui::ImageFormat::Jpeg)
    } else if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(gpui::ImageFormat::Webp)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(gpui::ImageFormat::Gif)
    } else {
        None
    }
}

const MAX_PICTURE_EDGE: u32 = 16_384;
// GPUI decodes every frame of a GIF at full size and keeps all of them while
// the picture is cached, so a GIF is admitted by its decoded size: at most
// 300 frames and 32 Mi frame pixels (128 MiB of RGBA) in all.
pub(super) const MAX_GIF_FRAMES: usize = 300;
pub(super) const MAX_GIF_FRAME_PIXELS: u64 = 32 * 1024 * 1024;

/// What a loaded picture costs beyond its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PictureShape {
    /// Pixel size, when the header says it.
    pub(super) size: Option<(u32, u32)>,
    /// Pixels of every decoded frame of an animation; 0 for a still.
    pub(super) animation_pixels: u64,
}

/// Read a picture's size from its header, off the UI thread. A still that
/// GPUI can draw is never refused here; a GIF must fit its frame budget.
pub(super) fn picture_shape(bytes: &[u8]) -> Result<PictureShape, String> {
    if image_format_from_bytes(bytes) == Some(gpui::ImageFormat::Gif) {
        let (width, height, animation_pixels) = gif_shape(bytes)?;
        return Ok(PictureShape {
            size: Some((width, height)),
            animation_pixels,
        });
    }
    let size = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.into_dimensions().ok())
        .filter(|(width, height)| *width > 0 && *height > 0);
    Ok(PictureShape {
        size,
        animation_pixels: 0,
    })
}

/// Walk a GIF's blocks without decoding: its canvas size and the pixels of
/// all its frames, each of which GPUI composes at canvas size.
fn gif_shape(bytes: &[u8]) -> Result<(u32, u32, u64), String> {
    fn error() -> String {
        "The GIF is invalid or too large to play".into()
    }
    fn word(bytes: &[u8], offset: usize) -> Option<u32> {
        Some(u32::from(u16::from_le_bytes([
            *bytes.get(offset)?,
            *bytes.get(offset + 1)?,
        ])))
    }
    fn color_table(packed: u8) -> usize {
        if packed & 0x80 == 0 {
            0
        } else {
            3 * (1usize << ((packed & 7) + 1))
        }
    }
    fn sub_blocks(bytes: &[u8], offset: &mut usize) -> Result<(), String> {
        loop {
            let size = usize::from(*bytes.get(*offset).ok_or_else(error)?);
            *offset += 1 + size;
            if *offset > bytes.len() {
                return Err(error());
            }
            if size == 0 {
                return Ok(());
            }
        }
    }
    let width = word(bytes, 6).ok_or_else(error)?;
    let height = word(bytes, 8).ok_or_else(error)?;
    let packed = *bytes.get(10).ok_or_else(error)?;
    if width == 0 || height == 0 || width > MAX_PICTURE_EDGE || height > MAX_PICTURE_EDGE {
        return Err(error());
    }
    let canvas = u64::from(width) * u64::from(height);
    let mut offset = 13 + color_table(packed);
    let (mut frames, mut pixels) = (0usize, 0u64);
    loop {
        match *bytes.get(offset).ok_or_else(error)? {
            0x3b if frames > 0 => return Ok((width, height, pixels)),
            0x21 => {
                offset += 2;
                sub_blocks(bytes, &mut offset)?;
            }
            0x2c => {
                let packed = *bytes.get(offset + 9).ok_or_else(error)?;
                // Descriptor, local colors, then the LZW code size byte.
                offset += 10 + color_table(packed) + 1;
                if offset > bytes.len() {
                    return Err(error());
                }
                sub_blocks(bytes, &mut offset)?;
                frames += 1;
                pixels += canvas;
                if frames > MAX_GIF_FRAMES || pixels > MAX_GIF_FRAME_PIXELS {
                    return Err(error());
                }
            }
            _ => return Err(error()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gif_with_frames(width: u16, height: u16, frames: usize) -> Vec<u8> {
        let (width, height) = (width.to_le_bytes(), height.to_le_bytes());
        let mut bytes = vec![
            b'G', b'I', b'F', b'8', b'9', b'a', width[0], width[1], height[0], height[1], 0x80, 0,
            0, 0, 0, 0, 0xff, 0xff, 0xff,
        ];
        for _ in 0..frames {
            // A 1x1 frame at the origin with one LZW sub-block.
            bytes.extend_from_slice(&[0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x44, 1, 0]);
        }
        bytes.push(0x3b);
        bytes
    }

    #[test]
    fn gifs_are_admitted_by_structure_and_decoded_size() {
        let gif = gif_with_frames(4, 2, 3);
        assert_eq!(image_format_from_bytes(&gif), Some(gpui::ImageFormat::Gif));
        assert_eq!(
            picture_shape(&gif),
            Ok(PictureShape {
                size: Some((4, 2)),
                animation_pixels: 24,
            })
        );
        assert!(picture_shape(&gif_with_frames(1, 1, 0)).is_err());
        assert!(picture_shape(&gif_with_frames(1, 1, MAX_GIF_FRAMES + 1)).is_err());
        // 3 frames of 4096x4096 is 48 Mi pixels, over the budget.
        assert!(picture_shape(&gif_with_frames(4096, 4096, 3)).is_err());
        assert!(picture_shape(&gif[..gif.len() - 2]).is_err());
        assert!(picture_shape(&gif[..12]).is_err());
    }

    #[test]
    fn large_stills_are_never_refused() {
        // A 24 MP camera photo shows today; reading its size must not stop it.
        // Encode a small JPEG and write the photo's size into its frame header.
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(8, 8)
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        let mut jpeg = jpeg.into_inner();
        let frame = jpeg
            .windows(2)
            .position(|marker| marker == [0xff, 0xc0])
            .unwrap();
        jpeg[frame + 5..frame + 7].copy_from_slice(&4000u16.to_be_bytes());
        jpeg[frame + 7..frame + 9].copy_from_slice(&6000u16.to_be_bytes());
        assert_eq!(
            picture_shape(&jpeg),
            Ok(PictureShape {
                size: Some((6000, 4000)),
                animation_pixels: 0,
            })
        );
        assert_eq!(
            picture_shape(b"\x89PNG broken"),
            Ok(PictureShape {
                size: None,
                animation_pixels: 0,
            })
        );
    }

    #[test]
    fn staged_failure_reformats_after_returning_from_worker() {
        use crate::ui::i18n::{TestLocalizationGuard, apply_preference};
        let _locale = TestLocalizationGuard::enable();
        let failure = std::thread::spawn(|| {
            PreparationError::from(staging::Failure::OpenAttachment(
                "OS detail /tmp/raw".into(),
            ))
        })
        .join()
        .unwrap();
        let notice = failure.into_text();
        for (locale, prefix) in [
            ("en", "Could not open the attachment:"),
            ("es", "No se pudo abrir el archivo adjunto:"),
            ("en", "Could not open the attachment:"),
        ] {
            apply_preference(Some(locale));
            let text = notice.resolve();
            assert!(text.starts_with(prefix));
            assert!(text.contains("OS detail /tmp/raw"));
        }
    }

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

    /// One decode gives the composer its square chip and the message its
    /// whole picture; neither is ever enlarged.
    #[test]
    fn draft_pictures_crop_the_chip_and_keep_the_shape_of_the_message_picture() {
        let encode = |width: u32, height: u32| {
            let mut bytes = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgba8(width, height)
                .write_to(&mut bytes, image::ImageFormat::Png)
                .unwrap();
            bytes.into_inner()
        };
        let size = |picture: &gpui::Image| {
            let decoded = image::load_from_memory(&picture.bytes).unwrap();
            (decoded.width(), decoded.height())
        };

        let wide = draft_pictures(&encode(1600, 400)).unwrap();
        assert_eq!(size(&wide.square), (128, 128));
        assert_eq!(size(&wide.whole), (144, 36));

        let tall = draft_pictures(&encode(300, 1200)).unwrap();
        assert_eq!(size(&tall.square), (128, 128));
        assert_eq!(size(&tall.whole), (36, 144));

        let small = draft_pictures(&encode(60, 40)).unwrap();
        assert_eq!(size(&small.square), (40, 40));
        assert_eq!(size(&small.whole), (60, 40));
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
        assert!(matches!(
            prepare_file(&original),
            Err(PreparationError::ImageTooLarge)
        ));
    }
}
