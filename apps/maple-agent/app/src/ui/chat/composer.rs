//! The chat pane chrome: the header and its menus, the slash palette,
//! the plan and side-question cards, and the composer itself.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

// Literal command token; never translate the text entered into the composer.
const BTW_COMMAND: &str = "btw";

use gpui::{Context, Div, IntoElement, SharedString, Window, div, prelude::*, px};
use maple_agent::agent::{AgentPermissionMode, AgentSlashCommand, SideQuestionTurn};

use super::cache::MarkdownKind;
use super::commands::ChatCommand;
use super::computers::{Destination, Navigate};
use super::thinking::thinking_label;
use super::transcript::{render_plan_row, render_subagent_row};
use super::{
    COMPOSER_PLACEHOLDER, ChatPopup, ChatScreen, DraftAttachment, OpenSettingsSection,
    ProjectComputer, SIDE_THREAD_PLACEHOLDER, Section,
};
use crate::ui::i18n::{UiText, tr};
use crate::ui::icons::{icon, spinner};
use crate::ui::markdown;
use crate::ui::motion;
use crate::ui::popup::{Menu, MenuItem, Placement};
use crate::ui::text_input::vim::VimMode;
use crate::ui::theme;
use crate::ui::titlebar;
use crate::ui::widgets;

/// The composer's button row, left to right. The row renders from this
/// list and application Vim walks it, so a new control is reachable from
/// the keyboard once it has an entry here, a rendering, and an action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ComposerControl {
    Model,
    Integrations,
    Mode,
    Attachments,
    Dictate,
}

impl ComposerControl {
    const ALL: [Self; 5] = [
        Self::Model,
        Self::Integrations,
        Self::Mode,
        Self::Attachments,
        Self::Dictate,
    ];
}

fn mode_item_id(mode: AgentPermissionMode) -> SharedString {
    SharedString::from(format!("mode-{}", mode.as_str()))
}

fn mode_icon(mode: AgentPermissionMode) -> &'static str {
    match mode {
        AgentPermissionMode::ReadOnly => "lock",
        AgentPermissionMode::Auto => "shield-check",
        AgentPermissionMode::FullAccess => "zap",
    }
}

fn mode_label(mode: AgentPermissionMode) -> &'static str {
    match mode {
        AgentPermissionMode::ReadOnly => "chat-mode-read-only",
        AgentPermissionMode::Auto => "chat-mode-auto",
        AgentPermissionMode::FullAccess => "chat-mode-full-access",
    }
}

/// The chip's accessible name: its mode alone does not say what it sets.
fn mode_chip_label(mode: AgentPermissionMode) -> &'static str {
    match mode {
        AgentPermissionMode::ReadOnly => "chat-mode-read-only-chip",
        AgentPermissionMode::Auto => "chat-mode-auto-chip",
        AgentPermissionMode::FullAccess => "chat-mode-full-access-chip",
    }
}

fn mode_note(mode: AgentPermissionMode) -> &'static str {
    match mode {
        AgentPermissionMode::ReadOnly => "chat-mode-read-only-note",
        AgentPermissionMode::Auto => "chat-mode-auto-note",
        AgentPermissionMode::FullAccess => "chat-mode-full-access-note",
    }
}

/// Heights the composer expand/shrink transition eases between, recorded
/// as each frame is laid out so the next one can start from the screen.
#[derive(Clone, Copy, Default)]
pub(super) struct ComposerFrame {
    /// The composer region: cards, composer box, palette, padding.
    pub(super) region: Option<gpui::Pixels>,
    /// The region's top in the window, so the composer can glide from
    /// there when the empty state gives way to a conversation, or back.
    pub(super) top: Option<gpui::Pixels>,
    /// The height the region takes when expanded.
    pub(super) room: Option<gpui::Pixels>,
    /// The empty state's title and privacy line at rest, which fold away
    /// while the composer expands there.
    pub(super) title: Option<gpui::Pixels>,
    pub(super) privacy: Option<gpui::Pixels>,
}

/// Stores a measured element's bounds in a [`ComposerFrame`].
pub(super) type Record = fn(&mut ComposerFrame, gpui::Bounds<gpui::Pixels>, &Window);

/// A paint-free canvas over its parent that hands the parent's bounds to
/// `record` at prepaint, for the next frame's composer transition.
pub(super) fn measure(
    frame: &std::rc::Rc<std::cell::Cell<ComposerFrame>>,
    record: Record,
) -> impl IntoElement {
    let frame = frame.clone();
    gpui::canvas(
        move |bounds, window, _| {
            let mut measured = frame.get();
            record(&mut measured, bounds, window);
            frame.set(measured);
        },
        |_, _, _, _| {},
    )
    .absolute()
    .inset_0()
}

/// The composer region, painted gliding from window y `from` toward its
/// place while `glide` runs (see [`ChatScreen::composer_glide_frame`]).
pub(super) fn glide(
    region: impl IntoElement,
    glide: Option<(gpui::Pixels, f32)>,
) -> gpui::AnyElement {
    match glide {
        Some((from, progress)) => motion::glide_from(region, from, progress).into_any_element(),
        None => region.into_any_element(),
    }
}

impl ChatScreen {
    pub(super) fn render_header(
        &self,
        sidebar_expansion: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let collapsed_progress = 1. - sidebar_expansion;
        let sidebar_width = super::SIDEBAR_RAIL_WIDTH
            + (super::SIDEBAR_WIDTH - super::SIDEBAR_RAIL_WIDTH) * sidebar_expansion;
        // Compensate for the moving pane so the toggle stays beside the
        // window controls throughout the transition, or follows the logo.
        let toolbar_left = super::header_toolbar_left(sidebar_expansion) - sidebar_width;
        let toolbar_width = window.rem_size() * 5.75;
        let header_gap = window.rem_size() * 0.75;
        let expanded_inset = window.rem_size();
        let collapsed_inset = super::header_toolbar_left(0.) - super::SIDEBAR_RAIL_WIDTH
            + toolbar_width
            + header_gap * 2.
            + px(1.);
        let inset = expanded_inset + (collapsed_inset - expanded_inset) * collapsed_progress;
        titlebar::drag_region(
            div()
                .id("chat-header")
                .debug_selector(|| "chat-header".to_string())
                .relative()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .h(titlebar::TITLE_BAR_HEIGHT)
                .flex_none()
                .pl(inset)
                .pr_3()
                .bg(gpui::rgb(theme::bg_app())),
        )
        .child(
            div()
                .absolute()
                .left(toolbar_left)
                .top(titlebar::title_bar_row_top())
                .flex()
                .items_center()
                .gap_1()
                .children(
                    [
                        (
                            "header-sidebar-toggle",
                            "panel-left",
                            tr("chat-toggle-sidebar"),
                            &super::ToggleSidebar as &dyn gpui::Action,
                            ChatCommand::ToggleSidebar,
                            3,
                        ),
                        (
                            "header-search",
                            "search",
                            tr("chat-search"),
                            &super::FocusSearch,
                            ChatCommand::FocusSearch,
                            4,
                        ),
                        (
                            "header-new-task",
                            "square-pen",
                            tr("chat-new-task"),
                            &super::NewTask,
                            ChatCommand::NewTask,
                            5,
                        ),
                    ]
                    .into_iter()
                    .filter(|(_, _, _, _, command, _)| {
                        collapsed_progress > 0. || !matches!(command, ChatCommand::NewTask)
                    })
                    .map(|(id, name, label, action, command, tab_index)| {
                        widgets::icon_button_for_action(
                            id,
                            name,
                            label,
                            px(16.),
                            theme::text_secondary(),
                            action,
                        )
                        .debug_selector(move || id.to_string())
                        .tab_index(tab_index)
                        .when(matches!(command, ChatCommand::NewTask), |button| {
                            button
                                .opacity(collapsed_progress)
                                .tab_stop(self.sidebar_collapsed)
                        })
                        .on_mouse_down(gpui::MouseButton::Left, |_event, _window, cx| {
                            cx.stop_propagation();
                        })
                        .on_key_down(Self::sidebar_button_key_down)
                        .on_click(cx.listener(
                            move |this, _event, window, cx| {
                                this.execute_command(command, window, cx);
                            },
                        ))
                    }),
                ),
        )
        .child(
            div()
                .flex_none()
                .w(px(14.))
                .h(px(14.))
                .debug_selector(|| "navigation-loading-slot".to_string())
                // A task that opens quickly never flashes the spinner.
                .when(
                    self.loading_spinner_due() || self.root_selecting || self.booting,
                    |slot| slot.child(spinner("navigation-loading", px(14.), theme::text_muted())),
                ),
        )
        .child(div().flex_1())
        .when_some(self.host_reconnect_status(), |row, status| {
            row.child(
                // The padding is hover room: the tooltip is how to find out
                // what is happening, so it must not take a 14 px aim. The
                // margin keeps the spinner itself on the row's gutter.
                div()
                    .id("host-reconnecting")
                    .debug_selector(|| "host-reconnecting".to_string())
                    .flex_none()
                    .p_1()
                    .mr(px(-4.))
                    .role(gpui::Role::Status)
                    .aria_label(status.clone())
                    .tooltip(widgets::tooltip(status, None))
                    .child(spinner("host-reconnecting", px(14.), theme::text_muted())),
            )
        })
    }

