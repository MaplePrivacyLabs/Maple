//! Streaming command output with bounded memory.
//!
//! Chunks are decoded as UTF-8 as they arrive (a character split across chunks is put
//! back together), only a decoded tail is kept for display, and once the output passes
//! the limits all of it goes to an output file the model can read.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use super::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationResult, truncate_tail,
};

/// A view of the output so far: its kept tail, how it was cut, and where the full
/// output is when there is a file.
#[derive(Clone, Debug)]
pub struct OutputSnapshot {
    pub content: String,
    pub truncation: TruncationResult,
    pub full_output_path: Option<PathBuf>,
}

pub struct OutputAccumulator {
    max_lines: usize,
    max_bytes: usize,
    max_rolling_bytes: usize,
    file_prefix: String,
    /// Bytes of an incomplete character at the end of the last chunk.
    pending: Vec<u8>,
    raw_chunks: Vec<Vec<u8>>,
    tail_text: String,
    tail_starts_at_line_boundary: bool,
    total_raw_bytes: usize,
    total_decoded_bytes: usize,
    completed_lines: usize,
    total_lines: usize,
    current_line_bytes: usize,
    has_open_line: bool,
    finished: bool,
    file_path: Option<PathBuf>,
    file: Option<BufWriter<File>>,
    file_error: Option<String>,
}

impl OutputAccumulator {
    /// Output files are named `<file_prefix>-<random hex>.log`.
    pub fn new(file_prefix: &str) -> Self {
        Self::with_limits(file_prefix, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES)
    }

    pub fn with_limits(file_prefix: &str, max_lines: usize, max_bytes: usize) -> Self {
        Self {
            max_lines,
            max_bytes,
            max_rolling_bytes: (max_bytes * 2).max(1),
            file_prefix: file_prefix.to_string(),
            pending: Vec::new(),
            raw_chunks: Vec::new(),
            tail_text: String::new(),
            tail_starts_at_line_boundary: true,
            total_raw_bytes: 0,
            total_decoded_bytes: 0,
            completed_lines: 0,
            total_lines: 0,
            current_line_bytes: 0,
            has_open_line: false,
            finished: false,
            file_path: None,
            file: None,
            file_error: None,
        }
    }

    pub fn append(&mut self, data: &[u8]) {
        if self.finished {
            return;
        }
        self.total_raw_bytes += data.len();
        self.pending.extend_from_slice(data);
        let text = decode_available(&mut self.pending, false);
        self.append_decoded(&text);
        if self.file.is_some() || self.should_use_file() {
            self.ensure_file();
            self.write_to_file(data);
        } else if !data.is_empty() {
            self.raw_chunks.push(data.to_vec());
        }
    }

    /// No more output: decode what is left and keep everything in a file if needed.
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let text = decode_available(&mut self.pending, true);
        self.append_decoded(&text);
        if self.should_use_file() {
            self.ensure_file();
        }
    }

    pub fn snapshot(&mut self, persist_if_truncated: bool) -> OutputSnapshot {
        let tail = truncate_tail(self.snapshot_text(), self.max_lines, self.max_bytes);
        let truncated =
            self.total_lines > self.max_lines || self.total_decoded_bytes > self.max_bytes;
        let truncated_by = if truncated {
            tail.truncated_by
                .or(Some(if self.total_decoded_bytes > self.max_bytes {
                    TruncatedBy::Bytes
                } else {
                    TruncatedBy::Lines
                }))
        } else {
            None
        };
        let truncation = TruncationResult {
            truncated,
            truncated_by,
            total_lines: self.total_lines,
            total_bytes: self.total_decoded_bytes,
            max_lines: self.max_lines,
            max_bytes: self.max_bytes,
            ..tail
        };
        if persist_if_truncated && truncation.truncated {
            self.ensure_file();
        }
        OutputSnapshot {
            content: truncation.content.clone(),
            truncation,
            full_output_path: self.file_path.clone(),
        }
    }

    /// Flush and close the output file. A file that could not be written is reported
    /// here.
    pub fn close_file(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
        }
        match self.file_error.take() {
            Some(error) => Err(io::Error::other(error)),
            None => Ok(()),
        }
    }

    /// Bytes of the line being written, for a note about a cut last line.
    pub fn last_line_bytes(&self) -> usize {
        self.current_line_bytes
    }

    fn append_decoded(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.total_decoded_bytes += text.len();
        self.tail_text.push_str(text);
        if self.tail_text.len() > self.max_rolling_bytes * 2 {
            self.trim_tail();
        }
        let newlines = text.matches('\n').count();
        if newlines == 0 {
            self.current_line_bytes += text.len();
            self.has_open_line = true;
        } else {
            self.completed_lines += newlines;
            let rest = &text[text.rfind('\n').map_or(0, |at| at + 1)..];
            self.current_line_bytes = rest.len();
            self.has_open_line = !rest.is_empty();
        }
        self.total_lines = self.completed_lines + usize::from(self.has_open_line);
    }

    fn trim_tail(&mut self) {
        if self.tail_text.len() <= self.max_rolling_bytes {
            return;
        }
        let mut start = self.tail_text.len() - self.max_rolling_bytes;
        while !self.tail_text.is_char_boundary(start) {
            start += 1;
        }
        if start > 0 {
            self.tail_starts_at_line_boundary = self.tail_text.as_bytes()[start - 1] == b'\n';
        }
        self.tail_text.drain(..start);
    }

    fn snapshot_text(&self) -> &str {
        if self.tail_starts_at_line_boundary {
            return &self.tail_text;
        }
        match self.tail_text.find('\n') {
            Some(at) => &self.tail_text[at + 1..],
            None => &self.tail_text,
        }
    }

    fn should_use_file(&self) -> bool {
        self.total_raw_bytes > self.max_bytes
            || self.total_decoded_bytes > self.max_bytes
            || self.total_lines > self.max_lines
    }

    fn ensure_file(&mut self) {
        if self.file_path.is_some() {
            return;
        }
        match create_output_file(&self.file_prefix, ".log") {
            Ok((path, file)) => {
                self.file_path = Some(path);
                self.file = Some(BufWriter::new(file));
                for chunk in std::mem::take(&mut self.raw_chunks) {
                    self.write_to_file(&chunk);
                }
            }
            Err(error) => {
                self.file_error = Some(format!("Could not save the full output: {error}"));
            }
        }
    }

    fn write_to_file(&mut self, data: &[u8]) {
        if let Some(file) = &mut self.file
            && let Err(error) = file.write_all(data)
        {
            self.file_error = Some(format!("Could not save the full output: {error}"));
            self.file = None;
        }
    }
}

