use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub(super) const MAX_AGENT_IMAGE_BYTES: usize = 10 * 1024 * 1024;
pub(super) const MAX_AGENT_IMAGES_PER_MESSAGE: usize = 10;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentImageUpload {
    pub name: String,
    pub data_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentImageAttachment {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub source: String,
}

#[derive(Debug, Clone)]
pub(super) struct PreparedAgentImage {
    pub attachment: AgentImageAttachment,
    pub base64_data: String,
}

#[derive(Debug, Clone)]
pub(super) struct AgentAttachmentStore {
    root: PathBuf,
}

impl AgentAttachmentStore {
    pub(super) fn new(account_local_data_dir: PathBuf) -> Self {
        Self {
            root: account_local_data_dir.join("attachments"),
        }
    }

    pub(super) fn store_uploads(
        &self,
        session_id: &str,
        uploads: &[AgentImageUpload],
    ) -> Result<Vec<PreparedAgentImage>, String> {
        if uploads.len() > MAX_AGENT_IMAGES_PER_MESSAGE {
            return Err(format!(
                "Attach at most {MAX_AGENT_IMAGES_PER_MESSAGE} images at a time"
            ));
        }
        if uploads.is_empty() {
            return Ok(Vec::new());
        }

        let session_dir = self.session_dir(session_id)?;
        if let Some(account_dir) = self.root.parent() {
            create_owner_only_dir(account_dir)?;
        }
        create_owner_only_dir(&self.root)?;
        create_owner_only_dir(&session_dir)?;

        let mut prepared = Vec::with_capacity(uploads.len());
        let mut created_paths = Vec::with_capacity(uploads.len());
        for upload in uploads {
            let result = self.store_upload(&session_dir, upload);
            match result {
                Ok((image, path)) => {
                    created_paths.push(path);
                    prepared.push(image);
                }
                Err(error) => {
                    for path in created_paths {
                        let _ = fs::remove_file(path);
                    }
                    return Err(error);
                }
            }
        }
        Ok(prepared)
    }

    pub(super) fn read(&self, session_id: &str, attachment_id: &str) -> Result<Vec<u8>, String> {
        validate_attachment_id(attachment_id)?;
        let path = self.session_dir(session_id)?.join(attachment_id);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("Failed to find Agent image attachment: {error}"))?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err("Agent image attachment is not a regular file".to_string());
        }
        if metadata.len() > MAX_AGENT_IMAGE_BYTES as u64 {
            return Err("Agent image attachment exceeds the 10MB limit".to_string());
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(&path)
            .map_err(|error| format!("Failed to read Agent image attachment: {error}"))?;
        if !file
            .metadata()
            .map_err(|error| format!("Failed to inspect Agent image attachment: {error}"))?
            .is_file()
        {
            return Err("Agent image attachment is not a regular file".to_string());
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_AGENT_IMAGE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("Failed to read Agent image attachment: {error}"))?;
        if bytes.len() > MAX_AGENT_IMAGE_BYTES {
            return Err("Agent image attachment exceeds the 10MB limit".to_string());
        }
        supported_image_mime(&bytes)?;
        Ok(bytes)
    }

    pub(super) fn delete_session(&self, session_id: &str) -> Result<(), String> {
        let path = self.session_dir(session_id)?;
        remove_exact_path(&path)
            .map_err(|error| format!("Failed to remove Agent image attachments: {error}"))
    }

    fn store_upload(
        &self,
        session_dir: &Path,
        upload: &AgentImageUpload,
    ) -> Result<(PreparedAgentImage, PathBuf), String> {
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
        let detected_mime = supported_image_mime(&bytes)?;
        if normalize_mime(declared_mime) != detected_mime {
            return Err("Image content does not match its declared type".to_string());
        }

        let id = format!("{:032x}", rand::random::<u128>());
        let path = session_dir.join(&id);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|error| format!("Failed to store Agent image attachment: {error}"))?;
        if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
            let _ = fs::remove_file(&path);
            return Err(format!("Failed to store Agent image attachment: {error}"));
        }

        let attachment = AgentImageAttachment {
            source: format!("maple-attachment://{id}"),
            id,
            name: safe_display_name(&upload.name),
            mime_type: detected_mime.to_string(),
        };
        Ok((
            PreparedAgentImage {
                attachment,
                base64_data: base64_data.to_string(),
            },
            path,
        ))
    }

    fn session_dir(&self, session_id: &str) -> Result<PathBuf, String> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Err("Agent task ID cannot be empty".to_string());
        }
        let digest = Sha256::digest(session_id.as_bytes());
        Ok(self.root.join(format!("{digest:x}")))
    }
}

pub(super) fn attachment_id_from_source(source: &str) -> Option<&str> {
    source
        .strip_prefix("maple-attachment://")
        .and_then(|id| (!id.is_empty() && !id.contains(['/', '\\', '?', '#'])).then_some(id))
}

const ATTACHED_IMAGE: &str = "The user attached the following image";
const READ_IMAGE_HINT: &str =
    "Use read_image with an attachment source when you need visual details from that image.";