    /// A new task's folder and branch, above the composer. A saved task's
    /// folder is in the sidebar, so the top bar carries neither.
    fn render_project_context(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .gap_3()
            .min_w_0()
            .max_w_full()
            .child(
                self.with_menu(
                    ChatPopup::Project,
                    project_chip(
                        if self.scratch_project {
                            "folder-plus"
                        } else if self.backend.remote_host().is_some() {
                            "globe"
                        } else {
                            "folder-open"
                        },
                        self.project_computer.clone(),
                        self.project_label.clone(),
                        self.popup.is_open(&ChatPopup::Project),
                    )
                    // Outside the composer box, so the standalone radius.
                    .rounded(theme::RADIUS_SM),
                    Placement::AboveStart,
                    window,
                    cx,
                    Self::project_menu,
                )
                .min_w_0()
                .max_w_full(),
            )
            .when_some(self.branch_label.clone(), |row, branch| {
                row.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(gpui::rgb(theme::status_error()))
                        .max_w(px(200.))
                        .truncate()
                        .debug_selector(|| "project-branch".to_string())
                        .child(gpui::text!(branch)),
                )
            })
    }

    /// `button` as the opener of `popup`, with `menu` attached below or
    /// above it while the popup is open.
    fn with_menu<E: gpui::StatefulInteractiveElement + IntoElement>(
        &self,
        popup: ChatPopup,
        button: E,
        placement: Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
        menu: impl FnOnce(&Self, &mut Context<Self>) -> Menu<Self>,
    ) -> Div {
        let menu = self.popup.is_open(&popup).then(|| {
            let menu = menu(self, cx);
            self.popup.render(menu, placement, window, cx)
        });
        div()
            .relative()
            .child(
                self.popup
                    .trigger(popup, button, cx, move |this, window, cx| {
                        this.press_chip(popup, window, cx)
                    }),
            )
            .children(menu)
    }

    /// A press on a chip: the project chip runs the command it
    /// shares with its shortcut; a composer chip opens or closes its menu.
    fn press_chip(&mut self, popup: ChatPopup, window: &mut Window, cx: &mut Context<Self>) {
        match popup {
            ChatPopup::Project => self.execute_command(ChatCommand::ChooseProject, window, cx),
            popup => self.toggle_popup(popup, cx),
        }
    }

    /// The controls the row shows now, in order.
    pub(super) fn composer_controls(&self) -> impl Iterator<Item = ComposerControl> + '_ {
        ComposerControl::ALL
            .into_iter()
            .filter(|control| match control {
                ComposerControl::Dictate => self.audio_caps.transcription,
                _ => true,
            })
    }

    /// Press `control` from the keyboard, as a click would. A menu opens on
    /// its current choice, the way a native popup button does.
    pub(super) fn activate_composer_control(
        &mut self,
        control: ComposerControl,
        cx: &mut Context<Self>,
    ) {
        match control {
            ComposerControl::Model => self.open_model_panel(cx),
            ComposerControl::Integrations => {
                self.popup.open(ChatPopup::Integrations, cx);
                self.refresh_session_mcp(cx);
            }
            ComposerControl::Mode => {
                self.popup
                    .open_highlighted(ChatPopup::Mode, mode_item_id(self.permission_mode), cx)
            }
            ComposerControl::Attachments => self.pick_attachments(cx),
            ComposerControl::Dictate => self.toggle_recording(cx),
        }
    }

    /// One control of the button row, underlined while application Vim has
    /// it selected.
    fn render_composer_control(
        &self,
        control: ComposerControl,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let element = match control {
            ComposerControl::Model => {
                let label = self
                    .selected_model
                    .as_deref()
                    .map(|model| self.model_label(model))
                    .map(str::to_string)
                    .unwrap_or_else(|| tr("chat-model").to_string());
                let chip_name = label.clone();
                // The level rides on the chip once the model's levels are
                // known, for a model that thinks: a pick waiting to save,
                // else the saved level, as the panel shows it.
                let chosen = self.effort_chosen();
                let level = chosen.map(thinking_label);
                // At a model's top level the effort word stays warm, as the
                // panel's slider does.
                let detail_color: gpui::Hsla = match (self.thinking.as_ref(), chosen) {
                    (Some(state), Some(chosen))
                        if state.levels.len() > 1 && state.levels.last() == Some(&chosen) =>
                    {
                        crate::ui::effort_slider::heat_color(super::effort::level_heat(chosen))
                    }
                    _ => gpui::rgb(theme::text_muted()).into(),
                };
                let mut button = chip(
                    "model-picker",
                    None,
                    label,
                    level
                        .clone()
                        .map(|level| (level, detail_color, self.thinking_epoch)),
                    true,
                    self.popup.is_open(&ChatPopup::Model),
                );
                if let Some(level) = level {
                    button = button.aria_label(self.messages.get(
                        "chat-model-effort-chip",
                        [
                            ("model", chip_name.into()),
                            ("effort", level.to_string().into()),
                        ],
                    ));
                }
                self.with_menu(
                    ChatPopup::Model,
                    button,
                    Placement::AboveStart,
                    window,
                    cx,
                    |this, cx| this.model_menu(cx),
                )
                // The model's name truncates before the row squeezes Send.
                .min_w_0()
            }
            ComposerControl::Integrations => self.with_menu(
                ChatPopup::Integrations,
                chip(
                    "mcp-menu",
                    Some("puzzle"),
                    if self.mcp_enabled_count == 0 {
                        tr("chat-integrations")
                    } else {
                        self.messages.get(
                            "chat-integration-count",
                            [("count", self.mcp_enabled_count.into())],
                        )
                    },
                    None,
                    false,
                    self.popup.is_open(&ChatPopup::Integrations),
                ),
                Placement::AboveStart,
                window,
                cx,
                |this, _| this.integrations_menu(),
            ),
            ComposerControl::Mode => self.with_menu(
                ChatPopup::Mode,
                chip(
                    "permission-mode-menu",
                    Some(mode_icon(self.permission_mode)),
                    tr(mode_label(self.permission_mode)).to_string(),
                    None,
                    true,
                    self.popup.is_open(&ChatPopup::Mode),
                )
                .aria_label(tr(mode_chip_label(self.permission_mode))),
                Placement::AboveStart,
                window,
                cx,
                |this, _| this.mode_menu(),
            ),
            ComposerControl::Attachments => div().child(
                div()
                    .id("add-attachments")
                    .size_8()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::RADIUS_LG)
                    .map(|element| {
                        let rest: gpui::Hsla = gpui::transparent_black();
                        crate::ui::motion::hover_background(
                            element,
                            rest,
                            gpui::rgb(theme::bg_sidebar_pill()),
                        )
                    })
                    .hover(|style| style.cursor_pointer())
                    .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_pill())))
                    .when(self.attachment_picking, |el| el.opacity(0.5))
                    .tooltip(widgets::tooltip(tr("chat-attach"), None))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.activate_composer_control(ComposerControl::Attachments, cx);
                    }))
                    .child(icon("paperclip", px(16.), theme::text_secondary())),
            ),
            ComposerControl::Dictate => {
                let recording = self.recording;
                let transcribing = self.transcribing;
                div().child(
                    div()
                        .id("record-voice")
                        .size_8()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(theme::RADIUS_LG)
                        .when(recording, |el| el.bg(gpui::rgb(theme::status_error())))
                        .map(|element| {
                            let rest: gpui::Hsla = if recording {
                                gpui::rgb(theme::status_error()).into()
                            } else {
                                gpui::transparent_black()
                            };
                            crate::ui::motion::hover_background(
                                element,
                                rest,
                                gpui::rgb(if recording {
                                    theme::status_error()
                                } else {
                                    theme::bg_sidebar_pill()
                                }),
                            )
                        })
                        .hover(|style| style.cursor_pointer())
                        .when(transcribing, |el| el.opacity(0.5))
                        .tooltip(widgets::tooltip(
                            if recording {
                                tr("chat-stop-recording")
                            } else {
                                tr("chat-dictate")
                            },
                            None,
                        ))
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.activate_composer_control(ComposerControl::Dictate, cx);
                        }))
                        .child(if transcribing {
                            spinner("transcribing", px(16.), theme::text_secondary())
                        } else if recording {
                            icon("square", px(14.), theme::on_accent()).into_any_element()
                        } else {
                            icon("mic", px(16.), theme::text_secondary()).into_any_element()
                        }),
                )
            }
        };
        // The accent bar is the application Vim selection mark, as on
        // sidebar and transcript rows; under a chip it sits along the
        // bottom without changing the row's layout.
        element
            .relative()
            .when(selected, |element| {
                element.child(
                    div()
                        .absolute()
                        .bottom_0()
                        .left_1()
                        .right_1()
                        .h(px(2.))
                        .rounded_full()
                        .bg(gpui::rgb(theme::accent())),
                )
            })
            .into_any_element()
    }

    /// Explicit new-project choices, followed by at most three recent projects.
    fn project_menu(&self, _cx: &mut Context<Self>) -> Menu<Self> {
        let mut menu = Menu::new("project-menu", px(320.))
            .label(tr("chat-projects"))
            .max_height(px(440.))
            .application_vim(self.application_vim_enabled);
        if self.computers.len() > 1 {
            return self.computers_project_menu();
        }
        menu = menu.item(self.scratch_project_item(
            "root-scratch",
            self.backend.remote_host().map(str::to_owned),
        ));
        menu = menu.item(
            MenuItem::new(
                "root-choose",
                tr("chat-choose-folder-menu"),
                |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                    this.choose_root_dialog(cx);
                },
            )
            .icon("folder-open"),
        );
        if !self.picker_projects.is_empty() {
            menu = menu.separator().header(tr("chat-recent-projects"));
        }
        for (_, root) in &self.picker_projects {
            let path = &root.path;
            let label = &root.name;
            let pick = path.clone();
            let mut item = MenuItem::new(
                SharedString::from(format!("root-{path}")),
                label.clone(),
                move |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                    this.select_project_root(pick.clone(), cx);
                },
            )
            .truncate_start()
            .current(self.project_root.as_deref() == Some(path.as_str()));
            if self
                .picker_projects
                .iter()
                .filter(|(_, peer)| &peer.name == label)
                .count()
                > 1
            {
                item = item.note(path.clone());
            }
            menu = menu.item(item);
        }
        menu
    }

    pub(super) fn scratch_project_item(
        &self,
        id: impl Into<gpui::ElementId>,
        computer: Option<String>,
    ) -> MenuItem<Self> {
        let current = self.scratch_project && computer.as_deref() == self.backend.remote_host();
        MenuItem::new(
            id,
            tr("chat-new-project-button"),
            move |this: &mut Self, _, cx| {
                if computer.as_deref() == this.backend.remote_host() {
                    this.select_scratch_project(cx);
                } else {
                    cx.emit(Navigate {
                        computer: computer.clone(),
                        destination: Destination::ScratchProject,
                    });
                }
            },
        )
        .icon("folder-plus")
        .current(current)
    }

    /// The models, the selected one checked, with its
    /// effort slider docked under them. A pick applies from the task's next
    /// message and closes the panel; Left and Right step the effort.
    fn model_menu(&self, cx: &mut Context<Self>) -> Menu<Self> {
        let selected = self.selected_model.as_deref();
        let mut menu = Menu::new("model-menu", px(320.))
            .label(tr("chat-model"))
            .max_height(px(480.))
            .application_vim(self.application_vim_enabled)
            .on_adjust(|this: &mut Self, step, _, cx| this.step_effort(step, cx));
        let sections = self.model_sections.group(&self.models);
        for (index, (heading, models)) in sections.into_iter().enumerate() {
            if let Some(heading) = heading {
                if index > 0 {
                    menu = menu.separator();
                }
                menu = menu.header(heading);
            }
            menu = menu.items(models.into_iter().map(|model| {
                let pick = model.to_string();
                let label = self.model_label(model).to_string();
                let popular = self.model_sections.is_popular(model);
                // The badge is drawn content; the row's name carries it for
                // assistive technology.
                let name: SharedString = if popular {
                    self.messages
                        .get("chat-model-popular-row", [("model", label.clone().into())])
                } else {
                    label.clone().into()
                };
                let row = MenuItem::new(
                    SharedString::from(format!("model-{model}")),
                    name,
                    move |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                        this.pick_model(pick.clone(), cx);
                    },
                )
                .current(selected == Some(model));
                if popular {
                    row.content(
                        div()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().min_w_0().truncate().child(label))
                            .child(popular_badge()),
                    )
                } else {
                    row
                }
            }));
        }
        // Levels that failed to load: Retry is a row, so the keyboard
        // reaches it, and the panel stays open while they load again.
        if self.effort_failed() {
            menu = menu.separator().item(
                MenuItem::new(
                    "retry-thinking",
                    tr("chat-thinking-retry"),
                    |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                        this.refresh_thinking(cx);
                        cx.notify();
                    },
                )
                .icon("rotate-ccw")
                .keep_open(),
            );
        }
        menu.footer(self.effort_footer(cx))
    }

    /// What the task may do without asking.
    fn mode_menu(&self) -> Menu<Self> {
        Menu::new("mode-menu", px(320.))
            .label(tr("chat-mode"))
            .application_vim(self.application_vim_enabled)
            .items(
                [
                    AgentPermissionMode::ReadOnly,
                    AgentPermissionMode::Auto,
                    AgentPermissionMode::FullAccess,
                ]
                .map(|mode| {
                    MenuItem::new(
                        mode_item_id(mode),
                        tr(mode_label(mode)),
                        move |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                            this.set_permission_mode(mode, cx);
                        },
                    )
                    .icon(mode_icon(mode))
                    .note(tr(mode_note(mode)))
                    .current(self.permission_mode == mode)
                }),
            )
    }

    /// The task's integrations, each with a switch. A server that still
    /// needs setup opens Settings instead.
    fn integrations_menu(&self) -> Menu<Self> {
        let mut menu = Menu::new("integrations-menu", px(360.))
            .label(tr("chat-integrations"))
            .max_height(px(320.))
            .application_vim(self.application_vim_enabled)
            .header(tr("chat-integrations"));
        if self.session_mcp.is_empty() {
            menu = menu.child(
                div()
                    .px_3()
                    .py_2()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(tr("chat-no-integrations")),
            );
        }
        for server in &self.session_mcp {
            let name = server.name.clone();
            let kind = server.kind;
            let enabled = server.enabled;
            let available = server.available;
            let usable = available || enabled;
            let content = div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .line_clamp(1)
                        .text_ellipsis()
                        .child(server.display_name.clone()),
                )
                .when(!server.description.is_empty(), |column| {
                    column.child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .line_clamp(2)
                            .child(
                                if server.kind
                                    == maple_agent::agent::AgentSessionIntegrationKind::Mcp
                                    && server.name == "cua-driver"
                                {
                                    tr("settings-integration-cua-description")
                                } else {
                                    crate::ui::settings::saved_description(
                                        &server.display_name,
                                        &server.description,
                                    )
                                },
                            ),
                    )
                })
                .when(!available, |column| {
                    column.child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::status_warning()))
                            .child(tr("chat-setup-integrations")),
                    )
                });
            let item = MenuItem::new(
                SharedString::from(format!("mcp-{kind:?}-{name}")),
                server.display_name.clone(),
                move |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                    if usable {
                        this.toggle_session_mcp(name.clone(), kind, !enabled, cx);
                    } else {
                        cx.emit(OpenSettingsSection(Section::Integrations));
                    }
                },
            )
            .content(content);
            menu = menu.item(if usable {
                item.switch(enabled)
            } else {
                item.trailing(widgets::switch_track(enabled))
            });
        }
        menu.separator().item(
            MenuItem::new(
                "mcp-manage",
                tr("chat-manage-integrations"),
                |_: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                    cx.emit(OpenSettingsSection(Section::Integrations));
                },
            )
            .style(|row| row.text_color(gpui::rgb(theme::accent()))),
        )
    }

    /// Command palette shown while the composer text starts with "/".
    /// Lists built-ins plus the project's skill commands, filtered by the
    /// typed prefix; clicking completes the command in the composer.
    pub(super) fn render_slash_palette(&self, cx: &mut Context<Self>) -> Option<Div> {
        let entries = &self.slash_entries;
        if entries.is_empty() {
            return None;
        }
        let selected = self.slash_selected.filter(|index| *index < entries.len());
        let chat = cx.entity().downgrade();
        let mut palette = div()
            .flex()
            .flex_col()
            .mt_1()
            .p_1()
            .rounded(theme::RADIUS_MD)
            .bg(gpui::rgb(theme::bg_elevated()))
            .border_1()
            .border_color(gpui::rgb(theme::border()))
            .shadow_md();
        for (index, entry) in entries.iter().enumerate() {
            let is_selected = selected == Some(index);
            let name = entry.name.clone();
            let chat = chat.clone();
            palette = palette.child(
                div()
                    .id(gpui::SharedString::from(format!("slash-{}", entry.name)))
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .rounded(theme::RADIUS_SM)
                    .when(is_selected, |row| {
                        row.bg(gpui::rgb(theme::bg_sidebar_row_selected()))
                    })
                    .map(|element| {
                        let rest: gpui::Hsla = if is_selected {
                            gpui::rgb(theme::bg_sidebar_row_selected()).into()
                        } else {
                            gpui::transparent_black()
                        };
                        crate::ui::motion::hover_background(
                            element,
                            rest,
                            gpui::rgb(theme::bg_sidebar_row_hover()),
                        )
                    })
                    .hover(|style| style.cursor_pointer())
                    .on_click(move |_event, _window, cx: &mut gpui::App| {
                        chat.update(cx, |chat, cx| chat.complete_slash_command(&name, cx))
                            .ok();
                    })
                    .child(
                        div()
                            .font_family(crate::assets::FONT_MONO)
                            .text_color(gpui::rgb(theme::accent()))
                            .child(format!("/{}", entry.name)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .line_clamp(1)
                            .text_ellipsis()
                            .child(entry.description.clone()),
                    ),
            );
        }
        Some(div().child(motion::rise_in(palette, "slash-palette-reveal")))
    }

    fn toggle_plan_collapsed(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.selected_session.clone() else {
            return;
        };
        if !self.plan_collapsed.remove(&session) {
            self.plan_collapsed.insert(session);
        }
        cx.notify();
    }

    /// Pinned checklist of the latest todo list, or `None` without one.
    /// Ask a `/btw` side question against the selected task. While the card
    /// is open, the question continues its thread; the earlier turns go with
    /// the request. The answer streams into the card and is never stored.
    pub(super) fn ask_side_question(
        &mut self,
        session_id: &str,
        question: &str,
        cx: &mut Context<Self>,
    ) {
        let question = question.trim();
        if question.is_empty() {
            self.notice = Some(UiText::key("chat-btw-question"));
            cx.notify();
            return;
        }
        if self.booting {
            self.notice = Some(UiText::key("chat-runtime-starting"));
            cx.notify();
            return;
        }
        if self.btw.as_ref().is_some_and(|btw| btw.pending) {
            self.notice = Some(UiText::key("chat-wait-answer"));
            cx.notify();
            return;
        }
        self.btw_sequence += 1;
        let request_id = format!("btw-{}", self.btw_sequence);
        // A turn that failed has no answer to replay; drop it.
        let mut turns = self.btw.take().map(|btw| btw.turns).unwrap_or_default();
        turns.retain(|turn| !turn.answer.is_empty());
        let prior: Vec<SideQuestionTurn> = turns
            .iter()
            .map(|turn| SideQuestionTurn {
                question: turn.question.to_string(),
                answer: turn.answer.clone(),
            })
            .collect();
        turns.push(SideThreadTurn {
            question: question.to_string().into(),
            answer: String::new(),
        });
        self.btw = Some(SideQuestionPanel {
            request_id: request_id.clone(),
            turns,
            revision: 0,
            pending: true,
            error: None,
        });
        if self.queue_edit.is_none()
            && let Some(composer) = self.composer.clone()
        {
            composer.update(cx, |input, cx| {
                input.set_placeholder_key(SIDE_THREAD_PLACEHOLDER, cx)
            });
        }
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let session_id = session_id.to_string();
        let question = question.to_string();
        let callback_id = request_id.clone();
        self.call(
            async move {
                backend
                    .ask_side_question(&user_id, &session_id, request_id, prior, question)
                    .await
            },
            cx,
            move |this, result, cx| {
                if let Err(message) = result
                    && let Some(btw) = this.btw.as_mut()
                    && btw.request_id == callback_id
                {
                    btw.pending = false;
                    btw.error = Some(message.into());
                    cx.notify();
                }
            },
        );
        cx.notify();
    }

    /// Close the side thread; later messages go to the task again.
    pub(super) fn close_side_thread(&mut self, cx: &mut Context<Self>) {
        self.btw = None;
        if self.queue_edit.is_none()
            && let Some(composer) = self.composer.clone()
        {
            composer.update(cx, |input, cx| {
                input.set_placeholder_key(COMPOSER_PLACEHOLDER, cx)
            });
        }
        cx.notify();
    }

    pub(super) fn render_btw_card(&self, cx: &mut Context<Self>) -> Option<Div> {
        let btw = self.btw.as_ref()?;
        let last = btw.turns.len().saturating_sub(1);
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(gpui::rgb(theme::text_primary()))
                    .child(BTW_COMMAND),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(tr("chat-side-thread-hint")),
            )
            .when(btw.pending, |header| {
                header.child(icon("loader-circle", px(14.), theme::text_muted()))
            })
            .child(
                div()
                    .id("btw-close")
                    .px_2()
                    .rounded(theme::RADIUS_SM)
                    .cursor_pointer()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .map(|element| {
                        let rest: gpui::Hsla = gpui::transparent_black();
                        crate::ui::motion::hover_background(
                            element,
                            rest,
                            gpui::rgb(theme::bg_elevated()),
                        )
                    })
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.close_side_thread(cx);
                    }))
                    .child("×"),
            );
        let mut body = div()
            .id("btw-body")
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .pb_2()
            .max_h(px(320.))
            .overflow_y_scroll()
            .text_sm()
            .text_color(gpui::rgb(theme::text_primary()));
        for (index, turn) in btw.turns.iter().enumerate() {
            // Finished turns never change; only the last one is re-parsed.
            let revision = if index == last { btw.revision } else { 0 };
            let key = format!("btw-answer-{index}");
            let document =
                self.markdown_cache
                    .get(&key, MarkdownKind::Body, revision, &turn.answer);
            // Same shape as the transcript: the question is a right-aligned
            // bubble, the answer is plain text on the left.
            body = body.child(
                div().flex().flex_col().items_end().mt_1().child(
                    div()
                        .max_w(gpui::relative(0.75))
                        .px_3()
                        .py_1p5()
                        .rounded(theme::RADIUS_MD)
                        .bg(gpui::rgb(theme::bg_user_bubble()))
                        .border_1()
                        .border_color(gpui::rgb(theme::user_bubble_border()))
                        .text_color(gpui::rgb(theme::text_primary()))
                        .child(turn.question.clone()),
                ),
            );
            if !turn.answer.is_empty() {
                body = body.child(div().pr_8().child(markdown::render(&document)));
            } else if btw.pending && index == last {
                body = body.child(
                    div()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(tr("chat-thinking")),
                );
            }
        }
        if let Some(error) = &btw.error {
            body = body.child(
                div()
                    .text_color(gpui::rgb(theme::status_error()))
                    .child(error.clone()),
            );
        }
        Some(
            div()
                .flex()
                .flex_col()
                .mb_2()
                .rounded(theme::RADIUS_SM)
                .bg(gpui::rgb(theme::bg_tool_card()))
                .border_1()
                .border_color(gpui::rgb(theme::border_subtle()))
                .overflow_hidden()
                .child(header)
                .child(body),
        )
    }

    /// The subagents working for this task, pinned above the composer.
    /// `None` when none are working, which is the common case.
    pub(super) fn render_subagents_card(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.subagents.is_empty() {
            return None;
        }
        let now = std::time::Instant::now();
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .child(icon("users", px(14.), theme::text_secondary()))
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(gpui::rgb(theme::text_primary()))
                    .child(tr("chat-external-agents")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(self.messages.get(
                        "chat-agents-running",
                        [("count", self.subagents.len().into())],
                    )),
            );
        Some(
            div()
                .flex()
                .flex_col()
                .mb_2()
                .rounded(theme::RADIUS_SM)
                .bg(gpui::rgb(theme::bg_tool_card()))
                .border_1()
                .border_color(gpui::rgb(theme::border_subtle()))
                .overflow_hidden()
                .child(header)
                .child(div().flex().flex_col().gap_1().px_3().pb_2().children(
                    self.subagents.iter().map(|subagent| {
                        let on_stop = subagent.external.as_ref().map(|external| {
                            let agent_id = external.agent_id.clone();
                            let listener =
                                cx.listener(move |this: &mut ChatScreen, _event, _window, cx| {
                                    this.stop_external_agent(&agent_id, cx);
                                });
                            Box::new(listener) as super::transcript::StopHandler
                        });
                        render_subagent_row(subagent, now, on_stop)
                    }),
                )),
        )
    }

    pub(super) fn render_plan_card(&self, cx: &mut Context<Self>) -> Option<Div> {
        if !self.plan_card_visible() {
            return None;
        }
        let collapsed = self
            .selected_session
            .as_ref()
            .is_some_and(|session| self.plan_collapsed.contains(session));
        let done = self.plan_done;
        let header = div()
            .id("plan-card-header")
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .cursor_pointer()
            .map(|element| {
                let rest: gpui::Hsla = gpui::transparent_black();
                crate::ui::motion::hover_background(element, rest, gpui::rgb(theme::bg_elevated()))
            })
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.toggle_plan_collapsed(cx);
            }))
            .child(icon(
                if collapsed {
                    "chevron-right"
                } else {
                    "chevron-down"
                },
                px(14.),
                theme::text_secondary(),
            ))
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(gpui::rgb(theme::text_primary()))
                    .child(tr("chat-plan")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(format!("{done}/{}", self.plan.len())),
            );
        let mut card = div()
            .flex()
            .flex_col()
            .mb_2()
            .rounded(theme::RADIUS_SM)
            .bg(gpui::rgb(theme::bg_tool_card()))
            .border_1()
            .border_color(gpui::rgb(theme::border_subtle()))
            .overflow_hidden()
            .child(header);
        if !collapsed {
            card = card.child(
                div()
                    .id("plan-card-body")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .pb_2()
                    .max_h(px(200.))
                    .overflow_y_scroll()
                    .children(self.plan.iter().map(render_plan_row)),
            );
        }
        Some(card)
    }

    /// The composer's controls, held back while the runtime boots or the
    /// first model load is in flight with no model chosen, then faded in
    /// once. Held controls keep their layout and take no clicks.
    fn reveal_composer_controls(
        &mut self,
        controls: Vec<gpui::AnyElement>,
    ) -> (gpui::AnyElement, bool) {
        // The row takes the room left beside Send and wraps onto a second
        // line when the chips do not fit, rather than letting a chip shrink
        // under its neighbours. Only a chip alone on its line still gives
        // way, the model chip's name first.
        let row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .flex_auto()
            .min_w_0()
            .children(controls);
        if self.selected_model.is_none() && (self.booting || self.models_loading) {
            self.controls_held = true;
            return (row.invisible().into_any_element(), true);
        }
        if std::mem::take(&mut self.controls_held) {
            let generation = self
                .controls_revealed
                .map_or(0, |(_, generation)| generation + 1);
            self.controls_revealed = Some((std::time::Instant::now(), generation));
        }
        let row = match self.controls_revealed {
            Some((at, generation)) if at.elapsed() < motion::REVEAL * 2 => {
                motion::fade_in(row, ("composer-controls-reveal", generation as usize))
                    .into_any_element()
            }
            _ => row.into_any_element(),
        };
        (row, false)
    }

    /// While the composer eases between the height of its text and
    /// expanded: how far toward expanded it is, and the region's height
    /// for this frame, from heights measured last frame. `None` at rest;
    /// once a transition lands the field goes back to its resting fill.
    pub(super) fn composer_resize_frame(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(f32, gpui::Pixels)> {
        let transition = self.composer_resize?;
        let now = motion::now();
        let frame = self.composer_frame.get();
        let sample = (!cx.reduce_motion() && transition.is_moving(now))
            .then(|| {
                let progress = transition.value(now);
                // The field fills the box throughout, so the box at the
                // height of its text is the region less the field's spare
                // height. Its row is never shorter than the expand button
                // beside it (`size_6`).
                let (drawn, text) = self.composer.as_ref()?.read(cx).drawn_and_text_height()?;
                let row = text.max(window.rem_size() * 1.5);
                let natural = frame.region? - (drawn - row).max(gpui::px(0.));
                let room = frame.room?;
                Some((progress, natural + (room - natural) * progress))
            })
            .flatten();
        if sample.is_some() {
            window.request_animation_frame();
            return sample;
        }
        self.composer_resize = None;
        let expanded = self.composer_expanded;
        if let Some(composer) = self.composer.clone() {
            composer.update(cx, |input, cx| input.set_fill_height(expanded, cx));
        }
        None
    }

    /// When the empty state gives way to a conversation (the first message,
    /// or opening a task) or back (New Task), the composer glides from where
    /// it was last drawn to its new place over [`motion::PANEL`]. Returns
    /// the window y it starts from and how far along it is; `None` at rest,
    /// on the first frame, and under reduced motion.
    pub(super) fn composer_glide_frame(
        &mut self,
        empty: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(gpui::Pixels, f32)> {
        let now = motion::now();
        let reduced = cx.reduce_motion();
        if self.composer_was_empty.is_some_and(|was| was != empty) {
            self.composer_glide = self
                .composer_frame
                .get()
                .top
                .filter(|_| !reduced)
                .map(|from| (from, now));
        }
        self.composer_was_empty = Some(empty);
        let glide = self.composer_glide.and_then(|(from, started)| {
            let progress = motion::progress(started, motion::PANEL, now, reduced);
            (progress < 1.).then_some((from, progress))
        });
        if glide.is_some() {
            window.request_animation_frame();
        } else {
            self.composer_glide = None;
        }
        glide
    }

    /// The composer box lays out expanded: at rest there, or on the way.
    pub(super) fn composer_fills(&self) -> bool {
        self.composer_expanded || self.composer_resize.is_some()
    }

    /// The selected task's latest turn ended out of usage and nothing has
    /// gone out since. A window that does not hold the tail (paged back
    /// into history, or with live rows past its budget) cannot tell.
    /// The offer stands down while the billing reading says extra credits
    /// cover the used-up plan (bought since the refusal, say), unless the
    /// refusal itself came while they did: then the credits did not pay.
    fn usage_limit_reached(&self) -> bool {
        !self.pages.after
            && !self.pages.live_has_more
            && !self.is_run_active()
            && !self.has_send_feedback()
            && self.timeline.last().is_some_and(|row| {
                super::transcript::is_usage_limit(row)
                    && (self.refused_on_credits.as_deref() == Some(row.id.as_str())
                        || !self
                            .sidebar_plan
                            .as_ref()
                            .is_some_and(crate::billing::PlanUsage::on_extra_credits))
            })
    }

    /// The out-of-usage notice, docked on top of the composer where the
    /// next message would go. It offers Billing until a message is sent;
    /// the transcript keeps a one-line marker of the turn.
    fn render_usage_tray(&self, cx: &mut Context<Self>) -> Div {
        div()
            .debug_selector(|| "composer-usage-limit".to_string())
            .flex()
            .items_center()
            .gap_3()
            .mx_2()
            .mt_2()
            .pl_2p5()
            .pr_2()
            .py_2()
            .rounded(theme::RADIUS_LG)
            .bg(gpui::rgb(theme::bg_composer_tray()))
            .border_1()
            .border_color(gpui::rgb(theme::border_subtle()))
            .child(
                div()
                    .flex_none()
                    .size_8()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::RADIUS_SM)
                    .bg(gpui::rgb(theme::accent_container()))
                    .child(icon("gauge", px(16.), theme::accent_ink())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(gpui::rgb(theme::text_heading()))
                            .child(gpui::text!(tr("chat-usage-limit-title"))),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(gpui::text!(tr("chat-usage-limit-body"))),
                    ),
            )
            .child(
                widgets::default_button("composer-open-billing")
                    .flex_none()
                    .pr_4()
                    .debug_selector(|| "composer-open-billing".to_string())
                    .child(gpui::text!(tr("chat-billing-open")))
                    .child(icon(
                        "arrow-right",
                        px(14.),
                        theme::text_on_default_button(),
                    ))
                    .on_click(cx.listener(|_this, _event, _window, cx| {
                        cx.stop_propagation();
                        cx.emit(OpenSettingsSection(Section::Billing));
                    })),
            )
    }

    pub(super) fn render_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let running = self.is_run_active();
        // The first send's create keeps the composer's text on show and
        // the composer waiting, like the runtime boot does.
        let pending_create = self.selected_session.is_none() && self.session_setup_pending;
        let disabled =
            self.booting || self.loading_session.is_some() || pending_create || self.send_pending();
        let status_label = if self.booting {
            tr("chat-starting")
        } else if self.loading_session.is_some() {
            tr("chat-opening-task")
        } else if pending_create {
            tr("chat-creating-task")
        } else {
            tr("chat-sending")
        };
        let has_text = self.composer_has_text;
        let has_attachments = !self.draft_attachments.is_empty();
        let attachments_ready = self.draft_attachments.iter().all(DraftAttachment::ready);
        let can_send = !disabled && attachments_ready && (has_text || has_attachments);
        // One control. Stop only while a run is active and nothing is
        // waiting to send. Text, or an file that is still being prepared,
        // turns it back into Send, which queues behind the run.
        let drafting = has_text || has_attachments;
        let show_stop = running && !self.booting && !pending_create && !drafting;
        let usage_tray = self
            .usage_limit_reached()
            .then(|| self.render_usage_tray(cx));
        let queue_chips = self.render_queue(cx);
        let expanded = self.composer_expanded;
        // Layout follows the box while it eases; the icon and tooltip
        // name the state it is heading to.
        let fills = self.composer_fills();
        // Send ↔ Stop and the send fill's enabled state ease over TOGGLE.
        let (send_stop, send_ready) = {
            let now = motion::now();
            let reduced = cx.reduce_motion();
            let step = |tween: &mut motion::Tween, on: bool| {
                tween.step(
                    if on { 1. } else { 0. },
                    motion::TOGGLE,
                    motion::ease_out_cubic,
                    now,
                    reduced,
                )
            };
            let (stop, stop_moving) = step(&mut self.send_stop, show_stop);
            let (ready, ready_moving) = step(&mut self.send_ready, can_send);
            if stop_moving || ready_moving {
                window.request_animation_frame();
            }
            (stop, ready)
        };
        let composer = self.composer.clone();
        let vim_badge = composer
            .as_ref()
            .and_then(|input| input.read(cx).vim_status())
            .and_then(|status| {
                let label = match status.mode {
                    VimMode::Normal => tr("chat-vim-normal"),
                    VimMode::Insert => tr("chat-vim-insert"),
                    VimMode::Visual if status.visual_linewise => tr("chat-vim-visual-line"),
                    VimMode::Visual => tr("chat-vim-visual"),
                    VimMode::Disabled => return None,
                };
                Some((label, status.notice))
            })
            .map(|(label, notice)| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("composer-vim-mode")
                            .flex_none()
                            .px_2()
                            .py_0p5()
                            .rounded(theme::RADIUS_SM)
                            .bg(gpui::rgb(theme::bg_elevated()))
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_secondary()))
                            .child(label),
                    )
                    .when_some(notice, |row, notice| {
                        row.child(
                            div()
                                .id("composer-vim-notice")
                                .max_w(px(260.))
                                .truncate()
                                .text_xs()
                                .text_color(gpui::rgb(theme::text_muted()))
                                .child(tr(notice.message)),
                        )
                    })
            });
        let selected_control = self.application_vim_selected_control(window);
        let controls = self
            .composer_controls()
            .map(|control| {
                self.render_composer_control(control, selected_control == Some(control), window, cx)
            })
            .collect::<Vec<_>>();
        let (controls, controls_held) = self.reveal_composer_controls(controls);
        let drafts = &self.draft_attachments;
        let upload = self
            .selected_upload()
            .map(|(id, upload)| (id.to_string(), upload.label.clone()));
        let composer_box = div()
            .w_full()
            .flex()
            .flex_col()
            .relative()
            .debug_selector(|| "composer-box".to_string())
            .when(fills, |container| container.flex_1().min_h_0())
            .rounded(theme::RADIUS_XL)
            .bg(gpui::rgb(theme::bg_app()))
            .border_1()
            .border_color(gpui::rgb(theme::accent()))
            .when(disabled, |container| {
                container.border_color(gpui::rgb(theme::border()))
            })
            // Native files are copied locally before any execution-side upload.
            .can_drop(|dragged, _window, _cx| {
                dragged.downcast_ref::<gpui::ExternalPaths>().is_some()
            })
            .drag_over::<gpui::ExternalPaths>(|style, _paths, _window, _cx| {
                style
                    .bg(gpui::rgb(theme::accent_container()))
                    .border_color(gpui::rgb(theme::accent_hover()))
                    .border_dashed()
            })
            .on_drop(
                cx.listener(|this, paths: &gpui::ExternalPaths, _window, cx| {
                    this.add_attachment_paths(paths.paths().to_vec(), cx);
                }),
            )
            .children(usage_tray)
            .children(queue_chips)
            .when_some(upload, |container, (id, label)| {
                container.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_4()
                        .pt_3()
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .text_color(gpui::rgb(theme::text_muted()))
                                .child(label),
                        )
                        .child(
                            widgets::secondary_button("cancel-attachment-upload")
                                .text_xs()
                                .child(gpui::text!(tr("chat-cancel")))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.cancel_attachment_send(&id, cx)
                                })),
                        ),
                )
            })
            .when(!drafts.is_empty(), |container| {
                container.child(div().flex().flex_wrap().gap_2().px_4().pt_4().children(
                    drafts.iter().map(|attachment| {
                        let id = attachment.id;
                        let chip = div()
                            .relative()
                            .max_w(px(260.))
                            .h_16()
                            .rounded(theme::RADIUS_SM)
                            .border_1()
                            .border_color(gpui::rgb(theme::border()))
                            .bg(gpui::rgb(theme::bg_elevated()))
                            .flex()
                            .items_center()
                            .gap_2()
                            .pr_4()
                            .child(match &attachment.thumbnail {
                                Some(thumbnail) => div().size_16().flex_none().child(
                                    gpui::img(gpui::ImageSource::Image(Arc::clone(thumbnail)))
                                        .size_full()
                                        .rounded(theme::RADIUS_SM),
                                ),
                                None => div()
                                    .size_12()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(icon(
                                        if attachment.mime_type.starts_with("image/") {
                                            "image"
                                        } else {
                                            "paperclip"
                                        },
                                        px(20.),
                                        theme::text_secondary(),
                                    )),
                            })
                            .child(
                                div()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div().truncate().text_sm().child(attachment.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(gpui::rgb(theme::text_muted()))
                                            .child(attachment.detail(&self.messages)),
                                    ),
                            )
                            .child(
                                div()
                                    .id(gpui::SharedString::from(format!("draft-remove-{id}")))
                                    .absolute()
                                    .top(px(-4.))
                                    .right(px(-4.))
                                    .size_5()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .bg(gpui::rgb(theme::bg_elevated()))
                                    .border_1()
                                    .border_color(gpui::rgb(theme::border()))
                                    .hover(|style| style.cursor_pointer())
                                    .on_click(cx.listener(move |this, _event, _window, cx| {
                                        this.remove_attachment(id, cx);
                                    }))
                                    .child(icon("x", px(10.), theme::text_primary())),
                            );
                        // A file joining the draft settles in; removing
                        // one is immediate.
                        motion::slide_in(
                            chip,
                            ("draft-attachment", id as usize),
                            motion::ENTER,
                            gpui::point(px(0.), motion::RISE),
                        )
                    }),
                ))
            })
            .child(
                div()
                    .flex()
                    .items_start()
                    .px_4()
                    .pt_4()
                    .pb_2()
                    .when(fills, |row| row.flex_1().min_h_0())
                    .child(
                        crate::ui::typography::chat_reading(div())
                            .flex_1()
                            .min_w_0()
                            .when(fills, |cell| cell.h_full())
                            .text_color(gpui::rgb(theme::text_primary()))
                            // The first send remains visibly editable until
                            // task creation settles; keep its real focus path.
                            .children(composer),
                    )
                    .child({
                        let label = if expanded {
                            tr("chat-shrink-composer")
                        } else {
                            tr("chat-expand-composer")
                        };
                        div()
                            .id("composer-expand")
                            .role(gpui::Role::Button)
                            .aria_label(label.clone())
                            .size_6()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(theme::RADIUS_SM)
                            .map(|element| {
                                let rest: gpui::Hsla = gpui::transparent_black();
                                crate::ui::motion::hover_background(
                                    element,
                                    rest,
                                    gpui::rgb(theme::bg_sidebar_pill()),
                                )
                            })
                            .hover(|style| style.cursor_pointer())
                            .tooltip(widgets::tooltip(label, None))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.toggle_composer_expanded(cx);
                            }))
                            .child(motion::icon_swap(
                                "composer-expand-icon",
                                expanded,
                                ["maximize-2", "minimize-2"],
                                px(14.),
                                theme::text_muted(),
                            ))
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .pb_2()
                    .pt_1()
                    .debug_selector(|| "composer-chips".to_string())
                    .relative()
                    .child(controls)
                    .children(vim_badge)
                    .when(disabled, |row| {
                        row.child(
                            div()
                                .debug_selector(|| "composer-status".to_string())
                                // Over the held-back controls rather than after their space.
                                .when(controls_held, |status| status.absolute().left_2())
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .px_2()
                                .text_xs()
                                .text_color(gpui::rgb(theme::text_muted()))
                                .child(spinner("composer-status", px(12.), theme::text_muted()))
                                .child(status_label),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("context-indicator")
                            .flex_none()
                            .flex()
                            .items_center()
                            .mr_1()
                            .child(crate::ui::context_ring::ContextRing::new(
                                self.context_fraction.unwrap_or(0.0),
                            )),
                    )
                    .child({
                        let button_id = if show_stop {
                            "stop-run"
                        } else {
                            "send-message"
                        };
                        let button_label: SharedString = if show_stop {
                            tr("chat-stop-run")
                        } else if running && drafting {
                            tr("chat-queue-next-turn")
                        } else {
                            tr("chat-send")
                        };
                        // The fills cross-fade and the arrow scales while
                        // the handler, role and label switch at once.
                        let send_layer = (send_stop < 1.).then(|| {
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(gpui::linear_gradient(
                                    180.,
                                    gpui::linear_color_stop(gpui::rgb(theme::send_top()), 0.),
                                    gpui::linear_color_stop(gpui::rgb(theme::send_bottom()), 1.),
                                ))
                                .opacity((0.4 + 0.6 * send_ready) * (1. - send_stop))
                                .child(if disabled {
                                    spinner("send-booting", px(16.), theme::on_accent())
                                } else {
                                    let scale = 1. - 0.4 * send_stop;
                                    icon("arrow-up", px(16.), theme::on_accent())
                                        .with_transformation(gpui::Transformation::scale(
                                            gpui::size(scale, scale),
                                        ))
                                        .into_any_element()
                                })
                        });
                        let stop_layer = (send_stop > 0.).then(|| {
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(gpui::rgb(theme::status_error()))
                                .opacity(send_stop)
                                .child(
                                    div()
                                        .size_3()
                                        .rounded(theme::RADIUS_SM)
                                        .bg(gpui::rgb(theme::bg_app())),
                                )
                        });
                        div()
                            .id(button_id)
                            .debug_selector(move || button_id.to_string())
                            .role(gpui::Role::Button)
                            .aria_label(button_label.clone())
                            .relative()
                            .flex_none()
                            .size_8()
                            .rounded_full()
                            .children(send_layer)
                            .children(stop_layer)
                            .when(show_stop, |el| {
                                el.hover(|style| style.opacity(0.85).cursor_pointer())
                                    .active(|style| style.opacity(0.7))
                                    .tooltip(widgets::tooltip(tr("chat-stop-run"), None))
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.stop(cx);
                                    }))
                            })
                            .when(!show_stop && can_send, |el| {
                                el.hover(|style| style.opacity(0.9).cursor_pointer())
                                    .active(|style| style.opacity(0.75))
                                    .tooltip(widgets::tooltip(button_label.clone(), Some("↵")))
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.send_inner(cx);
                                    }))
                            })
                    }),
            );
        if self.selected_session.is_some() {
            return composer_box;
        }
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            // The picker row comes with the box, so the pair fills the
            // region for as long as the box does: expanded, and on the way
            // there and back, not only once expanded.
            .when(fills, |container| container.flex_1().min_h_0())
            .child(self.render_project_context(window, cx).flex_none())
            .child(composer_box)
    }
}

