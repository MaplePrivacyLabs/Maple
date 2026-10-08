use super::common::*;
use pi_ai::api::openai_completions::build_params;
use pi_ai::types::*;
use pi_ai::utils::text::{get_system_message_text, render_system_message_update};
use pi_ai::utils::transcript::*;
use serde_json::{Value, json};

fn tool(name: &str) -> Value {
    json!({"name":name,"description":format!("{name} tool"),"parameters":{"type":"object","properties":{}}})
}
fn context() -> TranscriptContext {
    normalize_context(json(json!({"messages":[
        {"role":"system","content":"base prompt","sections":{"rules":"<rules>\nold rules\n</rules>","docs":"<docs>\nread docs\n</docs>"},"toolsAdded":[tool("base_tool")],"timestamp":0},
        {"role":"user","content":"before","timestamp":1},
        {"role":"system","content":"updated guidance","sections":{"rules":"<rules>\nnew rules\n</rules>","docs":null},"toolsRemoved":[{"name":"base_tool"}],"toolsAdded":[tool("late_tool")],"timestamp":2}
    ]})))
}
fn addition_context() -> TranscriptContext {
    normalize_context(json(json!({"messages":[
        {"role":"system","content":"base prompt","toolsAdded":[tool("base_tool")],"timestamp":0},
        {"role":"user","content":"before","timestamp":1},
        {"role":"system","content":"updated guidance","toolsAdded":[tool("late_tool")],"timestamp":2}
    ]})))
}
fn model(id: &str, name: &str, provider: &str, reasoning: bool, compat: Value) -> Model {
    json(
        json!({"id":id,"name":name,"api":"openai-completions","provider":provider,
        "baseUrl":"http://127.0.0.1:9","reasoning":reasoning,"input":["text"],
        "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
        "contextWindow":100000,"maxTokens":1000,"compat":compat}),
    )
}
fn names(tools: &[Tool]) -> Vec<String> {
    tools
        .iter()
        .map(|tool| tool.name.to_string_lossy())
        .collect()
}
fn system(message: &Message) -> &SystemMessage {
    match message {
        Message::System(system) => system,
        _ => panic!("expected system message"),
    }
}
fn tool_names(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect()
}
fn payload(model: &Model, context: &TranscriptContext) -> Value {
    serde_json::to_value(build_params(model, context, None, env().as_ref()).unwrap()).unwrap()
}
fn assert_collapsed(context: TranscriptContext) {
    assert_eq!(
        names(&get_current_tools(&context.messages).unwrap()),
        ["late_tool"]
    );
    assert_eq!(
        get_system_message_text(system(&context.messages[0])),
        JsString::from("base prompt\n\nupdated guidance\n\n<rules>\nnew rules\n</rules>")
    );
    let remaining = without_initial_system_message(&context.messages);
    assert_eq!(remaining.len(), 1);
    assert!(matches!(remaining[0], Message::User(_)));
}

mod transcript_system_messages {
    use super::*;

    #[test]
    fn sends_anthropic_updates_and_tool_changes_in_native_system_messages() {
        let context = context();
        assert_eq!(
            get_system_message_text(system(&context.messages[0])),
            JsString::from(
                "base prompt\n\n<rules>\nold rules\n</rules>\n\n<docs>\nread docs\n</docs>"
            )
        );
        let update = render_system_message_update(system(&context.messages[2])).to_string_lossy();
        assert!(update.contains("updated guidance"));
        assert!(update.contains("<rules>\nnew rules\n</rules>"));
        assert!(update.contains("Removed system prompt section \"docs\""));
    }

    #[test]
    fn sends_the_current_anthropic_tool_list_without_an_initial_tool() {
        let context = normalize_context(json(json!({"messages":[
            {"role":"system","content":"base prompt","timestamp":0},
            {"role":"user","content":"before","timestamp":1},
            {"role":"system","content":"updated guidance","toolsAdded":[tool("late_tool")],"timestamp":2}
        ]})));
        assert_eq!(
            names(&get_current_tools(&context.messages).unwrap()),
            ["late_tool"]
        );
    }

    #[test]
    fn folds_anthropic_updates_into_the_system_prompt_without_native_support() {
        assert_collapsed(resolve_transcript(context(), Some(false)).unwrap());
    }