/// The text a model gets for a message with images: what the user typed,
/// then each image's name and source, as Goose's runtime wrote it. A model
/// that gets the images beside the text (`inline`) is not told to look with
/// `read_image`; the source still lets it crop one.
pub(super) fn image_prompt(
    text: &str,
    attachments: &[AgentImageAttachment],
    inline: bool,
) -> String {
    let mut prompt = text.trim().to_string();
    if attachments.is_empty() {
        return prompt;
    }
    if !prompt.is_empty() {
        prompt.push_str("\n\n");
    }
    prompt.push_str(ATTACHED_IMAGE);
    prompt.push_str(if attachments.len() == 1 {
        ":\n"
    } else {
        "s:\n"
    });
    for attachment in attachments {
        let name = serde_json::to_string(&attachment.name).unwrap_or_else(|_| "\"image\"".into());
        prompt.push_str(&format!("- {name}: {}\n", attachment.source));
    }
    if !inline {
        prompt.push_str(READ_IMAGE_HINT);
    }
    prompt
}

/// What the user typed and which images they attached, read back from the
/// text [`image_prompt`] wrote. `None` for a message without images. An
/// attachment read back has no media type.
pub(super) fn split_image_prompt(prompt: &str) -> Option<(String, Vec<AgentImageAttachment>)> {
    let start = prompt.rfind(ATTACHED_IMAGE)?;
    let (typed, listing) = prompt.split_at(start);
    let typed = match typed.strip_suffix("\n\n") {
        Some(typed) => typed,
        None if typed.is_empty() => "",
        None => return None,
    };
    let mut lines = listing.lines();
    let head = lines.next()?.strip_prefix(ATTACHED_IMAGE)?;
    if head != ":" && head != "s:" {
        return None;
    }
    let mut attachments = Vec::new();
    for line in lines {
        if line == READ_IMAGE_HINT {
            break;
        }
        let item = line.strip_prefix("- ")?;
        let mut names = serde_json::Deserializer::from_str(item).into_iter::<String>();
        let name = names.next()?.ok()?;
        let source = item[names.byte_offset()..].strip_prefix(": ")?;
        let id = attachment_id_from_source(source)?;
        attachments.push(AgentImageAttachment {
            id: id.to_string(),
            name,
            mime_type: String::new(),
            source: source.to_string(),
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

fn safe_display_name(name: &str) -> String {
    let name = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(255)
        .collect::<String>();
    if name.is_empty() {
        "image".to_string()
    } else {
        name
    }
}

fn validate_attachment_id(id: &str) -> Result<(), String> {
    if id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("Invalid Agent image attachment ID".to_string())
    }
}

fn create_owner_only_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("Failed to create Agent attachment cache: {error}"))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Failed to inspect Agent attachment cache: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("Agent attachment cache is not a regular directory".to_string());
    }
    crate::private_file::set_owner_only_dir(path)
        .map_err(|error| format!("Failed to secure Agent attachment cache: {error}"))?;
    Ok(())
}

fn remove_exact_path(path: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || metadata.is_file() {
        fs::remove_file(path)
    } else if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        Err(std::io::Error::other("unsupported attachment cache entry"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    #[test]
    fn stores_reads_and_deletes_session_scoped_images() {
        let temp = tempdir().unwrap();
        let store = AgentAttachmentStore::new(temp.path().to_path_buf());
        let prepared = store
            .store_uploads(
                "session-a",
                &[AgentImageUpload {
                    name: "../screen.png".to_string(),
                    data_url: format!("data:image/png;base64,{PNG_1X1}"),
                }],
            )
            .unwrap();

        assert_eq!(prepared[0].attachment.name, "screen.png");
        assert_eq!(
            store.read("session-a", &prepared[0].attachment.id).unwrap(),
            BASE64_STANDARD.decode(PNG_1X1).unwrap()
        );
        assert!(store.read("session-b", &prepared[0].attachment.id).is_err());

        store.delete_session("session-a").unwrap();
        assert!(store.read("session-a", &prepared[0].attachment.id).is_err());
    }

    #[test]
    fn rejects_mime_mismatches_and_unsupported_images() {
        let temp = tempdir().unwrap();
        let store = AgentAttachmentStore::new(temp.path().to_path_buf());

        let mismatch = store.store_uploads(
            "session-a",
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
            "session-a",
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

    fn attachment(id: char, name: &str) -> AgentImageAttachment {
        let id = id.to_string().repeat(32);
        AgentImageAttachment {
            source: format!("maple-attachment://{id}"),
            id,
            name: name.to_string(),
            mime_type: String::new(),
        }
    }

    #[test]
    fn an_image_prompt_names_the_images_and_reads_back() {
        let one = [attachment('a', "dialog.png")];
        let prompt = image_prompt(" Why is this misaligned? ", &one, false);
        assert_eq!(
            prompt,
            format!(
                "Why is this misaligned?\n\nThe user attached the following image:\n- \"dialog.png\": maple-attachment://{}\n{READ_IMAGE_HINT}",
                "a".repeat(32)
            )
        );
        assert_eq!(
            split_image_prompt(&prompt),
            Some(("Why is this misaligned?".to_string(), one.to_vec()))
        );

        // With the images beside the text, nothing asks for read_image; a
        // name may hold anything, and the text may be empty.
        let two = [attachment('b', "a \"b\"\nc.png"), attachment('c', "d.png")];
        let prompt = image_prompt("", &two, true);
        assert!(prompt.starts_with("The user attached the following images:\n"));
        assert!(!prompt.contains("read_image"));
        assert_eq!(
            split_image_prompt(&prompt),
            Some((String::new(), two.to_vec()))
        );

        // Text without images is left as it is.
        assert_eq!(image_prompt(" plain ", &[], false), "plain");
        for text in [
            "plain",
            "The user attached the following image: nothing",
            "x\nThe user attached the following image:\n- \"a\": maple-attachment://1",
        ] {
            assert_eq!(split_image_prompt(text), None, "{text}");
        }
    }
}
