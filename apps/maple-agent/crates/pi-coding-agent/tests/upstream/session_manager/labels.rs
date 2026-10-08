use super::super::session_support::*;
use serde_json::json;
#[test]
fn sets_and_gets_labels() {
    let mut s = memory();
    let id = s.append_message(user("hello")).unwrap();
    assert_eq!(s.get_label(&id), None);
    let lid = s
        .append_label_change(&id, Some("checkpoint".into()))
        .unwrap();
    assert_eq!(s.get_label(&id), Some("checkpoint".into()));
    let e = s
        .get_entries()
        .into_iter()
        .find(|e| e.kind() == "label")
        .unwrap();
    assert_eq!(e.id(), lid);
    assert_eq!(field(&e, "targetId"), observed(&id));
    assert_eq!(field(&e, "label"), "checkpoint");
}
#[test]
fn clears_labels_with_undefined() {
    let mut s = memory();
    let id = s.append_message(user("hello")).unwrap();
    s.append_label_change(&id, Some("checkpoint".into()))
        .unwrap();
    assert_eq!(s.get_label(&id), Some("checkpoint".into()));
    s.append_label_change(&id, None).unwrap();
    assert_eq!(s.get_label(&id), None);
}
#[test]
fn last_label_wins() {
    let mut s = memory();
    let id = s.append_message(user("hello")).unwrap();
    s.append_label_change(&id, Some("first".into())).unwrap();
    s.append_label_change(&id, Some("second".into())).unwrap();
    let last = s.append_label_change(&id, Some("third".into())).unwrap();
    assert_eq!(s.get_label(&id), Some("third".into()));
    assert_eq!(
        s.get_tree()
            .iter()
            .find(|n| n.entry.id() == id)
            .unwrap()
            .label_timestamp,
        Some(s.get_entry(&last).unwrap().timestamp())
    );
}
#[test]
fn labels_in_tree_nodes() {
    let mut s = memory();
    let a = s.append_message(user("hello")).unwrap();
    let b = s.append_message(assistant("hi")).unwrap();
    let la = s.append_label_change(&a, Some("start".into())).unwrap();
    let lb = s.append_label_change(&b, Some("response".into())).unwrap();
    let tree = s.get_tree();
    let na = tree.iter().find(|n| n.entry.id() == a).unwrap();
    assert_eq!(na.label, Some("start".into()));
    assert_eq!(
        na.label_timestamp,
        Some(s.get_entry(&la).unwrap().timestamp())
    );
    let nb = na.children.iter().find(|n| n.entry.id() == b).unwrap();
    assert_eq!(nb.label, Some("response".into()));
    assert_eq!(
        nb.label_timestamp,
        Some(s.get_entry(&lb).unwrap().timestamp())
    );
}
#[test]
fn labels_preserved_when_forking() {
    let mut s = memory();
    let a = s.append_message(user("hello")).unwrap();
    let b = s.append_message(assistant("hi")).unwrap();
    let la = s.append_label_change(&a, Some("important".into())).unwrap();
    let lb = s
        .append_label_change(&b, Some("also-important".into()))
        .unwrap();
    let ea = s.get_entry(&la).unwrap();
    let eb = s.get_entry(&lb).unwrap();
    s.create_branched_session(&b).unwrap();
    assert_eq!(s.get_label(&a), Some("important".into()));
    assert_eq!(s.get_label(&b), Some("also-important".into()));
    assert_eq!(
        s.get_entries()
            .iter()
            .filter(|e| e.kind() == "label")
            .count(),
        2
    );
    let tree = s.get_tree();
    let na = tree.iter().find(|n| n.entry.id() == a).unwrap();
    let nb = na.children.iter().find(|n| n.entry.id() == b).unwrap();
    assert_eq!(na.label_timestamp, Some(ea.timestamp()));
    assert_eq!(nb.label_timestamp, Some(eb.timestamp()));
}
#[test]
fn rewires_children_of_removed_labels() {
    let mut s = memory();
    let a = s.append_message(user("hello")).unwrap();
    s.append_label_change(&a, Some("checkpoint".into()))
        .unwrap();
    let model = s.append_model_change("anthropic", "claude-test").unwrap();
    let b = s.append_message(user_at("followup", 2)).unwrap();
    s.create_branched_session(&b).unwrap();
    assert_eq!(s.get_entry(&model).unwrap().parent_id(), Some(a));
}
#[test]
fn excludes_labels_outside_fork_path() {
    let mut s = memory();
    let a = s.append_message(user("hello")).unwrap();
    let b = s.append_message(assistant("hi")).unwrap();
    let c = s.append_message(user_at("followup", 3)).unwrap();
    for (id, label) in [(&a, "first"), (&b, "second"), (&c, "third")] {
        s.append_label_change(id, Some(label.into())).unwrap();
    }
    s.create_branched_session(&b).unwrap();
    assert_eq!(s.get_label(&a), Some("first".into()));
    assert_eq!(s.get_label(&b), Some("second".into()));
    assert_eq!(s.get_label(&c), None);
}
#[test]
fn labels_excluded_from_context() {
    let mut s = memory();
    let a = s.append_message(user("hello")).unwrap();
    s.append_label_change(&a, Some("checkpoint".into()))
        .unwrap();
    let c = s.build_session_context();
    assert_eq!(c.messages.len(), 1);
    assert_eq!(observed(&c.messages[0])["role"], json!("user"));
}
#[test]
fn rejects_missing_label_target() {
    let mut s = memory();
    assert!(
        s.append_label_change(&"non-existent".into(), Some("label".into()))
            .unwrap_err()
            .to_string()
            .contains("Entry non-existent not found")
    );
}

fn user_at(text: &str, timestamp: u64) -> pi_agent_core::types::AgentMessage {
    json(json!({"role":"user","content":text,"timestamp":timestamp}))
}
fn user(text: &str) -> pi_agent_core::types::AgentMessage {
    user_at(text, 1)
}
fn assistant(text: &str) -> pi_agent_core::types::AgentMessage {
    let mut raw = observed(&super::super::session_support::assistant(text));
    raw["timestamp"] = json!(2);
    json(raw)
}
