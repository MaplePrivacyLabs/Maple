//! How to run Maple as an agent inside an editor. Editors start `maple-agent
//! acp` themselves and share this computer's host, so nothing here is saved:
//! the pane only shows what to paste for this installation.

use std::path::{Path, PathBuf};

use gpui::{Context, Div, SharedString, div, prelude::*};

use super::{SettingsScreen, SettingsTarget, activates, section_title, setting_copy};
use crate::ui::{theme, widgets};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EditorsTarget {
    Zed,
    JetBrains,
    Command,
}

pub(super) struct EditorsUiState {
    zed: SharedString,
    jetbrains: SharedString,
    command: SharedString,
    temporary_location: bool,
}

impl EditorsUiState {
    pub(super) fn new() -> Self {
        let executable = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("maple-agent"));
        let launcher = stable_launcher(&executable, dirs::home_dir().as_deref(), |path| {
            path.exists()
        });
        Self {
            temporary_location: temporary_location(&launcher),
            ..Self::for_launcher(&launcher, crate::profile::DISPLAY_NAME, cfg!(windows))
        }
    }

    fn for_launcher(launcher: &Path, name: &str, windows: bool) -> Self {
        let launcher = launcher.to_string_lossy();
        Self {
            zed: zed_entry(name, &launcher).into(),
            jetbrains: jetbrains_entry(name, &launcher).into(),
            command: shell_command(&launcher, windows).into(),
            temporary_location: false,
        }
    }

    fn text(&self, target: EditorsTarget) -> SharedString {
        match target {
            EditorsTarget::Zed => self.zed.clone(),
            EditorsTarget::JetBrains => self.jetbrains.clone(),
            EditorsTarget::Command => self.command.clone(),
        }
    }
}

impl SettingsScreen {
    pub(super) fn editors_targets(&self) -> Vec<SettingsTarget> {
        [
            EditorsTarget::Zed,
            EditorsTarget::JetBrains,
            EditorsTarget::Command,
        ]
        .map(SettingsTarget::Editors)
        .to_vec()
    }

    pub(super) fn copy_editors_text(&self, target: EditorsTarget, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
            self.editors.text(target).to_string(),
        ));
    }

    fn render_editors_card(
        &self,
        title: &str,
        description: &str,
        target: EditorsTarget,
        cx: &mut Context<Self>,
    ) -> Div {
        let (id, label, tab_index) = match target {
            EditorsTarget::Zed => ("editors-copy-zed", "Copy Zed configuration", 17),
            EditorsTarget::JetBrains => {
                ("editors-copy-jetbrains", "Copy JetBrains configuration", 18)
            }
            EditorsTarget::Command => ("editors-copy-command", "Copy command", 19),
        };
        let text = self.editors.text(target);
        widgets::card_row()
            .flex()
            .flex_col()
            .gap_3()
            .child(setting_copy(title, description))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1p5()
                            .rounded(theme::RADIUS_SM)
                            .bg(gpui::rgb(theme::bg_code_block()))
                            .text_xs()
                            .font_family(crate::assets::FONT_MONO)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(text.clone()),
                    )
                    .child(
                        self.application_target(
                            || SettingsTarget::Editors(target),
                            widgets::copy_button(id, text, None, Some(cx.entity_id()))
                                .aria_label(label)
                                .tab_index(tab_index)
                                .on_key_down(cx.listener(
                                    move |this, event: &gpui::KeyDownEvent, _, cx| {
                                        if activates(event) {
                                            cx.stop_propagation();
                                            this.copy_editors_text(target, cx);
                                        }
                                    },
                                )),
                        ),
                    ),
            )
    }

    pub(super) fn render_editors_pane(&self, cx: &mut Context<Self>) -> Div {
        let name = crate::profile::DISPLAY_NAME;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(section_title("Editors"))
            .child(setting_copy(
                "Use Maple in your editor",
                "Editors that support the Agent Client Protocol (ACP) can run Maple as their AI agent. They use this computer's sign-in, tasks and history, and start Maple themselves, so this window doesn't need to be open.",
            ))
            .when(self.editors.temporary_location, |pane| {
                pane.child(widgets::banner(theme::status_warning()).child(format!(
                    "{name} is running from a temporary location. Move it to your Applications folder and reopen it before setting up an editor."
                )))
            })
            .child(self.render_editors_card(
                "Zed",
                &format!(
                    "Open Agent Settings, go to External Agents, click Add Agent and choose Add Custom Agent. In the file Zed opens, replace the example under agent_servers with this, then start a new thread with {name}."
                ),
                EditorsTarget::Zed,
                cx,
            ))
            .child(self.render_editors_card(
                "JetBrains IDEs",
                &format!(
                    "In the AI Chat tool window, open the … menu and choose Add Custom Agent. Add this under agent_servers in the file that opens, then select {name} in AI Chat."
                ),
                EditorsTarget::JetBrains,
                cx,
            ))
            .child(self.render_editors_card(
                "Other editors",
                "Point any other ACP client at this command. It talks to the editor over standard input and output.",
                EditorsTarget::Command,
                cx,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child("Maple works in the folder your editor has open. The first time you use a folder, your editor asks whether to trust it.")
                    .child("As in this app, Maple runs tools without asking each time.")
                    .child("Computer use and handing work to Codex or Claude Code only work in this app.")
                    .child("If your editor can't start Maple, open this app and check that you're signed in."),
            )
    }
}

