//! The file tools: `read`, `edit` and `write`.
//!
//! Each works on one regular file, resolved against the task's working
//! directory, and refuses anything else (a FIFO or a device would block a
//! read forever). Edits and writes to one file are serialized, so two calls
//! in a parallel tool batch cannot interleave their writes.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use once_cell::sync::Lazy;
use serde::{Deserialize, Deserializer, de::Error as SerdeDeError};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub(super) const MAX_READ_LINES: usize = 2_000;
pub(super) const MAX_READ_BYTES: usize = 50 * 1024;
pub(super) const MAX_EDIT_BYTES: usize = 20 * 1024 * 1024;

type MutationLock = Mutex<()>;
type MutationLockMap = HashMap<PathBuf, Weak<MutationLock>>;

static MUTATION_LOCKS: Lazy<StdMutex<MutationLockMap>> =
    Lazy::new(|| StdMutex::new(HashMap::new()));

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadParams {
    pub(super) path: String,
    pub(super) offset: Option<usize>,
    pub(super) limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Replacement {
    old_text: String,
    new_text: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EditParams {
    pub(super) path: String,
    #[serde(deserialize_with = "deserialize_edits")]
    edits: Vec<Replacement>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WriteParams {
    pub(super) path: String,
    content: String,
}

/// `edits` as an array, or as the JSON text of one: some models send the
/// array as a string.
fn deserialize_edits<'de, D>(deserializer: D) -> Result<Vec<Replacement>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Array(_) => serde_json::from_value(value).map_err(D::Error::custom),
        serde_json::Value::String(text) => serde_json::from_str(&text).map_err(D::Error::custom),
        _ => Err(D::Error::custom("edits must be an array")),
    }
}

pub(super) fn resolve_path(path: &str, working_dir: Option<&Path>) -> PathBuf {
    let expanded = if path == "~" {
        home_dir().unwrap_or_else(|| PathBuf::from(path))
    } else if let Some(relative) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        home_dir()
            .map(|home| home.join(relative))
            .unwrap_or_else(|| PathBuf::from(path))
    } else {
        PathBuf::from(path)
    };

    if expanded.is_absolute() {
        expanded
    } else {
        working_dir
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
            .join(expanded)
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn regular_file_error(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("{} is not a regular file", path.display()),
    )
}

fn open_regular_file_for_read(path: &Path) -> std::io::Result<fs::File> {
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?
    };
    #[cfg(not(unix))]
    let file = {
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(regular_file_error(path));
        }
        OpenOptions::new().read(true).open(path)?
    };

    if !file.metadata()?.is_file() {
        return Err(regular_file_error(path));
    }
    Ok(file)
}

fn open_regular_file_for_write(path: &Path) -> std::io::Result<fs::File> {
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .create(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?
    };
    #[cfg(not(unix))]
    let file = {
        if let Ok(metadata) = fs::metadata(path) {
            if !metadata.is_file() {
                return Err(regular_file_error(path));
            }
        }
        OpenOptions::new().write(true).create(true).open(path)?
    };

    if !file.metadata()?.is_file() {
        return Err(regular_file_error(path));
    }
    Ok(file)
}

fn open_regular_file_for_edit(path: &Path) -> std::io::Result<fs::File> {
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?
    };
    #[cfg(not(unix))]
    let file = {
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(regular_file_error(path));
        }
        OpenOptions::new().read(true).write(true).open(path)?
    };

    if !file.metadata()?.is_file() {
        return Err(regular_file_error(path));
    }
    Ok(file)
}

pub(super) async fn read_file(
    params: ReadParams,
    working_dir: Option<&Path>,
    cancel_token: CancellationToken,
) -> Result<String, String> {
    if params.offset == Some(0) {
        return Err("offset must be at least 1".to_string());
    }
    if params.limit == Some(0) {
        return Err("limit must be at least 1".to_string());
    }

    let path = resolve_path(&params.path, working_dir);
    let worker_cancel_token = cancel_token.clone();
    let task =
        tokio::task::spawn_blocking(move || read_file_blocking(params, path, worker_cancel_token));
    tokio::select! {
        biased;
        _ = cancel_token.cancelled() => Err("Read cancelled".to_string()),
        result = task => match result {
            Ok(result) => result,
            Err(error) => Err(format!("Read task failed: {error}")),
        },
    }
}

