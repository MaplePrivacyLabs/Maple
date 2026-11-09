//! Scripts that tests run. A fork in another test inherits every descriptor
//! open in this process until that child execs, and the kernel refuses to run
//! a file open for writing (ETXTBSY). So a child process writes the file, which
//! leaves nothing here for a fork to inherit.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

pub(super) fn write_executable(path: &Path, contents: &str) {
    let status = Command::new("/bin/sh")
        .args(["-c", r#"printf %s "$2" > "$1""#, "sh"])
        .arg(path)
        .arg(contents)
        .status()
        .expect("the shell should start");
    assert!(status.success(), "{} is not writable", path.display());
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
