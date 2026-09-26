//! One command path for Maple's existing chat actions.
//!
//! GPUI key actions and equivalent pointer controls adapt into this enum.
//! The command seam deliberately stays local to `ChatScreen`: feature state
//! and business behavior remain in their owning chat modules, while the app
//! root continues to own only screen lifecycle and routing.

use gpui::{Context, Window};

use super::navigation::ApplicationVimCommand;
use super::{
    AllowPermission, ChatEscape, ChatScreen, ChooseProject, CopySelection, FocusSearch, NewTask,
    NextTask, OpenAppSettings, OpenSettings, PickQuestionOption, PreviousTask, SelectAllTranscript,
    ToggleArchived, ToggleSidebar,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ChatCommand {
    ApplicationVim(ApplicationVimCommand),
    ChooseProject,
    CopySelection,
    Escape,
    FocusSearch,
    NewTask,
    NextTask,
    OpenSettings,
    PickQuestionOption { index: usize },
    PreviousTask,
    RespondPermission { allow: bool },
    SelectAllTranscript,
    ToggleArchived,
    ToggleSidebar,
}

impl ChatScreen {
    /// Execute an existing chat command against live screen state.
    ///
    /// This match is intentionally shallow: each arm delegates to the same
    /// feature-owned method that already implements the behavior. Centralizing
    /// only dispatch keeps GPUI actions and pointer controls from drifting
    /// without moving chat state or backend work into `MapleApp`.
    pub(super) fn execute_command(
        &mut self,
        command: ChatCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(command, ChatCommand::ApplicationVim(_)) {
            self.application_vim.count.clear();
        }
        match command {
            ChatCommand::ApplicationVim(command) => {
                self.execute_application_vim(command, window, cx)
            }
            ChatCommand::ChooseProject => self.toggle_project_picker(cx),
            ChatCommand::CopySelection => self.copy_selected_text(cx),
            ChatCommand::Escape if self.application_vim_enabled => {
                self.application_escape(window, cx)
            }
            ChatCommand::Escape => self.escape(cx),
            ChatCommand::FocusSearch => self.focus_sidebar_search(window, cx),
            ChatCommand::NewTask => self.new_session(cx),
            ChatCommand::NextTask => self.step_task(1, cx),
            ChatCommand::OpenSettings => cx.emit(OpenSettings),
            ChatCommand::PickQuestionOption { index } => {
                self.pick_and_submit_question_option(index, cx)
            }
            ChatCommand::PreviousTask => self.step_task(-1, cx),
            ChatCommand::RespondPermission { allow } => self.respond_permission(allow, cx),
            ChatCommand::SelectAllTranscript => self.select_all_text(cx),
            ChatCommand::ToggleArchived => self.toggle_archived_visibility(cx),
            ChatCommand::ToggleSidebar => self.toggle_sidebar_visibility(cx),
        }
    }

    pub(super) fn allow_permission(
        &mut self,
        _: &AllowPermission,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_command(ChatCommand::RespondPermission { allow: true }, window, cx);
    }

    pub(super) fn chat_escape(
        &mut self,
        _: &ChatEscape,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_command(ChatCommand::Escape, window, cx);
    }

    pub(super) fn choose_project(
        &mut self,
        _: &ChooseProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_command(ChatCommand::ChooseProject, window, cx);
    }

    pub(super) fn copy_selection(
        &mut self,
        _: &CopySelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_command(ChatCommand::CopySelection, window, cx);
    }

    pub(super) fn focus_search(
        &mut self,
        _: &FocusSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_command(ChatCommand::FocusSearch, window, cx);
    }

    pub(super) fn new_task_action(
        &mut self,
        _: &NewTask,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_command(ChatCommand::NewTask, window, cx);
    }

    pub(super) fn next_task(&mut self, _: &NextTask, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_command(ChatCommand::NextTask, window, cx);
    }

    pub(super) fn open_app_settings(
        &mut self,
        _: &OpenAppSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_command(ChatCommand::OpenSettings, window, cx);
    }

    pub(super) fn pick_question_option(
        &mut self,
        action: &PickQuestionOption,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_command(
            ChatCommand::PickQuestionOption {
                index: action.index,
            },
            window,
            cx,
        );
    }

    pub(super) fn previous_task(
        &mut self,
        _: &PreviousTask,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_command(ChatCommand::PreviousTask, window, cx);
    }

    pub(super) fn select_all_transcript(
        &mut self,
        _: &SelectAllTranscript,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_command(ChatCommand::SelectAllTranscript, window, cx);
    }

    pub(super) fn toggle_archived(
        &mut self,
        _: &ToggleArchived,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_command(ChatCommand::ToggleArchived, window, cx);
    }

    pub(super) fn toggle_sidebar(
        &mut self,
        _: &ToggleSidebar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_command(ChatCommand::ToggleSidebar, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::{AppContext, IntoElement, Render, TestAppContext, div};

    use super::*;
    use crate::backend::AgentBackend;

    struct EmptyHost;

    impl Render for EmptyHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    fn screen(cx: &mut TestAppContext) -> gpui::Entity<ChatScreen> {
        let backend =
            Arc::new(AgentBackend::new("http://127.0.0.1:9".to_string()).expect("backend"));
        let host = backend.local_host("user");
        cx.new(|cx| ChatScreen::new_inner(backend, host, "user".to_string(), cx))
    }

    #[gpui::test]
    fn commands_keep_feature_state_in_its_owners(cx: &mut TestAppContext) {
        let chat = screen(cx);
        chat.update(cx, |this, _cx| {
            this.sidebar_collapsed = true;
        });

        let (_host, cx) = cx.add_window_view(|_window, _cx| EmptyHost);
        let search_focus =
            cx.update(|_window, app| chat.read(app).sidebar.read(app).search_focus_handle(app));

        cx.update(|window, app| {
            chat.update(app, |this, cx| {
                this.execute_command(ChatCommand::FocusSearch, window, cx);
                assert!(!this.sidebar_collapsed);

                this.execute_command(ChatCommand::ToggleSidebar, window, cx);
                assert!(this.sidebar_collapsed);

                this.execute_command(ChatCommand::ToggleArchived, window, cx);
                assert!(this.sidebar.read(cx).archived_expanded());
                this.execute_command(ChatCommand::ToggleArchived, window, cx);
                assert!(!this.sidebar.read(cx).archived_expanded());
            });
            assert_eq!(window.focused(app), Some(search_focus));
        });
    }
}
