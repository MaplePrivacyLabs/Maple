use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::store::{AccountId, ImageUpload, Store};

pub(super) const MAX_AGENT_IMAGE_BYTES: usize = 10 * 1024 * 1024;
pub(super) const MAX_AGENT_IMAGES_PER_MESSAGE: usize = 10;
const MAX_STORED_IMAGE_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentImageUpload {
    pub name: String,
    pub data_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentAttachment {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub source: String,
    pub size: u64,
    pub execution_path: String,
}

/// The runtime's account-bound view of the host-owned file attachment store.
/// Every resolution checks the execution account and task.
#[derive(Clone)]
pub(super) struct AgentAttachmentStore {
    store: Arc<Store>,
    account: AccountId,
}

impl AgentAttachmentStore {
    pub(super) fn new(store: Arc<Store>, account: AccountId) -> Self {
        Self { store, account }
    }

    pub(super) fn store_uploads(
        &self,
        session_id: &str,
        uploads: &[AgentImageUpload],
    ) -> Result<Vec<AgentAttachment>, String> {
        if uploads.len() > MAX_AGENT_IMAGES_PER_MESSAGE {
            return Err(format!(
                "Attach at most {MAX_AGENT_IMAGES_PER_MESSAGE} images at a time"
            ));
        }
        // Keep ACP's image validation messages. The privileged store independently
        // validates the entire batch before writing files.
        uploads
            .iter()
            .map(|upload| {
                let (declared_mime, base64_data) = parse_data_url(&upload.data_url)?;
                if base64_data.len() > maximum_base64_chars(MAX_AGENT_IMAGE_BYTES) {
                    return Err("Image too large (max 10MB)".to_string());
                }
                let bytes = BASE64_STANDARD
                    .decode(base64_data)
                    .map_err(|_| "Image attachment is not valid base64".to_string())?;
                if bytes.len() > MAX_AGENT_IMAGE_BYTES {
                    return Err("Image too large (max 10MB)".to_string());
                }
                let detected = supported_image_mime(&bytes)?;
                if normalize_mime(declared_mime) != detected {
                    return Err("Image content does not match its declared type".to_string());
                }
                Ok(())
            })
            .collect::<Result<Vec<_>, String>>()?;
        let uploads = uploads
            .iter()
            .map(|upload| ImageUpload {
                name: upload.name.clone(),
                data_url: upload.data_url.clone(),
            })
            .collect::<Vec<_>>();
        let stored = self
            .store
            .store_image_uploads(self.account, session_id, &uploads)
            .map_err(|error| error.to_string())?;
        Ok(stored.into_iter().map(AgentAttachment::from).collect())
    }

    /// Validate the entire message before publishing it. Pinned references
    /// cannot expire while a message waits in the queue or is being edited.
    pub(super) fn resolve_and_pin(
        &self,
        session_id: &str,
        ids: &[String],
    ) -> Result<Vec<AgentAttachment>, String> {
        let stored = self
            .store
            .resolve_attachments(self.account, session_id, ids)
            .map_err(|error| error.to_string())?;
        self.store
            .pin_attachments(self.account, session_id, ids)
            .map_err(|error| error.to_string())?;
        Ok(stored.into_iter().map(AgentAttachment::from).collect())
    }

    pub(super) fn prepare_model_context(
        &self,
        session_id: &str,
        messages: &mut [pi_ai::Message],
    ) -> Result<(), String> {
        // Recovery can move the file-store generation. Resolve execution paths
        // from scoped IDs each request instead of trusting persisted locations.
        for message in messages.iter_mut() {
            let pi_ai::Message::User(user) = message else {
                continue;
            };
            for block in &mut user.content {
                let pi_ai::Content::Text(text) = block else {
                    continue;
                };
                if !has_attachment_metadata(&text.text) {
                    continue;
                }
                let Some((typed, mut attachments, suffix)) = attachment_prompt_parts(&text.text)
                else {
                    continue;
                };
                for attachment in &mut attachments {
                    *attachment = self
                        .store
                        .attachment_metadata(self.account, session_id, &attachment.id)
                        .map_err(|error| error.to_string())?
                        .into();
                }
                // Keep Pi's normalization notes (including coordinate scaling)
                // and the original read_image guidance exactly as selected.
                text.text = format!("{}{}", attachment_prompt(typed, &attachments, true), suffix);
            }
        }
        self.store
            .materialize_message_images(self.account, session_id, messages)
            .map_err(|error| error.to_string())
    }

    pub(super) fn read(&self, session_id: &str, attachment_id: &str) -> Result<Vec<u8>, String> {
        validate_attachment_id(attachment_id)?;
        let mut bytes = Vec::new();
        loop {
            // Every bounded range rechecks both the captured account and task.
            let range = self
                .store
                .read_attachment(
                    self.account,
                    session_id,
                    attachment_id,
                    bytes.len() as u64,
                    crate::store::MAX_ATTACHMENT_RANGE_BYTES,
                )
                .map_err(|error| error.to_string())?;
            if range.size > MAX_STORED_IMAGE_BYTES as u64 {
                return Err("Agent image attachment exceeds the 20MB limit".to_string());
            }
            if bytes.len().saturating_add(range.data.len()) as u64 > range.size {
                return Err("Agent image attachment is incomplete".to_string());
            }
            bytes.extend_from_slice(&range.data);
            if bytes.len() as u64 == range.size {
                break;
            }
            if range.data.is_empty() {
                return Err("Agent image attachment is incomplete".to_string());
            }
        }
        supported_image_mime(&bytes)?;
        Ok(bytes)
    }
}

#[cfg(test)]
pub(super) fn fixture_attachment_store(
    root: &std::path::Path,
    tasks: &[&str],
) -> AgentAttachmentStore {
    let store = Arc::new(Store::open(crate::store::StorePaths::new(root)).unwrap());
    let account = store.upsert_account("attachment-fixture", 1).unwrap();
    for uuid in tasks {
        store
            .create_task(
                account,
                &crate::store::NewTask {
                    uuid: (*uuid).to_string(),
                    project_root: root.to_string_lossy().into_owned(),
                    kind: crate::store::TaskKind::Desktop,
                    title: "Image fixture".to_string(),
                    provisional: false,
                    model: None,
                    created_ms: 1,
                },
            )
            .unwrap();
    }
    AgentAttachmentStore::new(store, account)
}

pub(super) fn attachment_id_from_source(source: &str) -> Option<&str> {
    source
        .strip_prefix("maple-attachment://")
        .and_then(|id| (!id.is_empty() && !id.contains(['/', '\\', '?', '#'])).then_some(id))
}

impl From<crate::store::StoredAttachment> for AgentAttachment {
    fn from(value: crate::store::StoredAttachment) -> Self {
        Self {
            id: value.id,
            name: value.name,
            mime_type: value.mime_type,
            source: value.source,
            size: value.size,
            execution_path: value.execution_path,
        }
    }
}

impl AgentAttachment {
    pub fn is_image(&self) -> bool {
        self.mime_type.starts_with("image/")
    }
}

const ATTACHED_FILES: &str = "The user attached these files on the execution computer:";
const END_ATTACHMENTS: &str = "End of attachment metadata.";
const READ_IMAGE_HINT: &str = "Use read_image with an attachment source or executionPath when you need visual details from an image.";

pub(super) fn has_attachment_metadata(text: &str) -> bool {
    text.contains(ATTACHED_FILES)
}

pub(super) fn prompt_has_inline_images(text: &str) -> bool {
    has_attachment_metadata(text) && !text.lines().any(|line| line == READ_IMAGE_HINT)
}

/// Names and paths are JSON-quoted metadata, never instructions. Files are
/// stable copies on the execution computer; archives are not extracted.
/// The same metadata round-trips through queues, steering and the timeline.
pub(super) fn attachment_prompt(
    text: &str,
    attachments: &[AgentAttachment],
    inline: bool,
) -> String {
    let mut prompt = text.trim().to_string();
    if attachments.is_empty() {
        return prompt;
    }
    if !prompt.is_empty() {
        prompt.push_str("\n\n");
    }
    prompt.push_str(ATTACHED_FILES);
    prompt.push('\n');
    for attachment in attachments {
        prompt.push_str("- ");
        // A strongly typed descriptor only contains strings and an integer.
        prompt
            .push_str(&serde_json::to_string(attachment).expect("attachment metadata serializes"));
        prompt.push('\n');
    }
    prompt.push_str(END_ATTACHMENTS);
    if !inline && attachments.iter().any(AgentAttachment::is_image) {
        prompt.push('\n');
        prompt.push_str(READ_IMAGE_HINT);
    }
    prompt
}

pub(super) fn split_attachment_prompt(prompt: &str) -> Option<(String, Vec<AgentAttachment>)> {
    if has_attachment_metadata(prompt) {
        return attachment_prompt_parts(prompt).map(|(typed, items, _)| (typed.to_owned(), items));
    }
    // Historical prompts retain their labels after the byte migration. This
    // only reads old text; it never reintroduces the retired image backend.
    split_retired_image_prompt(prompt)
}

fn attachment_prompt_parts(prompt: &str) -> Option<(&str, Vec<AgentAttachment>, &str)> {
    let start = if prompt.starts_with(ATTACHED_FILES) {
        0
    } else {
        prompt.rfind(&format!("\n\n{ATTACHED_FILES}"))? + 2
    };
    let (typed, listing) = prompt.split_at(start);
    let typed = match typed.strip_suffix("\n\n") {
        Some(typed) => typed,
        None if typed.is_empty() => "",
        None => return None,
    };
    let (listing, suffix) = listing.split_once(&format!("\n{END_ATTACHMENTS}"))?;
    let mut lines = listing.lines();
    if lines.next()? != ATTACHED_FILES {
        return None;
    }
    let mut attachments = Vec::new();
    for line in lines {
        let attachment: AgentAttachment = serde_json::from_str(line.strip_prefix("- ")?).ok()?;
        if attachment_id_from_source(&attachment.source)? != attachment.id {
            return None;
        }
        attachments.push(attachment);
        if attachments.len() > MAX_AGENT_IMAGES_PER_MESSAGE {
            return None;
        }
    }
    (!attachments.is_empty()).then_some((typed, attachments, suffix))
}

fn split_retired_image_prompt(prompt: &str) -> Option<(String, Vec<AgentAttachment>)> {
    const HEAD: &str = "The user attached the following image";
    const HINT: &str =
        "Use read_image with an attachment source when you need visual details from that image.";
    let start = prompt.rfind(HEAD)?;
    let (typed, listing) = prompt.split_at(start);
    let typed = match typed.strip_suffix("\n\n") {
        Some(typed) => typed,
        None if typed.is_empty() => "",
        None => return None,
    };
    let mut lines = listing.lines();
    let head = lines.next()?.strip_prefix(HEAD)?;
    if head != ":" && head != "s:" {
        return None;
    }
    let mut attachments = Vec::new();
    for line in lines {
        if line == HINT {
            break;
        }
        let item = line.strip_prefix("- ")?;
        let mut names = serde_json::Deserializer::from_str(item).into_iter::<String>();
        let name = names.next()?.ok()?;
        let source = item[names.byte_offset()..].strip_prefix(": ")?;
        let id = attachment_id_from_source(source)?;
        attachments.push(AgentAttachment {
            id: id.to_string(),
            name,
            mime_type: "image/unknown".into(),
            source: source.to_string(),
            size: 0,
            execution_path: String::new(),
        });
    }
    (!attachments.is_empty()).then(|| (typed.to_string(), attachments))
}

fn parse_data_url(data_url: &str) -> Result<(&str, &str), String> {
    let (header, data) = data_url
        .split_once(',')
        .ok_or_else(|| "Image attachment must be a base64 data URL".to_string())?;
    let mime = header
        .strip_prefix("data:")
        .and_then(|value| value.strip_suffix(";base64"))
        .ok_or_else(|| "Image attachment must be a base64 data URL".to_string())?;
    if data.is_empty() {
        return Err("Image attachment cannot be empty".to_string());
    }
    Ok((mime, data))
}

fn maximum_base64_chars(bytes: usize) -> usize {
    bytes.div_ceil(3).saturating_mul(4)
}

fn normalize_mime(mime: &str) -> &str {
    if mime.eq_ignore_ascii_case("image/jpg") {
        "image/jpeg"
    } else {
        mime
    }
}

fn supported_image_mime(bytes: &[u8]) -> Result<&'static str, String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Ok("image/jpeg")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Ok("image/webp")
    } else {
        Err("Only JPEG, PNG, and WebP images are supported".to_string())
    }
}

