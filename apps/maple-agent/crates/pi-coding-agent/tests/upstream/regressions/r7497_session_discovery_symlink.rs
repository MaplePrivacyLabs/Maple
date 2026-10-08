//! Pi v1.0.4 regression #7497: discover sessions through symlinked directories.
//! The source's stubbed PI_CODING_AGENT_DIR is an instance-local HostConfig entry.
use pi_ai::types::JsString;
use pi_coding_agent::{config::HostConfig, core::session_manager::SessionManager};
use std::{
    fs,
    path::{Path, PathBuf},
};

struct Fixture {
    temp: tempfile::TempDir,
    sessions_dir: PathBuf,
    config: HostConfig,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("pi-session-discovery-")
            .tempdir()
            .unwrap();
        let agent_dir = temp.path().join("agent");
        let sessions_dir = agent_dir.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let mut config = HostConfig::new(
            "pi",
            ".pi",
            temp.path().join("home"),
            temp.path().join("project"),
        );
        config
            .environment
            .insert(config.env_agent_dir(), text(&agent_dir));
        Self {
            temp,
            sessions_dir,
            config,
        }
    }
    fn root(&self) -> &Path {
        self.temp.path()
    }
    fn write_session(&self, dir: &Path, id: &str) {
        fs::create_dir_all(dir).unwrap();
        let header = serde_json::json!({"type":"session","version":3,"id":id,"timestamp":"2026-08-03T00:00:00.000Z","cwd":text(&self.root().join("project"))});
        fs::write(dir.join(format!("{id}.jsonl")), format!("{header}\n")).unwrap();
    }
}
fn text(path: &Path) -> String {
    path.to_str()
        .expect("fixture path is valid Unicode")
        .to_owned()
}
#[cfg(unix)]
fn directory_link(target: &Path, alias: &Path) {
    std::os::unix::fs::symlink(target, alias).unwrap();
}
#[cfg(windows)]
fn directory_link(target: &Path, alias: &Path) {
    std::os::windows::fs::symlink_dir(target, alias).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn discovers_a_session_through_a_directory_link_and_preserves_the_alias_path() {
    let fixture = Fixture::new();
    let target_dir = fixture.root().join("linked-sessions");
    fixture.write_session(&target_dir, "linked");
    let alias_dir = fixture.sessions_dir.join("--linked--");
    directory_link(&target_dir, &alias_dir);
    let sessions = SessionManager::list_all(None, None, None, &fixture.config)
        .await
        .unwrap();
    assert_eq!(
        sessions
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>(),
        vec![JsString::from("linked")]
    );
    assert_eq!(sessions[0].path, text(&alias_dir.join("linked.jsonl")));
}

#[tokio::test(flavor = "current_thread")]
async fn ignores_a_broken_directory_link_without_hiding_valid_sessions() {
    let fixture = Fixture::new();
    fixture.write_session(&fixture.sessions_dir.join("--regular--"), "regular");
    let target_dir = fixture.root().join("removed-sessions");
    fs::create_dir(&target_dir).unwrap();
    directory_link(&target_dir, &fixture.sessions_dir.join("--broken--"));
    fs::remove_dir_all(target_dir).unwrap();
    let sessions = SessionManager::list_all(None, None, None, &fixture.config)
        .await
        .unwrap();
    assert_eq!(
        sessions
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>(),
        vec![JsString::from("regular")]
    );
}

#[tokio::test(flavor = "current_thread")]
#[cfg_attr(windows, ignore = "upstream skipIf(win32)")]
async fn ignores_links_to_files() {
    let fixture = Fixture::new();
    fixture.write_session(&fixture.sessions_dir.join("--regular--"), "regular");
    let target_file = fixture.root().join("not-a-directory");
    fs::write(&target_file, "").unwrap();
    let alias = fixture.sessions_dir.join("--file--");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target_file, &alias).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&target_file, &alias).unwrap();
    let sessions = SessionManager::list_all(None, None, None, &fixture.config)
        .await
        .unwrap();
    assert_eq!(
        sessions
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>(),
        vec![JsString::from("regular")]
    );
}