/// One row of the slash command palette.
/// A `/btw` side thread streamed into the card above the composer. The
/// newest turn is the one in flight; earlier turns are replayed on a
/// follow-up.
pub(super) struct SideQuestionPanel {
    /// Id of the request that streams into the last turn.
    pub(super) request_id: String,
    pub(super) turns: Vec<SideThreadTurn>,
    /// Bumped per chunk so the markdown cache re-parses the last answer.
    pub(super) revision: u64,
    pub(super) pending: bool,
    pub(super) error: Option<SharedString>,
}

/// One turn of the side thread as the card shows it. The question is a
/// `SharedString` so the render clones it by refcount.
pub(super) struct SideThreadTurn {
    pub(super) question: SharedString,
    pub(super) answer: String,
}

pub(super) struct SlashEntry {
    pub(super) name: String,
    pub(super) description: String,
}

/// Built-in and skill commands matching a "/" token, capped for the popup.
pub(super) fn slash_entries_for(token: &str, skills: &[AgentSlashCommand]) -> Vec<SlashEntry> {
    let query = token.to_lowercase();
    [
        ("btw", "chat-slash-btw"),
        ("compact", "chat-slash-compact"),
        ("new", "chat-slash-new"),
        ("pin", "chat-slash-pin"),
        ("model", "chat-slash-model"),
        ("help", "chat-slash-help"),
    ]
    .into_iter()
    .map(|(name, description)| SlashEntry {
        name: name.to_string(),
        description: tr(description).to_string(),
    })
    .chain(skills.iter().map(|command| SlashEntry {
        name: command.name.clone(),
        description: command.description.clone(),
    }))
    .filter(|entry| entry.name.to_lowercase().starts_with(&query))
    .take(8)
    .collect()
}

