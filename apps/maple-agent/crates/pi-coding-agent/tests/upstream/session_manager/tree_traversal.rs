use super::super::session_support::*;
use pi_coding_agent::core::session_manager::SessionManager;
use serde_json::json;
#[test]
fn append_message_parent_chain() {
    let mut s = memory();
    let a = s.append_message(user("first")).unwrap();
    let b = s.append_message(assistant("second")).unwrap();
    let c = s.append_message(user("third")).unwrap();
    let e = s.get_entries();
    assert_eq!(e.len(), 3);
    assert_eq!(e[0].id(), a);
    assert_eq!(e[0].parent_id(), None);
    assert_eq!(e[0].kind(), "message");
    assert_eq!(e[1].id(), b);
    assert_eq!(e[1].parent_id(), Some(a));
    assert_eq!(e[2].id(), c);
    assert_eq!(e[2].parent_id(), Some(b));
}
#[test]
fn append_thinking_integrates_into_tree() {
    let mut s = memory();
    let a = s.append_message(user("hello")).unwrap();
    let b = s.append_thinking_level_change("high").unwrap();
    s.append_message(assistant("response")).unwrap();
    let e = s.get_entries();
    assert_eq!(e.len(), 3);
    let t = e
        .iter()
        .find(|e| e.kind() == "thinking_level_change")
        .unwrap();
    assert_eq!(t.id(), b);
    assert_eq!(t.parent_id(), Some(a));
    assert_eq!(e[2].parent_id(), Some(b));
}
#[test]
fn append_model_integrates_into_tree() {
    let mut s = memory();
    let a = s.append_message(user("hello")).unwrap();
    let b = s.append_model_change("openai", "gpt-4").unwrap();
    s.append_message(assistant("response")).unwrap();
    let e = s.get_entries();
    let t = e.iter().find(|e| e.kind() == "model_change").unwrap();
    assert_eq!(t.id(), b);
    assert_eq!(t.parent_id(), Some(a));
    assert_eq!(field(t, "provider"), "openai");
    assert_eq!(field(t, "modelId"), "gpt-4");
    assert_eq!(e[2].parent_id(), Some(b));
}
#[test]
fn append_compaction_integrates_into_tree() {
    let mut s = memory();
    let a = s.append_message(user("1")).unwrap();
    let b = s.append_message(assistant("2")).unwrap();
    let c = s
        .append_compaction(
            "summary",
            Some(a.clone()),
            1000.0,
            None,
            Some(false),
            Some(usage()),
        )
        .unwrap();
    s.append_message(user("3")).unwrap();
    let e = s.get_entries();
    let t = e.iter().find(|e| e.kind() == "compaction").unwrap();
    assert_eq!(t.id(), c);
    assert_eq!(t.parent_id(), Some(b));
    assert_eq!(field(t, "summary"), "summary");
    assert_eq!(field(t, "firstKeptEntryId"), observed(&a));
    assert_eq!(field(t, "tokensBefore"), 1000);
    assert_eq!(field(t, "usage"), observed(&usage()));
    assert_eq!(e[3].parent_id(), Some(c));
}
#[test]
fn append_custom_integrates_into_tree() {
    let mut s = memory();
    let a = s.append_message(user("hello")).unwrap();
    let b = s
        .append_custom_entry("my_data", Some(value(json!({"key":"value"}))))
        .unwrap();
    s.append_message(assistant("response")).unwrap();
    let e = s.get_entries();
    let t = e.iter().find(|e| e.kind() == "custom").unwrap();
    assert_eq!(t.id(), b);
    assert_eq!(t.parent_id(), Some(a));
    assert_eq!(field(t, "customType"), "my_data");
    assert_eq!(field(t, "data"), json!({"key":"value"}));
    assert_eq!(e[2].parent_id(), Some(b));
}
#[test]
fn leaf_advances_after_each_append() {
    let mut s = memory();
    assert_eq!(s.get_leaf_id(), None);
    let a = s.append_message(user("1")).unwrap();
    assert_eq!(s.get_leaf_id(), Some(a));
    let b = s.append_message(assistant("2")).unwrap();
    assert_eq!(s.get_leaf_id(), Some(b));
    let c = s.append_thinking_level_change("high").unwrap();
    assert_eq!(s.get_leaf_id(), Some(c));
}
#[test]
fn empty_branch() {
    assert!(memory().get_branch(None).is_empty());
}
#[test]
fn single_entry_branch() {
    let mut s = memory();
    let id = s.append_message(user("hello")).unwrap();
    let p = s.get_branch(None);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].id(), id);
}
#[test]
fn full_root_to_leaf_branch() {
    let mut s = memory();
    let a = s.append_message(user("1")).unwrap();
    let b = s.append_message(assistant("2")).unwrap();
    let c = s.append_thinking_level_change("high").unwrap();
    let d = s.append_message(user("3")).unwrap();
    let p = s.get_branch(None);
    assert_eq!(p.len(), 4);
    assert_eq!(ids(&p), vec![a, b, c, d]);
}
#[test]
fn branch_to_specified_entry() {
    let mut s = memory();
    let a = s.append_message(user("1")).unwrap();
    let b = s.append_message(assistant("2")).unwrap();
    s.append_message(user("3")).unwrap();
    s.append_message(assistant("4")).unwrap();
    let p = s.get_branch(Some(&b));
    assert_eq!(p.len(), 2);
    assert_eq!(ids(&p), vec![a, b]);
}
#[test]
fn empty_tree() {
    assert!(memory().get_tree().is_empty());
}
#[test]
fn linear_tree_single_root() {
    let mut s = memory();
    let a = s.append_message(user("1")).unwrap();
    let b = s.append_message(assistant("2")).unwrap();
    let c = s.append_message(user("3")).unwrap();
    let tree = s.get_tree();
    assert_eq!(tree.len(), 1);
    let r = &tree[0];
    assert_eq!(r.entry.id(), a);
    assert_eq!(r.children.len(), 1);
    assert_eq!(r.children[0].entry.id(), b);
    assert_eq!(r.children[0].children.len(), 1);
    assert_eq!(r.children[0].children[0].entry.id(), c);
    assert!(r.children[0].children[0].children.is_empty());
}
#[test]
fn tree_has_sibling_branches() {
    let mut s = memory();
    let a = s.append_message(user("1")).unwrap();
    let b = s.append_message(assistant("2")).unwrap();
    let c = s.append_message(user("3")).unwrap();
    s.branch(&b).unwrap();
    let d = s.append_message(user("4-branch")).unwrap();
    let tree = s.get_tree();
    assert_eq!(tree.len(), 1);
    let r = &tree[0];
    assert_eq!(r.entry.id(), a);
    assert_eq!(r.children.len(), 1);
    let n = &r.children[0];
    assert_eq!(n.entry.id(), b);
    assert_eq!(n.children.len(), 2);
    let mut actual = n.children.iter().map(|n| n.entry.id()).collect::<Vec<_>>();
    actual.sort();
    let mut expected = vec![c, d];
    expected.sort();
    assert_eq!(actual, expected);
}
#[test]
fn multiple_branches_at_same_point() {
    let mut s = memory();
    s.append_message(user("root")).unwrap();
    let b = s.append_message(assistant("response")).unwrap();
    let mut expected = vec![];
    for text in ["branch-A", "branch-B", "branch-C"] {
        s.branch(&b).unwrap();
        expected.push(s.append_message(user(text)).unwrap());
    }
    let tree = s.get_tree();
    let n = &tree[0].children[0];
    assert_eq!(n.entry.id(), b);
    assert_eq!(n.children.len(), 3);
    let mut actual = n.children.iter().map(|n| n.entry.id()).collect::<Vec<_>>();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
}
#[test]
fn deep_branching() {
    let mut s = memory();
    s.append_message(user("1")).unwrap();
    let b = s.append_message(assistant("2")).unwrap();
    let c = s.append_message(user("3")).unwrap();
    s.append_message(assistant("4")).unwrap();
    s.branch(&b).unwrap();
    let e = s.append_message(user("5")).unwrap();
    s.append_message(assistant("6")).unwrap();
    s.branch(&e).unwrap();
    s.append_message(user("7")).unwrap();
    let tree = s.get_tree();
    let n = &tree[0].children[0];
    assert_eq!(n.children.len(), 2);
    assert_eq!(
        n.children
            .iter()
            .find(|n| n.entry.id() == e)
            .unwrap()
            .children
            .len(),
        2
    );
    assert_eq!(
        n.children
            .iter()
            .find(|n| n.entry.id() == c)
            .unwrap()
            .children
            .len(),
        1
    );
}
#[test]
fn branch_moves_leaf() {
    let mut s = memory();
    let a = s.append_message(user("1")).unwrap();
    s.append_message(assistant("2")).unwrap();
    let c = s.append_message(user("3")).unwrap();
    assert_eq!(s.get_leaf_id(), Some(c));
    s.branch(&a).unwrap();
    assert_eq!(s.get_leaf_id(), Some(a));
}
#[test]
fn branch_rejects_missing_entry() {
    let mut s = memory();
    s.append_message(user("hello")).unwrap();
    assert!(
        s.branch(&"nonexistent".into())
            .unwrap_err()
            .to_string()
            .contains("Entry nonexistent not found")
    );
}
#[test]
fn appends_become_branch_point_children() {
    let mut s = memory();
    let a = s.append_message(user("1")).unwrap();
    s.append_message(assistant("2")).unwrap();
    s.branch(&a).unwrap();
    let c = s.append_message(user("branched")).unwrap();
    assert_eq!(
        s.get_entries()
            .iter()
            .find(|e| e.id() == c)
            .unwrap()
            .parent_id(),
        Some(a)
    );
}
#[test]
fn summary_records_source_destination_and_advances_leaf() {
    let mut s = memory();
    let a = s.append_message(user("1")).unwrap();
    s.append_message(assistant("2")).unwrap();
    let c = s.append_message(user("3")).unwrap();
    let id = s
        .branch_with_summary(
            Some(a.clone()),
            "Summary of abandoned work",
            None,
            Some(false),
            Some(usage()),
        )
        .unwrap();
    assert_eq!(s.get_leaf_id(), Some(id));
    let e = s
        .get_entries()
        .into_iter()
        .find(|e| e.kind() == "branch_summary")
        .unwrap();
    assert_eq!(e.parent_id(), Some(a));
    assert_eq!(field(&e, "fromId"), observed(&c));
    assert_eq!(field(&e, "summary"), "Summary of abandoned work");
    assert_eq!(field(&e, "usage"), observed(&usage()));
}
#[test]
fn summary_rejects_missing_entry() {
    let mut s = memory();
    s.append_message(user("hello")).unwrap();
    assert!(
        s.branch_with_summary(Some("nonexistent".into()), "summary", None, None, None)
            .unwrap_err()
            .to_string()
            .contains("Entry nonexistent not found")
    );
}
#[test]
fn empty_leaf_entry() {
    assert!(memory().get_leaf_entry().is_none());
}
#[test]
fn returns_current_leaf_entry() {
    let mut s = memory();
    s.append_message(user("1")).unwrap();
    let b = s.append_message(assistant("2")).unwrap();
    assert_eq!(s.get_leaf_entry().unwrap().id(), b);
}
#[test]
fn missing_entry_is_none() {
    assert!(memory().get_entry(&"nonexistent".into()).is_none());
}
#[test]
fn returns_entry_by_id() {
    let mut s = memory();
    let a = s.append_message(user("first")).unwrap();
    let b = s.append_message(assistant("second")).unwrap();
    let first = s.get_entry(&a).unwrap();
    assert_eq!(first.kind(), "message");
    assert_eq!(first.message().unwrap().role(), "user");
    assert_eq!(text(&first.message().unwrap()), "first");
    let second = s.get_entry(&b).unwrap();
    assert_eq!(second.kind(), "message");
    assert_eq!(second.message().unwrap().role(), "assistant");
    assert_eq!(text(&second.message().unwrap()), "second");
}
#[test]
fn context_uses_current_branch_only() {
    let mut s = memory();
    s.append_message(user("msg1")).unwrap();
    let b = s.append_message(assistant("msg2")).unwrap();
    s.append_message(user("msg3")).unwrap();
    s.branch(&b).unwrap();
    s.append_message(assistant("msg4-branch")).unwrap();
    let c = s.build_session_context();
    assert_eq!(c.messages.len(), 3);
    assert_eq!(
        c.messages.iter().map(text).collect::<Vec<_>>(),
        ["msg1", "msg2", "msg4-branch"]
    );
}
#[test]
fn fork_rejects_missing_entry() {
    let mut s = memory();
    s.append_message(user("hello")).unwrap();
    assert!(
        s.create_branched_session(&"nonexistent".into())
            .unwrap_err()
            .to_string()
            .contains("Entry nonexistent not found")
    );
}
#[test]
fn fork_in_memory_extracts_path() {
    let mut s = memory();
    let a = s.append_message(user("1")).unwrap();
    let b = s.append_message(assistant("2")).unwrap();
    let c = s.append_message(user("3")).unwrap();
    s.append_message(assistant("4")).unwrap();
    s.branch(&c).unwrap();
    s.append_message(user("5")).unwrap();
    assert_eq!(s.create_branched_session(&b).unwrap(), None);
    let e = s.get_entries();
    assert_eq!(e.len(), 2);
    assert_eq!(e[0].id(), a);
    assert_eq!(e[1].id(), b);
}
#[test]
fn fork_extracts_correct_branched_path() {
    let mut s = memory();
    let a = s.append_message(user("1")).unwrap();
    let b = s.append_message(assistant("2")).unwrap();
    s.append_message(user("3")).unwrap();
    s.branch(&b).unwrap();
    let d = s.append_message(user("4")).unwrap();
    let e = s.append_message(assistant("5")).unwrap();
    s.create_branched_session(&e).unwrap();
    let entries = s.get_entries();
    assert_eq!(entries.len(), 4);
    assert_eq!(ids(&entries), vec![a, b, d, e]);
}
fn persisted(tmp: &tempfile::TempDir) -> SessionManager {
    let dir = tmp.path().to_str().unwrap();
    SessionManager::create(dir, Some(dir), None, env(), config(tmp.path())).unwrap()
}
#[test]
fn fork_before_user_does_not_duplicate_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = persisted(&tmp);
    let id = s
        .append_model_change("anthropic", "claude-sonnet-4-5")
        .unwrap();
    s.append_message(user("first question")).unwrap();
    s.append_message(assistant("first answer")).unwrap();
    let file = s.create_branched_session(&id).unwrap().unwrap();
    assert!(!std::path::Path::new(&file).exists());
    s.append_message(user("new question")).unwrap();
    assert!(std::path::Path::new(&file).exists());
    s.append_custom_entry("preset-state", Some(value(json!({"name":"plan"}))))
        .unwrap();
    s.append_message(assistant("new answer")).unwrap();
    assert_eq!(
        file_roles(file),
        ["session", "model_change", "user", "custom", "assistant"]
    );
}
#[test]
fn reload_preserves_tool_and_summary_usage() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = persisted(&tmp);
    let id = s.append_message(user("question")).unwrap();
    s.append_message(assistant("answer")).unwrap();
    s.append_message(json::<pi_agent_core::types::AgentMessage>(json!({"role":"toolResult","toolCallId":"call-1","toolName":"nested-model","content":[{"type":"text","text":"result"}],"isError":false,"usage":observed(&usage()),"timestamp":NOW}))).unwrap();
    s.append_compaction(
        "summary",
        Some(id.clone()),
        100.0,
        None,
        Some(false),
        Some(usage()),
    )
    .unwrap();
    s.branch_with_summary(Some(id), "branch summary", None, Some(false), Some(usage()))
        .unwrap();
    let file = s.get_session_file().unwrap();
    let reopened = SessionManager::open(
        &file,
        Some(tmp.path().to_str().unwrap()),
        None,
        env(),
        config(tmp.path()),
    )
    .unwrap();
    let entries = reopened.get_entries();
    for kind in ["compaction", "branch_summary"] {
        assert!(
            entries
                .iter()
                .any(|e| e.kind() == kind && field(e, "usage") == observed(&usage()))
        );
    }
    assert!(entries.iter().any(|e| e.kind() == "message"
        && field(e, "message")["role"] == "toolResult"
        && field(e, "message")["usage"] == observed(&usage())));
}
#[test]
fn fork_at_user_writes_immediately() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = persisted(&tmp);
    let id = s.append_message(user("first question")).unwrap();
    s.append_message(assistant("first answer")).unwrap();
    let file = s.create_branched_session(&id).unwrap().unwrap();
    assert!(std::path::Path::new(&file).exists());
    s.append_message(assistant("new answer")).unwrap();
    assert_eq!(file_roles(file), ["session", "user", "assistant"]);
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
