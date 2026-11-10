//! How to run Maple as an ACP agent inside another app. Clients start
//! `maple-agent acp` themselves and share this computer's host, so nothing here
//! is saved: the pane only shows the command for this installation and examples
//! of where to put it.

use std::path::{Path, PathBuf};

use crate::ui::i18n::{MessageCache, tr};
use gpui::{Context, Div, SharedString, div, prelude::*};

use super::{SettingsScreen, SettingsTarget, activates, setting_copy, subsection_title};
use crate::ui::{theme, widgets};

const COMMAND_TAB_INDEX: isize = 17;
const EXAMPLE_TAB_INDEX: isize = 18;
const EXAMPLE_COPY_TAB_INDEX: isize = EXAMPLE_TAB_INDEX + AcpExample::ALL.len() as isize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AcpExample {
    Zed,
    JetBrains,
    Paseo,
    T3Code,
}

impl AcpExample {
    const ALL: [Self; 4] = [Self::Zed, Self::JetBrains, Self::Paseo, Self::T3Code];

    fn label(self) -> &'static str {
        match self {
            Self::Zed => "Zed",
            Self::JetBrains => "JetBrains",
            Self::Paseo => "Paseo",
            Self::T3Code => "T3 Code",
        }
    }

    fn copy_id(self) -> &'static str {
        match self {
            Self::Zed => "acp-copy-zed",
            Self::JetBrains => "acp-copy-jetbrains",
            Self::Paseo => "acp-copy-paseo",
            Self::T3Code => "acp-copy-t3-code",
        }
    }

    fn copy_label(self) -> SharedString {
        match self {
            Self::T3Code => tr("settings-acp-copy-path"),
            Self::Zed | Self::JetBrains | Self::Paseo => tr("settings-acp-copy-config"),
        }
    }

    fn steps(self, name: &str, messages: &MessageCache) -> [String; 3] {
        match self {
            Self::Zed => [
                tr("settings-acp-zed-open").to_string(),
                tr("settings-acp-zed-config").to_string(),
                messages
                    .get("settings-acp-zed-start", [("name", name.to_owned().into())])
                    .to_string(),
            ],
            Self::JetBrains => [
                tr("settings-acp-jb-open").to_string(),
                tr("settings-acp-jb-config").to_string(),
                messages
                    .get("settings-acp-jb-select", [("name", name.to_owned().into())])
                    .to_string(),
            ],
            Self::Paseo => [
                tr("settings-acp-paseo-open").to_string(),
                tr("settings-acp-paseo-config").to_string(),
                messages
                    .get(
                        "settings-acp-paseo-start",
                        [("name", name.to_owned().into())],
                    )
                    .to_string(),
            ],
            Self::T3Code => [
                tr("settings-acp-t3-open").to_string(),
                tr("settings-acp-t3-config").to_string(),
                messages
                    .get("settings-acp-t3-start", [("name", name.to_owned().into())])
                    .to_string(),
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AcpTarget {
    Command,
    Example(AcpExample),
    CopyExample,
}

pub(super) struct AcpUiState {
    selected: AcpExample,
    command: SharedString,
    zed: SharedString,
    jetbrains: SharedString,
    paseo: SharedString,
    executable: SharedString,
    temporary_location: bool,
}

impl AcpUiState {
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
            selected: AcpExample::Zed,
            command: shell_command(&launcher, windows).into(),
            zed: zed_entry(name, &launcher).into(),
            jetbrains: jetbrains_entry(name, &launcher).into(),
            paseo: paseo_entry(name, &launcher).into(),
            executable: launcher.into_owned().into(),
            temporary_location: false,
        }
    }

    fn example_text(&self, example: AcpExample) -> SharedString {
        match example {
            AcpExample::Zed => self.zed.clone(),
            AcpExample::JetBrains => self.jetbrains.clone(),
            AcpExample::Paseo => self.paseo.clone(),
            AcpExample::T3Code => self.executable.clone(),
        }
    }
}

impl SettingsScreen {
    pub(super) fn acp_targets(&self) -> Vec<SettingsTarget> {
        std::iter::once(AcpTarget::Command)
            .chain(AcpExample::ALL.map(AcpTarget::Example))
            .chain(std::iter::once(AcpTarget::CopyExample))
            .map(SettingsTarget::Acp)
            .collect()
    }

    pub(super) fn activate_acp_target(&mut self, target: AcpTarget, cx: &mut Context<Self>) {
        let text = match target {
            AcpTarget::Command => self.acp.command.clone(),
            AcpTarget::CopyExample => self.acp.example_text(self.acp.selected),
            AcpTarget::Example(example) => {
                self.select_acp_example(example, cx);
                return;
            }
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_string()));
    }

    fn select_acp_example(&mut self, example: AcpExample, cx: &mut Context<Self>) {
        self.acp.selected = example;
        cx.notify();
    }

    fn render_acp_snippet(
        &self,
        id: &'static str,
        label: SharedString,
        text: SharedString,
        target: AcpTarget,
        tab_index: isize,
        cx: &mut Context<Self>,
    ) -> Div {
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
                    || SettingsTarget::Acp(target),
                    widgets::copy_button(id, text, None, Some(cx.entity_id()))
                        .aria_label(label)
                        .tab_index(tab_index)
                        .on_key_down(cx.listener(
                            move |this, event: &gpui::KeyDownEvent, _, cx| {
                                if activates(event) {
                                    cx.stop_propagation();
                                    this.activate_acp_target(target, cx);
                                }
                            },
                        )),
                ),
            )
    }

    fn render_acp_example_tab(
        &self,
        example: AcpExample,
        tab_index: isize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let selected = self.acp.selected == example;
        self.application_target(
            || SettingsTarget::Acp(AcpTarget::Example(example)),
            div()
                .id(SharedString::from(format!(
                    "acp-example-{}",
                    example.label()
                )))
                .role(gpui::Role::Tab)
                .aria_label(example.label())
                .aria_selected(selected)
                .tab_index(tab_index)
                .px_3()
                .py_1()
                .rounded(theme::RADIUS_SM)
                .text_sm()
                .text_color(gpui::rgb(if selected {
                    theme::text_primary()
                } else {
                    theme::text_secondary()
                }))
                .when(selected, |tab| tab.bg(gpui::rgb(theme::bg_sidebar_pill())))
                .focus_visible(widgets::focus_ring)
                .hover(|style| style.bg(theme::overlay_hover()).cursor_pointer())
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.select_acp_example(example, cx);
                }))
                .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                    if activates(event) {
                        cx.stop_propagation();
                        this.select_acp_example(example, cx);
                    }
                }))
                .child(example.label()),
        )
    }

    fn render_acp_examples(&self, cx: &mut Context<Self>) -> Div {
        let example = self.acp.selected;
        widgets::card_row()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .id("acp-examples")
                    .role(gpui::Role::TabList)
                    .aria_label(tr("settings-acp-examples"))
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .children(AcpExample::ALL.into_iter().zip(EXAMPLE_TAB_INDEX..).map(
                        |(example, tab_index)| self.render_acp_example_tab(example, tab_index, cx),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .children(
                        example
                            .steps(crate::profile::DISPLAY_NAME, &self.messages)
                            .into_iter()
                            .zip(1..)
                            .map(|(step, number)| format!("{number}. {step}")),
                    ),
            )
            .child(self.render_acp_snippet(
                example.copy_id(),
                example.copy_label(),
                self.acp.example_text(example),
                AcpTarget::CopyExample,
                EXAMPLE_COPY_TAB_INDEX,
                cx,
            ))
    }

    pub(super) fn render_acp_pane(&self, cx: &mut Context<Self>) -> Div {
        let name = crate::profile::DISPLAY_NAME;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(tr("settings-acp-intro")),
            )
            .when(self.acp.temporary_location, |pane| {
                pane.child(widgets::banner(
                    widgets::Tone::Warning,
                    self.messages
                        .get("settings-acp-temporary", [("name", name.into())]),
                ))
            })
            .child(
                widgets::card_row()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(setting_copy(
                        tr("settings-acp-command"),
                        tr("settings-acp-command-hint"),
                    ))
                    .child(self.render_acp_snippet(
                        "acp-copy-command",
                        tr("settings-acp-copy-command"),
                        self.acp.command.clone(),
                        AcpTarget::Command,
                        COMMAND_TAB_INDEX,
                        cx,
                    )),
            )
            .child(subsection_title(&tr("settings-acp-examples-heading")))
            .child(self.render_acp_examples(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(tr("settings-acp-folder"))
                    .child(tr("settings-acp-tools"))
                    .child(tr("settings-acp-desktop-only"))
                    .child(tr("settings-acp-troubleshoot")),
            )
    }
}

