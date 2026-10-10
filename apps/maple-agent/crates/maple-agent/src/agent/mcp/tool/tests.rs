use rmcp::model::{EmbeddedResource, Resource};

use super::*;

fn result(content: Vec<ContentBlock>) -> CallToolResult {
    CallToolResult::success(content)
}

fn texts(content: &[Content]) -> Vec<String> {
    content
        .iter()
        .map(|block| match block {
            Content::Text(text) => text.text.clone(),
            Content::Image(image) => format!("<image {}>", image.mime_type),
        })
        .collect()
}

#[test]
fn tools_are_named_as_pi_names_them() {
    let free = |_: &str| false;
    assert_eq!(
        tool_name("github", "list_issues", free),
        "mcp__github__list_issues"
    );
    assert_eq!(
        tool_name("my-server", "list.issues", free),
        "mcp__my_server__list_issues"
    );
    // A name another tool has, or one too long, ends in a hash of the two.
    let taken = tool_name("a", "b-c", |name| name == "mcp__a__b_c");
    assert!(taken.starts_with("mcp__a__b_c_") && taken.len() == "mcp__a__b_c_".len() + 8);
    let long = tool_name("server", &"x".repeat(80), free);
    assert_eq!(long.len(), 64);
    assert_ne!(long, tool_name("server", &"x".repeat(81), free));
}

#[test]
fn schemas_are_objects_with_properties() {
    let bare = JsonObject::new();
    assert_eq!(
        parameters(&bare),
        json!({"type": "object", "properties": {}})
    );
    let given: JsonObject = serde_json::from_value(json!({
        "type": "object",
        "properties": {"q": {"type": "string"}},
        "required": ["q"]
    }))
    .unwrap();
    assert_eq!(parameters(&given), Value::Object(given.clone()));

    let mut tool = rmcp::model::Tool::new("search", "  Search the docs  ", JsonObject::new());
    assert_eq!(description("docs", &tool), "Search the docs");
    tool.description = None;
    assert_eq!(
        description("docs", &tool),
        "MCP tool search from server docs"
    );
    tool.title = Some("Doc search".into());
    assert_eq!(description("docs", &tool), "Doc search");
}

#[test]
fn results_become_text_and_images() {
    let blob = |mime: &str, data: &[u8]| {
        ContentBlock::Resource(EmbeddedResource::new(
            ResourceContents::BlobResourceContents {
                uri: "file:///report.pdf".into(),
                mime_type: Some(mime.into()),
                blob: base64::engine::general_purpose::STANDARD.encode(data),
                meta: None,
            },
        ))
    };
    let mut link = Resource::new("file:///notes.md", "notes");
    link.mime_type = Some("text/markdown".into());
    link.size = Some(2048);
    let converted = convert_result(
        "docs",
        "fetch",
        result(vec![
            ContentBlock::text("plain"),
            ContentBlock::image("aW1n", "image/png"),
            ContentBlock::audio("YXVk", "audio/wav"),
            ContentBlock::ResourceLink(link),
            ContentBlock::Resource(EmbeddedResource::new(ResourceContents::text(
                "embedded text",
                "file:///a.txt",
            ))),
            blob("image/jpeg", b"jpg"),
            blob("application/json", br#"{"a":1}"#),
        ]),
    );
    assert!(!converted.is_error);
    assert_eq!(
        texts(&converted.content),
        [
            "plain",
            "<image image/png>",
            "[audio audio/wav omitted]",
            "[Resource file:///notes.md \"notes\" (text/markdown, 2.0KB)]",
            "embedded text",
            "<image image/jpeg>",
            "{\"a\":1}",
        ]
    );
    assert_eq!(
        converted.details.unwrap(),
        json!({"server": "docs", "tool": "fetch"})
    );

    // Other blobs are saved for the model to open.
    let saved = convert_result(
        "docs",
        "fetch",
        result(vec![blob("application/pdf", b"%PDF")]),
    );
    let text = texts(&saved.content).remove(0);
    let path = text
        .strip_prefix("[Binary resource file:///report.pdf (application/pdf, 4B) saved to ")
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(path.ends_with(".pdf"));
    assert_eq!(std::fs::read(path).unwrap(), b"%PDF");
    std::fs::remove_file(path).unwrap();
}

#[test]
fn structured_and_failed_results_say_something() {
    let mut structured = result(Vec::new());
    structured.structured_content = Some(json!({"count": 2}));
    assert_eq!(
        texts(&convert_result("db", "count", structured).content),
        ["{\n  \"count\": 2\n}"]
    );
    let failed = convert_result("db", "drop", CallToolResult::error(Vec::new()));
    assert!(failed.is_error);
    assert_eq!(
        texts(&failed.content),
        ["MCP tool db/drop returned an error"]
    );
    let explained = convert_result(
        "db",
        "drop",
        CallToolResult::error(vec![ContentBlock::text("no such table")]),
    );
    assert_eq!(texts(&explained.content), ["no such table"]);
}

#[test]
fn long_text_is_cut_in_the_middle_and_saved_whole() {
    let long = format!("{}\n{}", "a".repeat(MCP_OUTPUT_MAX_BYTES), "b".repeat(100));
    let converted = convert_result(
        "logs",
        "tail",
        result(vec![
            ContentBlock::text(long.clone()),
            ContentBlock::image("aW1n", "image/png"),
        ]),
    );
    let shown = texts(&converted.content);
    assert_eq!(shown.len(), 2, "the text, then the image");
    assert!(shown[0].starts_with(&format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: 2\n\naaaa",
        long.len().div_ceil(4)
    )));
    assert!(shown[0].contains("chars truncated…"));
    let path = converted.details.unwrap()["fullOutputPath"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(shown[0].ends_with(&format!(
        "[Full output: {path} (read it with offset/limit)]"
    )));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), long);
    std::fs::remove_file(path).unwrap();
    assert_eq!(shown[1], "<image image/png>");
}
