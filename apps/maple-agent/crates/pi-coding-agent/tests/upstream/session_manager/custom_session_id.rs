use super::super::session_support::*;
use pi_coding_agent::core::session_manager::{NewSessionOptions, SessionManager};
use serde_json::json;
fn options(id: &str) -> NewSessionOptions {
    NewSessionOptions {
        id: Some(id.into()),
        ..Default::default()
    }
}
fn assert_file_name(path: &str, id: &str) {
    let name = std::path::Path::new(path)
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();
    let suffix = format!("_{id}.jsonl");
    assert!(name.ends_with(&suffix));
    let stamp = &name[..name.len() - suffix.len()];
    assert_eq!(stamp.len(), 24);
    for (i, c) in stamp.chars().enumerate() {
        match i {
            4 | 7 | 13 | 16 | 19 => assert_eq!(c, '-'),
            10 => assert_eq!(c, 'T'),
            23 => assert_eq!(c, 'Z'),
            _ => assert!(c.is_ascii_digit()),
        }
    }
}
#[test]
fn uses_provided_id_for_new_session() {
    let mut s = memory();
    s.new_session(Some(options("my-custom-id"))).unwrap();
    assert_eq!(s.get_session_id(), "my-custom-id");
}
#[test]
fn uses_provided_id_in_memory() {
    let s = memory_with(Some(options("memory-session-id")), None);
    assert_eq!(s.get_session_id(), "memory-session-id");
    assert_eq!(s.get_header().unwrap().id(), "memory-session-id");
    assert_eq!(s.get_session_file(), None);
}
#[test]
fn allows_interior_punctuation() {
    let mut s = memory();
    s.new_session(Some(options("abc-123_def.456"))).unwrap();
    assert_eq!(s.get_session_id(), "abc-123_def.456");
}
#[test]
fn rejects_invalid_custom_ids() {
    for id in [
        "", "-abc", "abc-", "_abc", "abc_", ".abc", "abc.", "abc/def", "abc\\def", "abc def",
    ] {
        let mut s = memory();
        assert!(
            s.new_session(Some(options(id)))
                .unwrap_err()
                .to_string()
                .contains("Session id must be non-empty, contain only alphanumeric characters")
        );
    }
}
#[test]
fn generates_uuid_without_id() {
    let mut s = memory();
    s.new_session(None).unwrap();
    let id = s.get_session_id();
    assert!(!id.is_empty());
    assert_uuid_v7(&id);
}
#[test]
fn generates_uuid_with_options_without_id() {
    let mut s = memory();
    s.new_session(Some(NewSessionOptions {
        id: None,
        parent_session: Some("parent.jsonl".into()),
    }))
    .unwrap();
    let id = s.get_session_id();
    assert!(!id.is_empty());
    assert_uuid_v7(&id);
}
#[test]
fn includes_custom_id_in_header() {
    let mut s = memory();
    s.new_session(Some(options("header-test-id"))).unwrap();
    assert_eq!(s.get_header().unwrap().id(), "header-test-id");
}
#[test]
fn construction_generates_uuid() {
    let s = memory();
    assert_uuid_v7(&s.get_session_id());
    assert_eq!(s.get_header().unwrap().id(), s.get_session_id());
}
#[test]
fn uses_provided_id_persisted() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_str().unwrap();
    let s = SessionManager::create(
        dir,
        Some(dir),
        Some(options("created-session-id")),
        env(),
        config(tmp.path()),
    )
    .unwrap();
    assert_eq!(s.get_session_id(), "created-session-id");
    assert_eq!(s.get_header().unwrap().id(), "created-session-id");
    let file = s.get_session_file().unwrap();
    assert!(file.contains("created-session-id"));
    assert_file_name(&file, "created-session-id");
    assert!(!std::path::Path::new(&file).exists());
}
#[test]
fn branch_generates_uuid() {
    let mut s = memory();
    let id = s.append_message(user_parts("hello")).unwrap();
    s.create_branched_session(&id).unwrap();
    assert_uuid_v7(&s.get_session_id());
    assert_eq!(s.get_header().unwrap().id(), s.get_session_id());
}
fn source_file(root: &std::path::Path, id: &str, with_message: bool) -> std::path::PathBuf {
    let path = root.join("source.jsonl");
    let mut content=serde_json::to_string(&json!({"type":"session","version":3,"id":id,"timestamp":"2026-01-01T00:00:00.000Z","cwd":root})).unwrap()+"\n";
    if with_message {
        content+=&serde_json::to_string(&json!({"type":"message","id":"entry-1","parentId":null,"timestamp":"2026-01-01T00:00:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hello"}],"api":"openai-responses","provider":"openai","model":"gpt-5.4","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":NOW}})).unwrap();
        content.push('\n');
    }
    std::fs::write(&path, content).unwrap();
    path
}
#[test]
fn fork_file_generates_uuid() {
    let tmp = tempfile::tempdir().unwrap();
    let path = source_file(tmp.path(), "legacy-session-id", true);
    let dir = tmp.path().to_str().unwrap();
    let fork = SessionManager::fork_from(
        path.to_str().unwrap(),
        dir,
        Some(dir),
        None,
        env(),
        config(tmp.path()),
    )
    .unwrap();
    let h = fork.get_header().unwrap();
    assert_uuid_v7(&h.id());
    assert_eq!(field(&h, "parentSession"), json!(path));
}
#[test]
fn fork_file_uses_provided_id() {
    let tmp = tempfile::tempdir().unwrap();
    let path = source_file(tmp.path(), "source-session-id", false);
    let dir = tmp.path().to_str().unwrap();
    let fork = SessionManager::fork_from(
        path.to_str().unwrap(),
        dir,
        Some(dir),
        Some(options("forked-session-id")),
        env(),
        config(tmp.path()),
    )
    .unwrap();
    let h = fork.get_header().unwrap();
    assert_eq!(h.id(), "forked-session-id");
    assert_eq!(field(&h, "parentSession"), json!(path));
    let file = fork.get_session_file().unwrap();
    assert!(file.contains("forked-session-id"));
    assert_file_name(&file, "forked-session-id");
}
