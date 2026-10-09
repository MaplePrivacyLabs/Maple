use super::probe::{
    LOGIN_SHELL_PATH_TIMEOUT, parse_login_shell_search_paths, query_login_shell_search_paths,
};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

const COMMAND_NAME: &str = "maple-login-path-fixture";
const RESTRICTED_GUI_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("fixture should be writable");
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).unwrap();
}

#[test]
fn parses_marked_path_and_ignores_banners_relative_and_duplicate_entries() {
    let paths = parse_login_shell_search_paths(
        b"profile banner\n__MAPLE_LOGIN_SHELL_PATH_V1__\n/custom/bin:relative:/usr/bin::/custom/bin:/bin\nlogout banner\n",
    )
    .unwrap();
    assert_eq!(paths, ["/custom/bin", "/usr/bin", "/bin"]);
}

#[tokio::test]
async fn the_recovered_path_finds_what_the_login_shell_finds() {
    let fixture = tempfile::tempdir().unwrap();
    let command_bin = fixture.path().join("command-bin");
    fs::create_dir_all(&command_bin).unwrap();
    let shell = fixture.path().join("fixture-shell");
    write_executable(
        &shell,
        r#"#!/bin/sh
test "$1" = "-l" || exit 41
test "$2" = "-i" || exit 42
test "$3" = "-c" || exit 43
export PATH="${0%/*}/command-bin:/usr/bin:/bin:/usr/sbin:/sbin"
printf 'profile banner\n'
/bin/sh -c "$4"
status=$?
printf 'logout banner\n'
exit "$status"
"#,
    );
    write_executable(
        &command_bin.join(COMMAND_NAME),
        "#!/bin/sh\nprintf 'found\\n'\n",
    );

    let missing = tokio::process::Command::new(COMMAND_NAME)
        .env("PATH", RESTRICTED_GUI_PATH)
        .output()
        .await;
    assert!(missing.is_err(), "the GUI-style PATH must not find it");

    let paths = query_login_shell_search_paths(&shell, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(paths[0], command_bin.to_string_lossy());
    let output = tokio::process::Command::new(COMMAND_NAME)
        .env("PATH", std::env::join_paths(paths.iter()).unwrap())
        .output()
        .await
        .expect("the recovered path finds the bare command");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "found\n");
}

#[tokio::test]
async fn a_hanging_login_shell_is_bounded() {
    let fixture = tempfile::tempdir().unwrap();
    let shell = fixture.path().join("hanging-shell");
    write_executable(&shell, "#!/bin/sh\n/bin/sleep 30\n");
    let started = Instant::now();
    let result = query_login_shell_search_paths(&shell, Duration::from_millis(50)).await;
    assert!(result.unwrap_err().contains("did not finish"));
    assert!(started.elapsed() < Duration::from_secs(2));
}

/// Whether `pid` is still running. A killed child waits as a zombie until
/// its new parent reaps it, which some container inits do late; a zombie
/// has stopped.
fn process_exists(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next());
        return state != Some("Z");
    }
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[tokio::test]
async fn a_child_holding_the_output_open_is_killed_at_the_deadline() {
    let fixture = tempfile::tempdir().unwrap();
    let shell = fixture.path().join("same-group-stdout-shell");
    let pid_file = fixture.path().join("same-group-child.pid");
    write_executable(
        &shell,
        r#"#!/bin/sh
/bin/sleep 30 &
printf '%s\n' "$!" > "${0%/*}/same-group-child.pid"
printf '%s\n' "$MAPLE_LOGIN_SHELL_PATH_MARKER" '/usr/bin:/bin:/usr/sbin:/sbin'
exit 0
"#,
    );
    let started = Instant::now();
    let result = query_login_shell_search_paths(&shell, LOGIN_SHELL_PATH_TIMEOUT).await;
    let elapsed = started.elapsed();
    let pid: u32 = fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut stopped = !process_exists(pid);
    for _ in 0..40 {
        if stopped {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        stopped = !process_exists(pid);
    }
    if !stopped {
        // SAFETY: the fixture's own child; clean it up before failing.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
    let error = result.unwrap_err();
    assert!(error.contains("output did not close"), "{error}");
    assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    assert!(stopped, "the child {pid} survived cleanup");
}