/// One control in the composer chip row. `detail` is a second, quieter
/// value after the label (the model's effort); with one, the label gives
/// way first when the row runs out of room, so the detail stays readable.
/// `active` means its menu is open.
fn chip(
    id: &'static str,
    leading: Option<&'static str>,
    label: impl Into<SharedString>,
    detail: Option<(SharedString, gpui::Hsla, usize)>,
    chevron: bool,
    active: bool,
) -> crate::ui::motion::HoverBackground {
    let label = label.into();
    let shrinks = detail.is_some();
    chip_frame(id, leading, label.clone(), active)
        .when(shrinks, |el| el.min_w_0())
        .child(
            div()
                .whitespace_nowrap()
                .when(shrinks, |label| label.min_w_0().truncate())
                .child(label),
        )
        // The detail morphs from one value to the next (High to Maximum)
        // while `subject` stays the same; another subject's value swaps in.
        .children(detail.map(|(detail, color, subject)| {
            div()
                .flex_none()
                .whitespace_nowrap()
                .child(crate::ui::morph_text::morph_text((id, subject), detail).color(color))
        }))
        .when(chevron, |el| {
            el.child(icon("chevron-down", px(14.), chip_color(active)).flex_none())
        })
}

/// The least a long computer name shortens to in the project chip: about
/// eight letters and the ellipsis.
const COMPUTER_NAME_FLOOR: gpui::Pixels = px(64.);

