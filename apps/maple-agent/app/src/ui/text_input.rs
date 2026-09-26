//! Text input, ported from the gpui 0.2.2 `input` example and adapted for
//! Maple: neutral theming, optional password masking, an Enter-key hook for
//! form submit and composer send, and an optional multi-line mode that wraps
//! text and grows with its content (Shift+Enter inserts a newline).

mod bounds;
pub mod vim;
pub(crate) mod vim_actions;

use super::popup::{Menu, MenuItem, Placement, Popup};
use super::{application_vim, spell, theme};
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;

use gpui::{
    App, Bounds, ClipboardEntry, ClipboardItem, ContentMask, Context, CursorStyle, Element,
    ElementId, ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable, Font,
    GlobalElementId, InteractiveElement, LayoutId, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PaintQuad, Pixels, SharedString, Style, Subscription, TextAlign, TextRun, UTF16Selection,
    UnderlineStyle, Window, WrappedLine, actions, div, fill, point, prelude::*, px, relative, rgb,
    size,
};
use unicode_segmentation::UnicodeSegmentation;

use self::vim::{
    HistoryPlan, HistorySnapshot, InsertEditKind, InsertEntry, LifecycleEvent, Motion, VimCommand,
    VimMode, VimOutcome, VimSignal, VimState, VimStatus,
};
use self::vim_actions::{
    VimBeginOperator, VimCancel, VimContextual, VimCountDigit, VimDeleteChars, VimEnterInsert,
    VimMotion, VimOpenLine, VimPaste, VimRedo, VimRepeat, VimToggleVisual, VimUndo,
};

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
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
        ParagraphStart,
        ParagraphEnd,
        SelectParagraphStart,
        SelectParagraphEnd,
        DocumentStart,
        DocumentEnd,
        SelectDocumentStart,
        SelectDocumentEnd,
        Up,
        Down,
        SelectUp,
        SelectDown,
        ShowCharacterPalette,
        Paste,
        Cut,
        Copy,
        Undo,
        Redo,
    ]
);

type EnterHandler = Box<dyn Fn(String, &mut Window, &mut Context<TextInput>) + 'static>;
type PasteImageHandler = Box<dyn Fn(gpui::Image, &mut Window, &mut Context<TextInput>) + 'static>;
type VimLeaveHandler = Box<dyn Fn(&mut Window, &mut Context<TextInput>) + 'static>;
type ApplicationEscapeHandler = Box<dyn Fn(&mut Window, &mut Context<TextInput>) + 'static>;
/// First look at a key press with the input's current text. Return true to
/// consume it. The handler runs while this input is being updated, so it
/// must not read or update this input entity; defer anything that does.
type KeyHandler = Box<
    dyn Fn(&gpui::KeyDownEvent, &SharedString, &mut Window, &mut Context<TextInput>) -> bool
        + 'static,
>;
/// Called for the up (-1) and down (1) arrows; returning true consumes
/// the arrow instead of moving the caret.
type VerticalHandler = Box<dyn Fn(isize, &mut Window, &mut Context<TextInput>) -> bool + 'static>;

#[derive(Clone)]
struct ImeBaseline {
    content: String,
    range: Range<usize>,
}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    /// A soft wrap has two visual caret positions at the same byte offset.
    /// Line-start commands keep the caret on the following display row.
    cursor_affinity: CursorAffinity,
    /// Desired horizontal position during a run of vertical arrow commands.
    /// A short row must not pull later movements away from this column.
    vertical_goal_x: Option<Pixels>,
    marked_range: Option<Range<usize>>,
    last_layout: Option<TextLayout>,
    last_bounds: Option<Bounds<Pixels>>,
    /// Horizontal scroll of a single-line input so the cursor stays in
    /// view when the text is wider than the box.
    scroll_x: Pixels,
    /// Vertical scroll of a multi-line input so text taller than the box
    /// stays reachable. Positive values scroll down.
    scroll_y: Pixels,
    /// Scroll the cursor into view on the next prepaint. Set by edits and
    /// cursor moves; a wheel scroll leaves it unset so the view stays put.
    keep_cursor_visible: bool,
    /// Wrap long text and accept Shift+Enter newlines; the element grows
    /// with the content up to `max_lines`.
    multiline: bool,
    /// Fill the parent's height instead of sizing to the content
    /// (expanded composer).
    fill_height: bool,
    max_lines: usize,
    /// Last (text, wrap width, font, size, line-height) -> row count, so
    /// layout does not re-shape unchanged text every frame, and a chat
    /// face/size change does not keep a stale row count.
    measure_cache: Option<(SharedString, Pixels, Font, Pixels, Pixels, usize)>,
    /// Shaped lines from the last prepaint with the inputs they came
    /// from; reused while nothing that affects shaping has changed.
    shape_cache: Option<ShapeCache>,
    is_selecting: bool,
    /// The right-click menu, keyed by the window position it opened at.
    popup: Popup<TextInput, gpui::Point<Pixels>>,
    /// Render '*' in place of content characters (password fields).
    mask: bool,
    /// Clear the content once the Enter hook has consumed it (composer behavior).
    /// Explicit tab order for this input within its surface. Inputs without
    /// distinct indices collapse onto the same tab-stop path, which makes
    /// focus navigation a no-op.
    tab_index: Option<isize>,
    on_enter: Option<EnterHandler>,
    /// Called when the clipboard holds an image instead of text.
    on_paste_image: Option<PasteImageHandler>,
    /// First look at every key press; returning true consumes the key.
    on_key: Option<KeyHandler>,
    /// The up and down arrows are actions of this input, so the key hook
    /// never sees them; a list above the input takes them here.
    on_vertical: Option<VerticalHandler>,
    /// Underline words the dictionary rejects (composer only).
    spell_check: bool,
    /// Byte ranges of misspelled words, refreshed on every content change
    /// so prepaint only reads it.
    misspelled: Vec<Range<usize>>,
    /// `spell::generation()` at the last check. Text set before the
    /// dictionary finished loading is re-checked when this falls behind.
    spell_generation: u32,
    /// The misspelled word under the open right-click menu, with its
    /// replacement candidates.
    spell_menu: Option<(Range<usize>, Vec<String>)>,
    /// Text states before each edit, oldest first. Capped at
    /// [`UNDO_DEPTH`]; the oldest goes when the cap is reached.
    undo_stack: VecDeque<EditSnapshot>,
    /// States undone so far, newest last. An edit clears them.
    redo_stack: Vec<EditSnapshot>,
    /// Where the last edit left the cursor, so a run of typing or
    /// deleting at that point folds into one undo step.
    last_edit: Option<EditAnchor>,
    /// Whether this input is the main chat composer. Kept separately from the
    /// optional engine so a disabled composer still has the right key context
    /// without letting stale modal state participate in ordinary editing.
    is_composer: bool,
    /// Present only while composer Vim is enabled. An absent engine is a hard
    /// boundary: ordinary caret, selection, popup, and focus lifecycles cannot
    /// be overwritten by stale modal offsets.
    vim: Option<VimState>,
    /// Repeat the screen's application-Vim marker on the focused input's
    /// own key context. GPUI resolves bindings from the focused context, so
    /// this cannot rely on an ancestor screen marker alone.
    application_vim: bool,
    /// Stable state behind an active IME composition. Only its final commit
    /// becomes part of the current Vim insertion transaction.
    vim_ime_baseline: Option<ImeBaseline>,
    /// Synchronous application-region transition used by Normal Escape.
    on_vim_leave: Option<VimLeaveHandler>,
    /// Synchronous return from an ordinary field to its owning application
    /// navigation proxy. Kept separate from composer Vim's mode transition.
    on_application_escape: Option<ApplicationEscapeHandler>,
    /// Commits an Insert transaction and clears pending grammar when focus
    /// leaves the composer.
    focus_out_subscription: Option<Subscription>,
    /// Focus went from the field into its own right-click menu, which
    /// suspended Vim instead (see `on_right_click`). If the menu closes
    /// without handing focus back, focus left the field through it.
    focus_in_menu: bool,
}

/// How many text states one input remembers for undo.
const UNDO_DEPTH: usize = 128;

#[derive(Clone, Copy, PartialEq, Eq)]
enum CursorAffinity {
    Upstream,
    Downstream,
}

/// The content and selection before an edit.
#[derive(Clone)]
struct EditSnapshot {
    content: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    cursor_affinity: CursorAffinity,
}

/// The kinds of edit that fold into one undo step. Everything else
/// (a paste, an edit over a selection, an IME composition, new text
/// set from code) starts its own step.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Insert,
    Delete,
}

/// The last edit and the cursor it left behind.
#[derive(Clone, Copy, PartialEq, Eq)]
struct EditAnchor {
    kind: EditKind,
    offset: usize,
}

impl TextInput {
    /// Intercept key presses before the input's own handling. The composer
    /// uses this for slash-palette navigation; the handler receives the
    /// input's current text so it never reads this entity mid-update.
    pub fn on_key(
        mut self,
        handler: impl Fn(
            &gpui::KeyDownEvent,
            &SharedString,
            &mut Window,
            &mut Context<TextInput>,
        ) -> bool
        + 'static,
    ) -> Self {
        self.on_key = Some(Box::new(handler));
        self
    }

    /// Take the up and down arrows before they move the caret, for an
    /// input that drives a list. The handler runs inside this entity's
    /// update: defer any write back to the input.
    pub fn on_vertical(
        mut self,
        handler: impl Fn(isize, &mut Window, &mut Context<TextInput>) -> bool + 'static,
    ) -> Self {
        self.on_vertical = Some(Box::new(handler));
        self
    }

