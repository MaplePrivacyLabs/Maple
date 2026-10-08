use super::session_support::*;
use pi_agent_core::types::AgentMessage;
use pi_ai::types::JsValue;
use pi_coding_agent::core::{
    compaction::compaction::{
        CompactionPreparation, DEFAULT_COMPACTION_SETTINGS, estimate_projected_context_tokens,
        prepare_compaction,
    },
    session_manager::SessionManager,
};
use serde_json::json;
fn response(text: &str) -> AgentMessage {
    response_usage(text, 10, 1)
}
fn response_usage(text: &str, input: u64, output: u64) -> AgentMessage {
    json(
        json!({"role":"assistant","content":[{"type":"text","text":text}],"api":"faux","provider":"faux","model":"faux","usage":{"input":input,"output":output,"cacheRead":0,"cacheWrite":0,"totalTokens":input+output,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":NOW}),
    )
}
fn result(text: &str, error: bool) -> AgentMessage {
    let mut raw = json!({"role":"toolResult","toolCallId":"call-1","toolName":"read","content":[{"type":"text","text":text}],"isError":error,"timestamp":NOW});
    if error {
        raw["details"] = json!({"path":"large.txt"});
    }
    json(raw)
}
fn content(text: &str) -> JsValue {
    value(json!({"content":text}))
}
fn prepare(s: &SessionManager) -> Option<CompactionPreparation> {
    let mut settings = DEFAULT_COMPACTION_SETTINGS;
    settings.keep_recent_tokens = 1.0;
    prepare_compaction(&s.get_branch(None), &settings).unwrap()
}
fn texts(s: &SessionManager) -> Vec<String> {
    s.build_session_projection()
        .messages
        .iter()
        .map(|m| {
            observed(m)["summary"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| text(m))
        })
        .collect()
}
fn bookkeeping(s: &mut SessionManager) {
    s.append_custom_entry("bookkeeping", Some(value(json!({"source":"test"}))))
        .unwrap();
}
fn assert_excludes(p: &CompactionPreparation, needle: &str) {
    assert!(
        !observed(&p.messages_to_summarize)
            .to_string()
            .contains(needle)
    );
    assert!(
        !observed(&p.turn_prefix_messages)
            .to_string()
            .contains(needle)
    );
}
#[test]
fn omits_target_only_from_projection() {
    let mut s = memory();
    s.append_message(user("request")).unwrap();
    let a = s.append_message(response("partial")).unwrap();
    let r = result("raw output", true);
    let id = s.append_message(r.clone()).unwrap();
    s.append_context_edit(&a, JsValue::Null).unwrap();
    s.append_context_edit(&id, JsValue::Null).unwrap();
    assert_eq!(
        s.get_branch(None)
            .iter()
            .filter(|e| e.kind() == "message")
            .count(),
        3
    );
    assert_eq!(
        roles(&s.build_session_projection().messages),
        strings(&["user"])
    );
    assert!(s.get_entry(&id).unwrap().message().unwrap().ptr_eq(&r));
}
#[test]
fn replaces_only_content_latest_wins() {
    let mut s = memory();
    let id = s.append_message(response("original")).unwrap();
    s.append_context_edit(
        &id,
        value(json!({"content":[{"type":"text","text":"first"}]})),
    )
    .unwrap();
    s.append_context_edit(&id, JsValue::Null).unwrap();
    s.append_context_edit(
        &id,
        value(json!({"content":[{"type":"text","text":"restored"}]})),
    )
    .unwrap();
    let p = &s.build_session_projection().messages[0];
    assert_eq!(p.role(), "assistant");
    assert_eq!(text(p), "restored");
    assert_eq!(observed(p)["usage"]["totalTokens"], 11);
    assert_eq!(
        text(&s.get_entry(&id).unwrap().message().unwrap()),
        "original"
    );
}
#[test]
fn normalizes_string_replacements() {
    let mut s = memory();
    let a = s.append_message(response("original")).unwrap();
    let r = s.append_message(result("original result", false)).unwrap();
    let ae = s
        .append_context_edit(&a, content("assistant replacement"))
        .unwrap();
    let re = s
        .append_context_edit(&r, content("result replacement"))
        .unwrap();
    for (id, expected) in [(&ae, "assistant replacement"), (&re, "result replacement")] {
        assert_eq!(
            field(&s.get_entry(id).unwrap(), "replacement"),
            json!({"content":[{"type":"text","text":expected}]})
        );
    }
    let p = s.build_session_projection();
    for (i, role, expected) in [
        (0, "assistant", "assistant replacement"),
        (1, "toolResult", "result replacement"),
    ] {
        assert_eq!(p.messages[i].role(), role);
        assert_eq!(
            observed(&p.messages[i])["content"],
            json!([{"type":"text","text":expected}])
        );
    }
}
#[test]
fn normalizes_imported_string_replacements() {
    let mut s = memory();
    let a = s.append_message(response("original")).unwrap();
    let id = s.append_context_edit(&a, JsValue::Null).unwrap();
    let e = s.get_entry(&id).unwrap();
    assert_eq!(e.kind(), "context_edit");
    e.update(|raw| {
        raw.insert("replacement", content("imported replacement"));
    });
    let p = &s.build_session_projection().messages[0];
    assert_eq!(p.role(), "assistant");
    assert_eq!(
        observed(p)["content"],
        json!([{"type":"text","text":"imported replacement"}])
    );
}
#[test]
fn edits_are_branch_relative() {
    let mut s = memory();
    let id = s.append_message(user("original")).unwrap();
    s.append_context_edit(&id, content("edited")).unwrap();
    assert_eq!(text(&s.build_session_projection().messages[0]), "edited");
    s.branch(&id).unwrap();
    assert_eq!(text(&s.build_session_projection().messages[0]), "original");
}
#[test]
fn self_referencing_compaction_retains_none() {
    let mut s = memory();
    s.append_message(user("discarded")).unwrap();
    let id = s
        .append_compaction("exact handoff", None, 100.0, None, None, None)
        .unwrap();
    s.append_message(user("after")).unwrap();
    let e = s.get_entry(&id).unwrap();
    assert_eq!(e.kind(), "compaction");
    assert_eq!(field(&e, "firstKeptEntryId"), observed(&id));
    assert_eq!(
        roles(&s.build_session_projection().messages),
        strings(&["compactionSummary", "user"])
    );
    assert_eq!(texts(&s), ["exact handoff", "after"]);
}
#[test]
fn applies_edits_after_compaction_to_retained_entries() {
    let mut s = memory();
    s.append_message(user("summarized")).unwrap();
    let id = s.append_message(user("original retained")).unwrap();
    s.append_compaction("summary", Some(id.clone()), 100.0, None, None, None)
        .unwrap();
    s.append_context_edit(&id, content("edited retained"))
        .unwrap();
    assert_eq!(texts(&s), ["summary", "edited retained"]);
}
#[test]
fn newest_summary_when_retaining_before_previous_compaction() {
    let mut s = memory();
    s.append_message(user("summarized first")).unwrap();
    let id = s.append_message(user("retained")).unwrap();
    s.append_compaction("first summary", Some(id.clone()), 100.0, None, None, None)
        .unwrap();
    s.append_message(response("after first compaction"))
        .unwrap();
    s.append_compaction("second summary", Some(id), 80.0, None, None, None)
        .unwrap();
    s.append_message(user(&"new tail ".repeat(100))).unwrap();
    let summaries = s
        .build_session_projection()
        .messages
        .iter()
        .filter_map(|m| observed(m)["summary"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert_eq!(summaries, ["second summary"]);
    assert_eq!(
        prepare(&s).unwrap().previous_summary,
        Some("second summary".into())
    );
}
#[test]
fn repeated_retain_none_compactions() {
    let mut s = memory();
    s.append_message(user("discarded")).unwrap();
    s.append_compaction("first handoff", None, 100.0, None, None, None)
        .unwrap();
    s.append_message(user("also discarded")).unwrap();
    let id = s
        .append_compaction("second handoff", None, 50.0, None, None, None)
        .unwrap();
    assert_eq!(
        field(&s.get_entry(&id).unwrap(), "firstKeptEntryId"),
        observed(&id)
    );
    let summaries = s
        .build_session_projection()
        .messages
        .iter()
        .map(|m| observed(m)["summary"].as_str().unwrap_or("").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(summaries, ["second handoff"]);
}
#[test]
fn distrusts_pre_edit_usage() {
    let mut s = memory();
    let u = s
        .append_message(user(&"discarded input ".repeat(2000)))
        .unwrap();
    let a = s
        .append_message(response_usage("small answer", 10000, 1))
        .unwrap();
    s.append_context_edit(&u, JsValue::Null).unwrap();
    let e = estimate_projected_context_tokens(&s.build_session_projection(), &s.get_branch(None))
        .unwrap();
    assert_eq!(e.usage_tokens, 0.0);
    assert!(e.tokens < 100.0);
    s.append_context_edit(&a, JsValue::Null).unwrap();
    assert_eq!(
        estimate_projected_context_tokens(&s.build_session_projection(), &s.get_branch(None))
            .unwrap()
            .tokens,
        0.0
    );
}
#[test]
fn uses_usage_after_latest_edit() {
    let mut s = memory();
    let u = s.append_message(user("original")).unwrap();
    s.append_context_edit(&u, content("edited")).unwrap();
    s.append_message(response_usage("answer", 4000, 100))
        .unwrap();
    s.append_message(user("next")).unwrap();
    let e = estimate_projected_context_tokens(&s.build_session_projection(), &s.get_branch(None))
        .unwrap();
    assert_eq!(e.usage_tokens, 4100.0);
    assert_eq!(e.trailing_tokens, 1.0);
    assert_eq!(e.tokens, 4101.0);
}
#[test]
fn later_compaction_invalidates_post_edit_usage() {
    let mut s = memory();
    let u = s.append_message(user("small input")).unwrap();
    s.append_context_edit(&u, content("edited input")).unwrap();
    s.append_message(response_usage("answer", 50000, 1))
        .unwrap();
    s.append_compaction("small summary", Some(u), 50001.0, None, None, None)
        .unwrap();
    let e = estimate_projected_context_tokens(&s.build_session_projection(), &s.get_branch(None))
        .unwrap();
    assert_eq!(e.usage_tokens, 0.0);
    assert!(e.tokens < 100.0);
}
#[test]
fn estimates_include_system_and_tools() {
    let mut s = memory();
    s.append_message(json::<AgentMessage>(json!({"role":"system","content":"system prompt ".repeat(3000),"toolsAdded":[{"name":"example","description":"tool declaration ".repeat(100),"parameters":{"type":"object","properties":{}}}],"timestamp":NOW}))).unwrap();
    let u = s.append_message(user("ask")).unwrap();
    s.append_message(response("done")).unwrap();
    s.append_context_edit(&u, content("ask")).unwrap();
    assert!(
        estimate_projected_context_tokens(&s.build_session_projection(), &s.get_branch(None))
            .unwrap()
            .tokens
            > 10000.0
    );
}
#[test]
fn boundary_replacement_does_not_advance_cut() {
    let mut s = memory();
    s.append_message(user("old request")).unwrap();
    s.append_message(response("old answer")).unwrap();
    let u = s.append_message(user("original input")).unwrap();
    let a = s
        .append_message(response("answered original input"))
        .unwrap();
    s.append_context_edit(&u, content(&"NEW-INSTRUCTION ".repeat(100)))
        .unwrap();
    s.append_context_edit(&a, JsValue::Null).unwrap();
    bookkeeping(&mut s);
    let p = prepare(&s).unwrap();
    assert_eq!(p.first_kept_entry_id, u);
    assert_excludes(&p, "NEW-INSTRUCTION");
}
#[test]
fn metadata_does_not_move_cut_past_unsent_input() {
    let mut s = memory();
    s.append_message(user("old request")).unwrap();
    s.append_message(response("old answer")).unwrap();
    let id = s
        .append_custom_message_entry(
            "next-work",
            "UNSENT-INSTRUCTION ".repeat(100).into(),
            false,
            None,
        )
        .unwrap();
    bookkeeping(&mut s);
    let p = prepare(&s).unwrap();
    assert_eq!(p.first_kept_entry_id, id);
    assert_excludes(&p, "UNSENT-INSTRUCTION");
}
#[test]
fn omitted_custom_message_is_not_recovery_attempt() {
    let mut s = memory();
    s.append_message(user(&"unanswered input ".repeat(100)))
        .unwrap();
    let id = s
        .append_custom_message_entry("temporary", "temporary context".into(), false, None)
        .unwrap();
    s.append_context_edit(&id, JsValue::Null).unwrap();
    assert!(prepare(&s).is_none());
}
#[test]
fn omitted_assistant_recovery_advances_past_input() {
    let mut s = memory();
    let u = s
        .append_message(user(&"recovery input ".repeat(100)))
        .unwrap();
    let a = s.append_message(response("failed attempt")).unwrap();
    s.append_context_edit(&a, JsValue::Null).unwrap();
    bookkeeping(&mut s);
    let p = prepare(&s).unwrap();
    assert_eq!(p.first_kept_entry_id, a);
    assert_eq!(p.turn_prefix_messages.len(), 1);
    assert_eq!(p.turn_prefix_messages[0].role(), "user");
    assert!(text(&p.turn_prefix_messages[0]).contains("recovery input"));
    assert!(
        !p.messages_to_summarize
            .iter()
            .any(|m| m.role() == "user" && text(m).contains("recovery input"))
    );
    assert_ne!(u, a);
}
#[test]
fn prepares_compaction_from_edited_content() {
    let mut s = memory();
    let u = s.append_message(user(&"OMIT-ME ".repeat(100))).unwrap();
    s.append_message(response(&"old answer ".repeat(100)))
        .unwrap();
    s.append_context_edit(&u, JsValue::Null).unwrap();
    s.append_message(user("keep")).unwrap();
    s.append_message(response("suffix")).unwrap();
    assert_excludes(&prepare(&s).unwrap(), "OMIT-ME");
}
