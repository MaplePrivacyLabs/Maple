//! General > Command line: install the `maple-agent` launcher so terminals
//! and ACP clients can start this app. File work lives in
//! [`crate::cli_launcher`]; this is its row and button.

use gpui::{Context, Div, div, prelude::*};

use super::navigation::{GeneralTarget, SettingsTarget};
use super::{SettingsScreen, setting_copy, theme, widgets};
use crate::cli_launcher::{LauncherState, LauncherStatus};

#[derive(Default)]
pub(super) struct CommandLineState {
    /// None until the first check finishes.
    status: Option<LauncherStatus>,
    busy: bool,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CommandLineAction {
    Install,
    Update,
    Remove,
}

impl CommandLineAction {
    fn label(self, busy: bool) -> &'static str {
        match (self, busy) {
            (Self::Install, false) => "Install",
            (Self::Install, true) => "Installing…",
            (Self::Update, false) => "Update",
            (Self::Update, true) => "Updating…",
            (Self::Remove, false) => "Remove",
            (Self::Remove, true) => "Removing…",
        }
    }
}

fn description(status: Option<&LauncherStatus>) -> String {
    let Some(status) = status else {
        return "Checking…".to_string();
    };
    let command = status.command;
    match &status.state {
        LauncherState::Unavailable(reason) => (*reason).to_string(),
        LauncherState::Missing => format!(
            "Add {command} to {} so terminals and ACP clients such as Zed or Paseo \
             can run \u{201c}{command} acp\u{201d}.",
            status.display_dir
        ),
        LauncherState::Installed | LauncherState::InstalledByInstaller
            if status.on_path == Some(false) =>
        {
            format!(
                "Installed at {}, but {} is not on your shell\u{2019}s PATH. Add it to \
                 PATH to run {command} from a terminal.",
                status.display_path, status.display_dir
            )
        }
        LauncherState::Installed | LauncherState::InstalledByInstaller => format!(
            "{command} runs this app from {}. ACP clients can start it with \
             \u{201c}{command} acp\u{201d}.",
            status.display_path
        ),
        LauncherState::Stale => format!(
            "{} runs a different copy of Maple Work. Update it to run this one.",
            status.display_path
        ),
        LauncherState::Foreign => format!(
            "{} already exists and was not created by Maple Work. Remove or rename \
             it to install the command.",
            status.display_path
        ),
    }
}

impl SettingsScreen {
    pub(super) fn load_command_line(&self, cx: &mut Context<Self>) {
        self.call(
            async { Ok(crate::cli_launcher::load_status().await) },
            cx,
            |this, result: Result<LauncherStatus, String>, cx| {
                match result {
                    Ok(status) => this.command_line.status = Some(status),
                    Err(message) => this.command_line.error = Some(message),
                }
                if this.settings.application_vim_enabled {
                    this.reconcile_application_vim_target();
                }
                cx.notify();
            },
        );
    }

    /// The row's one button, if its state has something to do.
    pub(super) fn command_line_action(&self) -> Option<CommandLineAction> {
        match self.command_line.status.as_ref()?.state {
            LauncherState::Missing => Some(CommandLineAction::Install),
            LauncherState::Stale => Some(CommandLineAction::Update),
            LauncherState::Installed => Some(CommandLineAction::Remove),
            LauncherState::InstalledByInstaller
            | LauncherState::Foreign
            | LauncherState::Unavailable(_) => None,
        }
    }

    pub(super) fn run_command_line_action(&mut self, cx: &mut Context<Self>) {
        let Some(action) = self.command_line_action() else {
            return;
        };
        if self.command_line.busy {
            return;
        }
        self.command_line.busy = true;
        self.command_line.error = None;
        cx.notify();
        self.call(
            async move {
                match action {
                    CommandLineAction::Install | CommandLineAction::Update => {
                        crate::cli_launcher::install().await?
                    }
                    CommandLineAction::Remove => crate::cli_launcher::remove().await?,
                }
                Ok(crate::cli_launcher::load_status().await)
            },
            cx,
            |this, result: Result<LauncherStatus, String>, cx| {
                this.command_line.busy = false;
                match result {
                    Ok(status) => this.command_line.status = Some(status),
                    Err(message) => {
                        log::warn!("Command line launcher change failed");
                        this.command_line.error = Some(message);
                    }
                }
                if this.settings.application_vim_enabled {
                    this.reconcile_application_vim_target();
                }
                cx.notify();
            },
        );
    }

    pub(super) fn render_command_line_row(&self, cx: &mut Context<Self>) -> Div {
        let state = &self.command_line;
        let row = widgets::card_row()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(setting_copy("Command line tool", &description(state.status.as_ref())).flex_1())
            .when_some(self.command_line_action(), |row, action| {
                row.child(
                    self.application_target(
                        || SettingsTarget::General(GeneralTarget::CommandLine),
                        widgets::secondary_button("settings-command-line-action")
                            .py_1p5()
                            .flex_shrink_0()
                            .when(state.busy, |button| button.opacity(0.6))
                            .when(!state.busy, |button| {
                                button.on_click(cx.listener(|this, _event, _window, cx| {
                                    this.run_command_line_action(cx);
                                }))
                            })
                            .child(action.label(state.busy)),
                    ),
                )
            });
        div().flex().flex_col().gap_2().child(row).when_some(
            state.error.as_ref(),
            |column, error| {
                column.child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(theme::status_error()))
                        .child(error.clone()),
                )
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: LauncherState, on_path: Option<bool>) -> LauncherStatus {
        LauncherStatus {
            state,
            command: "maple-agent",
            display_path: "~/.local/bin/maple-agent".to_string(),
            display_dir: "~/.local/bin".to_string(),
            on_path,
        }
    }

    #[test]
    fn the_description_says_when_the_folder_is_off_path() {
        let off = description(Some(&status(LauncherState::Installed, Some(false))));
        assert!(off.contains("not on your shell\u{2019}s PATH"), "{off}");
        let on = description(Some(&status(LauncherState::Installed, Some(true))));
        assert!(!on.contains("PATH"), "{on}");
        let unknown = description(Some(&status(LauncherState::InstalledByInstaller, None)));
        assert!(!unknown.contains("PATH"), "{unknown}");
    }
}
