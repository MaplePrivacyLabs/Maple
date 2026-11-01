use crate::protocol::Stream;
use crate::{BACKGROUND_BYTES, FINAL_BYTES, FOREGROUND_BYTES};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

pub(crate) fn prefix(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackgroundChunk {
    pub execution_id: Option<u64>,
    pub stream: String,
    pub text: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BackgroundOutput {
    pub chunks: Vec<BackgroundChunk>,
    pub dropped_stdout_bytes: u64,
    pub dropped_stderr_bytes: u64,
}
#[derive(Default)]
pub(crate) struct Capture {
    pub stdout: String,
    pub stderr: String,
    pub dropped_stdout: u64,
    pub dropped_stderr: u64,
}
impl Capture {
    pub fn push(&mut self, stream: Stream, text: &str) {
        let available =
            (FOREGROUND_BYTES - FINAL_BYTES).saturating_sub(self.stdout.len() + self.stderr.len());
        let kept = prefix(text, available);
        let (output, dropped) = match stream {
            Stream::Stdout => (&mut self.stdout, &mut self.dropped_stdout),
            Stream::Stderr => (&mut self.stderr, &mut self.dropped_stderr),
        };
        output.push_str(kept);
        *dropped = dropped.saturating_add((text.len() - kept.len()) as u64);
    }
}
#[derive(Default)]
pub(crate) struct BackgroundRing {
    chunks: VecDeque<BackgroundChunk>,
    bytes: usize,
    dropped_stdout: u64,
    dropped_stderr: u64,
}
impl BackgroundRing {
    pub fn push(&mut self, execution_id: Option<u64>, stream: Stream, text: String) {
        if text.is_empty() {
            return;
        }
        self.bytes += text.len();
        self.chunks.push_back(BackgroundChunk {
            execution_id,
            stream: stream.as_str().into(),
            text,
        });
        while self.bytes > BACKGROUND_BYTES || self.chunks.len() > 256 {
            let removed = self.chunks.pop_front().unwrap();
            self.bytes -= removed.text.len();
            let dropped = if removed.stream == "stdout" {
                &mut self.dropped_stdout
            } else {
                &mut self.dropped_stderr
            };
            *dropped = dropped.saturating_add(removed.text.len() as u64);
        }
    }
    pub fn take(&mut self) -> BackgroundOutput {
        self.bytes = 0;
        BackgroundOutput {
            chunks: self.chunks.drain(..).collect(),
            dropped_stdout_bytes: std::mem::take(&mut self.dropped_stdout),
            dropped_stderr_bytes: std::mem::take(&mut self.dropped_stderr),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aggregate_capture_reserves_final_space_at_utf8_boundary() {
        let mut capture = Capture::default();
        let text = "😀".repeat(20_000);
        capture.push(Stream::Stdout, &text);
        capture.push(Stream::Stderr, &text);
        assert_eq!(
            capture.stdout.len() + capture.stderr.len(),
            FOREGROUND_BYTES - FINAL_BYTES
        );
        assert_eq!(
            capture.dropped_stdout + capture.dropped_stderr,
            (text.len() * 2 - (FOREGROUND_BYTES - FINAL_BYTES)) as u64
        );
    }
    #[test]
    fn ring_limits_tiny_chunk_metadata_and_is_consumed_once() {
        let mut ring = BackgroundRing::default();
        for id in 0..1_000 {
            ring.push(Some(id), Stream::Stdout, "x".into());
        }
        let output = ring.take();
        assert_eq!(output.chunks.len(), 256);
        assert_eq!(output.dropped_stdout_bytes, 744);
        assert!(ring.take().chunks.is_empty());
        assert_eq!(ring.take().dropped_stdout_bytes, 0);
    }
}
