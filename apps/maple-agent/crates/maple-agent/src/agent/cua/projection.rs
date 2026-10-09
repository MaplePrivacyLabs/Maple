//! What the model sees of CUA: its catalog bound to Maple's task session,
//! and each result as content, with the structured observation as bounded,
//! image-free text, and screenshots described for a model without vision.
//!
//! Nothing here touches the SDK, so all of it is tested on every platform.

use std::collections::HashSet;
use std::future::Future;

use pi_agent_core::AgentToolResult;
use pi_ai::Content;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::agent::side_models::IMAGE_DESCRIPTION_CONTEXT_MAX_CHARS;

/// Lifecycle tools Maple runs itself; the model never sees them.
pub(super) const HOST_OWNED_TOOLS: &[&str] = &[
    "start_session",
    "end_session",
    "get_session",
    "list_sessions",
    "get_session_state",
    "escalate_session",
];
/// The structured observation every model gets, at most this long.
pub(super) const MODEL_STRUCTURED_PROJECTION_MAX_CHARS: usize = 32_000;
/// The structured part of what the screenshot helper gets.
const HELPER_STRUCTURED_CHARS: usize = 6_000;
const REDACTED_IMAGE_PAYLOAD: &str = "[raw image payload omitted]";
/// Shorter than any screenshot and far longer than a CUA element token or
/// coordinate descriptor, so ordinary grounding fields are never redacted.
const MIN_REDACTED_BASE64_CHARS: usize = 512;
const PROJECTION_REMEDY: &str =
    "Request a focused CUA observation with query or lower result limits.";

/// One tool of the catalog, as the model is offered it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct CatalogTool {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) input_schema: Value,
}

/// CUA's tool catalog, bound to Maple's task session: the lifecycle tools
/// and every `session` or `_`-prefixed argument removed. Schemas reach the
/// model as CUA published them otherwise. An empty catalog, a nameless or
/// duplicate tool, or one with only lifecycle tools fails closed.
pub(super) fn parse_catalog(catalog: &str) -> Result<Vec<CatalogTool>, String> {
    let catalog: Value = serde_json::from_str(catalog)
        .map_err(|error| format!("Embedded CUA returned an invalid tool catalog: {error}"))?;
    let tools = catalog
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| "Embedded CUA returned an invalid tool catalog: no tools".to_string())?;
    if tools.is_empty() {
        return Err("Embedded CUA returned an empty tool catalog".to_string());
    }
    let mut names = HashSet::new();
    let mut bound = Vec::new();
    for tool in tools {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if name.is_empty() {
            return Err("Embedded CUA returned a tool with an empty name".to_string());
        }
        if !names.insert(name.clone()) {
            return Err(format!(
                "Embedded CUA returned duplicate tool name '{name}'"
            ));
        }
        if HOST_OWNED_TOOLS.contains(&name.as_str()) {
            continue;
        }
        let mut input_schema = tool
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"}));
        bind_schema(&mut input_schema);
        bound.push(CatalogTool {
            name,
            description: tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            input_schema,
        });
    }
    if bound.is_empty() {
        return Err("Embedded CUA returned no task tools after binding its lifecycle".to_string());
    }
    Ok(bound)
}

/// Remove the arguments Maple supplies itself: the bound session, and any
/// `_`-prefixed field the SDK reserves.
fn bind_schema(schema: &mut Value) {
    let Some(schema) = schema.as_object_mut() else {
        return;
    };
    if let Some(Value::Object(properties)) = schema.get_mut("properties") {
        properties.retain(|name, _| !reserved_argument(name));
    }
    if let Some(Value::Array(required)) = schema.get_mut("required") {
        required.retain(|field| field.as_str().is_none_or(|name| !reserved_argument(name)));
        if required.is_empty() {
            schema.remove("required");
        }
    }
}

fn reserved_argument(name: &str) -> bool {
    name == "session" || name.starts_with('_')
}