fn read_file_blocking(
    params: ReadParams,
    path: PathBuf,
    cancel_token: CancellationToken,
) -> Result<String, String> {
    if cancel_token.is_cancelled() {
        return Err("Read cancelled".to_string());
    }

    let mut file = match open_regular_file_for_read(&path) {
        Ok(file) => file,
        Err(error) => return Err(format!("Failed to read {}: {error}", params.path)),
    };

    let mut magic = [0u8; 12];
    let magic_len = match file.read(&mut magic) {
        Ok(length) => length,
        Err(error) => return Err(format!("Failed to read {}: {error}", params.path)),
    };
    if is_supported_image(&magic[..magic_len]) {
        return Ok(format!(
            "{} is an image. Use read_image to inspect it.",
            params.path
        ));
    }
    if let Err(error) = file.seek(SeekFrom::Start(0)) {
        return Err(format!("Failed to read {}: {error}", params.path));
    }

    let mut reader = BufReader::new(file);
    let start = params.offset.unwrap_or(1) - 1;
    for lines_seen in 0..start {
        match read_stream_line(&mut reader, None, &cancel_token) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(format!(
                    "Offset {} is beyond end of file ({lines_seen} lines total)",
                    params.offset.unwrap_or(1)
                ));
            }
            Err(error) => return Err(stream_read_error(&params.path, error)),
        }
    }

    let line_limit = params.limit.unwrap_or(usize::MAX).min(MAX_READ_LINES);
    let mut output_lines = Vec::new();
    let mut output_bytes = 0usize;
    let mut has_more = false;
    let mut first_selected_line = true;

    while output_lines.len() < line_limit {
        let separator_bytes = usize::from(!output_lines.is_empty());
        let Some(remaining_bytes) = MAX_READ_BYTES.checked_sub(output_bytes + separator_bytes)
        else {
            has_more = match read_stream_line(&mut reader, Some(0), &cancel_token) {
                Ok(line) => line.is_some(),
                Err(error) => return Err(stream_read_error(&params.path, error)),
            };
            break;
        };
        let line = match read_stream_line(&mut reader, Some(remaining_bytes), &cancel_token) {
            Ok(Some(line)) => line,
            Ok(None) if first_selected_line && start > 0 => {
                return Err(format!(
                    "Offset {} is beyond end of file ({start} lines total)",
                    params.offset.unwrap_or(1)
                ));
            }
            Ok(None) => break,
            Err(error) => return Err(stream_read_error(&params.path, error)),
        };
        first_selected_line = false;

        let text = String::from_utf8_lossy(&line.bytes).into_owned();
        if line.exceeded_limit || text.len() > remaining_bytes {
            if output_lines.is_empty() {
                let mut notice = format!(
                    "[Line {} exceeds the {}KB read limit. Use shell with a byte-limiting command to inspect it.",
                    start + 1,
                    MAX_READ_BYTES / 1024
                );
                // The model would otherwise have no way to move past the
                // long line; tell it the next offset when one exists.
                match line_follows(&mut reader, line.exceeded_limit, &cancel_token) {
                    Ok(true) => notice.push_str(&format!(" Use offset={} to continue.", start + 2)),
                    Ok(false) => {}
                    Err(error) => return Err(stream_read_error(&params.path, error)),
                }
                notice.push(']');
                return Ok(notice);
            }
            has_more = true;
            break;
        }

        output_bytes += separator_bytes + text.len();
        output_lines.push(text);
    }

    if !has_more && output_lines.len() == line_limit {
        has_more = match read_stream_line(&mut reader, Some(0), &cancel_token) {
            Ok(line) => line.is_some(),
            Err(error) => return Err(stream_read_error(&params.path, error)),
        };
    }

    let mut output = output_lines.join("\n");
    if has_more {
        let first_line = start + 1;
        let last_line = start + output_lines.len();
        let next_offset = last_line + 1;
        output.push_str(&format!(
            "\n\n[Showing lines {first_line}-{last_line}. Use offset={next_offset} to continue.]"
        ));
    }

    Ok(output)
}

struct StreamedLine {
    bytes: Vec<u8>,
    exceeded_limit: bool,
}

