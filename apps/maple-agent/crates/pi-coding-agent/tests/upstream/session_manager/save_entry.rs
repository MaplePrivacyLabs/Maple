use super::super::session_support::*;
use serde_json::json;
#[test]
fn appends_custom_entries_with_tree_structure() {
    let mut s = memory();
    let a = s
        .append_message(json::<pi_agent_core::types::AgentMessage>(
            json!({"role":"user","content":"hello","timestamp":1}),
        ))
        .unwrap();
    let b = s
        .append_custom_entry("my_data", Some(value(json!({"foo":"bar"}))))
        .unwrap();
    let mut message = observed(&assistant("hi"));
    message["timestamp"] = json!(2);
    let c = s
        .append_message(json::<pi_agent_core::types::AgentMessage>(message))
        .unwrap();
    let entries = s.get_entries();
    assert_eq!(entries.len(), 3);
    let custom = entries.iter().find(|e| e.kind() == "custom").unwrap();
    assert_eq!(field(custom, "customType"), "my_data");
    assert_eq!(field(custom, "data"), json!({"foo":"bar"}));
    assert_eq!(custom.id(), b);
    assert_eq!(custom.parent_id(), Some(a.clone()));
    assert_eq!(ids(&s.get_branch(None)), vec![a, b, c]);
    assert_eq!(s.build_session_context().messages.len(), 2);
}