/// Decode the complete characters in `pending`, leaving an incomplete one at its end
/// for the next chunk. Invalid bytes, and an incomplete character at the end of the
/// output, become U+FFFD.
pub(crate) fn decode_available(pending: &mut Vec<u8>, finish: bool) -> String {
    let mut text = String::new();
    let mut rest: &[u8] = pending;
    loop {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                text.push_str(valid);
                rest = &[];
                break;
            }
            Err(error) => {
                let (valid, after) = rest.split_at(error.valid_up_to());
                text.push_str(&String::from_utf8_lossy(valid));
                match error.error_len() {
                    Some(len) => {
                        text.push(char::REPLACEMENT_CHARACTER);
                        rest = &after[len..];
                    }
                    None if finish => {
                        text.push(char::REPLACEMENT_CHARACTER);
                        rest = &[];
                        break;
                    }
                    None => {
                        rest = after;
                        break;
                    }
                }
            }
        }
    }
    *pending = rest.to_vec();
    text
}

/// A new output file, `<dir>/<prefix>-<random hex><extension>` in the temporary
/// folder, readable only by the user since output can carry private data. It is
/// always a new file, never one someone placed at the path.
pub(crate) fn create_output_file(prefix: &str, extension: &str) -> io::Result<(PathBuf, File)> {
    create_output_file_in(&std::env::temp_dir(), prefix, extension)
}

fn create_output_file_in(dir: &Path, prefix: &str, extension: &str) -> io::Result<(PathBuf, File)> {
    loop {
        let path = dir.join(format!(
            "{prefix}-{:016x}{extension}",
            crate::ids::random_u64()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_character_split_across_chunks_is_decoded_whole() {
        let euro = "€\n".as_bytes();
        let mut output = OutputAccumulator::new("test-output");
        output.append(&euro[..1]);
        output.append(&euro[1..]);
        output.finish();
        assert_eq!(output.snapshot(false).content, "€\n");
    }

    #[test]
    fn invalid_bytes_become_replacement_characters() {
        let mut output = OutputAccumulator::new("test-output");
        output.append(b"a\xffb\xe2\x82");
        output.finish();
        assert_eq!(output.snapshot(false).content, "a\u{fffd}b\u{fffd}");
    }

    #[test]
    fn long_output_keeps_its_tail_and_saves_everything() {
        let mut output = OutputAccumulator::new("test-output");
        for line in 1..=4000 {
            output.append(format!("line-{line:04}\n").as_bytes());
        }
        output.finish();
        let snapshot = output.snapshot(true);
        output.close_file().unwrap();
        assert_eq!(snapshot.truncation.total_lines, 4000);
        assert_eq!(snapshot.truncation.output_lines, 2000);
        assert!(snapshot.content.starts_with("line-2001"));
        assert!(snapshot.content.ends_with("line-4000"));
        let path = snapshot.full_output_path.unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.starts_with("line-0001\n"));
        assert!(saved.ends_with("line-4000\n"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn short_output_needs_no_file() {
        let mut output = OutputAccumulator::new("test-output");
        output.append(b"hello\n");
        output.finish();
        let snapshot = output.snapshot(true);
        assert!(!snapshot.truncation.truncated);
        assert!(snapshot.full_output_path.is_none());
    }

    #[test]
    fn output_files_are_new_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let (first, _) = create_output_file_in(dir.path(), "out", ".log").unwrap();
        let (second, _) = create_output_file_in(dir.path(), "out", ".log").unwrap();
        assert_ne!(first, second);
        assert!(
            first
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("out-")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&first).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
