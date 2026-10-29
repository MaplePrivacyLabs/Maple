//! Exact-executable checks. No account, backend, or GUI is needed.
#![cfg(all(target_os = "linux", feature = "desktop"))]

use std::{process::Stdio, time::Duration};

use tokio::{io::AsyncReadExt as _, process::Command, time::timeout};

const BINARY: &str = env!("CARGO_BIN_EXE_maple-agent");
const DEADLINE: Duration = Duration::from_secs(5);

fn helper() -> Command {
    let mut command = Command::new(BINARY);
    command
        .env_clear()
        .current_dir("/")
        .arg("--internal-theme-watch")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn helper_failure_never_opens_application_state() {
    let state = tempfile::tempdir().unwrap();
    let mut child = helper()
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent-maple-theme-test-bus",
        )
        .env("XDG_DATA_HOME", state.path().join("data"))
        .env("XDG_CONFIG_HOME", state.path().join("config"))
        .spawn()
        .unwrap();
    let _lifetime = child.stdin.take().unwrap();
    let output = timeout(DEADLINE, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"Desktop theme watcher unavailable.\n");
    assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn internal_mode_is_hidden_and_cannot_start_other_modes() {
    let output = timeout(
        DEADLINE,
        Command::new(BINARY).env_clear().arg("--help").output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(output.status.success());
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("internal-theme-watch")
    );
    for arguments in [
        vec!["--build-info"],
        vec!["host", "serve"],
        vec!["--internal-store-integrity-check", "/nonexistent-store"],
        vec!["unexpected-desktop-argument"],
    ] {
        let output = timeout(DEADLINE, helper().args(arguments).output())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
}

#[tokio::test]
#[ignore = "requires a live desktop Settings portal"]
async fn live_portal_helper_exits_when_parent_pipe_closes() {
    let state = tempfile::tempdir().unwrap();
    let mut command = helper();
    for key in ["DBUS_SESSION_BUS_ADDRESS", "XDG_RUNTIME_DIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    // Canary roots prove this startup branch never initializes application state.
    command
        .env("XDG_DATA_HOME", state.path().join("data"))
        .env("XDG_CONFIG_HOME", state.path().join("config"));
    let mut child = command.spawn().unwrap();
    let lifetime = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    let mut frame = Vec::new();
    timeout(DEADLINE, async {
        for _ in 0..8 {
            let byte = output.read_u8().await.unwrap();
            frame.push(byte);
            if byte == b'\n' {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        frame.as_slice(),
        b"dark\n" | b"light\n" | b"default\n"
    ));
    // It must be a watcher, not a one-shot read.
    assert!(child.try_wait().unwrap().is_none());
    let pid = child.id().unwrap();
    // Portal identity needs /proc access, but ordinary core dumps remain off.
    assert!(std::fs::read_link(format!("/proc/{pid}/root")).is_ok());
    let limits = std::fs::read_to_string(format!("/proc/{pid}/limits")).unwrap();
    let core = limits
        .lines()
        .find(|line| line.starts_with("Max core file size"))
        .unwrap();
    assert_eq!(
        core.split_whitespace().skip(4).take(2).collect::<Vec<_>>(),
        ["0", "0"]
    );
    drop(lifetime);
    assert!(
        timeout(DEADLINE, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 0);
}
