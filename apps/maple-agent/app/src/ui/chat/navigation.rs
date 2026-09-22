//! Screen-owned application Vim navigation for Chat.
//!
//! Stable task, timeline-item, and project IDs are the source of truth; row
//! numbers are resolved only when revealing a target in a virtual list. This
//! keeps human navigation correct across streaming merges, reorder, filtering,
//! and project folding without importing the model/controller harness.

use std::collections::HashMap;

use gpui::{Context, Focusable, Window};
use maple_agent::agent::AgentTimelineItem;

use super::ChatScreen;
use super::commands::ChatCommand;
use super::transcript::{attachment_refs, has_tool_input};
use crate::ui::application_vim::{self, CountOutcome, CountState, SpatialDirection};
use crate::ui::text_input::vim::VimMode;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ChatRegion {
    Sidebar,
    #[default]
    Transcript,
    Composer,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum SidebarTarget {
    NewTask,
    Projects,
    Task(String),
    Archived,
}

#[derive(Clone, Debug)]
pub(super) struct ApplicationVimState {
    pub(super) region: ChatRegion,
    last_main_region: ChatRegion,
    return_from_composer: ChatRegion,
    pub(super) count: CountState,
    transcript_by_task: HashMap<String, String>,
    follow_by_task: HashMap<String, bool>,
    pub(super) permission_choice: usize,
}

impl Default for ApplicationVimState {
    fn default() -> Self {
        Self {
            region: ChatRegion::Transcript,
            last_main_region: ChatRegion::Transcript,
            return_from_composer: ChatRegion::Transcript,
            count: CountState::default(),
            transcript_by_task: HashMap::new(),
            follow_by_task: HashMap::new(),
            permission_choice: 0,
        }
    }
}

impl ApplicationVimState {
    fn set_region(&mut self, region: ChatRegion) {
        if matches!(region, ChatRegion::Transcript | ChatRegion::Composer) {
            self.last_main_region = region;
        }
        self.region = region;
        self.count.clear();
    }

    fn selected_transcript<'a>(&'a self, task_id: Option<&str>) -> Option<&'a str> {
        task_id.and_then(|task_id| self.transcript_by_task.get(task_id).map(String::as_str))
    }

    fn set_transcript(&mut self, task_id: &str, item_id: String, follow: bool) {
        self.transcript_by_task.insert(task_id.to_owned(), item_id);
        self.follow_by_task.insert(task_id.to_owned(), follow);
        self.set_region(ChatRegion::Transcript);
    }

    fn follows(&self, task_id: Option<&str>) -> bool {
        task_id
            .and_then(|task_id| self.follow_by_task.get(task_id))
            .copied()
            .unwrap_or(true)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ApplicationVimCommand {
    Activate,
    Collapse,
    CopyTarget,
    Escape,
    Expand,
    First,
    FocusComposer,
    Last,
    MoveRegion(SpatialDirection),
    NewestAssistant,
    Next,
    NextAnnotation,
    NextAssistant,
    Previous,
    PreviousAnnotation,
    PreviousAssistant,
    Search,
}

impl ChatScreen {
    pub(super) fn application_vim_context(&self) -> &'static str {
        if self.application_vim_enabled {
            "Chat ApplicationVim"
        } else {
            "Chat"
        }
    }

    /// Persisted application Vim is already enabled before the first async
    /// session snapshot arrives. Start an empty chat on the sidebar so root
    /// motions have a semantic target; sidebar rebuilds reconcile the stable
    /// selection as sessions appear.
    pub(super) fn initialize_application_vim_surface(&mut self) {
        if self.application_vim_enabled && self.navigable_timeline_ids().is_empty() {
            self.application_vim.set_region(ChatRegion::Sidebar);
        }
    }

    pub(super) fn set_application_vim_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        for input in [
            self.composer.as_ref(),
            self.pending_question_input.as_ref(),
            self.root_input.as_ref(),
        ]
        .into_iter()
        .flatten()
        .cloned()
        .collect::<Vec<_>>()
        {
            input.update(cx, |input, cx| {
                input.set_application_vim_enabled(enabled, cx)
            });
        }
        // The sidebar switches its own inputs and row targets.
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.set_application_vim_enabled(enabled, cx)
        });
        if self.application_vim_enabled == enabled {
            return;
        }
        self.application_vim_enabled = enabled;
        if enabled {
            if self.navigable_timeline_ids().is_empty() {
                self.application_vim.set_region(ChatRegion::Sidebar);
                self.ensure_sidebar_application_selection(cx);
            } else {
                self.application_vim.set_region(ChatRegion::Transcript);
                self.select_transcript_edge(false, true, cx);
            }
            self.screen_focus_pending = true;
        } else {
            // The disabled path carries no per-task or per-row navigation
            // projection. Re-enabling reconstructs it from authoritative Chat
            // state above.
            self.application_vim = ApplicationVimState::default();
        }
        cx.notify();
    }

    pub(super) fn focus_application_vim(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.application_vim_enabled {
            return;
        }
        if self.application_vim.region == ChatRegion::Composer {
            self.application_vim
                .set_region(self.application_vim.return_from_composer);
        }
        if let Some(handle) = &self.application_focus {
            window.focus(handle, cx);
        }
        cx.notify();
    }

    pub(super) fn restore_region_from_composer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.application_vim_enabled {
            return;
        }
        self.application_vim
            .set_region(self.application_vim.return_from_composer);
        if let Some(handle) = &self.application_focus {
            window.focus(handle, cx);
        }
        self.reveal_application_selection(cx);
        cx.notify();
    }

    pub(super) fn application_vim_owns_unfocused_typing(
        &self,
        window: &Window,
        cx: &gpui::App,
    ) -> bool {
        if !self.application_vim_enabled {
            return false;
        }
        let focused = window.focused(cx);
        !([
            self.composer.as_ref(),
            self.pending_question_input.as_ref(),
            self.root_input.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|input| Some(input.read(cx).focus_handle(cx)) == focused)
            || self.sidebar_input_focused(&focused, cx))
    }

    pub(super) fn application_vim_push_count(&mut self, digit: u8, cx: &mut Context<Self>) {
        if matches!(
            self.application_vim.count.push(digit),
            CountOutcome::Capped(_)
        ) {
            self.notice = Some("Application Vim count capped at 999999".into());
        }
        cx.notify();
    }

    pub(super) fn execute_application_vim(
        &mut self,
        command: ApplicationVimCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.application_vim_enabled {
            return;
        }
        match command {
            ApplicationVimCommand::Next => {
                let count = self.application_vim.count.take();
                self.move_application_selection(1, count, cx);
            }
            ApplicationVimCommand::Previous => {
                let count = self.application_vim.count.take();
                self.move_application_selection(-1, count, cx);
            }
            ApplicationVimCommand::First => {
                self.application_vim.count.clear();
                self.select_application_edge(true, cx);
            }
            ApplicationVimCommand::Last => {
                self.application_vim.count.clear();
                self.select_application_edge(false, cx);
            }
            ApplicationVimCommand::NewestAssistant => {
                self.application_vim.count.clear();
                self.select_assistant(false, usize::MAX, window, cx);
            }
            ApplicationVimCommand::NextAssistant => {
                let count = self.application_vim.count.take();
                self.select_assistant(true, count, window, cx);
            }
            ApplicationVimCommand::PreviousAssistant => {
                let count = self.application_vim.count.take();
                self.select_assistant(false, count, window, cx);
            }
            ApplicationVimCommand::NextAnnotation | ApplicationVimCommand::PreviousAnnotation => {
                self.application_vim.count.clear();
                self.notice = Some("No annotations are available in this preview".into());
                cx.notify();
            }
            ApplicationVimCommand::MoveRegion(direction) => {
                let count = self.application_vim.count.take();
                for _ in 0..count {
                    if !self.move_application_region(direction, window, cx) {
                        break;
                    }
                }
            }
            ApplicationVimCommand::FocusComposer => {
                self.application_vim.count.clear();
                self.focus_composer_last_insertion(window, cx);
            }
            ApplicationVimCommand::Search => {
                self.application_vim.count.clear();
                self.focus_sidebar_search(window, cx);
            }
            ApplicationVimCommand::Activate => {
                self.application_vim.count.clear();
                self.activate_application_selection(window, cx);
            }
            ApplicationVimCommand::Collapse => {
                self.application_vim.count.clear();
                self.set_application_expanded(false, cx);
            }
            ApplicationVimCommand::Expand => {
                self.application_vim.count.clear();
                self.set_application_expanded(true, cx);
            }
            ApplicationVimCommand::CopyTarget => {
                self.application_vim.count.clear();
                self.copy_application_target(cx);
            }
            ApplicationVimCommand::Escape => {
                self.application_vim.count.clear();
                self.application_escape(window, cx);
            }
        }
    }

    fn move_application_selection(
        &mut self,
        direction: isize,
        count: usize,
        cx: &mut Context<Self>,
    ) {
        if self.root_menu_open {
            for _ in 0..count {
                self.step_root_menu(direction, cx);
            }
            return;
        }
        if self
            .sidebar
            .update(cx, |sidebar, cx| sidebar.step_popup(direction, count, cx))
        {
            return;
        }
        if let Some(question) = self.current_question() {
            let step = self
                .question_step
                .min(question.questions.len().saturating_sub(1));
            let len = question
                .questions
                .get(step)
                .map(|question| question.options.len())
                .unwrap_or(0);
            if len > 0 {
                let current = self.question_selected.get(&step).and_then(|s| s.cursor);
                let next = stepped_index(current, len, direction, count);
                self.focus_question_option(step, next, cx);
            }
            return;
        }
        if self.current_permission().is_some() {
            self.application_vim.permission_choice = stepped_index(
                Some(self.application_vim.permission_choice),
                2,
                direction,
                count,
            );
            cx.notify();
            return;
        }
        match self.application_vim.region {
            ChatRegion::Sidebar => self.move_sidebar_selection(direction, count, cx),
            ChatRegion::Transcript => self.move_transcript_selection(direction, count, cx),
            ChatRegion::Composer => {}
        }
    }

    fn select_application_edge(&mut self, first: bool, cx: &mut Context<Self>) {
        if self.root_menu_open {
            let len = self.root_menu_rows();
            self.root_menu_selected = (len > 0).then_some(if first { 0 } else { len - 1 });
            cx.notify();
            return;
        }
        if self
            .sidebar
            .update(cx, |sidebar, cx| sidebar.popup_edge(first, cx))
        {
            return;
        }
        if let Some(question) = self.current_question() {
            let step = self
                .question_step
                .min(question.questions.len().saturating_sub(1));
            let len = question
                .questions
                .get(step)
                .map(|question| question.options.len())
                .unwrap_or(0);
            if len > 0 {
                self.focus_question_option(step, if first { 0 } else { len - 1 }, cx);
            }
            return;
        }
        if self.current_permission().is_some() {
            self.application_vim.permission_choice = usize::from(!first);
            cx.notify();
            return;
        }
        match self.application_vim.region {
            ChatRegion::Sidebar => self.select_sidebar_edge(first, cx),
            ChatRegion::Transcript => self.select_transcript_edge(first, !first, cx),
            ChatRegion::Composer => {}
        }
    }

    fn move_application_region(
        &mut self,
        direction: SpatialDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // A pointer click can focus the TextInput directly without passing
        // through `focus_composer`. Region commands originate from the
        // focused key context, so reconcile that live focus before planning
        // the move instead of trusting a stale application-region bookmark.
        let composer_focused = self
            .composer
            .as_ref()
            .is_some_and(|composer| Some(composer.read(cx).focus_handle(cx)) == window.focused(cx));
        if composer_focused && self.application_vim.region != ChatRegion::Composer {
            self.application_vim.return_from_composer = self.application_vim.region;
            self.application_vim.set_region(ChatRegion::Composer);
        }
        let current = self.application_vim.region;
        let target = match (current, direction) {
            (ChatRegion::Sidebar, SpatialDirection::Right) => {
                if self.application_vim.last_main_region == ChatRegion::Transcript
                    && !self.navigable_timeline_ids().is_empty()
                {
                    ChatRegion::Transcript
                } else {
                    ChatRegion::Composer
                }
            }
            (ChatRegion::Transcript, SpatialDirection::Left)
            | (ChatRegion::Composer, SpatialDirection::Left)
                if !self.sidebar_collapsed =>
            {
                ChatRegion::Sidebar
            }
            (ChatRegion::Transcript, SpatialDirection::Down) => ChatRegion::Composer,
            (ChatRegion::Composer, SpatialDirection::Up)
                if !self.navigable_timeline_ids().is_empty() =>
            {
                ChatRegion::Transcript
            }
            // Reaching a spatial boundary is normal Vim behavior.
            _ => return false,
        };
        if target == ChatRegion::Composer {
            self.focus_composer(window, cx);
        } else {
            self.application_vim.set_region(target);
            if target == ChatRegion::Sidebar {
                self.ensure_sidebar_application_selection(cx);
            } else {
                self.ensure_transcript_application_selection();
            }
            if let Some(handle) = &self.application_focus {
                window.focus(handle, cx);
            }
            self.reveal_application_selection(cx);
            cx.notify();
        }
        true
    }

    fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(composer) = self.composer.clone() else {
            return;
        };
        if self.application_vim.region != ChatRegion::Composer {
            self.application_vim.return_from_composer = self.application_vim.region;
        }
        self.application_vim.set_region(ChatRegion::Composer);
        let handle = composer.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn focus_composer_last_insertion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(composer) = self.composer.clone() else {
            return;
        };
        if matches!(composer.read(cx).vim_mode(), None | Some(VimMode::Disabled)) {
            self.notice = Some("Enable composer Vim to use gi".into());
            cx.notify();
            return;
        }
        if self.application_vim.region != ChatRegion::Composer {
            self.application_vim.return_from_composer = self.application_vim.region;
        }
        let entered = composer.update(cx, |input, cx| input.focus_last_insertion(cx));
        if !entered {
            self.notice = Some("Composer Vim is unavailable".into());
            cx.notify();
            return;
        }
        self.application_vim.set_region(ChatRegion::Composer);
        let handle = composer.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    pub(super) fn application_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.application_vim_ordinary_input_focused(window, cx) {
            self.focus_application_vim(window, cx);
            return;
        }
        if self.application_vim_disabled_composer_focused(window, cx) {
            self.restore_region_from_composer(window, cx);
            return;
        }
        // Application Vim owns only its focus transitions. Once none consumed
        // Escape, retain Chat's legacy close/menu/run-stop behavior.
        self.escape(cx);
    }

    fn application_vim_ordinary_input_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        let focused = window.focused(cx);
        self.pending_question_input
            .as_ref()
            .is_some_and(|input| Some(input.read(cx).focus_handle(cx)) == focused)
            || self.sidebar_input_focused(&focused, cx)
    }

    /// Whether the sidebar's search or rename field has focus.
    fn sidebar_input_focused(&self, focused: &Option<gpui::FocusHandle>, cx: &gpui::App) -> bool {
        self.sidebar
            .read(cx)
            .inputs()
            .iter()
            .any(|input| Some(input.read(cx).focus_handle(cx)) == *focused)
    }

    fn application_vim_disabled_composer_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        let Some(composer) = self.composer.as_ref() else {
            return false;
        };
        matches!(composer.read(cx).vim_mode(), None | Some(VimMode::Disabled))
            && Some(composer.read(cx).focus_handle(cx)) == window.focused(cx)
    }

    fn activate_application_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.root_menu_open {
            self.confirm_root_menu(cx);
            return;
        }
        if self
            .sidebar
            .update(cx, |sidebar, cx| sidebar.activate_popup(cx))
        {
            return;
        }
        if self.current_question().is_some() {
            let step = self
                .current_question()
                .map(|question| {
                    self.question_step
                        .min(question.questions.len().saturating_sub(1))
                })
                .unwrap_or_default();
            if self
                .current_question()
                .and_then(|q| q.questions.get(step))
                .is_some_and(|q| q.multi_select)
                && let Some(index) = self.question_selected.get(&step).and_then(|s| s.cursor)
            {
                self.toggle_question_option(step, index, cx);
            } else if self
                .question_selected
                .get(&step)
                .is_some_and(|s| !s.picked.is_empty())
            {
                self.submit_question(cx);
            } else if let Some(input) = self.pending_question_input.clone() {
                // Enter before an option is picked is the semantic route into
                // the card's free-form answer. Enter inside the input still
                // submits through TextInput's existing callback; j/k followed
                // by Enter retains the direct submit path for single choices.
                let handle = input.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
                cx.notify();
            }
            return;
        }
        if self.current_permission().is_some() {
            self.respond_permission(self.application_vim.permission_choice == 0, cx);
            return;
        }
        match self.application_vim.region {
            ChatRegion::Sidebar => match self.sidebar.read(cx).vim_selected().cloned() {
                Some(SidebarTarget::NewTask) => self.new_session(cx),
                Some(SidebarTarget::Projects) => self
                    .sidebar
                    .update(cx, |sidebar, cx| sidebar.toggle_switcher_menu(cx)),
                Some(SidebarTarget::Task(task_id)) => self.select_session(&task_id, cx),
                Some(SidebarTarget::Archived) => self.toggle_archived_visibility(cx),
                None => {}
            },
            ChatRegion::Transcript => self.set_application_expanded_toggle(cx),
            ChatRegion::Composer => {}
        }
    }

    fn set_application_expanded_toggle(&mut self, cx: &mut Context<Self>) {
        let Some(item_id) = self.selected_transcript_id().map(str::to_owned) else {
            return;
        };
        let Some(&(index, _)) = self.timeline_index.get(&item_id) else {
            return;
        };
        let Some(item) = self.timeline.get(index) else {
            return;
        };
        if matches!(
            item.item_type.as_str(),
            "tool" | "toolCall" | "thinking" | "reasoning"
        ) {
            let expanded = self.tool_details != self.toggled_tools.contains(&item_id);
            self.set_timeline_item_expanded(&item_id, !expanded, cx);
        }
    }

    fn set_application_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        if self.application_vim.region == ChatRegion::Sidebar {
            self.sidebar
                .update(cx, |sidebar, cx| sidebar.vim_set_expanded(expanded, cx));
            return;
        }
        let Some(item_id) = self.selected_transcript_id().map(str::to_owned) else {
            return;
        };
        self.set_timeline_item_expanded(&item_id, expanded, cx);
    }

    fn set_timeline_item_expanded(
        &mut self,
        item_id: &str,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(&(index, _)) = self.timeline_index.get(item_id) else {
            return;
        };
        let Some(item) = self.timeline.get(index) else {
            return;
        };
        // Expanding or collapsing a transcript leaf is a normal Vim no-op.
        if !matches!(
            item.item_type.as_str(),
            "tool" | "toolCall" | "thinking" | "reasoning"
        ) {
            return;
        }
        let current = self.tool_details != self.toggled_tools.contains(item_id);
        if current != expanded {
            if !self.toggled_tools.insert(item_id.to_owned()) {
                self.toggled_tools.remove(item_id);
            }
            self.list_state.remeasure_items(index..index + 1);
            cx.notify();
        }
    }

    fn copy_application_target(&mut self, cx: &mut Context<Self>) {
        if self.application_vim.region != ChatRegion::Transcript {
            self.notice = Some("y copies the selected transcript item".into());
            cx.notify();
            return;
        }
        let Some(item_id) = self.selected_transcript_id() else {
            return;
        };
        let Some(&(index, revision)) = self.timeline_index.get(item_id) else {
            return;
        };
        let Some(item) = self.timeline.get(index) else {
            return;
        };
        let text = self.canonical_timeline_item_text(item, revision);
        if text.trim().is_empty() {
            self.notice = Some("The selected item has no visible text to copy".into());
            cx.notify();
            return;
        }
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
    }

    fn canonical_timeline_item_text(&self, item: &AgentTimelineItem, revision: u64) -> String {
        match item.item_type.as_str() {
            "message" | "thinking" | "reasoning" => {
                self.derived.get(item, revision).text.to_string()
            }
            "tool" | "toolCall" => self
                .tool_summaries
                .get(&item.id)
                .map(ToString::to_string)
                .or_else(|| {
                    self.derived
                        .get(item, revision)
                        .output_text
                        .as_ref()
                        .map(ToString::to_string)
                })
                .or_else(|| item.title.clone())
                .unwrap_or_default(),
            _ => item
                .text
                .clone()
                .or_else(|| item.title.clone())
                .unwrap_or_default(),
        }
    }

    pub(super) fn navigable_timeline_ids(&self) -> Vec<String> {
        self.timeline
            .iter()
            .filter(|item| timeline_item_is_navigable(item))
            .map(|item| item.id.clone())
            .collect()
    }

    fn ensure_transcript_application_selection(&mut self) {
        let ids = self.navigable_timeline_ids();
        let Some(task_id) = self.selected_session.clone() else {
            return;
        };
        let selected = self
            .application_vim
            .selected_transcript(Some(&task_id))
            .is_some_and(|selected| ids.iter().any(|item_id| item_id == selected));
        if !selected && let Some(item_id) = ids.last() {
            self.application_vim
                .set_transcript(&task_id, item_id.clone(), true);
        }
    }

    fn move_transcript_selection(
        &mut self,
        direction: isize,
        count: usize,
        cx: &mut Context<Self>,
    ) {
        let ids = self.navigable_timeline_ids();
        let Some(task_id) = self.selected_session.clone() else {
            return;
        };
        if ids.is_empty() {
            return;
        }
        let current = self
            .application_vim
            .selected_transcript(Some(&task_id))
            .and_then(|selected| ids.iter().position(|item_id| item_id == selected));
        let index = stepped_index(current, ids.len(), direction, count);
        let follow = index + 1 == ids.len() && direction > 0;
        self.application_vim
            .set_transcript(&task_id, ids[index].clone(), follow);
        self.follow_tail(follow);
        self.reveal_application_selection(cx);
        cx.notify();
    }

    /// Keep the transcript pinned to its newest content, or hold it where
    /// the selection sits. The list re-engages on its own once the view
    /// returns to the bottom.
    fn follow_tail(&mut self, follow: bool) {
        if follow {
            self.list_state.scroll_to_end();
        } else {
            self.list_state.pause_following_tail();
        }
    }

    fn select_transcript_edge(&mut self, first: bool, follow: bool, cx: &mut Context<Self>) {
        let ids = self.navigable_timeline_ids();
        let Some(task_id) = self.selected_session.clone() else {
            return;
        };
        let Some(item_id) = (if first { ids.first() } else { ids.last() }) else {
            return;
        };
        self.application_vim
            .set_transcript(&task_id, item_id.clone(), follow && !first);
        self.follow_tail(follow && !first);
        self.reveal_application_selection(cx);
        cx.notify();
    }

    fn select_assistant(
        &mut self,
        forward: bool,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let assistant = self
            .timeline
            .iter()
            .filter(|item| item.role.as_deref() == Some("assistant"))
            .filter(|item| timeline_item_is_navigable(item))
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        let Some(task_id) = self.selected_session.clone() else {
            return;
        };
        if assistant.is_empty() {
            self.notice = Some("No assistant item is available".into());
            cx.notify();
            return;
        }
        let current = self
            .application_vim
            .selected_transcript(Some(&task_id))
            .and_then(|selected| assistant.iter().position(|item_id| item_id == selected));
        let index = if count == usize::MAX {
            assistant.len() - 1
        } else {
            stepped_index(
                current,
                assistant.len(),
                if forward { 1 } else { -1 },
                count,
            )
        };
        self.application_vim
            .set_transcript(&task_id, assistant[index].clone(), false);
        self.follow_tail(false);
        self.reveal_application_selection(cx);
        self.focus_application_vim(window, cx);
    }

    pub(super) fn reconcile_timeline_application_selection(&mut self, old_order: &[String]) {
        if !self.application_vim_enabled {
            return;
        }
        let Some(task_id) = self.selected_session.clone() else {
            return;
        };
        let next = self.navigable_timeline_ids();
        let previous = self
            .application_vim
            .selected_transcript(Some(&task_id))
            .map(str::to_owned);
        let selected = reconcile_stable_selection(previous.as_ref(), old_order, &next)
            .or_else(|| next.last().cloned());
        if let Some(selected) = selected {
            self.application_vim
                .transcript_by_task
                .insert(task_id, selected);
        } else {
            self.application_vim.transcript_by_task.remove(&task_id);
        }
    }

    pub(super) fn follow_application_stream(&mut self) {
        if !self.application_vim_enabled {
            return;
        }
        let Some(task_id) = self.selected_session.clone() else {
            return;
        };
        if !self.application_vim.follows(Some(&task_id)) {
            return;
        }
        if let Some(item_id) = self.navigable_timeline_ids().last().cloned() {
            self.application_vim
                .transcript_by_task
                .insert(task_id, item_id);
        }
    }

    pub(super) fn selected_transcript_id(&self) -> Option<&str> {
        self.application_vim
            .selected_transcript(self.selected_session.as_deref())
    }

    pub(super) fn application_vim_selects_timeline(&self, item_id: &str) -> bool {
        self.application_vim_enabled
            && self.application_vim.region == ChatRegion::Transcript
            && self.selected_transcript_id() == Some(item_id)
    }

    pub(super) fn select_timeline_from_pointer(
        &mut self,
        item_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.application_vim_enabled
            || !self
                .timeline
                .iter()
                .any(|item| item.id == item_id && timeline_item_is_navigable(item))
        {
            return;
        }
        let Some(task_id) = self.selected_session.clone() else {
            return;
        };
        self.application_vim
            .set_transcript(&task_id, item_id.to_owned(), false);
        self.focus_application_vim(window, cx);
    }

    fn ensure_sidebar_application_selection(&mut self, cx: &mut Context<Self>) {
        self.sidebar
            .update(cx, |sidebar, _cx| sidebar.vim_ensure_selection());
    }

    fn move_sidebar_selection(&mut self, direction: isize, count: usize, cx: &mut Context<Self>) {
        if self
            .sidebar
            .update(cx, |sidebar, cx| sidebar.vim_move(direction, count, cx))
        {
            self.application_vim.set_region(ChatRegion::Sidebar);
            cx.notify();
        }
    }

    fn select_sidebar_edge(&mut self, first: bool, cx: &mut Context<Self>) {
        if self
            .sidebar
            .update(cx, |sidebar, cx| sidebar.vim_edge(first, cx))
        {
            self.application_vim.set_region(ChatRegion::Sidebar);
            cx.notify();
        }
    }

    #[cfg(test)]
    pub(super) fn application_vim_projection_is_empty(&self, cx: &gpui::App) -> bool {
        self.application_vim.transcript_by_task.is_empty()
            && self.application_vim.follow_by_task.is_empty()
            && self.sidebar.read(cx).vim_projection_is_empty()
    }

    pub(super) fn select_sidebar_from_pointer(
        &mut self,
        target: SidebarTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.application_vim_enabled {
            return;
        }
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.vim_select(target, cx));
        self.application_vim.set_region(ChatRegion::Sidebar);
        self.focus_application_vim(window, cx);
    }

    fn reveal_application_selection(&self, cx: &gpui::App) {
        match self.application_vim.region {
            ChatRegion::Sidebar => self.sidebar.read(cx).vim_reveal(),
            ChatRegion::Transcript => {
                if let Some(item_id) = self.selected_transcript_id()
                    && let Some(&(row, _)) = self.timeline_index.get(item_id)
                {
                    self.list_state.scroll_to_reveal_item(row);
                }
            }
            ChatRegion::Composer => {}
        }
    }

    pub(super) fn application_permission_choice(&self) -> Option<usize> {
        (self.application_vim_enabled && self.current_permission().is_some())
            .then_some(self.application_vim.permission_choice)
    }

    // Typed GPUI action adapters. Every one enters through ChatCommand so
    // pointer and keyboard behavior keep the same shallow command seam.
    pub(super) fn app_vim_next(
        &mut self,
        _: &application_vim::Next,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_command(
            ChatCommand::ApplicationVim(ApplicationVimCommand::Next),
            window,
            cx,
        );
    }

    pub(super) fn app_vim_previous(
        &mut self,
        _: &application_vim::Previous,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_command(
            ChatCommand::ApplicationVim(ApplicationVimCommand::Previous),
            window,
            cx,
        );
    }

    pub(super) fn app_vim_count(
        &mut self,
        action: &application_vim::CountDigit,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.application_vim_push_count(action.digit, cx);
    }
}

