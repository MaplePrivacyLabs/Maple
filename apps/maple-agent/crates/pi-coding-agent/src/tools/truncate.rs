//! Truncation of tool output by two limits, lines and bytes; whichever is hit first wins.
//!
//! Never returns partial lines, except when tail truncation meets a last line longer
//! than the byte limit.

use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// The characters `grep` shows of a matching line.
pub const GREP_MAX_LINE_LENGTH: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TruncationResult {
    /// The content that was kept.
    pub content: String,
    pub truncated: bool,
    /// Which limit was hit; `None` when nothing was cut.
    pub truncated_by: Option<TruncatedBy>,
    pub total_lines: usize,
    pub total_bytes: usize,
    /// Complete lines kept.
    pub output_lines: usize,
    pub output_bytes: usize,
    /// The kept text starts inside a line (tail truncation of one long line).
    pub last_line_partial: bool,
    /// The first line alone is over the byte limit (head truncation).
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

/// Lines for counting: a trailing newline does not start another line.
fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Bytes as a short human-readable size.
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn untouched(
    content: &str,
    total_lines: usize,
    max_lines: usize,
    max_bytes: usize,
) -> TruncationResult {
    TruncationResult {
        content: content.to_string(),
        truncated: false,
        truncated_by: None,
        total_lines,
        total_bytes: content.len(),
        output_lines: total_lines,
        output_bytes: content.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Keep the first lines that fit, for file reads. Returns no content, with
/// `first_line_exceeds_limit`, when the first line alone is over the byte limit.
pub fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return untouched(content, total_lines, max_lines, max_bytes);
    }
    if lines[0].len() > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut kept_bytes = 0;
    let mut truncated_by = TruncatedBy::Lines;
    for (index, line) in lines.iter().take(max_lines).enumerate() {
        let line_bytes = line.len() + usize::from(index > 0);
        if kept_bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        kept.push(line);
        kept_bytes += line_bytes;
    }
    if kept.len() >= max_lines && kept_bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }
    let content = kept.join("\n");
    TruncationResult {
        output_bytes: content.len(),
        content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: kept.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Keep the last lines that fit, for command output, where errors and results come
/// last. A last line over the byte limit keeps its end.
pub fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return untouched(content, total_lines, max_lines, max_bytes);
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut kept_bytes = 0;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;
    for line in lines.iter().rev() {
        if kept.len() >= max_lines {
            break;
        }
        let line_bytes = line.len() + usize::from(!kept.is_empty());
        if kept_bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            if kept.is_empty() {
                let end = tail_at_char_boundary(line, max_bytes);
                kept.push(end);
                kept_bytes = end.len();
                last_line_partial = true;
            }
            break;
        }
        kept.push(line);
        kept_bytes += line_bytes;
    }
    kept.reverse();
    if kept.len() >= max_lines && kept_bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }
    let content = kept.join("\n");
    TruncationResult {
        output_bytes: content.len(),
        content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: kept.len(),
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Text with its middle cut out to fit `max_bytes`, as Pi's `truncateMiddle` cuts it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MiddleTruncation {
    /// The head and tail kept, joined by a `…N chars truncated…` marker.
    pub content: String,
    pub truncated: bool,
    /// Characters removed from the middle.
    pub removed_chars: usize,
    pub total_bytes: usize,
    pub total_lines: usize,
}

/// Keep the first and last halves of `max_bytes` and mark what was removed between them,
/// as Codex and Pi cut long MCP results. Cuts fall on character boundaries.
pub fn truncate_middle(content: &str, max_bytes: usize) -> MiddleTruncation {
    let total_lines = split_lines_for_counting(content).len();
    if content.len() <= max_bytes {
        return MiddleTruncation {
            content: content.to_string(),
            truncated: false,
            removed_chars: 0,
            total_bytes: content.len(),
            total_lines,
        };
    }
    let mut head_end = max_bytes / 2;
    while !content.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = content.len() - (max_bytes - max_bytes / 2);
    while !content.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let removed_chars = content[head_end..tail_start].chars().count();
    MiddleTruncation {
        content: format!(
            "{}…{removed_chars} chars truncated…{}",
            &content[..head_end],
            &content[tail_start..]
        ),
        truncated: true,
        removed_chars,
        total_bytes: content.len(),
        total_lines,
    }
}

/// `line` cut to `max_chars` characters, marked when it was cut, and whether it was.
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    match line.char_indices().nth(max_chars) {
        Some((end, _)) => (format!("{}... [truncated]", &line[..end]), true),
        None => (line.to_string(), false),
    }
}

/// The last `max_bytes` bytes of `text`, starting at a character boundary.
fn tail_at_char_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_within_both_limits_is_untouched() {
        let result = truncate_head("a\nb\n", 2, 100);
        assert!(!result.truncated);
        assert_eq!(result.content, "a\nb\n");
        assert_eq!(result.total_lines, 2);
        assert_eq!(truncate_tail("", 2, 100).total_lines, 0);
    }

    #[test]
    fn head_truncation_keeps_whole_lines_from_the_start() {
        let lines: Vec<String> = (1..=5).map(|i| format!("line {i}")).collect();
        let text = lines.join("\n");
        let by_lines = truncate_head(&text, 3, 1000);
        assert_eq!(by_lines.content, "line 1\nline 2\nline 3");
        assert_eq!(by_lines.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(by_lines.output_lines, 3);

        let by_bytes = truncate_head(&text, 100, 14);
        assert_eq!(by_bytes.content, "line 1\nline 2");
        assert_eq!(by_bytes.truncated_by, Some(TruncatedBy::Bytes));

        let long_first = truncate_head("0123456789\nx", 100, 5);
        assert!(long_first.first_line_exceeds_limit);
        assert_eq!(long_first.content, "");
    }

    #[test]
    fn tail_truncation_keeps_the_end_and_a_partial_long_line() {
        let text = (1..=5)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let by_lines = truncate_tail(&text, 2, 1000);
        assert_eq!(by_lines.content, "4\n5");
        assert_eq!(by_lines.total_lines, 5);

        let long_last = truncate_tail("short\nabcdé€xyz", 100, 6);
        assert!(long_last.last_line_partial);
        assert_eq!(long_last.content, "€xyz");
        assert_eq!(long_last.truncated_by, Some(TruncatedBy::Bytes));
    }

    #[test]
    fn long_lines_are_cut_at_a_character() {
        assert_eq!(truncate_line("short", 5), ("short".to_string(), false));
        assert_eq!(
            truncate_line("héllo world", 5),
            ("héllo... [truncated]".to_string(), true)
        );
    }

    #[test]
    fn the_middle_is_cut_out_at_characters() {
        let kept = truncate_middle("short\ntext", 64);
        assert!(!kept.truncated);
        assert_eq!(
            (kept.content.as_str(), kept.total_lines),
            ("short\ntext", 2)
        );

        let cut = truncate_middle("aaaa€€€€bbbb", 8);
        assert!(cut.truncated);
        // Four bytes from each end, moved off the middle of a three-byte character.
        assert_eq!(cut.content, "aaaa…4 chars truncated…bbbb");
        assert_eq!((cut.removed_chars, cut.total_bytes), (4, 20));
        let cut = truncate_middle("a€€b", 5);
        assert_eq!(cut.content, "a…2 chars truncated…b");
    }

    #[test]
    fn sizes_read_like_pi() {
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(50 * 1024), "50.0KB");
        assert_eq!(format_size(3 * 1024 * 1024 / 2), "1.5MB");
    }
}