/// The path to give a client, which keeps working after an update. A packaged
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
/// prefix that some clients cannot launch. A `\\?\UNC\` path keeps it.
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

fn paseo_entry(name: &str, launcher: &str) -> String {
    format!(
        "\"maple-work\": {{\n  \"extends\": \"acp\",\n  \"label\": {},\n  \"command\": [{}, \"acp\"]\n}}",
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

    fn entry(snippet: &SharedString, key: &str) -> Value {
        let parsed: Value = serde_json::from_str(&format!("{{{snippet}}}")).unwrap();
        parsed[key].clone()
    }

    #[test]
    fn examples_are_valid_for_any_install_path() {
        for path in [
            "/Applications/Maple Agent.app/Contents/MacOS/maple-agent",
            r"C:\Users\me\AppData\Local\Programs\Maple Work\maple-agent.exe",
        ] {
            let state = AcpUiState::for_launcher(Path::new(path), "Maple Work", false);
            assert_eq!(
                entry(&state.zed, "Maple Work"),
                json!({"type": "custom", "command": path, "args": ["acp"], "env": {}})
            );
            assert_eq!(
                entry(&state.jetbrains, "Maple Work"),
                json!({"command": path, "args": ["acp"]})
            );
            assert_eq!(
                entry(&state.paseo, "maple-work"),
                json!({"extends": "acp", "label": "Maple Work", "command": [path, "acp"]})
            );
            assert_eq!(state.executable, path);
        }
    }

    #[gpui::test]
    fn each_target_copies_what_it_shows_and_tabs_switch_the_example(cx: &mut gpui::TestAppContext) {
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
                super::super::Section::Api,
                cx,
            )
        });
        settings.update(cx, |settings, cx| {
            settings.select_developer_tab(super::super::DeveloperTab::Acp, cx);
            assert_eq!(settings.acp_targets().len(), 2 + AcpExample::ALL.len());
            settings.activate_acp_target(AcpTarget::Command, cx);
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some(settings.acp.command.to_string())
            );
            for example in AcpExample::ALL {
                settings.activate_acp_target(AcpTarget::Example(example), cx);
                assert_eq!(settings.acp.selected, example);
                settings.activate_acp_target(AcpTarget::CopyExample, cx);
                assert_eq!(
                    cx.read_from_clipboard().and_then(|item| item.text()),
                    Some(settings.acp.example_text(example).to_string())
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