/// Whether any data follows the current line. With `skip_current_line`
/// the rest of a partially consumed line is discarded first.
fn line_follows<R: BufRead>(
    reader: &mut R,
    skip_current_line: bool,
    cancel_token: &CancellationToken,
) -> Result<bool, StreamReadError> {
    if skip_current_line {
        loop {
            if cancel_token.is_cancelled() {
                return Err(StreamReadError::Cancelled);
            }
            let (consumed, ended) = {
                let available = reader.fill_buf().map_err(StreamReadError::Io)?;
                if available.is_empty() {
                    return Ok(false);
                }
                match available.iter().position(|byte| *byte == b'\n') {
                    Some(newline) => (newline + 1, true),
                    None => (available.len(), false),
                }
            };
            reader.consume(consumed);
            if ended {
                break;
            }
        }
    }
    Ok(!reader.fill_buf().map_err(StreamReadError::Io)?.is_empty())
}

enum StreamReadError {
    Cancelled,
    Io(std::io::Error),
}

fn read_stream_line<R: BufRead>(
    reader: &mut R,
    capture_limit: Option<usize>,
    cancel_token: &CancellationToken,
) -> Result<Option<StreamedLine>, StreamReadError> {
    let mut bytes = Vec::new();
    let mut saw_any = false;

    loop {
        if cancel_token.is_cancelled() {
            return Err(StreamReadError::Cancelled);
        }

        let (consumed, ended, exceeded_limit) = {
            let available = reader.fill_buf().map_err(StreamReadError::Io)?;
            if available.is_empty() {
                if !saw_any {
                    return Ok(None);
                }
                if bytes.last() == Some(&b'\r') {
                    bytes.pop();
                }
                return Ok(Some(StreamedLine {
                    bytes,
                    exceeded_limit: false,
                }));
            }
            saw_any = true;

            let newline = available.iter().position(|byte| *byte == b'\n');
            let segment_len = newline.unwrap_or(available.len());
            let mut exceeded_limit = false;
            let mut captured = 0usize;
            if let Some(limit) = capture_limit {
                let remaining = limit.saturating_sub(bytes.len());
                captured = remaining.min(segment_len);
                bytes.extend_from_slice(&available[..captured]);
                exceeded_limit = segment_len > remaining;
            }

            if exceeded_limit {
                ((captured + 1).min(segment_len), false, true)
            } else {
                (
                    segment_len + usize::from(newline.is_some()),
                    newline.is_some(),
                    false,
                )
            }
        };
        reader.consume(consumed);

        if exceeded_limit {
            return Ok(Some(StreamedLine {
                bytes,
                exceeded_limit: true,
            }));
        }
        if ended {
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            return Ok(Some(StreamedLine {
                bytes,
                exceeded_limit: false,
            }));
        }
    }
}

fn stream_read_error(path: &str, error: StreamReadError) -> String {
    match error {
        StreamReadError::Cancelled => "Read cancelled".to_string(),
        StreamReadError::Io(error) => format!("Failed to read {path}: {error}"),
    }
}

pub(super) fn is_supported_image(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(&[0xff, 0xd8, 0xff])
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP")
}

pub(super) async fn write_file(
    params: WriteParams,
    working_dir: Option<&Path>,
    cancel_token: CancellationToken,
) -> Result<String, String> {
    let path = resolve_path(&params.path, working_dir);
    let lock = mutation_lock(&path);
    let guard = tokio::select! {
        biased;
        _ = cancel_token.cancelled() => return Err("Write cancelled".to_string()),
        guard = lock.lock_owned() => guard,
    };
    let worker_cancel_token = cancel_token.clone();
    match tokio::task::spawn_blocking(move || {
        let _guard = guard;
        write_file_blocking(params, path, worker_cancel_token)
    })
    .await
    {
        Ok(result) => result,
        Err(error) => Err(format!("Write task failed: {error}")),
    }
}

