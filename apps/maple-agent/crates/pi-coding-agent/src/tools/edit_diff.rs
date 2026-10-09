//! Matching edits against a file, and the diffs that describe them.
//!
//! An edit's `oldText` is looked for exactly first. When it is not there, the file and
//! the text are compared loosely (trailing spaces, typographic quotes, dashes and
//! spaces, and Unicode compatibility forms ignored); the lines such a match touches are
//! rewritten and every other line keeps its original bytes.

use similar::{ChangeTag, TextDiff};
use unicode_normalization::UnicodeNormalization;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

/// The file's line ending: CRLF when its first line ends in one.
pub fn detect_line_ending(content: &str) -> &'static str {
    match (content.find("\r\n"), content.find('\n')) {
        (Some(crlf), Some(lf)) if crlf < lf => "\r\n",
        _ => "\n",
    }
}

pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

/// Split off a UTF-8 byte order mark, which a model never includes in `oldText`.
pub fn split_bom(content: &str) -> (&str, &str) {
    match content.strip_prefix('\u{FEFF}') {
        Some(text) => ("\u{FEFF}", text),
        None => ("", content),
    }
}

/// JavaScript's `trimEnd`, which the loose comparison is defined by.
fn trim_end_js(line: &str) -> &str {
    line.trim_end_matches(|c: char| {
        matches!(
            c,
            '\t' | '\n' | '\u{B}' | '\u{C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200A}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202F}'
                    | '\u{205F}'
                    | '\u{3000}'
                    | '\u{FEFF}'
        )
    })
}

/// The loose form of `text`: NFKC, no trailing whitespace on any line, and ASCII
/// quotes, hyphens and spaces for their typographic variants.
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    nfkc.split('\n')
        .map(trim_end_js)
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}'..='\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            c => c,
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuzzyMatch {
    pub index: usize,
    pub length: usize,
    pub used_fuzzy_match: bool,
}

/// Find `old_text` in `content`: exactly, or else in the loose forms of both, where
/// the offsets are in the loose form of `content`.
pub fn fuzzy_find_text(content: &str, old_text: &str) -> Option<FuzzyMatch> {
    if let Some(index) = content.find(old_text) {
        return Some(FuzzyMatch {
            index,
            length: old_text.len(),
            used_fuzzy_match: false,
        });
    }
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old = normalize_for_fuzzy_match(old_text);
    fuzzy_content.find(&fuzzy_old).map(|index| FuzzyMatch {
        index,
        length: fuzzy_old.len(),
        used_fuzzy_match: true,
    })
}

fn count_occurrences(content: &str, old_text: &str) -> usize {
    normalize_for_fuzzy_match(content)
        .matches(normalize_for_fuzzy_match(old_text).as_str())
        .count()
}

#[derive(Clone, Debug)]
struct Replacement {
    edit_index: usize,
    index: usize,
    length: usize,
    new_text: String,
}

/// Apply replacements sorted by position, last first so earlier offsets stay valid.
/// `offset` is where `content` starts in the text the positions refer to.
fn apply_replacements(content: &str, replacements: &[Replacement], offset: usize) -> String {
    let mut result = content.to_string();
    for replacement in replacements.iter().rev() {
        let start = replacement.index - offset;
        result.replace_range(start..start + replacement.length, &replacement.new_text);
    }
    result
}

/// Apply replacements matched against `base` (the loose form of `original`): the lines
/// each one touches are rewritten from `base`, and every other line is copied from
/// `original`.
fn apply_replacements_preserving_unchanged_lines(
    original: &str,
    base: &str,
    replacements: &[Replacement],
) -> Result<String, String> {
    let original_lines: Vec<&str> = original.split_inclusive('\n').collect();
    let mut base_lines = Vec::new();
    let mut offset = 0;
    for line in base.split_inclusive('\n') {
        base_lines.push((offset, offset + line.len()));
        offset += line.len();
    }
    if original_lines.len() != base_lines.len() {
        return Err(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .to_string(),
        );
    }

    let mut sorted = replacements.to_vec();
    sorted.sort_by_key(|replacement| replacement.index);
    // Runs of lines (start, end) and the replacements in them.
    let mut groups: Vec<(usize, usize, Vec<Replacement>)> = Vec::new();
    for replacement in sorted {
        let start_line = base_lines
            .iter()
            .position(|(start, end)| replacement.index >= *start && replacement.index < *end)
            .ok_or("Replacement range is outside the base content.")?;
        let replacement_end = replacement.index + replacement.length;
        let mut end_line = start_line;
        while end_line < base_lines.len() && base_lines[end_line].1 < replacement_end {
            end_line += 1;
        }
        if end_line >= base_lines.len() {
            return Err("Replacement range is outside the base content.".to_string());
        }
        let end_line = end_line + 1;
        match groups.last_mut() {
            Some(group) if start_line < group.1 => {
                group.1 = group.1.max(end_line);
                group.2.push(replacement);
            }
            _ => groups.push((start_line, end_line, vec![replacement])),
        }
    }

    let mut result = String::with_capacity(original.len());
    let mut original_index = 0;
    for (start_line, end_line, replacements) in &groups {
        result.extend(original_lines[original_index..*start_line].iter().copied());
        let group_start = base_lines[*start_line].0;
        let group_end = base_lines[end_line - 1].1;
        result.push_str(&apply_replacements(
            &base[group_start..group_end],
            replacements,
            group_start,
        ));
        original_index = *end_line;
    }
    result.extend(original_lines[original_index..].iter().copied());
    Ok(result)
}

