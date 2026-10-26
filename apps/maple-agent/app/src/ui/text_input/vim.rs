//! Pure modal editing engine for the chat composer.
//!
//! This module deliberately knows nothing about GPUI, key identities, or
//! `TextInput`.  The host translates descriptor-backed Vim actions into
//! [`VimCommand`] values, lends the engine a plain `String`, applies the
//! returned [`HistoryPlan`] to its own undo store, and mirrors
//! [`VimSelection`] into the widget's end-exclusive byte selection.
//!
//! All public offsets are UTF-8 byte offsets.  Every offset returned by the
//! engine is a Unicode grapheme boundary.  Normal and Visual cursors name the
//! first byte of a grapheme; an empty logical line uses its insertion boundary
//! as a stable sentinel.  Insert cursors are boundaries between graphemes.

use std::cmp::{max, min};
use std::ops::{Range, RangeInclusive};

use unicode_segmentation::UnicodeSegmentation;

/// Largest accepted Vim count.  Larger parsed or multiplied counts saturate
/// here and produce a [`NoticeKind::CountCapped`] notice instead of wrapping.
pub const MAX_COUNT: usize = 999_999;

/// Counts used for motion are naturally bounded by the document. Commands
/// that synthesize text or replay changes are not, so give those paths a
/// much smaller work budget before they allocate or enter a loop.
const MAX_EXPANSION_COUNT: usize = 4_096;
const MAX_REPEAT_COUNT: usize = 1_024;
const MAX_COUNTED_OUTPUT_BYTES: usize = 1_048_576;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VimMode {
    Disabled,
    Normal,
    Insert,
    Visual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Down,
    Up,
    WordForward,
    WordBackward,
    WordEnd,
    /// `W`, `B` and `E`: a WORD is any run of non-blank graphemes.
    BigWordForward,
    BigWordBackward,
    BigWordEnd,
    /// `ge` and `gE`.
    WordEndBackward,
    BigWordEndBackward,
    LineStart,
    /// `^`.
    FirstNonWhitespace,
    LineEnd,
    FirstLine,
    LastLine,
    /// `{` and `}`: paragraphs are separated by empty lines.
    ParagraphBackward,
    ParagraphForward,
    /// `%`, or with a count the line that far through the draft.
    MatchingBracket,
    /// `f`, `F`, `t` and `T` once their target has been typed.
    Find(Find),
    /// `;` and `,`.
    RepeatFind,
    RepeatFindReversed,
}

/// A single-line character search. `repeated` marks `;`/`,`, which step
/// past an adjacent target so a repeated `t` still moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Find {
    pub kind: FindKind,
    pub target: char,
    pub repeated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindKind {
    /// `f`.
    Forward,
    /// `F`.
    Backward,
    /// `t`.
    TillForward,
    /// `T`.
    TillBackward,
}

impl FindKind {
    fn reversed(self) -> Self {
        match self {
            Self::Forward => Self::Backward,
            Self::Backward => Self::Forward,
            Self::TillForward => Self::TillBackward,
            Self::TillBackward => Self::TillForward,
        }
    }

    fn forward(self) -> bool {
        matches!(self, Self::Forward | Self::TillForward)
    }