/// A new task's project chip. It has the row above the composer to itself
/// and shows its text whole while that row has room. When it does not, the
/// text gives way: on a paired computer a long computer name shortens
/// first, down to its first several letters, and then the folder, which
/// tells projects apart. A short computer name stays whole.
fn project_chip(
    leading: &'static str,
    computer: Option<ProjectComputer>,
    project: SharedString,
    active: bool,
) -> crate::ui::motion::HoverBackground {
    let name = computer
        .as_ref()
        .map_or_else(|| project.clone(), |computer| computer.caption.clone());
    chip_frame("root-picker", Some(leading), name, active)
        .max_w_full()
        .min_w_0()
        .when_some(computer, |el, computer| {
            el.child(
                div()
                    .map(|name| {
                        if computer.shortens {
                            // Room for the first several letters, so the
                            // computer stays recognizable beside a long folder.
                            name.min_w(COMPUTER_NAME_FLOOR)
                                .flex_shrink(1000.)
                                .truncate()
                        } else {
                            name.flex_none().whitespace_nowrap()
                        }
                    })
                    .debug_selector(|| "project-computer".to_string())
                    .child(computer.name),
            )
            .child(div().flex_none().child("·"))
        })
        .child(
            div()
                .min_w_0()
                .truncate()
                .debug_selector(|| "project-name".to_string())
                .child(project),
        )
        .child(icon("chevron-down", px(14.), chip_color(active)))
}

