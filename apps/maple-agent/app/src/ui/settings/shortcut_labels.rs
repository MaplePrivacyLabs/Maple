//! Presentation adapter for the built-in keymap catalog. Never apply this to
//! user text or arbitrary host errors. The coverage test follows catalog additions.
use crate::ui::i18n::tr;
use gpui::SharedString;

pub(super) fn label(value: &str) -> SharedString {
    key(value).map_or_else(|| value.to_string().into(), tr)
}

fn key(value: &str) -> Option<&'static str> {
    Some(match value {
        "Quit Maple" => "settings-shortcut-quit-maple",
        "Close the active chat surface" => "settings-shortcut-close-the-active-chat-surface",
        "New task" => "settings-shortcut-new-task",
        "Focus task search" => "settings-shortcut-focus-task-search",
        "Toggle sidebar" => "settings-shortcut-toggle-sidebar",
        "Toggle archived tasks" => "settings-shortcut-toggle-archived-tasks",
        "Open settings" => "settings-shortcut-open-settings",
        "Choose project" => "settings-shortcut-choose-project",
        "Previous task" => "settings-shortcut-previous-task",
        "Next task" => "settings-shortcut-next-task",
        "Hide Maple" => "settings-shortcut-hide-maple",
        "Hide Others" => "settings-shortcut-hide-others",
        "Copy transcript selection" => "settings-shortcut-copy-transcript-selection",
        "Select all transcript text" => "settings-shortcut-select-all-transcript-text",
        "Previous menu item" => "settings-shortcut-previous-menu-item",
        "Next menu item" => "settings-shortcut-next-menu-item",
        "First menu item" => "settings-shortcut-first-menu-item",
        "Last menu item" => "settings-shortcut-last-menu-item",
        "Choose menu item" => "settings-shortcut-choose-menu-item",
        "Choose menu item with Space" => "settings-shortcut-choose-menu-item-with-space",
        "Close menu" => "settings-shortcut-close-menu",
        "Open submenu" => "settings-shortcut-open-submenu",
        "Return to parent menu" => "settings-shortcut-return-to-parent-menu",
        "Delete backward" => "settings-shortcut-delete-backward",
        "Delete forward" => "settings-shortcut-delete-forward",
        "Move left" => "settings-shortcut-move-left",
        "Move right" => "settings-shortcut-move-right",
        "Select left" => "settings-shortcut-select-left",
        "Select right" => "settings-shortcut-select-right",
        "Select all text" => "settings-shortcut-select-all-text",
        "Paste" => "settings-shortcut-paste",
        "Copy" => "settings-shortcut-copy",
        "Cut" => "settings-shortcut-cut",
        "Move to line start" => "settings-shortcut-move-to-line-start",
        "Move to line end" => "settings-shortcut-move-to-line-end",
        "Move up" => "settings-shortcut-move-up",
        "Move down" => "settings-shortcut-move-down",
        "Select up" => "settings-shortcut-select-up",
        "Select down" => "settings-shortcut-select-down",
        "Undo" => "settings-shortcut-undo",
        "Redo" => "settings-shortcut-redo",
        "Show character palette" => "settings-shortcut-show-character-palette",
        "Vim insert at first non-whitespace" => {
            "settings-shortcut-vim-insert-at-first-non-whitespace"
        }
        "Vim insert at line end" => "settings-shortcut-vim-insert-at-line-end",
        "Vim open line below" => "settings-shortcut-vim-open-line-below",
        "Vim open line above" => "settings-shortcut-vim-open-line-above",
        "Vim undo" => "settings-shortcut-vim-undo",
        "Vim redo" => "settings-shortcut-vim-redo",
        "Vim repeat" => "settings-shortcut-vim-repeat",
        "Vim cancel" => "settings-shortcut-vim-cancel",
        "Vim cancel selection" => "settings-shortcut-vim-cancel-selection",
        "Vim leave Insert mode" => "settings-shortcut-vim-leave-insert-mode",
        "Move WORD forward" => "settings-shortcut-move-word-forward",
        "Move WORD backward" => "settings-shortcut-move-word-backward",
        "Move to WORD end" => "settings-shortcut-move-to-word-end",
        "Move to previous word end" => "settings-shortcut-move-to-previous-word-end",
        "Move to previous WORD end" => "settings-shortcut-move-to-previous-word-end",
        "Move to first non-blank" => "settings-shortcut-move-to-first-non-blank",
        "Move to matching bracket" => "settings-shortcut-move-to-matching-bracket",
        "Move to previous paragraph" => "settings-shortcut-move-to-previous-paragraph",
        "Move to next paragraph" => "settings-shortcut-move-to-next-paragraph",
        "Repeat last find" => "settings-shortcut-repeat-last-find",
        "Repeat last find backward" => "settings-shortcut-repeat-last-find-backward",
        "Find character" => "settings-shortcut-find-character",
        "Find character backward" => "settings-shortcut-find-character-backward",
        "Move to before character" => "settings-shortcut-move-to-before-character",
        "Move to after character backward" => "settings-shortcut-move-to-after-character-backward",
        "Replace with character" => "settings-shortcut-replace-with-character",
        "Toggle case" => "settings-shortcut-toggle-case",
        "Join lines" => "settings-shortcut-join-lines",
        "Toggle linewise Visual mode" => "settings-shortcut-toggle-linewise-visual-mode",
        "Substitute characters" => "settings-shortcut-substitute-characters",
        "Change selection" => "settings-shortcut-change-selection",
        "Substitute lines" => "settings-shortcut-substitute-lines",
        "Change selected lines" => "settings-shortcut-change-selected-lines",
        "Change to line end" => "settings-shortcut-change-to-line-end",
        "Change selected lines (C)" => "settings-shortcut-change-selected-lines-c",
        "Delete to line end" => "settings-shortcut-delete-to-line-end",
        "Delete selected lines" => "settings-shortcut-delete-selected-lines",
        "Yank to line end" => "settings-shortcut-yank-to-line-end",
        "Yank selected lines" => "settings-shortcut-yank-selected-lines",
        "Delete before cursor" => "settings-shortcut-delete-before-cursor",
        "Delete selected lines (X)" => "settings-shortcut-delete-selected-lines-x",
        "Toggle case operator" => "settings-shortcut-toggle-case-operator",
        "Lowercase operator" => "settings-shortcut-lowercase-operator",
        "Uppercase operator" => "settings-shortcut-uppercase-operator",
        "Uppercase line (gUU)" => "settings-shortcut-uppercase-line-guu",
        "Go to other end of selection" => "settings-shortcut-go-to-other-end-of-selection",
        "Lowercase selection" => "settings-shortcut-lowercase-selection",
        "Uppercase selection" => "settings-shortcut-uppercase-selection",
        "Delete previous word" => "settings-shortcut-delete-previous-word",
        "Delete to line start" => "settings-shortcut-delete-to-line-start",
        "Delete previous character" => "settings-shortcut-delete-previous-character",
        "Vim cancel pending key" => "settings-shortcut-vim-cancel-pending-key",
        "Move word left" => "settings-shortcut-move-word-left",
        "Move word right" => "settings-shortcut-move-word-right",
        "Select word left" => "settings-shortcut-select-word-left",
        "Select word right" => "settings-shortcut-select-word-right",
        "Delete next word" => "settings-shortcut-delete-next-word",
        "Select to line start" => "settings-shortcut-select-to-line-start",
        "Select to line end" => "settings-shortcut-select-to-line-end",
        "Move to line start (Command)" => "settings-shortcut-move-to-line-start-command",
        "Move to line end (Command)" => "settings-shortcut-move-to-line-end-command",
        "Select to line start (Command)" => "settings-shortcut-select-to-line-start-command",
        "Select to line end (Command)" => "settings-shortcut-select-to-line-end-command",
        "Move to paragraph start" => "settings-shortcut-move-to-paragraph-start",
        "Move to paragraph end" => "settings-shortcut-move-to-paragraph-end",
        "Select to paragraph start" => "settings-shortcut-select-to-paragraph-start",
        "Select to paragraph end" => "settings-shortcut-select-to-paragraph-end",
        "Move to document start" => "settings-shortcut-move-to-document-start",
        "Move to document end" => "settings-shortcut-move-to-document-end",
        "Select to document start" => "settings-shortcut-select-to-document-start",
        "Select to document end" => "settings-shortcut-select-to-document-end",
        "Application Vim next item" => "settings-shortcut-application-vim-next-item",
        "Application Vim previous item" => "settings-shortcut-application-vim-previous-item",
        "Application Vim first item" => "settings-shortcut-application-vim-first-item",
        "Application Vim last item" => "settings-shortcut-application-vim-last-item",
        "Application Vim activate item" => "settings-shortcut-application-vim-activate-item",
        "Application Vim collapse item" => "settings-shortcut-application-vim-collapse-item",
        "Application Vim expand item" => "settings-shortcut-application-vim-expand-item",
        "Application Vim copy transcript item" => {
            "settings-shortcut-application-vim-copy-transcript-item"
        }
        "Application Vim search" => "settings-shortcut-application-vim-search",
        "Application Vim newest assistant item" => {
            "settings-shortcut-application-vim-newest-assistant-item"
        }
        "Application Vim previous assistant item" => {
            "settings-shortcut-application-vim-previous-assistant-item"
        }
        "Application Vim next assistant item" => {
            "settings-shortcut-application-vim-next-assistant-item"
        }
        "Application Vim previous annotation" => {
            "settings-shortcut-application-vim-previous-annotation"
        }
        "Application Vim next annotation" => "settings-shortcut-application-vim-next-annotation",
        "Application Vim focus last composer insertion" => {
            "settings-shortcut-application-vim-focus-last-composer-insertion"
        }
        "Application Vim close or return" => "settings-shortcut-application-vim-close-or-return",
        "Application Vim new task" => "settings-shortcut-application-vim-new-task",
        "Application Vim open settings" => "settings-shortcut-application-vim-open-settings",
        "Application Vim move to left region" => {
            "settings-shortcut-application-vim-move-to-left-region"
        }
        "Application Vim move to lower region" => {
            "settings-shortcut-application-vim-move-to-lower-region"
        }
        "Application Vim move to upper region" => {
            "settings-shortcut-application-vim-move-to-upper-region"
        }
        "Application Vim move to right region" => {
            "settings-shortcut-application-vim-move-to-right-region"
        }
        "Application Vim previous menu item" => {
            "settings-shortcut-application-vim-previous-menu-item"
        }
        "Application Vim next menu item" => "settings-shortcut-application-vim-next-menu-item",
        "Application Vim first menu item" => "settings-shortcut-application-vim-first-menu-item",
        "Application Vim last menu item" => "settings-shortcut-application-vim-last-menu-item",
        "Return from an application Vim text field" => {
            "settings-shortcut-return-from-an-application-vim-text-field"
        }
        "Leave composer for newest assistant item" => {
            "settings-shortcut-leave-composer-for-newest-assistant-item"
        }
        "Composer Vim last insertion" => "settings-shortcut-composer-vim-last-insertion",
        "Leave composer for left region" => "settings-shortcut-leave-composer-for-left-region",
        "Move from composer to lower region" => {
            "settings-shortcut-move-from-composer-to-lower-region"
        }
        "Leave composer for upper region" => "settings-shortcut-leave-composer-for-upper-region",
        "Move from composer to right region" => {
            "settings-shortcut-move-from-composer-to-right-region"
        }
        "Move left (h)" => "settings-shortcut-move-left-h",
        "Move down (j)" => "settings-shortcut-move-down-j",
        "Move up (k)" => "settings-shortcut-move-up-k",
        "Move right (l)" => "settings-shortcut-move-right-l",
        "Move word backward" => "settings-shortcut-move-word-backward",
        "Move to word end" => "settings-shortcut-move-to-word-end",
        "Move to first line" => "settings-shortcut-move-to-first-line",
        "Move to last line" => "settings-shortcut-move-to-last-line",
        "Move left (arrow)" => "settings-shortcut-move-left-arrow",
        "Move down (arrow)" => "settings-shortcut-move-down-arrow",
        "Move up (arrow)" => "settings-shortcut-move-up-arrow",
        "Move right (arrow)" => "settings-shortcut-move-right-arrow",
        "Move word forward" => "settings-shortcut-move-word-forward",
        "Inner text object / insert" => "settings-shortcut-inner-text-object-insert",
        "Around text object / append" => "settings-shortcut-around-text-object-append",
        "Delete operator" => "settings-shortcut-delete-operator",
        "Change operator" => "settings-shortcut-change-operator",
        "Yank operator" => "settings-shortcut-yank-operator",
        "Toggle Visual mode" => "settings-shortcut-toggle-visual-mode",
        "Delete characters" => "settings-shortcut-delete-characters",
        "Paste after" => "settings-shortcut-paste-after",
        "Paste before" => "settings-shortcut-paste-before",
        "Choose question option 1" => "settings-shortcut-choose-question-option-1",
        "Choose question option 2" => "settings-shortcut-choose-question-option-2",
        "Choose question option 3" => "settings-shortcut-choose-question-option-3",
        "Choose question option 4" => "settings-shortcut-choose-question-option-4",
        "Choose question option 5" => "settings-shortcut-choose-question-option-5",
        "Choose question option 6" => "settings-shortcut-choose-question-option-6",
        "Choose question option 7" => "settings-shortcut-choose-question-option-7",
        "Choose question option 8" => "settings-shortcut-choose-question-option-8",
        "Choose question option 9" => "settings-shortcut-choose-question-option-9",
        "Application Vim count 0" => "settings-shortcut-application-vim-count-0",
        "Application Vim count 1" => "settings-shortcut-application-vim-count-1",
        "Application Vim count 2" => "settings-shortcut-application-vim-count-2",
        "Application Vim count 3" => "settings-shortcut-application-vim-count-3",
        "Application Vim count 4" => "settings-shortcut-application-vim-count-4",
        "Application Vim count 5" => "settings-shortcut-application-vim-count-5",
        "Application Vim count 6" => "settings-shortcut-application-vim-count-6",
        "Application Vim count 7" => "settings-shortcut-application-vim-count-7",
        "Application Vim count 8" => "settings-shortcut-application-vim-count-8",
        "Application Vim count 9" => "settings-shortcut-application-vim-count-9",
        "Count digit 0" => "settings-shortcut-count-digit-0",
        "Count digit 1" => "settings-shortcut-count-digit-1",
        "Count digit 2" => "settings-shortcut-count-digit-2",
        "Count digit 3" => "settings-shortcut-count-digit-3",
        "Count digit 4" => "settings-shortcut-count-digit-4",
        "Count digit 5" => "settings-shortcut-count-digit-5",
        "Count digit 6" => "settings-shortcut-count-digit-6",
        "Count digit 7" => "settings-shortcut-count-digit-7",
        "Count digit 8" => "settings-shortcut-count-digit-8",
        "Count digit 9" => "settings-shortcut-count-digit-9",
        _ => return None,
    })
}

pub(super) fn category(value: &str) -> SharedString {
    match value {
        "Application" => tr("settings-shortcut-category-application"),
        "Chat" => tr("settings-shortcut-category-chat"),
        "Transcript" => tr("settings-shortcut-category-transcript"),
        "Menus" => tr("settings-shortcut-category-menus"),
        "Text Editing" => tr("settings-shortcut-category-text-editing"),
        "Composer Vim" => tr("settings-shortcut-category-composer-vim"),
        "Application Vim" => tr("settings-shortcut-category-application-vim"),
        _ => value.to_string().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_shipped_shortcut_has_a_translation() {
        for slot in crate::keymap::catalog() {
            assert!(
                key(slot.label).is_some(),
                "untranslated shortcut: {}",
                slot.id
            );
        }
    }
}
