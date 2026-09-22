//! Maple's permanent catalog of shipped GPUI shortcuts and its keymap installer.
//!
//! This module remains the single source of truth for default bindings even if
//! the experimental shortcut-customization feature is removed.

use std::rc::Rc;

use gpui::{Action, App, DummyKeyboardMapper, KeyBinding, KeyBindingContextPredicate};

use crate::{
    desktop::QuitApp,
    ui::{
        application_vim::{self, SpatialDirection},
        chat,
        text_input::{
            self,
            vim::{
                ContextualToken, InsertEntry, Motion, OpenLinePlacement, Operator, PastePlacement,
            },
            vim_actions,
        },
    },
};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ShortcutCategory {
    Application,
    Chat,
    Transcript,
    ProjectMenu,
    TextEditing,
    ComposerVim,
    ApplicationVim,
}

impl ShortcutCategory {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Application => "Application",
            Self::Chat => "Chat",
            Self::Transcript => "Transcript",
            Self::ProjectMenu => "Project Menu",
            Self::TextEditing => "Text Editing",
            Self::ComposerVim => "Composer Vim",
            Self::ApplicationVim => "Application Vim",
        }
    }
}

#[derive(Clone, Copy)]
enum SlotAction {
    QuitApp,
    ChatEscape,
    NewTask,
    FocusSearch,
    ToggleSidebar,
    ToggleArchived,
    OpenAppSettings,
    ChooseProject,
    PreviousTask,
    NextTask,
    AllowPermission,
    PickQuestionOption(usize),
    CopySelection,
    SelectAllTranscript,
    RootMenuPrevious,
    RootMenuNext,
    RootMenuConfirm,
    Backspace,
    Delete,
    Left,
    Right,
    SelectLeft,
    SelectRight,
    SelectAll,
    Paste,
    Copy,
    Cut,
    Home,
    End,
    SelectLineStart,
    SelectLineEnd,
    DeleteToLineStart,
    DeleteToLineEnd,
    WordLeft,
    WordRight,
    SelectWordLeft,
    SelectWordRight,
    DeleteWordBackward,
    DeleteWordForward,
    Up,
    Down,
    SelectUp,
    SelectDown,
    ParagraphStart,
    ParagraphEnd,
    SelectParagraphStart,
    SelectParagraphEnd,
    DocumentStart,
    DocumentEnd,
    SelectDocumentStart,
    SelectDocumentEnd,
    Undo,
    Redo,
    ShowCharacterPalette,
    VimMotion(Motion),
    VimContextual(ContextualToken),
    VimBeginOperator(Operator),
    VimCountDigit(u8),
    VimToggleVisual,
    VimDeleteChars,
    VimPaste(PastePlacement),
    VimEnterInsert(InsertEntry),
    VimOpenLine(OpenLinePlacement),
    VimUndo,
    VimRedo,
    VimRepeat,
    VimCancel,
    ApplicationVimNext,
    ApplicationVimPrevious,
    ApplicationVimFirst,
    ApplicationVimLast,
    ApplicationVimActivate,
    ApplicationVimCollapse,
    ApplicationVimExpand,
    ApplicationVimCopyTarget,
    ApplicationVimSearch,
    ApplicationVimEscape,
    ApplicationVimFocusComposer,
    ApplicationVimNewestAssistant,
    ApplicationVimNextAssistant,
    ApplicationVimPreviousAssistant,
    ApplicationVimNextAnnotation,
    ApplicationVimPreviousAnnotation,
    ApplicationVimCountDigit(u8),
    ApplicationVimMoveRegion(SpatialDirection),
}

