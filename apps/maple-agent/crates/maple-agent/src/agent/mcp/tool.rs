//! An MCP server's tools as Pi tools, by the conventions of Pi's MCP support:
//! tools named `mcp__<server>__<tool>`, the server's own description and
//! input schema, and results as text and images, long text cut in the middle
//! with the full text saved to a file the model can read.

use base64::Engine as _;
use pi_ai::Content;
use pi_coding_agent::tools::{format_size, truncate_middle, write_output_file};
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, ResourceContents};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Providers take tool names of at most 64 characters of `[A-Za-z0-9_-]`.
const MAX_TOOL_NAME_CHARS: usize = 64;

/// Model-facing text of a result beyond this is cut in the middle.
pub(super) const MCP_OUTPUT_MAX_BYTES: usize = 20 * 1024;

/// The start of an output file's name.
const OUTPUT_FILE_PREFIX: &str = "maple-mcp";

/// `mcp__<server>__<tool>` with everything but `[A-Za-z0-9_]` made `_`, as
/// Pi and Codex name MCP tools. A name too long, or `taken` by another tool
/// once cleaned (`a-b` and `a_b`), is shortened and given a hash of the two.
pub(super) fn tool_name(server: &str, tool: &str, taken: impl Fn(&str) -> bool) -> String {
    let name: String = format!("mcp__{server}__{tool}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.len() <= MAX_TOOL_NAME_CHARS && !taken(&name) {
        return name;
    }
    let digest = Sha256::digest(format!("{server}\0{tool}").as_bytes());
    let hash: String = digest
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let keep = MAX_TOOL_NAME_CHARS - hash.len() - 1;
    format!("{}_{hash}", &name[..name.len().min(keep)])
}

/// A tool's input schema as Pi's tools take it: an object, with
/// `properties`, which some providers require.
pub(super) fn parameters(schema: &JsonObject) -> Value {
    let mut schema = schema.clone();
    schema
        .entry("type")
        .or_insert_with(|| Value::String("object".into()));
    schema
        .entry("properties")
        .or_insert_with(|| Value::Object(Default::default()));
    Value::Object(schema)
}

/// What the model is told a tool does: the server's description, else its
/// title, else where it comes from.
pub(super) fn description(server: &str, tool: &rmcp::model::Tool) -> String {
    tool.description
        .as_deref()
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .map(str::to_string)
        .or_else(|| tool.title.clone().filter(|title| !title.trim().is_empty()))
        .unwrap_or_else(|| format!("MCP tool {} from server {server}", tool.name))
}

/// The text of a blob a person can read.
fn is_text_mime_type(mime_type: Option<&str>) -> bool {
    let Some(mime_type) = mime_type else {
        return false;
    };
    let kind = mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    kind.starts_with("text/")
        || kind == "application/json"
        || kind.ends_with("+json")
        || kind.ends_with("+xml")
}

/// The extension a saved resource gets: the one its URI ends in, else `.bin`.
fn extension_of(uri: &str) -> String {
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, extension))
            if !stem.is_empty()
                && (1..=8).contains(&extension.len())
                && extension.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            format!(".{extension}")
        }
        _ => ".bin".to_string(),
    }
}

/// What the model gets of one content block.
fn block_content(block: &ContentBlock) -> Content {
    match block {
        ContentBlock::Text(text) => Content::text(text.text.clone()),
        ContentBlock::Image(image) => Content::image(image.data.clone(), image.mime_type.clone()),
        ContentBlock::Audio(audio) => Content::text(format!("[audio {} omitted]", audio.mime_type)),
        ContentBlock::ResourceLink(link) => {
            let mut details = Vec::new();
            details.extend(link.mime_type.clone());
            details.extend(link.size.map(|size| format_size(size as usize)));
            let details = if details.is_empty() {
                String::new()
            } else {
                format!(" ({})", details.join(", "))
            };
            let description = link
                .description
                .as_deref()
                .map(|description| format!(": {description}"))
                .unwrap_or_default();
            Content::text(format!(
                "[Resource {} \"{}\"{details}{description}]",
                link.uri,
                link.title.as_deref().unwrap_or(&link.name)
            ))
        }
        ContentBlock::Resource(embedded) => match &embedded.resource {
            ResourceContents::TextResourceContents { text, .. } => Content::text(text.clone()),
            ResourceContents::BlobResourceContents {
                mime_type, blob, ..
            } if mime_type
                .as_deref()
                .is_some_and(|mime_type| mime_type.starts_with("image/")) =>
            {
                Content::image(blob.clone(), mime_type.clone().unwrap_or_default())
            }
            ResourceContents::BlobResourceContents {
                uri,
                mime_type,
                blob,
                ..
            } => binary_resource(uri, mime_type.as_deref(), blob),
            _ => Content::text("[unsupported MCP resource omitted]"),
        },
        _ => Content::text("[unsupported MCP content omitted]"),
    }
}

