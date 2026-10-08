use super::super::session_support::*;
use pi_coding_agent::core::session_manager::{
    SessionContext, SessionEntry, build_context_entries, build_session_context,
};
use serde_json::json;
fn msg(id: &str, parent: Option<&str>, role: &str, text: &str) -> SessionEntry {
    let mut message = observed(&if role == "user" {
        user(text)
    } else {
        assistant(text)
    });
    message["timestamp"] = json!(1);
    if role == "assistant" {
        message["model"] = json!("claude-test");
    }
    json(
        json!({"type":"message","id":id,"parentId":parent,"timestamp":"2025-01-01T00:00:00Z","message":message}),
    )
}
fn special(id: &str, parent: &str, kind: &str, extra: serde_json::Value) -> SessionEntry {
    let mut raw = json!({"type":kind,"id":id,"parentId":parent,"timestamp":"2025-01-01T00:00:00Z"});
    raw.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    json(raw)
}
fn compact(id: &str, parent: &str, summary: &str, kept: &str) -> SessionEntry {
    special(
        id,
        parent,
        "compaction",
        json!({"summary":summary,"firstKeptEntryId":kept,"tokensBefore":1000}),
    )
}
fn summary(id: &str, parent: &str, text: &str, from: &str) -> SessionEntry {
    special(
        id,
        parent,
        "branch_summary",
        json!({"summary":text,"fromId":from}),
    )
}
fn context(entries: &[SessionEntry]) -> SessionContext {
    build_session_context(entries, None, None)
}
fn leaf(entries: &[SessionEntry], id: &str) -> SessionContext {
    build_session_context(entries, Some(Some(&id.into())), None)
}
fn assert_summary(message: &pi_agent_core::types::AgentMessage, expected: &str) {
    assert!(
        observed(message)["summary"]
            .as_str()
            .unwrap()
            .contains(expected)
    );
}
#[test]
fn empty_entries_returns_empty_context() {
    let c = context(&[]);
    assert!(c.messages.is_empty());
    assert_eq!(c.thinking_level, "off");
    assert!(c.model.is_none());
}
#[test]
fn single_user_message() {
    let c = context(&[msg("1", None, "user", "hello")]);
    assert_eq!(c.messages.len(), 1);
    assert_eq!(c.messages[0].role(), "user");
}
#[test]
fn simple_conversation() {
    let c = context(&[
        msg("1", None, "user", "hello"),
        msg("2", Some("1"), "assistant", "hi there"),
        msg("3", Some("2"), "user", "how are you"),
        msg("4", Some("3"), "assistant", "great"),
    ]);
    assert_eq!(c.messages.len(), 4);
    assert_eq!(
        roles(&c.messages),
        strings(&["user", "assistant", "user", "assistant"])
    );
}
#[test]
fn tracks_thinking_level_changes() {
    let c = context(&[
        msg("1", None, "user", "hello"),
        special(
            "2",
            "1",
            "thinking_level_change",
            json!({"thinkingLevel":"high"}),
        ),
        msg("3", Some("2"), "assistant", "thinking hard"),
    ]);
    assert_eq!(c.thinking_level, "high");
    assert_eq!(c.messages.len(), 2);
}
#[test]
fn tracks_model_from_assistant_message() {
    let c = context(&[
        msg("1", None, "user", "hello"),
        msg("2", Some("1"), "assistant", "hi"),
    ]);
    assert_eq!(
        observed(&c.model),
        json!({"provider":"anthropic","modelId":"claude-test"})
    );
}
#[test]
fn tracks_model_from_model_change_entry() {
    let c = context(&[
        msg("1", None, "user", "hello"),
        special(
            "2",
            "1",
            "model_change",
            json!({"provider":"openai","modelId":"gpt-4"}),
        ),
        msg("3", Some("2"), "assistant", "hi"),
    ]);
    assert_eq!(
        observed(&c.model),
        json!({"provider":"anthropic","modelId":"claude-test"})
    );
}
#[test]
fn includes_summary_before_kept_messages() {
    let c = context(&[
        msg("1", None, "user", "first"),
        msg("2", Some("1"), "assistant", "response1"),
        msg("3", Some("2"), "user", "second"),
        msg("4", Some("3"), "assistant", "response2"),
        compact("5", "4", "Summary of first two turns", "3"),
        msg("6", Some("5"), "user", "third"),
        msg("7", Some("6"), "assistant", "response3"),
    ]);
    assert_eq!(c.messages.len(), 5);
    assert_summary(&c.messages[0], "Summary of first two turns");
    assert_eq!(
        c.messages[1..].iter().map(text).collect::<Vec<_>>(),
        ["second", "response2", "third", "response3"]
    );
}
#[test]
fn handles_compaction_keeping_from_first_message() {
    let c = context(&[
        msg("1", None, "user", "first"),
        msg("2", Some("1"), "assistant", "response"),
        compact("3", "2", "Empty summary", "1"),
        msg("4", Some("3"), "user", "second"),
    ]);
    assert_eq!(c.messages.len(), 4);
    assert_summary(&c.messages[0], "Empty summary");
}
#[test]
fn multiple_compactions_uses_latest() {
    let c = context(&[
        msg("1", None, "user", "a"),
        msg("2", Some("1"), "assistant", "b"),
        compact("3", "2", "First summary", "1"),
        msg("4", Some("3"), "user", "c"),
        msg("5", Some("4"), "assistant", "d"),
        compact("6", "5", "Second summary", "4"),
        msg("7", Some("6"), "user", "e"),
    ]);
    assert_eq!(c.messages.len(), 4);
    assert_summary(&c.messages[0], "Second summary");
}
#[test]
fn context_entries_includes_custom_entries() {
    let e = [
        msg("1", None, "user", "first"),
        special(
            "2",
            "1",
            "custom",
            json!({"customType":"old-state","data":{"hidden":true}}),
        ),
        msg("3", Some("2"), "assistant", "response1"),
        special(
            "4",
            "3",
            "custom",
            json!({"customType":"kept-card","data":{"title":"Kept"}}),
        ),
        msg("5", Some("4"), "user", "second"),
        compact("6", "5", "Summary", "4"),
        special(
            "7",
            "6",
            "custom",
            json!({"customType":"after-card","data":{"title":"After"}}),
        ),
        msg("8", Some("7"), "assistant", "response2"),
    ];
    assert_eq!(
        ids(&build_context_entries(&e, None, None)),
        strings(&["6", "4", "5", "7", "8"])
    );
    assert_eq!(
        roles(&context(&e).messages),
        strings(&["compactionSummary", "user", "assistant"])
    );
}
#[test]
fn keeps_settings_after_compaction() {
    let c = context(&[
        msg("1", None, "user", "first"),
        special(
            "2",
            "1",
            "thinking_level_change",
            json!({"thinkingLevel":"high"}),
        ),
        msg("3", Some("2"), "assistant", "response1"),
        msg("4", Some("3"), "user", "second"),
        compact("5", "4", "Summary", "4"),
    ]);
    assert_eq!(c.thinking_level, "high");
    assert_eq!(roles(&c.messages), strings(&["compactionSummary", "user"]));
}
#[test]
fn follows_path_to_specified_leaf() {
    let e = [
        msg("1", None, "user", "start"),
        msg("2", Some("1"), "assistant", "response"),
        msg("3", Some("2"), "user", "branch A"),
        msg("4", Some("2"), "user", "branch B"),
    ];
    for (id, expected) in [("3", "branch A"), ("4", "branch B")] {
        let c = leaf(&e, id);
        assert_eq!(c.messages.len(), 3);
        assert_eq!(text(&c.messages[2]), expected);
    }
}
#[test]
fn includes_branch_summary_in_path() {
    let c = leaf(
        &[
            msg("1", None, "user", "start"),
            msg("2", Some("1"), "assistant", "response"),
            msg("3", Some("2"), "user", "abandoned path"),
            summary("4", "2", "Summary of abandoned work", "3"),
            msg("5", Some("4"), "user", "new direction"),
        ],
        "5",
    );
    assert_eq!(c.messages.len(), 4);
    assert_summary(&c.messages[2], "Summary of abandoned work");
    assert_eq!(text(&c.messages[3]), "new direction");
}
#[test]
fn complex_tree_with_branches_and_compaction() {
    let e = [
        msg("1", None, "user", "start"),
        msg("2", Some("1"), "assistant", "r1"),
        msg("3", Some("2"), "user", "q2"),
        msg("4", Some("3"), "assistant", "r2"),
        compact("5", "4", "Compacted history", "3"),
        msg("6", Some("5"), "user", "q3"),
        msg("7", Some("6"), "assistant", "r3"),
        msg("8", Some("3"), "user", "wrong path"),
        msg("9", Some("8"), "assistant", "wrong response"),
        summary("10", "3", "Tried wrong approach", "9"),
        msg("11", Some("10"), "user", "better approach"),
    ];
    let c = leaf(&e, "7");
    assert_eq!(c.messages.len(), 5);
    assert_summary(&c.messages[0], "Compacted history");
    assert_eq!(
        c.messages[1..].iter().map(text).collect::<Vec<_>>(),
        ["q2", "r2", "q3", "r3"]
    );
    let c = leaf(&e, "11");
    assert_eq!(c.messages.len(), 5);
    assert_eq!(
        c.messages[..3].iter().map(text).collect::<Vec<_>>(),
        ["start", "r1", "q2"]
    );
    assert_summary(&c.messages[3], "Tried wrong approach");
    assert_eq!(text(&c.messages[4]), "better approach");
}
#[test]
fn uses_last_entry_when_leaf_missing() {
    assert_eq!(
        leaf(
            &[
                msg("1", None, "user", "hello"),
                msg("2", Some("1"), "assistant", "hi")
            ],
            "nonexistent"
        )
        .messages
        .len(),
        2
    );
}
#[test]
fn handles_orphaned_entries() {
    assert_eq!(
        leaf(
            &[
                msg("1", None, "user", "hello"),
                msg("2", Some("missing"), "assistant", "orphan")
            ],
            "2"
        )
        .messages
        .len(),
        1
    );
}

// These source assertions distinguish user string content from assistant blocks.
fn text(message: &pi_agent_core::types::AgentMessage) -> String {
    let raw = observed(message);
    if message.role() == "user" {
        raw["content"]
            .as_str()
            .expect("upstream user content is a string")
            .to_owned()
    } else {
        raw["content"][0]["text"]
            .as_str()
            .expect("upstream assistant first text block")
            .to_owned()
    }
}