fn write_file_blocking(
    params: WriteParams,
    path: PathBuf,
    cancel_token: CancellationToken,
) -> Result<String, String> {
    if cancel_token.is_cancelled() {
        return Err("Write cancelled".to_string());
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(error) = fs::create_dir_all(parent)
    {
        return Err(format!(
            "Failed to create directory {}: {error}",
            parent.display()
        ));
    }
    if cancel_token.is_cancelled() {
        return Err("Write cancelled".to_string());
    }

    let existed = path.exists();
    let mut file = match open_regular_file_for_write(&path) {
        Ok(file) => file,
        Err(error) => return Err(format!("Failed to write {}: {error}", params.path)),
    };
    if cancel_token.is_cancelled() {
        return Err("Write cancelled".to_string());
    }
    if let Err(error) = file
        .set_len(0)
        .and_then(|_| file.seek(SeekFrom::Start(0)).map(|_| ()))
        .and_then(|_| file.write_all(params.content.as_bytes()))
    {
        return Err(format!("Failed to write {}: {error}", params.path));
    }

    let action = if existed { "Wrote" } else { "Created" };
    Ok(format!(
        "{action} {} ({} bytes)",
        params.path,
        params.content.len()
    ))
}

pub(super) async fn edit_file(
    params: EditParams,
    working_dir: Option<&Path>,
    cancel_token: CancellationToken,
) -> Result<String, String> {
    if params.edits.is_empty() {
        return Err("edits must contain at least one replacement".to_string());
    }

    let path = resolve_path(&params.path, working_dir);
    let lock = mutation_lock(&path);
    let guard = tokio::select! {
        biased;
        _ = cancel_token.cancelled() => return Err("Edit cancelled".to_string()),
        guard = lock.lock_owned() => guard,
    };
    let worker_cancel_token = cancel_token.clone();
    let task = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        edit_file_blocking(params, path, worker_cancel_token)
    });
    tokio::select! {
        biased;
        _ = cancel_token.cancelled() => Err("Edit cancelled".to_string()),
        result = task => match result {
            Ok(result) => result,
            Err(error) => Err(format!("Edit task failed: {error}")),
        },
    }
}

