use super::super::session_support::*;
use pi_coding_agent::core::session_manager::{NewSessionOptions, SessionEntry};
use serde_json::json;
fn options(id: &str) -> NewSessionOptions {
    NewSessionOptions {
        id: Some(id.into()),
        ..Default::default()
    }
}
fn stored(
    build: impl FnOnce(&mut pi_coding_agent::core::session_manager::SessionManager),
) -> Vec<SessionEntry> {
    let mut s = memory();
    build(&mut s);
    s.get_entries()
}
#[test]
fn adopts_entries_verbatim() {
    let e = stored(|s| {
        s.append_message(user_parts("hello")).unwrap();
        s.append_model_change("anthropic", "claude-opus-4-5")
            .unwrap();
        s.append_message(user_parts("again")).unwrap();
    });
    let s = memory_with(None, Some(e.clone()));
    assert_eq!(s.get_entries(), e);
}
#[test]
fn continues_from_loaded_leaf() {
    let e = stored(|s| {
        s.append_message(user_parts("hello")).unwrap();
        s.append_message(user_parts("again")).unwrap();
    });
    let last = e.last().unwrap().id();
    let mut s = memory_with(None, Some(e));
    let id = s.append_message(user_parts("continued")).unwrap();
    assert_eq!(s.get_leaf_id(), Some(id.clone()));
    assert_eq!(s.get_entry(&id).unwrap().parent_id(), Some(last));
}
#[test]
fn never_collides_with_loaded_ids() {
    let e = stored(|s| {
        for i in 0..50 {
            s.append_message(user_parts(&format!("message {i}")))
                .unwrap();
        }
    });
    let mut s = memory_with(None, Some(e.clone()));
    let id = s.append_message(user_parts("continued")).unwrap();
    assert!(!e.iter().any(|e| e.id() == id));
}
#[test]
fn restores_branch_structure() {
    let e = stored(|s| {
        let a = s.append_message(user_parts("hello")).unwrap();
        s.append_message(user_parts("abandoned")).unwrap();
        s.branch(&a).unwrap();
        s.append_message(user_parts("kept")).unwrap();
    });
    let s = memory_with(None, Some(e));
    let roots = s.get_tree();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].children.len(), 2);
}
#[test]
fn rebuilds_labels() {
    let mut id = Default::default();
    let e = stored(|s| {
        id = s.append_message(user_parts("hello")).unwrap();
        s.append_label_change(&id, Some("checkpoint".into()))
            .unwrap();
    });
    assert_eq!(
        memory_with(None, Some(e)).get_label(&id),
        Some("checkpoint".into())
    );
}
#[test]
fn resolves_compaction_against_original_entry() {
    let mut id = Default::default();
    let e = stored(|s| {
        s.append_message(user_parts("dropped")).unwrap();
        id = s.append_message(user_parts("kept")).unwrap();
        s.append_compaction("summary so far", Some(id.clone()), 1000.0, None, None, None)
            .unwrap();
    });
    assert!(
        memory_with(None, Some(e))
            .build_context_entries()
            .iter()
            .any(|e| e.id() == id)
    );
}
#[test]
fn creates_header_from_options() {
    let e = stored(|s| {
        s.append_message(user_parts("hello")).unwrap();
    });
    let s = memory_with(Some(options("restored-session")), Some(e));
    assert_eq!(s.get_session_id(), "restored-session");
    let h = s.get_header().unwrap();
    assert_eq!(h.id(), "restored-session");
    assert_eq!(field(&h, "cwd"), "/project");
}
#[test]
fn generates_session_id() {
    let e = stored(|s| {
        s.append_message(user_parts("hello")).unwrap();
    });
    let s = memory_with(None, Some(e));
    assert_uuid_v7(&s.get_session_id());
    assert_eq!(s.get_header().unwrap().id(), s.get_session_id());
}
#[test]
fn stays_off_filesystem() {
    let e = stored(|s| {
        s.append_message(user_parts("hello")).unwrap();
    });
    let mut s = memory_with(None, Some(e));
    s.append_message(user_parts("continued")).unwrap();
    assert_eq!(s.get_session_file(), None);
    assert!(!s.is_persisted());
}
#[test]
fn starts_empty_session() {
    let s = memory_with(Some(options("empty-session")), Some(vec![]));
    assert_eq!(s.get_session_id(), "empty-session");
    assert!(s.get_entries().is_empty());
    assert_eq!(s.get_leaf_id(), None);
}
#[test]
fn adopts_header_identity() {
    let mut e = vec![json(
        json!({"type":"session","version":3,"id":"stored-session","timestamp":"2026-01-01T00:00:00Z","cwd":"/stored"}),
    )];
    e.extend(stored(|s| {
        s.append_message(user_parts("hello")).unwrap();
    }));
    let s = memory_with(Some(options("ignored")), Some(e));
    assert_eq!(s.get_session_id(), "stored-session");
    assert_eq!(field(&s.get_header().unwrap(), "cwd"), "/stored");
}
fn hook() -> SessionEntry {
    json(
        json!({"type":"message","id":"abc12345","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"hookMessage","content":"from a hook","timestamp":1}}),
    )
}
#[test]
fn migrates_entries_with_old_header() {
    let s = memory_with(
        None,
        Some(vec![
            json(
                json!({"type":"session","version":2,"id":"v2-session","timestamp":"2026-01-01T00:00:00Z","cwd":"/project"}),
            ),
            hook(),
        ]),
    );
    let restored = &s.get_entries()[0];
    assert_eq!(field(&s.get_header().unwrap(), "version"), 3);
    assert_eq!(restored.message().unwrap().role(), "custom");
    assert_eq!(restored.id(), "abc12345");
}
#[test]
fn preserves_headerless_entries_as_current_version() {
    let s = memory_with(None, Some(vec![hook()]));
    assert_eq!(s.get_entries()[0].message().unwrap().role(), "hookMessage");
}