impl SlotAction {
    fn build(self) -> Box<dyn Action> {
        match self {
            Self::QuitApp => Box::new(QuitApp),
            Self::ChatEscape => Box::new(chat::ChatEscape),
            Self::NewTask => Box::new(chat::NewTask),
            Self::FocusSearch => Box::new(chat::FocusSearch),
            Self::ToggleSidebar => Box::new(chat::ToggleSidebar),
            Self::ToggleArchived => Box::new(chat::ToggleArchived),
            Self::OpenAppSettings => Box::new(chat::OpenAppSettings),
            Self::ChooseProject => Box::new(chat::ChooseProject),
            Self::PreviousTask => Box::new(chat::PreviousTask),
            Self::NextTask => Box::new(chat::NextTask),
            Self::AllowPermission => Box::new(chat::AllowPermission),
            Self::PickQuestionOption(index) => Box::new(chat::PickQuestionOption { index }),
            Self::CopySelection => Box::new(chat::CopySelection),
            Self::SelectAllTranscript => Box::new(chat::SelectAllTranscript),
            Self::RootMenuPrevious => Box::new(chat::RootMenuPrevious),
            Self::RootMenuNext => Box::new(chat::RootMenuNext),
            Self::RootMenuConfirm => Box::new(chat::RootMenuConfirm),
            Self::Backspace => Box::new(text_input::Backspace),
            Self::Delete => Box::new(text_input::Delete),
            Self::Left => Box::new(text_input::Left),
            Self::Right => Box::new(text_input::Right),
            Self::SelectLeft => Box::new(text_input::SelectLeft),
            Self::SelectRight => Box::new(text_input::SelectRight),
            Self::SelectAll => Box::new(text_input::SelectAll),
            Self::Paste => Box::new(text_input::Paste),
            Self::Copy => Box::new(text_input::Copy),
            Self::Cut => Box::new(text_input::Cut),
            Self::Home => Box::new(text_input::Home),
            Self::End => Box::new(text_input::End),
            Self::SelectLineStart => Box::new(text_input::SelectLineStart),
            Self::SelectLineEnd => Box::new(text_input::SelectLineEnd),
            Self::DeleteToLineStart => Box::new(text_input::DeleteToLineStart),
            Self::DeleteToLineEnd => Box::new(text_input::DeleteToLineEnd),
            Self::WordLeft => Box::new(text_input::WordLeft),
            Self::WordRight => Box::new(text_input::WordRight),
            Self::SelectWordLeft => Box::new(text_input::SelectWordLeft),
            Self::SelectWordRight => Box::new(text_input::SelectWordRight),
            Self::DeleteWordBackward => Box::new(text_input::DeleteWordBackward),
            Self::DeleteWordForward => Box::new(text_input::DeleteWordForward),
            Self::Up => Box::new(text_input::Up),
            Self::Down => Box::new(text_input::Down),
            Self::SelectUp => Box::new(text_input::SelectUp),
            Self::SelectDown => Box::new(text_input::SelectDown),
            Self::ParagraphStart => Box::new(text_input::ParagraphStart),
            Self::ParagraphEnd => Box::new(text_input::ParagraphEnd),
            Self::SelectParagraphStart => Box::new(text_input::SelectParagraphStart),
            Self::SelectParagraphEnd => Box::new(text_input::SelectParagraphEnd),
            Self::DocumentStart => Box::new(text_input::DocumentStart),
            Self::DocumentEnd => Box::new(text_input::DocumentEnd),
            Self::SelectDocumentStart => Box::new(text_input::SelectDocumentStart),
            Self::SelectDocumentEnd => Box::new(text_input::SelectDocumentEnd),
            Self::Undo => Box::new(text_input::Undo),
            Self::Redo => Box::new(text_input::Redo),
            Self::ShowCharacterPalette => Box::new(text_input::ShowCharacterPalette),
            Self::VimMotion(motion) => Box::new(vim_actions::VimMotion { motion }),
            Self::VimContextual(token) => Box::new(vim_actions::VimContextual { token }),
            Self::VimBeginOperator(operator) => {
                Box::new(vim_actions::VimBeginOperator { operator })
            }
            Self::VimCountDigit(digit) => Box::new(vim_actions::VimCountDigit { digit }),
            Self::VimToggleVisual => Box::new(vim_actions::VimToggleVisual),
            Self::VimDeleteChars => Box::new(vim_actions::VimDeleteChars),
            Self::VimPaste(placement) => Box::new(vim_actions::VimPaste { placement }),
            Self::VimEnterInsert(placement) => Box::new(vim_actions::VimEnterInsert { placement }),
            Self::VimOpenLine(placement) => Box::new(vim_actions::VimOpenLine { placement }),
            Self::VimUndo => Box::new(vim_actions::VimUndo),
            Self::VimRedo => Box::new(vim_actions::VimRedo),
            Self::VimRepeat => Box::new(vim_actions::VimRepeat),
            Self::VimCancel => Box::new(vim_actions::VimCancel),
            Self::ApplicationVimNext => Box::new(application_vim::Next),
            Self::ApplicationVimPrevious => Box::new(application_vim::Previous),
            Self::ApplicationVimFirst => Box::new(application_vim::First),
            Self::ApplicationVimLast => Box::new(application_vim::Last),
            Self::ApplicationVimActivate => Box::new(application_vim::Activate),
            Self::ApplicationVimCollapse => Box::new(application_vim::Collapse),
            Self::ApplicationVimExpand => Box::new(application_vim::Expand),
            Self::ApplicationVimCopyTarget => Box::new(application_vim::CopyTarget),
            Self::ApplicationVimSearch => Box::new(application_vim::Search),
            Self::ApplicationVimEscape => Box::new(application_vim::Escape),
            Self::ApplicationVimFocusComposer => Box::new(application_vim::FocusComposer),
            Self::ApplicationVimNewestAssistant => Box::new(application_vim::NewestAssistant),
            Self::ApplicationVimNextAssistant => Box::new(application_vim::NextAssistant),
            Self::ApplicationVimPreviousAssistant => Box::new(application_vim::PreviousAssistant),
            Self::ApplicationVimNextAnnotation => Box::new(application_vim::NextAnnotation),
            Self::ApplicationVimPreviousAnnotation => Box::new(application_vim::PreviousAnnotation),
            Self::ApplicationVimCountDigit(digit) => {
                Box::new(application_vim::CountDigit { digit })
            }
            Self::ApplicationVimMoveRegion(direction) => {
                Box::new(application_vim::MoveRegion { direction })
            }
        }
    }
}

pub(crate) struct ShortcutSlot {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) category: ShortcutCategory,
    pub(crate) context: Option<&'static str>,
    pub(crate) default_sequence: &'static str,
    action: SlotAction,
}

fn slot(
    id: &'static str,
    label: &'static str,
    category: ShortcutCategory,
    context: Option<&'static str>,
    default_sequence: &'static str,
    action: SlotAction,
) -> ShortcutSlot {
    ShortcutSlot {
        id,
        label,
        category,
        context,
        default_sequence,
        action,
    }
}

/// Word and line shortcuts while Vim is off or in Insert. Normal and
/// Visual keep `b`/`w`/`e`/`$`. A plain `TextInput` predicate would still
/// match those modes, and Vim does not bind Option or Command arrows, so
/// the exclusion has to live in the predicate.
pub(crate) const STANDARD_TEXT_CONTEXT: &str =
    "TextInput && editor_vim_mode != normal && editor_vim_mode != visual";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostOs {
    Macos,
    Linux,
    Windows,
}

