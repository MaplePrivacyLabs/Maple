//! Settings → Tools: a switch for each tool the execution computer's tasks
//! can offer the model. The host saves them for the account; a change
//! applies to new tasks and reaches open ones from their next request. The
//! host names each switch by id; its label and description are the UI's.
//! A paired computer too old for the page refuses with a code, which the
//! notice words in the interface's language.

use std::collections::HashSet;

use gpui::{Context, Div, SharedString, div, prelude::*};
use maple_agent::agent::AgentToolSwitch;

use super::{SettingsScreen, SettingsTarget, section_title, setting_copy};
use crate::ui::i18n::{UiText, tr};
use crate::ui::task::Failure;
use crate::ui::{theme, widgets};

/// Every switch: they tab in the order they are drawn.
const SWITCH_TAB_INDEX: isize = 15;

#[derive(Default)]
pub(super) struct ToolsState {
    /// The execution computer's switches; `None` until they load.
    pub(super) switches: Option<Vec<AgentToolSwitch>>,
    pub(super) notice: Option<UiText>,
    /// Switches whose change the host has not answered yet.
    pub(super) saving: HashSet<String>,
}

/// A switch's label and one-line description. A switch this build does not
/// know, from a newer host, shows its id alone.
fn tool_copy(id: &str) -> Option<(SharedString, SharedString)> {
    Some(match id {
        "read" => (
            tr("settings-tools-read"),
            tr("settings-tools-read-description"),
        ),
        "shell" => (
            tr("settings-tools-shell"),
            tr("settings-tools-shell-description"),
        ),
        "edit" => (
            tr("settings-tools-edit"),
            tr("settings-tools-edit-description"),
        ),
        "write" => (
            tr("settings-tools-write"),
            tr("settings-tools-write-description"),
        ),
        "grep" => (
            tr("settings-tools-grep"),
            tr("settings-tools-grep-description"),
        ),
        "find" => (
            tr("settings-tools-find"),
            tr("settings-tools-find-description"),
        ),
        "ls" => (tr("settings-tools-ls"), tr("settings-tools-ls-description")),
        "read_image" => (
            tr("settings-tools-read-image"),
            tr("settings-tools-read-image-description"),
        ),
        "todo_write" => (
            tr("settings-tools-todo-write"),
            tr("settings-tools-todo-write-description"),
        ),
        "request_user_input" => (
            tr("settings-tools-request-user-input"),
            tr("settings-tools-request-user-input-description"),
        ),
        "python" => (
            tr("settings-tools-python"),
            tr("settings-tools-python-description"),
        ),
        _ => return None,
    })
}

impl SettingsScreen {
    pub(super) fn load_tools(&self, cx: &mut Context<Self>) {
        let backend = self.integrations_backend.clone();
        let user_id = self.user_id.clone();
        self.call_presented(
            async move { backend.call_presented("list_tools", (&user_id,)).await },
            cx,
            |this, result, cx| this.finish_loading_tools(result, cx),
        );
    }

