use super::super::session_support::*;
use pi_coding_agent::core::session_manager::{FileEntry, migrate_session_entries};
use serde_json::json;
fn entries(version: Option<u32>) -> Vec<FileEntry> {
    let mut raw = json!([{"type":"session","id":"sess-1","timestamp":"2025-01-01T00:00:00Z","cwd":"/tmp"},{"type":"message","timestamp":"2025-01-01T00:00:01Z","message":{"role":"user","content":"hi","timestamp":1}},{"type":"message","timestamp":"2025-01-01T00:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"hello"}],"api":"test","provider":"test","model":"test","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0},"stopReason":"stop","timestamp":2}}]);
    if let Some(v) = version {
        raw[0]["version"] = json!(v);
        raw[1]["id"] = json!("abc12345");
        raw[1]["parentId"] = json!(null);
        raw[2]["id"] = json!("def67890");
        raw[2]["parentId"] = json!("abc12345");
    }
    json(raw)
}
#[test]
fn adds_ids_and_parents_to_v1() {
    let mut e = entries(None);
    migrate_session_entries(&mut e, env().as_ref()).unwrap();
    assert_eq!(field(&e[0], "version"), 3);
    assert_eq!(e[1].id().utf16_len(), 8);
    assert_eq!(e[1].parent_id(), None);
    assert_eq!(e[2].id().utf16_len(), 8);
    assert_eq!(e[2].parent_id(), Some(e[1].id()));
}
#[test]
fn idempotent_preserves_existing_ids() {
    let mut e = entries(Some(2));
    migrate_session_entries(&mut e, env().as_ref()).unwrap();
    assert_eq!(e[1].id(), "abc12345");
    assert_eq!(e[2].id(), "def67890");
    assert_eq!(e[2].parent_id(), Some("abc12345".into()));
}