impl HostOs {
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

pub(crate) fn catalog() -> Vec<ShortcutSlot> {
    catalog_for(HostOs::current())
}

fn catalog_for(os: HostOs) -> Vec<ShortcutSlot> {
    // macOS Option-Up/Down is paragraph motion, so task switching uses Command-Option.
    let (previous_task, next_task) = if matches!(os, HostOs::Macos) {
        ("cmd-alt-up", "cmd-alt-down")
    } else {
        ("alt-up", "alt-down")
    };
    let mut slots = vec![
        slot(
            "app.quit",
            "Quit Maple",
            ShortcutCategory::Application,
            None,
            "secondary-q",
            SlotAction::QuitApp,
        ),
        slot(
            "chat.escape",
            "Close the active chat surface",
            ShortcutCategory::Chat,
            Some("Chat"),
            "escape",
            SlotAction::ChatEscape,
        ),
        slot(
            "chat.new_task",
            "New task",
            ShortcutCategory::Chat,
            Some("Chat"),
            "secondary-n",
            SlotAction::NewTask,
        ),
        slot(
            "chat.focus_search",
            "Focus task search",
            ShortcutCategory::Chat,
            Some("Chat"),
            "secondary-k",
            SlotAction::FocusSearch,
        ),
        slot(
            "chat.toggle_sidebar",
            "Toggle sidebar",
            ShortcutCategory::Chat,
            Some("Chat"),
            "secondary-b",
            SlotAction::ToggleSidebar,
        ),
        slot(
            "chat.toggle_archived",
            "Toggle archived tasks",
            ShortcutCategory::Chat,
            Some("Chat"),
            "secondary-shift-a",
            SlotAction::ToggleArchived,
        ),
        slot(
            "chat.open_settings",
            "Open settings",
            ShortcutCategory::Chat,
            Some("Chat"),
            "secondary-,",
            SlotAction::OpenAppSettings,
        ),
        slot(
            "chat.choose_project",
            "Choose project",
            ShortcutCategory::Chat,
            Some("Chat"),
            "secondary-p",
            SlotAction::ChooseProject,
        ),
        slot(
            "chat.previous_task",
            "Previous task",
            ShortcutCategory::Chat,
            Some("Chat"),
            previous_task,
            SlotAction::PreviousTask,
        ),
        slot(
            "chat.next_task",
            "Next task",
            ShortcutCategory::Chat,
            Some("Chat"),
            next_task,
            SlotAction::NextTask,
        ),
        slot(
            "chat.allow_permission",
            "Allow permission request",
            ShortcutCategory::Chat,
            Some("Chat"),
            "secondary-y",
            SlotAction::AllowPermission,
        ),
    ];
    for (id, label, sequence, index) in [
        (
            "chat.pick_question_option_1",
            "Choose question option 1",
            "secondary-1",
            0,
        ),
        (
            "chat.pick_question_option_2",
            "Choose question option 2",
            "secondary-2",
            1,
        ),
        (
            "chat.pick_question_option_3",
            "Choose question option 3",
            "secondary-3",
            2,
        ),
        (
            "chat.pick_question_option_4",
            "Choose question option 4",
            "secondary-4",
            3,
        ),
        (
            "chat.pick_question_option_5",
            "Choose question option 5",
            "secondary-5",
            4,
        ),
        (
            "chat.pick_question_option_6",
            "Choose question option 6",
            "secondary-6",
            5,
        ),
        (
            "chat.pick_question_option_7",
            "Choose question option 7",
            "secondary-7",
            6,
        ),
        (
            "chat.pick_question_option_8",
            "Choose question option 8",
            "secondary-8",
            7,
        ),
        (
            "chat.pick_question_option_9",
            "Choose question option 9",
            "secondary-9",
            8,
        ),
    ] {
        slots.push(slot(
            id,
            label,
            ShortcutCategory::Chat,
            Some("Chat"),
            sequence,
            SlotAction::PickQuestionOption(index),
        ));
    }
    slots.extend([
        slot(
            "transcript.copy_selection",
            "Copy transcript selection",
            ShortcutCategory::Transcript,
            Some("Transcript"),
            "secondary-c",
            SlotAction::CopySelection,
        ),
        slot(
            "transcript.select_all",
            "Select all transcript text",
            ShortcutCategory::Transcript,
            Some("Transcript"),
            "secondary-a",
            SlotAction::SelectAllTranscript,
        ),
        slot(
            "project_menu.previous",
            "Previous project menu item",
            ShortcutCategory::ProjectMenu,
            Some("RootMenu"),
            "up",
            SlotAction::RootMenuPrevious,
        ),
        slot(
            "project_menu.next",
            "Next project menu item",
            ShortcutCategory::ProjectMenu,
            Some("RootMenu"),
            "down",
            SlotAction::RootMenuNext,
        ),
        slot(
            "project_menu.confirm",
            "Choose project menu item",
            ShortcutCategory::ProjectMenu,
            Some("RootMenu"),
            "enter",
            SlotAction::RootMenuConfirm,
        ),
    ]);

    add_application_vim_slots(&mut slots);

    slots.extend([
        slot(
            "text_input.backspace",
            "Delete backward",
            ShortcutCategory::TextEditing,
            Some("TextInput"),
            "backspace",
            SlotAction::Backspace,
        ),
        slot(
            "text_input.delete",
            "Delete forward",
            ShortcutCategory::TextEditing,
            Some("TextInput"),
            "delete",
            SlotAction::Delete,
        ),
        slot(
            "text_input.left",
            "Move left",
            ShortcutCategory::TextEditing,
            Some("TextInput"),
            "left",
            SlotAction::Left,
        ),
        slot(
            "text_input.right",
            "Move right",
            ShortcutCategory::TextEditing,
            Some("TextInput"),
            "right",
            SlotAction::Right,
        ),
        slot(
            "text_input.select_left",
            "Select left",
            ShortcutCategory::TextEditing,
            Some("TextInput"),
            "shift-left",
            SlotAction::SelectLeft,
        ),
        slot(
            "text_input.select_right",
            "Select right",
            ShortcutCategory::TextEditing,
            Some("TextInput"),
            "shift-right",
            SlotAction::SelectRight,
        ),
        slot(
            "text_input.select_all",
            "Select all text",
            ShortcutCategory::TextEditing,
            Some("TextInput"),
            "secondary-a",
            SlotAction::SelectAll,
        ),
        slot(
            "text_input.paste",
            "Paste",
            ShortcutCategory::TextEditing,
            Some("TextInput"),
            "secondary-v",
            SlotAction::Paste,
        ),
        slot(
            "text_input.copy",
            "Copy",
            ShortcutCategory::TextEditing,
            Some("TextInput"),
            "secondary-c",
            SlotAction::Copy,
        ),
        slot(
            "text_input.cut",
            "Cut",
            ShortcutCategory::TextEditing,
            Some("TextInput"),
            "secondary-x",
            SlotAction::Cut,
        ),
        slot(
            "text_input.home",
            "Move to line start",
            ShortcutCategory::TextEditing,
            Some("TextInput"),
            "home",
            SlotAction::Home,
        ),
        slot(
            "text_input.end",
            "Move to line end",
            ShortcutCategory::TextEditing,
            Some("TextInput"),
            "end",
            SlotAction::End,
        ),
        slot(
            "text_input.up",
            "Move up",
            ShortcutCategory::TextEditing,
            Some("TextInput"),
            "up",
            SlotAction::Up,
        ),
        slot(
            "text_input.down",
            "Move down",
            ShortcutCategory::TextEditing,
            Some("TextInput"),
            "down",
            SlotAction::Down,
        ),
        slot(
            "text_input.select_up",
            "Select up",
            ShortcutCategory::TextEditing,
            Some(STANDARD_TEXT_CONTEXT),
            "shift-up",
            SlotAction::SelectUp,
        ),
        slot(
            "text_input.select_down",
            "Select down",
            ShortcutCategory::TextEditing,
            Some(STANDARD_TEXT_CONTEXT),
            "shift-down",
            SlotAction::SelectDown,
        ),
        slot(
            "text_input.undo",
            "Undo",
            ShortcutCategory::TextEditing,
            Some("TextInput"),
            "secondary-z",
            SlotAction::Undo,
        ),
        slot(
            "text_input.redo",
            "Redo",
            ShortcutCategory::TextEditing,
            Some("TextInput"),
            "secondary-shift-z",
            SlotAction::Redo,
        ),
        slot(
            "text_input.character_palette",
            "Show character palette",
            ShortcutCategory::TextEditing,
            Some("TextInput"),
            "ctrl-cmd-space",
            SlotAction::ShowCharacterPalette,
        ),
    ]);

    add_vim_mode_slots(&mut slots, "normal", vim_actions::NORMAL_CONTEXT);
    add_vim_mode_slots(&mut slots, "visual", vim_actions::VISUAL_CONTEXT);
    slots.extend([
        slot(
            "composer_vim.normal.insert.first_non_whitespace",
            "Vim insert at first non-whitespace",
            ShortcutCategory::ComposerVim,
            Some(vim_actions::NORMAL_CONTEXT),
            "I",
            SlotAction::VimEnterInsert(InsertEntry::FirstNonWhitespace),
        ),
        slot(
            "composer_vim.normal.insert.line_end",
            "Vim insert at line end",
            ShortcutCategory::ComposerVim,
            Some(vim_actions::NORMAL_CONTEXT),
            "A",
            SlotAction::VimEnterInsert(InsertEntry::LineEnd),
        ),
        slot(
            "composer_vim.normal.open_line.below",
            "Vim open line below",
            ShortcutCategory::ComposerVim,
            Some(vim_actions::NORMAL_CONTEXT),
            "o",
            SlotAction::VimOpenLine(OpenLinePlacement::Below),
        ),
        slot(
            "composer_vim.normal.open_line.above",
            "Vim open line above",
            ShortcutCategory::ComposerVim,
            Some(vim_actions::NORMAL_CONTEXT),
            "O",
            SlotAction::VimOpenLine(OpenLinePlacement::Above),
        ),
        slot(
            "composer_vim.normal.undo",
            "Vim undo",
            ShortcutCategory::ComposerVim,
            Some(vim_actions::NORMAL_CONTEXT),
            "u",
            SlotAction::VimUndo,
        ),
        slot(
            "composer_vim.normal.redo",
            "Vim redo",
            ShortcutCategory::ComposerVim,
            Some(vim_actions::NORMAL_CONTEXT),
            "ctrl-r",
            SlotAction::VimRedo,
        ),
        slot(
            "composer_vim.normal.repeat",
            "Vim repeat",
            ShortcutCategory::ComposerVim,
            Some(vim_actions::NORMAL_CONTEXT),
            ".",
            SlotAction::VimRepeat,
        ),
        slot(
            "composer_vim.normal.cancel",
            "Vim cancel",
            ShortcutCategory::ComposerVim,
            Some(vim_actions::NORMAL_CONTEXT),
            "escape",
            SlotAction::VimCancel,
        ),
        slot(
            "composer_vim.visual.cancel",
            "Vim cancel selection",
            ShortcutCategory::ComposerVim,
            Some(vim_actions::VISUAL_CONTEXT),
            "escape",
            SlotAction::VimCancel,
        ),
        slot(
            "composer_vim.insert.cancel",
            "Vim leave Insert mode",
            ShortcutCategory::ComposerVim,
            Some(vim_actions::INSERT_CONTEXT),
            "escape",
            SlotAction::VimCancel,
        ),
    ]);
    add_platform_text_slots(&mut slots, os);
    slots
}

/// Platform text motions. `secondary` is Command on macOS and Control
/// elsewhere, so Command+Left and Control+Left cannot share one sequence.
fn add_platform_text_slots(slots: &mut Vec<ShortcutSlot>, os: HostOs) {
    let mac = matches!(os, HostOs::Macos);
    let (
        word_left,
        word_right,
        select_word_left,
        select_word_right,
        delete_word_back,
        delete_word_forward,
    ) = if mac {
        (
            "alt-left",
            "alt-right",
            "alt-shift-left",
            "alt-shift-right",
            "alt-backspace",
            "alt-delete",
        )
    } else {
        (
            "ctrl-left",
            "ctrl-right",
            "ctrl-shift-left",
            "ctrl-shift-right",
            "ctrl-backspace",
            "ctrl-delete",
        )
    };
    slots.extend([
        slot(
            "text_input.word_left",
            "Move word left",
            ShortcutCategory::TextEditing,
            Some(STANDARD_TEXT_CONTEXT),
            word_left,
            SlotAction::WordLeft,
        ),
        slot(
            "text_input.word_right",
            "Move word right",
            ShortcutCategory::TextEditing,
            Some(STANDARD_TEXT_CONTEXT),
            word_right,
            SlotAction::WordRight,
        ),
        slot(
            "text_input.select_word_left",
            "Select word left",
            ShortcutCategory::TextEditing,
            Some(STANDARD_TEXT_CONTEXT),
            select_word_left,
            SlotAction::SelectWordLeft,
        ),
        slot(
            "text_input.select_word_right",
            "Select word right",
            ShortcutCategory::TextEditing,
            Some(STANDARD_TEXT_CONTEXT),
            select_word_right,
            SlotAction::SelectWordRight,
        ),
        slot(
            "text_input.delete_word_backward",
            "Delete previous word",
            ShortcutCategory::TextEditing,
            Some(STANDARD_TEXT_CONTEXT),
            delete_word_back,
            SlotAction::DeleteWordBackward,
        ),
        slot(
            "text_input.delete_word_forward",
            "Delete next word",
            ShortcutCategory::TextEditing,
            Some(STANDARD_TEXT_CONTEXT),
            delete_word_forward,
            SlotAction::DeleteWordForward,
        ),
        slot(
            "text_input.select_line_start",
            "Select to line start",
            ShortcutCategory::TextEditing,
            Some(STANDARD_TEXT_CONTEXT),
            "shift-home",
            SlotAction::SelectLineStart,
        ),
        slot(
            "text_input.select_line_end",
            "Select to line end",
            ShortcutCategory::TextEditing,
            Some(STANDARD_TEXT_CONTEXT),
            "shift-end",
            SlotAction::SelectLineEnd,
        ),
    ]);
    if mac {
        slots.extend([
            slot(
                "text_input.command_line_start",
                "Move to line start (Command)",
                ShortcutCategory::TextEditing,
                Some(STANDARD_TEXT_CONTEXT),
                "cmd-left",
                SlotAction::Home,
            ),
            slot(
                "text_input.command_line_end",
                "Move to line end (Command)",
                ShortcutCategory::TextEditing,
                Some(STANDARD_TEXT_CONTEXT),
                "cmd-right",
                SlotAction::End,
            ),
            slot(
                "text_input.command_select_line_start",
                "Select to line start (Command)",
                ShortcutCategory::TextEditing,
                Some(STANDARD_TEXT_CONTEXT),
                "cmd-shift-left",
                SlotAction::SelectLineStart,
            ),
            slot(
                "text_input.command_select_line_end",
                "Select to line end (Command)",
                ShortcutCategory::TextEditing,
                Some(STANDARD_TEXT_CONTEXT),
                "cmd-shift-right",
                SlotAction::SelectLineEnd,
            ),
            slot(
                "text_input.delete_to_line_start",
                "Delete to line start",
                ShortcutCategory::TextEditing,
                Some(STANDARD_TEXT_CONTEXT),
                "cmd-backspace",
                SlotAction::DeleteToLineStart,
            ),
            slot(
                "text_input.delete_to_line_end",
                "Delete to line end",
                ShortcutCategory::TextEditing,
                Some(STANDARD_TEXT_CONTEXT),
                "cmd-delete",
                SlotAction::DeleteToLineEnd,
            ),
        ]);
        for (id, label, sequence, action) in [
            (
                "text_input.paragraph_start",
                "Move to paragraph start",
                "alt-up",
                SlotAction::ParagraphStart,
            ),
            (
                "text_input.paragraph_end",
                "Move to paragraph end",
                "alt-down",
                SlotAction::ParagraphEnd,
            ),
            (
                "text_input.select_paragraph_start",
                "Select to paragraph start",
                "alt-shift-up",
                SlotAction::SelectParagraphStart,
            ),
            (
                "text_input.select_paragraph_end",
                "Select to paragraph end",
                "alt-shift-down",
                SlotAction::SelectParagraphEnd,
            ),
            (
                "text_input.document_start",
                "Move to document start",
                "cmd-up",
                SlotAction::DocumentStart,
            ),
            (
                "text_input.document_end",
                "Move to document end",
                "cmd-down",
                SlotAction::DocumentEnd,
            ),
            (
                "text_input.select_document_start",
                "Select to document start",
                "cmd-shift-up",
                SlotAction::SelectDocumentStart,
            ),
            (
                "text_input.select_document_end",
                "Select to document end",
                "cmd-shift-down",
                SlotAction::SelectDocumentEnd,
            ),
        ] {
            slots.push(slot(
                id,
                label,
                ShortcutCategory::TextEditing,
                Some(STANDARD_TEXT_CONTEXT),
                sequence,
                action,
            ));
        }
    }
}

fn add_application_vim_slots(slots: &mut Vec<ShortcutSlot>) {
    let root = application_vim::ROOT_CONTEXT;
    for (id, label, sequence, action) in [
        (
            "application_vim.next",
            "Application Vim next item",
            "j",
            SlotAction::ApplicationVimNext,
        ),
        (
            "application_vim.previous",
            "Application Vim previous item",
            "k",
            SlotAction::ApplicationVimPrevious,
        ),
        (
            "application_vim.first",
            "Application Vim first item",
            "g g",
            SlotAction::ApplicationVimFirst,
        ),
        (
            "application_vim.last",
            "Application Vim last item",
            "G",
            SlotAction::ApplicationVimLast,
        ),
        (
            "application_vim.activate",
            "Application Vim activate item",
            "enter",
            SlotAction::ApplicationVimActivate,
        ),
        (
            "application_vim.collapse",
            "Application Vim collapse item",
            "h",
            SlotAction::ApplicationVimCollapse,
        ),
        (
            "application_vim.expand",
            "Application Vim expand item",
            "l",
            SlotAction::ApplicationVimExpand,
        ),
        (
            "application_vim.copy_target",
            "Application Vim copy transcript item",
            "y",
            SlotAction::ApplicationVimCopyTarget,
        ),
        (
            "application_vim.search",
            "Application Vim search",
            "/",
            SlotAction::ApplicationVimSearch,
        ),
        (
            "application_vim.newest_assistant",
            "Application Vim newest assistant item",
            "g a",
            SlotAction::ApplicationVimNewestAssistant,
        ),
        (
            "application_vim.previous_assistant",
            "Application Vim previous assistant item",
            "[ a",
            SlotAction::ApplicationVimPreviousAssistant,
        ),
        (
            "application_vim.next_assistant",
            "Application Vim next assistant item",
            "] a",
            SlotAction::ApplicationVimNextAssistant,
        ),
        (
            "application_vim.previous_annotation",
            "Application Vim previous annotation",
            "[ d",
            SlotAction::ApplicationVimPreviousAnnotation,
        ),
        (
            "application_vim.next_annotation",
            "Application Vim next annotation",
            "] d",
            SlotAction::ApplicationVimNextAnnotation,
        ),
        (
            "application_vim.focus_composer",
            "Application Vim focus last composer insertion",
            "g i",
            SlotAction::ApplicationVimFocusComposer,
        ),
        (
            "application_vim.escape",
            "Application Vim close or return",
            "escape",
            SlotAction::ApplicationVimEscape,
        ),
        (
            "application_vim.leader.new_task",
            "Application Vim new task",
            "space s n",
            SlotAction::NewTask,
        ),
        (
            "application_vim.leader.settings",
            "Application Vim open settings",
            "space ,",
            SlotAction::OpenAppSettings,
        ),
    ] {
        slots.push(slot(
            id,
            label,
            ShortcutCategory::ApplicationVim,
            Some(root),
            sequence,
            action,
        ));
    }

    for (direction, id, label, sequence) in [
        (
            SpatialDirection::Left,
            "application_vim.region.left",
            "Application Vim move to left region",
            "ctrl-w h",
        ),
        (
            SpatialDirection::Down,
            "application_vim.region.down",
            "Application Vim move to lower region",
            "ctrl-w j",
        ),
        (
            SpatialDirection::Up,
            "application_vim.region.up",
            "Application Vim move to upper region",
            "ctrl-w k",
        ),
        (
            SpatialDirection::Right,
            "application_vim.region.right",
            "Application Vim move to right region",
            "ctrl-w l",
        ),
    ] {
        slots.push(slot(
            id,
            label,
            ShortcutCategory::ApplicationVim,
            Some(root),
            sequence,
            SlotAction::ApplicationVimMoveRegion(direction),
        ));
    }

    const COUNT_IDS: [&str; 10] = [
        "application_vim.count.0",
        "application_vim.count.1",
        "application_vim.count.2",
        "application_vim.count.3",
        "application_vim.count.4",
        "application_vim.count.5",
        "application_vim.count.6",
        "application_vim.count.7",
        "application_vim.count.8",
        "application_vim.count.9",
    ];
    const COUNT_LABELS: [&str; 10] = [
        "Application Vim count 0",
        "Application Vim count 1",
        "Application Vim count 2",
        "Application Vim count 3",
        "Application Vim count 4",
        "Application Vim count 5",
        "Application Vim count 6",
        "Application Vim count 7",
        "Application Vim count 8",
        "Application Vim count 9",
    ];
    const COUNT_KEYS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
    for digit in 0..=9 {
        slots.push(slot(
            COUNT_IDS[digit],
            COUNT_LABELS[digit],
            ShortcutCategory::ApplicationVim,
            Some(root),
            COUNT_KEYS[digit],
            SlotAction::ApplicationVimCountDigit(digit as u8),
        ));
    }

    for (id, label, sequence, action) in [
        (
            "application_vim.project_menu.previous",
            "Application Vim previous project menu item",
            "k",
            SlotAction::ApplicationVimPrevious,
        ),
        (
            "application_vim.project_menu.next",
            "Application Vim next project menu item",
            "j",
            SlotAction::ApplicationVimNext,
        ),
    ] {
        slots.push(slot(
            id,
            label,
            ShortcutCategory::ApplicationVim,
            Some(application_vim::ROOT_MENU_CONTEXT),
            sequence,
            action,
        ));
    }

    slots.push(slot(
        "application_vim.input.escape",
        "Return from an application Vim text field",
        ShortcutCategory::ApplicationVim,
        Some(application_vim::OTHER_INPUT_CONTEXT),
        "escape",
        SlotAction::ApplicationVimEscape,
    ));

    for (id, label, sequence, action) in [
        (
            "application_vim.composer.newest_assistant",
            "Leave composer for newest assistant item",
            "g a",
            SlotAction::ApplicationVimNewestAssistant,
        ),
        (
            "application_vim.composer.last_insertion",
            "Composer Vim last insertion",
            "g i",
            SlotAction::ApplicationVimFocusComposer,
        ),
        (
            "application_vim.composer.region.left",
            "Leave composer for left region",
            "ctrl-w h",
            SlotAction::ApplicationVimMoveRegion(SpatialDirection::Left),
        ),
        (
            "application_vim.composer.region.down",
            "Move from composer to lower region",
            "ctrl-w j",
            SlotAction::ApplicationVimMoveRegion(SpatialDirection::Down),
        ),
        (
            "application_vim.composer.region.up",
            "Leave composer for upper region",
            "ctrl-w k",
            SlotAction::ApplicationVimMoveRegion(SpatialDirection::Up),
        ),
        (
            "application_vim.composer.region.right",
            "Move from composer to right region",
            "ctrl-w l",
            SlotAction::ApplicationVimMoveRegion(SpatialDirection::Right),
        ),
    ] {
        slots.push(slot(
            id,
            label,
            ShortcutCategory::ApplicationVim,
            Some(application_vim::COMPOSER_NORMAL_CONTEXT),
            sequence,
            action,
        ));
    }
}

fn add_vim_mode_slots(slots: &mut Vec<ShortcutSlot>, mode: &'static str, context: &'static str) {
    let (motion_ids, contextual_ids, operator_ids, digit_ids, action_ids) = if mode == "normal" {
        (
            [
                "composer_vim.normal.motion.left.h",
                "composer_vim.normal.motion.down.j",
                "composer_vim.normal.motion.up.k",
                "composer_vim.normal.motion.right.l",
                "composer_vim.normal.motion.word_backward",
                "composer_vim.normal.motion.word_end",
                "composer_vim.normal.motion.line_end",
                "composer_vim.normal.motion.first_line",
                "composer_vim.normal.motion.last_line",
                "composer_vim.normal.motion.left.arrow",
                "composer_vim.normal.motion.down.arrow",
                "composer_vim.normal.motion.up.arrow",
                "composer_vim.normal.motion.right.arrow",
            ],
            [
                "composer_vim.normal.contextual.word",
                "composer_vim.normal.contextual.inner",
                "composer_vim.normal.contextual.around",
            ],
            [
                "composer_vim.normal.operator.delete",
                "composer_vim.normal.operator.change",
                "composer_vim.normal.operator.yank",
            ],
            [
                "composer_vim.normal.count.0",
                "composer_vim.normal.count.1",
                "composer_vim.normal.count.2",
                "composer_vim.normal.count.3",
                "composer_vim.normal.count.4",
                "composer_vim.normal.count.5",
                "composer_vim.normal.count.6",
                "composer_vim.normal.count.7",
                "composer_vim.normal.count.8",
                "composer_vim.normal.count.9",
            ],
            [
                "composer_vim.normal.toggle_visual",
                "composer_vim.normal.delete_chars",
                "composer_vim.normal.paste.after",
                "composer_vim.normal.paste.before",
            ],
        )
    } else {
        (
            [
                "composer_vim.visual.motion.left.h",
                "composer_vim.visual.motion.down.j",
                "composer_vim.visual.motion.up.k",
                "composer_vim.visual.motion.right.l",
                "composer_vim.visual.motion.word_backward",
                "composer_vim.visual.motion.word_end",
                "composer_vim.visual.motion.line_end",
                "composer_vim.visual.motion.first_line",
                "composer_vim.visual.motion.last_line",
                "composer_vim.visual.motion.left.arrow",
                "composer_vim.visual.motion.down.arrow",
                "composer_vim.visual.motion.up.arrow",
                "composer_vim.visual.motion.right.arrow",
            ],
            [
                "composer_vim.visual.contextual.word",
                "composer_vim.visual.contextual.inner",
                "composer_vim.visual.contextual.around",
            ],
            [
                "composer_vim.visual.operator.delete",
                "composer_vim.visual.operator.change",
                "composer_vim.visual.operator.yank",
            ],
            [
                "composer_vim.visual.count.0",
                "composer_vim.visual.count.1",
                "composer_vim.visual.count.2",
                "composer_vim.visual.count.3",
                "composer_vim.visual.count.4",
                "composer_vim.visual.count.5",
                "composer_vim.visual.count.6",
                "composer_vim.visual.count.7",
                "composer_vim.visual.count.8",
                "composer_vim.visual.count.9",
            ],
            [
                "composer_vim.visual.toggle_visual",
                "composer_vim.visual.delete_chars",
                "composer_vim.visual.paste.after",
                "composer_vim.visual.paste.before",
            ],
        )
    };
    for (((id, key), label), motion) in motion_ids
        .into_iter()
        .zip([
            "h", "j", "k", "l", "b", "e", "$", "g g", "G", "left", "down", "up", "right",
        ])
        .zip([
            "Move left (h)",
            "Move down (j)",
            "Move up (k)",
            "Move right (l)",
            "Move word backward",
            "Move to word end",
            "Move to line end",
            "Move to first line",
            "Move to last line",
            "Move left (arrow)",
            "Move down (arrow)",
            "Move up (arrow)",
            "Move right (arrow)",
        ])
        .zip([
            Motion::Left,
            Motion::Down,
            Motion::Up,
            Motion::Right,
            Motion::WordBackward,
            Motion::WordEnd,
            Motion::LineEnd,
            Motion::FirstLine,
            Motion::LastLine,
            Motion::Left,
            Motion::Down,
            Motion::Up,
            Motion::Right,
        ])
    {
        slots.push(slot(
            id,
            label,
            ShortcutCategory::ComposerVim,
            Some(context),
            key,
            SlotAction::VimMotion(motion),
        ));
    }
    for (((id, key), label), token) in contextual_ids
        .into_iter()
        .zip(["w", "i", "a"])
        .zip([
            "Move by word / complete text object",
            "Inner text object / insert",
            "Around text object / append",
        ])
        .zip([
            ContextualToken::WordOrTextObject,
            ContextualToken::InnerOrInsert,
            ContextualToken::AroundOrAppend,
        ])
    {
        slots.push(slot(
            id,
            label,
            ShortcutCategory::ComposerVim,
            Some(context),
            key,
            SlotAction::VimContextual(token),
        ));
    }
    for (((id, key), label), operator) in operator_ids
        .into_iter()
        .zip(["d", "c", "y"])
        .zip(["Delete operator", "Change operator", "Yank operator"])
        .zip([Operator::Delete, Operator::Change, Operator::Yank])
    {
        slots.push(slot(
            id,
            label,
            ShortcutCategory::ComposerVim,
            Some(context),
            key,
            SlotAction::VimBeginOperator(operator),
        ));
    }
    for (digit, id) in digit_ids.into_iter().enumerate() {
        const DIGIT_KEYS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
        const DIGIT_LABELS: [&str; 10] = [
            "Count digit 0",
            "Count digit 1",
            "Count digit 2",
            "Count digit 3",
            "Count digit 4",
            "Count digit 5",
            "Count digit 6",
            "Count digit 7",
            "Count digit 8",
            "Count digit 9",
        ];
        slots.push(slot(
            id,
            DIGIT_LABELS[digit],
            ShortcutCategory::ComposerVim,
            Some(context),
            DIGIT_KEYS[digit],
            SlotAction::VimCountDigit(digit as u8),
        ));
    }
    for (((id, key), label), action) in action_ids
        .into_iter()
        .zip(["v", "x", "p", "P"])
        .zip([
            "Toggle Visual mode",
            "Delete characters",
            "Paste after",
            "Paste before",
        ])
        .zip([
            SlotAction::VimToggleVisual,
            SlotAction::VimDeleteChars,
            SlotAction::VimPaste(PastePlacement::After),
            SlotAction::VimPaste(PastePlacement::Before),
        ])
    {
        slots.push(slot(
            id,
            label,
            ShortcutCategory::ComposerVim,
            Some(context),
            key,
            action,
        ));
    }
}

/// Install the shipped keymap without enabling the customization layer.
///
/// The current desktop immediately installs a persisted customization
/// generation, so this fallback is exercised by the default-map tests and is
/// the production entry point if the experimental layer is removed.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn bootstrap(cx: &mut App) {
    let bindings = catalog()
        .into_iter()
        .map(|slot| build_binding(&slot, slot.default_sequence))
        .collect::<Result<Vec<_>, _>>()
        .expect("the compiled-in shortcut catalog must always be valid");
    install(bindings, cx);
}

