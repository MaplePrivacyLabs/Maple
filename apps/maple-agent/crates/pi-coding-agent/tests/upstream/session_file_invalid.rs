use super::session_support::env;
use pi_coding_agent::{config::HostConfig, core::session_manager::SessionManager};
use std::{fs, sync::Arc};

// Upstream: packages/coding-agent/test/session-file-invalid.test.ts > --session invalid file handling > prints a friendly error and preserves non-session file content
// The store Result boundary preserves the bare error and original file bytes.
// CLI-only presentation (the "Error: " prefix, exit code 1, and stack suppression)
// belongs to the excluded CLI and is not claimed by this adaptation.
#[test]
fn prints_a_friendly_error_and_preserves_non_session_file_content() {
    let temp = tempfile::Builder::new()
        .prefix("pi-session-file-invalid-")
        .tempdir()
        .unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let agent_dir = root.join("agent");
    let project_dir = root.join("project");
    let session_file = root.join("not-a-session.log");
    let original = "{\"type\":\"event\",\"data\":\"not a session\"}\n";
    fs::create_dir_all(&agent_dir).unwrap();
    fs::create_dir_all(&project_dir).unwrap();
    fs::write(&session_file, original).unwrap();
    let mut config = HostConfig::new("pi", ".pi", &root, &project_dir);
    config.agent_dir = Some(agent_dir);
    config.environment.insert("PI_OFFLINE".into(), "1".into());

    let Err(error) = SessionManager::open(
        session_file.to_str().unwrap(),
        None,
        None,
        env(),
        Arc::new(config),
    ) else {
        panic!("non-session file must reject");
    };

    assert_eq!(
        error.to_string(),
        format!(
            "Session file is not a valid pi session: {}",
            session_file.display()
        )
    );
    assert_eq!(fs::read(session_file).unwrap(), original.as_bytes());
}