    /// Show the host's switches, or, in place of them, why there are none.
    fn finish_loading_tools(
        &mut self,
        result: Result<Vec<AgentToolSwitch>, Failure>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(switches) => {
                self.tools.switches = Some(switches);
                self.tools.notice = None;
            }
            Err(failure) => {
                self.tools.switches = Some(Vec::new());
                self.tools.notice = Some(failure.into());
            }
        }
        if self.settings.application_vim_enabled {
            self.reconcile_application_vim_target();
        }
        cx.notify();
    }

    /// The switches the page shows: a tool this computer cannot offer, such
    /// as `python` without its bundled CPython, has no row.
    fn visible_tools(&self) -> impl Iterator<Item = &AgentToolSwitch> {
        self.tools
            .switches
            .iter()
            .flatten()
            .filter(|switch| switch.available)
    }

    pub(super) fn tool_targets(&self) -> Vec<SettingsTarget> {
        self.visible_tools()
            .map(|switch| SettingsTarget::Tool(switch.id.clone()))
            .collect()
    }

    pub(super) fn toggle_tool(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.tools.saving.contains(id) {
            return;
        }
        let Some(enabled) = self
            .visible_tools()
            .find(|switch| switch.id == id)
            .map(|switch| !switch.enabled)
        else {
            return;
        };
        let id = id.to_string();
        self.tools.saving.insert(id.clone());
        self.tools.notice = None;
        cx.notify();

        let backend = self.integrations_backend.clone();
        let user_id = self.user_id.clone();
        let request_id = id.clone();
        self.call_presented(
            async move {
                backend
                    .call_presented("set_tool_enabled", (&user_id, &request_id, enabled))
                    .await
            },
            cx,
            move |this, result, cx| this.finish_tool_change(&id, result, cx),
        );
    }

    /// Show the host's switches after a change. A refused change leaves the
    /// switch as it was and says why.
    fn finish_tool_change(
        &mut self,
        id: &str,
        result: Result<Vec<AgentToolSwitch>, Failure>,
        cx: &mut Context<Self>,
    ) {
        self.tools.saving.remove(id);
        match result {
            Ok(switches) => self.tools.switches = Some(switches),
            Err(failure) => self.tools.notice = Some(failure.into()),
        }
        if self.settings.application_vim_enabled {
            self.reconcile_application_vim_target();
        }
        cx.notify();
    }

    fn render_tool_row(
        &self,
        switch: &AgentToolSwitch,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (label, description) = tool_copy(&switch.id)
            .unwrap_or_else(|| (switch.id.clone().into(), SharedString::default()));
        let saving = self.tools.saving.contains(&switch.id);
        let toggle = {
            let id = switch.id.clone();
            move |this: &mut Self, cx: &mut Context<Self>| this.toggle_tool(&id, cx)
        };
        let control = widgets::switch(
            SharedString::from(format!("settings-tool-switch-{}", switch.id)),
            switch.enabled,
        )
        .aria_label(label.clone())
        .when(!description.is_empty(), |control| {
            control.aria_description(description.clone())
        })
        .tab_index(SWITCH_TAB_INDEX)
        .on_key_down(Self::on_press_key(toggle.clone(), cx))
        .when(saving, |control| control.opacity(0.5));
        // The whole row is the click target, as on the General page.
        let row = div()
            .id(SharedString::from(format!("settings-tool-{}", switch.id)))
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _event, _window, cx| toggle(this, cx)))
            .child(setting_copy(&label, &description))
            .child(control);
        self.application_target(|| SettingsTarget::Tool(switch.id.clone()), row)
    }

    pub(super) fn render_tools_pane(&self, cx: &mut Context<Self>) -> Div {
        let muted =
            |text: SharedString| div().text_color(gpui::rgb(theme::text_muted())).child(text);
        let mut pane = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(section_title(&tr("settings-section-tools")))
            .child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(if self.integrations_backend.remote_host().is_some() {
                        tr("settings-tools-intro-remote")
                    } else {
                        tr("settings-tools-intro")
                    }),
            );
        if let Some(notice) = &self.tools.notice {
            pane = pane.child(widgets::notice(
                "tools-notice-close",
                notice.resolve(),
                cx.listener(|this, _event, _window, cx| {
                    this.tools.notice = None;
                    cx.notify();
                }),
            ));
        }
        if self.tools.switches.is_none() {
            pane = pane.child(muted(tr("settings-tools-loading")).text_sm());
        }
        let rows: Vec<gpui::AnyElement> = self
            .visible_tools()
            .map(|switch| self.render_tool_row(switch, cx))
            .collect();
        if !rows.is_empty() {
            pane = pane.child(widgets::card_stack(rows));
        }
        pane.child(muted(tr("settings-tools-elsewhere")).text_xs())
    }
}

#[cfg(test)]
mod tests {
    use super::super::Section;
    use super::*;
    use gpui::{AppContext, TestAppContext};