fn chip_color(active: bool) -> u32 {
    if active {
        theme::text_primary()
    } else {
        theme::text_secondary()
    }
}

/// What every chip shares: the button, its accessible name, its states and
/// its leading icon.
fn chip_frame(
    id: &'static str,
    leading: Option<&'static str>,
    name: SharedString,
    active: bool,
) -> crate::ui::motion::HoverBackground {
    let color = chip_color(active);
    div()
        .id(id)
        .debug_selector(|| id.to_string())
        .role(gpui::Role::Button)
        .aria_label(name)
        .h_8()
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        // Concentric with the composer box: 24px radius minus the 8px inset.
        .rounded(theme::RADIUS_LG)
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(gpui::rgb(color))
        .when(active, |el| el.bg(gpui::rgb(theme::bg_sidebar_pill())))
        .map(|element| {
            let rest: gpui::Hsla = if active {
                gpui::rgb(theme::bg_sidebar_pill()).into()
            } else {
                gpui::transparent_black()
            };
            crate::ui::motion::hover_background(element, rest, gpui::rgb(theme::bg_sidebar_pill()))
        })
        .hover(|style| style.cursor_pointer())
        .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_row_selected())))
        .children(leading.map(|name| icon(name, px(16.), color)))
}

/// The Popular badge on a model row: coral on dark, grey on light (see
/// `badge_fill`), held to 4.5:1 by the contrast test.
fn popular_badge() -> Div {
    div()
        .flex_none()
        .px_1p5()
        .py(px(1.))
        .rounded_full()
        .bg(gpui::rgb(theme::badge_fill()))
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(gpui::rgb(theme::badge_text()))
        .child(tr("chat-model-popular"))
}

