//! Read a bounded amount of output from a short-lived child process.
//!
//! Maple probes local helper executables in a few places. Each probe must cap
//! how much it will read so a hostile or broken child cannot exhaust memory.
//! Keeping one implementation means a fix to that cap, or to the way a partial
//! read is reported, applies everywhere.

use tokio::io::AsyncReadExt;

/// Read at most `max_bytes` from `stdout`, or fail if the child produced more.
///
/// `subject` names what is being read and appears in every error, so a caller
/// does not have to wrap the result to make it legible.
pub(super) async fn read_bounded_stdout(
    stdout: tokio::process::ChildStdout,
    max_bytes: usize,
    subject: &str,
) -> Result<Vec<u8>, String> {
    let mut stdout = stdout.take((max_bytes as u64).saturating_add(1));
    let mut bytes = Vec::new();
    stdout
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("could not read {subject}: {error}"))?;
    if bytes.len() > max_bytes {
        return Err(format!("{subject} exceeds the {max_bytes}-byte limit"));
    }
    Ok(bytes)
}