pub(crate) fn build_binding(slot: &ShortcutSlot, sequence: &str) -> Result<KeyBinding, String> {
    let predicate = slot
        .context
        .map(KeyBindingContextPredicate::parse)
        .transpose()
        .map_err(|error| {
            format!(
                "shortcut '{}' has invalid context '{}': {error}",
                slot.id,
                slot.context.unwrap_or_default()
            )
        })?
        .map(Rc::new);
    KeyBinding::load(
        sequence,
        slot.action.build(),
        predicate,
        false,
        None,
        &DummyKeyboardMapper,
    )
    .map_err(|error| {
        format!(
            "shortcut '{}' has invalid sequence '{}': {error}",
            slot.id, sequence
        )
    })
}

/// The shipped catalog exclusively owns GPUI's process-wide keymap. Replacing
/// it clears every binding, including bindings registered elsewhere, so every
/// Maple or extension binding must be represented in `catalog()` before this
/// installer runs.
pub(crate) fn install(bindings: Vec<KeyBinding>, cx: &mut App) {
    cx.clear_key_bindings();
    cx.bind_keys(bindings);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn catalog_contains_existing_shortcuts_and_the_application_vim_layer() {
        for os in [HostOs::Macos, HostOs::Linux, HostOs::Windows] {
            let catalog = catalog_for(os);
            let ids = catalog.iter().map(|slot| slot.id).collect::<BTreeSet<_>>();
            assert_eq!(ids.len(), catalog.len());
            assert_eq!(
                catalog
                    .iter()
                    .filter(|slot| slot.category == ShortcutCategory::ComposerVim)
                    .count(),
                76
            );
            assert_eq!(
                catalog
                    .iter()
                    .filter(|slot| slot.category == ShortcutCategory::ApplicationVim)
                    .count(),
                41
            );
            for (id, sequence) in [
                ("app.quit", "secondary-q"),
                ("project_menu.confirm", "enter"),
                ("text_input.character_palette", "ctrl-cmd-space"),
                ("application_vim.previous_annotation", "[ d"),
                ("application_vim.next_annotation", "] d"),
                ("text_input.select_line_start", "shift-home"),
                ("text_input.select_line_end", "shift-end"),
            ] {
                assert_eq!(
                    catalog
                        .iter()
                        .find(|slot| slot.id == id)
                        .map(|slot| slot.default_sequence),
                    Some(sequence),
                    "{id} on {os:?}"
                );
            }
        }

        let mac = catalog_for(HostOs::Macos);
        let linux = catalog_for(HostOs::Linux);
        let windows = catalog_for(HostOs::Windows);
        assert_eq!(mac.len(), linux.len() + 14);
        assert_eq!(linux.len(), windows.len());
        for (id, mac_sequence, other_sequence) in [
            ("text_input.word_left", "alt-left", "ctrl-left"),
            ("text_input.word_right", "alt-right", "ctrl-right"),
            (
                "text_input.select_word_left",
                "alt-shift-left",
                "ctrl-shift-left",
            ),
            (
                "text_input.select_word_right",
                "alt-shift-right",
                "ctrl-shift-right",
            ),
            (
                "text_input.delete_word_backward",
                "alt-backspace",
                "ctrl-backspace",
            ),
            (
                "text_input.delete_word_forward",
                "alt-delete",
                "ctrl-delete",
            ),
        ] {
            assert_eq!(sequence(&mac, id), mac_sequence);
            assert_eq!(sequence(&linux, id), other_sequence);
            assert_eq!(sequence(&windows, id), other_sequence);
            assert_eq!(
                context(&mac, id),
                Some(STANDARD_TEXT_CONTEXT),
                "{id} stays out of Vim Normal and Visual"
            );
        }
        for id in [
            "text_input.command_line_start",
            "text_input.command_line_end",
            "text_input.command_select_line_start",
            "text_input.command_select_line_end",
            "text_input.delete_to_line_start",
            "text_input.delete_to_line_end",
        ] {
            assert!(mac.iter().any(|slot| slot.id == id), "{id} on macOS");
            assert!(linux.iter().all(|slot| slot.id != id), "{id} off macOS");
        }
        assert_eq!(sequence(&mac, "text_input.command_line_start"), "cmd-left");
        assert_eq!(
            sequence(&mac, "text_input.delete_to_line_start"),
            "cmd-backspace"
        );
        assert_eq!(
            sequence(&mac, "text_input.delete_to_line_end"),
            "cmd-delete"
        );
        assert_eq!(sequence(&mac, "chat.previous_task"), "cmd-alt-up");
        assert_eq!(sequence(&mac, "chat.next_task"), "cmd-alt-down");
        assert_eq!(sequence(&linux, "chat.previous_task"), "alt-up");
        assert_eq!(sequence(&windows, "chat.previous_task"), "alt-up");
        assert_eq!(sequence(&linux, "chat.next_task"), "alt-down");
        assert_eq!(sequence(&windows, "chat.next_task"), "alt-down");
        assert_eq!(sequence(&mac, "text_input.paragraph_start"), "alt-up");
        assert_eq!(sequence(&mac, "text_input.paragraph_end"), "alt-down");
        for (os, catalog) in [("macOS", &mac), ("Linux", &linux), ("Windows", &windows)] {
            let chat = catalog
                .iter()
                .filter(|slot| slot.context == Some("Chat"))
                .map(|slot| slot.default_sequence)
                .collect::<BTreeSet<_>>();
            let text = catalog
                .iter()
                .filter(|slot| {
                    matches!(
                        slot.context,
                        Some("TextInput") | Some(STANDARD_TEXT_CONTEXT)
                    )
                })
                .map(|slot| slot.default_sequence)
                .collect::<BTreeSet<_>>();
            let shared = chat.intersection(&text).copied().collect::<Vec<_>>();
            assert!(
                shared.is_empty(),
                "{os} text fields share a default key with Chat: {shared:?}"
            );
        }
    }

    fn sequence<'a>(catalog: &'a [ShortcutSlot], id: &str) -> &'a str {
        catalog
            .iter()
            .find(|slot| slot.id == id)
            .map(|slot| slot.default_sequence)
            .unwrap_or_else(|| panic!("missing slot {id}"))
    }

    fn context<'a>(catalog: &'a [ShortcutSlot], id: &str) -> Option<&'a str> {
        catalog
            .iter()
            .find(|slot| slot.id == id)
            .and_then(|slot| slot.context)
    }
}