/// How the catalog presents the models: the panel's sections in display
/// order, each model's section, and the models it marks Popular. The server
/// owns all three, so a model deployed without an app release lists where
/// its release put it.
#[derive(Clone, Debug, Default)]
pub(super) struct ModelSections {
    groups: Vec<maple_sdk::CatalogGroup>,
    group_of: HashMap<String, String>,
    popular: HashSet<String>,
}

/// The badge string the catalog marks a popular model with.
const POPULAR_BADGE: &str = "Popular";

impl ModelSections {
    pub(super) fn from_catalog(catalog: &maple_sdk::ModelCatalogResponse) -> Self {
        Self {
            groups: catalog.groups.clone(),
            group_of: catalog
                .data
                .iter()
                .filter_map(|model| Some((model.id.clone(), model.group.clone()?)))
                .collect(),
            popular: catalog
                .data
                .iter()
                .filter(|model| model.badges.iter().any(|badge| badge == POPULAR_BADGE))
                .map(|model| model.id.clone())
                .collect(),
        }
    }

    pub(super) fn is_popular(&self, model: &str) -> bool {
        self.popular.contains(model)
    }

    /// `models` in their sections: the catalog's groups in its order, each
    /// holding its models in the order `models` lists them, then the models
    /// without a served group. When no model has one (an older server, or a
    /// catalog that failed to load) the list is one unnamed section.
    pub(super) fn group<'a>(
        &self,
        models: &'a [String],
    ) -> Vec<(Option<SharedString>, Vec<&'a str>)> {
        let served = |model: &str| {
            self.group_of
                .get(model)
                .filter(|id| self.groups.iter().any(|group| &group.id == *id))
        };
        let rest: Vec<&str> = models
            .iter()
            .map(String::as_str)
            .filter(|model| served(model).is_none())
            .collect();
        if rest.len() == models.len() {
            return vec![(None, rest)];
        }
        let mut sections: Vec<_> = self
            .groups
            .iter()
            .filter_map(|group| {
                let members: Vec<&str> = models
                    .iter()
                    .map(String::as_str)
                    .filter(|model| served(model) == Some(&group.id))
                    .collect();
                (!members.is_empty()).then(|| (Some(group_heading(group)), members))
            })
            .collect();
        if !rest.is_empty() {
            sections.push((Some(tr("chat-model-group-more")), rest));
        }
        sections
    }
}

/// A section's heading: the app's translation for a group it knows, and
/// the catalog's English label for one added since this build.
fn group_heading(group: &maple_sdk::CatalogGroup) -> SharedString {
    match group.id.as_str() {
        "capable" => tr("chat-model-group-capable"),
        "uncensored" => tr("chat-model-group-uncensored"),
        "fast" => tr("chat-model-group-fast"),
        _ => group.label.clone().into(),
    }
}

