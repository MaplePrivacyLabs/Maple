//! GPUI transport for the composer-local Vim engine.
//!
//! The pure engine in [`super::vim`] never sees physical keys. These typed
//! actions are the temporary fixed keymap for the focused composer; the
//! shortcut-customization layer can later replace the bindings without
//! changing editor behavior. The one exception is the character after `f`,
//! `t`, `r`, `i` or `a`: the field turns off these bindings with the
//! `waiting` mode and hands that key to the engine as a literal.

use gpui::{Context, InteractiveElement, actions};

use super::TextInput;
use super::vim::{
    CaseChange, ContextualToken, FindKind, InsertEntry, Motion, OpenLinePlacement, Operator,
    OperatorShortcut, PastePlacement,
};

#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = composer_vim, no_json)]
pub struct VimMotion {
    pub motion: Motion,
}

#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = composer_vim, no_json)]
pub struct VimBeginOperator {
    pub operator: Operator,
}

#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = composer_vim, no_json)]
pub struct VimCountDigit {
    pub digit: u8,
}

#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = composer_vim, no_json)]
pub struct VimContextual {
    pub token: ContextualToken,
}

#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = composer_vim, no_json)]
pub struct VimEnterInsert {
    pub placement: InsertEntry,
}

#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = composer_vim, no_json)]
pub struct VimOpenLine {
    pub placement: OpenLinePlacement,
}

#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = composer_vim, no_json)]
pub struct VimPaste {
    pub placement: PastePlacement,
}

#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = composer_vim, no_json)]
pub struct VimBeginFind {
    pub kind: FindKind,
}

#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = composer_vim, no_json)]
pub struct VimShortcut {
    pub shortcut: OperatorShortcut,
}

#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = composer_vim, no_json)]
pub struct VimChangeCase {
    pub change: CaseChange,
}

actions!(
    composer_vim,
    [
        VimToggleVisual,
        VimToggleVisualLine,
        VimSwapVisualEnds,
        VimDeleteChars,
        VimBeginReplace,
        VimJoinLines,
        VimInsertDeleteWord,
        VimInsertDeleteLine,
        VimUndo,
        VimRedo,
        VimRepeat,
        VimCancel,
    ]
);

pub const NORMAL_CONTEXT: &str = "TextInput && input_role == composer && editor_vim_mode == normal";
pub const VISUAL_CONTEXT: &str = "TextInput && input_role == composer && editor_vim_mode == visual";
pub const INSERT_CONTEXT: &str = "TextInput && input_role == composer && editor_vim_mode == insert";
/// Normal or Visual while the engine waits for a character. Only Escape is
/// bound here; every other key reaches the field as that character.
pub const WAITING_CONTEXT: &str =
    "TextInput && input_role == composer && editor_vim_mode == waiting";

/// Attach every composer-Vim action at the TextInput boundary. Keeping this
/// list behind one call site makes the optional editor layer straightforward
/// to remove without disturbing ordinary text-input actions.
pub(super) fn attach_actions<E: InteractiveElement>(element: E, cx: &mut Context<TextInput>) -> E {
    element
        .on_action(cx.listener(TextInput::vim_motion))
        .on_action(cx.listener(TextInput::vim_begin_operator))
        .on_action(cx.listener(TextInput::vim_count_digit))
        .on_action(cx.listener(TextInput::vim_contextual))
        .on_action(cx.listener(TextInput::vim_enter_insert))
        .on_action(cx.listener(TextInput::vim_open_line))
        .on_action(cx.listener(TextInput::vim_toggle_visual))
        .on_action(cx.listener(TextInput::vim_toggle_visual_line))
        .on_action(cx.listener(TextInput::vim_swap_visual_ends))
        .on_action(cx.listener(TextInput::vim_delete_chars))
        .on_action(cx.listener(TextInput::vim_paste))
        .on_action(cx.listener(TextInput::vim_begin_find))
        .on_action(cx.listener(TextInput::vim_begin_replace))
        .on_action(cx.listener(TextInput::vim_shortcut))
        .on_action(cx.listener(TextInput::vim_change_case))
        .on_action(cx.listener(TextInput::vim_join_lines))
        .on_action(cx.listener(TextInput::vim_insert_delete_word))
        .on_action(cx.listener(TextInput::vim_insert_delete_line))
        .on_action(cx.listener(TextInput::vim_undo))
        .on_action(cx.listener(TextInput::vim_redo))
        .on_action(cx.listener(TextInput::vim_repeat))
        .on_action(cx.listener(TextInput::vim_cancel))
}