fn edit_file_blocking(
    params: EditParams,
    path: PathBuf,
    cancel_token: CancellationToken,
) -> Result<String, String> {
    if cancel_token.is_cancelled() {
        return Err("Edit cancelled".to_string());
    }
    let mut file = match open_regular_file_for_edit(&path) {
        Ok(file) => file,
        Err(error) => return Err(format!("Failed to read {}: {error}", params.path)),
    };
    let len = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) => return Err(format!("Failed to inspect {}: {error}", params.path)),
    };
    if len > MAX_EDIT_BYTES as u64 {
        return Err(format!(
            "{} is too large to edit safely: {len} bytes exceeds the {MAX_EDIT_BYTES} byte limit",
            params.path
        ));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        if cancel_token.is_cancelled() {
            return Err("Edit cancelled".to_string());
        }
        let read = match file.read(&mut chunk) {
            Ok(read) => read,
            Err(error) => {
                return Err(format!("Failed to read {}: {error}", params.path));
            }
        };
        if read == 0 {
            break;
        }
        if bytes.len().saturating_add(read) > MAX_EDIT_BYTES {
            return Err(format!(
                "{} grew beyond the {MAX_EDIT_BYTES} byte edit limit while being read",
                params.path
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    let original = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(_) => return Err(format!("{} is not a UTF-8 text file", params.path)),
    };

    let (bom, line_ending, mut normalized) = match normalize_text_file(&original) {
        Ok(file) => file,
        Err(error) => return Err(format!("Cannot edit {}: {error}", params.path)),
    };
    let mut resolved_edits = Vec::with_capacity(params.edits.len());
    let mut has_change = false;
    for (index, replacement) in params.edits.iter().enumerate() {
        if cancel_token.is_cancelled() {
            return Err("Edit cancelled".to_string());
        }
        let old_text = normalize_newlines(&replacement.old_text);
        let new_text = normalize_newlines(&replacement.new_text);
        if old_text.is_empty() {
            return Err(format!("edits[{index}].oldText must not be empty"));
        }

        let matches = overlapping_match_positions(&normalized, &old_text);
        match matches.as_slice() {
            [] => {
                return Err(format!(
                    "edits[{index}].oldText was not found in {}",
                    params.path
                ));
            }
            [start] => {
                has_change |= old_text != new_text;
                resolved_edits.push((*start, *start + old_text.len(), new_text));
            }
            _ => {
                return Err(format!(
                    "edits[{index}].oldText matched more than once; include more context so it is unique"
                ));
            }
        }
    }
    if !has_change {
        return Err("edits would not change the file".to_string());
    }

    resolved_edits.sort_by_key(|(start, _, _)| *start);
    for pair in resolved_edits.windows(2) {
        if pair[1].0 < pair[0].1 {
            return Err("edits contain overlapping replacements".to_string());
        }
    }

    let updated_len =
        resolved_edits
            .iter()
            .try_fold(normalized.len(), |len, (start, end, replacement)| {
                len.checked_sub(end - start)?.checked_add(replacement.len())
            });
    if updated_len.is_none_or(|len| len > MAX_EDIT_BYTES) {
        return Err(format!(
            "Edited content would exceed the {MAX_EDIT_BYTES} byte edit limit"
        ));
    }

    for (start, end, replacement) in resolved_edits.iter().rev() {
        normalized.replace_range(*start..*end, replacement);
    }

    if cancel_token.is_cancelled() {
        return Err("Edit cancelled".to_string());
    }
    let updated = restore_text_file(&normalized, bom, line_ending);
    if updated.len() > MAX_EDIT_BYTES {
        return Err(format!(
            "Edited content would exceed the {MAX_EDIT_BYTES} byte edit limit"
        ));
    }
    if cancel_token.is_cancelled() {
        return Err("Edit cancelled".to_string());
    }
    if let Err(error) = file
        .set_len(0)
        .and_then(|_| file.seek(SeekFrom::Start(0)).map(|_| ()))
        .and_then(|_| file.write_all(updated.as_bytes()))
    {
        return Err(format!("Failed to write {}: {error}", params.path));
    }
    Ok(format!(
        "Edited {} ({} replacements)",
        params.path,
        resolved_edits.len()
    ))
}

#[derive(Clone, Copy)]
enum LineEnding {
    Lf,
    CrLf,
    Cr,
}

fn normalize_text_file(content: &str) -> Result<(bool, LineEnding, String), &'static str> {
    let (bom, content) = match content.strip_prefix('\u{feff}') {
        Some(content) => (true, content),
        None => (false, content),
    };
    let mut saw_lf = false;
    let mut saw_crlf = false;
    let mut saw_cr = false;
    let bytes = content.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                saw_crlf = true;
                index += 2;
            }
            b'\r' => {
                saw_cr = true;
                index += 1;
            }
            b'\n' => {
                saw_lf = true;
                index += 1;
            }
            _ => index += 1,
        }
    }
    if usize::from(saw_lf) + usize::from(saw_crlf) + usize::from(saw_cr) > 1 {
        return Err(
            "mixed line endings are not supported because editing could rewrite untouched lines",
        );
    }
    let line_ending = if saw_crlf {
        LineEnding::CrLf
    } else if saw_cr {
        LineEnding::Cr
    } else {
        LineEnding::Lf
    };
    Ok((bom, line_ending, normalize_newlines(content)))
}

fn normalize_newlines(content: &str) -> String {
    content.replace("\r\n", "\n").replace('\r', "\n")
}

fn restore_text_file(content: &str, bom: bool, line_ending: LineEnding) -> String {
    let content = match line_ending {
        LineEnding::Lf => content.to_string(),
        LineEnding::CrLf => content.replace('\n', "\r\n"),
        LineEnding::Cr => content.replace('\n', "\r"),
    };
    if bom {
        format!("\u{feff}{content}")
    } else {
        content
    }
}

fn overlapping_match_positions(haystack: &str, needle: &str) -> Vec<usize> {
    let mut positions = Vec::with_capacity(2);
    let mut search_start = 0usize;
    while search_start <= haystack.len() {
        let Some(relative) = haystack[search_start..].find(needle) else {
            break;
        };
        let position = search_start + relative;
        positions.push(position);
        if positions.len() == 2 {
            break;
        }
        let advance = haystack[position..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
        search_start = position + advance;
    }
    positions
}

fn mutation_lock(path: &Path) -> Arc<MutationLock> {
    let key = mutation_key(path);
    let mut locks = MUTATION_LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }

    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

fn mutation_key(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }
    if let (Some(parent), Some(file_name)) = (path.parent(), path.file_name())
        && let Ok(canonical_parent) = fs::canonicalize(parent)
    {
        return canonical_parent.join(file_name);
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests;
