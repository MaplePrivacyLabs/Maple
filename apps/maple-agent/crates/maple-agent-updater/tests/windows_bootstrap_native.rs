//! Native component harness for the app's real stable Windows bootstrap module.
//! This intentionally reuses the updater workspace lock/dependencies without
//! linking GPUI or claiming an officially signed A-to-B application upgrade.

#![cfg(windows)]

// App entrypoints are type-checked here but unit tests do not call every one.
#[allow(dead_code)]
#[path = "../../../app/src/windows_bootstrap.rs"]
mod windows_bootstrap;

#[test]
fn native_reparse_fixtures_are_available() {
    use std::os::windows::fs::{symlink_dir, symlink_file};

    let scratch = tempfile::tempdir().unwrap();
    let file = scratch.path().join("source");
    std::fs::write(&file, b"task-owned fixture").unwrap();
    let file_link = scratch.path().join("file-link");
    symlink_file(&file, &file_link)
        .expect("native reparse coverage requires Windows symlink privilege");
    assert!(std::fs::symlink_metadata(file_link).unwrap().is_symlink());
    let directory = scratch.path().join("directory");
    std::fs::create_dir(&directory).unwrap();
    let directory_link = scratch.path().join("directory-link");
    symlink_dir(&directory, &directory_link)
        .expect("native ancestor-reparse coverage requires Windows symlink privilege");
    assert!(
        std::fs::symlink_metadata(directory_link)
            .unwrap()
            .is_symlink()
    );
}