    fn till(self) -> bool {
        matches!(self, Self::TillForward | Self::TillBackward)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operator {
    Delete,
    Change,
    Yank,
    /// `g~`, `gu` and `gU`.
    Case(CaseChange),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseChange {
    Toggle,
    Lower,
    Upper,
}

impl CaseChange {
    fn apply(self, text: &str) -> String {
        match self {
            Self::Lower => text.to_lowercase(),
            Self::Upper => text.to_uppercase(),
            Self::Toggle => text
                .chars()
                .flat_map(|character| {
                    let swapped: Vec<char> = if character.is_lowercase() {
                        character.to_uppercase().collect()
                    } else if character.is_uppercase() {
                        character.to_lowercase().collect()
                    } else {
                        vec![character]
                    };
                    swapped
                })
                .collect(),
        }
    }
}

/// Single-key operator forms. In Normal mode `s` is `cl`, `S` is `cc`, `C`
/// is `c$`, `D` is `d$`, `Y` is `y$` and `X` is `dh`. In Visual mode `s`
/// changes the selection and the others act on its whole lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperatorShortcut {
    SubstituteChars,
    SubstituteLines,
    ChangeToLineEnd,
    DeleteToLineEnd,
    YankToLineEnd,
    DeleteBefore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextObjectPrefix {
    Inner,
    Around,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextObject {
    Word,
    BigWord,
    /// `ip` and `ap`, which are linewise.
    Paragraph,
    /// `i"`, `i'` and `` i` `` on the current line.
    Quote(char),
    /// `i(`, `i[`, `i{` and `i<`, with `b` and `B` for `(` and `{`.
    Bracket(Bracket),
}

impl TextObject {
    /// The object a key names after `i` or `a`.
    fn for_key(key: char) -> Option<Self> {
        Some(match key {
            'w' => Self::Word,
            'W' => Self::BigWord,
            'p' => Self::Paragraph,
            '"' | '\'' | '`' => Self::Quote(key),
            '(' | ')' | 'b' => Self::Bracket(Bracket::Paren),
            '[' | ']' => Self::Bracket(Bracket::Square),
            '{' | '}' | 'B' => Self::Bracket(Bracket::Curly),
            '<' | '>' => Self::Bracket(Bracket::Angle),
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bracket {
    Paren,
    Square,
    Curly,
    Angle,
}

impl Bracket {
    fn pair(self) -> (char, char) {
        match self {
            Self::Paren => ('(', ')'),
            Self::Square => ('[', ']'),
            Self::Curly => ('{', '}'),
            Self::Angle => ('<', '>'),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertEntry {
    BeforeCursor,
    AfterCursor,
    FirstNonWhitespace,
    LineEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenLinePlacement {
    Above,
    Below,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PastePlacement {
    Before,
    After,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterKind {
    Characterwise,
    Linewise,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisterSnapshot {
    pub text: String,
    pub kind: RegisterKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MotionShape {
    Characterwise,
    Linewise,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MotionInclusion {
    Exclusive,
    Inclusive,
}

/// Semantic result of a motion.  `cursor` is the Normal-mode destination;
/// `operator_boundary` may be an insertion boundary beyond that cursor (for
/// example `w` at the final word).  Linewise endpoints carry the complete
/// addressed logical-line interval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MotionEndpoint {
    pub cursor: usize,
    pub operator_boundary: usize,
    pub shape: MotionShape,
    pub inclusion: MotionInclusion,
    pub addressed_lines: Option<RangeInclusive<usize>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextualToken {
    /// `0` is line-start with no count and a count digit otherwise.
    #[cfg(test)]
    ZeroOrLineStart,
    /// `i` is insert-before normally and an inner text-object prefix while an
    /// operator or Visual selection is pending.
    InnerOrInsert,
    /// `a` is append normally and an around text-object prefix while an
    /// operator or Visual selection is pending.
    AroundOrAppend,
    /// `w` is a motion normally and completes `iw`/`aw` after a prefix.
    WordOrTextObject,
}

/// Descriptor-backed semantic commands consumed by the engine.  Physical
/// key resolution belongs to the effective keymap, not this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VimCommand {
    Motion(Motion),
    BeginOperator(Operator),
    CountDigit(u8),
    TextObjectPrefix(TextObjectPrefix),
    TextObject(TextObject),
    EnterInsert(InsertEntry),
    OpenLine(OpenLinePlacement),
    ToggleVisual,
    /// `V`.
    ToggleVisualLine,
    /// Visual `o`.
    SwapVisualEnds,
    DeleteChars,
    Paste(PastePlacement),
    Undo,
    Redo,
    Repeat,
    Cancel,
    Contextual(ContextualToken),
    /// `f`, `F`, `t` or `T`; the target arrives as a [`Self::Literal`].
    BeginFind(FindKind),
    /// `r`; the replacement arrives as a [`Self::Literal`].
    BeginReplace,
    /// The character typed while [`VimStatus::awaiting_literal`] is set.
    Literal(char),
    Shortcut(OperatorShortcut),
    /// `~` in Normal mode; `~`, `u` and `U` in Visual mode. After `g~`,
    /// `gu` or `gU` the same key acts on the line (`g~~`, `gUU`).
    ChangeCase(CaseChange),
    /// `J`.
    JoinLines,
    /// Safety token for an unmatched printable/invalid grammar input.
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertEditKind {
    Text,
    Backspace,
    Delete,
    SelectionReplacement,
    ImeCommit,
}

/// One replayable step relative to the insertion cursor at that point in the
/// transaction.  Signed values are grapheme-boundary deltas, never byte
/// deltas, which makes the recipe portable to a differently sized target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InsertDeltaStep {
    Move {
        graphemes: isize,
    },
    Replace {
        start: isize,
        end: isize,
        replacement: String,
        /// Byte offset into the deterministic replacement. Unlike `start` and
        /// `end`, this cannot be a post-edit grapheme distance: combining
        /// marks and ZWJ pieces may merge with an adjacent grapheme and make
        /// the old replacement boundary disappear.
        cursor_after_bytes: usize,
        kind: InsertEditKind,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InsertDelta {
    pub steps: Vec<InsertDeltaStep>,
}

impl InsertDelta {
    pub fn is_empty(&self) -> bool {
        !self.steps.iter().any(|step| {
            matches!(
                step,
                InsertDeltaStep::Replace {
                    start,
                    end,
                    replacement,
                    ..
                } if start != end || !replacement.is_empty()
            )
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndentationPolicy {
    CopyCurrentLine,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualOperator {
    Delete,
    Change,
    Paste,
    Replace(char),
    Case(CaseChange),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MotionOrTextObject {
    Motion(Motion),
    TextObject {
        prefix: TextObjectPrefix,
        object: TextObject,
    },
    RepeatedOperatorLine,
}

/// A semantic change recipe used by dot-repeat.  Recipes contain commands,
/// counts, captured register values, and normalized committed insert deltas;
/// they never contain raw keystrokes or original byte offsets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepeatRecipe {
    Insert {
        entry: InsertEntry,
        inserted: InsertDelta,
    },
    Operator {
        operator: Operator,
        target: MotionOrTextObject,
        count: usize,
        inserted: Option<InsertDelta>,
    },
    DeleteChars {
        count: usize,
    },
    Paste {
        placement: PastePlacement,
        register: RegisterSnapshot,
        count: usize,
    },
    VisualChange {
        operator: VisualOperator,
        grapheme_span: usize,
        register: Option<RegisterSnapshot>,
        count: usize,
        inserted: Option<InsertDelta>,
    },
    OpenLine {
        placement: OpenLinePlacement,
        indentation: IndentationPolicy,
        inserted: InsertDelta,
    },
    Replace {
        count: usize,
        replacement: char,
    },
    ToggleCase {
        count: usize,
    },
    Join {
        count: usize,
    },
    /// A linewise Visual change over the same number of lines.
    VisualLines {
        operator: VisualOperator,
        line_count: usize,
        register: Option<RegisterSnapshot>,
        count: usize,
        inserted: Option<InsertDelta>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistorySnapshot {
    pub text: String,
    pub cursor: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryPlan {
    None,
    /// Add `before` to undo history and clear redo.  A transaction spanning
    /// an operator deletion plus Insert returns exactly one Commit on Escape.
    Commit {
        before: HistorySnapshot,
        after: HistorySnapshot,
    },
    /// The host restores up to `count` snapshots and then calls
    /// [`VimState::sync_after_history`].
    Undo {
        count: usize,
    },
    Redo {
        count: usize,
    },
    /// External draft replacement invalidates history tied to the old draft.
    Reset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VimSignal {
    None,
    /// A Normal-mode Escape should synchronously move focus to the prior
    /// application region.  The pure engine cannot perform that transition.
    LeaveComposer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    CountCapped,
    InvalidGrammar,
    NoChange,
    RepeatUnavailable,
    RepeatFailed,
    InsertDeltaIncomplete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VimNotice {
    pub kind: NoticeKind,
    pub message: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionKind {
    Insert,
    Change,
    OpenLine,
    VisualChange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionStatus {
    Idle,
    Active {
        kind: TransactionKind,
        changed: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VimStatus {
    pub mode: VimMode,
    /// Linewise Visual (`V`).
    pub visual_linewise: bool,
    /// Digits accumulated for the next command or operator motion.
    pub count: Option<usize>,
    pub pending_operator: Option<Operator>,
    /// Count captured before the pending operator (for example `3` in `3d`).
    pub operator_count: Option<usize>,
    pub pending_text_object: Option<TextObjectPrefix>,
    /// The next key is a character rather than a command: the target of
    /// `f`/`t`, the replacement for `r`, or the text object after `i`/`a`.
    pub awaiting_literal: bool,
    pub transaction: TransactionStatus,
    pub notice: Option<VimNotice>,
}

/// End-exclusive selection suitable for `TextInput::selected_range`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VimSelection {
    pub range: Range<usize>,
    pub reversed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VimOutcome {
    pub consumed: bool,
    pub text_changed: bool,
    pub selection: VimSelection,
    pub history: HistoryPlan,
    pub signal: VimSignal,
    pub status: VimStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// Commit a real insertion transaction, clear grammar/Visual state, and
    /// enter Disabled.  A same-draft host may retain register/recipe state.
    Disable,
    /// Commit local edits and cancel transient grammar before task/screen
    /// ownership changes.
    TaskOrScreenSwitch,
    /// Commit the insertion transaction before the host sends/clears.
    SendOrClear,
    /// Commit, cancel modal grammar, and place the modal cursor at the supplied
    /// byte boundary. Disabled remains Disabled.
    MouseCaretMove { offset: usize },
    /// Suspend beneath a modal popup.  Insert remains Insert, while pending
    /// Normal/Visual grammar is cancelled.
    PopupTakeover,
    /// Current draft content was replaced externally. The host must clear
    /// old-draft undo/redo and provide the new cursor separately through
    /// [`VimState::reset_after_external_text`]. Disabled remains Disabled.
    ExternalDraftReplacement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingOperator {
    operator: Operator,
    count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TransactionOrigin {
    Insert(InsertEntry),
    Operator {
        target: MotionOrTextObject,
        count: usize,
    },
    Visual {
        grapheme_span: usize,
    },
    VisualLines {
        line_count: usize,
    },
    OpenLine(OpenLinePlacement),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingLiteral {
    Find(FindKind),
    Replace,
}

/// What dot-repeat replays for an operator: the motion or object that chose
/// its text, or how many lines a linewise Visual selection covered.
#[derive(Clone, Debug)]
enum OperatorOrigin {
    Target {
        target: MotionOrTextObject,
        count: usize,
    },
    VisualLines {
        line_count: usize,
    },
}

impl OperatorOrigin {
    /// The recipe for a finished change. Yanks change nothing and never
    /// record one.
    fn recipe(&self, operator: Operator) -> RepeatRecipe {
        match self {
            Self::Target { target, count } => RepeatRecipe::Operator {
                operator,
                target: target.clone(),
                count: *count,
                inserted: None,
            },
            Self::VisualLines { line_count } => RepeatRecipe::VisualLines {
                operator: match operator {
                    Operator::Delete => VisualOperator::Delete,
                    Operator::Change => VisualOperator::Change,
                    Operator::Case(change) => VisualOperator::Case(change),
                    Operator::Yank => unreachable!("a yank records no change"),
                },
                line_count: *line_count,
                register: None,
                count: 1,
                inserted: None,
            },
        }
    }

    fn transaction(&self) -> TransactionOrigin {
        match self {
            Self::Target { target, count } => TransactionOrigin::Operator {
                target: target.clone(),
                count: *count,
            },
            Self::VisualLines { line_count } => TransactionOrigin::VisualLines {
                line_count: *line_count,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VisualEdit {
    Replace(char),
    Case(CaseChange),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InsertTransaction {
    before: HistorySnapshot,
    origin: TransactionOrigin,
    delta: InsertDelta,
    changed: bool,
}

#[derive(Clone, Debug)]
pub struct VimState {
    mode: VimMode,
    cursor: usize,
    visual_anchor: Option<usize>,
    visual_head: Option<usize>,
    visual_linewise: bool,
    count: Option<usize>,
    pending_operator: Option<PendingOperator>,
    pending_text_object: Option<TextObjectPrefix>,
    pending_literal: Option<PendingLiteral>,
    preferred_column: Option<usize>,
    register: Option<RegisterSnapshot>,
    transaction: Option<InsertTransaction>,
    last_change: Option<RepeatRecipe>,
    /// The last `f`/`F`/`t`/`T`, for `;` and `,`.
    last_find: Option<Find>,
    /// Revision-compatible insertion boundary used by application `gi`.
    last_insertion: usize,
    notice: Option<VimNotice>,
}

impl Default for VimState {
    fn default() -> Self {
        Self::disabled()
    }
}

#[derive(Clone, Copy, Debug)]
struct LogicalLine {
    start: usize,
    end: usize,
    full_end: usize,
}

#[derive(Clone, Debug)]
struct LineMap {
    lines: Vec<LogicalLine>,
}

impl LineMap {
    fn new(text: &str) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        for (offset, character) in text.char_indices() {
            if character == '\n' {
                lines.push(LogicalLine {
                    start,
                    end: offset,
                    full_end: offset + character.len_utf8(),
                });
                start = offset + character.len_utf8();
            }
        }
        lines.push(LogicalLine {
            start,
            end: text.len(),
            full_end: text.len(),
        });
        Self { lines }
    }

    fn line_index(&self, offset: usize) -> usize {
        let offset = offset.min(self.lines.last().map_or(0, |line| line.full_end));
        self.lines
            .iter()
            .position(|line| offset < line.full_end)
            .unwrap_or(self.lines.len() - 1)
    }

    fn line(&self, index: usize) -> LogicalLine {
        self.lines[index.min(self.lines.len() - 1)]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LexicalClass {
    Whitespace,
    Word,
    Punctuation,
}

#[derive(Clone, Debug)]
struct GraphemeUnit {
    range: Range<usize>,
    class: LexicalClass,
}

fn classify(grapheme: &str) -> LexicalClass {
    if grapheme.chars().all(char::is_whitespace) {
        LexicalClass::Whitespace
    } else if grapheme
        .chars()
        .any(|character| character.is_alphanumeric() || character == '_')
    {
        LexicalClass::Word
    } else {
        LexicalClass::Punctuation
    }
}

fn grapheme_units(text: &str) -> Vec<GraphemeUnit> {
    text.grapheme_indices(true)
        .map(|(start, grapheme)| GraphemeUnit {
            range: start..start + grapheme.len(),
            class: classify(grapheme),
        })
        .collect()
}

/// `w` words separate letters from punctuation; `W` WORDs only split on
/// whitespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WordKind {
    Small,
    Big,
}

fn word_units(text: &str, kind: WordKind) -> Vec<GraphemeUnit> {
    let mut units = grapheme_units(text);
    if kind == WordKind::Big {
        for unit in &mut units {
            if unit.class == LexicalClass::Punctuation {
                unit.class = LexicalClass::Word;
            }
        }
    }
    units
}

/// Vim counts an empty line as a word. Lines end in a newline unit, so an
/// empty line is a newline at the start of the draft or after another one.
fn is_empty_line_unit(text: &str, units: &[GraphemeUnit], index: usize) -> bool {
    &text[units[index].range.clone()] == "\n"
        && (index == 0 || &text[units[index - 1].range.clone()] == "\n")
}

fn line_graphemes(text: &str, line: LogicalLine) -> Vec<Range<usize>> {
    text[line.start..line.end]
        .grapheme_indices(true)
        .map(|(start, grapheme)| line.start + start..line.start + start + grapheme.len())
        .collect()
}

fn floor_char_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn floor_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = floor_char_boundary(text, offset);
    if offset == text.len() {
        return offset;
    }
    text.grapheme_indices(true)
        .map(|(start, _)| start)
        .take_while(|start| *start <= offset)
        .last()
        .unwrap_or(0)
}

fn normal_cursor(text: &str, offset: usize) -> usize {
    let map = LineMap::new(text);
    let offset = floor_grapheme_boundary(text, offset);
    let line = map.line(map.line_index(offset));
    let graphemes = line_graphemes(text, line);
    if graphemes.is_empty() {
        return line.start;
    }
    if offset <= line.start {
        return line.start;
    }
    if offset >= line.end {
        return graphemes.last().expect("nonempty line").start;
    }
    graphemes
        .iter()
        .find(|range| range.start <= offset && offset < range.end)
        .map_or_else(
            || graphemes.last().expect("nonempty line").start,
            |range| range.start,
        )
}

fn insertion_boundary(text: &str, offset: usize) -> usize {
    floor_grapheme_boundary(text, offset)
}

fn grapheme_end_at(text: &str, offset: usize) -> usize {
    text.grapheme_indices(true)
        .find_map(|(start, grapheme)| (start == offset).then_some(start + grapheme.len()))
        .unwrap_or(offset.min(text.len()))
}

fn grapheme_column(text: &str, line: LogicalLine, cursor: usize) -> usize {
    line_graphemes(text, line)
        .iter()
        .position(|range| range.start == cursor)
        .unwrap_or(0)
}

fn cursor_at_column(text: &str, line: LogicalLine, column: usize) -> usize {
    let graphemes = line_graphemes(text, line);
    if graphemes.is_empty() {
        line.start
    } else {
        graphemes[column.min(graphemes.len() - 1)].start
    }
}

fn first_non_whitespace_boundary(text: &str, line: LogicalLine) -> usize {
    line_graphemes(text, line)
        .into_iter()
        .find(|range| classify(&text[range.clone()]) != LexicalClass::Whitespace)
        .map_or(line.end, |range| range.start)
}

fn first_non_whitespace_cursor(text: &str, line: LogicalLine) -> usize {
    let boundary = first_non_whitespace_boundary(text, line);
    if boundary == line.end {
        line_graphemes(text, line)
            .first()
            .map_or(line.start, |range| range.start)
    } else {
        boundary
    }
}

fn leading_indentation(text: &str, line: LogicalLine) -> &str {
    let boundary = first_non_whitespace_boundary(text, line);
    &text[line.start..boundary]
}

fn signed_boundary_distance(text: &str, from: usize, to: usize) -> Option<isize> {
    let boundaries = all_boundaries(text);
    let from_index = boundaries.iter().position(|boundary| *boundary == from)?;
    let to_index = boundaries.iter().position(|boundary| *boundary == to)?;
    Some(to_index as isize - from_index as isize)
}

fn boundary_offset(text: &str, from: usize, delta: isize) -> Option<usize> {
    let boundaries = all_boundaries(text);
    let from_index = boundaries.iter().position(|boundary| *boundary == from)? as isize;
    let target = from_index.checked_add(delta)?;
    (target >= 0)
        .then_some(target as usize)
        .and_then(|index| boundaries.get(index).copied())
}

fn previous_boundary(text: &str, boundary: usize) -> usize {
    all_boundaries(text)
        .into_iter()
        .take_while(|candidate| *candidate < boundary)
        .last()
        .unwrap_or(boundary.min(text.len()))
}

fn normal_cursor_before_insert_boundary(text: &str, boundary: usize) -> usize {
    let boundary = insertion_boundary(text, boundary);
    let map = LineMap::new(text);
    let line = map.line(map.line_index(boundary));
    line_graphemes(text, line)
        .into_iter()
        .rfind(|range| range.end <= boundary)
        .map_or_else(|| normal_cursor(text, boundary), |range| range.start)
}

fn all_boundaries(text: &str) -> Vec<usize> {
    let mut boundaries: Vec<_> = text
        .grapheme_indices(true)
        .map(|(start, _)| start)
        .collect();
    if boundaries.last().copied() != Some(text.len()) {
        boundaries.push(text.len());
    }
    if boundaries.is_empty() {
        boundaries.push(0);
    }
    boundaries
}

fn range_grapheme_count(text: &str, range: Range<usize>) -> usize {
    text.get(range)
        .map_or(0, |selected| selected.graphemes(true).count())
}

fn checked_repeated_text(text: &str, count: usize) -> Option<String> {
    if count > MAX_EXPANSION_COUNT {
        return None;
    }
    let bytes = text.len().checked_mul(count)?;
    (bytes <= MAX_COUNTED_OUTPUT_BYTES).then(|| text.repeat(count))
}

fn run_start(units: &[GraphemeUnit], mut index: usize) -> usize {
    let class = units[index].class;
    while index > 0 && units[index - 1].class == class {
        index -= 1;
    }
    index
}

fn run_end(units: &[GraphemeUnit], mut index: usize) -> usize {
    let class = units[index].class;
    while index + 1 < units.len() && units[index + 1].class == class {
        index += 1;
    }
    index
}

fn word_forward(text: &str, cursor: usize, count: usize, kind: WordKind) -> (usize, usize) {
    let units = word_units(text, kind);
    if units.is_empty() {
        return (0, 0);
    }
    let lines = LineMap::new(text);
    let mut destination = cursor;
    let mut operator_boundary = cursor;
    for _ in 0..count {
        let start = destination;
        let source_line = lines.line(lines.line_index(destination));
        let Some(mut index) = units
            .iter()
            .position(|unit| unit.range.start >= destination)
        else {
            operator_boundary = text.len();
            break;
        };
        if units[index].range.start > destination && index > 0 {
            index -= 1;
        }

        if units[index].class != LexicalClass::Whitespace {
            index = run_end(&units, index) + 1;
        }
        while index < units.len() && units[index].class == LexicalClass::Whitespace {
            // Vim counts an empty line as a word and stops on it.
            if units[index].range.start > start && is_empty_line_unit(text, &units, index) {
                break;
            }
            index += 1;
        }
        // Vim's operator-pending `w` does not consume a line ending when
        // this step merely finds the first word on the following line.
        // The cursor motion still reaches that word; a further count may
        // then continue from it and legitimately cross the line.
        let boundary = |destination: usize| {
            if destination > source_line.end {
                source_line.end
            } else {
                destination
            }
        };
        if index < units.len() {
            destination = units[index].range.start;
            operator_boundary = boundary(destination);
            continue;
        }
        // So is an empty last line, which has no newline of its own.
        if text.ends_with('\n') && start < text.len() {
            destination = text.len();
            operator_boundary = boundary(destination);
            continue;
        }

        let Some(last_meaningful) = units
            .iter()
            .rposition(|unit| unit.class != LexicalClass::Whitespace)
        else {
            operator_boundary = text.len();
            break;
        };
        destination = units[last_meaningful].range.start;
        operator_boundary = units[run_end(&units, last_meaningful)].range.end;
        break;
    }
    (normal_cursor(text, destination), operator_boundary)
}

/// `b`, following Vim's `bck_word`: step back over blanks, stopping on an
/// empty line, then to the start of the word there.
fn word_backward(text: &str, cursor: usize, count: usize, kind: WordKind) -> usize {
    let units = word_units(text, kind);
    let mut index = units
        .iter()
        .position(|unit| unit.range.start >= cursor)
        .unwrap_or(units.len());
    'count: for _ in 0..count {
        if index == 0 {
            break;
        }
        index -= 1;
        while units[index].class == LexicalClass::Whitespace {
            if is_empty_line_unit(text, &units, index) {
                continue 'count;
            }
            if index == 0 {
                break 'count;
            }
            index -= 1;
        }
        index = run_start(&units, index);
    }
    units
        .get(index)
        .map_or(text.len(), |unit| normal_cursor(text, unit.range.start))
}

fn word_end(text: &str, cursor: usize, count: usize, kind: WordKind) -> usize {
    let units = word_units(text, kind);
    if units.is_empty() {
        return 0;
    }
    let mut destination = cursor;
    for _ in 0..count {
        let current = units
            .iter()
            .position(|unit| unit.range.start == destination);
        let mut index = current.unwrap_or_else(|| {
            units
                .iter()
                .position(|unit| unit.range.start >= destination)
                .unwrap_or(units.len() - 1)
        });
        if units[index].class == LexicalClass::Whitespace {
            while index < units.len() && units[index].class == LexicalClass::Whitespace {
                index += 1;
            }
        } else if run_end(&units, index) == index {
            index += 1;
            while index < units.len() && units[index].class == LexicalClass::Whitespace {
                index += 1;
            }
        }
        if index >= units.len() {
            if let Some(last) = units
                .iter()
                .rposition(|unit| unit.class != LexicalClass::Whitespace)
            {
                destination = units[last].range.start;
            }
            break;
        }
        destination = units[run_end(&units, index)].range.start;
    }
    normal_cursor(text, destination)
}

/// `cw` is Vim's change-to-end-of-word special case, but a cursor already on
/// the final grapheme still changes that grapheme rather than advancing to the
/// end of the following word like a standalone `e` motion would.
fn change_word_end(text: &str, cursor: usize, count: usize, kind: WordKind) -> usize {
    let units = word_units(text, kind);
    let Some(current) = units.iter().position(|unit| unit.range.start == cursor) else {
        return normal_cursor(text, cursor);
    };
    let mut destination = units[run_end(&units, current)].range.start;
    for _ in 1..count.max(1) {
        destination = word_end(text, destination, 1, kind);
    }
    normal_cursor(text, destination)
}

/// `ge`: back to the end of the previous word, stopping on empty lines.
/// Fails only when the cursor is already at the start of the draft; a count
/// that runs out of words stops there.
fn word_end_backward(text: &str, cursor: usize, count: usize, kind: WordKind) -> Option<usize> {
    let units = word_units(text, kind);
    let class_at = |index: usize| {
        units
            .get(index)
            .map_or(LexicalClass::Whitespace, |unit| unit.class)
    };
    let mut index = units
        .iter()
        .position(|unit| unit.range.start >= cursor)
        .unwrap_or(units.len());
    for step in 0..count {
        let start_class = class_at(index);
        if index == 0 {
            return (step > 0).then_some(0);
        }
        index -= 1;
        if start_class != LexicalClass::Whitespace {
            while class_at(index) == start_class {
                if index == 0 {
                    return Some(0);
                }
                index -= 1;
            }
        }
        while class_at(index) == LexicalClass::Whitespace {
            if is_empty_line_unit(text, &units, index) || index == 0 {
                break;
            }
            index -= 1;
        }
    }
    Some(normal_cursor(text, units[index].range.start))
}

/// `}` and `{`: the next or previous empty line. Forward motion that runs
/// out of paragraphs lands on the last grapheme instead, inclusively; it
/// fails if a further count remains.
fn paragraph_motion(
    text: &str,
    cursor: usize,
    count: usize,
    forward: bool,
) -> Option<MotionEndpoint> {
    let map = LineMap::new(text);
    let last = map.lines.len() - 1;
    let mut current = map.line_index(cursor);
    let empty = |index: usize| {
        let line = map.line(index);
        line.start == line.end
    };
    for remaining in (0..count).rev() {
        let mut seen_text = false;
        let mut first = true;
        loop {
            if !empty(current) {
                seen_text = true;
            }
            if !first && seen_text && empty(current) {
                break;
            }
            first = false;
            let next = if forward {
                (current < last).then(|| current + 1)
            } else {
                current.checked_sub(1)
            };
            match next {
                Some(next) => current = next,
                None if remaining > 0 => return None,
                None => break,
            }
        }
    }
    let line = map.line(current);
    let (destination, inclusion) = if forward && current == last && line.start != line.end {
        let last_grapheme = line_graphemes(text, line)
            .last()
            .map_or(line.start, |range| range.start);
        (last_grapheme, MotionInclusion::Inclusive)
    } else {
        (line.start, MotionInclusion::Exclusive)
    };
    Some(MotionEndpoint {
        cursor: destination,
        operator_boundary: destination,
        shape: MotionShape::Characterwise,
        inclusion,
        addressed_lines: None,
    })
}

/// `f`, `F`, `t` and `T` on the cursor's line. A grapheme matches when it
/// starts with the target, so `fe` finds an accented `e`.
fn find_on_line(text: &str, cursor: usize, find: Find, count: usize) -> Option<usize> {
    let map = LineMap::new(text);
    let graphemes = line_graphemes(text, map.line(map.line_index(cursor)));
    let current = graphemes.iter().position(|range| range.start == cursor)?;
    let matches = |index: usize| text[graphemes[index].clone()].starts_with(find.target);
    // A repeated `t`/`T` steps over a target right beside the cursor.
    let skip = usize::from(find.repeated && find.kind.till() && count == 1);
    let mut found = current;
    for _ in 0..count {
        if find.kind.forward() {
            found = (found + 1 + skip..graphemes.len()).find(|index| matches(*index))?;
        } else {
            found = (0..found.checked_sub(skip)?)
                .rev()
                .find(|index| matches(*index))?;
        }
    }
    let destination = match find.kind {
        FindKind::Forward | FindKind::Backward => found,
        FindKind::TillForward => found - 1,
        FindKind::TillBackward => found + 1,
    };
    Some(graphemes[destination].start)
}

const BRACKETS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];

/// The bracket that matches the one at `position`, counting nesting.
fn matching_bracket(text: &str, position: usize, open: char, close: char) -> Option<usize> {
    let opening = text[position..].starts_with(open);
    let mut depth = 0usize;
    if opening {
        for (offset, character) in text[position..].char_indices() {
            if character == open {
                depth += 1;
            } else if character == close {
                depth -= 1;
                if depth == 0 {
                    return Some(position + offset);
                }
            }
        }
    } else {
        for (offset, character) in text[..=position].char_indices().rev() {
            if character == close {
                depth += 1;
            } else if character == open {
                depth -= 1;
                if depth == 0 {
                    return Some(offset);
                }
            }
        }
    }
    None
}

/// `%`: the first bracket at or after the cursor on its line, then its
/// match anywhere in the draft.
fn matching_bracket_motion(text: &str, cursor: usize) -> Option<usize> {
    let map = LineMap::new(text);
    let line = map.line(map.line_index(cursor));
    let (position, open, close) =
        text[cursor..line.end]
            .char_indices()
            .find_map(|(offset, character)| {
                BRACKETS
                    .into_iter()
                    .find(|(open, close)| character == *open || character == *close)
                    .map(|(open, close)| (cursor + offset, open, close))
            })?;
    matching_bracket(text, position, open, close)
}

/// Where Insert-mode `Ctrl-W` deletes back to: over blanks, then over one
/// run of word or of punctuation characters, staying on the line. At the
/// start of a line it takes the line break, as Vim does.
pub fn insert_word_start(text: &str, cursor: usize) -> usize {
    let cursor = insertion_boundary(text, cursor);
    let map = LineMap::new(text);
    let line = map.line(map.line_index(cursor));
    if cursor == line.start {
        return previous_boundary(text, cursor);
    }
    let graphemes: Vec<_> = line_graphemes(text, line)
        .into_iter()
        .take_while(|range| range.end <= cursor)
        .collect();
    let class = |index: usize| classify(&text[graphemes[index].clone()]);
    let mut index = graphemes.len();
    while index > 0 && class(index - 1) == LexicalClass::Whitespace {
        index -= 1;
    }
    if index > 0 {
        let word = class(index - 1);
        while index > 0 && class(index - 1) == word {
            index -= 1;
        }
    }
    graphemes.get(index).map_or(cursor, |range| range.start)
}

/// Where Insert-mode `Ctrl-U` deletes back to: the end of the indentation,
/// or from inside it the start of the line. At the start of a line it
/// takes the line break.
pub fn insert_line_start(text: &str, cursor: usize) -> usize {
    let cursor = insertion_boundary(text, cursor);
    let map = LineMap::new(text);
    let line = map.line(map.line_index(cursor));
    if cursor == line.start {
        return previous_boundary(text, cursor);
    }
    let indentation_end = first_non_whitespace_boundary(text, line);
    if cursor > indentation_end {
        indentation_end
    } else {
        line.start
    }
}

/// `^`: the first non-blank grapheme, or the last blank of a blank line.
fn first_non_blank_motion(text: &str, line: LogicalLine) -> usize {
    let graphemes = line_graphemes(text, line);
    graphemes
        .iter()
        .find(|range| classify(&text[(*range).clone()]) != LexicalClass::Whitespace)
        .or(graphemes.last())
        .map_or(line.start, |range| range.start)
}

/// Resolve a motion from the cursor. `None` is a motion that cannot move,
/// such as `f` without a match: Vim leaves the cursor where it is and
/// cancels a pending operator.
fn compute_motion(
    text: &str,
    cursor: usize,
    motion: Motion,
    count: usize,
    explicit_count: bool,
    operator_pending: bool,
    preferred_column: &mut Option<usize>,
) -> Option<MotionEndpoint> {
    let map = LineMap::new(text);
    let cursor = normal_cursor(text, cursor);
    let current_line_index = map.line_index(cursor);
    let current_line = map.line(current_line_index);
    let count = count.max(1);
    let characterwise = |destination: usize, inclusion: MotionInclusion| MotionEndpoint {
        cursor: destination,
        operator_boundary: destination,
        shape: MotionShape::Characterwise,
        inclusion,
        addressed_lines: None,
    };
    let word_kind = |small: Motion| {
        if motion == small {
            WordKind::Small
        } else {
            WordKind::Big
        }
    };
    if !matches!(motion, Motion::Down | Motion::Up) {
        *preferred_column = None;
    }

    Some(match motion {
        Motion::Left | Motion::Right => {
            let graphemes = line_graphemes(text, current_line);
            let current = graphemes
                .iter()
                .position(|range| range.start == cursor)
                .unwrap_or(0);
            let target = if motion == Motion::Left {
                current.saturating_sub(count)
            } else {
                current
                    .saturating_add(count)
                    .min(graphemes.len().saturating_sub(1))
            };
            let destination = graphemes
                .get(target)
                .map_or(current_line.start, |range| range.start);
            let mut endpoint = characterwise(destination, MotionInclusion::Exclusive);
            // An operator's `l` may cover the final grapheme, so `dl` and
            // `s` still change the last character of a line.
            if motion == Motion::Right && operator_pending {
                endpoint.operator_boundary = graphemes
                    .get(current.saturating_add(count))
                    .map_or(current_line.end, |range| range.start);
            }
            endpoint
        }
        Motion::Down | Motion::Up => {
            let column =
                preferred_column.get_or_insert_with(|| grapheme_column(text, current_line, cursor));
            let target_line_index = if motion == Motion::Down {
                current_line_index
                    .saturating_add(count)
                    .min(map.lines.len() - 1)
            } else {
                current_line_index.saturating_sub(count)
            };
            let destination = cursor_at_column(text, map.line(target_line_index), *column);
            MotionEndpoint {
                cursor: destination,
                operator_boundary: destination,
                shape: MotionShape::Linewise,
                inclusion: MotionInclusion::Inclusive,
                addressed_lines: Some(
                    min(current_line_index, target_line_index)
                        ..=max(current_line_index, target_line_index),
                ),
            }
        }
        Motion::WordForward | Motion::BigWordForward => {
            let (destination, operator_boundary) =
                word_forward(text, cursor, count, word_kind(Motion::WordForward));
            MotionEndpoint {
                cursor: destination,
                operator_boundary,
                shape: MotionShape::Characterwise,
                inclusion: MotionInclusion::Exclusive,
                addressed_lines: None,
            }
        }
        Motion::WordBackward | Motion::BigWordBackward => characterwise(
            word_backward(text, cursor, count, word_kind(Motion::WordBackward)),
            MotionInclusion::Exclusive,
        ),
        Motion::WordEnd | Motion::BigWordEnd => characterwise(
            word_end(text, cursor, count, word_kind(Motion::WordEnd)),
            MotionInclusion::Inclusive,
        ),
        Motion::WordEndBackward | Motion::BigWordEndBackward => characterwise(
            word_end_backward(text, cursor, count, word_kind(Motion::WordEndBackward))?,
            MotionInclusion::Inclusive,
        ),
        Motion::LineStart => characterwise(current_line.start, MotionInclusion::Exclusive),
        Motion::FirstNonWhitespace => characterwise(
            first_non_blank_motion(text, current_line),
            MotionInclusion::Exclusive,
        ),
        Motion::ParagraphBackward | Motion::ParagraphForward => {
            paragraph_motion(text, cursor, count, motion == Motion::ParagraphForward)?
        }
        Motion::MatchingBracket if explicit_count => {
            // `{count}%` goes that far through the draft, to the first
            // non-blank. Under an operator Neovim treats it as exclusive and
            // characterwise, so the `:help exclusive` rules shape the result.
            if count > 100 {
                return None;
            }
            let target_line_index = ((count * map.lines.len()).div_ceil(100))
                .saturating_sub(1)
                .min(map.lines.len() - 1);
            characterwise(
                first_non_whitespace_cursor(text, map.line(target_line_index)),
                MotionInclusion::Exclusive,
            )
        }
        Motion::MatchingBracket => characterwise(
            matching_bracket_motion(text, cursor)?,
            MotionInclusion::Inclusive,
        ),
        Motion::Find(find) => characterwise(
            find_on_line(text, cursor, find, count)?,
            if find.kind.forward() {
                MotionInclusion::Inclusive
            } else {
                MotionInclusion::Exclusive
            },
        ),
        // The engine resolves these against the last find before computing.
        Motion::RepeatFind | Motion::RepeatFindReversed => return None,
        Motion::LineEnd => {
            let target_line_index = current_line_index
                .saturating_add(count.saturating_sub(1))
                .min(map.lines.len() - 1);
            let target_line = map.line(target_line_index);
            let graphemes = line_graphemes(text, target_line);
            let destination = graphemes
                .last()
                .map_or(target_line.start, |range| range.start);
            // An empty line has no last character, so `D` there must not
            // take the line break.
            characterwise(
                destination,
                if graphemes.is_empty() {
                    MotionInclusion::Exclusive
                } else {
                    MotionInclusion::Inclusive
                },
            )
        }
        Motion::FirstLine | Motion::LastLine => {
            let target_line_index = if explicit_count {
                count.saturating_sub(1).min(map.lines.len() - 1)
            } else if motion == Motion::FirstLine {
                0
            } else {
                map.lines.len() - 1
            };
            let target_line = map.line(target_line_index);
            let destination = first_non_whitespace_cursor(text, target_line);
            MotionEndpoint {
                cursor: destination,
                operator_boundary: destination,
                shape: MotionShape::Linewise,
                inclusion: MotionInclusion::Inclusive,
                addressed_lines: Some(
                    min(current_line_index, target_line_index)
                        ..=max(current_line_index, target_line_index),
                ),
            }
        }
    })
}

/// What an operator acts on once its motion or object is resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
enum OperatorSpan {
    Characters(Range<usize>),
    Lines(RangeInclusive<usize>),
}

/// Vim's `:help exclusive` rules. An exclusive motion that ends at the
/// start of a later line stops at the end of the line before it instead;
/// when it also started in the indentation, the operator is linewise.
fn exclusive_operator_span(text: &str, range: Range<usize>) -> OperatorSpan {
    let map = LineMap::new(text);
    let start_line = map.line_index(range.start);
    let end_line = map.line_index(range.end);
    if end_line <= start_line || range.end != map.line(end_line).start {
        return OperatorSpan::Characters(range);
    }
    if range.start <= first_non_whitespace_boundary(text, map.line(start_line)) {
        return OperatorSpan::Lines(start_line..=end_line - 1);
    }
    let previous = map.line(end_line - 1);
    OperatorSpan::Characters(range.start..previous.end)
}

/// Vim's delete rule: a characterwise delete over several lines that
/// starts in the indentation and leaves only blanks after its end deletes
/// the whole lines.
fn delete_operator_span(text: &str, range: Range<usize>) -> OperatorSpan {
    let map = LineMap::new(text);
    let start_line = map.line_index(range.start);
    let end_line = map.line_index(range.end.saturating_sub(1).max(range.start));
    let end = map.line(end_line);
    let rest_is_blank = text[range.end.min(end.end)..end.end]
        .chars()
        .all(|character| character == ' ' || character == '\t');
    if end_line > start_line
        && rest_is_blank
        && range.end <= end.end
        && range.start <= first_non_whitespace_boundary(text, map.line(start_line))
    {
        OperatorSpan::Lines(start_line..=end_line)
    } else {
        OperatorSpan::Characters(range)
    }
}

fn characterwise_motion_range(
    text: &str,
    origin: usize,
    endpoint: &MotionEndpoint,
) -> Range<usize> {
    let boundary = endpoint.operator_boundary;
    if boundary > origin {
        let end = if endpoint.inclusion == MotionInclusion::Inclusive {
            grapheme_end_at(text, endpoint.cursor)
        } else {
            boundary
        };
        origin..end.max(origin)
    } else if boundary < origin {
        let end = if endpoint.inclusion == MotionInclusion::Inclusive {
            grapheme_end_at(text, origin)
        } else {
            origin
        };
        boundary..end
    } else if endpoint.inclusion == MotionInclusion::Inclusive {
        origin..grapheme_end_at(text, origin)
    } else {
        origin..origin
    }
}

/// Where a text object reaches. Inner brackets that end at the start of
/// the closing line are exclusive there, which the operator rules can turn
/// linewise (`di{` on a block deletes its lines).
#[derive(Clone, Debug, PartialEq, Eq)]
enum ObjectExtent {
    Characters {
        range: Range<usize>,
        exclusive: bool,
    },
    Lines(RangeInclusive<usize>),
}

fn text_object_extent(
    text: &str,
    cursor: usize,
    prefix: TextObjectPrefix,
    object: TextObject,
    count: usize,
) -> Option<ObjectExtent> {
    let characters = |range| ObjectExtent::Characters {
        range,
        exclusive: false,
    };
    match object {
        TextObject::Word => {
            word_object(text, cursor, prefix, count, WordKind::Small).map(characters)
        }
        TextObject::BigWord => {
            word_object(text, cursor, prefix, count, WordKind::Big).map(characters)
        }
        TextObject::Paragraph => {
            paragraph_object(text, cursor, prefix, count).map(ObjectExtent::Lines)
        }
        TextObject::Quote(quote) => {
            quote_object(text, cursor, prefix, quote, count).map(characters)
        }
        TextObject::Bracket(bracket) => bracket_object(text, cursor, prefix, bracket, count),
    }
}

fn word_object(
    text: &str,
    cursor: usize,
    prefix: TextObjectPrefix,
    count: usize,
    kind: WordKind,
) -> Option<Range<usize>> {
    let map = LineMap::new(text);
    let line = map.line(map.line_index(cursor));
    let units: Vec<_> = word_units(text, kind)
        .into_iter()
        .filter(|unit| unit.range.start >= line.start && unit.range.end <= line.end)
        .collect();
    if units.is_empty() {
        return None;
    }
    let current = units
        .iter()
        .position(|unit| unit.range.start == cursor)
        .unwrap_or(0);
    if prefix == TextObjectPrefix::Inner && units[current].class == LexicalClass::Whitespace {
        let start_index = run_start(&units, current);
        let mut end_index = run_end(&units, current);
        // On whitespace Vim treats each successive lexical run as another
        // inner object: `iw` is the whitespace, `2iw` adds the next word.
        for _ in 1..count.max(1) {
            if end_index + 1 >= units.len() {
                break;
            }
            end_index = run_end(&units, end_index + 1);
        }
        return Some(units[start_index].range.start..units[end_index].range.end);
    }
    let meaningful = if units[current].class != LexicalClass::Whitespace {
        current
    } else {
        units
            .iter()
            .enumerate()
            .skip(current)
            .find_map(|(index, unit)| (unit.class != LexicalClass::Whitespace).then_some(index))
            .or_else(|| {
                units
                    .iter()
                    .enumerate()
                    .take(current)
                    .rev()
                    .find_map(|(index, unit)| {
                        (unit.class != LexicalClass::Whitespace).then_some(index)
                    })
            })?
    };
    let start_index = run_start(&units, meaningful);
    let mut end_index = run_end(&units, meaningful);
    for _ in 1..count.max(1) {
        let mut next = end_index + 1;
        while next < units.len() && units[next].class == LexicalClass::Whitespace {
            next += 1;
        }
        if next >= units.len() {
            break;
        }
        end_index = run_end(&units, next);
    }
    let mut start = units[start_index].range.start;
    let mut end = units[end_index].range.end;
    if prefix == TextObjectPrefix::Around {
        let mut trailing = end_index + 1;
        while trailing < units.len() && units[trailing].class == LexicalClass::Whitespace {
            end = units[trailing].range.end;
            trailing += 1;
        }
        if end == units[end_index].range.end {
            let mut leading = start_index;
            while leading > 0 && units[leading - 1].class == LexicalClass::Whitespace {
                leading -= 1;
                start = units[leading].range.start;
            }
        }
    }
    Some(start..end)
}

fn is_blank_line(text: &str, line: LogicalLine) -> bool {
    text[line.start..line.end]
        .chars()
        .all(|character| character == ' ' || character == '\t')
}

/// `ip` and `ap`, following Vim's `current_par`: a paragraph is a run of
/// lines that are all blank or all not; `ap` adds the blank lines after
/// it, or before it when there are none after.
fn paragraph_object(
    text: &str,
    cursor: usize,
    prefix: TextObjectPrefix,
    count: usize,
) -> Option<RangeInclusive<usize>> {
    let map = LineMap::new(text);
    let last = map.lines.len() - 1;
    let blank = |index: usize| is_blank_line(text, map.line(index));
    let include = prefix == TextObjectPrefix::Around;
    let mut start = map.line_index(cursor);
    let white_in_front = blank(start);
    while start > 0 && blank(start - 1) == white_in_front {
        start -= 1;
    }
    let mut end = start;
    while end <= last && blank(end) {
        end += 1;
    }
    // `end` is now the line after the blank run, or `start` itself.
    let mut end = end as isize - 1;
    let mut remaining = count.max(1);
    if !include && white_in_front {
        remaining -= 1;
    }
    while remaining > 0 {
        remaining -= 1;
        if end == last as isize {
            return None;
        }
        let next_blank = blank((end + 1) as usize);
        if include || !next_blank {
            end += 1;
            while (end as usize) < last && !blank(end as usize + 1) {
                end += 1;
            }
        }
        if remaining == 0 && white_in_front && include {
            break;
        }
        if include || next_blank {
            while (end as usize) < last && blank(end as usize + 1) {
                end += 1;
            }
        }
    }
    let end = end.max(0) as usize;
    if !white_in_front && !blank(end) && include {
        while start > 0 && blank(start - 1) {
            start -= 1;
        }
    }
    Some(start..=end)
}

/// The next unescaped `quote` at or after `from`. An escaped quote only
/// counts as a delimiter when `escape` is false.
fn find_next_quote(line: &str, from: usize, quote: char, escape: bool) -> Option<usize> {
    let mut characters = line.get(from..)?.char_indices();
    while let Some((offset, character)) = characters.next() {
        if escape && character == '\\' {
            characters.next();
        } else if character == quote {
            return Some(from + offset);
        }
    }
    None
}

/// The previous unescaped `quote` before `from`, or 0 when there is none.
fn find_prev_quote(line: &str, from: usize, quote: char) -> usize {
    let bytes = line.as_bytes();
    let mut column = from;
    while column > 0 {
        column = floor_char_boundary(line, column - 1);
        let mut escapes = 0;
        while column > escapes && bytes[column - escapes - 1] == b'\\' {
            escapes += 1;
        }
        if escapes % 2 == 1 {
            column -= escapes;
        } else if line[column..].starts_with(quote) {
            break;
        }
    }
    column
}

/// `i"` and `a"` on the cursor's line, following Vim's `current_quote`. On
/// a quote, pairs are counted from the start of the line; elsewhere the
/// nearest quotes around the cursor are used, or the first string after it.
/// A counted `2i"` takes the quotes but not the blanks around them.
fn quote_object(
    text: &str,
    cursor: usize,
    prefix: TextObjectPrefix,
    quote: char,
    count: usize,
) -> Option<Range<usize>> {
    let map = LineMap::new(text);
    let line = map.line(map.line_index(cursor));
    let source = &text[line.start..line.end];
    let column = cursor - line.start;
    let (mut open, mut close);
    if source[column..].starts_with(quote) {
        let mut from = 0;
        loop {
            open = find_next_quote(source, from, quote, false)?;
            if open > column {
                return None;
            }
            close = find_next_quote(source, open + quote.len_utf8(), quote, true)?;
            if column <= close {
                break;
            }
            from = close + quote.len_utf8();
        }
    } else {
        open = find_prev_quote(source, column, quote);
        if !source[open..].starts_with(quote) {
            open = find_next_quote(source, open, quote, false)?;
        }
        close = find_next_quote(source, open + quote.len_utf8(), quote, true)?;
    }
    let blank = |character: char| character == ' ' || character == '\t';
    let mut end = close + quote.len_utf8();
    if prefix == TextObjectPrefix::Around {
        let trailing = source[end..]
            .find(|character: char| !blank(character))
            .unwrap_or(source.len() - end);
        if trailing > 0 {
            end += trailing;
        } else {
            open = source[..open].trim_end_matches(blank).len();
        }
        return Some(line.start + open..line.start + end);
    }
    if count >= 2 {
        return Some(line.start + open..line.start + end);
    }
    Some(line.start + open + quote.len_utf8()..line.start + close)
}

/// The unmatched `open` before `position`, skipping balanced pairs.
fn enclosing_open(text: &str, position: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, character) in text[..position].char_indices().rev() {
        if character == close {
            depth += 1;
        } else if character == open {
            if depth == 0 {
                return Some(offset);
            }
            depth -= 1;
        }
    }
    None
}

/// The first `open` after `position` that no earlier `close` balances.
fn next_open(text: &str, position: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, character) in text[position..].char_indices() {
        if character == close {
            depth += 1;
        } else if character == open {
            if depth == 0 {
                return Some(position + offset);
            }
            depth -= 1;
        }
    }
    None
}

/// `i(`, `a{` and the other bracket objects, following Vim's
/// `current_block`. A cursor outside every pair uses the next pair after
/// it. Inner objects drop the brackets, and when the closing bracket has
/// only indentation before it they also drop that line.
fn bracket_object(
    text: &str,
    cursor: usize,
    prefix: TextObjectPrefix,
    bracket: Bracket,
    count: usize,
) -> Option<ObjectExtent> {
    let (open, close) = bracket.pair();
    let map = LineMap::new(text);
    let mut position = cursor;
    if open == '{' {
        // Vim ignores the indentation in front of a `{`.
        let line = map.line(map.line_index(cursor));
        let first = first_non_whitespace_boundary(text, line);
        if position < first && first < line.end {
            position = first;
        }
    }
    if text[position..].starts_with(open) {
        position += open.len_utf8();
    }
    let mut start = match enclosing_open(text, position, open, close) {
        Some(found) => found,
        None => next_open(text, position, open, close)?,
    };
    for _ in 1..count.max(1) {
        start = enclosing_open(text, start, open, close)?;
    }
    let end = matching_bracket(text, start, open, close)?;
    if prefix == TextObjectPrefix::Around {
        return Some(ObjectExtent::Characters {
            range: start..end + close.len_utf8(),
            exclusive: false,
        });
    }

    // Step inside the brackets. Past a bracket that ends its line, the
    // inner text starts on the next line.
    let mut inner_start = start + open.len_utf8();
    if text[inner_start..].starts_with('\n') {
        inner_start += 1;
    }
    let close_line = map.line(map.line_index(end));
    let mut sol = end == close_line.start;
    let mut inner_end = previous_char_position(text, end);
    // Skip back over the closing line's indentation.
    loop {
        let line = map.line(map.line_index(inner_end));
        if inner_end >= line.start && inner_end < first_non_whitespace_boundary(text, line) {
            sol = true;
            if inner_end == line.start {
                inner_end = previous_char_position(text, inner_end);
                break;
            }
            inner_end = previous_char_position(text, inner_end);
        } else {
            break;
        }
    }
    let range = if sol {
        let line = map.line(map.line_index(inner_end));
        let after = if inner_end >= line.end || grapheme_end_at(text, inner_end) >= line.end {
            line.full_end
        } else {
            grapheme_end_at(text, inner_end)
        };
        return Some(ObjectExtent::Characters {
            range: inner_start..after.max(inner_start),
            exclusive: true,
        });
    } else if inner_start <= inner_end {
        inner_start..grapheme_end_at(text, inner_end)
    } else {
        inner_start..inner_start
    };
    Some(ObjectExtent::Characters {
        range,
        exclusive: false,
    })
}

/// The position Vim's `decl` reaches from `position`: the previous
/// grapheme, or across a line break the last grapheme of the line before
/// (its start when that line is empty).
fn previous_char_position(text: &str, position: usize) -> usize {
    if position == 0 {
        return 0;
    }
    let map = LineMap::new(text);
    let line = map.line(map.line_index(position));
    if position > line.start {
        return previous_boundary(text, position);
    }
    let previous = map.line(map.line_index(position - 1));
    line_graphemes(text, previous)
        .last()
        .map_or(previous.start, |range| range.start)
}

fn visual_range(text: &str, anchor: usize, head: usize) -> Range<usize> {
    let start = min(anchor, head);
    let far = max(anchor, head);
    let end = grapheme_end_at(text, far);
    start..end.max(far)
}

fn linewise_normalized_register(text: &str, line_range: RangeInclusive<usize>) -> String {
    let map = LineMap::new(text);
    let mut captured = String::new();
    for index in line_range {
        let line = map.line(index);
        captured.push_str(&text[line.start..line.end]);
        captured.push('\n');
    }
    captured
}

fn linewise_delete_range(text: &str, line_range: RangeInclusive<usize>) -> Range<usize> {
    let map = LineMap::new(text);
    let start_index = *line_range.start();
    let end_index = *line_range.end();
    let first = map.line(start_index);
    let last = map.line(end_index);
    if end_index + 1 < map.lines.len() {
        first.start..last.full_end
    } else if start_index > 0 {
        let previous = map.line(start_index - 1);
        previous.end..last.end
    } else {
        0..text.len()
    }
}

fn line_contents(text: &str) -> Vec<String> {
    text.split('\n').map(ToOwned::to_owned).collect()
}

fn join_lines(lines: &[String]) -> String {
    lines.join("\n")
}

fn span_from_cursor(text: &str, cursor: usize, count: usize) -> Option<Range<usize>> {
    let units = grapheme_units(text);
    let start = units.iter().position(|unit| unit.range.start == cursor)?;
    let end = start.saturating_add(count.max(1)).min(units.len());
    Some(units[start].range.start..units[end - 1].range.end)
}

impl VimState {
    /// Create an enabled composer engine in Normal mode at the empty-line
    /// sentinel.  Use [`Self::set_cursor`] after loading an existing draft.
    pub fn new() -> Self {
        Self {
            mode: VimMode::Normal,
            cursor: 0,
            visual_anchor: None,
            visual_head: None,
            visual_linewise: false,
            count: None,
            pending_operator: None,
            pending_text_object: None,
            pending_literal: None,
            preferred_column: None,
            register: None,
            transaction: None,
            last_change: None,
            last_find: None,
            last_insertion: 0,
            notice: None,
        }
    }

    pub fn disabled() -> Self {
        let mut state = Self::new();
        state.mode = VimMode::Disabled;
        state
    }

    pub fn at(text: &str, cursor: usize) -> Self {
        let mut state = Self::new();
        state.cursor = normal_cursor(text, cursor);
        state.last_insertion = insertion_boundary(text, cursor);
        state
    }

    pub fn mode(&self) -> VimMode {
        self.mode
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    #[cfg(test)]
    pub fn last_insertion(&self) -> usize {
        self.last_insertion
    }

    /// Enter Insert at the last valid insertion boundary. The host resets
    /// this boundary whenever it replaces the draft, so stale offsets never
    /// cross draft revisions.
    pub fn enter_at_last_insertion(&mut self, text: &str) -> VimOutcome {
        self.notice = None;
        if self.mode == VimMode::Disabled {
            return self.not_consumed(text);
        }
        self.mode = VimMode::Insert;
        self.cursor = insertion_boundary(text, self.last_insertion);
        self.leave_visual();
        self.clear_grammar();
        self.preferred_column = None;
        self.transaction = Some(InsertTransaction {
            before: self.snapshot(text),
            origin: TransactionOrigin::Insert(InsertEntry::BeforeCursor),
            delta: InsertDelta::default(),
            changed: false,
        });
        self.outcome(text, false, HistoryPlan::None, VimSignal::None)
    }

    #[cfg(test)]
    pub fn unnamed_register(&self) -> Option<&RegisterSnapshot> {
        self.register.as_ref()
    }

    #[cfg(test)]
    pub fn last_change(&self) -> Option<&RepeatRecipe> {
        self.last_change.as_ref()
    }

    pub fn transaction_status(&self) -> TransactionStatus {
        let Some(transaction) = &self.transaction else {
            return TransactionStatus::Idle;
        };
        let kind = match transaction.origin {
            TransactionOrigin::Insert(_) => TransactionKind::Insert,
            TransactionOrigin::Operator { .. } => TransactionKind::Change,
            TransactionOrigin::Visual { .. } | TransactionOrigin::VisualLines { .. } => {
                TransactionKind::VisualChange
            }
            TransactionOrigin::OpenLine(_) => TransactionKind::OpenLine,
        };
        TransactionStatus::Active {
            kind,
            changed: transaction.changed,
        }
    }

    pub fn status(&self) -> VimStatus {
        VimStatus {
            mode: self.mode,
            visual_linewise: self.mode == VimMode::Visual && self.visual_linewise,
            count: self.count,
            pending_operator: self.pending_operator.map(|pending| pending.operator),
            operator_count: self.pending_operator.map(|pending| pending.count),
            pending_text_object: self.pending_text_object,
            awaiting_literal: self.awaiting_literal(),
            transaction: self.transaction_status(),
            notice: self.notice.clone(),
        }
    }

    /// The next key is a character, not a command: see
    /// [`VimStatus::awaiting_literal`].
    pub fn awaiting_literal(&self) -> bool {
        self.pending_literal.is_some() || self.pending_text_object.is_some()
    }

    /// The lines a linewise Visual selection covers.
    fn visual_lines(&self, text: &str) -> RangeInclusive<usize> {
        let map = LineMap::new(text);
        let anchor = map.line_index(self.visual_anchor.unwrap_or(self.cursor));
        let head = map.line_index(self.visual_head.unwrap_or(self.cursor));
        min(anchor, head)..=max(anchor, head)
    }

    pub fn selection(&self, text: &str) -> VimSelection {
        if self.mode == VimMode::Visual {
            let anchor = self.visual_anchor.unwrap_or(self.cursor);
            let head = self.visual_head.unwrap_or(self.cursor);
            let range = if self.visual_linewise {
                // Whole lines, through the last line's break so each one
                // highlights edge to edge.
                let map = LineMap::new(text);
                let lines = self.visual_lines(text);
                map.line(*lines.start()).start..map.line(*lines.end()).full_end
            } else {
                visual_range(text, anchor, head)
            };
            VimSelection {
                range,
                reversed: head < anchor,
            }
        } else {
            let cursor = if self.mode == VimMode::Insert {
                insertion_boundary(text, self.cursor)
            } else {
                normal_cursor(text, self.cursor)
            };
            VimSelection {
                range: cursor..cursor,
                reversed: false,
            }
        }
    }

    #[cfg(test)]
    pub fn set_cursor(&mut self, text: &str, cursor: usize) {
        self.cursor = if self.mode == VimMode::Insert {
            insertion_boundary(text, cursor)
        } else {
            normal_cursor(text, cursor)
        };
        if self.mode == VimMode::Visual {
            self.visual_head = Some(self.cursor);
        }
        self.preferred_column = None;
    }

    /// Synchronize the cursor after the host applied an Undo or Redo request.
    /// History restoration always returns the editor to Normal mode and clears
    /// transient grammar without altering the unnamed register or dot recipe.
    pub fn sync_after_history(&mut self, text: &str, cursor: usize) {
        self.mode = VimMode::Normal;
        self.cursor = normal_cursor(text, cursor);
        self.leave_visual();
        self.transaction = None;
        self.clear_grammar();
        self.preferred_column = None;
    }

    fn snapshot(&self, text: &str) -> HistorySnapshot {
        HistorySnapshot {
            text: text.to_owned(),
            cursor: self.cursor,
        }
    }

    fn outcome(
        &self,
        text: &str,
        text_changed: bool,
        history: HistoryPlan,
        signal: VimSignal,
    ) -> VimOutcome {
        VimOutcome {
            consumed: true,
            text_changed,
            selection: self.selection(text),
            history,
            signal,
            status: self.status(),
        }
    }

    fn not_consumed(&self, text: &str) -> VimOutcome {
        VimOutcome {
            consumed: false,
            text_changed: false,
            selection: self.selection(text),
            history: HistoryPlan::None,
            signal: VimSignal::None,
            status: self.status(),
        }
    }

    fn clear_grammar(&mut self) {
        self.count = None;
        self.pending_operator = None;
        self.pending_text_object = None;
        self.pending_literal = None;
    }

    fn leave_visual(&mut self) {
        self.visual_anchor = None;
        self.visual_head = None;
        self.visual_linewise = false;
    }

    fn invalid_grammar(&mut self, text: &str) -> VimOutcome {
        self.clear_grammar();
        self.notice = Some(VimNotice {
            kind: NoticeKind::InvalidGrammar,
            message: "invalid Vim command",
        });
        self.outcome(text, false, HistoryPlan::None, VimSignal::None)
    }

    fn reject_counted_work(&mut self, text: &str, message: &'static str) -> VimOutcome {
        self.clear_grammar();
        self.notice = Some(VimNotice {
            kind: NoticeKind::CountCapped,
            message,
        });
        self.outcome(text, false, HistoryPlan::None, VimSignal::None)
    }

    fn push_count_digit(&mut self, digit: u8) {
        let current = self.count.unwrap_or(0);
        let next = current
            .checked_mul(10)
            .and_then(|value| value.checked_add(usize::from(digit)))
            .unwrap_or(MAX_COUNT);
        if next > MAX_COUNT || (current == MAX_COUNT && digit != 0) {
            self.count = Some(MAX_COUNT);
            self.notice = Some(VimNotice {
                kind: NoticeKind::CountCapped,
                message: "Vim count capped at 999999",
            });
        } else {
            self.count = Some(next);
        }
    }

    fn take_count(&mut self) -> (usize, bool) {
        let explicit = self.count.is_some();
        (self.count.take().unwrap_or(1), explicit)
    }

    fn multiplied_count(&mut self, left: usize, right: usize) -> usize {
        match left.checked_mul(right) {
            Some(product) if product <= MAX_COUNT => product,
            _ => {
                self.notice = Some(VimNotice {
                    kind: NoticeKind::CountCapped,
                    message: "multiplied Vim count capped at 999999",
                });
                MAX_COUNT
            }
        }
    }

    fn contextual_command(&self, token: ContextualToken) -> VimCommand {
        match token {
            #[cfg(test)]
            ContextualToken::ZeroOrLineStart => {
                if self.count.is_some() {
                    VimCommand::CountDigit(0)
                } else {
                    VimCommand::Motion(Motion::LineStart)
                }
            }
            ContextualToken::InnerOrInsert => {
                if self.pending_operator.is_some() || self.mode == VimMode::Visual {
                    VimCommand::TextObjectPrefix(TextObjectPrefix::Inner)
                } else {
                    VimCommand::EnterInsert(InsertEntry::BeforeCursor)
                }
            }
            ContextualToken::AroundOrAppend => {
                if self.pending_operator.is_some() || self.mode == VimMode::Visual {
                    VimCommand::TextObjectPrefix(TextObjectPrefix::Around)
                } else {
                    VimCommand::EnterInsert(InsertEntry::AfterCursor)
                }
            }
            ContextualToken::WordOrTextObject => {
                if self.pending_text_object.is_some() {
                    VimCommand::TextObject(TextObject::Word)
                } else {
                    VimCommand::Motion(Motion::WordForward)
                }
            }
        }
    }

    /// Consume a typed semantic command and mutate `text` when the command is
    /// an immediate Vim change.  Insert/IME edits use [`Self::insert_edit`].
    pub fn handle_command(&mut self, text: &mut String, command: VimCommand) -> VimOutcome {
        self.notice = None;
        if self.mode == VimMode::Disabled {
            return self.not_consumed(text);
        }
        let command = match command {
            VimCommand::Contextual(token) => self.contextual_command(token),
            command => command,
        };

        if self.mode == VimMode::Insert {
            return if command == VimCommand::Cancel {
                self.finish_insert(text, true)
            } else {
                // Insert stays on the ordinary TextInput/IME path.  The raw
                // unmatched-printable safety guard is only for Normal/Visual.
                self.not_consumed(text)
            };
        }

        if command == VimCommand::Cancel {
            // Escape while a key is awaited drops only the pending command,
            // so Visual mode survives `vf<Esc>` as it does in Vim.
            if self.awaiting_literal() {
                self.clear_grammar();
                return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
            }
            if self.mode == VimMode::Visual
                || self.pending_operator.is_some()
                || self.count.is_some()
            {
                self.mode = VimMode::Normal;
                self.cursor = self.visual_head.unwrap_or(self.cursor);
                self.leave_visual();
                self.clear_grammar();
                return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
            }
            return self.outcome(text, false, HistoryPlan::None, VimSignal::LeaveComposer);
        }

        if command == VimCommand::Invalid {
            return self.invalid_grammar(text);
        }

        if self.pending_literal.is_some() {
            return match command {
                VimCommand::Literal(literal) => self.resolve_literal(text, literal),
                _ => self.invalid_grammar(text),
            };
        }
        if self.pending_text_object.is_some()
            && !matches!(
                command,
                VimCommand::TextObject(_) | VimCommand::CountDigit(_) | VimCommand::Literal(_)
            )
        {
            return self.invalid_grammar(text);
        }
        // `guu`, `gUU` and `g~~`: the case operator's own key repeats it,
        // which Vim treats like `gugu`. In Normal mode `u` is undo and `U`
        // has no other meaning, so they arrive as those commands.
        if let Some(PendingOperator {
            operator: Operator::Case(change),
            ..
        }) = self.pending_operator
        {
            let repeated = match command {
                VimCommand::Undo => change == CaseChange::Lower,
                VimCommand::ChangeCase(key) => key == change,
                _ => false,
            };
            if repeated {
                return self.begin_or_complete_operator(text, Operator::Case(change));
            }
        }
        if self.pending_operator.is_some()
            && !matches!(
                command,
                VimCommand::Motion(_)
                    | VimCommand::BeginOperator(_)
                    | VimCommand::CountDigit(_)
                    | VimCommand::TextObjectPrefix(_)
                    | VimCommand::TextObject(_)
                    | VimCommand::BeginFind(_)
                    | VimCommand::Literal(_)
            )
        {
            return self.invalid_grammar(text);
        }

        match command {
            VimCommand::CountDigit(digit) if digit <= 9 => {
                if digit == 0 && self.count.is_none() {
                    self.apply_motion(text, Motion::LineStart)
                } else {
                    self.push_count_digit(digit);
                    self.outcome(text, false, HistoryPlan::None, VimSignal::None)
                }
            }
            VimCommand::CountDigit(_) => self.invalid_grammar(text),
            VimCommand::Motion(motion) => self.apply_motion(text, motion),
            VimCommand::BeginOperator(operator) => self.begin_or_complete_operator(text, operator),
            VimCommand::TextObjectPrefix(prefix) => {
                if self.pending_operator.is_some() || self.mode == VimMode::Visual {
                    self.pending_text_object = Some(prefix);
                    self.outcome(text, false, HistoryPlan::None, VimSignal::None)
                } else {
                    self.invalid_grammar(text)
                }
            }
            VimCommand::TextObject(object) => self.apply_text_object(text, object),
            VimCommand::EnterInsert(entry) => {
                if self.mode == VimMode::Normal && self.pending_operator.is_none() {
                    self.enter_insert(text, entry)
                } else {
                    self.invalid_grammar(text)
                }
            }
            VimCommand::OpenLine(placement) => {
                if self.mode == VimMode::Normal && self.pending_operator.is_none() {
                    self.open_line(text, placement)
                } else {
                    self.invalid_grammar(text)
                }
            }
            VimCommand::ToggleVisual => self.toggle_visual(text),
            VimCommand::ToggleVisualLine => self.toggle_visual_line(text),
            VimCommand::SwapVisualEnds => self.swap_visual_ends(text),
            VimCommand::DeleteChars => self.delete_chars(text),
            VimCommand::Paste(placement) => self.paste(text, placement),
            VimCommand::Undo => self.request_history(text, true),
            VimCommand::Redo => self.request_history(text, false),
            VimCommand::Repeat => self.repeat(text),
            VimCommand::BeginFind(kind) => {
                self.pending_literal = Some(PendingLiteral::Find(kind));
                self.outcome(text, false, HistoryPlan::None, VimSignal::None)
            }
            VimCommand::BeginReplace => {
                self.pending_literal = Some(PendingLiteral::Replace);
                self.outcome(text, false, HistoryPlan::None, VimSignal::None)
            }
            VimCommand::Literal(literal) => self.resolve_literal(text, literal),
            VimCommand::Shortcut(shortcut) => self.operator_shortcut(text, shortcut),
            VimCommand::ChangeCase(change) => self.change_case(text, change),
            VimCommand::JoinLines => self.join_lines(text),
            VimCommand::Cancel | VimCommand::Contextual(_) | VimCommand::Invalid => {
                unreachable!("handled before dispatch")
            }
        }
    }

    /// The character typed after `f`/`t`/`r`, or the object after `i`/`a`.
    fn resolve_literal(&mut self, text: &mut String, literal: char) -> VimOutcome {
        if self.pending_text_object.is_some() {
            return match TextObject::for_key(literal) {
                Some(object) => self.apply_text_object(text, object),
                None => self.invalid_grammar(text),
            };
        }
        match self.pending_literal.take() {
            Some(PendingLiteral::Find(kind)) => {
                let find = Find {
                    kind,
                    target: literal,
                    repeated: false,
                };
                self.last_find = Some(find);
                self.apply_motion(text, Motion::Find(find))
            }
            Some(PendingLiteral::Replace) => self.replace_chars(text, literal),
            None => self.invalid_grammar(text),
        }
    }

    /// A motion that cannot move leaves the cursor in place and drops any
    /// pending operator, as Vim's beep does.
    fn failed_motion(&mut self, text: &str) -> VimOutcome {
        self.clear_grammar();
        self.notice = Some(VimNotice {
            kind: NoticeKind::NoChange,
            message: "Vim motion cannot move",
        });
        self.outcome(text, false, HistoryPlan::None, VimSignal::None)
    }

    fn apply_motion(&mut self, text: &mut String, motion: Motion) -> VimOutcome {
        let (motion_count, motion_count_explicit) = self.take_count();
        let motion = match motion {
            Motion::RepeatFind | Motion::RepeatFindReversed => {
                let Some(last) = self.last_find else {
                    return self.failed_motion(text);
                };
                Motion::Find(Find {
                    kind: if motion == Motion::RepeatFindReversed {
                        last.kind.reversed()
                    } else {
                        last.kind
                    },
                    target: last.target,
                    repeated: true,
                })
            }
            motion => motion,
        };
        if let Some(pending) = self.pending_operator.take() {
            let count = self.multiplied_count(pending.count, motion_count);
            let target = MotionOrTextObject::Motion(motion);
            let change_word = pending.operator == Operator::Change
                && matches!(motion, Motion::WordForward | Motion::BigWordForward)
                && self.cursor_class(text) != LexicalClass::Whitespace;
            let endpoint = if change_word {
                self.preferred_column = None;
                let kind = if motion == Motion::WordForward {
                    WordKind::Small
                } else {
                    WordKind::Big
                };
                let destination = change_word_end(text, self.cursor, count, kind);
                Some(MotionEndpoint {
                    cursor: destination,
                    operator_boundary: destination,
                    shape: MotionShape::Characterwise,
                    inclusion: MotionInclusion::Inclusive,
                    addressed_lines: None,
                })
            } else {
                compute_motion(
                    text,
                    self.cursor,
                    motion,
                    count,
                    motion_count_explicit || pending.count != 1,
                    true,
                    &mut self.preferred_column,
                )
            };
            self.pending_text_object = None;
            let Some(endpoint) = endpoint else {
                return self.failed_motion(text);
            };
            return self.apply_operator_endpoint(
                text,
                pending.operator,
                endpoint,
                target,
                count,
                motion != Motion::MatchingBracket,
            );
        }

        let Some(endpoint) = compute_motion(
            text,
            self.cursor,
            motion,
            motion_count,
            motion_count_explicit,
            false,
            &mut self.preferred_column,
        ) else {
            return self.failed_motion(text);
        };
        self.cursor = endpoint.cursor;
        if self.mode == VimMode::Visual {
            self.visual_head = Some(endpoint.cursor);
        }
        self.outcome(text, false, HistoryPlan::None, VimSignal::None)
    }

    fn cursor_class(&self, text: &str) -> LexicalClass {
        grapheme_units(text)
            .into_iter()
            .find(|unit| unit.range.start == self.cursor)
            .map_or(LexicalClass::Whitespace, |unit| unit.class)
    }

    fn begin_or_complete_operator(&mut self, text: &mut String, operator: Operator) -> VimOutcome {
        if self.mode == VimMode::Visual {
            return self.apply_visual_operator(text, operator, None);
        }
        if let Some(pending) = self.pending_operator.take() {
            if pending.operator != operator {
                return self.invalid_grammar(text);
            }
            let (line_count, _) = self.take_count();
            let count = self.multiplied_count(pending.count, line_count);
            let map = LineMap::new(text);
            let current = map.line_index(self.cursor);
            let end = current
                .saturating_add(count.saturating_sub(1))
                .min(map.lines.len() - 1);
            // A doubled case operator first moves to the first non-blank of
            // its last line, so `g~~` starts there when that is earlier.
            let start = if matches!(operator, Operator::Case(_)) {
                min(
                    self.cursor,
                    first_non_whitespace_cursor(text, map.line(end)),
                )
            } else {
                self.cursor
            };
            return self.apply_linewise_operator(
                text,
                operator,
                current..=end,
                OperatorOrigin::Target {
                    target: MotionOrTextObject::RepeatedOperatorLine,
                    count,
                },
                start,
            );
        }
        let (count, _) = self.take_count();
        self.pending_operator = Some(PendingOperator { operator, count });
        self.pending_text_object = None;
        self.outcome(text, false, HistoryPlan::None, VimSignal::None)
    }

    fn apply_operator_endpoint(
        &mut self,
        text: &mut String,
        operator: Operator,
        endpoint: MotionEndpoint,
        target: MotionOrTextObject,
        count: usize,
        delete_rule: bool,
    ) -> VimOutcome {
        let origin = OperatorOrigin::Target { target, count };
        if endpoint.shape == MotionShape::Linewise {
            let current_line = LineMap::new(text).line_index(self.cursor);
            let lines = endpoint
                .addressed_lines
                .unwrap_or(current_line..=current_line);
            let start = min(self.cursor, endpoint.cursor);
            self.apply_linewise_operator(text, operator, lines, origin, start)
        } else {
            let range = characterwise_motion_range(text, self.cursor, &endpoint);
            let exclusive = endpoint.inclusion == MotionInclusion::Exclusive;
            self.apply_operator_span(text, operator, range, exclusive, delete_rule, origin)
        }
    }

    /// Run an operator over a characterwise range after Vim's exclusive
    /// and delete rules, which may hand it whole lines instead. Neovim
    /// leaves `%` out of the delete rule, so its callers pass false.
    fn apply_operator_span(
        &mut self,
        text: &mut String,
        operator: Operator,
        range: Range<usize>,
        exclusive: bool,
        delete_rule: bool,
        origin: OperatorOrigin,
    ) -> VimOutcome {
        let start = range.start;
        let span = if exclusive {
            exclusive_operator_span(text, range)
        } else {
            OperatorSpan::Characters(range)
        };
        let span = match span {
            OperatorSpan::Characters(range) if operator == Operator::Delete && delete_rule => {
                delete_operator_span(text, range)
            }
            span => span,
        };
        match span {
            OperatorSpan::Characters(range) => {
                self.apply_characterwise_operator(text, operator, range, origin)
            }
            OperatorSpan::Lines(lines) => {
                self.apply_linewise_operator(text, operator, lines, origin, start)
            }
        }
    }

    fn apply_characterwise_operator(
        &mut self,
        text: &mut String,
        operator: Operator,
        range: Range<usize>,
        origin: OperatorOrigin,
    ) -> VimOutcome {
        self.clear_grammar();
        // An empty change still starts Insert there, as `c0` and `ci(` on
        // `()` do in Vim; other operators have nothing to act on.
        if text.get(range.clone()).is_none() || (range.is_empty() && operator != Operator::Change) {
            self.notice = Some(VimNotice {
                kind: NoticeKind::NoChange,
                message: "Vim command made no change",
            });
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        }
        self.preferred_column = None;

        if let Operator::Case(change) = operator {
            let before = self.snapshot(text);
            let replaced = change.apply(&text[range.clone()]);
            text.replace_range(range.clone(), &replaced);
            self.cursor = normal_cursor(text, range.start);
            self.last_change = Some(origin.recipe(operator));
            return self.commit_change(text, before);
        }

        if !range.is_empty() {
            self.register = Some(RegisterSnapshot {
                text: text[range.clone()].to_owned(),
                kind: RegisterKind::Characterwise,
            });
        }
        if operator == Operator::Yank {
            self.cursor = normal_cursor(text, range.start);
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        }

        let before = self.snapshot(text);
        text.replace_range(range.clone(), "");
        self.cursor = if operator == Operator::Change {
            insertion_boundary(text, range.start)
        } else {
            normal_cursor(text, range.start)
        };
        if operator == Operator::Change {
            self.mode = VimMode::Insert;
            self.last_insertion = self.cursor;
            self.transaction = Some(InsertTransaction {
                changed: before.text != *text,
                before,
                origin: origin.transaction(),
                delta: InsertDelta::default(),
            });
            return self.outcome(text, !range.is_empty(), HistoryPlan::None, VimSignal::None);
        }

        self.mode = VimMode::Normal;
        self.last_change = Some(origin.recipe(operator));
        let after = self.snapshot(text);
        self.outcome(
            text,
            true,
            HistoryPlan::Commit { before, after },
            VimSignal::None,
        )
    }

    /// Commit a finished change, or report that it left the text as it was.
    fn commit_change(&mut self, text: &str, before: HistorySnapshot) -> VimOutcome {
        if before.text == text {
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        }
        let after = self.snapshot(text);
        self.outcome(
            text,
            true,
            HistoryPlan::Commit { before, after },
            VimSignal::None,
        )
    }

    /// `start` is where Vim leaves the cursor after a yank or case change:
    /// the earlier end of the motion, or the selection's start.
    fn apply_linewise_operator(
        &mut self,
        text: &mut String,
        operator: Operator,
        line_range: RangeInclusive<usize>,
        origin: OperatorOrigin,
        start: usize,
    ) -> VimOutcome {
        self.clear_grammar();
        let map = LineMap::new(text);
        let start_line = (*line_range.start()).min(map.lines.len() - 1);
        let end_line = (*line_range.end()).min(map.lines.len() - 1);
        let line_range = min(start_line, end_line)..=max(start_line, end_line);
        let (start_line, end_line) = (*line_range.start(), *line_range.end());
        self.preferred_column = None;

        if let Operator::Case(change) = operator {
            let range = map.line(start_line).start..map.line(end_line).end;
            let start_line_index = map.line_index(start);
            let column = grapheme_column(text, map.line(start_line_index), start);
            let before = self.snapshot(text);
            let replaced = change.apply(&text[range.clone()]);
            text.replace_range(range, &replaced);
            let changed_map = LineMap::new(text);
            self.cursor = cursor_at_column(text, changed_map.line(start_line_index), column);
            self.last_change = Some(origin.recipe(operator));
            return self.commit_change(text, before);
        }

        let delete_range =
            (operator == Operator::Delete).then(|| linewise_delete_range(text, line_range.clone()));
        if delete_range.as_ref().is_some_and(Range::is_empty) {
            self.notice = Some(VimNotice {
                kind: NoticeKind::NoChange,
                message: "line is already empty",
            });
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        }
        let captured = linewise_normalized_register(text, line_range.clone());
        self.register = Some(RegisterSnapshot {
            text: captured,
            kind: RegisterKind::Linewise,
        });
        if operator == Operator::Yank {
            self.cursor = normal_cursor(text, start);
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        }

        let before = self.snapshot(text);
        if operator == Operator::Change {
            let indentation = leading_indentation(text, map.line(start_line)).to_owned();
            let mut lines = line_contents(text);
            lines.splice(start_line..=end_line, [indentation.clone()]);
            *text = join_lines(&lines);
            let changed_map = LineMap::new(text);
            let changed_line = changed_map.line(start_line.min(changed_map.lines.len() - 1));
            self.cursor = changed_line.start + indentation.len();
            self.cursor = insertion_boundary(text, self.cursor);
            self.mode = VimMode::Insert;
            self.last_insertion = self.cursor;
            self.transaction = Some(InsertTransaction {
                before,
                origin: origin.transaction(),
                delta: InsertDelta::default(),
                changed: true,
            });
            return self.outcome(text, true, HistoryPlan::None, VimSignal::None);
        }

        let delete_range = delete_range.expect("delete operator has a delete range");
        text.replace_range(delete_range, "");
        let changed_map = LineMap::new(text);
        let cursor_line = start_line.min(changed_map.lines.len() - 1);
        self.cursor = first_non_whitespace_cursor(text, changed_map.line(cursor_line));
        self.mode = VimMode::Normal;
        self.last_change = Some(origin.recipe(operator));
        let after = self.snapshot(text);
        self.outcome(
            text,
            before.text != *text,
            HistoryPlan::Commit { before, after },
            VimSignal::None,
        )
    }

    fn apply_text_object(&mut self, text: &mut String, object: TextObject) -> VimOutcome {
        let Some(prefix) = self.pending_text_object.take() else {
            return self.invalid_grammar(text);
        };
        let (object_count, _) = self.take_count();
        let count = if let Some(pending) = self.pending_operator {
            self.multiplied_count(pending.count, object_count)
        } else {
            object_count
        };
        let Some(extent) = text_object_extent(text, self.cursor, prefix, object, count) else {
            self.clear_grammar();
            self.notice = Some(VimNotice {
                kind: NoticeKind::NoChange,
                message: "text object is unavailable",
            });
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        };
        if self.mode == VimMode::Visual {
            self.count = None;
            match extent {
                ObjectExtent::Characters { range, .. } if !range.is_empty() => {
                    self.visual_linewise = false;
                    self.visual_anchor = Some(range.start);
                    let head = previous_boundary(text, range.end);
                    self.visual_head = Some(head);
                    self.cursor = head;
                }
                ObjectExtent::Characters { .. } => {}
                ObjectExtent::Lines(lines) => {
                    let map = LineMap::new(text);
                    self.visual_linewise = true;
                    self.visual_anchor = Some(map.line(*lines.start()).start);
                    let head = map.line(*lines.end()).start;
                    self.visual_head = Some(head);
                    self.cursor = head;
                }
            }
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        }
        let Some(pending) = self.pending_operator.take() else {
            return self.invalid_grammar(text);
        };
        let origin = OperatorOrigin::Target {
            target: MotionOrTextObject::TextObject { prefix, object },
            count,
        };
        match extent {
            ObjectExtent::Characters { range, exclusive } => {
                self.apply_operator_span(text, pending.operator, range, exclusive, true, origin)
            }
            ObjectExtent::Lines(lines) => {
                let start = LineMap::new(text).line(*lines.start()).start;
                self.apply_linewise_operator(text, pending.operator, lines, origin, start)
            }
        }
    }

    /// `v`: enter characterwise Visual, leave it, or switch to it from `V`.
    fn toggle_visual(&mut self, text: &str) -> VimOutcome {
        self.clear_grammar();
        if self.mode == VimMode::Visual && self.visual_linewise {
            self.visual_linewise = false;
        } else if self.mode == VimMode::Visual {
            self.mode = VimMode::Normal;
            self.cursor = self.visual_head.unwrap_or(self.cursor);
            self.leave_visual();
        } else if self.mode == VimMode::Normal {
            self.enter_visual(text, false);
        }
        self.outcome(text, false, HistoryPlan::None, VimSignal::None)
    }

    /// `V`: enter linewise Visual, leave it, or switch to it from `v`.
    fn toggle_visual_line(&mut self, text: &str) -> VimOutcome {
        self.clear_grammar();
        if self.mode == VimMode::Visual && !self.visual_linewise {
            self.visual_linewise = true;
        } else if self.mode == VimMode::Visual {
            self.mode = VimMode::Normal;
            self.cursor = self.visual_head.unwrap_or(self.cursor);
            self.leave_visual();
        } else if self.mode == VimMode::Normal {
            self.enter_visual(text, true);
        }
        self.outcome(text, false, HistoryPlan::None, VimSignal::None)
    }

    fn enter_visual(&mut self, text: &str, linewise: bool) {
        self.mode = VimMode::Visual;
        self.cursor = normal_cursor(text, self.cursor);
        self.visual_anchor = Some(self.cursor);
        self.visual_head = Some(self.cursor);
        self.visual_linewise = linewise;
    }

    /// Visual `o`: the cursor moves to the other end of the selection.
    fn swap_visual_ends(&mut self, text: &str) -> VimOutcome {
        if self.mode != VimMode::Visual {
            return self.invalid_grammar(text);
        }
        self.clear_grammar();
        std::mem::swap(&mut self.visual_anchor, &mut self.visual_head);
        self.cursor = self.visual_head.unwrap_or(self.cursor);
        self.preferred_column = None;
        self.outcome(text, false, HistoryPlan::None, VimSignal::None)
    }

    /// Where Vim leaves the cursor after a Visual operator: the start of
    /// the selection. In linewise Visual that is column 0 of the anchor's
    /// line, unless the cursor is above it.
    fn visual_start(&self, text: &str) -> usize {
        let anchor = self.visual_anchor.unwrap_or(self.cursor);
        let head = self.visual_head.unwrap_or(self.cursor);
        if self.visual_linewise {
            let map = LineMap::new(text);
            let anchor_line = map.line(map.line_index(anchor)).start;
            if head < anchor_line {
                head
            } else {
                anchor_line
            }
        } else {
            min(anchor, head)
        }
    }

    /// Single-key operator forms; see [`OperatorShortcut`].
    fn operator_shortcut(&mut self, text: &mut String, shortcut: OperatorShortcut) -> VimOutcome {
        use OperatorShortcut::*;
        if self.mode == VimMode::Visual {
            let operator = match shortcut {
                SubstituteChars => return self.apply_visual_operator(text, Operator::Change, None),
                SubstituteLines | ChangeToLineEnd => Operator::Change,
                DeleteToLineEnd | DeleteBefore => Operator::Delete,
                YankToLineEnd => Operator::Yank,
            };
            // The capital forms act on whole lines, as `V` would.
            self.visual_linewise = true;
            return self.apply_visual_operator(text, operator, None);
        }
        let (count, _) = self.take_count();
        let (operator, motion) = match shortcut {
            SubstituteChars => (Operator::Change, Motion::Right),
            SubstituteLines => {
                self.pending_operator = Some(PendingOperator {
                    operator: Operator::Change,
                    count,
                });
                return self.begin_or_complete_operator(text, Operator::Change);
            }
            ChangeToLineEnd => (Operator::Change, Motion::LineEnd),
            DeleteToLineEnd => (Operator::Delete, Motion::LineEnd),
            YankToLineEnd => (Operator::Yank, Motion::LineEnd),
            DeleteBefore => (Operator::Delete, Motion::Left),
        };
        self.pending_operator = Some(PendingOperator { operator, count });
        self.apply_motion(text, motion)
    }

    /// `~` toggles the case of `count` graphemes and moves past them; in
    /// Visual mode `~`, `u` and `U` change the selection.
    fn change_case(&mut self, text: &mut String, change: CaseChange) -> VimOutcome {
        if self.mode == VimMode::Visual {
            return self.visual_edit(text, VisualEdit::Case(change));
        }
        if change != CaseChange::Toggle {
            return self.invalid_grammar(text);
        }
        let (count, _) = self.take_count();
        let map = LineMap::new(text);
        let graphemes = line_graphemes(text, map.line(map.line_index(self.cursor)));
        let Some(first) = graphemes
            .iter()
            .position(|range| range.start == self.cursor)
        else {
            self.notice = Some(VimNotice {
                kind: NoticeKind::NoChange,
                message: "no character to change",
            });
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        };
        let last = first.saturating_add(count).min(graphemes.len()) - 1;
        let range = graphemes[first].start..graphemes[last].end;
        let before = self.snapshot(text);
        let replaced = change.apply(&text[range.clone()]);
        text.replace_range(range.clone(), &replaced);
        self.cursor = normal_cursor(text, range.start + replaced.len());
        self.preferred_column = None;
        self.last_change = Some(RepeatRecipe::ToggleCase { count });
        self.commit_change(text, before)
    }

    /// `r`: replace `count` graphemes with the typed character. Vim fails
    /// rather than replace fewer than asked.
    fn replace_chars(&mut self, text: &mut String, replacement: char) -> VimOutcome {
        if self.mode == VimMode::Visual {
            return self.visual_edit(text, VisualEdit::Replace(replacement));
        }
        let (count, _) = self.take_count();
        let map = LineMap::new(text);
        let graphemes = line_graphemes(text, map.line(map.line_index(self.cursor)));
        let first = graphemes
            .iter()
            .position(|range| range.start == self.cursor);
        let Some(first) = first.filter(|first| first.saturating_add(count) <= graphemes.len())
        else {
            self.notice = Some(VimNotice {
                kind: NoticeKind::NoChange,
                message: "not enough characters to replace",
            });
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        };
        let range = graphemes[first].start..graphemes[first + count - 1].end;
        let before = self.snapshot(text);
        let replaced: String = std::iter::repeat_n(replacement, count).collect();
        text.replace_range(range.clone(), &replaced);
        self.cursor = normal_cursor(text, range.start + replaced.len() - replacement.len_utf8());
        self.preferred_column = None;
        self.last_change = Some(RepeatRecipe::Replace { count, replacement });
        self.commit_change(text, before)
    }

    /// `r` and the case commands on a Visual selection. Line breaks stay;
    /// the cursor goes to the start of the selection.
    fn visual_edit(&mut self, text: &mut String, edit: VisualEdit) -> VimOutcome {
        let map = LineMap::new(text);
        let start = self.visual_start(text);
        let start_line = map.line_index(start);
        let column = grapheme_column(text, map.line(start_line), start);
        let linewise = self.visual_linewise;
        let lines = self.visual_lines(text);
        let range = if linewise {
            map.line(*lines.start()).start..map.line(*lines.end()).end
        } else {
            self.selection(text).range
        };
        let grapheme_span = range_grapheme_count(text, range.clone());
        self.mode = VimMode::Normal;
        self.leave_visual();
        self.clear_grammar();
        let (replaced, operator) = match edit {
            VisualEdit::Replace(replacement) => (
                text[range.clone()]
                    .graphemes(true)
                    .map(|grapheme| if grapheme == "\n" { '\n' } else { replacement })
                    .collect::<String>(),
                VisualOperator::Replace(replacement),
            ),
            VisualEdit::Case(change) => (
                change.apply(&text[range.clone()]),
                VisualOperator::Case(change),
            ),
        };
        let before = self.snapshot(text);
        text.replace_range(range, &replaced);
        let changed_map = LineMap::new(text);
        self.cursor = cursor_at_column(text, changed_map.line(start_line), column);
        self.preferred_column = None;
        self.last_change = Some(if linewise {
            RepeatRecipe::VisualLines {
                operator,
                line_count: lines.end() - lines.start() + 1,
                register: None,
                count: 1,
                inserted: None,
            }
        } else {
            RepeatRecipe::VisualChange {
                operator,
                grapheme_span,
                register: None,
                count: 1,
                inserted: None,
            }
        });
        self.commit_change(text, before)
    }

    /// `J`: join `count` lines (at least two), or the Visual lines, with
    /// Vim's spacing: leading blanks of each joined line go, one space goes
    /// between, except after a blank or a tab, before `)`, or around an
    /// empty line. The cursor lands where the last join happened.
    fn join_lines(&mut self, text: &mut String) -> VimOutcome {
        let map = LineMap::new(text);
        let (first, mut count) = if self.mode == VimMode::Visual {
            let lines = self.visual_lines(text);
            self.mode = VimMode::Normal;
            self.leave_visual();
            (*lines.start(), (lines.end() - lines.start() + 1).max(2))
        } else {
            let (count, _) = self.take_count();
            (map.line_index(self.cursor), count.max(2))
        };
        self.clear_grammar();
        let last = map.lines.len() - 1;
        if first.saturating_add(count - 1) > last {
            if count <= 2 || first == last {
                self.notice = Some(VimNotice {
                    kind: NoticeKind::NoChange,
                    message: "no line to join",
                });
                return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
            }
            count = last - first + 1;
        }
        let first_line = map.line(first);
        let mut joined = text[first_line.start..first_line.end].to_owned();
        let mut previous_last = joined.chars().last();
        let mut join_point = 0;
        for index in first + 1..first + count {
            let line = map.line(index);
            let next = text[line.start..line.end].trim_start_matches([' ', '\t']);
            let space = !next.is_empty()
                && !next.starts_with(')')
                && !joined.is_empty()
                && !matches!(previous_last, Some(' ' | '\t'));
            join_point = joined.len();
            if space {
                joined.push(' ');
            }
            joined.push_str(next);
            previous_last = next.chars().last();
        }
        let before = self.snapshot(text);
        text.replace_range(first_line.start..map.line(first + count - 1).end, &joined);
        self.cursor = normal_cursor(text, first_line.start + join_point);
        self.preferred_column = None;
        self.last_change = Some(RepeatRecipe::Join { count });
        self.commit_change(text, before)
    }

    fn request_history(&mut self, text: &str, undo: bool) -> VimOutcome {
        let (count, _) = self.take_count();
        self.mode = VimMode::Normal;
        self.leave_visual();
        self.clear_grammar();
        let history = if undo {
            HistoryPlan::Undo { count }
        } else {
            HistoryPlan::Redo { count }
        };
        self.outcome(text, false, history, VimSignal::None)
    }

    fn insert_entry_boundary(&self, text: &str, entry: InsertEntry) -> usize {
        let map = LineMap::new(text);
        let cursor = normal_cursor(text, self.cursor);
        let line = map.line(map.line_index(cursor));
        match entry {
            InsertEntry::BeforeCursor => cursor,
            InsertEntry::AfterCursor => {
                let end = grapheme_end_at(text, cursor);
                end.min(line.end)
            }
            InsertEntry::FirstNonWhitespace => first_non_whitespace_boundary(text, line),
            InsertEntry::LineEnd => line.end,
        }
    }

    fn enter_insert(&mut self, text: &str, entry: InsertEntry) -> VimOutcome {
        self.clear_grammar();
        let before = self.snapshot(text);
        self.cursor = self.insert_entry_boundary(text, entry);
        self.last_insertion = self.cursor;
        self.mode = VimMode::Insert;
        self.leave_visual();
        self.preferred_column = None;
        self.transaction = Some(InsertTransaction {
            before,
            origin: TransactionOrigin::Insert(entry),
            delta: InsertDelta::default(),
            changed: false,
        });
        self.outcome(text, false, HistoryPlan::None, VimSignal::None)
    }

    fn open_line(&mut self, text: &mut String, placement: OpenLinePlacement) -> VimOutcome {
        self.clear_grammar();
        let before = self.snapshot(text);
        let map = LineMap::new(text);
        let line = map.line(map.line_index(self.cursor));
        let indentation = leading_indentation(text, line).to_owned();
        let cursor = match placement {
            OpenLinePlacement::Above => {
                let insertion = format!("{indentation}\n");
                text.insert_str(line.start, &insertion);
                line.start + indentation.len()
            }
            OpenLinePlacement::Below if line.full_end > line.end => {
                let insertion = format!("{indentation}\n");
                text.insert_str(line.full_end, &insertion);
                line.full_end + indentation.len()
            }
            OpenLinePlacement::Below => {
                let insertion = format!("\n{indentation}");
                text.insert_str(line.end, &insertion);
                line.end + 1 + indentation.len()
            }
        };
        self.cursor = insertion_boundary(text, cursor);
        self.last_insertion = self.cursor;
        self.mode = VimMode::Insert;
        self.preferred_column = None;
        self.transaction = Some(InsertTransaction {
            changed: before.text != *text,
            before,
            origin: TransactionOrigin::OpenLine(placement),
            delta: InsertDelta::default(),
        });
        self.outcome(text, true, HistoryPlan::None, VimSignal::None)
    }

    /// Apply one committed Insert-mode replacement.  The range belongs to the
    /// pre-edit string and `cursor_after` belongs to the resulting string.
    /// Marked-but-uncommitted IME composition must not call this method; the
    /// final commit should use [`InsertEditKind::ImeCommit`].
    pub fn insert_edit(
        &mut self,
        text: &mut String,
        range: Range<usize>,
        replacement: &str,
        cursor_after: usize,
        kind: InsertEditKind,
    ) -> VimOutcome {
        self.notice = None;
        if self.mode != VimMode::Insert || self.transaction.is_none() {
            return self.not_consumed(text);
        }
        if range.start > range.end
            || text.get(range.clone()).is_none()
            || insertion_boundary(text, range.start) != range.start
            || insertion_boundary(text, range.end) != range.end
        {
            self.notice = Some(VimNotice {
                kind: NoticeKind::InvalidGrammar,
                message: "insert edit was not on grapheme boundaries",
            });
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        }
        let cursor_before = insertion_boundary(text, self.cursor);
        let Some(start) = signed_boundary_distance(text, cursor_before, range.start) else {
            return self.invalid_grammar(text);
        };
        let Some(end) = signed_boundary_distance(text, cursor_before, range.end) else {
            return self.invalid_grammar(text);
        };
        let Some(cursor_after_bytes) = cursor_after.checked_sub(range.start) else {
            return self.invalid_grammar(text);
        };
        if cursor_after_bytes > replacement.len()
            || !replacement.is_char_boundary(cursor_after_bytes)
        {
            self.notice = Some(VimNotice {
                kind: NoticeKind::InvalidGrammar,
                message: "insert cursor escaped its committed replacement",
            });
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        }
        let old = text.clone();
        text.replace_range(range.clone(), replacement);
        let Some(cursor_after) = range
            .start
            .checked_add(cursor_after_bytes)
            .filter(|cursor| *cursor <= text.len() && text.is_char_boundary(*cursor))
        else {
            *text = old;
            self.notice = Some(VimNotice {
                kind: NoticeKind::InvalidGrammar,
                message: "insert cursor escaped its committed replacement",
            });
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        };
        let cursor_after = insertion_boundary(text, cursor_after);
        self.cursor = cursor_after;
        self.last_insertion = cursor_after;
        let changed = old != *text;
        if let Some(transaction) = &mut self.transaction
            && changed
        {
            transaction.delta.steps.push(InsertDeltaStep::Replace {
                start,
                end,
                replacement: replacement.to_owned(),
                cursor_after_bytes,
                kind,
            });
            transaction.changed = true;
        }
        self.outcome(text, changed, HistoryPlan::None, VimSignal::None)
    }

    /// Record an Insert-mode caret motion as a semantic grapheme delta.  The
    /// TextInput host should call this for cursor actions that remain inside an
    /// insertion transaction so dot can reproduce later relative edits.
    pub fn move_insert_cursor(&mut self, text: &str, cursor: usize) -> VimOutcome {
        self.notice = None;
        if self.mode != VimMode::Insert || self.transaction.is_none() {
            return self.not_consumed(text);
        }
        let old = insertion_boundary(text, self.cursor);
        let cursor = insertion_boundary(text, cursor);
        let Some(distance) = signed_boundary_distance(text, old, cursor) else {
            return self.invalid_grammar(text);
        };
        if distance != 0
            && let Some(transaction) = &mut self.transaction
        {
            transaction.delta.steps.push(InsertDeltaStep::Move {
                graphemes: distance,
            });
        }
        self.cursor = cursor;
        self.last_insertion = cursor;
        self.outcome(text, false, HistoryPlan::None, VimSignal::None)
    }

    fn recipe_from_transaction(transaction: &InsertTransaction) -> RepeatRecipe {
        match &transaction.origin {
            TransactionOrigin::Insert(entry) => RepeatRecipe::Insert {
                entry: *entry,
                inserted: transaction.delta.clone(),
            },
            TransactionOrigin::Operator { target, count } => RepeatRecipe::Operator {
                operator: Operator::Change,
                target: target.clone(),
                count: *count,
                inserted: Some(transaction.delta.clone()),
            },
            TransactionOrigin::Visual { grapheme_span } => RepeatRecipe::VisualChange {
                operator: VisualOperator::Change,
                grapheme_span: *grapheme_span,
                register: None,
                count: 1,
                inserted: Some(transaction.delta.clone()),
            },
            TransactionOrigin::VisualLines { line_count } => RepeatRecipe::VisualLines {
                operator: VisualOperator::Change,
                line_count: *line_count,
                register: None,
                count: 1,
                inserted: Some(transaction.delta.clone()),
            },
            TransactionOrigin::OpenLine(placement) => RepeatRecipe::OpenLine {
                placement: *placement,
                indentation: IndentationPolicy::CopyCurrentLine,
                inserted: transaction.delta.clone(),
            },
        }
    }

    fn finish_insert(&mut self, text: &str, update_recipe: bool) -> VimOutcome {
        let insert_boundary = insertion_boundary(text, self.cursor);
        self.last_insertion = insert_boundary;
        self.mode = VimMode::Normal;
        self.clear_grammar();
        self.preferred_column = None;
        let Some(transaction) = self.transaction.take() else {
            self.cursor = normal_cursor_before_insert_boundary(text, insert_boundary);
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        };
        // Compare the snapshots as a final safety net.  The host is expected
        // to route every committed edit through `insert_edit`, but an
        // untracked mutation must still become one undo unit rather than
        // silently disappearing from history.  It is deliberately omitted
        // from dot when no normalized delta exists.
        let changed = transaction.before.text != text;
        self.cursor = if changed {
            normal_cursor_before_insert_boundary(text, insert_boundary)
        } else {
            normal_cursor(text, transaction.before.cursor)
        };
        if !changed {
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        }

        let recipe = Self::recipe_from_transaction(&transaction);
        let incomplete_plain_insert = matches!(transaction.origin, TransactionOrigin::Insert(_))
            && transaction.delta.is_empty();
        if update_recipe && !incomplete_plain_insert {
            self.last_change = Some(recipe);
        } else if incomplete_plain_insert {
            self.notice = Some(VimNotice {
                kind: NoticeKind::InsertDeltaIncomplete,
                message: "insert change committed without a replayable delta",
            });
        }
        let after = self.snapshot(text);
        self.outcome(
            text,
            true,
            HistoryPlan::Commit {
                before: transaction.before,
                after,
            },
            VimSignal::None,
        )
    }

    fn delete_chars(&mut self, text: &mut String) -> VimOutcome {
        if self.mode == VimMode::Visual {
            return self.apply_visual_operator(text, Operator::Delete, None);
        }
        let (count, _) = self.take_count();
        let map = LineMap::new(text);
        let line = map.line(map.line_index(self.cursor));
        let graphemes = line_graphemes(text, line);
        let Some(start_index) = graphemes
            .iter()
            .position(|range| range.start == normal_cursor(text, self.cursor))
        else {
            self.notice = Some(VimNotice {
                kind: NoticeKind::NoChange,
                message: "no grapheme to delete",
            });
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        };
        let end_index = start_index.saturating_add(count).min(graphemes.len());
        let range = graphemes[start_index].start..graphemes[end_index.saturating_sub(1)].end;
        let before = self.snapshot(text);
        self.register = Some(RegisterSnapshot {
            text: text[range.clone()].to_owned(),
            kind: RegisterKind::Characterwise,
        });
        text.replace_range(range.clone(), "");
        self.cursor = normal_cursor(text, range.start);
        self.last_change = Some(RepeatRecipe::DeleteChars { count });
        self.preferred_column = None;
        let after = self.snapshot(text);
        self.outcome(
            text,
            true,
            HistoryPlan::Commit { before, after },
            VimSignal::None,
        )
    }

    fn paste(&mut self, text: &mut String, placement: PastePlacement) -> VimOutcome {
        if self.mode == VimMode::Visual {
            return self.visual_paste(text, placement);
        }
        let Some(register) = self.register.clone() else {
            self.notice = Some(VimNotice {
                kind: NoticeKind::NoChange,
                message: "unnamed register is empty",
            });
            self.count = None;
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        };
        let (count, _) = self.take_count();
        if count > MAX_EXPANSION_COUNT
            || register
                .text
                .len()
                .checked_mul(count)
                .is_none_or(|bytes| bytes > MAX_COUNTED_OUTPUT_BYTES)
        {
            return self.reject_counted_work(text, "Vim paste exceeds the safe expansion limit");
        }
        let before = self.snapshot(text);
        match register.kind {
            RegisterKind::Characterwise => {
                let map = LineMap::new(text);
                let line = map.line(map.line_index(self.cursor));
                let insertion = if placement == PastePlacement::Before {
                    normal_cursor(text, self.cursor)
                } else {
                    grapheme_end_at(text, normal_cursor(text, self.cursor)).min(line.end)
                };
                let pasted = checked_repeated_text(&register.text, count)
                    .expect("counted paste was checked before allocation");
                if pasted.is_empty() {
                    self.notice = Some(VimNotice {
                        kind: NoticeKind::NoChange,
                        message: "unnamed register is empty",
                    });
                    return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
                }
                text.insert_str(insertion, &pasted);
                // Vim leaves the cursor on the last pasted character, or on
                // the first when the text spans lines.
                self.cursor = if pasted.contains('\n') {
                    normal_cursor(text, insertion)
                } else {
                    normal_cursor(text, previous_boundary(text, insertion + pasted.len()))
                };
            }
            RegisterKind::Linewise => {
                let map = LineMap::new(text);
                let current_line = map.line_index(self.cursor);
                let insertion_line = if placement == PastePlacement::Before {
                    current_line
                } else {
                    current_line + 1
                };
                let register_body = register.text.strip_suffix('\n').unwrap_or(&register.text);
                let register_lines: Vec<String> =
                    register_body.split('\n').map(ToOwned::to_owned).collect();
                let Some(pasted_line_count) = register_lines.len().checked_mul(count) else {
                    return self
                        .reject_counted_work(text, "Vim paste exceeds the safe expansion limit");
                };
                let mut pasted_lines = Vec::with_capacity(pasted_line_count);
                for _ in 0..count {
                    pasted_lines.extend(register_lines.iter().cloned());
                }
                let mut lines = line_contents(text);
                let insertion_line = insertion_line.min(lines.len());
                lines.splice(insertion_line..insertion_line, pasted_lines);
                *text = join_lines(&lines);
                let changed_map = LineMap::new(text);
                self.cursor = first_non_whitespace_cursor(
                    text,
                    changed_map.line(insertion_line.min(changed_map.lines.len() - 1)),
                );
            }
        }
        if before.text == *text {
            self.notice = Some(VimNotice {
                kind: NoticeKind::NoChange,
                message: "paste made no change",
            });
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        }
        self.last_change = Some(RepeatRecipe::Paste {
            placement,
            register,
            count,
        });
        self.preferred_column = None;
        let after = self.snapshot(text);
        self.outcome(
            text,
            true,
            HistoryPlan::Commit { before, after },
            VimSignal::None,
        )
    }

    fn apply_visual_operator(
        &mut self,
        text: &mut String,
        operator: Operator,
        _placement: Option<PastePlacement>,
    ) -> VimOutcome {
        if let Operator::Case(change) = operator {
            return self.visual_edit(text, VisualEdit::Case(change));
        }
        if self.visual_linewise {
            let lines = self.visual_lines(text);
            let start = self.visual_start(text);
            let line_count = lines.end() - lines.start() + 1;
            self.mode = VimMode::Normal;
            self.leave_visual();
            return self.apply_linewise_operator(
                text,
                operator,
                lines,
                OperatorOrigin::VisualLines { line_count },
                start,
            );
        }
        let selection = self.selection(text);
        let range = selection.range;
        let span = range_grapheme_count(text, range.clone());
        self.clear_grammar();
        if range.is_empty() || span == 0 {
            self.mode = VimMode::Normal;
            self.leave_visual();
            self.cursor = normal_cursor(text, range.start);
            self.notice = Some(VimNotice {
                kind: NoticeKind::NoChange,
                message: "Visual selection is empty",
            });
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        }
        self.register = Some(RegisterSnapshot {
            text: text[range.clone()].to_owned(),
            kind: RegisterKind::Characterwise,
        });
        if operator == Operator::Yank {
            self.mode = VimMode::Normal;
            self.cursor = normal_cursor(text, range.start);
            self.leave_visual();
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        }

        let before = self.snapshot(text);
        text.replace_range(range.clone(), "");
        self.leave_visual();
        self.preferred_column = None;
        if operator == Operator::Change {
            self.mode = VimMode::Insert;
            self.cursor = insertion_boundary(text, range.start);
            self.last_insertion = self.cursor;
            self.transaction = Some(InsertTransaction {
                before,
                origin: TransactionOrigin::Visual {
                    grapheme_span: span,
                },
                delta: InsertDelta::default(),
                changed: true,
            });
            return self.outcome(text, true, HistoryPlan::None, VimSignal::None);
        }

        self.mode = VimMode::Normal;
        self.cursor = normal_cursor(text, range.start);
        self.last_change = Some(RepeatRecipe::VisualChange {
            operator: VisualOperator::Delete,
            grapheme_span: span,
            register: None,
            count: 1,
            inserted: None,
        });
        let after = self.snapshot(text);
        self.outcome(
            text,
            true,
            HistoryPlan::Commit { before, after },
            VimSignal::None,
        )
    }

    fn visual_paste(&mut self, text: &mut String, _placement: PastePlacement) -> VimOutcome {
        let Some(saved_register) = self.register.clone() else {
            self.notice = Some(VimNotice {
                kind: NoticeKind::NoChange,
                message: "unnamed register is empty",
            });
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        };
        if self.visual_linewise {
            return self.visual_lines_paste(text, saved_register);
        }
        let (count, _) = self.take_count();
        let range = self.selection(text).range;
        let span = range_grapheme_count(text, range.clone());
        if range.is_empty() || span == 0 {
            self.mode = VimMode::Normal;
            self.leave_visual();
            self.notice = Some(VimNotice {
                kind: NoticeKind::NoChange,
                message: "Visual selection is empty",
            });
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        }
        let Some(replacement) = checked_repeated_text(&saved_register.text, count) else {
            return self
                .reject_counted_work(text, "Vim visual paste exceeds the safe expansion limit");
        };
        let displaced = text[range.clone()].to_owned();
        if displaced == replacement {
            self.mode = VimMode::Normal;
            self.leave_visual();
            self.cursor = normal_cursor(text, range.start);
            self.preferred_column = None;
            self.notice = Some(VimNotice {
                kind: NoticeKind::NoChange,
                message: "Visual paste made no change",
            });
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        }
        let before = self.snapshot(text);
        text.replace_range(range.clone(), &replacement);
        self.register = Some(RegisterSnapshot {
            text: displaced,
            kind: RegisterKind::Characterwise,
        });
        self.mode = VimMode::Normal;
        self.leave_visual();
        self.cursor = normal_cursor(text, range.start);
        self.preferred_column = None;
        self.last_change = Some(RepeatRecipe::VisualChange {
            operator: VisualOperator::Paste,
            grapheme_span: span,
            register: Some(saved_register),
            count,
            inserted: None,
        });
        let after = self.snapshot(text);
        self.outcome(
            text,
            true,
            HistoryPlan::Commit { before, after },
            VimSignal::None,
        )
    }

    /// `p` over linewise Visual: the register replaces the selected lines,
    /// as lines even when it holds characters, and the old lines go into
    /// the register.
    fn visual_lines_paste(&mut self, text: &mut String, register: RegisterSnapshot) -> VimOutcome {
        let (count, _) = self.take_count();
        let lines = self.visual_lines(text);
        let line_count = lines.end() - lines.start() + 1;
        let body = match register.kind {
            RegisterKind::Linewise => register.text.strip_suffix('\n').unwrap_or(&register.text),
            RegisterKind::Characterwise => &register.text,
        };
        let pasted = match register.kind {
            RegisterKind::Linewise => {
                let Some(mut repeated) = checked_repeated_text(&format!("{body}\n"), count) else {
                    return self.reject_counted_work(
                        text,
                        "Vim visual paste exceeds the safe expansion limit",
                    );
                };
                repeated.pop();
                repeated
            }
            RegisterKind::Characterwise => {
                let Some(repeated) = checked_repeated_text(body, count) else {
                    return self.reject_counted_work(
                        text,
                        "Vim visual paste exceeds the safe expansion limit",
                    );
                };
                repeated
            }
        };
        let displaced = linewise_normalized_register(text, lines.clone());
        let before = self.snapshot(text);
        let mut all = line_contents(text);
        all.splice(lines.clone(), pasted.split('\n').map(ToOwned::to_owned));
        *text = join_lines(&all);
        self.register = Some(RegisterSnapshot {
            text: displaced,
            kind: RegisterKind::Linewise,
        });
        self.mode = VimMode::Normal;
        self.leave_visual();
        self.clear_grammar();
        let changed_map = LineMap::new(text);
        self.cursor = first_non_whitespace_cursor(
            text,
            changed_map.line((*lines.start()).min(changed_map.lines.len() - 1)),
        );
        self.preferred_column = None;
        self.last_change = Some(RepeatRecipe::VisualLines {
            operator: VisualOperator::Paste,
            line_count,
            register: Some(register),
            count,
            inserted: None,
        });
        self.commit_change(text, before)
    }

    /// Select `line_count` whole lines from the cursor for a repeated
    /// linewise Visual change, as far as the draft allows.
    fn prepare_visual_lines(&mut self, text: &str, line_count: usize) {
        let map = LineMap::new(text);
        let first = map.line_index(self.cursor);
        let last = first
            .saturating_add(line_count.max(1) - 1)
            .min(map.lines.len() - 1);
        self.mode = VimMode::Visual;
        self.visual_linewise = true;
        self.visual_anchor = Some(normal_cursor(text, self.cursor));
        let head = map.line(last).start;
        self.visual_head = Some(head);
        self.cursor = head;
    }

    fn replay_insert_delta(&mut self, text: &mut String, delta: &InsertDelta) -> bool {
        if self.mode != VimMode::Insert || self.transaction.is_none() {
            return false;
        }
        let original_text = text.clone();
        let original_cursor = self.cursor;
        for step in &delta.steps {
            match step {
                InsertDeltaStep::Move { graphemes } => {
                    let Some(cursor) = boundary_offset(text, self.cursor, *graphemes) else {
                        *text = original_text;
                        self.cursor = original_cursor;
                        return false;
                    };
                    self.cursor = cursor;
                }
                InsertDeltaStep::Replace {
                    start,
                    end,
                    replacement,
                    cursor_after_bytes,
                    ..
                } => {
                    let Some(start) = boundary_offset(text, self.cursor, *start) else {
                        *text = original_text;
                        self.cursor = original_cursor;
                        return false;
                    };
                    let Some(end) = boundary_offset(text, self.cursor, *end) else {
                        *text = original_text;
                        self.cursor = original_cursor;
                        return false;
                    };
                    if start > end {
                        *text = original_text;
                        self.cursor = original_cursor;
                        return false;
                    }
                    text.replace_range(start..end, replacement);
                    let Some(cursor) = start
                        .checked_add(*cursor_after_bytes)
                        .filter(|cursor| *cursor <= text.len() && text.is_char_boundary(*cursor))
                    else {
                        *text = original_text;
                        self.cursor = original_cursor;
                        return false;
                    };
                    self.cursor = insertion_boundary(text, cursor);
                    if let Some(transaction) = &mut self.transaction {
                        transaction.changed = true;
                    }
                }
            }
        }
        if let Some(transaction) = &mut self.transaction {
            transaction.delta = delta.clone();
        }
        self.last_insertion = self.cursor;
        true
    }

    fn replay_operator(
        &mut self,
        text: &mut String,
        operator: Operator,
        target: &MotionOrTextObject,
        count: usize,
        inserted: Option<&InsertDelta>,
    ) -> bool {
        self.mode = VimMode::Normal;
        self.clear_grammar();
        self.pending_operator = Some(PendingOperator { operator, count });
        match target {
            MotionOrTextObject::Motion(motion) => {
                let _ = self.apply_motion(text, *motion);
            }
            MotionOrTextObject::TextObject { prefix, object } => {
                self.pending_text_object = Some(*prefix);
                let _ = self.apply_text_object(text, *object);
            }
            MotionOrTextObject::RepeatedOperatorLine => {
                let _ = self.begin_or_complete_operator(text, operator);
            }
        }
        if operator == Operator::Change {
            if self.mode != VimMode::Insert {
                return false;
            }
            if let Some(delta) = inserted
                && !self.replay_insert_delta(text, delta)
            {
                return false;
            }
            let _ = self.finish_insert(text, false);
        }
        self.mode == VimMode::Normal
    }

    fn prepare_visual_span(&mut self, text: &str, span: usize) -> bool {
        let Some(range) = span_from_cursor(text, normal_cursor(text, self.cursor), span) else {
            return false;
        };
        self.mode = VimMode::Visual;
        self.visual_linewise = false;
        self.visual_anchor = Some(range.start);
        let head = previous_boundary(text, range.end);
        self.visual_head = Some(head);
        self.cursor = head;
        true
    }

    /// Replay a Visual operator over the selection just prepared.
    fn replay_visual_operator(
        &mut self,
        text: &mut String,
        operator: VisualOperator,
        register: Option<&RegisterSnapshot>,
        count: usize,
        inserted: Option<&InsertDelta>,
    ) -> bool {
        match operator {
            VisualOperator::Delete => {
                let _ = self.apply_visual_operator(text, Operator::Delete, None);
            }
            VisualOperator::Change => {
                let _ = self.apply_visual_operator(text, Operator::Change, None);
                if self.mode != VimMode::Insert {
                    return false;
                }
                if let Some(delta) = inserted
                    && !self.replay_insert_delta(text, delta)
                {
                    return false;
                }
                let _ = self.finish_insert(text, false);
            }
            VisualOperator::Paste => {
                let Some(register) = register else {
                    return false;
                };
                self.register = Some(register.clone());
                self.count = Some(count);
                let _ = self.visual_paste(text, PastePlacement::After);
            }
            VisualOperator::Replace(replacement) => {
                let _ = self.visual_edit(text, VisualEdit::Replace(replacement));
            }
            VisualOperator::Case(change) => {
                let _ = self.visual_edit(text, VisualEdit::Case(change));
            }
        }
        true
    }

    fn replay_recipe_once(&mut self, text: &mut String, recipe: &RepeatRecipe) -> bool {
        let before = text.clone();
        match recipe {
            RepeatRecipe::Insert { entry, inserted } => {
                let _ = self.enter_insert(text, *entry);
                if !self.replay_insert_delta(text, inserted) {
                    return false;
                }
                let _ = self.finish_insert(text, false);
            }
            RepeatRecipe::Operator {
                operator,
                target,
                count,
                inserted,
            } => {
                if !self.replay_operator(text, *operator, target, *count, inserted.as_ref()) {
                    return false;
                }
            }
            RepeatRecipe::DeleteChars { count } => {
                self.count = Some(*count);
                let _ = self.delete_chars(text);
            }
            RepeatRecipe::Paste {
                placement,
                register,
                count,
            } => {
                self.register = Some(register.clone());
                self.count = Some(*count);
                let _ = self.paste(text, *placement);
            }
            RepeatRecipe::VisualChange {
                operator,
                grapheme_span,
                register,
                count,
                inserted,
            } => {
                if !self.prepare_visual_span(text, *grapheme_span)
                    || !self.replay_visual_operator(
                        text,
                        *operator,
                        register.as_ref(),
                        *count,
                        inserted.as_ref(),
                    )
                {
                    return false;
                }
            }
            RepeatRecipe::VisualLines {
                operator,
                line_count,
                register,
                count,
                inserted,
            } => {
                self.prepare_visual_lines(text, *line_count);
                if !self.replay_visual_operator(
                    text,
                    *operator,
                    register.as_ref(),
                    *count,
                    inserted.as_ref(),
                ) {
                    return false;
                }
            }
            RepeatRecipe::OpenLine {
                placement,
                indentation: IndentationPolicy::CopyCurrentLine,
                inserted,
            } => {
                let _ = self.open_line(text, *placement);
                if !self.replay_insert_delta(text, inserted) {
                    return false;
                }
                let _ = self.finish_insert(text, false);
            }
            RepeatRecipe::Replace { count, replacement } => {
                self.count = Some(*count);
                let _ = self.replace_chars(text, *replacement);
            }
            RepeatRecipe::ToggleCase { count } => {
                self.count = Some(*count);
                let _ = self.change_case(text, CaseChange::Toggle);
            }
            RepeatRecipe::Join { count } => {
                self.count = Some(*count);
                let _ = self.join_lines(text);
            }
        }
        before != *text
    }

    fn repeat(&mut self, text: &mut String) -> VimOutcome {
        let (count, _) = self.take_count();
        let Some(recipe) = self.last_change.clone() else {
            self.notice = Some(VimNotice {
                kind: NoticeKind::RepeatUnavailable,
                message: "no Vim change to repeat",
            });
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        };
        if count > MAX_REPEAT_COUNT {
            return self.reject_counted_work(text, "Vim repeat exceeds the safe work limit");
        }
        let before = self.snapshot(text);
        let baseline = self.clone();
        let mut candidate = self.clone();
        let mut candidate_text = text.clone();
        candidate.mode = VimMode::Normal;
        candidate.transaction = None;
        candidate.leave_visual();
        candidate.clear_grammar();
        for _ in 0..count {
            if !candidate.replay_recipe_once(&mut candidate_text, &recipe) {
                *self = baseline;
                self.notice = Some(VimNotice {
                    kind: NoticeKind::RepeatFailed,
                    message: "Vim repeat could not apply at this position",
                });
                return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
            }
            if candidate_text.len().saturating_sub(text.len()) > MAX_COUNTED_OUTPUT_BYTES {
                *self = baseline;
                self.notice = Some(VimNotice {
                    kind: NoticeKind::CountCapped,
                    message: "Vim repeat exceeds the safe output limit",
                });
                return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
            }
        }
        if candidate_text == *text {
            self.notice = Some(VimNotice {
                kind: NoticeKind::RepeatFailed,
                message: "Vim repeat made no change",
            });
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        }
        candidate.last_change = Some(recipe);
        candidate.transaction = None;
        candidate.mode = VimMode::Normal;
        candidate.clear_grammar();
        candidate.notice = None;
        *self = candidate;
        *text = candidate_text;
        let after = self.snapshot(text);
        self.outcome(
            text,
            true,
            HistoryPlan::Commit { before, after },
            VimSignal::None,
        )
    }

    /// Apply a lifecycle boundary from the host.  The returned history plan
    /// is authoritative: profile/task/mouse/send integration should process a
    /// Commit before changing or clearing the caller-owned draft.
    pub fn handle_lifecycle(&mut self, text: &str, event: LifecycleEvent) -> VimOutcome {
        self.notice = None;
        let boundary_mode = if self.mode == VimMode::Disabled {
            VimMode::Disabled
        } else {
            VimMode::Normal
        };
        if event == LifecycleEvent::ExternalDraftReplacement {
            self.transaction = None;
            self.last_change = None;
            self.mode = boundary_mode;
            self.leave_visual();
            self.clear_grammar();
            self.cursor = normal_cursor(text, self.cursor);
            return self.outcome(text, false, HistoryPlan::Reset, VimSignal::None);
        }

        if event == LifecycleEvent::PopupTakeover {
            if self.mode == VimMode::Visual {
                self.cursor = self.visual_head.unwrap_or(self.cursor);
                self.mode = VimMode::Normal;
                self.leave_visual();
            }
            self.clear_grammar();
            return self.outcome(text, false, HistoryPlan::None, VimSignal::None);
        }

        let mut history = HistoryPlan::None;
        let mut changed = false;
        if self.mode == VimMode::Insert {
            let outcome = self.finish_insert(text, true);
            history = outcome.history;
            changed = outcome.text_changed;
        }
        self.leave_visual();
        self.clear_grammar();
        self.preferred_column = None;
        match event {
            LifecycleEvent::Disable => {
                self.mode = VimMode::Disabled;
                self.cursor = normal_cursor(text, self.cursor);
            }
            LifecycleEvent::TaskOrScreenSwitch | LifecycleEvent::SendOrClear => {
                self.mode = boundary_mode;
                self.cursor = normal_cursor(text, self.cursor);
            }
            LifecycleEvent::MouseCaretMove { offset } => {
                self.mode = boundary_mode;
                self.cursor = normal_cursor(text, offset);
            }
            LifecycleEvent::PopupTakeover | LifecycleEvent::ExternalDraftReplacement => {
                unreachable!("handled above")
            }
        }
        self.outcome(text, changed, history, VimSignal::None)
    }

    /// Reset after the host has installed a different draft revision.  The
    /// caller decides whether a task-local register remains compatible.
    pub fn reset_after_external_text(
        &mut self,
        text: &str,
        cursor: usize,
        preserve_register: bool,
    ) {
        self.transaction = None;
        self.last_change = None;
        if !preserve_register {
            self.register = None;
        }
        self.mode = if self.mode == VimMode::Disabled {
            VimMode::Disabled
        } else {
            VimMode::Normal
        };
        self.cursor = normal_cursor(text, cursor);
        self.last_insertion = insertion_boundary(text, cursor);
        self.leave_visual();
        self.clear_grammar();
        self.preferred_column = None;
        self.notice = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Editor {
        text: String,
        vim: VimState,
        undo: Vec<HistorySnapshot>,
        redo: Vec<HistorySnapshot>,
    }

    impl Editor {
        fn new(text: &str) -> Self {
            Self {
                text: text.to_owned(),
                vim: VimState::at(text, 0),
                undo: Vec::new(),
                redo: Vec::new(),
            }
        }

        fn at(text: &str, cursor: usize) -> Self {
            let mut editor = Self::new(text);
            editor.vim.set_cursor(&editor.text, cursor);
            editor
        }

        fn command(&mut self, command: VimCommand) -> VimOutcome {
            let outcome = self.vim.handle_command(&mut self.text, command);
            self.apply_history(outcome.history.clone());
            outcome
        }

        fn apply_history(&mut self, plan: HistoryPlan) {
            match plan {
                HistoryPlan::None | HistoryPlan::Reset => {}
                HistoryPlan::Commit { before, after } => {
                    assert_eq!(after.text, self.text);
                    self.undo.push(before);
                    self.redo.clear();
                }
                HistoryPlan::Undo { count } => {
                    for _ in 0..count {
                        let Some(previous) = self.undo.pop() else {
                            break;
                        };
                        // As in the composer, redo returns to where the
                        // change began.
                        self.redo.push(HistorySnapshot {
                            text: self.text.clone(),
                            cursor: previous.cursor.min(self.text.len()),
                        });
                        self.text = previous.text;
                        self.vim.sync_after_history(&self.text, previous.cursor);
                    }
                }
                HistoryPlan::Redo { count } => {
                    for _ in 0..count {
                        let Some(next) = self.redo.pop() else {
                            break;
                        };
                        self.undo.push(HistorySnapshot {
                            text: self.text.clone(),
                            cursor: self.vim.cursor(),
                        });
                        self.text = next.text;
                        self.vim.sync_after_history(&self.text, next.cursor);
                    }
                }
            }
        }

        fn insert(&mut self, value: &str) {
            self.insert_kind(value, InsertEditKind::Text);
        }

        fn insert_kind(&mut self, value: &str, kind: InsertEditKind) {
            let cursor = self.vim.cursor();
            let outcome = self.vim.insert_edit(
                &mut self.text,
                cursor..cursor,
                value,
                cursor + value.len(),
                kind,
            );
            assert!(outcome.consumed);
            self.apply_history(outcome.history);
        }

        fn replace(
            &mut self,
            range: Range<usize>,
            value: &str,
            cursor_after: usize,
            kind: InsertEditKind,
        ) {
            let outcome = self
                .vim
                .insert_edit(&mut self.text, range, value, cursor_after, kind);
            assert!(outcome.consumed);
            self.apply_history(outcome.history);
        }

        fn backspace(&mut self) {
            let cursor = self.vim.cursor();
            let start = previous_boundary(&self.text, cursor);
            self.replace(start..cursor, "", start, InsertEditKind::Backspace);
        }

        fn escape(&mut self) -> VimOutcome {
            self.command(VimCommand::Cancel)
        }

        fn visual_text(&self) -> &str {
            let selection = self.vim.selection(&self.text);
            &self.text[selection.range]
        }

        fn count(&mut self, digits: &str) {
            for digit in digits.bytes() {
                self.command(VimCommand::CountDigit(digit - b'0'));
            }
        }

        fn operator_motion(&mut self, operator: Operator, motion: Motion) {
            self.command(VimCommand::BeginOperator(operator));
            self.command(VimCommand::Motion(motion));
        }

        fn repeated_operator(&mut self, operator: Operator) {
            self.command(VimCommand::BeginOperator(operator));
            self.command(VimCommand::BeginOperator(operator));
        }
    }

    #[test]
    fn grapheme_horizontal_motion_never_splits_unicode() {
        let mut editor = Editor::new("a\u{301}🙂x");
        editor.command(VimCommand::Motion(Motion::Right));
        assert_eq!(editor.vim.cursor(), "a\u{301}".len());
        editor.command(VimCommand::Motion(Motion::Right));
        assert_eq!(editor.vim.cursor(), "a\u{301}🙂".len());
        editor.command(VimCommand::Motion(Motion::Left));
        assert_eq!(editor.vim.cursor(), "a\u{301}".len());

        let mut family = Editor::new("👨‍👩‍👧‍👦!");
        family.command(VimCommand::Motion(Motion::Right));
        assert_eq!(family.vim.cursor(), "👨‍👩‍👧‍👦".len());
    }

    #[test]
    fn empty_document_and_empty_lines_use_stable_sentinels() {
        let mut editor = Editor::new("");
        for motion in [
            Motion::Left,
            Motion::Right,
            Motion::Up,
            Motion::Down,
            Motion::LineStart,
            Motion::LineEnd,
            Motion::FirstLine,
            Motion::LastLine,
        ] {
            editor.command(VimCommand::Motion(motion));
            assert_eq!(editor.vim.cursor(), 0);
        }
        editor.command(VimCommand::ToggleVisual);
        assert_eq!(editor.vim.selection(&editor.text).range, 0..0);
        let outcome = editor.command(VimCommand::DeleteChars);
        assert!(!outcome.text_changed);
        assert!(editor.undo.is_empty());

        let mut multiline = Editor::at("a\n\nb", 2);
        assert_eq!(multiline.vim.cursor(), 2);
        multiline.command(VimCommand::Motion(Motion::LineEnd));
        assert_eq!(multiline.vim.cursor(), 2);
    }

    #[test]
    fn vertical_motion_preserves_preferred_grapheme_column() {
        let mut editor = Editor::at("abcd\nx\nwxyz", 3);
        editor.command(VimCommand::Motion(Motion::Down));
        assert_eq!(editor.vim.cursor(), 5);
        editor.command(VimCommand::Motion(Motion::Down));
        assert_eq!(editor.vim.cursor(), 10);
        editor.command(VimCommand::Motion(Motion::Up));
        assert_eq!(editor.vim.cursor(), 5);
        editor.command(VimCommand::Motion(Motion::Left));
        editor.command(VimCommand::Motion(Motion::Down));
        assert_eq!(editor.vim.cursor(), 7);
    }

    #[test]
    fn zero_rule_and_multi_digit_counts_are_synchronous() {
        let text = (0..14)
            .map(|n| format!("{n}{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut editor = Editor::at(&text, 4);
        editor.command(VimCommand::Contextual(ContextualToken::ZeroOrLineStart));
        assert_eq!(editor.vim.cursor(), 3);

        editor.command(VimCommand::CountDigit(1));
        editor.command(VimCommand::Contextual(ContextualToken::ZeroOrLineStart));
        assert_eq!(editor.vim.status().count, Some(10));
        editor.command(VimCommand::Motion(Motion::Down));
        assert_eq!(
            LineMap::new(&editor.text).line_index(editor.vim.cursor()),
            11
        );
        assert_eq!(editor.vim.status().count, None);
    }

    #[test]
    fn count_parsing_and_multiplication_cap_without_wrapping() {
        let mut editor = Editor::new("a b");
        for _ in 0..8 {
            editor.command(VimCommand::CountDigit(9));
        }
        assert_eq!(editor.vim.status().count, Some(MAX_COUNT));
        assert_eq!(
            editor
                .vim
                .status()
                .notice
                .as_ref()
                .map(|notice| notice.kind),
            Some(NoticeKind::CountCapped)
        );
        editor.command(VimCommand::BeginOperator(Operator::Delete));
        editor.count("999999");
        editor.command(VimCommand::Motion(Motion::WordForward));
        assert!(editor.text.len() <= 3);
    }

    #[test]
    fn first_and_last_line_motions_honor_optional_one_based_count() {
        let mut editor = Editor::at("a\n  b\nc\nd", 5);
        editor.command(VimCommand::Motion(Motion::FirstLine));
        assert_eq!(editor.vim.cursor(), 0);
        editor.command(VimCommand::Motion(Motion::LastLine));
        assert_eq!(editor.vim.cursor(), 8);
        editor.command(VimCommand::CountDigit(2));
        editor.command(VimCommand::Motion(Motion::LastLine));
        assert_eq!(editor.vim.cursor(), 4);
        editor.command(VimCommand::CountDigit(3));
        editor.command(VimCommand::Motion(Motion::FirstLine));
        assert_eq!(editor.vim.cursor(), 6);
    }

    #[test]
    fn lexical_classes_cover_words_punctuation_whitespace_and_newlines() {
        let mut editor = Editor::new("one,  two\nthree");
        editor.command(VimCommand::Motion(Motion::WordForward));
        assert_eq!(&editor.text[editor.vim.cursor()..], ",  two\nthree");
        editor.command(VimCommand::Motion(Motion::WordForward));
        assert_eq!(&editor.text[editor.vim.cursor()..], "two\nthree");
        editor.command(VimCommand::Motion(Motion::WordForward));
        assert_eq!(&editor.text[editor.vim.cursor()..], "three");
        editor.command(VimCommand::Motion(Motion::WordBackward));
        assert_eq!(&editor.text[editor.vim.cursor()..], "two\nthree");
        editor.command(VimCommand::Motion(Motion::WordEnd));
        assert_eq!(&editor.text[editor.vim.cursor()..], "o\nthree");
    }

    #[test]
    fn operator_motion_inclusivity_matches_w_e_and_line_end() {
        let mut dw = Editor::new("one two");
        dw.operator_motion(Operator::Delete, Motion::WordForward);
        assert_eq!(dw.text, "two");

        let mut de = Editor::new("one two");
        de.operator_motion(Operator::Delete, Motion::WordEnd);
        assert_eq!(de.text, " two");

        let mut dollar = Editor::at("abc def\nnext", 4);
        dollar.operator_motion(Operator::Delete, Motion::LineEnd);
        assert_eq!(dollar.text, "abc \nnext");

        let mut left = Editor::at("abc", 1);
        left.operator_motion(Operator::Delete, Motion::Left);
        assert_eq!(left.text, "bc");
    }

    #[test]
    fn operator_and_motion_counts_multiply_once() {
        let mut editor = Editor::new("one two three four five six seven");
        editor.command(VimCommand::CountDigit(2));
        editor.command(VimCommand::BeginOperator(Operator::Delete));
        editor.command(VimCommand::CountDigit(3));
        editor.command(VimCommand::Motion(Motion::WordForward));
        assert_eq!(editor.text, "seven");
        assert!(matches!(
            editor.vim.last_change(),
            Some(RepeatRecipe::Operator { count: 6, .. })
        ));
    }

    #[test]
    fn cw_uses_ce_on_text_but_normal_w_on_whitespace() {
        let mut word = Editor::new("one two");
        word.operator_motion(Operator::Change, Motion::WordForward);
        assert_eq!(word.text, " two");
        assert_eq!(word.vim.mode(), VimMode::Insert);
        word.insert("X");
        word.escape();
        assert_eq!(word.text, "X two");

        let mut whitespace = Editor::at("one   two", 3);
        whitespace.operator_motion(Operator::Change, Motion::WordForward);
        assert_eq!(whitespace.text, "onetwo");
        whitespace.insert(" ");
        whitespace.escape();
        assert_eq!(whitespace.text, "one two");

        let mut final_grapheme = Editor::at("one two", 2);
        final_grapheme.operator_motion(Operator::Change, Motion::WordForward);
        final_grapheme.insert("X");
        final_grapheme.escape();
        assert_eq!(final_grapheme.text, "onX two");
    }

    #[test]
    fn dw_at_document_and_newline_boundaries_is_predictable() {
        let mut final_word = Editor::new("one\n");
        final_word.operator_motion(Operator::Delete, Motion::WordForward);
        assert_eq!(final_word.text, "\n");

        let mut crossing = Editor::new("one\nnext");
        crossing.operator_motion(Operator::Delete, Motion::WordForward);
        assert_eq!(crossing.text, "\nnext");

        let mut counted = Editor::new("one\nnext more");
        counted.command(VimCommand::BeginOperator(Operator::Delete));
        counted.command(VimCommand::CountDigit(2));
        counted.command(VimCommand::Motion(Motion::WordForward));
        assert_eq!(counted.text, "more");
    }

    #[test]
    fn repeated_line_delete_handles_first_last_only_and_final_newline() {
        let mut first = Editor::new("a\nb\nc");
        first.repeated_operator(Operator::Delete);
        assert_eq!(first.text, "b\nc");
        assert_eq!(first.vim.unnamed_register().unwrap().text, "a\n");
        assert_eq!(first.vim.cursor(), 0);

        let mut last = Editor::at("a\nb", 2);
        last.repeated_operator(Operator::Delete);
        assert_eq!(last.text, "a");
        assert_eq!(last.vim.unnamed_register().unwrap().text, "b\n");
        assert_eq!(last.vim.cursor(), 0);

        let mut trailing = Editor::at("a\nb\n", 2);
        trailing.repeated_operator(Operator::Delete);
        assert_eq!(trailing.text, "a\n");

        let mut only = Editor::new("a");
        only.repeated_operator(Operator::Delete);
        assert_eq!(only.text, "");
        assert_eq!(only.vim.cursor(), 0);

        let mut empty = Editor::new("");
        empty.repeated_operator(Operator::Delete);
        assert_eq!(empty.text, "");
        assert!(empty.undo.is_empty());
        assert!(empty.vim.last_change().is_none());
        assert!(empty.vim.unnamed_register().is_none());
    }

    #[test]
    fn d2d_deletes_two_lines_and_consumes_counts_once() {
        let mut editor = Editor::new("a\nb\nc");
        editor.command(VimCommand::BeginOperator(Operator::Delete));
        editor.command(VimCommand::CountDigit(2));
        editor.command(VimCommand::BeginOperator(Operator::Delete));
        assert_eq!(editor.text, "c");
        assert_eq!(editor.vim.unnamed_register().unwrap().text, "a\nb\n");
    }

    #[test]
    fn cc_preserves_indentation_and_is_one_transaction() {
        let mut editor = Editor::new("  one\nnext");
        editor.repeated_operator(Operator::Change);
        assert_eq!(editor.text, "  \nnext");
        assert_eq!(editor.vim.mode(), VimMode::Insert);
        assert_eq!(editor.vim.cursor(), 2);
        editor.insert("value");
        let outcome = editor.escape();
        assert!(matches!(outcome.history, HistoryPlan::Commit { .. }));
        assert_eq!(editor.text, "  value\nnext");
        assert_eq!(editor.undo.len(), 1);
        editor.command(VimCommand::Undo);
        assert_eq!(editor.text, "  one\nnext");

        let mut final_line = Editor::at("a\n  b", 4);
        final_line.repeated_operator(Operator::Change);
        assert_eq!(final_line.text, "a\n  ");
    }

    #[test]
    fn yy_is_linewise_and_does_not_create_history_or_dot_change() {
        let mut editor = Editor::new("a\nb");
        editor.repeated_operator(Operator::Yank);
        assert_eq!(editor.text, "a\nb");
        assert_eq!(
            editor.vim.unnamed_register().unwrap().kind,
            RegisterKind::Linewise
        );
        assert_eq!(editor.vim.unnamed_register().unwrap().text, "a\n");
        assert!(editor.undo.is_empty());
        assert!(editor.vim.last_change().is_none());
    }

    #[test]
    fn inner_and_around_word_cover_words_punctuation_and_whitespace() {
        let mut word = Editor::at("one  two!", 1);
        word.command(VimCommand::ToggleVisual);
        word.command(VimCommand::TextObjectPrefix(TextObjectPrefix::Inner));
        word.command(VimCommand::TextObject(TextObject::Word));
        assert_eq!(word.visual_text(), "one");

        let mut around = Editor::new("one  two");
        around.command(VimCommand::ToggleVisual);
        around.command(VimCommand::TextObjectPrefix(TextObjectPrefix::Around));
        around.command(VimCommand::TextObject(TextObject::Word));
        assert_eq!(around.visual_text(), "one  ");

        let mut whitespace = Editor::at("one  two", 3);
        whitespace.command(VimCommand::ToggleVisual);
        whitespace.command(VimCommand::TextObjectPrefix(TextObjectPrefix::Inner));
        whitespace.command(VimCommand::TextObject(TextObject::Word));
        assert_eq!(whitespace.visual_text(), "  ");

        let mut punctuation = Editor::at("one... two", 4);
        punctuation.command(VimCommand::ToggleVisual);
        punctuation.command(VimCommand::TextObjectPrefix(TextObjectPrefix::Inner));
        punctuation.command(VimCommand::TextObject(TextObject::Word));
        assert_eq!(punctuation.visual_text(), "...");
    }

    #[test]
    fn operator_and_object_counts_multiply_and_visual_counts_expand() {
        let mut inner = Editor::new("one two three four five six seven");
        inner.command(VimCommand::CountDigit(2));
        inner.command(VimCommand::BeginOperator(Operator::Delete));
        inner.command(VimCommand::CountDigit(3));
        inner.command(VimCommand::TextObjectPrefix(TextObjectPrefix::Inner));
        inner.command(VimCommand::TextObject(TextObject::Word));
        assert_eq!(inner.text, " seven");
        assert!(matches!(
            inner.vim.last_change(),
            Some(RepeatRecipe::Operator { count: 6, .. })
        ));

        let mut around = Editor::new("one two three");
        around.command(VimCommand::CountDigit(2));
        around.command(VimCommand::BeginOperator(Operator::Delete));
        around.command(VimCommand::TextObjectPrefix(TextObjectPrefix::Around));
        around.command(VimCommand::TextObject(TextObject::Word));
        assert_eq!(around.text, "three");

        let mut visual = Editor::new("one two three");
        visual.command(VimCommand::ToggleVisual);
        visual.command(VimCommand::CountDigit(2));
        visual.command(VimCommand::TextObjectPrefix(TextObjectPrefix::Inner));
        visual.command(VimCommand::TextObject(TextObject::Word));
        assert_eq!(visual.visual_text(), "one two");
    }

    #[test]
    fn characterwise_paste_before_after_and_count() {
        let mut after = Editor::new("ab");
        after.vim.register = Some(RegisterSnapshot {
            text: "🙂".to_owned(),
            kind: RegisterKind::Characterwise,
        });
        after.command(VimCommand::Paste(PastePlacement::After));
        assert_eq!(after.text, "a🙂b");

        let mut before = Editor::new("ab");
        before.vim.register = Some(RegisterSnapshot {
            text: "X".to_owned(),
            kind: RegisterKind::Characterwise,
        });
        before.count("3");
        before.command(VimCommand::Paste(PastePlacement::Before));
        assert_eq!(before.text, "XXXab");
    }

    #[test]
    fn excessive_counted_paste_is_a_surfaced_no_op() {
        let mut editor = Editor::new("ab");
        editor.vim.register = Some(RegisterSnapshot {
            text: "X".to_owned(),
            kind: RegisterKind::Characterwise,
        });
        editor.count("999999");
        let outcome = editor.command(VimCommand::Paste(PastePlacement::After));

        assert_eq!(editor.text, "ab");
        assert!(editor.undo.is_empty());
        assert_eq!(editor.vim.status().count, None);
        assert_eq!(
            outcome.status.notice.as_ref().map(|notice| notice.kind),
            Some(NoticeKind::CountCapped)
        );
    }

    #[test]
    fn linewise_paste_is_stable_with_and_without_final_newline() {
        let register = RegisterSnapshot {
            text: "x\n".to_owned(),
            kind: RegisterKind::Linewise,
        };
        let mut no_newline = Editor::new("a");
        no_newline.vim.register = Some(register.clone());
        no_newline.command(VimCommand::Paste(PastePlacement::After));
        assert_eq!(no_newline.text, "a\nx");
        assert_eq!(&no_newline.text[no_newline.vim.cursor()..], "x");

        let mut final_newline = Editor::new("a\n");
        final_newline.vim.register = Some(register.clone());
        final_newline.command(VimCommand::Paste(PastePlacement::After));
        assert_eq!(final_newline.text, "a\nx\n");

        let mut above = Editor::new("a");
        above.vim.register = Some(register);
        above.command(VimCommand::Paste(PastePlacement::Before));
        assert_eq!(above.text, "x\na");
        assert_eq!(above.vim.cursor(), 0);
    }

    #[test]
    fn visual_selection_is_inclusive_forward_reverse_and_unicode_safe() {
        let text = "a\u{301}🙂z";
        let mut forward = Editor::new(text);
        forward.command(VimCommand::ToggleVisual);
        assert_eq!(forward.visual_text(), "a\u{301}");
        forward.command(VimCommand::Motion(Motion::Right));
        assert_eq!(forward.visual_text(), "a\u{301}🙂");

        let mut reverse = Editor::at(text, "a\u{301}".len());
        reverse.command(VimCommand::ToggleVisual);
        reverse.command(VimCommand::Motion(Motion::Left));
        assert_eq!(reverse.visual_text(), "a\u{301}🙂");
        assert!(reverse.vim.selection(&reverse.text).reversed);
        reverse.command(VimCommand::BeginOperator(Operator::Yank));
        assert_eq!(reverse.vim.unnamed_register().unwrap().text, "a\u{301}🙂");
        assert_eq!(reverse.vim.cursor(), 0);
    }

    #[test]
    fn visual_delete_and_change_use_normalized_grapheme_spans() {
        let mut delete = Editor::new("🙂ab");
        delete.command(VimCommand::ToggleVisual);
        delete.command(VimCommand::Motion(Motion::Right));
        delete.command(VimCommand::BeginOperator(Operator::Delete));
        assert_eq!(delete.text, "b");
        assert!(matches!(
            delete.vim.last_change(),
            Some(RepeatRecipe::VisualChange {
                grapheme_span: 2,
                operator: VisualOperator::Delete,
                ..
            })
        ));

        let mut change = Editor::at("ab🙂", 1);
        change.command(VimCommand::ToggleVisual);
        change.command(VimCommand::Motion(Motion::Right));
        change.command(VimCommand::BeginOperator(Operator::Change));
        change.insert("X");
        change.escape();
        assert_eq!(change.text, "aX");
    }

    #[test]
    fn visual_paste_swaps_the_displaced_text_into_the_register() {
        let mut editor = Editor::new("a\u{301}🙂z");
        editor.vim.register = Some(RegisterSnapshot {
            text: "X".to_owned(),
            kind: RegisterKind::Characterwise,
        });
        editor.command(VimCommand::ToggleVisual);
        editor.command(VimCommand::Motion(Motion::Right));
        editor.command(VimCommand::Paste(PastePlacement::After));
        assert_eq!(editor.text, "Xz");
        assert_eq!(editor.vim.unnamed_register().unwrap().text, "a\u{301}🙂");
        assert!(matches!(
            editor.vim.last_change(),
            Some(RepeatRecipe::VisualChange {
                operator: VisualOperator::Paste,
                ..
            })
        ));
    }

    #[test]
    fn insert_and_append_entries_use_grapheme_boundaries() {
        let mut before = Editor::new("🙂a");
        before.command(VimCommand::EnterInsert(InsertEntry::BeforeCursor));
        before.insert("X");
        before.escape();
        assert_eq!(before.text, "X🙂a");

        let mut after = Editor::new("🙂a");
        after.command(VimCommand::EnterInsert(InsertEntry::AfterCursor));
        after.insert("X");
        after.escape();
        assert_eq!(after.text, "🙂Xa");

        let mut line = Editor::new("  value");
        line.command(VimCommand::EnterInsert(InsertEntry::FirstNonWhitespace));
        assert_eq!(line.vim.cursor(), 2);
        line.escape();
        line.command(VimCommand::EnterInsert(InsertEntry::LineEnd));
        assert_eq!(line.vim.cursor(), line.text.len());
    }

    #[test]
    fn open_line_copies_indentation_above_and_below() {
        let mut below = Editor::new("  a\nnext");
        below.command(VimCommand::OpenLine(OpenLinePlacement::Below));
        assert_eq!(below.text, "  a\n  \nnext");
        assert_eq!(below.vim.cursor(), 6);
        below.insert("b");
        below.escape();
        assert_eq!(below.text, "  a\n  b\nnext");

        let mut above = Editor::at("first\n  second", 8);
        above.command(VimCommand::OpenLine(OpenLinePlacement::Above));
        assert_eq!(above.text, "first\n  \n  second");
        above.insert("new");
        above.escape();
        assert_eq!(above.text, "first\n  new\n  second");
    }

    #[test]
    fn undo_and_redo_plan_complete_vim_transactions() {
        let mut editor = Editor::new("one two");
        editor.command(VimCommand::BeginOperator(Operator::Change));
        editor.command(VimCommand::Contextual(ContextualToken::InnerOrInsert));
        editor.command(VimCommand::Contextual(ContextualToken::WordOrTextObject));
        editor.insert("hello");
        editor.escape();
        assert_eq!(editor.text, "hello two");
        assert_eq!(editor.undo.len(), 1);

        editor.command(VimCommand::Undo);
        assert_eq!(editor.text, "one two");
        editor.command(VimCommand::Redo);
        assert_eq!(editor.text, "hello two");
    }

    #[test]
    fn dot_repeats_semantic_ciw_on_a_differently_sized_word() {
        let mut editor = Editor::new("one elephant");
        editor.command(VimCommand::BeginOperator(Operator::Change));
        editor.command(VimCommand::TextObjectPrefix(TextObjectPrefix::Inner));
        editor.command(VimCommand::TextObject(TextObject::Word));
        editor.insert("X");
        editor.escape();
        assert_eq!(editor.text, "X elephant");
        editor.command(VimCommand::Motion(Motion::WordForward));
        editor.command(VimCommand::Repeat);
        assert_eq!(editor.text, "X X");
    }

    #[test]
    fn dot_repeats_insert_append_and_open_line_recipes() {
        let mut insert = Editor::new("a b");
        insert.command(VimCommand::EnterInsert(InsertEntry::BeforeCursor));
        insert.insert("!");
        insert.escape();
        insert.command(VimCommand::Motion(Motion::WordForward));
        insert.command(VimCommand::Motion(Motion::WordForward));
        insert.command(VimCommand::Repeat);
        assert_eq!(insert.text, "!a !b");

        let mut append = Editor::new("one\ntwo");
        append.command(VimCommand::EnterInsert(InsertEntry::LineEnd));
        append.insert("!");
        append.escape();
        append.command(VimCommand::Motion(Motion::Down));
        append.command(VimCommand::Repeat);
        assert_eq!(append.text, "one!\ntwo!");

        let mut open = Editor::new("a");
        open.command(VimCommand::OpenLine(OpenLinePlacement::Below));
        open.insert("hello");
        open.escape();
        open.command(VimCommand::Motion(Motion::Up));
        open.command(VimCommand::Repeat);
        assert_eq!(open.text, "a\nhello\nhello");
    }

    #[test]
    fn dot_repeats_counted_delete_and_captured_paste() {
        let mut delete = Editor::new("abcdefgh");
        delete.count("3");
        delete.command(VimCommand::DeleteChars);
        assert_eq!(delete.text, "defgh");
        delete.command(VimCommand::Repeat);
        assert_eq!(delete.text, "gh");

        let mut paste = Editor::new("ab");
        paste.vim.register = Some(RegisterSnapshot {
            text: "X".to_owned(),
            kind: RegisterKind::Characterwise,
        });
        paste.command(VimCommand::Paste(PastePlacement::After));
        paste.command(VimCommand::Motion(Motion::Right));
        paste.command(VimCommand::Repeat);
        assert_eq!(paste.text, "aXbX");
    }

    #[test]
    fn excessive_counted_dot_is_a_surfaced_no_op() {
        let mut editor = Editor::new("abc");
        editor.command(VimCommand::DeleteChars);
        let text = editor.text.clone();
        let history_len = editor.undo.len();
        let recipe = editor.vim.last_change().cloned();

        editor.count("999999");
        let outcome = editor.command(VimCommand::Repeat);

        assert_eq!(editor.text, text);
        assert_eq!(editor.undo.len(), history_len);
        assert_eq!(editor.vim.last_change(), recipe.as_ref());
        assert_eq!(editor.vim.status().count, None);
        assert_eq!(
            outcome.status.notice.as_ref().map(|notice| notice.kind),
            Some(NoticeKind::CountCapped)
        );
    }

    #[test]
    fn dot_repeats_visual_delete_change_and_saved_register_paste() {
        let mut delete = Editor::new("abcdef");
        delete.command(VimCommand::ToggleVisual);
        delete.command(VimCommand::Motion(Motion::Right));
        delete.command(VimCommand::BeginOperator(Operator::Delete));
        delete.command(VimCommand::Repeat);
        assert_eq!(delete.text, "ef");

        let mut change = Editor::new("abcdef");
        change.command(VimCommand::ToggleVisual);
        change.command(VimCommand::Motion(Motion::Right));
        change.command(VimCommand::BeginOperator(Operator::Change));
        change.insert("X");
        change.escape();
        change.command(VimCommand::Motion(Motion::Right));
        change.command(VimCommand::Repeat);
        assert_eq!(change.text, "XXef");

        let mut paste = Editor::new("abcdef");
        paste.vim.register = Some(RegisterSnapshot {
            text: "Z".to_owned(),
            kind: RegisterKind::Characterwise,
        });
        paste.command(VimCommand::ToggleVisual);
        paste.command(VimCommand::Motion(Motion::Right));
        paste.command(VimCommand::Paste(PastePlacement::After));
        assert_eq!(paste.vim.unnamed_register().unwrap().text, "ab");
        paste.command(VimCommand::Motion(Motion::Right));
        paste.command(VimCommand::Repeat);
        assert_eq!(paste.text, "ZZef");
    }

    #[test]
    fn insert_delta_records_backspace_replacement_and_ime_commit() {
        let mut backspace = Editor::new("a\nb");
        backspace.command(VimCommand::EnterInsert(InsertEntry::LineEnd));
        backspace.insert("xy");
        backspace.backspace();
        backspace.escape();
        assert_eq!(backspace.text, "ax\nb");
        let delta = match backspace.vim.last_change().unwrap() {
            RepeatRecipe::Insert { inserted, .. } => inserted,
            other => panic!("unexpected recipe: {other:?}"),
        };
        assert!(delta.steps.iter().any(|step| matches!(
            step,
            InsertDeltaStep::Replace {
                kind: InsertEditKind::Backspace,
                ..
            }
        )));
        backspace.command(VimCommand::Motion(Motion::Down));
        backspace.command(VimCommand::Repeat);
        assert_eq!(backspace.text, "ax\nbx");

        let mut replacement = Editor::new("ab\ncd");
        replacement.command(VimCommand::EnterInsert(InsertEntry::BeforeCursor));
        replacement.replace(0..1, "🙂", "🙂".len(), InsertEditKind::ImeCommit);
        replacement.escape();
        replacement.command(VimCommand::Motion(Motion::Down));
        replacement.command(VimCommand::Repeat);
        assert_eq!(replacement.text, "🙂b\n🙂d");
    }

    #[test]
    fn insert_and_repeat_accept_graphemes_that_merge_with_their_left_neighbor() {
        for (base, combining) in [
            ("a", "\u{301}"),
            ("👍", "🏽"),
            ("👩", "\u{200d}💻"),
            ("क", "ि"),
        ] {
            let mut editor = Editor::new(&format!("{base}\n{base}"));
            editor.command(VimCommand::EnterInsert(InsertEntry::LineEnd));
            editor.insert_kind(combining, InsertEditKind::ImeCommit);
            editor.escape();
            assert_eq!(editor.text, format!("{base}{combining}\n{base}"));

            editor.command(VimCommand::Motion(Motion::Down));
            editor.command(VimCommand::Repeat);
            assert_eq!(editor.text, format!("{base}{combining}\n{base}{combining}"));
        }
    }

    #[test]
    fn cursor_movement_inside_insert_is_part_of_the_normalized_delta() {
        let mut editor = Editor::new("ab\ncd");
        editor.command(VimCommand::EnterInsert(InsertEntry::LineEnd));
        let moved = editor.vim.cursor() - 1;
        editor.vim.move_insert_cursor(&editor.text, moved);
        editor.insert("X");
        editor.escape();
        assert_eq!(editor.text, "aXb\ncd");
        editor.command(VimCommand::Motion(Motion::Down));
        editor.command(VimCommand::Repeat);
        assert_eq!(editor.text, "aXb\ncXd");
    }

    #[test]
    fn counted_dot_is_one_undo_unit_and_does_not_replace_recipe() {
        let mut editor = Editor::new("abcdefg");
        editor.command(VimCommand::DeleteChars);
        let recipe = editor.vim.last_change().cloned().unwrap();
        editor.count("2");
        editor.command(VimCommand::Repeat);
        assert_eq!(editor.text, "defg");
        assert_eq!(editor.vim.last_change(), Some(&recipe));
        assert_eq!(editor.undo.len(), 2);
        editor.command(VimCommand::Undo);
        assert_eq!(editor.text, "bcdefg");
    }

    #[test]
    fn no_op_changes_do_not_affect_history_or_dot_recipe() {
        let mut editor = Editor::new("a");
        editor.command(VimCommand::DeleteChars);
        let recipe = editor.vim.last_change().cloned().unwrap();
        let history_len = editor.undo.len();
        editor.command(VimCommand::DeleteChars);
        assert_eq!(editor.text, "");
        assert_eq!(editor.undo.len(), history_len);
        assert_eq!(editor.vim.last_change(), Some(&recipe));
        let outcome = editor.command(VimCommand::Repeat);
        assert!(!outcome.text_changed);
        assert_eq!(editor.vim.last_change(), Some(&recipe));
    }

    #[test]
    fn identical_visual_paste_preserves_register_history_and_dot_recipe() {
        let mut editor = Editor::new("aa");
        editor.command(VimCommand::DeleteChars);
        let recipe = editor.vim.last_change().cloned().unwrap();
        let register = editor.vim.unnamed_register().cloned().unwrap();
        editor.command(VimCommand::Undo);
        let history_len = editor.undo.len();
        editor.command(VimCommand::ToggleVisual);
        let outcome = editor.command(VimCommand::Paste(PastePlacement::After));

        assert_eq!(editor.text, "aa");
        assert!(!outcome.text_changed);
        assert_eq!(editor.undo.len(), history_len);
        assert_eq!(editor.vim.last_change(), Some(&recipe));
        assert_eq!(editor.vim.unnamed_register(), Some(&register));
        assert_eq!(
            outcome.status.notice.as_ref().map(|notice| notice.kind),
            Some(NoticeKind::NoChange)
        );
    }

    #[test]
    fn invalid_pending_grammar_is_consumed_and_cancelled() {
        let mut editor = Editor::new("abc");
        editor.command(VimCommand::BeginOperator(Operator::Delete));
        let outcome = editor.command(VimCommand::Paste(PastePlacement::After));
        assert!(outcome.consumed);
        assert_eq!(outcome.status.pending_operator, None);
        assert_eq!(
            outcome.status.notice.as_ref().map(|notice| notice.kind),
            Some(NoticeKind::InvalidGrammar)
        );
        assert_eq!(editor.text, "abc");

        editor.command(VimCommand::CountDigit(2));
        editor.escape();
        assert_eq!(editor.vim.status().count, None);
        assert_eq!(editor.vim.mode(), VimMode::Normal);
    }

    #[test]
    fn insert_mode_leaves_unmatched_input_to_text_input_and_ime() {
        let mut editor = Editor::new("a");
        editor.command(VimCommand::EnterInsert(InsertEntry::AfterCursor));
        let outcome = editor.command(VimCommand::Invalid);
        assert!(!outcome.consumed);
        assert_eq!(editor.vim.mode(), VimMode::Insert);
        editor.insert_kind("é", InsertEditKind::ImeCommit);
        editor.escape();
        assert_eq!(editor.text, "aé");
    }

    #[test]
    fn lifecycle_hooks_commit_or_reset_transient_state_deterministically() {
        let mut editor = Editor::new("a");
        editor.command(VimCommand::EnterInsert(InsertEntry::AfterCursor));
        editor.insert("x");
        let outcome = editor
            .vim
            .handle_lifecycle(&editor.text, LifecycleEvent::Disable);
        editor.apply_history(outcome.history.clone());
        assert!(matches!(outcome.history, HistoryPlan::Commit { .. }));
        assert_eq!(editor.vim.mode(), VimMode::Disabled);
        assert!(editor.vim.last_change().is_some());

        editor.vim = VimState::at(&editor.text, editor.vim.cursor());
        editor.command(VimCommand::BeginOperator(Operator::Delete));
        editor
            .vim
            .handle_lifecycle(&editor.text, LifecycleEvent::MouseCaretMove { offset: 0 });
        assert_eq!(editor.vim.status().pending_operator, None);

        editor.vim.reset_after_external_text("new", 3, false);
        assert!(editor.vim.last_change().is_none());
        assert!(editor.vim.unnamed_register().is_none());
        assert_eq!(editor.vim.cursor(), 2);
    }

    #[test]
    fn disabled_mode_is_sticky_across_non_profile_lifecycle_boundaries() {
        for event in [
            LifecycleEvent::TaskOrScreenSwitch,
            LifecycleEvent::SendOrClear,
            LifecycleEvent::MouseCaretMove { offset: 1 },
            LifecycleEvent::PopupTakeover,
            LifecycleEvent::ExternalDraftReplacement,
        ] {
            let mut vim = VimState::at("abc", 1);
            vim.handle_lifecycle("abc", LifecycleEvent::Disable);

            let outcome = vim.handle_lifecycle("abc", event);

            assert_eq!(vim.mode(), VimMode::Disabled, "event: {event:?}");
            assert_eq!(outcome.status.mode, VimMode::Disabled, "event: {event:?}");
        }
    }

    #[test]
    fn normal_escape_requests_application_transition_but_visual_escape_does_not() {
        let mut editor = Editor::new("a");
        let normal = editor.escape();
        assert_eq!(normal.signal, VimSignal::LeaveComposer);

        editor.command(VimCommand::ToggleVisual);
        let visual = editor.escape();
        assert_eq!(visual.signal, VimSignal::None);
        assert_eq!(editor.vim.mode(), VimMode::Normal);
    }

    #[test]
    fn application_gi_enters_insert_at_the_saved_insertion_boundary() {
        let mut editor = Editor::at("one two", 4);
        editor.command(VimCommand::EnterInsert(InsertEntry::BeforeCursor));
        editor.insert("X");
        editor.escape();
        assert_eq!(editor.text, "one Xtwo");
        assert_eq!(editor.vim.last_insertion(), 5);
        editor.command(VimCommand::Motion(Motion::LineStart));

        let outcome = editor.vim.enter_at_last_insertion(&editor.text);
        assert!(outcome.consumed);
        assert_eq!(outcome.status.mode, VimMode::Insert);
        assert_eq!(editor.vim.cursor(), 5);
    }

    #[test]
    fn operators_compose_with_remaining_characterwise_motions() {
        let mut right = Editor::new("abc");
        right.operator_motion(Operator::Delete, Motion::Right);
        assert_eq!(right.text, "bc");

        let mut backward_word = Editor::at("one two", 4);
        backward_word.operator_motion(Operator::Delete, Motion::WordBackward);
        assert_eq!(backward_word.text, "two");

        let mut zero = Editor::at("abc", 2);
        zero.command(VimCommand::BeginOperator(Operator::Delete));
        zero.command(VimCommand::Contextual(ContextualToken::ZeroOrLineStart));
        assert_eq!(zero.text, "c");

        // Vim deletes whole lines when a characterwise delete over several
        // lines starts in the indentation and leaves only blanks behind.
        let mut counted_end = Editor::new("a\nb\nc");
        counted_end.command(VimCommand::BeginOperator(Operator::Delete));
        counted_end.command(VimCommand::CountDigit(2));
        counted_end.command(VimCommand::Motion(Motion::LineEnd));
        assert_eq!(counted_end.text, "c");
        assert_eq!(
            counted_end.vim.unnamed_register().unwrap().kind,
            RegisterKind::Linewise
        );
    }

    #[test]
    fn operators_compose_with_linewise_navigation_motions() {
        let mut down = Editor::new("a\nb\nc");
        down.operator_motion(Operator::Delete, Motion::Down);
        assert_eq!(down.text, "c");

        let mut up = Editor::at("a\nb\nc", 2);
        up.operator_motion(Operator::Delete, Motion::Up);
        assert_eq!(up.text, "c");

        let mut first = Editor::at("a\nb\nc", 4);
        first.operator_motion(Operator::Delete, Motion::FirstLine);
        assert_eq!(first.text, "");

        let mut last = Editor::at("a\nb\nc", 2);
        last.operator_motion(Operator::Delete, Motion::LastLine);
        assert_eq!(last.text, "a");
    }

    #[test]
    fn linewise_yank_handles_final_lines_and_trailing_newline() {
        let mut final_line = Editor::at("a\nb", 2);
        final_line.repeated_operator(Operator::Yank);
        assert_eq!(final_line.vim.unnamed_register().unwrap().text, "b\n");
        assert_eq!(final_line.text, "a\nb");

        let mut before_sentinel = Editor::at("a\nb\n", 2);
        before_sentinel.repeated_operator(Operator::Yank);
        assert_eq!(before_sentinel.vim.unnamed_register().unwrap().text, "b\n");

        let mut sentinel = Editor::at("a\nb\n", 4);
        sentinel.repeated_operator(Operator::Yank);
        assert_eq!(sentinel.vim.unnamed_register().unwrap().text, "\n");
    }

    #[test]
    fn counted_undo_and_redo_restore_complete_changes() {
        let mut editor = Editor::new("abcd");
        editor.command(VimCommand::DeleteChars);
        editor.command(VimCommand::DeleteChars);
        editor.command(VimCommand::DeleteChars);
        assert_eq!(editor.text, "d");
        editor.count("2");
        editor.command(VimCommand::Undo);
        assert_eq!(editor.text, "bcd");
        editor.count("2");
        editor.command(VimCommand::Redo);
        assert_eq!(editor.text, "d");
    }

    #[test]
    fn counted_linewise_paste_and_dot_use_saved_register() {
        let mut editor = Editor::new("a");
        editor.vim.register = Some(RegisterSnapshot {
            text: "x\n".to_owned(),
            kind: RegisterKind::Linewise,
        });
        editor.count("2");
        editor.command(VimCommand::Paste(PastePlacement::After));
        assert_eq!(editor.text, "a\nx\nx");
        editor.command(VimCommand::Repeat);
        assert_eq!(editor.text, "a\nx\nx\nx\nx");
    }

    #[test]
    fn find_and_replace_treat_graphemes_whole() {
        // `fe` lands on an accented `e`, and `r` replaces the whole grapheme.
        let mut editor = Editor::new("cafe\u{301} 🙂!");
        editor.command(VimCommand::BeginFind(FindKind::Forward));
        editor.command(VimCommand::Literal('e'));
        assert_eq!(editor.vim.cursor(), "caf".len());
        editor.command(VimCommand::BeginReplace);
        editor.command(VimCommand::Literal('x'));
        assert_eq!(editor.text, "cafx 🙂!");
        editor.command(VimCommand::BeginFind(FindKind::TillForward));
        editor.command(VimCommand::Literal('!'));
        assert_eq!(&editor.text[editor.vim.cursor()..], "🙂!");
    }

    #[test]
    fn a_pending_character_rejects_commands_and_escape_keeps_visual() {
        let mut editor = Editor::new("abc");
        editor.command(VimCommand::BeginOperator(Operator::Delete));
        editor.command(VimCommand::BeginFind(FindKind::Forward));
        assert!(editor.vim.awaiting_literal());
        let outcome = editor.command(VimCommand::Motion(Motion::Right));
        assert_eq!(
            outcome.status.notice.map(|notice| notice.kind),
            Some(NoticeKind::InvalidGrammar)
        );
        assert!(!editor.vim.awaiting_literal());
        assert_eq!(editor.vim.status().pending_operator, None);
        assert_eq!(editor.text, "abc");

        editor.command(VimCommand::ToggleVisual);
        editor.command(VimCommand::BeginReplace);
        let outcome = editor.escape();
        assert_eq!(outcome.signal, VimSignal::None);
        assert_eq!(editor.vim.mode(), VimMode::Visual);
        assert!(!editor.vim.awaiting_literal());
    }

    #[test]
    fn case_operators_repeat_on_their_own_key() {
        let mut toggle = Editor::new("ab cd");
        toggle.command(VimCommand::BeginOperator(Operator::Case(
            CaseChange::Toggle,
        )));
        toggle.command(VimCommand::ChangeCase(CaseChange::Toggle));
        assert_eq!(toggle.text, "AB CD");

        // `guu`: in Normal mode `u` arrives as undo.
        let mut lower = Editor::new("AB CD");
        lower.command(VimCommand::BeginOperator(Operator::Case(CaseChange::Lower)));
        lower.command(VimCommand::Undo);
        assert_eq!(lower.text, "ab cd");
        assert_eq!(lower.undo.len(), 1);

        let mut upper = Editor::new("ab\ncd");
        upper.count("2");
        upper.command(VimCommand::BeginOperator(Operator::Case(CaseChange::Upper)));
        upper.command(VimCommand::ChangeCase(CaseChange::Upper));
        assert_eq!(upper.text, "AB\nCD");
        // Without `gU` first, `U` changes nothing.
        let outcome = upper.command(VimCommand::ChangeCase(CaseChange::Upper));
        assert!(!outcome.text_changed);
    }

    #[test]
    fn linewise_visual_changes_repeat_over_the_same_number_of_lines() {
        let mut editor = Editor::new("a\nb\nc\nd\ne");
        editor.command(VimCommand::ToggleVisualLine);
        editor.command(VimCommand::Motion(Motion::Down));
        editor.command(VimCommand::BeginOperator(Operator::Delete));
        assert_eq!(editor.text, "c\nd\ne");
        assert!(matches!(
            editor.vim.last_change(),
            Some(RepeatRecipe::VisualLines {
                line_count: 2,
                operator: VisualOperator::Delete,
                ..
            })
        ));
        editor.command(VimCommand::Repeat);
        assert_eq!(editor.text, "e");
        editor.command(VimCommand::Undo);
        assert_eq!(editor.text, "c\nd\ne");
    }

    #[test]
    fn join_and_replace_repeat_with_their_counts() {
        let mut join = Editor::new("a\nb\nc\nd\ne");
        join.count("3");
        join.command(VimCommand::JoinLines);
        assert_eq!(join.text, "a b c\nd\ne");
        // Only two lines remain below, so the repeated join takes both.
        join.command(VimCommand::Motion(Motion::Down));
        join.command(VimCommand::Repeat);
        assert_eq!(join.text, "a b c\nd e");

        let mut replace = Editor::new("abcd abcd");
        replace.count("2");
        replace.command(VimCommand::BeginReplace);
        replace.command(VimCommand::Literal('🙂'));
        assert_eq!(replace.text, "🙂🙂cd abcd");
        replace.command(VimCommand::Motion(Motion::BigWordForward));
        replace.command(VimCommand::Repeat);
        assert_eq!(replace.text, "🙂🙂cd 🙂🙂cd");
    }

    #[test]
    fn insert_then_backspace_to_original_is_not_a_change() {
        let mut editor = Editor::new("a");
        editor.command(VimCommand::EnterInsert(InsertEntry::AfterCursor));
        editor.insert("X");
        editor.backspace();
        let outcome = editor.escape();
        assert_eq!(editor.text, "a");
        assert!(!outcome.text_changed);
        assert!(editor.undo.is_empty());
        assert!(editor.vim.last_change().is_none());
    }
}
