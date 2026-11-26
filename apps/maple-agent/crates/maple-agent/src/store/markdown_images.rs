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

use std::io::Read;

use pi_ai::Message;
use pi_coding_agent::{
    SessionMessage,
    session::{EntryKind, SessionEntry},
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::project_files::{ProjectRoot, local_candidates, open_confined};
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
                let Some(found) = self.task_project_root(account, task_uuid)? else {
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
}