/// Apply edits, all matched against `normalized_content` (LF line endings), and
/// return the content before and after. Every edit must match once, and no two may
/// overlap; nothing is applied otherwise.
pub fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<(String, String), String> {
    let total = edits.len();
    let edits: Vec<Edit> = edits
        .iter()
        .map(|edit| Edit {
            old_text: normalize_to_lf(&edit.old_text),
            new_text: normalize_to_lf(&edit.new_text),
        })
        .collect();
    for (index, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(if total == 1 {
                format!("oldText must not be empty in {path}.")
            } else {
                format!("edits[{index}].oldText must not be empty in {path}.")
            });
        }
    }

    let used_fuzzy_match = edits.iter().any(|edit| {
        fuzzy_find_text(normalized_content, &edit.old_text)
            .is_some_and(|found| found.used_fuzzy_match)
    });
    let base = if used_fuzzy_match {
        normalize_for_fuzzy_match(normalized_content)
    } else {
        normalized_content.to_string()
    };

    let mut matched = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        let found = fuzzy_find_text(&base, &edit.old_text).ok_or_else(|| {
            if total == 1 {
                format!(
                    "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
                )
            } else {
                format!(
                    "Could not find edits[{index}] in {path}. The oldText must match exactly including all whitespace and newlines."
                )
            }
        })?;
        let occurrences = count_occurrences(&base, &edit.old_text);
        if occurrences > 1 {
            return Err(if total == 1 {
                format!(
                    "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
                )
            } else {
                format!(
                    "Found {occurrences} occurrences of edits[{index}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
                )
            });
        }
        matched.push(Replacement {
            edit_index: index,
            index: found.index,
            length: found.length,
            new_text: edit.new_text.clone(),
        });
    }

    matched.sort_by_key(|replacement| replacement.index);
    for pair in matched.windows(2) {
        if pair[0].index + pair[0].length > pair[1].index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                pair[0].edit_index, pair[1].edit_index
            ));
        }
    }

    let new_content = if used_fuzzy_match {
        apply_replacements_preserving_unchanged_lines(normalized_content, &base, &matched)?
    } else {
        apply_replacements(&base, &matched, 0)
    };
    if new_content == normalized_content {
        return Err(if total == 1 {
            format!(
                "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
            )
        } else {
            format!("No changes made to {path}. The replacements produced identical content.")
        });
    }
    Ok((normalized_content.to_string(), new_content))
}

/// A standard unified patch.
pub fn generate_unified_patch(path: &str, old: &str, new: &str, context_lines: usize) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(context_lines)
        .header(path, path)
        .to_string()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PartKind {
    Same,
    Added,
    Removed,
}