fn validate_attachment_id(id: &str) -> Result<(), String> {
    if id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("Invalid Agent image attachment ID".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const TASK_A: &str = "018ddbcc-b80b-7c20-8123-000000000001";
    const TASK_B: &str = "018ddbcc-b80b-7c20-8123-000000000002";
    const TASK_C: &str = "018ddbcc-b80b-7c20-8123-000000000003";

    const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    #[test]
    fn stores_reads_and_deletes_session_scoped_images() {
        let temp = tempdir().unwrap();
        let store = fixture_attachment_store(temp.path(), &[TASK_A, TASK_B]);
        let prepared = store
            .store_uploads(
                TASK_A,
                &[AgentImageUpload {
                    name: "../screen.png".to_string(),
                    data_url: format!("data:image/png;base64,{PNG_1X1}"),
                }],
            )
            .unwrap();

        assert_eq!(prepared[0].name, "screen.png");
        assert_eq!(
            store.read(TASK_A, &prepared[0].id).unwrap(),
            BASE64_STANDARD.decode(PNG_1X1).unwrap()
        );
        assert!(store.read(TASK_B, &prepared[0].id).is_err());

        assert!(!temp.path().join("attachments").exists());
        store
            .store
            .mark_task_deleted(store.account, TASK_A, 2)
            .unwrap();
        assert!(store.read(TASK_A, &prepared[0].id).is_err());
        let mut collected = false;
        for _ in 0..100 {
            if store
                .store
                .collect_task_step(store.account, TASK_A)
                .unwrap()
                .done
            {
                collected = true;
                break;
            }
        }
        assert!(collected);
        assert!(store.store.task(store.account, TASK_A).unwrap().is_none());
    }

    #[test]
    fn rejects_mime_mismatches_and_unsupported_images() {
        let temp = tempdir().unwrap();
        let store = fixture_attachment_store(temp.path(), &[TASK_A]);

        let mismatch = store.store_uploads(
            TASK_A,
            &[AgentImageUpload {
                name: "screen.jpg".to_string(),
                data_url: format!("data:image/jpeg;base64,{PNG_1X1}"),
            }],
        );
        assert_eq!(
            mismatch.unwrap_err(),
            "Image content does not match its declared type"
        );

        let unsupported = store.store_uploads(
            TASK_A,
            &[AgentImageUpload {
                name: "screen.gif".to_string(),
                data_url: format!(
                    "data:image/gif;base64,{}",
                    BASE64_STANDARD.encode(b"GIF89a")
                ),
            }],
        );
        assert_eq!(
            unsupported.unwrap_err(),
            "Only JPEG, PNG, and WebP images are supported"
        );
    }

    #[test]
    fn generated_images_above_the_upload_cap_remain_readable() {
        use pi_coding_agent::SessionMessage;
        let temp = tempdir().unwrap();
        let store = fixture_attachment_store(temp.path(), &[TASK_A]);
        let mut pixels = BASE64_STANDARD.decode(PNG_1X1).unwrap();
        pixels.resize(MAX_AGENT_IMAGE_BYTES + 1, 0);
        let encoded = BASE64_STANDARD.encode(&pixels);
        assert!(
            store
                .store_uploads(
                    TASK_A,
                    &[AgentImageUpload {
                        name: "too-large-for-upload.png".into(),
                        data_url: format!("data:image/png;base64,{encoded}"),
                    }]
                )
                .is_err()
        );
        let mut manager = store
            .store
            .load_session(store.account, TASK_A, crate::store::SessionLoad::Full)
            .unwrap()
            .into_manager("/project");
        manager.append_message(SessionMessage::Llm(pi_ai::Message::User(
            pi_ai::UserMessage {
                content: vec![pi_ai::Content::image(encoded, "image/png")],
                timestamp: 1,
            },
        )));
        manager.flush().unwrap();
        let saved = store
            .store
            .load_session(store.account, TASK_A, crate::store::SessionLoad::Full)
            .unwrap();
        let source = saved
            .entries
            .iter()
            .find_map(|entry| match &entry.kind {
                pi_coding_agent::session::EntryKind::Message {
                    message: SessionMessage::Llm(pi_ai::Message::User(user)),
                } => user.content.iter().find_map(|block| match block {
                    pi_ai::Content::Image(image) => Some(image.data.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .unwrap();
        let id =
            attachment_id_from_source(&source).expect("generated image stored as a file reference");
        assert_eq!(store.read(TASK_A, id).unwrap(), pixels);
    }

    #[test]
    fn attachment_adapter_captures_account_and_never_falls_back_to_files() {
        let temp = tempdir().unwrap();
        let store = fixture_attachment_store(temp.path(), &[TASK_A]);
        let prepared = store
            .store_uploads(
                TASK_A,
                &[AgentImageUpload {
                    name: "image.png".to_string(),
                    data_url: format!("data:image/png;base64,{PNG_1X1}"),
                }],
            )
            .unwrap();
        let other = store
            .store
            .upsert_account("other-attachment-fixture", 1)
            .unwrap();
        store
            .store
            .create_task(
                other,
                &crate::store::NewTask {
                    uuid: TASK_C.to_string(),
                    project_root: temp.path().to_string_lossy().into_owned(),
                    kind: crate::store::TaskKind::Desktop,
                    title: "Other account".to_string(),
                    provisional: false,
                    model: None,
                    created_ms: 1,
                },
            )
            .unwrap();
        let other = AgentAttachmentStore::new(Arc::clone(&store.store), other);
        assert!(other.read(TASK_A, &prepared[0].id).is_err());
        assert!(other.read(TASK_C, &prepared[0].id).is_err());

        // A previous-format fixture may remain on disk. An unknown SQL id
        // cannot select it, and the adapter never creates or removes its files.
        let old = temp.path().join("attachments");
        std::fs::create_dir(&old).unwrap();
        let id = "f".repeat(32);
        let untouched = old.join(&id);
        let bytes = BASE64_STANDARD.decode(PNG_1X1).unwrap();
        std::fs::write(&untouched, &bytes).unwrap();
        assert!(store.read(TASK_A, &id).is_err());
        assert_eq!(std::fs::read(untouched).unwrap(), bytes);
    }

    #[test]
    fn committed_attachment_is_read_after_reopening_the_new_store() {
        let temp = tempdir().unwrap();
        let store = fixture_attachment_store(temp.path(), &[TASK_A]);
        let prepared = store
            .store_uploads(
                TASK_A,
                &[AgentImageUpload {
                    name: "image.png".to_string(),
                    data_url: format!("data:image/png;base64,{PNG_1X1}"),
                }],
            )
            .unwrap();
        let id = prepared[0].id.clone();
        drop(store);
        let reopened = Arc::new(Store::open(crate::store::StorePaths::new(temp.path())).unwrap());
        let account = reopened.account_id("attachment-fixture").unwrap().unwrap();
        let store = AgentAttachmentStore::new(reopened, account);
        assert_eq!(
            store.read(TASK_A, &id).unwrap(),
            BASE64_STANDARD.decode(PNG_1X1).unwrap()
        );
    }

    fn attachment(id: char, name: &str, mime: &str) -> AgentAttachment {
        let id = id.to_string().repeat(32);
        AgentAttachment {
            source: format!("maple-attachment://{id}"),
            id,
            name: name.into(),
            mime_type: mime.into(),
            size: 42,
            execution_path: format!("/uploads/session/{name}"),
        }
    }

    #[test]
    fn mixed_attachment_metadata_round_trips_and_quotes_paths() {
        let items = [
            attachment('a', "screen.png", "image/png"),
            attachment('b', "report \"2026\"\n.zip", "application/zip"),
        ];
        let prompt = attachment_prompt(" inspect these ", &items, false);
        assert!(prompt.contains("executionPath"));
        assert!(prompt.contains(READ_IMAGE_HINT));
        assert_eq!(
            split_attachment_prompt(&prompt),
            Some(("inspect these".into(), items.to_vec()))
        );
        let inline = attachment_prompt("", &items, true);
        assert!(!inline.contains(READ_IMAGE_HINT));
        assert_eq!(
            split_attachment_prompt(&inline),
            Some((String::new(), items.to_vec()))
        );
        let files = attachment_prompt("", &items[1..], false);
        assert!(!files.contains("read_image"));
        assert_eq!(attachment_prompt(" plain ", &[], false), "plain");
        assert!(split_attachment_prompt("plain").is_none());
    }

    #[test]
    fn selected_metadata_refreshes_execution_paths_without_reading_pixels() {
        let temp = tempdir().unwrap();
        let store = fixture_attachment_store(temp.path(), &[TASK_A]);
        let stored = store
            .store_uploads(
                TASK_A,
                &[AgentImageUpload {
                    name: "screen.png".into(),
                    data_url: format!("data:image/png;base64,{PNG_1X1}"),
                }],
            )
            .unwrap()
            .remove(0);
        let mut old = stored.clone();
        old.execution_path = "/an/old/store-generation/screen.png".into();
        let original = format!(
            "{}\n[Image coordinates scaled by Pi.]",
            attachment_prompt("look", &[old], false)
        );
        let mut messages = [pi_ai::Message::User(pi_ai::UserMessage {
            content: vec![pi_ai::Content::text(original.clone())],
            timestamp: 1,
        })];
        std::fs::remove_file(&stored.execution_path).unwrap();
        store.prepare_model_context(TASK_A, &mut messages).unwrap();
        let pi_ai::Message::User(user) = &messages[0] else {
            panic!("user")
        };
        let (text, metadata) =
            split_attachment_prompt(&pi_ai::content_text(&user.content)).unwrap();
        assert_eq!(text, "look");
        assert_eq!(metadata, [stored]);
        assert!(pi_ai::content_text(&user.content).ends_with("[Image coordinates scaled by Pi.]"));
        assert!(pi_ai::content_text(&user.content).contains(READ_IMAGE_HINT));
        assert!(original.contains("/an/old/store-generation/"));
    }

    #[test]
    fn retired_images_keep_labels_but_no_payload_or_path() {
        let prompt = format!(
            "hello\n\nThe user attached the following image:\n- \"screen.png\": maple-attachment://{}\n",
            "a".repeat(32)
        );
        let (text, items) = split_attachment_prompt(&prompt).unwrap();
        assert_eq!(text, "hello");
        assert_eq!(items[0].name, "screen.png");
        assert!(items[0].is_image());
        assert!(items[0].execution_path.is_empty());
    }
}