/// A blob that is not an image: its text when it is text, else a file the
/// model can open.
fn binary_resource(uri: &str, mime_type: Option<&str>, blob: &str) -> Content {
    let data = match base64::engine::general_purpose::STANDARD.decode(blob) {
        Ok(data) => data,
        Err(_) => return Content::text(format!("[Binary resource {uri} could not be decoded]")),
    };
    if is_text_mime_type(mime_type) {
        return Content::text(String::from_utf8_lossy(&data).into_owned());
    }
    let kind = format!(
        "{}, {}",
        mime_type.unwrap_or("unknown type"),
        format_size(data.len())
    );
    match write_output_file(OUTPUT_FILE_PREFIX, &extension_of(uri), &data) {
        Ok(path) => Content::text(format!(
            "[Binary resource {uri} ({kind}) saved to {}]",
            path.display()
        )),
        Err(error) => Content::text(format!(
            "[Binary resource {uri} ({kind}) could not be saved: {error}]"
        )),
    }
}

fn text_of(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            Content::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A server's result as Pi gives it to the model. An error result stays an
/// error, with a line saying so when the server gave no text. Text over
/// [`MCP_OUTPUT_MAX_BYTES`] is cut in the middle, in Codex's format, and the
/// full text saved to a file; images follow it.
pub(super) fn convert_result(
    server: &str,
    tool: &str,
    result: CallToolResult,
) -> pi_agent_core::AgentToolResult {
    let is_error = result.is_error == Some(true);
    let mut content: Vec<Content> = result.content.iter().map(block_content).collect();
    // Servers should mirror a structured result as text, and do not always.
    if content.is_empty()
        && let Some(structured) = &result.structured_content
    {
        content.push(Content::text(
            serde_json::to_string_pretty(structured).unwrap_or_default(),
        ));
    }
    if is_error && text_of(&content).is_empty() {
        content.push(Content::text(format!(
            "MCP tool {server}/{tool} returned an error"
        )));
    }
    let mut details = json!({ "server": server, "tool": tool });
    let combined = text_of(&content);
    let cut = truncate_middle(&combined, MCP_OUTPUT_MAX_BYTES);
    if cut.truncated {
        let saved = write_output_file(OUTPUT_FILE_PREFIX, ".txt", combined.as_bytes());
        let location = match &saved {
            Ok(path) => format!(
                "[Full output: {} (read it with offset/limit)]",
                path.display()
            ),
            Err(error) => format!("[Could not save the full output: {error}]"),
        };
        if let Ok(path) = saved {
            details["fullOutputPath"] = Value::String(path.display().to_string());
        }
        let tokens = cut.total_bytes.div_ceil(4);
        let text = format!(
            "Warning: truncated output (original token count: {tokens})\nTotal output lines: {}\n\n{}\n\n{location}",
            cut.total_lines, cut.content
        );
        content = std::iter::once(Content::text(text))
            .chain(
                content
                    .into_iter()
                    .filter(|block| matches!(block, Content::Image(_))),
            )
            .collect();
    }
    pi_agent_core::AgentToolResult {
        content,
        details: Some(details),
        is_error,
        ..pi_agent_core::AgentToolResult::default()
    }
}

#[cfg(test)]
mod tests;
