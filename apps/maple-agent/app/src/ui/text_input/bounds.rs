//! Word and line targets for the platform text-editing shortcuts.
//!
//! Word breaks follow Unicode word boundaries, the same segmentation
//! double-click uses. Apostrophes and connector underscores stay inside a
//! word (`don't`, `foo_bar`). Hyphens, commas, periods, and parentheses
//! are boundaries. macOS Option+Right and GTK Ctrl+Right stop at the end
//! of a word. Windows Ctrl+Right stops at the start of the next word.
//! Leftward motion stops at the start of a word on every platform.
//! A line is the text between newlines, matching composer Vim.

use unicode_segmentation::UnicodeSegmentation;

/// Where a forward word motion places the caret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WordStop {
    /// End of the current word, or the next word when already at an end.
    BoundaryEnd,
    /// Start of the following word.
    NextStart,
}

/// Forward word stop for this operating system.
pub(super) fn host_word_stop() -> WordStop {
    if cfg!(target_os = "windows") {
        WordStop::NextStart
    } else {
        WordStop::BoundaryEnd
    }
}

/// Byte offset where a forward word motion from `offset` lands.
pub(super) fn word_right(text: &str, offset: usize, stop: WordStop) -> usize {
    let offset = offset.min(text.len());
    match stop {
        WordStop::BoundaryEnd => word_ranges(text)
            .find(|word| word.end > offset)
            .map(|word| word.end)
            .unwrap_or(text.len()),
        WordStop::NextStart => word_ranges(text)
            .find(|word| word.start > offset)
            .map(|word| word.start)
            .unwrap_or(text.len()),
    }
}

/// Byte offset where a backward word motion from `offset` lands.
pub(super) fn word_left(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    word_ranges(text)
        .filter(|word| word.start < offset)
        .last()
        .map(|word| word.start)
        .unwrap_or(0)
}

/// Start of the newline-delimited line containing `offset`.
pub(super) fn line_start(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    text[..offset]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0)
}

/// End of the newline-delimited line containing `offset`, before the newline.
pub(super) fn line_end(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    text[offset..]
        .find('\n')
        .map(|index| offset + index)
        .unwrap_or(text.len())
}

struct WordRange {
    start: usize,
    end: usize,
}

fn word_ranges(text: &str) -> impl Iterator<Item = WordRange> + '_ {
    text.split_word_bound_indices()
        .filter_map(|(start, segment)| {
            is_word(segment).then_some(WordRange {
                start,
                end: start + segment.len(),
            })
        })
}

fn is_word(segment: &str) -> bool {
    !segment.is_empty()
        && !segment.chars().all(char::is_whitespace)
        && !segment.chars().all(is_boundary_mark)
}

fn is_boundary_mark(character: char) -> bool {
    matches!(
        character,
        ',' | '.'
            | ';'
            | ':'
            | '!'
            | '?'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '\''
            | '"'
            | '-'
            | '_'
            | '/'
            | '\\'
            | '@'
            | '#'
            | '$'
            | '%'
            | '^'
            | '&'
            | '*'
            | '+'
            | '='
            | '<'
            | '>'
            | '|'
            | '~'
            | '`'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_breaks_keep_apostrophes_and_split_punctuation() {
        let text = "hello, don't foo_bar";
        assert_eq!(word_right(text, 0, WordStop::BoundaryEnd), 5);
        assert_eq!(word_right(text, 2, WordStop::BoundaryEnd), 5);
        assert_eq!(word_right(text, 5, WordStop::BoundaryEnd), 12);
        assert_eq!(word_right(text, 7, WordStop::BoundaryEnd), 12);
        // `foo_bar` is one word. `_` is a connector, matching double-click.
        assert_eq!(word_right(text, 13, WordStop::BoundaryEnd), 20);
        assert_eq!(word_left(text, 20), 13);
        assert_eq!(word_left(text, 16), 13);
        assert_eq!(word_left(text, 13), 7);
        let hyphenated = "foo-bar";
        assert_eq!(word_right(hyphenated, 0, WordStop::BoundaryEnd), 3);
        assert_eq!(word_right(hyphenated, 3, WordStop::BoundaryEnd), 7);
        assert_eq!(word_left(hyphenated, 7), 4);
        assert_eq!(word_left(text, 7), 0);
        assert_eq!(word_left(text, 5), 0);
        assert_eq!(word_left(text, 0), 0);
        assert_eq!(word_right(text, 20, WordStop::BoundaryEnd), 20);
    }

    #[test]
    fn windows_forward_motion_lands_on_the_next_word_start() {
        let text = "The quick";
        assert_eq!(word_right(text, 0, WordStop::NextStart), 4);
        assert_eq!(word_right(text, 1, WordStop::NextStart), 4);
        assert_eq!(word_right(text, 4, WordStop::NextStart), text.len());
        assert_eq!(word_right(text, 0, WordStop::BoundaryEnd), 3);
        assert_eq!(word_right(text, 3, WordStop::BoundaryEnd), text.len());
    }

    #[test]
    fn whitespace_is_skipped_on_the_way_to_a_word() {
        let text = "  hello";
        assert_eq!(word_right(text, 0, WordStop::BoundaryEnd), 7);
        assert_eq!(word_right(text, 0, WordStop::NextStart), 2);
        assert_eq!(word_left(text, 2), 0);
        assert_eq!(word_left(text, 7), 2);
    }

    #[test]
    fn line_edges_follow_newlines() {
        let text = "one\ntwo\n";
        assert_eq!(line_start(text, 0), 0);
        assert_eq!(line_end(text, 0), 3);
        assert_eq!(line_start(text, 3), 0);
        assert_eq!(line_start(text, 4), 4);
        assert_eq!(line_end(text, 5), 7);
        assert_eq!(line_start(text, 8), 8);
        assert_eq!(line_end(text, 8), 8);
        assert_eq!(line_start("", 0), 0);
        assert_eq!(line_end("", 0), 0);
    }
}