/// A call's arguments without what the model must not choose: the session
/// and reserved fields. Nested values are the user's data and stay.
pub(super) fn bound_arguments(mut arguments: Map<String, Value>) -> Map<String, Value> {
    arguments.retain(|name, _| !reserved_argument(name));
    arguments
}

/// One CUA result, as the SDK returns it in MCP's shape.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct CuaResult {
    pub(super) content: Vec<Content>,
    pub(super) structured: Option<Value>,
    pub(super) is_error: bool,
}

/// Read the SDK's canonical result. Text and images become content;
/// anything else is named in a line, so nothing disappears unnoticed.
pub(super) fn parse_result(raw_json: &str) -> Result<CuaResult, String> {
    let raw: Value = serde_json::from_str(raw_json)
        .map_err(|_| "Embedded CUA returned an unreadable result".to_string())?;
    let content = raw
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| blocks.iter().map(content_block).collect())
        .unwrap_or_default();
    Ok(CuaResult {
        content,
        structured: raw.get("structuredContent").cloned(),
        is_error: raw.get("isError").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn content_block(block: &Value) -> Content {
    let field = |name: &str| block.get(name).and_then(Value::as_str).unwrap_or_default();
    match field("type") {
        "text" => Content::text(field("text")),
        "image" => Content::image(field("data"), field("mimeType")),
        other => Content::text(format!("[{other} content omitted]")),
    }
}

/// The structured observation without pixels, and without what a text
/// block already carries. Both the model's projection and the helper's
/// context are cut from it.
pub(super) fn structured_grounding_base(structured: Option<&Value>) -> Option<Value> {
    let mut projection = structured?.clone();
    redact_image_payloads(&mut projection);
    if let Some(object) = projection.as_object_mut() {
        // CUA already includes the tree markdown in a normal text content
        // block. Do not spend the projection budget sending it twice.
        object.remove("tree_markdown");
        object.remove("_note");
    }
    if projection.as_object().is_some_and(Map::is_empty) {
        return None;
    }
    Some(projection)
}

/// `base` as JSON of at most `max_chars`. A larger observation keeps every
/// collection's first part, then only the fields a follow-up action needs,
/// then shorter strings, and says what it left out.
pub(super) fn structured_grounding_projection(base: &Value, max_chars: usize) -> Option<String> {
    let mut projection = base.clone();
    let serialized = serde_json::to_string(&projection).ok()?;
    if within_char_budget(&serialized, max_chars) {
        return Some(serialized);
    }
    let original_chars = serialized.chars().count();

    // CUA observations use different collection names: native AX snapshots
    // expose `elements`, while browser snapshots expose `refs`, `outline`, and
    // `content_refs`. Shrink all structured arrays as prefixes so every
    // projection remains valid JSON and identity/coordinate fields outside
    // those collections remain available to the model.
    let original = base;
    insert_projection_metadata(&mut projection, original, original_chars, false);
    loop {
        let serialized = serde_json::to_string(&projection).ok()?;
        if within_char_budget(&serialized, max_chars) {
            return Some(serialized);
        }
        if !shrink_projection_arrays(&mut projection) {
            break;
        }
        insert_projection_metadata(&mut projection, original, original_chars, false);
    }

    // An additive CUA field could still contain an unexpectedly large scalar
    // or object. Fall back to the small set of grounding fields needed to issue
    // a follow-up action, rather than slicing a serialized JSON document.
    projection = prioritized_grounding_fields(original);
    insert_projection_metadata(&mut projection, original, original_chars, true);
    loop {
        let serialized = serde_json::to_string(&projection).ok()?;
        if within_char_budget(&serialized, max_chars) {
            return Some(serialized);
        }
        if !shrink_projection_arrays(&mut projection) {
            break;
        }
        insert_projection_metadata(&mut projection, original, original_chars, true);
    }

    // CUA identifiers and coordinate descriptors are normally short. Bound
    // pathological future scalar values structurally as a final safety valve.
    for string_limit in [1_024, 512, 256, 128, 64, 32] {
        truncate_projection_strings(&mut projection, string_limit);
        let serialized = serde_json::to_string(&projection).ok()?;
        if within_char_budget(&serialized, max_chars) {
            return Some(serialized);
        }
    }

    let minimal = json!({
        "_maple_projection": {
            "truncated": true,
            "original_chars": original_chars,
            "strategy": "metadata_only",
            "remedy": PROJECTION_REMEDY,
        }
    });
    let serialized = serde_json::to_string(&minimal).ok()?;
    within_char_budget(&serialized, max_chars)
        .then_some(serialized)
        .or_else(|| (max_chars >= 2).then(|| "{}".to_string()))
}

/// A string never has more characters than bytes, so the cheap length test
/// settles the common case without walking the text.
fn within_char_budget(value: &str, max_chars: usize) -> bool {
    value.len() <= max_chars || value.chars().count() <= max_chars
}

fn insert_projection_metadata(
    projection: &mut Value,
    original: &Value,
    original_chars: usize,
    priority_fields_only: bool,
) {
    let Some(object) = projection.as_object_mut() else {
        return;
    };
    let mut collections = Map::new();
    if let Some(original) = original.as_object() {
        for (name, original_value) in original {
            let Some(available) = original_value.as_array().map(Vec::len) else {
                continue;
            };
            let included = object
                .get(name)
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            collections.insert(
                name.clone(),
                json!({ "included": included, "available": available }),
            );
        }
    }
    let mut metadata = json!({
        "truncated": true,
        "original_chars": original_chars,
        "strategy": if priority_fields_only {
            "priority_fields_only"
        } else {
            "structured_collection_prefixes"
        },
        "remedy": PROJECTION_REMEDY,
    });
    if !collections.is_empty() {
        metadata["collections"] = Value::Object(collections);
    }
    object.insert("_maple_projection".to_string(), metadata);
}

fn shrink_projection_arrays(value: &mut Value) -> bool {
    match value {
        Value::Array(values) => {
            let mut changed = false;
            if !values.is_empty() {
                values.truncate(values.len() / 2);
                changed = true;
            }
            for value in values {
                changed |= shrink_projection_arrays(value);
            }
            changed
        }
        Value::Object(object) => object
            .iter_mut()
            .filter(|(name, _)| name.as_str() != "_maple_projection")
            .fold(false, |changed, (_, value)| {
                shrink_projection_arrays(value) || changed
            }),
        _ => false,
    }
}

fn prioritized_grounding_fields(original: &Value) -> Value {
    const PRIORITY_FIELDS: &[&str] = &[
        "status",
        "mode",
        "pid",
        "application",
        "app",
        "app_name",
        "target_id",
        "tab_id",
        "window_id",
        "window",
        "window_title",
        "title",
        "url",
        "page",
        "snapshot",
        "screenshot",
        "screenshot_width",
        "screenshot_height",
        "screenshot_mime_type",
        "coordinate_space",
        "coordinate_frame",
        "bounds",
        "scale",
        "degradation",
        "escalation",
        "background_input",
        "error",
        "message",
    ];

    let Some(original) = original.as_object() else {
        return json!({ "value": original });
    };
    let mut projection = Map::new();
    for name in PRIORITY_FIELDS {
        if let Some(value) = original.get(*name) {
            projection.insert((*name).to_string(), value.clone());
        }
    }
    Value::Object(projection)
}

fn truncate_projection_strings(value: &mut Value, max_chars: usize) {
    match value {
        Value::String(text) => {
            if text.chars().count() > max_chars {
                *text = text.chars().take(max_chars).collect();
            }
        }
        Value::Array(values) => {
            for value in values {
                truncate_projection_strings(value, max_chars);
            }
        }
        Value::Object(object) => {
            for (name, value) in object {
                if name != "_maple_projection" {
                    truncate_projection_strings(value, max_chars);
                }
            }
        }
        _ => {}
    }
}

fn redact_image_payloads(value: &mut Value) {
    match value {
        Value::String(text) => {
            if text.starts_with("data:image/") || looks_like_encoded_image(text) {
                *text = REDACTED_IMAGE_PAYLOAD.to_string();
            }
        }
        Value::Object(object) => {
            for (key, value) in object {
                let normalized = key.to_ascii_lowercase();
                if normalized.contains("base64") || normalized.ends_with("_b64") {
                    *value = Value::String(REDACTED_IMAGE_PAYLOAD.to_string());
                } else {
                    redact_image_payloads(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_image_payloads(value);
            }
        }
        _ => {}
    }
}

/// Detect a bare base64 blob under any key or inside any array.
///
/// An additive SDK field could carry pixels under a name Maple does not know,
/// which would otherwise spend the whole projection budget on image bytes that
/// the text-only model cannot read anyway.
fn looks_like_encoded_image(text: &str) -> bool {
    text.len() >= MIN_REDACTED_BASE64_CHARS
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
}

/// What the screenshot helper is told besides the screenshot: the tool,
/// a smaller projection of the observation, and the result's text, bounded.
pub(super) fn mediation_context(
    tool_name: &str,
    content: &[Content],
    grounding: Option<&Value>,
) -> String {
    let mut context = format!(
        "CUA observation tool: {tool_name}\n\
         Cross-check the screenshot against the retained accessibility and structured facts below. \
         Report only visual evidence; the primary model will choose any next action."
    );
    if let Some(structured) =
        grounding.and_then(|base| structured_grounding_projection(base, HELPER_STRUCTURED_CHARS))
    {
        context.push_str("\n\nRetained structured grounding data:\n");
        context.push_str(&structured);
    }
    for block in content {
        if let Content::Text(text) = block
            && !text.text.trim().is_empty()
        {
            context.push_str("\n\nRetained accessibility/tool text:\n");
            context.push_str(&text.text);
        }
    }
    bounded_with_suffix(
        &context,
        IMAGE_DESCRIPTION_CONTEXT_MAX_CHARS,
        "\n[CUA helper context truncated; rely on the retained primary-model tool result for omitted details]",
    )
}

/// `text` within `max_chars`, ending in `suffix` when it was cut.
fn bounded_with_suffix(text: &str, max_chars: usize, suffix: &str) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(suffix.chars().count());
    let mut bounded: String = text.chars().take(keep).collect();
    bounded.push_str(suffix);
    bounded
}

/// One screenshot the helper is asked about: its place among the result's
/// screenshots, its base64 data and type, and the context it gets.
pub(super) struct Screenshot {
    pub(super) index: usize,
    pub(super) count: usize,
    pub(super) data: String,
    pub(super) mime_type: String,
    pub(super) context: String,
}

/// What the model gets of one CUA result. Every model gets the structured
/// observation as text after the result's own content. A model without
/// vision gets no image: each screenshot is described by `describe` in its
/// place, after the rest, and a description that fails says so. A call
/// cancelled meanwhile is reported as cancelled.
pub(super) async fn result_for_model<F, Fut>(
    tool_name: &str,
    result: CuaResult,
    vision: bool,
    cancel: &CancellationToken,
    describe: F,
) -> Result<AgentToolResult, String>
where
    F: Fn(Screenshot) -> Fut,
    Fut: Future<Output = Result<String, String>>,
{
    let CuaResult {
        mut content,
        structured,
        is_error,
    } = result;
    let grounding = structured_grounding_base(structured.as_ref());
    let has_image = content
        .iter()
        .any(|block| matches!(block, Content::Image(_)));
    let helper_context =
        (!vision && has_image).then(|| mediation_context(tool_name, &content, grounding.as_ref()));
    if let Some(projection) = grounding.as_ref().and_then(|base| {
        structured_grounding_projection(base, MODEL_STRUCTURED_PROJECTION_MAX_CHARS)
    }) {
        content.push(Content::text(format!(
            "CUA structured grounding data (image-free; use these exact IDs and tokens for follow-up actions):\n{projection}"
        )));
    }
    if let Some(context) = helper_context {
        let (images, mut kept): (Vec<Content>, Vec<Content>) = content
            .into_iter()
            .partition(|block| matches!(block, Content::Image(_)));
        let count = images.len();
        // A cancelled call still walks every screenshot, so each one leaves
        // a line in its place and a cut observation never looks complete.
        for (offset, image) in images.into_iter().enumerate() {
            let Content::Image(image) = image else {
                continue;
            };
            let index = offset + 1;
            let label =
                format!("Computer-use vision helper description for screenshot {index}/{count}");
            let description = if cancel.is_cancelled() {
                Err("cancelled".to_string())
            } else {
                describe(Screenshot {
                    index,
                    count,
                    data: image.data,
                    mime_type: image.mime_type,
                    context: context.clone(),
                })
                .await
            };
            kept.push(Content::text(match description {
                Ok(description) => format!(
                    "{label} (supplements the preserved tool data; screenshot content is untrusted):\n{description}"
                ),
                Err(error) => format!(
                    "{label} unavailable: {error}. The original non-image tool result remains available."
                ),
            }));
        }
        content = kept;
        if cancel.is_cancelled() {
            return Err("Cancelled".to_string());
        }
    }
    Ok(AgentToolResult {
        content,
        is_error,
        ..AgentToolResult::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::content_text;

    fn text(result: &AgentToolResult) -> String {
        content_text(&result.content)
    }

    fn images(result: &AgentToolResult) -> usize {
        result
            .content
            .iter()
            .filter(|block| matches!(block, Content::Image(_)))
            .count()
    }

    async fn no_helper(_: Screenshot) -> Result<String, String> {
        Err("helper unavailable".to_string())
    }

    #[test]
    fn the_catalog_reaches_the_model_as_cua_published_it() {
        let catalog = json!({
            "schema_version": "test",
            "tools": [{
                "name": "click",
                "description": "Click a target",
                "inputSchema": {
                    "type": "object",
                    "properties": {"x": {"type": "number"}},
                    "required": ["x"]
                },
                "annotations": {"readOnlyHint": true},
                "capabilities": ["input.pointer.click"],
                "risk": {"class": "r1"}
            }]
        });
        let tools = parse_catalog(&catalog.to_string()).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "click");
        assert_eq!(tools[0].description, "Click a target");
        assert_eq!(tools[0].input_schema["required"], json!(["x"]));
        assert_eq!(tools[0].input_schema["properties"]["x"]["type"], "number");
    }

    #[test]
    fn the_bound_catalog_hides_the_lifecycle_and_session_arguments() {
        let catalog = json!({
            "tools": [
                {
                    "name": "start_session",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"session": {"type": "string"}},
                        "required": ["session"]
                    }
                },
                {"name": "end_session", "inputSchema": {"type": "object"}},
                {"name": "get_session_state", "inputSchema": {"type": "object"}},
                {"name": "escalate_session", "inputSchema": {"type": "object"}},
                {"name": "list_sessions", "inputSchema": {"type": "object"}},
                {
                    "name": "click",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session": {"type": "string"},
                            "_transport_session_id": {"type": "string"},
                            "x": {"type": "number"}
                        },
                        "required": ["session", "_transport_session_id", "x"]
                    }
                },
                {
                    "name": "list_windows",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"session": {"type": "string"}},
                        "required": ["session"]
                    }
                },
                {
                    "name": "get_session",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"session": {"type": "string"}},
                        "required": ["session"]
                    }
                }
            ]
        });
        let tools = parse_catalog(&catalog.to_string()).unwrap();
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, ["click", "list_windows"]);
        let click = &tools[0].input_schema;
        assert!(click["properties"].get("session").is_none());
        assert!(click["properties"].get("_transport_session_id").is_none());
        assert_eq!(click["required"], json!(["x"]));
        let list_windows = &tools[1].input_schema;
        assert!(list_windows["properties"].get("session").is_none());
        assert!(list_windows.get("required").is_none());
    }

    #[test]
    fn a_catalog_that_is_empty_duplicated_or_only_lifecycle_fails_closed() {
        assert!(parse_catalog(&json!({"tools": []}).to_string()).is_err());
        assert!(parse_catalog("not json").is_err());
        let duplicate = json!({
            "tools": [
                {"name": "click", "inputSchema": {"type": "object"}},
                {"name": "click", "inputSchema": {"type": "object"}}
            ]
        });
        assert!(parse_catalog(&duplicate.to_string()).is_err());
        let nameless = json!({"tools": [{"name": "  ", "inputSchema": {"type": "object"}}]});
        assert!(parse_catalog(&nameless.to_string()).is_err());
        let lifecycle = json!({
            "tools": HOST_OWNED_TOOLS
                .iter()
                .map(|name| json!({"name": name, "inputSchema": {"type": "object"}}))
                .collect::<Vec<_>>()
        });
        assert!(parse_catalog(&lifecycle.to_string()).is_err());
    }

    #[test]
    fn calls_drop_model_supplied_session_and_reserved_arguments() {
        let arguments = bound_arguments(Map::from_iter([
            ("session".to_string(), json!("cli-session")),
            ("_session_id".to_string(), json!("forged")),
            ("_transport_session_id".to_string(), json!("forged")),
            ("_future_internal".to_string(), json!(true)),
            ("window_id".to_string(), json!(7)),
            (
                "target".to_string(),
                json!({"_field": "nested values are user data"}),
            ),
        ]));
        assert!(arguments.get("session").is_none());
        assert!(arguments.get("_session_id").is_none());
        assert!(arguments.get("_transport_session_id").is_none());
        assert!(arguments.get("_future_internal").is_none());
        assert_eq!(arguments["window_id"], 7);
        assert_eq!(arguments["target"]["_field"], "nested values are user data");
    }

    #[test]
    fn image_bytes_are_redacted_wherever_they_appear() {
        let pixels = "A".repeat(MIN_REDACTED_BASE64_CHARS);
        let mut value = json!({
            "screenshot_b64": "short-but-named",
            "frames": [pixels.clone()],
            "nested": {"data": pixels.clone()},
            "inline": "data:image/png;base64,aGk=",
            "element_token": "sdef:4",
            "prose": "A window titled Calculator is in front."
        });
        redact_image_payloads(&mut value);
        assert_eq!(value["screenshot_b64"], REDACTED_IMAGE_PAYLOAD);
        // A bare blob inside an array or under an unknown key is caught too.
        assert_eq!(value["frames"][0], REDACTED_IMAGE_PAYLOAD);
        assert_eq!(value["nested"]["data"], REDACTED_IMAGE_PAYLOAD);
        assert_eq!(value["inline"], REDACTED_IMAGE_PAYLOAD);
        // Ordinary grounding fields and prose survive untouched.
        assert_eq!(value["element_token"], "sdef:4");
        assert_eq!(value["prose"], "A window titled Calculator is in front.");
    }

    #[test]
    fn a_result_keeps_its_images_text_and_error() {
        let result = parse_result(
            &json!({
                "content": [
                    {"type": "text", "text": "captured"},
                    {"type": "image", "mimeType": "image/png", "data": "cG5n"},
                    {"type": "audio", "mimeType": "audio/wav", "data": "d2F2"}
                ],
                "structuredContent": {"width": 100},
                "isError": true
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(result.content.len(), 3);
        assert!(matches!(&result.content[1], Content::Image(image) if image.data == "cG5n"));
        assert_eq!(
            content_text(&result.content[2..]),
            "[audio content omitted]"
        );
        assert_eq!(result.structured.unwrap()["width"], 100);
        assert!(result.is_error);
        assert!(parse_result("not json").is_err());
    }

    #[tokio::test]
    async fn every_model_gets_the_grounding_and_a_vision_model_keeps_images() {
        let result = parse_result(
            &json!({
                "content": [
                    {"type": "image", "mimeType": "image/png", "data": "raw-pixels"},
                    {"type": "text", "text": "window tree"}
                ],
                "structuredContent": {"window_id": 7},
                "isError": false
            })
            .to_string(),
        )
        .unwrap();
        let vision = result_for_model(
            "get_window_state",
            result.clone(),
            true,
            &CancellationToken::new(),
            no_helper,
        )
        .await
        .unwrap();
        assert_eq!(vision.content.len(), 3);
        assert_eq!(images(&vision), 1);
        assert!(text(&vision).contains("\"window_id\":7"));
        assert!(!vision.is_error);

        let image_free = parse_result(
            &json!({
                "content": [{"type": "text", "text": "tree only"}],
                "structuredContent": {"window_id": 7}
            })
            .to_string(),
        )
        .unwrap();
        let projected = result_for_model(
            "get_window_state",
            image_free,
            false,
            &CancellationToken::new(),
            no_helper,
        )
        .await
        .unwrap();
        assert_eq!(projected.content.len(), 2);
        assert!(text(&projected).starts_with("tree only"));
        assert!(text(&projected).contains("\"window_id\":7"));

        // Without structured grounding, a vision model gets the result as is.
        let plain = parse_result(
            &json!({"content": [{"type": "text", "text": "No structured payload"}]}).to_string(),
        )
        .unwrap();
        let unchanged = result_for_model(
            "health_report",
            plain.clone(),
            true,
            &CancellationToken::new(),
            no_helper,
        )
        .await
        .unwrap();
        assert_eq!(unchanged.content, plain.content);
    }

    #[tokio::test]
    async fn a_model_without_vision_gets_descriptions_in_place_of_screenshots() {
        let result = parse_result(
            &json!({
                "content": [
                    {"type": "image", "mimeType": "image/png", "data": "first-raw-image"},
                    {"type": "text", "text": "window_id=7 size=900x600\n[3] AXButton: 19"},
                    {"type": "image", "mimeType": "image/jpeg", "data": "second-raw-image"}
                ],
                "structuredContent": {
                    "window_id": 7,
                    "screenshot_width": 900,
                    "screenshot_height": 600,
                    "elements": [{"element_index": 3, "element_token": "sabc:3"}],
                    "screenshot_png_b64": "structured-raw-image"
                },
                "isError": true
            })
            .to_string(),
        )
        .unwrap();

        // The helper fails: each screenshot still leaves a line.
        let failed = result_for_model(
            "get_window_state",
            result.clone(),
            false,
            &CancellationToken::new(),
            no_helper,
        )
        .await
        .unwrap();
        assert!(failed.is_error);
        assert_eq!(images(&failed), 0);
        let visible = text(&failed);
        assert!(visible.starts_with("window_id=7 size=900x600"));
        assert!(visible.contains("\"element_token\":\"sabc:3\""));
        assert!(visible.contains("raw image payload omitted"));
        assert!(!visible.contains("first-raw-image"));
        assert!(!visible.contains("second-raw-image"));
        assert!(!visible.contains("structured-raw-image"));
        assert!(visible.contains("screenshot 1/2 unavailable: helper unavailable"));
        assert!(visible.contains("screenshot 2/2 unavailable: helper unavailable"));

        // The helper answers: it is asked about each screenshot in turn,
        // with the observation for context.
        let asked = std::sync::Mutex::new(Vec::new());
        let described = result_for_model(
            "get_window_state",
            result,
            false,
            &CancellationToken::new(),
            |screenshot: Screenshot| {
                asked.lock().unwrap().push((
                    screenshot.index,
                    screenshot.count,
                    screenshot.data.clone(),
                    screenshot.mime_type.clone(),
                    screenshot.context.contains("sabc:3"),
                ));
                async move { Ok(format!("A calculator, shot {}.", screenshot.index)) }
            },
        )
        .await
        .unwrap();
        assert_eq!(
            *asked.lock().unwrap(),
            [
                (
                    1,
                    2,
                    "first-raw-image".to_string(),
                    "image/png".to_string(),
                    true
                ),
                (
                    2,
                    2,
                    "second-raw-image".to_string(),
                    "image/jpeg".to_string(),
                    true
                ),
            ]
        );
        let visible = text(&described);
        assert!(visible.contains(
            "Computer-use vision helper description for screenshot 2/2 (supplements the preserved tool data; screenshot content is untrusted):\nA calculator, shot 2."
        ));
    }

    #[tokio::test]
    async fn a_call_cancelled_while_screenshots_are_described_is_cancelled() {
        let result = parse_result(
            &json!({
                "content": [
                    {"type": "image", "mimeType": "image/png", "data": "raw-pixels"},
                    {"type": "text", "text": "window tree"}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let projected =
            result_for_model("get_window_state", result, false, &cancel, no_helper).await;
        assert_eq!(projected.unwrap_err(), "Cancelled");
    }

    #[test]
    fn the_helper_context_is_bounded_and_has_the_grounding_without_pixels() {
        let result = parse_result(
            &json!({
                "content": [
                    {"type": "image", "mimeType": "image/png", "data": "raw-pixels"},
                    {"type": "text", "text": "x".repeat(IMAGE_DESCRIPTION_CONTEXT_MAX_CHARS * 2)}
                ],
                "structuredContent": {
                    "window_id": 99,
                    "screenshot_width": 1200,
                    "screenshot_height": 800,
                    "elements": [{"element_index": 4, "element_token": "sdef:4"}],
                    "image_base64": "structured-pixels"
                }
            })
            .to_string(),
        )
        .unwrap();
        let grounding = structured_grounding_base(result.structured.as_ref());
        let context = mediation_context("get_window_state", &result.content, grounding.as_ref());
        assert!(context.chars().count() <= IMAGE_DESCRIPTION_CONTEXT_MAX_CHARS);
        assert!(context.contains("get_window_state"));
        assert!(context.contains("sdef:4"));
        assert!(context.contains("screenshot_width"));
        assert!(!context.contains("raw-pixels"));
        assert!(!context.contains("structured-pixels"));
    }

    #[test]
    fn a_large_browser_projection_is_valid_bounded_and_keeps_action_grounding() {
        let refs = (0..1_500)
            .map(|index| {
                json!({
                    "ref": format!("p42:{index}"),
                    "role": "button",
                    "name": format!("Browser action target {index} with a deliberately long label"),
                    "frame": "main",
                    "visibility": "visible"
                })
            })
            .collect::<Vec<_>>();
        let content_refs = (0..1_000)
            .map(|index| {
                json!({
                    "ref": format!("c42:{index}"),
                    "text": format!("Visible browser content row {index} with additional context")
                })
            })
            .collect::<Vec<_>>();
        let structured = json!({
            "status": "ok",
            "mode": "snapshot",
            "target_id": "target-7",
            "tab_id": "tab-9",
            "window_id": 17,
            "refs": refs,
            "content_refs": content_refs,
            "outline": (0..500).map(|index| format!("outline row {index}")).collect::<Vec<_>>(),
            "screenshot": {
                "source": "cdp_tab",
                "scope": "viewport",
                "width": 2400,
                "height": 1600,
                "coordinate_space": "viewport_css_px",
                "viewport_css_width": 1200,
                "viewport_css_height": 800,
                "pixel_to_css_scale_x": 0.5,
                "pixel_to_css_scale_y": 0.5
            }
        });
        let base = structured_grounding_base(Some(&structured)).unwrap();
        for budget in [6_000, MODEL_STRUCTURED_PROJECTION_MAX_CHARS] {
            let projection = structured_grounding_projection(&base, budget).unwrap();
            assert!(projection.chars().count() <= budget);
            let projection: Value = serde_json::from_str(&projection).unwrap();
            assert_eq!(projection["target_id"], "target-7");
            assert_eq!(projection["tab_id"], "tab-9");
            assert_eq!(projection["window_id"], 17);
            assert_eq!(
                projection["screenshot"]["coordinate_space"],
                "viewport_css_px"
            );
            assert_eq!(projection["screenshot"]["width"], 2400);
            assert_eq!(projection["_maple_projection"]["truncated"], true);
            assert_eq!(
                projection["_maple_projection"]["collections"]["refs"]["available"],
                1_500
            );
            assert!(
                projection["_maple_projection"]["collections"]["refs"]["included"]
                    .as_u64()
                    .unwrap()
                    < 1_500
            );
        }
    }
}
