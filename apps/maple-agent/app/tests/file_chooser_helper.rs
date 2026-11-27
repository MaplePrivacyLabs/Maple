//! Exact-executable checks of the Linux file-chooser helper. No account,
//! backend, or GUI is needed; the live test also needs a desktop portal.
#![cfg(all(target_os = "linux", feature = "desktop"))]

use std::{process::Stdio, time::Duration};

use tokio::{process::Command, time::timeout};

const BINARY: &str = env!("CARGO_BIN_EXE_maple-agent");
const DEADLINE: Duration = Duration::from_secs(5);

fn helper(request: [&str; 4]) -> Command {
    let mut command = Command::new(BINARY);
    command
        .env_clear()
        .current_dir("/")
        .arg("--internal-file-chooser")
        .args(request)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn unreachable_portal_is_reported_without_opening_application_state() {
    let state = tempfile::tempdir().unwrap();
    let mut child = helper(["files", "", "Open Files", ""])
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent-maple-file-chooser-test-bus",
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
    assert!(output.status.success());
    assert!(output.stdout.starts_with(b"failed\n"), "{output:?}");
    assert_eq!(
        output.stdout.iter().filter(|&&byte| byte == b'\n').count(),
        2
    );
    assert!(output.stderr.is_empty());
    assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn malformed_requests_exit_before_any_portal_call() {
    for request in [
        ["directory", "", "Open", ""],
        ["files", "x11:not-hex", "Open", ""],
        ["files", "wayland:", "Open", ""],
    ] {
        let output = timeout(DEADLINE, helper(request).output())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{request:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, b"Desktop file chooser unavailable.\n");
    }
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
            .contains("internal-file-chooser")
    );
    for arguments in [
        vec!["--build-info"],
        vec!["--internal-theme-watch"],
        vec!["host", "serve"],
        vec!["--internal-store-integrity-check", "/nonexistent-store"],
        vec!["unexpected-desktop-argument"],
    ] {
        let output = timeout(
            DEADLINE,
            helper(["files", "", "Open", ""]).args(arguments).output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
    // Every field is required, even when empty.
    let output = timeout(
        DEADLINE,
        Command::new(BINARY)
            .env_clear()
            .args(["--internal-file-chooser", "files", "", "Open"])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

/// Run from a desktop session. The portal accepts the request and shows a
/// dialog, which stays open until the parent pipe closes.
#[tokio::test]
#[ignore = "requires a live desktop FileChooser portal; shows a dialog"]
async fn live_portal_accepts_the_helper_and_closes_with_its_parent() {
    let state = tempfile::tempdir().unwrap();
    let mut command = helper(["files", "", "Maple file chooser test", ""]);
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
    // A refused request answers at once; an accepted one waits on the person.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        child.try_wait().unwrap().is_none(),
        "the portal answered at once"
    );
    let pid = child.id().unwrap();
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
    let output = timeout(DEADLINE, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 0);
}