/// A diff for display, with line numbers and a few lines of context around each
/// change, and the first changed line in the new file.
pub fn generate_diff_string(old: &str, new: &str, context_lines: usize) -> (String, Option<usize>) {
    let diff = TextDiff::from_lines(old, new);
    let mut parts: Vec<(PartKind, Vec<&str>)> = Vec::new();
    for change in diff.iter_all_changes() {
        let kind = match change.tag() {
            ChangeTag::Equal => PartKind::Same,
            ChangeTag::Insert => PartKind::Added,
            ChangeTag::Delete => PartKind::Removed,
        };
        let line = change.value();
        let line = line.strip_suffix('\n').unwrap_or(line);
        match parts.last_mut() {
            Some((last, lines)) if *last == kind => lines.push(line),
            _ => parts.push((kind, vec![line])),
        }
    }

    let width = old
        .split('\n')
        .count()
        .max(new.split('\n').count())
        .to_string()
        .len();
    let mut output: Vec<String> = Vec::new();
    let (mut old_line, mut new_line) = (1usize, 1usize);
    let mut last_was_change = false;
    let mut first_changed_line = None;
    let skipped_marker = format!(" {:>width$} ...", "");

    for (index, (kind, lines)) in parts.iter().enumerate() {
        if *kind != PartKind::Same {
            first_changed_line.get_or_insert(new_line);
            for line in lines {
                if *kind == PartKind::Added {
                    output.push(format!("+{new_line:>width$} {line}"));
                    new_line += 1;
                } else {
                    output.push(format!("-{old_line:>width$} {line}"));
                    old_line += 1;
                }
            }
            last_was_change = true;
            continue;
        }

        let next_is_change = parts
            .get(index + 1)
            .is_some_and(|(next, _)| *next != PartKind::Same);
        let show = |shown: &[&str], output: &mut Vec<String>, old: &mut usize, new: &mut usize| {
            for line in shown {
                output.push(format!(" {:>width$} {line}", *old));
                *old += 1;
                *new += 1;
            }
        };
        match (last_was_change, next_is_change) {
            (true, true) if lines.len() <= context_lines * 2 => {
                show(lines, &mut output, &mut old_line, &mut new_line);
            }
            (true, true) => {
                show(
                    &lines[..context_lines],
                    &mut output,
                    &mut old_line,
                    &mut new_line,
                );
                let skipped = lines.len() - 2 * context_lines;
                output.push(skipped_marker.clone());
                old_line += skipped;
                new_line += skipped;
                show(
                    &lines[lines.len() - context_lines..],
                    &mut output,
                    &mut old_line,
                    &mut new_line,
                );
            }
            (true, false) => {
                let shown = lines.len().min(context_lines);
                show(&lines[..shown], &mut output, &mut old_line, &mut new_line);
                let skipped = lines.len() - shown;
                if skipped > 0 {
                    output.push(skipped_marker.clone());
                    old_line += skipped;
                    new_line += skipped;
                }
            }
            (false, true) => {
                let skipped = lines.len().saturating_sub(context_lines);
                if skipped > 0 {
                    output.push(skipped_marker.clone());
                    old_line += skipped;
                    new_line += skipped;
                }
                show(&lines[skipped..], &mut output, &mut old_line, &mut new_line);
            }
            (false, false) => {
                old_line += lines.len();
                new_line += lines.len();
            }
        }
        last_was_change = false;
    }
    (output.join("\n"), first_changed_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> Edit {
        Edit {
            old_text: old.to_string(),
            new_text: new.to_string(),
        }
    }

    #[test]
    fn line_endings_are_detected_and_restored() {
        assert_eq!(detect_line_ending("a\r\nb\n"), "\r\n");
        assert_eq!(detect_line_ending("a\nb\r\n"), "\n");
        assert_eq!(detect_line_ending("no newline"), "\n");
        assert_eq!(normalize_to_lf("a\r\nb\rc"), "a\nb\nc");
        assert_eq!(restore_line_endings("a\nb\n", "\r\n"), "a\r\nb\r\n");
    }

    #[test]
    fn the_loose_form_ignores_typography_and_trailing_space() {
        assert_eq!(
            normalize_for_fuzzy_match(
                "\u{2018}a\u{2019} \u{201C}b\u{201D} 1\u{2013}2 x\u{A0}y   \nＡＢＣ"
            ),
            "'a' \"b\" 1-2 x y\nABC"
        );
    }

    #[test]
    fn edits_apply_together_or_not_at_all() {
        let (_, new) = apply_edits_to_normalized_content(
            "foo\nbar\nbaz\n",
            &[edit("foo\n", "foo bar\n"), edit("bar\n", "BAR\n")],
            "f",
        )
        .unwrap();
        assert_eq!(new, "foo bar\nBAR\nbaz\n");

        let error = apply_edits_to_normalized_content(
            "one\ntwo\nthree\n",
            &[edit("one\ntwo\n", "x"), edit("two\nthree\n", "y")],
            "f",
        )
        .unwrap_err();
        assert!(error.contains("overlap"), "{error}");
        let error = apply_edits_to_normalized_content("foo foo foo", &[edit("foo", "bar")], "f")
            .unwrap_err();
        assert!(error.starts_with("Found 3 occurrences"), "{error}");
    }

    #[test]
    fn a_loose_match_keeps_the_bytes_of_untouched_lines() {
        let original = "keep before  \nfirst target  \nfirst after\nkeep middle   \n";
        let (_, new) = apply_edits_to_normalized_content(
            original,
            &[edit("first target\nfirst after", "FIRST\nFIRST2")],
            "f",
        )
        .unwrap();
        assert_eq!(new, "keep before  \nFIRST\nFIRST2\nkeep middle   \n");
    }

    #[test]
    fn the_display_diff_numbers_lines_and_collapses_gaps() {
        let old: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let new = old
            .replace("line 5\n", "LINE 5\n")
            .replace("line 25\n", "LINE 25\n");
        let (diff, first) = generate_diff_string(&old, &new, 2);
        assert_eq!(first, Some(5));
        assert!(diff.contains("- 5 line 5"), "{diff}");
        assert!(diff.contains("+ 5 LINE 5"), "{diff}");
        assert!(diff.contains("   ..."), "{diff}");
        assert!(!diff.contains("line 15"), "{diff}");
    }

    #[test]
    fn the_patch_is_a_unified_diff() {
        let patch = generate_unified_patch("a.txt", "Hello, world!", "Hello, testing!", 4);
        assert!(patch.starts_with("--- a.txt\n+++ a.txt\n@@"), "{patch}");
        assert!(patch.contains("-Hello, world!"));
        assert!(patch.contains("+Hello, testing!"));
    }
}