/// The path to give an editor, which keeps working after an update. A packaged
/// app does not always run from the path its users should start it by.
fn stable_launcher(
    executable: &Path,
    home: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    let launcher = windows_launcher(executable)
        .filter(|launcher| exists(launcher))
        .or_else(|| linux_launcher(executable, home, &exists))
        .unwrap_or_else(|| executable.to_path_buf());
    without_verbatim_prefix(launcher)
}

/// Windows runs from `versions/<generation>/`, which an update replaces. The
/// launcher in the install root stays put and forwards arguments and stdio.
fn windows_launcher(executable: &Path) -> Option<PathBuf> {
    let versions = executable.parent()?.parent()?;
    if versions.file_name()? != "versions" {
        return None;
    }
    Some(versions.parent()?.join(executable.file_name()?))
}

/// Linux starts a bundle's `usr/bin/maple-agent` through its `AppRun`, which
/// the installer also exposes as `~/.local/bin/<command>`.
fn linux_launcher(
    executable: &Path,
    home: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if !executable.ends_with("usr/bin/maple-agent") {
        return None;
    }
    let bundle = executable.parent()?.parent()?.parent()?;
    let app_run = bundle.join("AppRun");
    if !exists(&app_run) {
        return None;
    }
    let wrapper = home
        .map(|home| home.join(".local"))
        .filter(|local| bundle.parent() == Some(local))
        .and_then(|local| Some(local.join("bin").join(bundle.file_stem()?)))
        .filter(|wrapper| exists(wrapper));
    Some(wrapper.unwrap_or(app_run))
}