macro_rules! unit_adapter {
    ($method:ident, $action:ty, $command:expr) => {
        impl ChatScreen {
            pub(super) fn $method(
                &mut self,
                _: &$action,
                window: &mut Window,
                cx: &mut Context<Self>,
            ) {
                self.execute_command(ChatCommand::ApplicationVim($command), window, cx);
            }
        }
    };
}

unit_adapter!(
    app_vim_first,
    application_vim::First,
    ApplicationVimCommand::First
);
unit_adapter!(
    app_vim_last,
    application_vim::Last,
    ApplicationVimCommand::Last
);
unit_adapter!(
    app_vim_activate,
    application_vim::Activate,
    ApplicationVimCommand::Activate
);
unit_adapter!(
    app_vim_collapse,
    application_vim::Collapse,
    ApplicationVimCommand::Collapse
);
unit_adapter!(
    app_vim_expand,
    application_vim::Expand,
    ApplicationVimCommand::Expand
);
unit_adapter!(
    app_vim_copy,
    application_vim::CopyTarget,
    ApplicationVimCommand::CopyTarget
);
unit_adapter!(
    app_vim_search,
    application_vim::Search,
    ApplicationVimCommand::Search
);
unit_adapter!(
    app_vim_escape,
    application_vim::Escape,
    ApplicationVimCommand::Escape
);
unit_adapter!(
    app_vim_composer,
    application_vim::FocusComposer,
    ApplicationVimCommand::FocusComposer
);
unit_adapter!(
    app_vim_newest_assistant,
    application_vim::NewestAssistant,
    ApplicationVimCommand::NewestAssistant
);
unit_adapter!(
    app_vim_next_assistant,
    application_vim::NextAssistant,
    ApplicationVimCommand::NextAssistant
);
unit_adapter!(
    app_vim_previous_assistant,
    application_vim::PreviousAssistant,
    ApplicationVimCommand::PreviousAssistant
);
unit_adapter!(
    app_vim_next_annotation,
    application_vim::NextAnnotation,
    ApplicationVimCommand::NextAnnotation
);
unit_adapter!(
    app_vim_previous_annotation,
    application_vim::PreviousAnnotation,
    ApplicationVimCommand::PreviousAnnotation
);

