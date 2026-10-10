//! Shell commands the user runs (`!command`), after Pi's bash executor. The output is
//! cleaned for reading as it streams (no colour codes, control characters or carriage
//! returns), the result keeps its last 2000 lines or 50KB, and long output is also saved
//! whole to a file.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use regex::Regex;
use tokio_util::sync::CancellationToken;

use crate::tools::output::decode_available;
use crate::tools::{BashOperations, ExecError, ExecOptions, OnData, OutputAccumulator};

/// Gets a command's output, cleaned, as it comes.
pub type OnChunk = Arc<dyn Fn(&str) + Send + Sync>;

/// What a user shell command did.
#[derive(Clone, Debug, PartialEq)]
pub struct BashResult {
    /// Its output, cleaned, and cut to its last 2000 lines or 50KB.
    pub output: String,
    /// `None` when it was cancelled.
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    /// The whole output, kept when it was long.
    pub full_output_path: Option<PathBuf>,
}

/// Terminal escape sequences: OSC strings, and CSI and related sequences.
static ANSI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\x1B\][\s\S]*?(?:\x07|\x1B\\|\x{9C})",
        r"|[\x1B\x{9B}][\[\]()#;?]*(?:[0-9]{1,4}(?:[;:][0-9]{0,4})*)?[0-9A-PR-TZcf-nq-uy=><~]",
    ))
    .expect("pattern compiles")
});

/// Control characters other than tab and newline, carriage returns, and interlinear
/// annotation marks.
static CONTROL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\x00-\x08\x0B-\x1F\x{FFF9}-\x{FFFB}]").expect("pattern compiles")
});

/// `text` without colour codes, control characters (tabs and newlines stay) or carriage
/// returns.
pub fn sanitize_output(text: &str) -> String {
    let text = if text.contains(['\x1B', '\u{9B}']) {
        ANSI.replace_all(text, "")
    } else {
        Cow::Borrowed(text)
    };
    CONTROL.replace_all(&text, "").into_owned()
}

struct Collected {
    /// Bytes of a character split between chunks.
    pending: Vec<u8>,
    output: OutputAccumulator,
}

fn lock(collected: &Mutex<Collected>) -> std::sync::MutexGuard<'_, Collected> {
    collected.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run `command` in `cwd` through `operations` until it ends or `cancel` fires.
/// `on_chunk` gets the cleaned output as it comes; full-output files are named after
/// `file_prefix`. A command that could not run at all is an error.
pub async fn execute_bash_with_operations(
    command: &str,
    cwd: &Path,
    operations: &dyn BashOperations,
    file_prefix: &str,
    on_chunk: Option<OnChunk>,
    cancel: CancellationToken,
) -> Result<BashResult, String> {
    let collected = Arc::new(Mutex::new(Collected {
        pending: Vec::new(),
        output: OutputAccumulator::new(file_prefix),
    }));
    let on_data: OnData = {
        let collected = collected.clone();
        Arc::new(move |data: &[u8]| {
            let text = {
                let mut collected = lock(&collected);
                collected.pending.extend_from_slice(data);
                let text = sanitize_output(&decode_available(&mut collected.pending, false));
                collected.output.append(text.as_bytes());
                text
            };
            if let Some(on_chunk) = &on_chunk
                && !text.is_empty()
            {
                on_chunk(&text);
            }
        })
    };
    let result = operations
        .exec(
            command,
            cwd,
            ExecOptions {
                on_data,
                cancel: cancel.clone(),
                timeout: None,
                env: None,
                contain: false,
            },
        )
        .await;
    let exit_code = match result {
        Ok(code) => code,
        Err(_) if cancel.is_cancelled() => None,
        Err(ExecError::Aborted) => None,
        Err(ExecError::TimedOut(seconds)) => {
            return Err(format!("Command timed out after {seconds} seconds"));
        }
        Err(ExecError::Failed(message)) => return Err(message),
    };

    let mut collected = lock(&collected);
    let rest = sanitize_output(&decode_available(&mut collected.pending, true));
    collected.output.append(rest.as_bytes());
    collected.output.finish();
    let snapshot = collected.output.snapshot(true);
    // A full-output file that could not be written leaves the tail, which is kept.
    let _ = collected.output.close_file();
    let cancelled = cancel.is_cancelled();
    Ok(BashResult {
        output: snapshot.content,
        exit_code: if cancelled { None } else { exit_code },
        cancelled,
        truncated: snapshot.truncation.truncated,
        full_output_path: snapshot.full_output_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_cleaned_for_reading() {
        assert_eq!(sanitize_output("plain\ttext\n"), "plain\ttext\n");
        assert_eq!(
            sanitize_output("\x1B[1;31mred\x1B[0m and \x1B]8;;https://x\x07link\x1B]8;;\x07"),
            "red and link"
        );
        assert_eq!(sanitize_output("a\r\nb\x07\x00c\u{FFF9}"), "a\nbc");
    }
}