/// The Windows bootstrap starts a canonicalized path, which carries a `\\?\`
/// prefix that some editors cannot launch. A `\\?\UNC\` path keeps it.
fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    path.to_str()
        .and_then(|text| text.strip_prefix(r"\\?\"))
        .filter(|text| !text.starts_with(r"UNC\"))
        .map(PathBuf::from)
        .unwrap_or(path)
}

#[cfg(target_os = "macos")]
fn temporary_location(launcher: &Path) -> bool {
    crate::update::manual_macos_path(launcher)
}

#[cfg(not(target_os = "macos"))]
fn temporary_location(_launcher: &Path) -> bool {
    false
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("a string encodes as JSON")
}

fn zed_entry(name: &str, launcher: &str) -> String {
    format!(
        "{}: {{\n  \"type\": \"custom\",\n  \"command\": {},\n  \"args\": [\"acp\"],\n  \"env\": {{}}\n}}",
        json_string(name),
        json_string(launcher),
    )
}

fn jetbrains_entry(name: &str, launcher: &str) -> String {
    format!(
        "{}: {{\n  \"command\": {},\n  \"args\": [\"acp\"]\n}}",
        json_string(name),
        json_string(launcher),
    )
}

fn shell_command(launcher: &str, windows: bool) -> String {
    if windows {
        format!("\"{launcher}\" acp")
    } else {
        format!("'{}' acp", launcher.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn launcher(executable: &str, home: &str, existing: &[&str]) -> PathBuf {
        stable_launcher(Path::new(executable), Some(Path::new(home)), |path| {
            existing.iter().any(|existing| path == Path::new(existing))
        })
    }

    #[test]
    fn macos_and_unpackaged_builds_use_the_running_executable() {
        let app = "/Applications/Maple Agent.app/Contents/MacOS/maple-agent";
        assert_eq!(launcher(app, "/Users/me", &[app]), Path::new(app));
        let debug = "/work/target/debug/maple-agent";
        assert_eq!(launcher(debug, "/Users/me", &[debug]), Path::new(debug));
    }

    #[test]
    fn windows_uses_the_stable_launcher_rather_than_a_generation() {
        let generation = "/Programs/Maple Work/versions/1.2.3-4/maple-agent.exe";
        let stable = "/Programs/Maple Work/maple-agent.exe";
        assert_eq!(
            launcher(generation, "/Users/me", &[stable]),
            Path::new(stable)
        );
        assert_eq!(
            launcher(generation, "/Users/me", &[]),
            Path::new(generation)
        );
    }

    #[test]
    fn linux_prefers_the_installed_command_and_falls_back_to_the_bundle() {
        for (bundle, wrapper) in [
            (
                "/home/me/.local/maple-agent.app",
                "/home/me/.local/bin/maple-agent",
            ),
            (
                "/home/me/.local/maple-agent-dev.app",
                "/home/me/.local/bin/maple-agent-dev",
            ),
        ] {
            let executable = format!("{bundle}/usr/bin/maple-agent");
            let app_run = format!("{bundle}/AppRun");
            assert_eq!(
                launcher(&executable, "/home/me", &[&app_run, wrapper]),
                Path::new(wrapper)
            );
            assert_eq!(
                launcher(&executable, "/home/me", &[&app_run]),
                Path::new(&app_run)
            );
        }
        assert_eq!(
            launcher(
                "/tmp/maple-agent.app/usr/bin/maple-agent",
                "/home/me",
                &[
                    "/tmp/maple-agent.app/AppRun",
                    "/home/me/.local/bin/maple-agent"
                ]
            ),
            Path::new("/tmp/maple-agent.app/AppRun")
        );
    }

    #[test]
    fn verbatim_prefixes_are_removed_except_from_network_paths() {
        assert_eq!(
            without_verbatim_prefix(PathBuf::from(r"\\?\C:\Maple Work\maple-agent.exe")),
            PathBuf::from(r"C:\Maple Work\maple-agent.exe")
        );
        let network = PathBuf::from(r"\\?\UNC\server\share\maple-agent.exe");
        assert_eq!(without_verbatim_prefix(network.clone()), network);
    }

    fn entry(snippet: &SharedString) -> Value {
        let parsed: Value = serde_json::from_str(&format!("{{{snippet}}}")).unwrap();
        parsed["Maple Work"].clone()
    }

    #[test]
    fn configuration_entries_are_valid_json_for_any_install_path() {
        for path in [
            "/Applications/Maple Agent.app/Contents/MacOS/maple-agent",
            r"C:\Users\me\AppData\Local\Programs\Maple Work\maple-agent.exe",
        ] {
            let state = EditorsUiState::for_launcher(Path::new(path), "Maple Work", false);
            assert_eq!(
                entry(&state.zed),
                json!({"type": "custom", "command": path, "args": ["acp"], "env": {}})
            );
            assert_eq!(
                entry(&state.jetbrains),
                json!({"command": path, "args": ["acp"]})
            );
        }
    }

    #[gpui::test]
    fn every_copy_target_writes_what_its_card_shows(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let backend = std::sync::Arc::new(
            crate::backend::AgentBackend::for_test("http://127.0.0.1:1".into(), String::new())
                .unwrap(),
        );
        let settings = cx.new(|cx| {
            SettingsScreen::new(
                backend.clone(),
                backend,
                "user".into(),
                crate::settings::AppSettings::default(),
                crate::shortcuts::ShortcutSnapshot {
                    generation: 1,
                    rows: Vec::new(),
                    last_error: None,
                    compatibility_warning: None,
                },
                super::super::Section::Editors,
                cx,
            )
        });
        settings.update(cx, |settings, cx| {
            let targets = settings.editors_targets();
            assert_eq!(targets.len(), 3);
            for target in targets {
                let SettingsTarget::Editors(target) = target else {
                    unreachable!("the Editors pane offers only its own targets");
                };
                settings.copy_editors_text(target, cx);
                assert_eq!(
                    cx.read_from_clipboard().and_then(|item| item.text()),
                    Some(settings.editors.text(target).to_string())
                );
            }
        });
    }

    #[test]
    fn the_command_is_quoted_for_each_platform() {
        assert_eq!(
            shell_command("/Applications/Maple Agent.app/maple-agent", false),
            "'/Applications/Maple Agent.app/maple-agent' acp"
        );
        assert_eq!(
            shell_command("/home/o'neil/maple-agent", false),
            r"'/home/o'\''neil/maple-agent' acp"
        );
        assert_eq!(
            shell_command(r"C:\Maple Work\maple-agent.exe", true),
            r#""C:\Maple Work\maple-agent.exe" acp"#
        );
    }
}