#[cfg(test)]
mod resize_tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use maple_agent::agent::AgentTimelineItem;
    use std::time::{Duration, Instant};

    struct Host {
        chat: Entity<ChatScreen>,
    }

    impl Render for Host {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(1200.))
                .h(px(800.))
                .flex()
                .flex_col()
                .child(self.chat.clone())
        }
    }

    /// A chat whose transcript holds `rows` messages and follows its
    /// tail; none shows the empty state.
    fn chat_window(
        cx: &mut TestAppContext,
        rows: usize,
    ) -> (Entity<ChatScreen>, &mut VisualTestContext) {
        cx.executor().allow_parking();
        let backend = {
            let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
            Arc::new(
                crate::backend::AgentBackend::for_test("http://127.0.0.1:9".into(), String::new())
                    .expect("backend"),
            )
        };
        let chat = cx.new(|cx| {
            let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
            let mut chat = ChatScreen::new_without_start(backend, "user".to_string(), cx);
            chat.selected_session = Some("s1".to_string());
            chat.booting = false;
            chat.trust_prompts = false;
            chat.sidebar_collapsed = true;
            chat.sidebar
                .update(cx, |sidebar, _| sidebar.reset_persisted_for_test());
            chat.replace_timeline(
                (0..rows)
                    .map(|index| AgentTimelineItem {
                        label: None,
                        persisted: None,
                        live: None,
                        truncated: None,
                        revision: 0,
                        id: format!("m{index}"),
                        item_type: "message".to_string(),
                        role: None,
                        title: None,
                        text: Some(format!("Message {index}\n\n{}", "lorem ipsum ".repeat(30))),
                        status: None,
                        input: None,
                        output: None,
                        created_ms: 0,
                        merge: "replace".to_string(),
                    })
                    .collect(),
            );
            chat
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| Host { chat: chat.clone() });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        cx.run_until_parked();
        (chat, cx)
    }

    fn composer_height(cx: &mut VisualTestContext) -> gpui::Pixels {
        cx.debug_bounds("composer-box")
            .expect("composer")
            .size
            .height
    }

    /// Space between the newest transcript row and the composer: constant
    /// while the transcript keeps its tail on screen.
    fn tail_gap(cx: &mut VisualTestContext) -> gpui::Pixels {
        let newest = cx
            .debug_bounds("timeline-row-m39")
            .expect("the newest row is on screen");
        cx.debug_bounds("composer-box").expect("composer").top() - newest.bottom()
    }

    fn toggle(chat: &Entity<ChatScreen>, cx: &mut VisualTestContext) {
        chat.update(cx, |chat, cx| chat.toggle_composer_expanded(cx));
        cx.run_until_parked();
    }

    fn frame_at(at: Instant, cx: &mut VisualTestContext) {
        motion::set_test_now(Some(at));
        cx.update(|window, app| window.simulate_next_frame(app));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn expanding_the_composer_eases_reverses_and_keeps_the_tail(cx: &mut TestAppContext) {
        let start = Instant::now();
        motion::set_test_now(Some(start));
        cx.update(|cx| cx.set_reduce_motion(false));
        let (chat, cx) = chat_window(cx, 40);
        let rest = composer_height(cx);
        let pinned = tail_gap(cx);

        toggle(&chat, cx);
        assert_eq!(
            composer_height(cx),
            rest,
            "starts from the height on screen"
        );
        frame_at(start + motion::PANEL - Duration::from_millis(1), cx);
        let landing = composer_height(cx);
        assert_eq!(tail_gap(cx), pinned, "the transcript keeps its tail");
        frame_at(start + motion::PANEL, cx);
        let expanded = composer_height(cx);
        assert!(expanded > rest * 2., "expanded: {expanded:?}");
        assert!(
            (expanded - landing).abs() < px(1.),
            "lands on the resting layout: {landing:?} then {expanded:?}"
        );
        assert_eq!(tail_gap(cx), pinned, "the transcript keeps its tail");

        // Shrink, then expand again halfway down.
        let shrink = start + motion::PANEL * 2;
        motion::set_test_now(Some(shrink));
        toggle(&chat, cx);
        frame_at(shrink + motion::PANEL / 2, cx);
        let partway = composer_height(cx);
        assert!(partway > rest && partway < expanded, "partway: {partway:?}");
        assert_eq!(tail_gap(cx), pinned, "the tail stays put mid-flight");
        toggle(&chat, cx);
        assert_eq!(
            composer_height(cx),
            partway,
            "a reversal starts where it is"
        );
        frame_at(shrink + motion::PANEL / 2 + Duration::from_millis(30), cx);
        assert!(composer_height(cx) > partway, "heading back up");
        frame_at(shrink + motion::PANEL * 2, cx);
        assert_eq!(composer_height(cx), expanded);
        assert!(chat.read_with(cx, |chat, _| chat.composer_resize.is_none()));

        // Reduced motion switches at once.
        cx.update(|_window, app| app.set_reduce_motion(true));
        toggle(&chat, cx);
        assert_eq!(composer_height(cx), rest);
        assert_eq!(tail_gap(cx), pinned);
        motion::set_test_now(None);
    }

    #[gpui::test]
    fn expanding_from_the_empty_state_folds_the_hero_without_a_jump(cx: &mut TestAppContext) {
        let start = Instant::now();
        motion::set_test_now(Some(start));
        cx.update(|cx| cx.set_reduce_motion(false));
        let (chat, cx) = chat_window(cx, 0);
        let rest = cx.debug_bounds("composer-box").expect("composer");

        toggle(&chat, cx);
        assert_eq!(
            cx.debug_bounds("composer-box"),
            Some(rest),
            "the heading and the note are still there on the first frame"
        );
        frame_at(start + motion::PANEL / 2, cx);
        let partway = composer_height(cx);
        assert!(partway > rest.size.height, "partway: {partway:?}");
        frame_at(start + motion::PANEL - Duration::from_millis(1), cx);
        let landing = cx.debug_bounds("composer-box").expect("composer");
        frame_at(start + motion::PANEL, cx);
        let expanded = cx.debug_bounds("composer-box").expect("composer");
        assert!(
            (expanded.top() - landing.top()).abs() < px(1.),
            "{landing:?} {expanded:?}"
        );
        assert!(
            (expanded.size.height - landing.size.height).abs() < px(1.),
            "{landing:?} {expanded:?}"
        );

        let shrink = start + motion::PANEL * 2;
        motion::set_test_now(Some(shrink));
        toggle(&chat, cx);
        assert_eq!(cx.debug_bounds("composer-box"), Some(expanded));
        // On the way back the heading rides above the composer at its full
        // size: the folding box never cuts it against the composer's edge.
        for step in 1..8 {
            frame_at(shrink + motion::PANEL * step / 8, cx);
            let composer = cx.debug_bounds("composer-box").expect("composer");
            let title = cx
                .debug_bounds("empty-title")
                .expect("the heading draws on the way back");
            assert!(
                title.bottom() <= composer.top(),
                "the heading stays clear of the composer: {title:?} {composer:?}"
            );
        }
        frame_at(shrink + motion::PANEL - Duration::from_millis(1), cx);
        let landing = cx.debug_bounds("composer-box").expect("composer");
        frame_at(shrink + motion::PANEL, cx);
        assert!(
            (rest.top() - landing.top()).abs() < px(1.),
            "{landing:?} {rest:?}"
        );
        assert_eq!(cx.debug_bounds("composer-box"), Some(rest));
        motion::set_test_now(None);
    }

    /// A new task has no session yet. Shrinking its expanded composer starts
    /// from the expanded box and eases down to rest, so nothing around it
    /// jumps, whatever the composer brings along for a new task.
    #[gpui::test]
    fn a_new_task_shrinks_its_composer_from_where_it_was(cx: &mut TestAppContext) {
        let start = Instant::now();
        motion::set_test_now(Some(start));
        cx.update(|cx| cx.set_reduce_motion(false));
        let (chat, cx) = chat_window(cx, 0);
        chat.update(cx, |chat, cx| {
            chat.selected_session = None;
            cx.notify();
        });
        cx.run_until_parked();
        let rest = cx.debug_bounds("composer-box").expect("composer");
        toggle(&chat, cx);
        frame_at(start + motion::PANEL * 2, cx);
        let expanded = cx.debug_bounds("composer-box").expect("composer");
        assert!(
            expanded.size.height > rest.size.height,
            "{rest:?} {expanded:?}"
        );

        let shrink = start + motion::PANEL * 3;
        motion::set_test_now(Some(shrink));
        toggle(&chat, cx);
        assert_eq!(
            cx.debug_bounds("composer-box"),
            Some(expanded),
            "shrinking starts from the expanded box"
        );
        let mut heights = vec![expanded.size.height];
        for step in 1..8 {
            frame_at(shrink + motion::PANEL * step / 8, cx);
            heights.push(composer_height(cx));
        }
        assert!(
            heights.windows(2).all(|pair| pair[1] <= pair[0]),
            "it only shrinks: {heights:?}"
        );
        assert!(
            heights[1] > rest.size.height,
            "it eases rather than snapping to rest: {heights:?}"
        );
        frame_at(shrink + motion::PANEL * 2, cx);
        assert_eq!(
            cx.debug_bounds("composer-box"),
            Some(rest),
            "it lands at rest"
        );
        motion::set_test_now(None);
    }

    /// The first message carries the composer from the empty state down to
    /// the foot of the conversation: it starts where it was drawn, moves
    /// only down, lands exactly at rest, and then asks for no frames. New
    /// Task under reduced motion puts it straight back.
    #[gpui::test]
    fn leaving_the_empty_state_glides_the_composer_down(cx: &mut TestAppContext) {
        let start = Instant::now();
        motion::set_test_now(Some(start));
        cx.update(|cx| cx.set_reduce_motion(false));
        let (chat, cx) = chat_window(cx, 0);
        let hero = cx.debug_bounds("composer-box").expect("composer");

        let message = |chat: &mut ChatScreen| {
            chat.replace_timeline(vec![AgentTimelineItem {
                label: None,
                persisted: None,
                live: None,
                truncated: None,
                revision: 0,
                id: "m0".to_string(),
                item_type: "message".to_string(),
                role: None,
                title: None,
                text: Some("Hello".to_string()),
                status: None,
                input: None,
                output: None,
                created_ms: 0,
                merge: "replace".to_string(),
            }]);
        };
        chat.update(cx, |chat, cx| {
            message(chat);
            cx.notify();
        });
        cx.run_until_parked();
        let first = cx.debug_bounds("composer-box").expect("composer");
        assert!(
            (first.top() - hero.top()).abs() < px(1.),
            "starts where it was drawn: {hero:?} {first:?}"
        );
        let mut last = first.top();
        for step in 1..=12 {
            frame_at(start + motion::PANEL * step / 12, cx);
            let top = cx.debug_bounds("composer-box").expect("composer").top();
            assert!(top >= last, "only moves down: {last:?} then {top:?}");
            last = top;
        }
        frame_at(start + motion::PANEL * 2, cx);
        let rest = cx.debug_bounds("composer-box").expect("composer");
        assert!(rest.top() > hero.top() + px(100.), "{hero:?} {rest:?}");
        assert_eq!(cx.update(|window, app| window.simulate_next_frame(app)), 0);

        cx.update(|_, cx| cx.set_reduce_motion(true));
        chat.update(cx, |chat, cx| {
            chat.replace_timeline(Vec::new());
            cx.notify();
        });
        cx.run_until_parked();
        let back = cx.debug_bounds("composer-box").expect("composer");
        assert!(
            (back.top() - hero.top()).abs() < px(1.),
            "reduced motion lands at once: {hero:?} {back:?}"
        );
        motion::set_test_now(None);
    }
}