    pub fn new(placeholder: &str, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: SharedString::from(placeholder.to_string()),
            selected_range: 0..0,
            selection_reversed: false,
            cursor_affinity: CursorAffinity::Upstream,
            vertical_goal_x: None,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            scroll_x: px(0.),
            scroll_y: px(0.),
            keep_cursor_visible: false,
            multiline: false,
            fill_height: false,
            max_lines: 8,
            measure_cache: None,
            shape_cache: None,
            is_selecting: false,
            popup: Popup::new(|this| &mut this.popup, cx),
            mask: false,
            tab_index: None,
            on_enter: None,
            on_paste_image: None,
            on_key: None,
            on_vertical: None,
            spell_check: false,
            misspelled: Vec::new(),
            spell_generation: 0,
            spell_menu: None,
            undo_stack: VecDeque::new(),
            redo_stack: Vec::new(),
            last_edit: None,
            is_composer: false,
            vim: None,
            application_vim: false,
            vim_ime_baseline: None,
            on_vim_leave: None,
            on_application_escape: None,
            focus_out_subscription: None,
            focus_in_menu: false,
        }
    }

    /// Mark this input as the chat composer and choose its initial modal
    /// state. Ordinary inputs never call this builder.
    pub fn composer_vim(mut self, enabled: bool) -> Self {
        self.is_composer = true;
        self.vim = enabled.then(|| VimState::at(&self.content, self.cursor_offset()));
        self
    }

    pub fn application_vim(mut self, enabled: bool) -> Self {
        self.application_vim = enabled;
        self
    }

    pub fn set_application_vim_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.application_vim != enabled {
            self.application_vim = enabled;
            cx.notify();
        }
    }

    /// Handle a second composer Escape (already in Normal mode) by returning
    /// synchronously to the owning application's semantic focus proxy.
    pub fn on_vim_leave(
        mut self,
        handler: impl Fn(&mut Window, &mut Context<TextInput>) + 'static,
    ) -> Self {
        self.on_vim_leave = Some(Box::new(handler));
        self
    }

    /// Return Escape from an ordinary input to its screen-owned semantic
    /// focus proxy while application Vim is enabled. The key binding remains
    /// context-gated, so ordinary inputs keep their standard editing behavior
    /// when application Vim is off.
    pub fn on_application_escape(
        mut self,
        handler: impl Fn(&mut Window, &mut Context<TextInput>) + 'static,
    ) -> Self {
        self.on_application_escape = Some(Box::new(handler));
        self
    }

    pub fn vim_mode(&self) -> Option<VimMode> {
        self.vim.as_ref().map(VimState::mode)
    }

    pub fn vim_status(&self) -> Option<VimStatus> {
        self.vim.as_ref().map(VimState::status)
    }

    /// Underline misspelled words and offer replacements in the
    /// right-click menu.
    pub fn spell_check(mut self) -> Self {
        self.spell_check = true;
        self
    }

    /// Recompute the misspelled ranges. Call after every content change.
    fn refresh_spelling(&mut self) {
        if self.spell_check && !self.mask {
            self.spell_generation = spell::generation();
            self.misspelled = spell::misspelled_ranges(&self.content);
        }
    }

    /// Replace the word under the right-click menu with `replacement`.
    fn apply_suggestion(&mut self, range: Range<usize>, replacement: &str, cx: &mut Context<Self>) {
        if self.content.get(range.clone()).is_none() {
            return;
        }
        self.replace_range_with_kind(range, replacement, InsertEditKind::SelectionReplacement, cx);
    }

    /// Give this input an explicit position in the tab order.
    ///
    /// The order lives on the focus handle, not only on the element: gpui
    /// applies an element's `tab_index` only to a focus handle it creates
    /// itself, and this input tracks its own handle. Without this the handle
    /// stays a non-tab-stop and `window.focus_next` skips the input.
    pub fn with_tab_index(mut self, index: isize) -> Self {
        self.tab_index = Some(index);
        self.focus_handle = self.focus_handle.clone().tab_index(index).tab_stop(true);
        self
    }

    pub fn masked(mut self) -> Self {
        self.mask = true;
        self
    }

    /// Wrap text and grow with the content, up to `max_lines` rows.
    pub fn multiline(mut self, max_lines: usize) -> Self {
        self.multiline = true;
        self.max_lines = max_lines.max(1);
        self
    }

    /// Fill the parent's height (used while the composer is expanded).
    pub fn set_fill_height(&mut self, fill: bool, cx: &mut Context<Self>) {
        if self.fill_height != fill {
            self.fill_height = fill;
            cx.notify();
        }
    }

    pub fn on_enter(
        mut self,
        handler: impl Fn(String, &mut Window, &mut Context<TextInput>) + 'static,
    ) -> Self {
        self.on_enter = Some(Box::new(handler));
        self
    }

    /// Receive images pasted with Ctrl-V; text pastes go into the input.
    pub fn on_paste_image(
        mut self,
        handler: impl Fn(gpui::Image, &mut Window, &mut Context<TextInput>) + 'static,
    ) -> Self {
        self.on_paste_image = Some(Box::new(handler));
        self
    }

    /// Attach or replace the Enter hook after construction. The handler
    /// receives the current text so it never reads this entity re-entrantly.
    pub fn set_on_enter(
        &mut self,
        handler: impl Fn(String, &mut Window, &mut Context<TextInput>) + 'static,
    ) {
        self.on_enter = Some(Box::new(handler));
    }

    pub fn text(&self) -> String {
        self.content.to_string()
    }

    /// The content without a copy, for checks that only read it.
    pub fn text_ref(&self) -> &str {
        &self.content
    }

    pub fn set_placeholder(&mut self, text: &str, cx: &mut Context<Self>) {
        self.placeholder = SharedString::from(text.to_string());
        cx.notify();
    }

    /// Change the composer setting live without replacing its draft.
    pub fn set_vim_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if !self.is_composer {
            return;
        }

        match (enabled, self.vim.take()) {
            (true, None) => {
                // Standard-mode edits are not tracked by the Vim engine. A
                // newly enabled session starts from the live ordinary caret
                // with empty registers and repeat state.
                self.vim = Some(VimState::at(&self.content, self.cursor_offset()));
                self.apply_vim_selection();
                self.last_edit = None;
                cx.notify();
            }
            (true, Some(vim)) => {
                self.vim = Some(vim);
            }
            (false, Some(mut vim)) => {
                // Commit an active Insert transaction and mirror its final
                // caret once before dropping the engine. From this point on,
                // ordinary editing has no modal lifecycle to run.
                let outcome = vim.handle_lifecycle(&self.content, LifecycleEvent::Disable);
                self.vim = Some(vim);
                self.apply_vim_outcome(outcome, cx);
                self.vim = None;
                self.vim_ime_baseline = None;
                cx.notify();
            }
            (false, None) => {}
        }
    }

    /// Existing Chat type-to-compose remains ordinary when only composer Vim
    /// is enabled: entering from another surface starts one Insert transaction
    /// before the first character is applied.
    pub fn prepare_for_typing(&mut self, cx: &mut Context<Self>) {
        if matches!(self.vim_mode(), Some(VimMode::Normal | VimMode::Visual)) {
            if self.vim_mode() == Some(VimMode::Visual) {
                self.execute_vim_command(VimCommand::Cancel, cx);
            }
            self.execute_vim_command(VimCommand::EnterInsert(InsertEntry::BeforeCursor), cx);
        }
    }

    /// A task change is a hard modal boundary even though Maple currently
    /// keeps one visible composer draft. Registers, dot state, and history
    /// must not accidentally operate across task ownership.
    pub fn reset_vim_context(&mut self, cx: &mut Context<Self>) {
        if let Some(mut vim) = self.vim.take() {
            let outcome =
                vim.handle_lifecycle(&self.content, LifecycleEvent::ExternalDraftReplacement);
            self.vim = Some(vim);
            self.apply_vim_outcome(outcome, cx);
            let cursor_offset = self.cursor_offset();
            if let Some(vim) = &mut self.vim {
                vim.reset_after_external_text(&self.content, cursor_offset, false);
            }
            self.apply_vim_selection();
            self.forget_edits();
            cx.notify();
        }
    }

    /// Application `gi`: enter Insert at the last insertion boundary. Draft
    /// replacement paths reset the engine first, so the saved byte offset is
    /// always valid for the current text.
    pub fn focus_last_insertion(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(mut vim) = self.vim.take() else {
            return false;
        };
        if vim.mode() == VimMode::Disabled {
            self.vim = Some(vim);
            return false;
        }
        let outcome = vim.enter_at_last_insertion(&self.content);
        self.vim = Some(vim);
        self.apply_vim_outcome(outcome, cx);
        true
    }

    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.vim.is_some() {
            self.finish_vim_lifecycle(LifecycleEvent::ExternalDraftReplacement, cx);
            self.forget_edits();
        } else {
            self.record_edit(false);
            self.last_edit = None;
        }
        self.content = SharedString::from(text.to_string());
        self.selected_range = self.content.len()..self.content.len();
        self.keep_cursor_visible = true;
        self.forget_text_positions(cx);
        if let Some(vim) = &mut self.vim {
            vim.reset_after_external_text(&self.content, self.content.len(), false);
        }
        self.apply_vim_selection();
        self.refresh_spelling();
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.finish_vim_lifecycle(LifecycleEvent::SendOrClear, cx);
        self.forget_edits();
        self.content = "".into();
        self.selected_range = 0..0;
        self.scroll_y = px(0.);
        self.forget_text_positions(cx);
        self.misspelled.clear();
        if let Some(vim) = &mut self.vim {
            vim.reset_after_external_text("", 0, true);
        }
        self.apply_vim_selection();
        cx.notify();
    }

    /// Drop every state that holds a byte range into the old content: an
    /// IME composition, a reversed selection, and the menus whose items
    /// point at a word.
    fn forget_text_positions(&mut self, cx: &mut Context<Self>) {
        self.vertical_goal_x = None;
        self.cursor_affinity = CursorAffinity::Upstream;
        self.marked_range = None;
        self.vim_ime_baseline = None;
        self.selection_reversed = false;
        self.spell_menu = None;
        self.popup.close(cx);
    }

    fn push_history_snapshot(&mut self, snapshot: HistorySnapshot) {
        if self.undo_stack.len() == UNDO_DEPTH {
            self.undo_stack.pop_front();
        }
        self.undo_stack.push_back(EditSnapshot {
            content: snapshot.text.into(),
            selected_range: snapshot.cursor..snapshot.cursor,
            selection_reversed: false,
            cursor_affinity: CursorAffinity::Upstream,
        });
        self.redo_stack.clear();
        self.last_edit = None;
    }

    fn apply_vim_history(&mut self, history: HistoryPlan, cx: &mut Context<Self>) {
        match history {
            HistoryPlan::None => {}
            HistoryPlan::Reset => self.forget_edits(),
            HistoryPlan::Commit { before, .. } => self.push_history_snapshot(before),
            HistoryPlan::Undo { count } => {
                for _ in 0..count {
                    let Some(previous) = self.undo_stack.pop_back() else {
                        break;
                    };
                    self.redo_stack.push(self.snapshot());
                    self.restore(previous, cx);
                    let cursor = self.cursor_offset();
                    if let Some(vim) = &mut self.vim {
                        vim.sync_after_history(&self.content, cursor);
                    }
                }
            }
            HistoryPlan::Redo { count } => {
                for _ in 0..count {
                    let Some(next) = self.redo_stack.pop() else {
                        break;
                    };
                    self.undo_stack.push_back(self.snapshot());
                    self.restore(next, cx);
                    let cursor = self.cursor_offset();
                    if let Some(vim) = &mut self.vim {
                        vim.sync_after_history(&self.content, cursor);
                    }
                }
            }
        }
    }

    fn apply_vim_selection(&mut self) {
        let Some(vim) = &self.vim else {
            return;
        };
        let selection = vim.selection(&self.content);
        self.selected_range = selection.range;
        self.selection_reversed = selection.reversed;
    }

    fn apply_vim_outcome(&mut self, outcome: VimOutcome, cx: &mut Context<Self>) {
        self.vertical_goal_x = None;
        self.cursor_affinity = CursorAffinity::Upstream;
        let text_changed = outcome.text_changed;
        if outcome.consumed {
            self.keep_cursor_visible = true;
        }
        self.apply_vim_history(outcome.history, cx);
        self.apply_vim_selection();
        self.last_edit = None;
        self.marked_range = None;
        self.vim_ime_baseline = None;
        if text_changed {
            self.refresh_spelling();
        }
        cx.notify();
    }

    fn finish_vim_lifecycle(&mut self, event: LifecycleEvent, cx: &mut Context<Self>) {
        let Some(mut vim) = self.vim.take() else {
            return;
        };
        let outcome = vim.handle_lifecycle(&self.content, event);
        self.vim = Some(vim);
        self.apply_vim_outcome(outcome, cx);
    }

    fn execute_vim_command_with_signal(
        &mut self,
        command: VimCommand,
        cx: &mut Context<Self>,
    ) -> (bool, VimSignal) {
        if self.marked_range.is_some() && command == VimCommand::Cancel {
            if let Some(baseline) = self.vim_ime_baseline.take() {
                let cursor = baseline.range.start;
                self.content = baseline.content.into();
                self.selected_range = cursor..cursor;
                self.refresh_spelling();
            }
            self.marked_range = None;
            cx.notify();
            return (true, VimSignal::None);
        }
        let Some(mut vim) = self.vim.take() else {
            return (false, VimSignal::None);
        };
        let before = self.content.to_string();
        let mut text = before.clone();
        let outcome = vim.handle_command(&mut text, command);
        let consumed = outcome.consumed;
        let signal = outcome.signal;
        self.vim = Some(vim);
        if text != before {
            self.content = text.into();
        }
        self.apply_vim_outcome(outcome, cx);
        (consumed, signal)
    }

    fn execute_vim_command(&mut self, command: VimCommand, cx: &mut Context<Self>) -> bool {
        self.execute_vim_command_with_signal(command, cx).0
    }

    fn invoke_vim_leave(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handler) = self.on_vim_leave.take() {
            handler(window, cx);
            self.on_vim_leave = Some(handler);
        }
    }

    fn dispatch_vim_action(&mut self, command: VimCommand, cx: &mut Context<Self>) {
        if self.execute_vim_command(command, cx) {
            cx.stop_propagation();
        }
    }

    fn replace_range_with_kind(
        &mut self,
        range: Range<usize>,
        new_text: &str,
        vim_kind: InsertEditKind,
        cx: &mut Context<Self>,
    ) {
        if self
            .vim_mode()
            .is_some_and(|mode| mode != VimMode::Disabled)
        {
            if self.vim_mode() != Some(VimMode::Insert) {
                return;
            }
            let Some(mut vim) = self.vim.take() else {
                return;
            };
            let before = self.content.to_string();
            let mut text = before.clone();
            let cursor_after = range.start.saturating_add(new_text.len());
            let outcome = vim.insert_edit(&mut text, range, new_text, cursor_after, vim_kind);
            self.vim = Some(vim);
            if text != before {
                self.content = text.into();
            }
            self.apply_vim_outcome(outcome, cx);
            return;
        }

        self.replace_range_standard(range, new_text, cx);
    }

    #[cfg(test)]
    fn replace_range(&mut self, range: Range<usize>, new_text: &str, cx: &mut Context<Self>) {
        let kind = if range.is_empty() {
            InsertEditKind::Text
        } else {
            InsertEditKind::SelectionReplacement
        };
        self.replace_range_with_kind(range, new_text, kind, cx);
    }

    /// The ordinary editor core: swap `range` for `new_text`, record the
    /// undo step, and refresh what old byte ranges pointed at.
    fn replace_range_standard(
        &mut self,
        range: Range<usize>,
        new_text: &str,
        cx: &mut Context<Self>,
    ) {
        // An IME composition was already recorded when it started; a
        // single typed or deleted character continues the run the last
        // one started; anything else is its own undo step.
        let composing = self.marked_range.is_some();
        let kind = if composing {
            None
        } else if new_text.is_empty() && !range.is_empty() {
            Some(EditKind::Delete)
        } else if range.is_empty() && new_text.chars().count() == 1 && new_text != "\n" {
            Some(EditKind::Insert)
        } else {
            None
        };
        if !composing {
            let continues = match (kind, self.last_edit) {
                (Some(EditKind::Insert), Some(last)) => {
                    last.kind == EditKind::Insert && last.offset == range.start
                }
                (Some(EditKind::Delete), Some(last)) => {
                    last.kind == EditKind::Delete
                        && (last.offset == range.start || last.offset == range.end)
                }
                _ => false,
            };
            self.record_edit(continues);
        }

        self.cursor_affinity = CursorAffinity::Upstream;
        self.vertical_goal_x = None;
        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.last_edit = kind.map(|kind| EditAnchor {
            kind,
            offset: self.selected_range.start,
        });
        self.marked_range.take();
        // Menu items hold ranges into the old text.
        self.spell_menu = None;
        self.popup.close(cx);
        self.keep_cursor_visible = true;
        self.refresh_spelling();
        cx.notify();
    }

    fn snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            content: self.content.clone(),
            selected_range: self.selected_range.clone(),
            selection_reversed: self.selection_reversed,
            cursor_affinity: self.cursor_affinity,
        }
    }

    /// Keep the text as it is now, so undo can come back to it. A step
    /// that continues the run the last edit started is folded into it;
    /// every recorded step drops the redo history.
    fn record_edit(&mut self, continues_run: bool) {
        if !continues_run {
            if self.undo_stack.len() == UNDO_DEPTH {
                self.undo_stack.pop_front();
            }
            self.undo_stack.push_back(self.snapshot());
        }
        self.redo_stack.clear();
    }

    /// Forget the edit history. For content that is replaced wholesale
    /// and must not come back, such as a sent composer message.
    fn forget_edits(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.last_edit = None;
    }

    fn restore(&mut self, snapshot: EditSnapshot, cx: &mut Context<Self>) {
        self.content = snapshot.content;
        self.forget_text_positions(cx);
        self.selected_range = snapshot.selected_range;
        self.selection_reversed = snapshot.selection_reversed;
        self.cursor_affinity = snapshot.cursor_affinity;
        self.last_edit = None;
        self.keep_cursor_visible = true;
        self.refresh_spelling();
        cx.notify();
    }

    fn undo_last(&mut self, cx: &mut Context<Self>) {
        let Some(previous) = self.undo_stack.pop_back() else {
            return;
        };
        self.redo_stack.push(self.snapshot());
        self.restore(previous, cx);
    }

    fn redo_last(&mut self, cx: &mut Context<Self>) {
        let Some(next) = self.redo_stack.pop() else {
            return;
        };
        self.undo_stack.push_back(self.snapshot());
        self.restore(next, cx);
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if self
            .vim_mode()
            .is_some_and(|mode| mode != VimMode::Disabled)
        {
            self.execute_vim_command(VimCommand::Undo, cx);
        } else {
            self.undo_last(cx);
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if self
            .vim_mode()
            .is_some_and(|mode| mode != VimMode::Disabled)
        {
            self.execute_vim_command(VimCommand::Redo, cx);
        } else {
            self.redo_last(cx);
        }
    }

    fn enter_pressed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Hand the handler the current text so it never has to read this
        // entity while it is being updated.
        let text = self.text();
        if let Some(on_enter) = self.on_enter.take() {
            on_enter(text, window, cx);
            self.on_enter = Some(on_enter);
        }
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.vim_mode(), Some(VimMode::Normal | VimMode::Visual)) {
            self.execute_vim_command(VimCommand::Motion(Motion::Left), cx);
            return;
        }
        if self.vim.is_none() && self.marked_range.is_some() {
            self.replace_text_in_range(None, "", window, cx);
            return;
        }
        let range = if self.selected_range.is_empty() {
            self.previous_boundary(self.cursor_offset())..self.cursor_offset()
        } else {
            self.selected_range.clone()
        };
        let _ = window;
        self.replace_range_with_kind(range, "", InsertEditKind::Backspace, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.vim_mode(), Some(VimMode::Normal | VimMode::Visual)) {
            self.execute_vim_command(VimCommand::DeleteChars, cx);
            return;
        }
        if self.vim.is_none() && self.marked_range.is_some() {
            self.replace_text_in_range(None, "", window, cx);
            return;
        }
        let range = if self.selected_range.is_empty() {
            self.cursor_offset()..self.next_boundary(self.cursor_offset())
        } else {
            self.selected_range.clone()
        };
        let _ = window;
        self.replace_range_with_kind(range, "", InsertEditKind::Delete, cx)
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.vim_mode(), Some(VimMode::Normal | VimMode::Visual)) {
            self.execute_vim_command(VimCommand::Motion(Motion::Left), cx);
            return;
        }
        if self.selected_range.is_empty() {
            let offset = self.previous_boundary(self.cursor_offset());
            self.move_to(offset, cx);
            self.sync_insert_cursor(offset, cx);
        } else {
            let offset = self.selected_range.start;
            self.move_to(offset, cx);
            self.sync_insert_cursor(offset, cx);
        }
        self.cursor_affinity = CursorAffinity::Downstream;
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.vim_mode(), Some(VimMode::Normal | VimMode::Visual)) {
            self.execute_vim_command(VimCommand::Motion(Motion::Right), cx);
            return;
        }
        if self.selected_range.is_empty() {
            let offset = self.next_boundary(self.selected_range.end);
            self.move_to(offset, cx);
            self.sync_insert_cursor(offset, cx);
        } else {
            let offset = self.selected_range.end;
            self.move_to(offset, cx);
            self.sync_insert_cursor(offset, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.vim_mode(), Some(VimMode::Normal | VimMode::Visual)) {
            self.execute_vim_command(VimCommand::Motion(Motion::Left), cx);
            return;
        }
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        self.sync_insert_cursor(self.cursor_offset(), cx);
        self.cursor_affinity = CursorAffinity::Downstream;
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.vim_mode(), Some(VimMode::Normal | VimMode::Visual)) {
            self.execute_vim_command(VimCommand::Motion(Motion::Right), cx);
            return;
        }
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
        self.sync_insert_cursor(self.cursor_offset(), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &Home, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.vim_mode(), Some(VimMode::Normal | VimMode::Visual)) {
            self.execute_vim_command(VimCommand::Motion(Motion::LineStart), cx);
        } else {
            let offset = self.line_target(false, window);
            self.move_to_offset(offset, cx);
            self.cursor_affinity = CursorAffinity::Downstream;
        }
    }

    fn end(&mut self, _: &End, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.vim_mode(), Some(VimMode::Normal | VimMode::Visual)) {
            self.execute_vim_command(VimCommand::Motion(Motion::LineEnd), cx);
        } else {
            let offset = self.line_target(true, window);
            self.move_to_offset(offset, cx);
        }
    }

    fn select_line_start(
        &mut self,
        _: &SelectLineStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_line_edge(false, window, cx);
    }

    fn select_line_end(&mut self, _: &SelectLineEnd, window: &mut Window, cx: &mut Context<Self>) {
        self.select_line_edge(true, window, cx);
    }

    fn delete_to_line_start(
        &mut self,
        _: &DeleteToLineStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_to_line_edge(false, InsertEditKind::Backspace, window, cx);
    }

    fn delete_to_line_end(
        &mut self,
        _: &DeleteToLineEnd,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_to_line_edge(true, InsertEditKind::Delete, window, cx);
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_by_word(false, cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_by_word(true, cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_by_word(false, cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_by_word(true, cx);
    }

    fn delete_word_backward(
        &mut self,
        _: &DeleteWordBackward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_word(false, window, cx);
    }

    fn delete_word_forward(
        &mut self,
        _: &DeleteWordForward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_word(true, window, cx);
    }

    /// Normal and Visual keep Vim's own motions. These shortcuts are for
    /// Vim off and Insert, which is where text is typed.
    fn standard_editing(&self) -> bool {
        !matches!(self.vim_mode(), Some(VimMode::Normal | VimMode::Visual))
    }

    fn move_to_offset(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.move_to(offset, cx);
        self.sync_insert_cursor(offset, cx);
    }

    fn move_by_word(&mut self, forward: bool, cx: &mut Context<Self>) {
        if !self.standard_editing() {
            return;
        }
        let offset = self.word_target(forward);
        self.move_to_offset(offset, cx);
        if !forward {
            self.cursor_affinity = CursorAffinity::Downstream;
        }
    }

    fn select_by_word(&mut self, forward: bool, cx: &mut Context<Self>) {
        if !self.standard_editing() {
            return;
        }
        let offset = self.word_target(forward);
        self.select_to(offset, cx);
        self.sync_insert_cursor(self.cursor_offset(), cx);
        if !forward {
            self.cursor_affinity = CursorAffinity::Downstream;
        }
    }

    fn select_line_edge(&mut self, end: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.standard_editing() {
            return;
        }
        let offset = self.line_target(end, window);
        if cfg!(target_os = "macos") {
            self.extend_to_edge(offset, end, cx);
        } else {
            self.select_to(offset, cx);
            self.sync_insert_cursor(self.cursor_offset(), cx);
        }
        self.cursor_affinity = if end {
            CursorAffinity::Upstream
        } else {
            CursorAffinity::Downstream
        };
    }

    fn delete_word(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.standard_editing() {
            return;
        }
        if self.consume_marked_text(window, cx) {
            return;
        }
        let range = if self.selected_range.is_empty() {
            let cursor = self.cursor_offset();
            let target = self.word_target(forward);
            if forward {
                cursor..target
            } else {
                target..cursor
            }
        } else {
            self.selected_range.clone()
        };
        if range.is_empty() {
            return;
        }
        let kind = if forward {
            InsertEditKind::Delete
        } else {
            InsertEditKind::Backspace
        };
        self.replace_range_with_kind(range, "", kind, cx);
    }

    fn delete_to_line_edge(
        &mut self,
        end: bool,
        kind: InsertEditKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.standard_editing() {
            return;
        }
        if self.consume_marked_text(window, cx) {
            return;
        }
        let range = if self.selected_range.is_empty() {
            let cursor = self.cursor_offset();
            let mut target = self.line_target(end, window);
            if cfg!(target_os = "macos")
                && end
                && target == cursor
                && self.content[cursor..].starts_with('\n')
            {
                target += 1;
            }
            if target < cursor {
                target..cursor
            } else {
                cursor..target
            }
        } else {
            self.selected_range.clone()
        };
        if range.is_empty() {
            return;
        }
        self.replace_range_with_kind(range, "", kind, cx);
    }

    /// macOS line commands follow the rows the user sees. Paragraph commands
    /// and Vim motions continue to use explicit newline boundaries.
    fn line_target(&mut self, end: bool, window: &mut Window) -> usize {
        let (cursor, affinity) = if cfg!(target_os = "macos") && !self.selected_range.is_empty() {
            if end {
                (self.selected_range.end, CursorAffinity::Upstream)
            } else {
                (self.selected_range.start, CursorAffinity::Downstream)
            }
        } else {
            (self.cursor_offset(), self.cursor_affinity)
        };
        if cfg!(target_os = "macos") && self.multiline {
            self.ensure_navigation_layout(window);
            if let Some(layout) = self
                .last_layout
                .as_ref()
                .filter(|l| self.layout_is_current(l))
                && let Some(range) =
                    layout.visual_line_range(self.to_display_offset(cursor), affinity)
            {
                return self.content_offset_for_display(if end { range.end } else { range.start });
            }
        }
        if end {
            bounds::line_end(&self.content, cursor)
        } else {
            bounds::line_start(&self.content, cursor)
        }
    }

    /// Multiple key events can arrive before prepaint. Re-shape only when an
    /// edit made the last layout stale, using the field's actual font and width.
    fn ensure_navigation_layout(&mut self, window: &mut Window) {
        if self
            .last_layout
            .as_ref()
            .is_some_and(|l| self.layout_is_current(l))
        {
            return;
        }
        let Some(cache) = &self.shape_cache else {
            return;
        };
        let Some(base_run) = cache.runs.first() else {
            return;
        };
        let text = self.display_text();
        let run = TextRun {
            len: text.len(),
            ..base_run.clone()
        };
        if let Ok(lines) = window.text_system().shape_text(
            text.clone(),
            cache.font_size,
            &[run],
            cache.wrap_width,
            None,
        ) {
            self.last_layout = Some(TextLayout {
                lines: Arc::new(lines.into_vec()),
                line_height: self
                    .last_layout
                    .as_ref()
                    .map_or(window.line_height(), |l| l.line_height),
                text,
            });
        }
    }

    fn paragraph_start(&mut self, _: &ParagraphStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_paragraph(false, false, cx);
    }

    fn paragraph_end(&mut self, _: &ParagraphEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_paragraph(true, false, cx);
    }

    fn select_paragraph_start(
        &mut self,
        _: &SelectParagraphStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_paragraph(false, true, cx);
    }

    fn select_paragraph_end(
        &mut self,
        _: &SelectParagraphEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_paragraph(true, true, cx);
    }

    fn move_paragraph(&mut self, forward: bool, select: bool, cx: &mut Context<Self>) {
        if !self.standard_editing() {
            return;
        }
        let cursor = self.cursor_offset();
        let mut offset = if !select && !self.selected_range.is_empty() {
            if forward {
                bounds::line_end(&self.content, self.selected_range.end)
            } else {
                bounds::line_start(&self.content, self.selected_range.start)
            }
        } else if forward && select {
            (bounds::line_end(&self.content, cursor) + 1).min(self.content.len())
        } else if forward {
            bounds::line_end(&self.content, self.next_boundary(cursor))
        } else {
            bounds::line_start(&self.content, self.previous_boundary(cursor))
        };
        if select && !self.selected_range.is_empty() {
            // Native paragraph selection stops at its anchor when reversing.
            if forward && self.selection_reversed {
                offset = offset.min(self.selected_range.end);
            } else if !forward && !self.selection_reversed {
                offset = offset.max(self.selected_range.start);
            }
        }
        self.move_standard(offset, select, cx);
        if select || !forward {
            self.cursor_affinity = CursorAffinity::Downstream;
        }
    }

    fn document_start(&mut self, _: &DocumentStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_standard(0, false, cx);
    }

    fn document_end(&mut self, _: &DocumentEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_standard(self.content.len(), false, cx);
    }

    fn select_document_start(
        &mut self,
        _: &SelectDocumentStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.extend_to_edge(0, false, cx);
    }

    fn select_document_end(
        &mut self,
        _: &SelectDocumentEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.extend_to_edge(self.content.len(), true, cx);
    }

    /// Native line/document selection extends the corresponding end of the
    /// entire selection, unlike paragraph selection's anchored active head.
    fn extend_to_edge(&mut self, offset: usize, end: bool, cx: &mut Context<Self>) {
        if !self.standard_editing() {
            return;
        }
        self.selection_reversed = !end;
        self.move_standard(offset, true, cx);
    }

    fn move_standard(&mut self, offset: usize, select: bool, cx: &mut Context<Self>) {
        if !self.standard_editing() {
            return;
        }
        if select {
            self.select_to(offset, cx);
            let selection = self.selected_range.clone();
            let reversed = self.selection_reversed;
            self.sync_insert_cursor(self.cursor_offset(), cx);
            // These Shift commands retain a selection in Vim Insert mode.
            self.selected_range = selection;
            self.selection_reversed = reversed;
        } else {
            self.move_to_offset(offset, cx);
        }
        cx.stop_propagation();
    }

    fn word_target(&self, forward: bool) -> usize {
        let cursor = self.cursor_offset();
        if forward {
            bounds::word_right(&self.content, cursor, bounds::host_word_stop())
        } else {
            bounds::word_left(&self.content, cursor)
        }
    }

    /// An in-progress IME composition owns the next delete. Returns true
    /// when that composition was cleared.
    fn consume_marked_text(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.vim.is_none() && self.marked_range.is_some() {
            self.replace_text_in_range(None, "", window, cx);
            true
        } else {
            false
        }
    }

    /// Offer an arrow to the vertical hook; true when it took it.
    fn take_vertical(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(on_vertical) = self.on_vertical.take() else {
            return false;
        };
        let consumed = on_vertical(delta, window, cx);
        self.on_vertical = Some(on_vertical);
        consumed
    }

    fn up(&mut self, _: &Up, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.vim_mode(), Some(VimMode::Normal | VimMode::Visual)) {
            self.execute_vim_command(VimCommand::Motion(Motion::Up), cx);
            return;
        }
        if self.take_vertical(-1, window, cx) {
            return;
        }
        self.ensure_navigation_layout(window);
        match self.vertical_neighbor(-1, None) {
            Some((offset, affinity)) => {
                self.move_to(offset, cx);
                self.sync_insert_cursor(offset, cx);
                self.cursor_affinity = affinity;
            }
            None => {
                self.move_to(0, cx);
                self.sync_insert_cursor(0, cx);
            }
        }
    }

    fn down(&mut self, _: &Down, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.vim_mode(), Some(VimMode::Normal | VimMode::Visual)) {
            self.execute_vim_command(VimCommand::Motion(Motion::Down), cx);
            return;
        }
        if self.take_vertical(1, window, cx) {
            return;
        }
        self.ensure_navigation_layout(window);
        match self.vertical_neighbor(1, None) {
            Some((offset, affinity)) => {
                self.move_to(offset, cx);
                self.sync_insert_cursor(offset, cx);
                self.cursor_affinity = affinity;
            }
            None => {
                let offset = self.content.len();
                self.move_to(offset, cx);
                self.sync_insert_cursor(offset, cx);
            }
        }
    }

    fn select_up(&mut self, _: &SelectUp, window: &mut Window, cx: &mut Context<Self>) {
        self.select_vertical(-1, window, cx);
    }

    fn select_down(&mut self, _: &SelectDown, window: &mut Window, cx: &mut Context<Self>) {
        self.select_vertical(1, window, cx);
    }

    fn select_vertical(&mut self, direction: i32, window: &mut Window, cx: &mut Context<Self>) {
        if !self.standard_editing() {
            return;
        }
        self.ensure_navigation_layout(window);
        let goal_x = self.vertical_goal_x.or_else(|| {
            self.last_layout
                .as_ref()?
                .position_for_index_with_affinity(
                    self.to_display_offset(self.cursor_offset()),
                    self.cursor_affinity,
                )
                .map(|position| position.x)
        });
        let neighbor = self.vertical_neighbor(direction, goal_x);
        let (offset, affinity) = neighbor.unwrap_or_else(|| {
            if direction < 0 {
                (0, CursorAffinity::Downstream)
            } else {
                (self.content.len(), CursorAffinity::Upstream)
            }
        });
        self.move_standard(offset, true, cx);
        self.cursor_affinity = affinity;
        // Reaching a document boundary starts a new horizontal goal, unlike
        // temporarily clamping to the end of a shorter intervening row.
        self.vertical_goal_x = neighbor.and(goal_x);
    }

    fn sync_insert_cursor(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.vim_mode() != Some(VimMode::Insert) {
            return;
        }
        let Some(mut vim) = self.vim.take() else {
            return;
        };
        let outcome = vim.move_insert_cursor(&self.content, offset);
        self.vim = Some(vim);
        self.apply_vim_outcome(outcome, cx);
    }

    fn key_context(&self) -> &'static str {
        match (self.application_vim, self.is_composer, self.vim_mode()) {
            (true, true, Some(VimMode::Normal)) => {
                "TextInput ApplicationVim input_role = composer editor_vim_mode = normal"
            }
            (true, true, Some(VimMode::Insert)) => {
                "TextInput ApplicationVim input_role = composer editor_vim_mode = insert"
            }
            (true, true, Some(VimMode::Visual)) => {
                "TextInput ApplicationVim input_role = composer editor_vim_mode = visual"
            }
            (true, true, Some(VimMode::Disabled) | None) => {
                "TextInput ApplicationVim input_role = composer editor_vim_mode = disabled"
            }
            (true, false, _) => {
                "TextInput ApplicationVim input_role = other editor_vim_mode = disabled"
            }
            (false, true, Some(VimMode::Normal)) => {
                "TextInput input_role = composer editor_vim_mode = normal"
            }
            (false, true, Some(VimMode::Insert)) => {
                "TextInput input_role = composer editor_vim_mode = insert"
            }
            (false, true, Some(VimMode::Visual)) => {
                "TextInput input_role = composer editor_vim_mode = visual"
            }
            (false, true, Some(VimMode::Disabled) | None) => {
                "TextInput input_role = composer editor_vim_mode = disabled"
            }
            (false, false, _) => "TextInput input_role = other editor_vim_mode = disabled",
        }
    }

    fn vim_motion(&mut self, action: &VimMotion, _: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_vim_action(VimCommand::Motion(action.motion), cx);
    }

    fn vim_begin_operator(
        &mut self,
        action: &VimBeginOperator,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_vim_action(VimCommand::BeginOperator(action.operator), cx);
    }

    fn vim_count_digit(&mut self, action: &VimCountDigit, _: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_vim_action(VimCommand::CountDigit(action.digit), cx);
    }

    fn vim_contextual(&mut self, action: &VimContextual, _: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_vim_action(VimCommand::Contextual(action.token), cx);
    }

    fn vim_enter_insert(
        &mut self,
        action: &VimEnterInsert,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_vim_action(VimCommand::EnterInsert(action.placement), cx);
    }

    fn vim_open_line(&mut self, action: &VimOpenLine, _: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_vim_action(VimCommand::OpenLine(action.placement), cx);
    }

    fn vim_toggle_visual(&mut self, _: &VimToggleVisual, _: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_vim_action(VimCommand::ToggleVisual, cx);
    }

    fn vim_delete_chars(&mut self, _: &VimDeleteChars, _: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_vim_action(VimCommand::DeleteChars, cx);
    }

    fn vim_paste(&mut self, action: &VimPaste, _: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_vim_action(VimCommand::Paste(action.placement), cx);
    }

    fn vim_undo(&mut self, _: &VimUndo, _: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_vim_action(VimCommand::Undo, cx);
    }

    fn vim_redo(&mut self, _: &VimRedo, _: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_vim_action(VimCommand::Redo, cx);
    }

    fn vim_repeat(&mut self, _: &VimRepeat, _: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_vim_action(VimCommand::Repeat, cx);
    }

    fn vim_cancel(&mut self, _: &VimCancel, window: &mut Window, cx: &mut Context<Self>) {
        let (consumed, signal) = self.execute_vim_command_with_signal(VimCommand::Cancel, cx);
        if signal == VimSignal::LeaveComposer {
            self.invoke_vim_leave(window, cx);
        }
        if consumed {
            cx.stop_propagation();
        }
    }

    fn application_escape(
        &mut self,
        _: &application_vim::Escape,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(handler) = self.on_application_escape.take() else {
            return;
        };
        handler(window, cx);
        self.on_application_escape = Some(handler);
        cx.stop_propagation();
    }

    /// Offset on the row above (-1) or below (+1) the cursor, keeping the
    /// horizontal position. None when there is no such row.
    fn vertical_neighbor(
        &self,
        direction: i32,
        goal_x: Option<Pixels>,
    ) -> Option<(usize, CursorAffinity)> {
        let layout = self.last_layout.as_ref()?;
        if !self.layout_is_current(layout) {
            return None;
        }
        let cursor = self.to_display_offset(self.cursor_offset());
        let position = layout.position_for_index_with_affinity(cursor, self.cursor_affinity)?;
        let target_y =
            position.y + layout.line_height * (direction as f32) + layout.line_height / 2.;
        if target_y < px(0.) || target_y > layout.height() {
            return None;
        }
        let target = point(goal_x.unwrap_or(position.x), target_y);
        let display = layout.closest_index_for_position(target);
        let affinity = layout.affinity_for_position(display, target);
        Some((self.content_offset_for_display(display), affinity))
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.popup.close(cx);
        let index = self.index_for_mouse_position(event.position);
        let affinity = self.affinity_for_mouse_position(index, event.position);
        if self
            .vim_mode()
            .is_some_and(|mode| mode != VimMode::Disabled)
        {
            self.is_selecting = false;
            self.finish_vim_lifecycle(LifecycleEvent::MouseCaretMove { offset: index }, cx);
            if self.standard_editing() {
                self.cursor_affinity = affinity;
            }
            return;
        }
        match event.click_count {
            // A drag only follows a single click; jitter after a double
            // click must not collapse the word selection.
            2 => {
                self.is_selecting = false;
                self.select_word_at(index, cx);
            }
            count if count >= 3 => {
                self.is_selecting = false;
                self.move_to(0, cx);
                self.select_to(self.content.len(), cx);
            }
            _ => {
                self.is_selecting = true;
                if event.modifiers.shift {
                    self.select_to(index, cx);
                } else {
                    self.move_to(index, cx)
                }
                self.cursor_affinity = affinity;
            }
        }
    }

    /// Select the word (or run of spaces) that contains byte `index`.
    fn select_word_at(&mut self, index: usize, cx: &mut Context<Self>) {
        let index = index.min(self.content.len());
        let range = self
            .content
            .split_word_bound_indices()
            .map(|(start, word)| start..start + word.len())
            .find(|range| index < range.end)
            .or_else(|| {
                self.content
                    .split_word_bound_indices()
                    .next_back()
                    .map(|(start, word)| start..start + word.len())
            })
            .unwrap_or(0..0);
        self.move_to(range.start, cx);
        self.select_to(range.end, cx);
    }

    fn on_right_click(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_selecting = false;
        self.finish_vim_lifecycle(LifecycleEvent::PopupTakeover, cx);
        window.focus(&self.focus_handle, cx);
        let index = self.index_for_mouse_position(event.position);
        self.spell_menu = self
            .misspelled
            .iter()
            .find(|range| range.start <= index && index <= range.end)
            .cloned()
            .map(|range| {
                let suggestions = spell::suggestions(&self.content[range.clone()], 4);
                (range, suggestions)
            });
        self.popup.open(event.position, cx);
    }

    /// Right-click menu: spelling fixes for the word under the pointer,
    /// then cut, copy, paste, select all.
    fn render_context_menu(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let position = *self.popup.open_key()?;
        let has_selection = !self.selected_range.is_empty();
        let mut menu = Menu::new("text-input-menu", px(160.))
            .label("Edit")
            .application_vim(self.application_vim);
        if let Some((range, suggestions)) = &self.spell_menu {
            for (i, suggestion) in suggestions.iter().enumerate() {
                let range = range.clone();
                let replacement = suggestion.clone();
                menu = menu.item(MenuItem::new(
                    ElementId::NamedInteger("text-input-suggest".into(), i as u64),
                    suggestion.clone(),
                    move |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                        this.spell_menu = None;
                        this.apply_suggestion(range.clone(), &replacement, cx);
                    },
                ));
            }
            let range = range.clone();
            menu = menu
                .item(MenuItem::new(
                    "text-input-add-word",
                    "Add to dictionary",
                    move |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                        this.spell_menu = None;
                        if let Some(word) = this.content.get(range.clone()) {
                            spell::add_word(word);
                        }
                        this.refresh_spelling();
                        cx.notify();
                    },
                ))
                .separator();
        }
        let edit = |id: &'static str,
                    label: &'static str,
                    enabled: bool,
                    action: fn(&mut Self, &mut Window, &mut Context<Self>)| {
            MenuItem::new(
                id,
                label,
                move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                    this.spell_menu = None;
                    action(this, window, cx);
                    cx.notify();
                },
            )
            .enabled(enabled)
        };
        let menu = menu
            .item(edit(
                "text-input-cut",
                "Cut",
                has_selection,
                |this, window, cx| this.cut(&Cut, window, cx),
            ))
            .item(edit(
                "text-input-copy",
                "Copy",
                has_selection,
                |this, window, cx| this.copy(&Copy, window, cx),
            ))
            .item(edit(
                "text-input-paste",
                "Paste",
                true,
                |this, window, cx| this.paste(&Paste, window, cx),
            ))
            .item(edit(
                "text-input-select-all",
                "Select all",
                !self.content.is_empty(),
                |this, window, cx| this.select_all(&SelectAll, window, cx),
            ));
        Some(self.popup.render(menu, Placement::At(position), window, cx))
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            let index = self.index_for_mouse_position(event.position);
            let affinity = self.affinity_for_mouse_position(index, event.position);
            self.select_to(index, cx);
            self.cursor_affinity = affinity;
        }
    }

    /// Wheel-scroll a multi-line input whose text is taller than its box.
    /// The event is consumed only when there is something to scroll, so a
    /// wheel over a short input still reaches the surface behind it.
    fn on_scroll_wheel(
        &mut self,
        event: &gpui::ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(layout), Some(bounds)) = (self.last_layout.as_ref(), self.last_bounds.as_ref())
        else {
            return;
        };
        let max_scroll = layout.height() - bounds.size.height;
        if max_scroll <= px(0.) {
            return;
        }
        cx.stop_propagation();
        let delta = event.delta.pixel_delta(layout.line_height).y;
        let next = (self.scroll_y - delta).min(max_scroll).max(px(0.));
        if next != self.scroll_y {
            self.scroll_y = next;
            cx.notify();
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        if let Some(on_paste_image) = self.on_paste_image.take() {
            let image = item.entries().iter().find_map(|entry| match entry {
                ClipboardEntry::Image(image) => Some(image.clone()),
                // File-list entries are not pastable text or pixels.
                _ => None,
            });
            if let Some(image) = image {
                on_paste_image(image, window, cx);
                self.on_paste_image = Some(on_paste_image);
                return;
            }
            self.on_paste_image = Some(on_paste_image);
        }
        if let Some(text) = item.text() {
            let text = if self.multiline {
                text.replace("\r\n", "\n")
            } else {
                text.replace('\n', " ")
            };
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.vertical_goal_x = None;
        self.cursor_affinity = CursorAffinity::Upstream;
        let offset = snap_to_char_boundary(&self.content, offset);
        self.selected_range = offset..offset;
        self.keep_cursor_visible = true;
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    /// Byte offset into the shaped display string for a content byte offset.
    fn to_display_offset(&self, offset: usize) -> usize {
        if self.mask {
            self.content[..offset.min(self.content.len())]
                .chars()
                .count()
        } else {
            offset
        }
    }

    /// Content byte offset for a display-string byte offset (mask aware).
    fn content_offset_for_display(&self, display: usize) -> usize {
        if self.mask {
            let chars = self.content.char_indices().collect::<Vec<_>>();
            chars
                .get(display)
                .map(|(i, _)| *i)
                .unwrap_or(self.content.len())
        } else {
            display.min(self.content.len())
        }
    }

    fn display_text(&self) -> SharedString {
        if self.mask {
            "*".repeat(self.content.chars().count()).into()
        } else {
            self.content.clone()
        }
    }

    fn affinity_for_mouse_position(
        &self,
        index: usize,
        position: gpui::Point<Pixels>,
    ) -> CursorAffinity {
        if let (Some(layout), Some(bounds)) = (&self.last_layout, &self.last_bounds)
            && self.layout_is_current(layout)
        {
            return layout.affinity_for_position(
                self.to_display_offset(index),
                point(position.x - bounds.left(), position.y - bounds.top()),
            );
        }
        CursorAffinity::Upstream
    }

    fn index_for_mouse_position(&self, position: gpui::Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(layout)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() || !self.layout_is_current(layout) {
            // Below the box, or the layout is from a frame before the text
            // changed: its offsets no longer describe `content`.
            return self.content.len();
        }
        let local = point(position.x - bounds.left(), position.y - bounds.top());
        let display = layout.closest_index_for_position(local);
        self.content_offset_for_display(display)
    }

    /// Whether `layout` was shaped from the current content. A masked
    /// input shapes one `*` per char, so only the lengths can be compared.
    fn layout_is_current(&self, layout: &TextLayout) -> bool {
        if self.mask {
            layout.text.len() == self.content.chars().count()
        } else {
            layout.text == self.content
        }
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.vertical_goal_x = None;
        self.cursor_affinity = CursorAffinity::Upstream;
        let offset = snap_to_char_boundary(&self.content, offset);
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.keep_cursor_visible = true;
        cx.notify()
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    fn commit_vim_ime(&mut self, committed: &str, cx: &mut Context<Self>) -> bool {
        let Some(baseline) = self.vim_ime_baseline.take() else {
            return false;
        };
        self.content = baseline.content.into();
        self.marked_range = None;
        self.replace_range_with_kind(baseline.range, committed, InsertEditKind::ImeCommit, cx);
        true
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.vim_ime_baseline.is_some() {
            let committed = self
                .marked_range
                .as_ref()
                .and_then(|range| self.content.get(range.clone()))
                .unwrap_or_default()
                .to_owned();
            self.commit_vim_ime(&committed, cx);
        } else {
            self.marked_range = None;
        }
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.vim_ime_baseline.is_some() {
            self.commit_vim_ime(new_text, cx);
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let kind = if self.marked_range.is_some() {
            InsertEditKind::ImeCommit
        } else if range.is_empty() {
            InsertEditKind::Text
        } else {
            InsertEditKind::SelectionReplacement
        };
        self.replace_range_with_kind(range, new_text, kind, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.vertical_goal_x = None;
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        if self
            .vim_mode()
            .is_some_and(|mode| mode != VimMode::Disabled)
        {
            if self.vim_mode() != Some(VimMode::Insert) {
                return;
            }
            if self.vim_ime_baseline.is_none() {
                self.vim_ime_baseline = Some(ImeBaseline {
                    content: self.content.to_string(),
                    range: range.clone(),
                });
            }
            self.cursor_affinity = CursorAffinity::Upstream;
            self.content =
                (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                    .into();
            self.marked_range = (!new_text.is_empty())
                .then_some(range.start..range.start.saturating_add(new_text.len()));
            let len = self.content.len();
            self.selected_range = new_selected_range_utf16
                .as_ref()
                .map(|range_utf16| self.range_from_utf16(range_utf16))
                .map(|new_range| {
                    (new_range.start + range.start).min(len)..(new_range.end + range.start).min(len)
                })
                .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
            self.spell_menu = None;
            self.popup.close(cx);
            self.refresh_spelling();
            cx.notify();
            return;
        }

        // The whole composition is one undo step: record the text as it
        // was before the first marked update.
        if self.marked_range.is_none() {
            self.record_edit(false);
        }
        self.cursor_affinity = CursorAffinity::Upstream;
        self.last_edit = None;

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        // The IME reports the selection relative to the marked text.
        let len = self.content.len();
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| {
                (new_range.start + range.start).min(len)..(new_range.end + range.start).min(len)
            })
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.spell_menu = None;
        self.popup.close(cx);
        self.keep_cursor_visible = true;

        self.refresh_spelling();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let caret_affinity = if range.is_empty() && range.start == self.cursor_offset() {
            self.cursor_affinity
        } else {
            CursorAffinity::Upstream
        };
        let start = layout.position_for_index_with_affinity(
            self.to_display_offset(range.start),
            caret_affinity,
        )?;
        let end = layout
            .position_for_index_with_affinity(self.to_display_offset(range.end), caret_affinity)?;
        Some(Bounds::from_corners(
            point(bounds.left() + start.x, bounds.top() + start.y),
            point(
                bounds.left() + end.x,
                bounds.top() + end.y + layout.line_height,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let bounds = self.last_bounds?;
        let layout = self.last_layout.as_ref()?;
        if !self.layout_is_current(layout) {
            // Layout is from a previous frame; report no hit instead of
            // slicing with stale offsets.
            return None;
        }
        let local = gpui::point(point.x - bounds.left(), point.y - bounds.top());
        let display = layout.closest_index_for_position(local);
        Some(self.offset_to_utf16(self.content_offset_for_display(display)))
    }
}

/// `offset` clamped to `text` and moved back to the nearest char boundary,
/// so a cursor from a stale layout or a mouse hit can never split a
/// multi-byte character.
fn snap_to_char_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// Everything `shape_text` was given last time, and what it returned.
/// A cursor move or a blink must not re-shape the text: prepaint runs
/// on every notify, and shaping is the expensive part of it.
struct ShapeCache {
    text: SharedString,
    wrap_width: Option<Pixels>,
    font_size: Pixels,
    /// Runs carry the font, color, and underline ranges, so a theme
    /// switch or a new misspelling misses the cache as it should.
    runs: Vec<TextRun>,
    lines: Arc<Vec<WrappedLine>>,
}

impl ShapeCache {
    fn matches(
        &self,
        text: &SharedString,
        wrap_width: Option<Pixels>,
        font_size: Pixels,
        runs: &[TextRun],
    ) -> bool {
        self.wrap_width == wrap_width
            && self.font_size == font_size
            && self.text == *text
            && self.runs == runs
    }
}

/// Shaped paragraphs of one input plus the offsets needed to map between
/// byte indices and pixel positions across newlines and wrap rows.
struct TextLayout {
    lines: Arc<Vec<WrappedLine>>,
    line_height: Pixels,
    text: SharedString,
}

impl TextLayout {
    fn height(&self) -> Pixels {
        self.lines
            .iter()
            .map(|line| line.size(self.line_height).height)
            .fold(px(0.), |acc, h| acc + h)
            .max(self.line_height)
    }

    fn position_for_index(&self, index: usize) -> Option<gpui::Point<Pixels>> {
        let mut y = px(0.);
        let mut start = 0;
        for line in self.lines.iter() {
            let end = start + line.len();
            if index <= end {
                let local = line.position_for_index(index - start, self.line_height)?;
                return Some(point(local.x, local.y + y));
            }
            y += line.size(self.line_height).height;
            start = end + 1;
        }
        Some(point(px(0.), y))
    }

    fn position_for_index_with_affinity(
        &self,
        index: usize,
        affinity: CursorAffinity,
    ) -> Option<gpui::Point<Pixels>> {
        let mut position = self.position_for_index(index)?;
        if affinity == CursorAffinity::Downstream
            && index > 0
            && self.text.as_bytes().get(index - 1) != Some(&b'\n')
            && self.visual_line_range(index, affinity)?.start == index
        {
            position = point(px(0.), position.y + self.line_height);
        }
        Some(position)
    }

    fn visual_line_range(&self, index: usize, affinity: CursorAffinity) -> Option<Range<usize>> {
        let mut start = 0;
        for line in self.lines.iter() {
            let end = start + line.len();
            if index <= end {
                let mut row_start = start;
                for row_end in line
                    .wrap_boundaries()
                    .iter()
                    .map(|boundary| {
                        start + line.runs()[boundary.run_ix].glyphs[boundary.glyph_ix].index
                    })
                    .chain([end])
                {
                    if index < row_end
                        || (index == row_end
                            && (row_end == end || affinity == CursorAffinity::Upstream))
                    {
                        return Some(row_start..row_end);
                    }
                    row_start = row_end;
                }
            }
            start = end + 1;
        }
        None
    }

    fn affinity_for_position(&self, index: usize, position: gpui::Point<Pixels>) -> CursorAffinity {
        match self.position_for_index_with_affinity(index, CursorAffinity::Downstream) {
            Some(downstream) if position.y >= downstream.y => CursorAffinity::Downstream,
            _ => CursorAffinity::Upstream,
        }
    }

    fn closest_index_for_position(&self, position: gpui::Point<Pixels>) -> usize {
        let mut y = px(0.);
        let mut start = 0;
        let last = self.lines.len().saturating_sub(1);
        for (i, line) in self.lines.iter().enumerate() {
            let height = line.size(self.line_height).height;
            if position.y < y + height || i == last {
                let local = point(position.x, (position.y - y).max(px(0.)));
                let index = match line.closest_index_for_position(local, self.line_height) {
                    Ok(index) | Err(index) => index,
                };
                return start + index.min(line.len());
            }
            y += height;
            start += line.len() + 1;
        }
        self.text.len()
    }
}

/// Split one base run into pieces so each underlined range gets its own
/// run. Ranges must not overlap; they are sorted here.
fn split_runs(
    base: &TextRun,
    len: usize,
    mut underlined: Vec<(Range<usize>, UnderlineStyle)>,
) -> Vec<TextRun> {
    if underlined.is_empty() {
        return vec![base.clone()];
    }
    underlined.sort_by_key(|(range, _)| range.start);
    let mut runs = Vec::with_capacity(underlined.len() * 2 + 1);
    let mut at = 0;
    for (range, underline) in underlined {
        let start = range.start.max(at).min(len);
        let end = range.end.min(len);
        if end <= start {
            continue;
        }
        if start > at {
            runs.push(TextRun {
                len: start - at,
                ..base.clone()
            });
        }
        runs.push(TextRun {
            len: end - start,
            underline: Some(underline),
            ..base.clone()
        });
        at = end;
    }
    if at < len {
        runs.push(TextRun {
            len: len - at,
            ..base.clone()
        });
    }
    runs
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    layout: Option<TextLayout>,
    scroll_x: Pixels,
    scroll_y: Pixels,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let input = self.input.read(cx);
        if !input.multiline {
            style.size.height = window.line_height().into();
            return (window.request_layout(style, [], cx), ());
        }
        if input.fill_height {
            style.size.height = relative(1.).into();
            style.min_size.height = window.line_height().into();
            return (window.request_layout(style, [], cx), ());
        }
        // Grow with the wrapped content, one to `max_lines` rows.
        let entity = self.input.clone();
        let layout = window.request_measured_layout(style, move |known, available, window, cx| {
            let input = entity.read(cx);
            let text = if input.content.is_empty() {
                input.placeholder.clone()
            } else {
                input.display_text()
            };
            let line_height = window.line_height();
            let width = known.width.or(match available.width {
                gpui::AvailableSpace::Definite(width) => Some(width),
                _ => None,
            });
            let cache_width = width.unwrap_or(px(0.));
            let style = window.text_style();
            let font_size = style.font_size.to_pixels(window.rem_size());
            let font = style.font();
            if let Some((
                cached_text,
                cached_width,
                cached_font,
                cached_font_size,
                cached_line_height,
                rows,
            )) = input.measure_cache.as_ref()
                && *cached_text == text
                && *cached_width == cache_width
                && *cached_font == font
                && *cached_font_size == font_size
                && *cached_line_height == line_height
            {
                return size(cache_width, line_height * *rows as f32);
            }
            let run = TextRun {
                len: text.len(),
                font: font.clone(),
                color: style.color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let rows = window
                .text_system()
                .shape_text(text.clone(), font_size, &[run], width, None)
                .map(|lines| {
                    lines
                        .iter()
                        .map(|line| line.wrap_boundaries().len() + 1)
                        .sum::<usize>()
                })
                .unwrap_or(1)
                .clamp(1, input.max_lines);
            entity.update(cx, |input, _| {
                input.measure_cache = Some((text, cache_width, font, font_size, line_height, rows));
            });
            size(cache_width, line_height * rows as f32)
        });
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        {
            // Text set before the dictionary loaded has no ranges yet;
            // one atomic compare per frame, a re-check only on change.
            let input = self.input.read(cx);
            if input.spell_check && input.spell_generation != spell::generation() {
                self.input.update(cx, |input, _| input.refresh_spelling());
            }
        }
        let input = self.input.read(cx);
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let cursor_affinity = input.cursor_affinity;
        let vim_mode = input.vim_mode();
        let vim_head = input.vim.as_ref().map(VimState::cursor);
        let mask = input.mask;
        let style = window.text_style();

        // Typed text takes the theme's primary color, not whatever the
        // surrounding element happens to set: an input inside a caption
        // row or an unstyled container still reads in both themes.
        let (display_text, text_color) = if content.is_empty() {
            (input.placeholder.clone(), theme::placeholder())
        } else {
            (input.display_text(), rgb(theme::text_primary()).into())
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        // Underlines: the IME marked range (plain) and misspelled words
        // (wavy red), except the word the cursor is still inside.
        let mut underlined: Vec<(Range<usize>, UnderlineStyle)> = Vec::new();
        if let Some(marked_range) = input.marked_range.as_ref() {
            underlined.push((
                marked_range.clone(),
                UnderlineStyle {
                    color: Some(run.color),
                    thickness: px(1.0),
                    wavy: false,
                },
            ));
        }
        if !content.is_empty() && !mask {
            let cursor_in = |range: &Range<usize>| {
                selected_range.is_empty() && range.start <= cursor && cursor <= range.end
            };
            underlined.extend(
                input
                    .misspelled
                    .iter()
                    .filter(|range| range.end <= content.len() && !cursor_in(range))
                    .map(|range| {
                        (
                            range.clone(),
                            UnderlineStyle {
                                color: Some(rgb(theme::spell_error()).into()),
                                thickness: px(1.0),
                                wavy: true,
                            },
                        )
                    }),
            );
        }
        let runs = split_runs(&run, display_text.len(), underlined);

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let multiline = input.multiline;
        let last_scroll_x = input.scroll_x;
        let last_scroll_y = input.scroll_y;
        let keep_cursor_visible = input.keep_cursor_visible;
        let wrap_width = multiline.then_some(bounds.size.width);
        // Reuse last frame's shaping when its inputs are unchanged. Lines
        // are Arc-backed, so the clone is a refcount per line.
        let cached = input
            .shape_cache
            .as_ref()
            .filter(|cache| cache.matches(&display_text, wrap_width, font_size, &runs))
            .map(|cache| cache.lines.clone());
        let lines = match cached {
            Some(lines) => lines,
            None => {
                let lines = Arc::new(
                    window
                        .text_system()
                        .shape_text(display_text.clone(), font_size, &runs, wrap_width, None)
                        .map(|lines| lines.into_vec())
                        .unwrap_or_default(),
                );
                self.input.update(cx, |input, _| {
                    input.shape_cache = Some(ShapeCache {
                        text: display_text.clone(),
                        wrap_width,
                        font_size,
                        runs,
                        lines: lines.clone(),
                    });
                });
                lines
            }
        };
        let layout = TextLayout {
            lines,
            line_height,
            text: display_text,
        };

        let display_cursor = if mask {
            content[..cursor.min(content.len())].chars().count()
        } else {
            cursor
        };
        // Single-line inputs do not wrap. Scroll so the cursor stays in
        // view and clip the paint to the box.
        let scroll_x = if multiline {
            px(0.)
        } else {
            let cursor_x = layout
                .position_for_index_with_affinity(display_cursor, cursor_affinity)
                .map(|p| p.x)
                .unwrap_or_default();
            let text_width = layout
                .lines
                .iter()
                .map(|line| line.size(line_height).width)
                .fold(px(0.), |acc, w| acc.max(w));
            let width = bounds.size.width;
            let mut sx = last_scroll_x;
            if cursor_x - sx > width - px(2.) {
                sx = cursor_x - width + px(2.);
            }
            if cursor_x - sx < px(0.) {
                sx = cursor_x;
            }
            sx.min((text_width - width + px(2.)).max(px(0.)))
                .max(px(0.))
        };
        // Multi-line inputs wrap instead. Scroll vertically: follow the
        // cursor after an edit or a cursor move, keep a wheel scroll put,
        // and clamp when the content shrinks.
        let scroll_y = if multiline {
            let viewport = bounds.size.height;
            let max_scroll = (layout.height() - viewport).max(px(0.));
            let mut sy = last_scroll_y.min(max_scroll).max(px(0.));
            if keep_cursor_visible {
                let cursor_y = layout
                    .position_for_index_with_affinity(display_cursor, cursor_affinity)
                    .map(|p| p.y)
                    .unwrap_or_default();
                if cursor_y + line_height - sy > viewport {
                    sy = cursor_y + line_height - viewport;
                }
                if cursor_y < sy {
                    sy = cursor_y;
                }
                sy = sy.min(max_scroll).max(px(0.));
            }
            sy
        } else {
            px(0.)
        };
        let text_bounds = Bounds::new(
            point(bounds.left() - scroll_x, bounds.top() - scroll_y),
            bounds.size,
        );
        let (selection, cursor) = if selected_range.is_empty() {
            let cursor_pos = layout
                .position_for_index_with_affinity(display_cursor, cursor_affinity)
                .unwrap_or_default();
            let modal_block = matches!(vim_mode, Some(VimMode::Normal | VimMode::Visual));
            let cursor_width = if modal_block {
                let next = content
                    .grapheme_indices(true)
                    .find_map(|(offset, _)| (offset > cursor).then_some(offset))
                    .unwrap_or(content.len());
                let display_next = if mask {
                    content[..next.min(content.len())].chars().count()
                } else {
                    next
                };
                let next_pos = layout
                    .position_for_index(display_next)
                    .unwrap_or(cursor_pos);
                if next_pos.y == cursor_pos.y {
                    (next_pos.x - cursor_pos.x).max(px(7.))
                } else {
                    px(7.)
                }
            } else {
                px(2.)
            };
            (
                Vec::new(),
                Some(fill(
                    Bounds::new(
                        point(
                            text_bounds.left() + cursor_pos.x,
                            text_bounds.top() + cursor_pos.y,
                        ),
                        size(cursor_width, line_height),
                    ),
                    if modal_block {
                        theme::text_selection()
                    } else {
                        rgb(theme::text_cursor())
                    },
                )),
            )
        } else {
            let start = if mask {
                content[..selected_range.start.min(content.len())]
                    .chars()
                    .count()
            } else {
                selected_range.start
            };
            let end = if mask {
                content[..selected_range.end.min(content.len())]
                    .chars()
                    .count()
            } else {
                selected_range.end
            };
            let start_pos = layout
                .position_for_index_with_affinity(start, CursorAffinity::Downstream)
                .unwrap_or_default();
            let end_pos = layout.position_for_index(end).unwrap_or_default();
            let color = theme::text_selection();
            let mut quads = Vec::new();
            if start_pos.y == end_pos.y {
                quads.push(fill(
                    Bounds::from_corners(
                        point(
                            text_bounds.left() + start_pos.x,
                            text_bounds.top() + start_pos.y,
                        ),
                        point(
                            text_bounds.left() + end_pos.x,
                            text_bounds.top() + start_pos.y + line_height,
                        ),
                    ),
                    color,
                ));
            } else {
                // First row to the right edge, full middle rows, then the
                // last row from the left edge.
                quads.push(fill(
                    Bounds::from_corners(
                        point(
                            text_bounds.left() + start_pos.x,
                            text_bounds.top() + start_pos.y,
                        ),
                        point(
                            text_bounds.right(),
                            text_bounds.top() + start_pos.y + line_height,
                        ),
                    ),
                    color,
                ));
                if start_pos.y + line_height < end_pos.y {
                    quads.push(fill(
                        Bounds::from_corners(
                            point(
                                text_bounds.left(),
                                text_bounds.top() + start_pos.y + line_height,
                            ),
                            point(text_bounds.right(), text_bounds.top() + end_pos.y),
                        ),
                        color,
                    ));
                }
                quads.push(fill(
                    Bounds::from_corners(
                        point(text_bounds.left(), text_bounds.top() + end_pos.y),
                        point(
                            text_bounds.left() + end_pos.x,
                            text_bounds.top() + end_pos.y + line_height,
                        ),
                    ),
                    color,
                ));
            }
            let visual_head = if vim_mode == Some(VimMode::Visual) {
                let head = vim_head.unwrap_or(cursor);
                let head = if mask {
                    content[..head.min(content.len())].chars().count()
                } else {
                    head
                };
                let head_pos = layout.position_for_index(head).unwrap_or_default();
                Some(fill(
                    Bounds::new(
                        point(
                            text_bounds.left() + head_pos.x,
                            text_bounds.top() + head_pos.y,
                        ),
                        size(px(2.), line_height),
                    ),
                    rgb(theme::text_cursor()),
                ))
            } else {
                None
            };
            (quads, visual_head)
        };
        if !multiline && last_scroll_x != scroll_x {
            self.input.update(cx, |input, _| input.scroll_x = scroll_x);
        }
        if multiline && (last_scroll_y != scroll_y || keep_cursor_visible) {
            self.input.update(cx, |input, _| {
                input.scroll_y = scroll_y;
                input.keep_cursor_visible = false;
            });
        }
        PrepaintState {
            layout: Some(layout),
            scroll_x,
            scroll_y,
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let layout = prepaint.layout.take().unwrap();
        let text_bounds = Bounds::new(
            point(
                bounds.left() - prepaint.scroll_x,
                bounds.top() - prepaint.scroll_y,
            ),
            bounds.size,
        );
        let focused = focus_handle.is_focused(window);
        let selection = std::mem::take(&mut prepaint.selection);
        let cursor = prepaint.cursor.take();
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for quad in selection {
                window.paint_quad(quad)
            }
            let mut origin = text_bounds.origin;
            for line in layout.lines.iter() {
                let height = line.size(layout.line_height).height;
                // A scrolled input shapes every line but paints only the
                // rows inside the box.
                if origin.y + height >= bounds.top() && origin.y <= bounds.bottom() {
                    line.paint(
                        origin,
                        layout.line_height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    )
                    .ok();
                }
                origin.y += height;
            }
            if focused && let Some(cursor) = cursor {
                window.paint_quad(cursor);
            }
        });

        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(layout);
            input.last_bounds = Some(text_bounds);
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.popup.sync_focus(window, cx);
        if self.vim.is_some() && self.focus_out_subscription.is_none() {
            let focus = self.focus_handle.clone();
            self.focus_out_subscription =
                Some(
                    cx.on_focus_out(&focus, window, |input, _event, _window, cx| {
                        if input.popup.open_key().is_some() {
                            input.focus_in_menu = true;
                            return;
                        }
                        input.finish_vim_lifecycle(LifecycleEvent::TaskOrScreenSwitch, cx);
                    }),
                );
        }
        if self.focus_in_menu && self.popup.open_key().is_none() {
            self.focus_in_menu = false;
            if !self.focus_handle.is_focused(window) {
                self.finish_vim_lifecycle(LifecycleEvent::TaskOrScreenSwitch, cx);
            }
        }
        let key_context = self.key_context();
        let input = div()
            .id(("text-input", cx.entity_id().as_u64()))
            .role(if self.multiline {
                gpui::Role::MultilineTextInput
            } else {
                gpui::Role::TextInput
            })
            .aria_label(self.placeholder.clone())
            .aria_placeholder(self.placeholder.clone())
            .flex()
            .key_context(key_context)
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam);
        let field = vim_actions::attach_actions(input, cx)
            .on_action(cx.listener(Self::application_escape))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_line_start))
            .on_action(cx.listener(Self::select_line_end))
            .on_action(cx.listener(Self::delete_to_line_start))
            .on_action(cx.listener(Self::delete_to_line_end))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::delete_word_backward))
            .on_action(cx.listener(Self::delete_word_forward))
            .on_action(cx.listener(Self::paragraph_start))
            .on_action(cx.listener(Self::paragraph_end))
            .on_action(cx.listener(Self::select_paragraph_start))
            .on_action(cx.listener(Self::select_paragraph_end))
            .on_action(cx.listener(Self::document_start))
            .on_action(cx.listener(Self::document_end))
            .on_action(cx.listener(Self::select_document_start))
            .on_action(cx.listener(Self::select_document_end))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                let modal = matches!(this.vim_mode(), Some(VimMode::Normal | VimMode::Visual));
                if !modal && let Some(on_key) = this.on_key.take() {
                    let consumed = on_key(event, &this.content.clone(), window, cx);
                    this.on_key = Some(on_key);
                    if consumed {
                        cx.stop_propagation();
                        return;
                    }
                }
                let no_modifiers = !event.keystroke.modifiers.control
                    && !event.keystroke.modifiers.alt
                    && !event.keystroke.modifiers.platform;
                if modal {
                    let visual_enter =
                        this.vim_mode() == Some(VimMode::Visual) && event.keystroke.key == "enter";
                    let forbidden_newline =
                        event.keystroke.key == "enter" && event.keystroke.modifiers.shift;
                    let unmatched_printable = no_modifiers
                        && !event.keystroke.key.eq_ignore_ascii_case("enter")
                        && !event.keystroke.key.eq_ignore_ascii_case("tab")
                        && event
                            .keystroke
                            .key_char
                            .as_deref()
                            .is_some_and(|text| !text.is_empty());
                    if visual_enter
                        || forbidden_newline
                        || event.keystroke.key.eq_ignore_ascii_case("tab")
                        || unmatched_printable
                    {
                        this.execute_vim_command(VimCommand::Invalid, cx);
                        cx.stop_propagation();
                        return;
                    }
                }
                if event.keystroke.key.eq_ignore_ascii_case("tab")
                    && no_modifiers
                    && this.marked_range.is_none()
                {
                    if event.keystroke.modifiers.shift {
                        window.focus_prev(cx);
                    } else {
                        window.focus_next(cx);
                    }
                    cx.stop_propagation();
                }
                let is_enter = event.keystroke.key == "enter" && this.marked_range.is_none();
                if is_enter && no_modifiers && event.keystroke.modifiers.shift && this.multiline {
                    this.replace_text_in_range(None, "\n", window, cx);
                    cx.stop_propagation();
                    return;
                }
                let plain_enter = is_enter && no_modifiers && !event.keystroke.modifiers.shift;
                if plain_enter {
                    this.enter_pressed(window, cx);
                    // Stop the platform text input from also inserting a
                    // newline for the unhandled Return keystroke.
                    cx.stop_propagation();
                }
            }))
            .when_some(self.tab_index, |el, index| el.tab_index(index))
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(gpui::MouseButton::Right, cx.listener(Self::on_right_click))
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(gpui::MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .when(self.multiline, |el| {
                el.on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            })
            .w_full()
            .when(self.multiline && self.fill_height, |el| el.h_full())
            .child(TextElement { input: cx.entity() });
        // The right-click menu sits beside the field's key context, not in
        // it: while the menu has focus, keys it does not bind must not edit
        // the text or reach the field's Vim.
        let menu = self.render_context_menu(window, cx);
        div()
            .w_full()
            .when(self.multiline && self.fill_height, |el| el.h_full())
            .child(field)
            .children(menu)
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    struct InputHost {
        input: Entity<TextInput>,
    }

    impl Render for InputHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().w(px(500.)).h(px(240.)).child(self.input.clone())
        }
    }

    struct ApplicationInputHost {
        input: Entity<TextInput>,
    }

    impl Render for ApplicationInputHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .key_context("Settings ApplicationVim")
                .w(px(500.))
                .h(px(240.))
                .child(self.input.clone())
        }
    }

    #[test]
    fn snapping_clamps_and_respects_char_boundaries() {
        assert_eq!(snap_to_char_boundary("abc", 10), 3);
        assert_eq!(snap_to_char_boundary("abc", 1), 1);
        // "é" is two bytes; offset 1 is inside it.
        assert_eq!(snap_to_char_boundary("é", 1), 0);
        assert_eq!(snap_to_char_boundary("aé", 2), 1);
        assert_eq!(snap_to_char_boundary("", 5), 0);
    }

    #[test]
    fn shape_cache_hits_only_on_identical_inputs() {
        let run = |len: usize, underline: Option<UnderlineStyle>| TextRun {
            len,
            font: gpui::font("Sans"),
            color: gpui::black(),
            background_color: None,
            underline,
            strikethrough: None,
        };
        let text = SharedString::from("hello");
        let cache = ShapeCache {
            text: text.clone(),
            wrap_width: Some(px(200.)),
            font_size: px(14.),
            runs: vec![run(5, None)],
            lines: Arc::new(Vec::new()),
        };
        assert!(cache.matches(&text, Some(px(200.)), px(14.), &[run(5, None)]));
        assert!(!cache.matches(&"hellp".into(), Some(px(200.)), px(14.), &[run(5, None)]));
        assert!(!cache.matches(&text, Some(px(100.)), px(14.), &[run(5, None)]));
        assert!(!cache.matches(&text, None, px(14.), &[run(5, None)]));
        assert!(!cache.matches(&text, Some(px(200.)), px(16.), &[run(5, None)]));
        let wavy = UnderlineStyle {
            color: None,
            thickness: px(1.),
            wavy: true,
        };
        assert!(!cache.matches(
            &text,
            Some(px(200.)),
            px(14.),
            &[run(2, None), run(3, Some(wavy))]
        ));
    }

    #[gpui::test]
    fn test_typing_undoes_as_one_step(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| TextInput::new("", cx));
        input.update(cx, |input, cx| {
            for (offset, letter) in ["h", "i"].iter().enumerate() {
                input.replace_range(offset..offset, letter, cx);
            }
            assert_eq!(input.text(), "hi");
            input.undo_last(cx);
            assert_eq!(input.text(), "");
            input.redo_last(cx);
            assert_eq!(input.text(), "hi");
            assert_eq!(input.selected_range, 2..2);
        });
    }

    #[gpui::test]
    fn test_a_new_run_starts_its_own_undo_step(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| TextInput::new("", cx));
        input.update(cx, |input, cx| {
            input.replace_range(0..0, "a", cx);
            // Typing away from the last cursor is a second step.
            input.replace_range(0..0, "b", cx);
            assert_eq!(input.text(), "ba");
            input.undo_last(cx);
            assert_eq!(input.text(), "a");
            input.undo_last(cx);
            assert_eq!(input.text(), "");
        });
    }

    #[gpui::test]
    fn test_delete_and_paste_are_their_own_steps(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| TextInput::new("", cx));
        input.update(cx, |input, cx| {
            input.set_text("word", cx);
            // A pasted run of characters never folds into typing.
            input.replace_range(4..4, " and more", cx);
            input.replace_range(3..13, "", cx);
            assert_eq!(input.text(), "wor");
            input.undo_last(cx);
            assert_eq!(input.text(), "word and more");
            input.undo_last(cx);
            assert_eq!(input.text(), "word");
            input.undo_last(cx);
            assert_eq!(input.text(), "");
        });
    }

    #[gpui::test]
    fn test_an_edit_drops_the_redo_history(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| TextInput::new("", cx));
        input.update(cx, |input, cx| {
            input.set_text("one", cx);
            input.undo_last(cx);
            input.set_text("two", cx);
            input.redo_last(cx);
            assert_eq!(input.text(), "two");
        });
    }

    #[gpui::test]
    fn test_clear_forgets_the_history(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| TextInput::new("", cx));
        input.update(cx, |input, cx| {
            input.set_text("sent message", cx);
            input.clear(cx);
            input.undo_last(cx);
            assert_eq!(input.text(), "");
        });
    }

    #[gpui::test]
    fn test_multiline_text_taller_than_the_box_scrolls(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        struct Host {
            input: Entity<TextInput>,
        }
        impl Render for Host {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                // Fixed size: the harness does not apply the window bounds
                // to the root view.
                div()
                    .w(px(300.))
                    .h(px(400.))
                    .flex()
                    .flex_col()
                    .child(self.input.clone())
            }
        }

        let input = cx.new(|cx| TextInput::new("", cx).multiline(8));
        // Enough text to wrap far past eight rows in a 300 px box.
        input.update(cx, |input, cx| input.set_text(&"word ".repeat(400), cx));
        let (_host, cx) = cx.add_window_view(|_window, _cx| Host {
            input: input.clone(),
        });
        cx.simulate_resize(gpui::size(px(300.), px(400.)));

        // The cursor sits at the end of the text, so the first draw must
        // scroll the view down to keep it visible.
        let scrolled = cx.update(|_window, app| input.read(app).scroll_y);
        assert!(
            scrolled > px(0.),
            "the view must follow the cursor to the bottom, got {scrolled:?}"
        );

        // A wheel over the input moves the view up and stays put: prepaint
        // must not snap back to the cursor without a new edit.
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: point(px(150.), px(10.)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(50.))),
            ..Default::default()
        });
        let wheeled = cx.update(|_window, app| input.read(app).scroll_y);
        assert!(
            wheeled < scrolled,
            "the wheel must scroll the text up, got {wheeled:?} from {scrolled:?}"
        );

        // Typing pulls the cursor back into view; in test mode the dirty
        // window redraws at the end of the update.
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.replace_text_in_range(None, "!", window, cx)
            })
        });
        let followed = cx.update(|_window, app| input.read(app).scroll_y);
        assert!(
            followed > wheeled,
            "an edit must scroll the cursor back into view, got {followed:?}"
        );
    }

    #[gpui::test]
    fn test_cursor_moves_never_leave_the_content(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| TextInput::new("", cx));
        input.update(cx, |input, cx| {
            input.set_text("héllo", cx);
            input.move_to(100, cx);
            assert_eq!(input.selected_range, 6..6);
            input.move_to(0, cx);
            input.select_to(2, cx);
            assert_eq!(input.selected_range, 0..1);
            input.select_to(99, cx);
            assert_eq!(input.selected_range, 0..6);
        });
    }

    /// The right-click menu takes the keyboard: disabled rows are skipped,
    /// Enter picks, and the field gets its focus back.
    #[gpui::test]
    fn the_right_click_menu_walks_picks_and_returns_focus(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| {
            crate::desktop::register_key_bindings(cx);
            let mut input = TextInput::new("", cx);
            input.set_text("hello world", cx);
            input
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| InputHost {
            input: input.clone(),
        });
        cx.simulate_resize(gpui::size(px(500.), px(240.)));
        cx.update(|window, _| window.activate_window());
        let focus = cx.update(|_window, app| input.read(app).focus_handle(app));
        cx.update(|window, app| window.focus(&focus, app));
        cx.run_until_parked();

        let at = gpui::point(px(40.), px(10.));
        cx.simulate_mouse_down(at, gpui::MouseButton::Right, gpui::Modifiers::default());
        cx.simulate_mouse_up(at, gpui::MouseButton::Right, gpui::Modifiers::default());
        assert_eq!(
            input.update(cx, |input, _| input.popup.open_key().copied()),
            Some(at)
        );
        // Keys the menu does not bind do not edit the text behind it.
        cx.simulate_keystrokes("backspace");
        assert_eq!(input.update(cx, |input, _| input.text()), "hello world");
        // Nothing is selected, so Cut and Copy are disabled: the first Down
        // lands on Paste, the next on Select all.
        cx.simulate_keystrokes("down down enter");
        input.update(cx, |input, _| {
            assert_eq!(input.popup.open_key(), None);
            assert_eq!(input.selected_range, 0..input.text_ref().len());
        });
        assert_eq!(cx.update(|window, app| window.focused(app)), Some(focus));
    }

    #[gpui::test]
    fn ordinary_input_keeps_standard_editing_with_vim_bindings_registered(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| {
            crate::desktop::register_key_bindings(cx);
            TextInput::new("", cx)
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| InputHost {
            input: input.clone(),
        });
        cx.simulate_resize(gpui::size(px(500.), px(240.)));
        let focus = cx.update(|_window, app| input.read(app).focus_handle(app));
        cx.update(|window, app| window.focus(&focus, app));

        cx.simulate_input("hi");
        cx.simulate_keystrokes("left backspace");

        cx.update(|_window, app| {
            let input = input.read(app);
            assert_eq!(input.text_ref(), "i");
            assert_eq!(input.vim_mode(), None);
            assert_eq!(
                input.key_context(),
                "TextInput input_role = other editor_vim_mode = disabled"
            );
        });
    }

    #[gpui::test]
    fn vim_off_task_switch_preserves_standard_undo_history(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| TextInput::new("", cx).composer_vim(false));

        input.update(cx, |input, cx| {
            input.replace_range(0..0, "draft", cx);
            input.reset_vim_context(cx);
            input.undo_last(cx);

            assert_eq!(input.text_ref(), "");
        });
    }

    #[gpui::test]
    fn non_vim_backspace_deletes_the_active_marked_range(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| {
            let mut input = TextInput::new("", cx);
            input.set_text("a", cx);
            input
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| InputHost {
            input: input.clone(),
        });

        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.replace_and_mark_text_in_range(None, "xy", None, window, cx);
                assert_eq!(input.marked_range, Some(1..3));

                input.backspace(&Backspace, window, cx);

                assert_eq!(input.text_ref(), "a");
                assert_eq!(input.marked_range, None);
            });
        });
    }

    #[gpui::test]
    fn non_vim_delete_deletes_the_active_marked_range(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| {
            let mut input = TextInput::new("", cx);
            input.set_text("a", cx);
            input
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| InputHost {
            input: input.clone(),
        });

        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.replace_and_mark_text_in_range(None, "xy", None, window, cx);
                assert_eq!(input.marked_range, Some(1..3));

                input.delete(&Delete, window, cx);

                assert_eq!(input.text_ref(), "a");
                assert_eq!(input.marked_range, None);
            });
        });
    }

    #[gpui::test]
    fn disabled_composer_vim_never_overwrites_the_ordinary_selection(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| {
            let mut input = TextInput::new("", cx).composer_vim(false);
            input.set_text("hello world", cx);
            input.selected_range = 6..11;
            input
        });

        input.update(cx, |input, cx| {
            assert_eq!(input.vim_mode(), None);
            assert_eq!(
                input.key_context(),
                "TextInput input_role = composer editor_vim_mode = disabled"
            );

            input.finish_vim_lifecycle(LifecycleEvent::PopupTakeover, cx);
            assert_eq!(input.selected_range, 6..11);
            input.set_vim_enabled(false, cx);
            assert_eq!(input.selected_range, 6..11);
            input.reset_vim_context(cx);
            assert_eq!(input.selected_range, 6..11);
        });
    }

    #[gpui::test]
    fn ordinary_input_escape_reaches_its_application_focus_handoff(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let leaves = Rc::new(Cell::new(0));
        let leaves_for_handler = leaves.clone();
        let input = cx.new(|cx| {
            crate::desktop::register_key_bindings(cx);
            TextInput::new("", cx)
                .application_vim(true)
                .on_application_escape(move |_window, _cx| {
                    leaves_for_handler.set(leaves_for_handler.get() + 1)
                })
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| ApplicationInputHost {
            input: input.clone(),
        });
        let focus = cx.update(|_window, app| input.read(app).focus_handle(app));
        cx.update(|window, app| window.focus(&focus, app));

        cx.simulate_keystrokes("escape");
        assert_eq!(leaves.get(), 1);
    }

    #[gpui::test]
    fn composer_vim_bindings_dispatch_and_insert_uses_the_input_handler(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| {
            crate::desktop::register_key_bindings(cx);
            let mut input = TextInput::new("", cx).composer_vim(true).multiline(8);
            input.set_text("one two", cx);
            input
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| InputHost {
            input: input.clone(),
        });
        cx.simulate_resize(gpui::size(px(500.), px(240.)));
        let focus = cx.update(|_window, app| input.read(app).focus_handle(app));
        cx.update(|window, app| window.focus(&focus, app));

        // Direct text input is a no-op in Normal. The following edits arrive
        // through real, context-resolved GPUI bindings and the normal input
        // handler rather than by calling the engine in the test.
        cx.simulate_input("X");
        cx.simulate_keystrokes("0 d w");
        assert_eq!(cx.update(|_window, app| input.read(app).text()), "two");

        cx.simulate_keystrokes("i");
        assert_eq!(
            cx.update(|_window, app| input.read(app).vim_mode()),
            Some(VimMode::Insert)
        );
        cx.simulate_input("é🙂");
        cx.simulate_keystrokes("escape");
        cx.update(|_window, app| {
            let input = input.read(app);
            assert_eq!(input.text_ref(), "é🙂two");
            assert_eq!(input.vim_mode(), Some(VimMode::Normal));
        });

        // One Insert transaction is one undo/redo step.
        cx.simulate_keystrokes("u");
        assert_eq!(cx.update(|_window, app| input.read(app).text()), "two");
        cx.simulate_keystrokes("ctrl-r");
        assert_eq!(cx.update(|_window, app| input.read(app).text()), "é🙂two");
    }

    #[gpui::test]
    fn normal_backspace_moves_left_without_editing(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| {
            crate::desktop::register_key_bindings(cx);
            let mut input = TextInput::new("", cx).composer_vim(true);
            input.set_text("abc", cx);
            input
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| InputHost {
            input: input.clone(),
        });
        let focus = cx.update(|_window, app| input.read(app).focus_handle(app));
        cx.update(|window, app| window.focus(&focus, app));

        cx.simulate_keystrokes("backspace");
        cx.update(|_window, app| {
            let input = input.read(app);
            assert_eq!(input.text_ref(), "abc");
            assert_eq!(input.selected_range, 1..1);
        });
    }

    #[gpui::test]
    fn vim_spelling_refreshes_after_edits_and_history_but_not_motions(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| {
            let mut input = TextInput::new("", cx).composer_vim(true).spell_check();
            input.set_text("abc", cx);
            input
        });

        input.update(cx, |input, cx| {
            let stale = input.content.len() + 10..input.content.len() + 11;
            input.misspelled = vec![stale.clone()];
            input.execute_vim_command(VimCommand::Motion(Motion::Left), cx);
            assert_eq!(input.misspelled.as_slice(), std::slice::from_ref(&stale));

            input.execute_vim_command(VimCommand::DeleteChars, cx);
            assert_eq!(input.text_ref(), "ac");
            assert!(
                input
                    .misspelled
                    .iter()
                    .all(|range| range.end <= input.content.len())
            );

            input.misspelled = vec![stale];
            input.execute_vim_command(VimCommand::Undo, cx);
            assert_eq!(input.text_ref(), "abc");
            assert!(
                input
                    .misspelled
                    .iter()
                    .all(|range| range.end <= input.content.len())
            );
        });
    }

    #[gpui::test]
    fn composer_normal_escape_notifies_the_application_synchronously(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let leaves = Rc::new(Cell::new(0));
        let leaves_for_handler = leaves.clone();
        let input = cx.new(|cx| {
            crate::desktop::register_key_bindings(cx);
            TextInput::new("", cx)
                .composer_vim(true)
                .on_vim_leave(move |_window, _cx| {
                    leaves_for_handler.set(leaves_for_handler.get() + 1)
                })
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| InputHost {
            input: input.clone(),
        });
        let focus = cx.update(|_window, app| input.read(app).focus_handle(app));
        cx.update(|window, app| window.focus(&focus, app));

        cx.simulate_keystrokes("escape");
        assert_eq!(leaves.get(), 1);

        // Insert and Visual Escape are editor-local mode transitions. Only
        // Escape from settled Normal asks the owning application to leave.
        cx.simulate_keystrokes("i escape v escape");
        assert_eq!(leaves.get(), 1);
        cx.simulate_keystrokes("escape");
        assert_eq!(leaves.get(), 2);
    }

    #[gpui::test]
    fn composer_vim_preserves_maple_enter_behavior(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let submitted = Rc::new(RefCell::new(Vec::<String>::new()));
        let submitted_for_handler = submitted.clone();
        let input = cx.new(|cx| {
            crate::desktop::register_key_bindings(cx);
            let mut input = TextInput::new("", cx)
                .composer_vim(true)
                .multiline(8)
                .on_enter(move |text, _window, _cx| {
                    submitted_for_handler.borrow_mut().push(text);
                });
            input.set_text("draft", cx);
            input
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| InputHost {
            input: input.clone(),
        });
        cx.simulate_resize(gpui::size(px(500.), px(240.)));
        let focus = cx.update(|_window, app| input.read(app).focus_handle(app));
        cx.update(|window, app| window.focus(&focus, app));

        // Normal Enter sends; Shift-Enter is deliberately unavailable.
        cx.simulate_keystrokes("shift-enter enter");
        assert_eq!(submitted.borrow().as_slice(), &["draft"]);
        assert_eq!(cx.update(|_window, app| input.read(app).text()), "draft");

        // Insert keeps Maple's newline/send behavior.
        cx.simulate_keystrokes("A shift-enter enter");
        assert_eq!(cx.update(|_window, app| input.read(app).text()), "draft\n");
        assert_eq!(submitted.borrow().as_slice(), &["draft", "draft\n"]);

        // Visual Enter is consumed and never sends.
        cx.simulate_keystrokes("escape v enter");
        assert_eq!(submitted.borrow().len(), 2);
        assert_eq!(
            cx.update(|_window, app| input.read(app).vim_mode()),
            Some(VimMode::Visual)
        );
    }

    #[gpui::test]
    fn toggling_vim_cannot_reuse_state_across_standard_edits(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| {
            let mut input = TextInput::new("", cx).composer_vim(true);
            input.set_text("abc", cx);
            input
        });
        input.update(cx, |input, cx| {
            assert!(input.execute_vim_command(VimCommand::CountDigit(0), cx));
            assert!(input.execute_vim_command(VimCommand::DeleteChars, cx));
            assert_eq!(input.text_ref(), "bc");
            assert!(input.vim.as_ref().unwrap().last_change().is_some());
            assert!(input.vim.as_ref().unwrap().unnamed_register().is_some());

            input.set_vim_enabled(false, cx);
            assert_eq!(input.vim_mode(), None);
            let end = input.content.len();
            input.replace_range_standard(end..end, "z", cx);
            input.set_vim_enabled(true, cx);

            let vim = input.vim.as_ref().unwrap();
            assert_eq!(vim.mode(), VimMode::Normal);
            assert!(vim.last_change().is_none());
            assert!(vim.unnamed_register().is_none());
            assert_eq!(input.text_ref(), "bcz");
        });
    }

    fn word_left_key() -> &'static str {
        if cfg!(target_os = "macos") {
            "alt-left"
        } else {
            "ctrl-left"
        }
    }

    fn select_word_left_key() -> &'static str {
        if cfg!(target_os = "macos") {
            "alt-shift-left"
        } else {
            "ctrl-shift-left"
        }
    }

    fn delete_word_backward_key() -> &'static str {
        if cfg!(target_os = "macos") {
            "alt-backspace"
        } else {
            "ctrl-backspace"
        }
    }

    #[gpui::test]
    fn home_and_word_keys_edit_the_current_line(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| {
            crate::desktop::register_key_bindings(cx);
            let mut input = TextInput::new("", cx).multiline(4);
            input.set_text("hello world\nsecond", cx);
            input
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| InputHost {
            input: input.clone(),
        });
        let focus = cx.update(|_window, app| input.read(app).focus_handle(app));
        cx.update(|window, app| window.focus(&focus, app));

        // Home is the current line, not the start of the whole field.
        cx.simulate_keystrokes("home");
        cx.update(|_window, app| {
            assert_eq!(input.read(app).selected_range, 12..12);
        });
        cx.simulate_keystrokes("shift-end");
        cx.update(|_window, app| {
            assert_eq!(input.read(app).selected_range, 12..18);
        });

        // Step back onto the end of "hello world", then to the start of "world".
        cx.simulate_keystrokes("left");
        cx.simulate_keystrokes(word_left_key());
        cx.update(|_window, app| {
            assert_eq!(input.read(app).selected_range, 6..6);
        });
        cx.simulate_keystrokes(delete_word_backward_key());
        cx.update(|_window, app| {
            let input = input.read(app);
            assert_eq!(input.text_ref(), "world\nsecond");
            assert_eq!(input.selected_range, 0..0);
        });
    }

    #[gpui::test]
    fn select_word_extends_the_selection(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| {
            crate::desktop::register_key_bindings(cx);
            let mut input = TextInput::new("", cx);
            input.set_text("hello world", cx);
            input
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| InputHost {
            input: input.clone(),
        });
        let focus = cx.update(|_window, app| input.read(app).focus_handle(app));
        cx.update(|window, app| window.focus(&focus, app));

        cx.simulate_keystrokes(select_word_left_key());
        cx.update(|_window, app| {
            assert_eq!(input.read(app).selected_range, 6..11);
        });
    }

    #[gpui::test]
    fn word_keys_run_in_insert_and_stay_out_of_normal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| {
            crate::desktop::register_key_bindings(cx);
            let mut input = TextInput::new("", cx).composer_vim(true);
            input.set_text("hello world", cx);
            input
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| InputHost {
            input: input.clone(),
        });
        let focus = cx.update(|_window, app| input.read(app).focus_handle(app));
        cx.update(|window, app| window.focus(&focus, app));

        cx.simulate_keystrokes(word_left_key());
        cx.update(|_window, app| {
            let input = input.read(app);
            assert_eq!(input.vim_mode(), Some(VimMode::Normal));
            // Normal mode sits on the last character, and the word key does not move it.
            assert_eq!(input.selected_range, 10..10);
        });

        cx.simulate_keystrokes("i");
        cx.simulate_keystrokes(word_left_key());
        cx.update(|_window, app| {
            let input = input.read(app);
            assert_eq!(input.vim_mode(), Some(VimMode::Insert));
            assert_eq!(input.selected_range, 6..6);
        });
    }

    #[gpui::test]
    fn delete_to_line_start_removes_the_current_line_prefix(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| {
            let mut input = TextInput::new("", cx).multiline(4);
            input.set_text("keep\nremove me", cx);
            // Caret at the start of "me".
            input.selected_range = 12..12;
            input
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| InputHost {
            input: input.clone(),
        });
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.delete_to_line_start(&DeleteToLineStart, window, cx);
            });
        });
        cx.update(|_window, app| {
            let input = input.read(app);
            assert_eq!(input.text_ref(), "keep\nme");
            assert_eq!(input.selected_range, 5..5);
        });
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn command_line_shortcuts_use_visible_rows(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let text = "alpha beta gamma delta ".repeat(12);
        let input = cx.new(|cx| {
            crate::desktop::register_key_bindings(cx);
            let mut input = TextInput::new("", cx).multiline(8);
            input.set_text(&text, cx);
            input
        });
        let (_host, cx) = cx.add_window_view(|_, _| InputHost {
            input: input.clone(),
        });
        let (start, end) = cx.update(|window, app| {
            window.focus(&input.read(app).focus_handle(app), app);
            let line = &input.read(app).last_layout.as_ref().unwrap().lines[0];
            let wrap = |n: usize| {
                let boundary = line.wrap_boundaries()[n];
                line.runs()[boundary.run_ix].glyphs[boundary.glyph_ix].index
            };
            (wrap(0), wrap(1))
        });
        cx.update(|_, app| input.update(app, |input, cx| input.move_to(start + 2, cx)));
        cx.simulate_keystrokes("cmd-left cmd-left cmd-backspace");
        cx.update(|_, app| {
            assert_eq!(input.read(app).selected_range, start..start);
            assert_eq!(input.read(app).text_ref(), text);
        });
        cx.simulate_keystrokes("cmd-shift-right");
        cx.update(|_, app| assert_eq!(input.read(app).selected_range, start..end));
        cx.simulate_keystrokes("cmd-right cmd-backspace");
        cx.update(|_, app| {
            assert_eq!(
                input.read(app).text_ref(),
                format!("{}{}", &text[..start], &text[end..])
            )
        });
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn vertical_shortcuts_select_rows_paragraphs_and_document(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| {
            crate::desktop::register_key_bindings(cx);
            let mut input = TextInput::new("", cx).multiline(8).composer_vim(true);
            input.set_text("aaaaaaaaaa\na\naaaaaaaaaa\n\naaaaaaaaaa", cx);
            input
        });
        let (_host, cx) = cx.add_window_view(|_, _| InputHost {
            input: input.clone(),
        });
        cx.update(|window, app| window.focus(&input.read(app).focus_handle(app), app));
        cx.simulate_keystrokes("i");
        cx.update(|_, app| input.update(app, |input, cx| input.move_to_offset(8, cx)));
        // Short and empty rows preserve the column; reversal preserves the anchor.
        for (key, heads) in [
            ("shift-down", [12, 21, 24, 33]),
            ("shift-up", [24, 21, 12, 8]),
        ] {
            for head in heads {
                cx.simulate_keystrokes(key);
                cx.update(|_, app| assert_eq!(input.read(app).selected_range, 8..head));
            }
        }
        for (key, range) in [
            ("alt-shift-down", 8..11),
            ("alt-shift-up", 8..8),
            ("alt-down", 10..10),
            ("alt-down", 12..12),
            ("alt-up", 11..11),
            ("cmd-shift-up", 0..11),
            ("cmd-shift-down", 0..35),
            ("cmd-up", 0..0),
            ("cmd-down", 35..35),
        ] {
            cx.simulate_keystrokes(key);
            cx.update(|_, app| assert_eq!(input.read(app).selected_range, range, "{key}"));
        }
    }
}