impl ChatScreen {
    pub(super) fn app_vim_move_region(
        &mut self,
        action: &application_vim::MoveRegion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_command(
            ChatCommand::ApplicationVim(ApplicationVimCommand::MoveRegion(action.direction)),
            window,
            cx,
        );
    }
}

fn timeline_item_is_navigable(item: &AgentTimelineItem) -> bool {
    if item.item_type == "internal" {
        return false;
    }
    if matches!(item.item_type.as_str(), "tool" | "toolCall") && has_tool_input(item, "todos") {
        return false;
    }
    if item.item_type == "message" {
        return item
            .text
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
            || attachment_refs(item).next().is_some();
    }
    true
}

pub(super) fn stepped_index(
    current: Option<usize>,
    len: usize,
    direction: isize,
    count: usize,
) -> usize {
    debug_assert!(len > 0);
    match current {
        Some(start) if direction > 0 => start.saturating_add(count).min(len - 1),
        Some(start) => start.saturating_sub(count),
        None if direction > 0 => count.saturating_sub(1).min(len - 1),
        None => len.saturating_sub(count.max(1)),
    }
}

pub(super) fn reconcile_stable_selection<T: Clone + Eq>(
    selected: Option<&T>,
    old_order: &[T],
    next_order: &[T],
) -> Option<T> {
    let selected = selected?;
    if next_order.contains(selected) {
        return Some(selected.clone());
    }
    let old_index = old_order.iter().position(|target| target == selected)?;
    old_order
        .iter()
        .skip(old_index + 1)
        .find(|target| next_order.contains(target))
        .or_else(|| {
            old_order[..old_index]
                .iter()
                .rev()
                .find(|target| next_order.contains(target))
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_selection_survives_reorder_and_uses_successor_then_predecessor_on_removal() {
        let old = vec!["a", "b", "c", "d"];
        let reordered = vec!["d", "b", "a", "c"];
        assert_eq!(
            reconcile_stable_selection(Some(&"b"), &old, &reordered),
            Some("b")
        );
        assert_eq!(
            reconcile_stable_selection(Some(&"b"), &old, &["a", "c", "d"]),
            Some("c")
        );
        assert_eq!(
            reconcile_stable_selection(Some(&"d"), &old, &["a", "b"]),
            Some("b")
        );
    }

    #[test]
    fn counted_moves_clamp_without_wrapping() {
        assert_eq!(stepped_index(Some(1), 5, 1, 20), 4);
        assert_eq!(stepped_index(Some(3), 5, -1, 20), 0);
        assert_eq!(stepped_index(None, 5, 1, 1), 0);
        assert_eq!(stepped_index(None, 5, -1, 1), 4);
    }
}