    fn screen(cx: &mut TestAppContext) -> gpui::Entity<SettingsScreen> {
        cx.executor().allow_parking();
        let backend = std::sync::Arc::new(
            crate::backend::AgentBackend::for_test("http://127.0.0.1:9".into(), String::new())
                .expect("test backend"),
        );
        cx.new(|cx| {
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
                Section::Tools,
                cx,
            )
        })
    }

    /// Wait for the host to answer the page's calls.
    fn settle(
        cx: &mut TestAppContext,
        screen: &gpui::Entity<SettingsScreen>,
        ready: impl Fn(&SettingsScreen) -> bool,
    ) {
        for _ in 0..500 {
            cx.run_until_parked();
            if screen.read_with(cx, |screen, _| ready(screen)) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the host did not answer");
    }

    fn enabled(screen: &SettingsScreen) -> Vec<&str> {
        screen
            .visible_tools()
            .filter(|switch| switch.enabled)
            .map(|switch| switch.id.as_str())
            .collect()
    }

    #[gpui::test]
    fn tools_sit_beside_integrations_and_list_every_switch_with_its_copy(cx: &mut TestAppContext) {
        let screen = screen(cx);
        settle(cx, &screen, |screen| screen.tools.switches.is_some());
        screen.update(cx, |screen, _| {
            let sections = screen.visible_sections();
            let integrations = sections
                .iter()
                .position(|section| *section == Section::Integrations)
                .unwrap();
            assert_eq!(sections.get(integrations + 1), Some(&Section::Tools));
            assert_eq!(Section::Tools.id(), "Tools");
            assert_eq!(Section::Tools.label().as_ref(), "Tools");

            assert_eq!(screen.tools.notice, None);
            let switches = screen.tools.switches.as_deref().unwrap();
            for switch in switches {
                assert!(tool_copy(&switch.id).is_some(), "{} has no copy", switch.id);
            }
            // This test binary bundles no CPython, so `python` has no row.
            let python = switches.iter().find(|switch| switch.id == "python");
            assert!(python.is_some_and(|python| !python.available));
            assert!(screen.visible_tools().all(|switch| switch.id != "python"));
            assert_eq!(
                enabled(screen),
                [
                    "read",
                    "shell",
                    "edit",
                    "write",
                    "read_image",
                    "todo_write",
                    "request_user_input"
                ]
            );
        });
    }

    #[gpui::test]
    fn a_switch_saves_through_the_host(cx: &mut TestAppContext) {
        let screen = screen(cx);
        settle(cx, &screen, |screen| screen.tools.switches.is_some());
        for expected in [true, false] {
            screen.update(cx, |screen, cx| {
                screen.toggle_tool("grep", cx);
                assert!(screen.tools.saving.contains("grep"));
                // A second press waits for the first to land.
                screen.toggle_tool("grep", cx);
            });
            settle(cx, &screen, |screen| screen.tools.saving.is_empty());
            screen.update(cx, |screen, _| {
                assert_eq!(screen.tools.notice, None);
                assert_eq!(enabled(screen).contains(&"grep"), expected);
            });
        }

        // A tool the computer cannot offer has no row to switch.
        screen.update(cx, |screen, cx| {
            screen.toggle_tool("python", cx);
            assert!(screen.tools.saving.is_empty());
        });
    }

    /// A paired computer released before this page refuses its calls with a
    /// code. The notice reads in the interface's language: in place of the
    /// switches when they cannot be listed, and with the switch left as it
    /// was when a change is refused.
    #[gpui::test]
    fn an_older_paired_computer_asks_to_be_updated_in_the_interface_language(
        cx: &mut TestAppContext,
    ) {
        let _locale = crate::ui::i18n::TestLocalizationGuard::enable();
        let screen = screen(cx);
        settle(cx, &screen, |screen| screen.tools.switches.is_some());
        let refusal = |code: &str| {
            Failure::Host(crate::backend::HostFailure::new(
                code,
                "English diagnostic from the bridge",
            ))
        };
        let unsupported = [
            (
                "en",
                "Update Maple on the remote computer to choose its tools from here.",
            ),
            (
                "es",
                "Actualiza Maple en la computadora remota para elegir sus herramientas desde aquí.",
            ),
        ];
        let unconfirmed = [
            (
                "en",
                "Could not check the remote computer's tools. Try again, or update Maple there.",
            ),
            (
                "es",
                "No se pudieron comprobar las herramientas de la computadora remota. Inténtalo de nuevo o actualiza Maple ahí.",
            ),
        ];
        let reads = |screen: &SettingsScreen, expected: [(&str, &str); 2]| {
            let reads = expected.into_iter().all(|(locale, text)| {
                crate::ui::i18n::apply_preference(Some(locale));
                screen.tools.notice.as_ref().map(UiText::resolve).as_deref() == Some(text)
            });
            crate::ui::i18n::apply_preference(Some("en"));
            reads
        };

        screen.update(cx, |screen, cx| {
            screen.tools.saving.insert("grep".into());
            screen.finish_tool_change("grep", Err(refusal("remote-tools-unsupported")), cx);
            assert!(screen.tools.saving.is_empty());
            assert!(!enabled(screen).contains(&"grep"), "the switch stays off");
            assert!(reads(screen, unsupported));

            for (code, expected) in [
                ("remote-tools-unsupported", unsupported),
                ("remote-tools-unconfirmed", unconfirmed),
            ] {
                screen.finish_loading_tools(Err(refusal(code)), cx);
                assert!(screen.tools.switches.as_ref().is_some_and(Vec::is_empty));
                assert!(screen.tool_targets().is_empty(), "no rows, only the notice");
                assert!(reads(screen, expected), "{code}");
            }
        });
    }
}