    #[test]
    fn requires_both_anthropic_capabilities_for_native_tool_changes() {
        let context = collapse_system_messages(context()).unwrap();
        assert_eq!(
            names(&get_current_tools(&context.messages).unwrap()),
            ["late_tool"]
        );
        let remaining = without_initial_system_message(&context.messages);
        assert_eq!(remaining.len(), 1);
        assert!(matches!(remaining[0], Message::User(_)));
    }

    #[test]
    fn anchors_openai_additions_at_their_developer_message() {
        let context = addition_context();
        let resolved = resolve_transcript_tools(&context.messages, true).unwrap();
        assert_eq!(names(&resolved.request_tools), ["base_tool"]);
        assert!(resolved.anchors_additions);
        assert_eq!(
            names(system(&context.messages[2]).tools_added.as_ref().unwrap()),
            ["late_tool"]
        );
        let text = context
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::System(message) => {
                    Some(get_system_message_text(message).to_string_lossy())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(text, ["base prompt", "updated guidance"]);
    }

    #[test]
    fn maps_system_message_additions_into_synthetic_tool_search() {
        let context = addition_context();
        let resolved = resolve_transcript_tools(&context.messages, true).unwrap();
        assert_eq!(names(&resolved.request_tools), ["base_tool"]);
        assert!(resolved.anchors_additions);
        assert_eq!(
            names(system(&context.messages[2]).tools_added.as_ref().unwrap()),
            ["late_tool"]
        );
    }

    #[test]
    fn folds_openai_updates_into_the_leading_developer_message_without_native_support() {
        assert_collapsed(resolve_transcript(context(), Some(false)).unwrap());
    }

    #[test]
    fn falls_back_to_the_complete_current_tool_state_when_removals_are_unsupported() {
        let context = context();
        let tools = resolve_transcript_tools(&context.messages, true).unwrap();
        assert_eq!(names(&tools.request_tools), ["late_tool"]);
        assert!(!tools.anchors_additions);
        let resolved = resolve_transcript(context, Some(true)).unwrap();
        assert_eq!(
            resolved
                .messages
                .iter()
                .filter(|message| matches!(message, Message::System(_)))
                .count(),
            2
        );
    }

    #[test]
    fn anchors_kimi_additions_in_tool_bearing_system_messages() {
        let model = model(
            "kimi-k3",
            "Kimi K3",
            "moonshotai",
            true,
            json!({"supportsMidConvoSystemMessages":true,"supportsMidConvoToolAdditions":true}),
        );
        let payload = payload(&model, &addition_context());
        assert_eq!(tool_names(&payload["tools"]), ["base_tool"]);
        let messages = payload["messages"].as_array().unwrap();
        let tools = messages
            .iter()
            .find_map(|message| message.get("tools"))
            .unwrap();
        assert_eq!(tool_names(tools), ["late_tool"]);
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["role"] == "system")
                .map(|message| message.get("content").and_then(Value::as_str))
                .collect::<Vec<_>>(),
            [Some("base prompt"), None, Some("updated guidance")]
        );
    }

    #[test]
    fn keeps_kimi_k2_system_text_inline_without_dynamic_tool_messages() {
        let model = model(
            "kimi-k2.7-code",
            "Kimi K2.7 Code",
            "moonshotai",
            true,
            json!({"supportsMidConvoSystemMessages":true}),
        );
        let payload = payload(&model, &addition_context());
        assert_eq!(tool_names(&payload["tools"]), ["base_tool", "late_tool"]);
        let messages = payload["messages"].as_array().unwrap();
        assert!(
            !messages
                .iter()
                .any(|message| message.get("tools").is_some())
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["role"] == "system")
                .map(|message| message["content"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["base prompt", "updated guidance"]
        );
    }

    #[test]
    fn folds_openai_compatible_updates_into_the_system_prompt_without_native_support() {
        let model = model(
            "custom-model",
            "Custom model",
            "custom-provider",
            false,
            json!({}),
        );
        let payload = payload(&model, &context());
        assert_eq!(tool_names(&payload["tools"]), ["late_tool"]);
        assert_eq!(
            payload["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|message| message["role"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["system", "user"]
        );
        assert_eq!(
            payload["messages"][0]["content"],
            "base prompt\n\nupdated guidance\n\n<rules>\nnew rules\n</rules>"
        );
    }
}
