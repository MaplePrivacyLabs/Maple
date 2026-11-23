//! The chat pane chrome: the header and its menus, the slash palette,
//! the plan and side-question cards, and the composer itself.

use crate::ui::motion::HoverBg as _;
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
    ChatPopup, ChatScreen, DraftAttachment, OpenSettingsSection, ProjectComputer, Section,
};
use crate::ui::i18n::{UiText, tr};
use crate::ui::icons::{icon, spinner, spinner_with_id};
use crate::ui::markdown;
use crate::ui::motion;
use crate::ui::popup::{Menu, MenuItem, Placement};
use crate::ui::text_input::vim::VimMode;
use crate::ui::theme;
use crate::ui::titlebar;
use crate::ui::widgets;

/// The composer's controls application Vim walks. A control is reachable
/// from the keyboard once it has an entry here, a rendering, and an action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ComposerControl {
    /// `+`: a new task's menu (permissions, integrations, attach), or a
    /// conversation's attach popover.
    Add,
    Model,
    Dictate,
    /// The integrations that are on: a conversation's frame, or a new
    /// task's row once any is on.
    Integrations,
    /// The permission mode: a conversation's frame, or a new task's row
    /// once it is not the default.
    Mode,
    /// A conversation's context bar, once the task has used some.
    Context,
    /// A conversation's Compact, once the window nears its limit.
    Compact,
}

impl ComposerControl {
    /// A conversation's: the box's row, then its frame.
    const ALL: [Self; 7] = [
        Self::Add,
        Self::Model,
        Self::Dictate,
        Self::Integrations,
        Self::Mode,
        Self::Context,
        Self::Compact,
    ];
    /// A new task's one row, its tokens after the model.
    const NEW_TASK: [Self; 5] = [
        Self::Add,
        Self::Model,
        Self::Integrations,
        Self::Mode,
        Self::Dictate,
    ];
}

/// What the composer's one round button does now. It names the most
/// likely next step: dictate into an empty box, send a draft, queue a
/// draft behind a run, or stop the run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PrimaryAction {
    /// Nothing drafted and dictation available: the button is the mic.
    Dictate,
    /// Nothing drafted and no dictation: a muted Send, waiting for a draft.
    Idle,
    Send,
    /// A run is going and a draft is waiting: it goes out next turn.
    Queue,
    Stop,
    /// Dictation is listening; pressing stops it.
    Recording,
    /// Something is under way that the button waits for: transcribing,
    /// starting, or files still being prepared.
    Busy,
}

/// The order a context window holds its parts, which the context card's
/// bar and legend both follow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ContextPart {
    Instructions,
    Summary,
    Messages,
    ToolResults,
    Images,
}

impl ContextPart {
    pub(super) const ALL: [Self; 5] = [
        Self::Instructions,
        Self::Summary,
        Self::Messages,
        Self::ToolResults,
        Self::Images,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Instructions => "chat-context-instructions",
            Self::Summary => "chat-context-summary",
            Self::Messages => "chat-context-messages",
            Self::ToolResults => "chat-context-tool-results",
            Self::Images => "chat-context-images",
        }
    }

    pub(super) fn color(self) -> u32 {
        match self {
            Self::Instructions => theme::context_instructions(),
            Self::Summary => theme::context_summary(),
            Self::Messages => theme::context_messages(),
            Self::ToolResults => theme::context_tool_results(),
            Self::Images => theme::context_images(),
        }
    }
}

/// How full the context window is, as the bar and card color it: quiet
/// until it nears the limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ContextLevel {
    Room,
    Near,
    Full,
}

/// A share of the window in whole percent, rounded down as the plan bar's
/// is, so it reads 100% only once full. The nudge keeps a share such as
/// 29 of 100, which `f32` holds as 0.2899…, at 29.
pub(super) fn whole_percent(fraction: f32) -> u32 {
    (fraction * 100. + 1e-3).floor().clamp(0., 100.) as u32
}

impl ContextLevel {
    /// The level of the percentage the bar writes, so "75%" is never quiet
    /// and "90%" never amber.
    pub(super) fn of(fraction: f32) -> Self {
        match whole_percent(fraction) {
            90.. => Self::Full,
            75.. => Self::Near,
            _ => Self::Room,
        }
    }

    /// The level's ink: the bar's fill, the percentage, Compact.
    pub(super) fn color(self) -> u32 {
        match self {
            Self::Room => theme::meter_quiet(),
            Self::Near => theme::status_warning(),
            Self::Full => theme::status_error(),
        }
    }
}

/// Each mode's icon. None of them means anything else in Maple: `lock` is
/// the encryption mark and an untrusted folder, `shield-check` a trusted
/// folder, and `zap` a warning.
pub(super) fn mode_icon(mode: AgentPermissionMode) -> &'static str {
    match mode {
        AgentPermissionMode::ReadOnly => "eye",
        AgentPermissionMode::Auto => "wand-sparkles",
        AgentPermissionMode::FullAccess => "key-round",
    }
}

pub(super) fn mode_label(mode: AgentPermissionMode) -> &'static str {
    match mode {
        AgentPermissionMode::ReadOnly => "chat-mode-read-only",
        AgentPermissionMode::Auto => "chat-mode-auto",
        AgentPermissionMode::FullAccess => "chat-mode-full-access",
    }
}

/// The chip's accessible name: its mode alone does not say what it sets.
pub(super) fn mode_chip_label(mode: AgentPermissionMode) -> &'static str {
    match mode {
        AgentPermissionMode::ReadOnly => "chat-mode-read-only-chip",
        AgentPermissionMode::Auto => "chat-mode-auto-chip",
        AgentPermissionMode::FullAccess => "chat-mode-full-access-chip",
    }
}

pub(super) fn mode_note(mode: AgentPermissionMode) -> &'static str {
    match mode {
        AgentPermissionMode::ReadOnly => "chat-mode-read-only-note",
        AgentPermissionMode::Auto => "chat-mode-auto-note",
        AgentPermissionMode::FullAccess => "chat-mode-full-access-note",
    }
}

/// The share of the chat column the composer region takes when expanded.
/// The layout and the expand transition both read it, so the transition
/// always lands on the height the layout then holds.
pub(super) const EXPANDED_SHARE: f32 = 0.7;
/// The expand button's side, in rems. The composer's text row is never
/// shorter than this button beside it, which the expand transition allows
/// for when it works out the box's height at rest.
pub(super) const EXPAND_BUTTON_REMS: f32 = 1.5;
/// The empty state's vertical padding while the composer fills its pane, in
/// rems, on each side.
pub(super) const EMPTY_PANE_PADDING_REMS: f32 = 1.5;

/// Heights the composer expand/shrink transition eases between, recorded
/// as each frame is laid out so the next one can start from the screen.
#[derive(Clone, Copy, Default)]
pub(super) struct ComposerFrame {
    /// The composer region: cards, composer box, palette, padding.
    pub(super) region: Option<gpui::Pixels>,
    /// A conversation's strip: its width, how wide its two groups are at
    /// each [`StripFit`](super::composer_frame::StripFit), and the words
    /// they were measured with.
    pub(super) strip_width: Option<gpui::Pixels>,
    pub(super) strip_left: [Option<gpui::Pixels>; 4],
    pub(super) strip_right: [Option<gpui::Pixels>; 4],
    pub(super) strip_words: u64,
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
                    |this, _, cx| this.project_menu(cx),
                )
                .min_w_0()
                .max_w_full(),
            )
    }

    /// `button` as the opener of `popup`, with `menu` attached below or
    /// above it while the popup is open.
    pub(super) fn with_menu<E: gpui::StatefulInteractiveElement + IntoElement>(
        &self,
        popup: ChatPopup,
        button: E,
        placement: Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
        menu: impl FnOnce(&Self, &mut Window, &mut Context<Self>) -> Menu<Self>,
    ) -> Div {
        let menu = self.popup.is_open(&popup).then(|| {
            let menu = menu(self, window, cx);
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
            // From the keyboard, the permissions open on their control.
            ChatPopup::Mode if !self.popup.is_open(&popup) && window.last_input_was_keyboard() => {
                self.open_permissions(true, cx)
            }
            popup => self.toggle_popup(popup, cx),
        }
    }

    /// The controls the composer shows now, in application Vim's order:
    /// left to right as drawn. A new task's tokens sit between the model
    /// and dictation; a conversation's frame follows the box's row.
    pub(super) fn composer_controls(&self) -> impl Iterator<Item = ComposerControl> + '_ {
        let conversation = self.selected_session.is_some();
        let listening = self.recording || self.transcribing;
        let order: &[ComposerControl] = if conversation {
            &ComposerControl::ALL
        } else {
            &ComposerControl::NEW_TASK
        };
        order.iter().copied().filter(move |control| match control {
            // While dictation listens, its row takes the place of `+`
            // and the model.
            ComposerControl::Add | ComposerControl::Model => !listening,
            // Only while it is drawn: during a run the round button is Stop.
            ComposerControl::Dictate => self.audio_caps.transcription && self.dictation_drawn,
            ComposerControl::Integrations => {
                conversation || (!listening && self.integrations_token_shown())
            }
            ComposerControl::Mode => conversation || (!listening && self.mode_token_shown()),
            ComposerControl::Context => conversation && self.context_shown(),
            ComposerControl::Compact => conversation && self.compact_offered(),
        })
    }

    /// A new task shows its integrations beside the model once any is on,
    /// and keeps the token while its menu is open, so switching the last
    /// one off does not pull the menu away.
    fn integrations_token_shown(&self) -> bool {
        self.mcp_enabled_count > 0 || self.popup.is_open(&ChatPopup::Integrations)
    }

    /// A new task shows its permission mode beside the model once it is not
    /// Auto, and keeps the token while its menu is open.
    fn mode_token_shown(&self) -> bool {
        self.permission_mode != AgentPermissionMode::Auto || self.popup.is_open(&ChatPopup::Mode)
    }

    /// Press `control` from the keyboard, as a click would. A menu opens on
    /// its current choice, the way a native popup button does.
    pub(super) fn activate_composer_control(
        &mut self,
        control: ComposerControl,
        cx: &mut Context<Self>,
    ) {
        match control {
            ComposerControl::Add => self.open_add(cx),
            ComposerControl::Model => self.open_model_panel(cx),
            ComposerControl::Dictate => self.toggle_recording(cx),
            ComposerControl::Integrations => {
                self.popup.open(ChatPopup::Integrations, cx);
                self.refresh_session_mcp(cx);
            }
            ComposerControl::Mode => self.open_permissions(true, cx),
            ComposerControl::Context => self.open_context_card(true, cx),
            ComposerControl::Compact => self.compact_now(cx),
        }
    }

    /// `+`: a new task's menu, or a conversation's attach popover.
    pub(super) fn open_add(&mut self, cx: &mut Context<Self>) {
        if self.selected_session.is_some() {
            self.popup.open(ChatPopup::Attach, cx);
        } else {
            self.add_page = None;
            self.popup.open(ChatPopup::Add, cx);
        }
    }

    /// The `+` button. A new task's opens the menu with permissions,
    /// integrations and attach; a conversation keeps those in its frame,
    /// so its `+` only attaches.
    fn render_add_button(
        &self,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let conversation = self.selected_session.is_some();
        let popup = if conversation {
            ChatPopup::Attach
        } else {
            ChatPopup::Add
        };
        let open = self.popup.is_open(&popup);
        let label = if conversation {
            tr("chat-attach")
        } else {
            tr("chat-add")
        };
        let button = div()
            .id("composer-add")
            .debug_selector(|| "composer-add".to_string())
            .role(gpui::Role::Button)
            .aria_label(label.clone())
            .tab_index(0)
            .focus_visible(widgets::focus_ring)
            .on_key_down(ChatScreen::sidebar_button_key_down)
            .size_8()
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::RADIUS_LG)
            .when(open, |el| el.bg(gpui::rgb(theme::bg_sidebar_pill())))
            .hover_bg(gpui::rgb(theme::bg_sidebar_pill()))
            .hover(|style| style.cursor_pointer())
            .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_row_selected())))
            .when(self.attachment_picking, |el| el.opacity(0.5))
            .when(!open, |el| el.tooltip(widgets::tooltip(label, None)))
            .child(icon("plus", px(18.), chip_color(open)));
        let menu = self.popup.is_open(&popup).then(|| {
            let menu = if conversation {
                self.attach_menu(window, cx)
            } else {
                self.add_menu(window, cx)
            };
            self.popup.render(menu, Placement::AboveStart, window, cx)
        });
        vim_mark(
            div()
                .relative()
                .child(self.popup.trigger(popup, button, cx, |this, _, cx| {
                    if this.popup.is_open(&ChatPopup::Add) || this.popup.is_open(&ChatPopup::Attach)
                    {
                        this.popup.close(cx);
                    } else {
                        this.open_add(cx);
                    }
                }))
                .children(menu),
            selected,
        )
    }

    /// The model chip, with the effort level riding on it.
    fn render_model_chip(
        &self,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
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
        vim_mark(
            self.with_menu(
                ChatPopup::Model,
                button,
                Placement::AboveStart,
                window,
                cx,
                |this, _, cx| this.model_menu(cx),
            ),
            selected,
        )
        // The model's name truncates before the row squeezes Send.
        .min_w_0()
    }

    /// A new task's quiet tokens beside the model: the integrations once
    /// any is on, and the permission mode once it is not the default. A
    /// conversation shows both in its frame instead.
    fn render_new_task_tokens(
        &self,
        selected: Option<ComposerControl>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut tokens = Vec::new();
        if self.integrations_token_shown() {
            let open = self.popup.is_open(&ChatPopup::Integrations);
            let button = token_frame("mcp-menu", false, open)
                .aria_label(self.integrations_count_label())
                .when(!open, |el| {
                    el.tooltip(widgets::tooltip(self.integrations_tooltip(), None))
                })
                .child(icon("puzzle", px(14.), theme::text_secondary()))
                .child(self.integrations_count_text());
            tokens.push(
                vim_mark(
                    self.with_menu(
                        ChatPopup::Integrations,
                        button,
                        Placement::AboveStart,
                        window,
                        cx,
                        |this, _, _| this.integrations_menu(),
                    ),
                    selected == Some(ComposerControl::Integrations),
                )
                .into_any_element(),
            );
        }
        if self.mode_token_shown() {
            let warm = self.permission_mode == AgentPermissionMode::FullAccess;
            let open = self.popup.is_open(&ChatPopup::Mode);
            let ink = if warm {
                theme::accent_ink()
            } else {
                theme::text_secondary()
            };
            let button = token_frame("permission-mode-menu", warm, open)
                .aria_label(tr(mode_chip_label(self.permission_mode)))
                .when(!open, |el| {
                    el.tooltip(widgets::tooltip(tr("chat-mode"), None))
                })
                .child(icon(mode_icon(self.permission_mode), px(14.), ink))
                .child(
                    div()
                        .whitespace_nowrap()
                        .child(tr(mode_label(self.permission_mode))),
                );
            tokens.push(
                vim_mark(
                    self.with_menu(
                        ChatPopup::Mode,
                        button,
                        Placement::AboveStart,
                        window,
                        cx,
                        |this, window, cx| this.permissions_menu(window, cx),
                    ),
                    selected == Some(ComposerControl::Mode),
                )
                .into_any_element(),
            );
        }
        tokens
    }

    /// What the round button does now, from the draft and the run.
    pub(super) fn primary_action(
        &self,
        running: bool,
        pending_create: bool,
        disabled: bool,
        drafting: bool,
        can_send: bool,
    ) -> PrimaryAction {
        // Only a message waits for a run. A command runs at once, and while
        // the side thread is open a plain message is asked there at once.
        let asks_now = self.composer_is_command || self.btw.is_some();
        if self.recording {
            PrimaryAction::Recording
        } else if self.transcribing || self.recording_starting {
            PrimaryAction::Busy
        } else if running && !self.booting && !pending_create && !drafting {
            PrimaryAction::Stop
        } else if can_send {
            if running && !asks_now {
                PrimaryAction::Queue
            } else {
                PrimaryAction::Send
            }
        } else if drafting || disabled {
            PrimaryAction::Busy
        } else if self.audio_caps.transcription {
            PrimaryAction::Dictate
        } else {
            PrimaryAction::Idle
        }
    }

    /// The round button. Its fills and glyphs cross-fade over TOGGLE as
    /// it changes job, and it widens into a pill to say Queue; its role,
    /// name and handler switch at once.
    fn render_primary(
        &mut self,
        action: PrimaryAction,
        status_label: SharedString,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let now = motion::now();
        let reduced = cx.reduce_motion();
        let (queue, moving) = self.primary_width.step(
            if action == PrimaryAction::Queue {
                1.
            } else {
                0.
            },
            motion::TOGGLE,
            now,
            reduced,
        );
        if moving {
            window.request_animation_frame();
        }
        let (selector, label): (&'static str, Option<SharedString>) = match action {
            PrimaryAction::Dictate => ("record-voice", Some(tr("chat-dictate"))),
            PrimaryAction::Recording => ("record-voice", Some(tr("chat-stop-recording"))),
            PrimaryAction::Send => ("send-message", Some(tr("chat-send"))),
            PrimaryAction::Queue => ("send-message", Some(tr("chat-queue-next-turn"))),
            PrimaryAction::Stop => ("stop-run", Some(tr("chat-stop-run"))),
            PrimaryAction::Busy => ("send-message", Some(self.busy_label(status_label))),
            // A Send with nothing to send: named, and disabled.
            PrimaryAction::Idle => ("send-message", Some(tr("chat-send"))),
        };
        // One layer per look; each eases to on or off by itself, so a
        // change of job never shows two glyphs at full strength.
        let neutral = matches!(
            action,
            PrimaryAction::Dictate
                | PrimaryAction::Busy
                | PrimaryAction::Queue
                | PrimaryAction::Idle
        );
        // The arrow lifts away as a message goes out.
        let launched = self
            .send_launched
            .is_some_and(|at| now.saturating_duration_since(at) < motion::TOGGLE.settle);
        let layers = [
            primary_layer("composer-primary-neutral", neutral, |on| {
                fill(on, theme::bg_sidebar_pill()).into_any_element()
            }),
            primary_layer(
                "composer-primary-send",
                action == PrimaryAction::Send,
                |on| {
                    fill(on, theme::send_top())
                        .bg(gpui::linear_gradient(
                            180.,
                            gpui::linear_color_stop(gpui::rgb(theme::send_top()), 0.),
                            gpui::linear_color_stop(gpui::rgb(theme::send_bottom()), 1.),
                        ))
                        .into_any_element()
                },
            ),
            primary_layer(
                "composer-primary-stop",
                action == PrimaryAction::Stop,
                |on| fill(on, theme::bg_default_button()).into_any_element(),
            ),
            primary_layer(
                "composer-primary-recording",
                action == PrimaryAction::Recording,
                |on| {
                    if on <= 0. {
                        return fill(0., theme::status_error()).into_any_element();
                    }
                    // Listening breathes, so the button says it is live.
                    motion::ticker(
                        "composer-primary-breath",
                        motion::LISTENING_BREATH,
                        move |phase| {
                            fill(
                                on * motion::pulse(phase, 0., 0.78, 1.),
                                theme::status_error(),
                            )
                            .into_any_element()
                        },
                    )
                    .into_any_element()
                },
            ),
            primary_layer(
                "composer-primary-mic",
                action == PrimaryAction::Dictate,
                |on| glyph(on, scaled_icon("mic", theme::text_secondary(), on)),
            ),
            primary_layer(
                "composer-primary-arrow",
                action == PrimaryAction::Send && !launched,
                move |on| {
                    let lift = if launched { 1. - on } else { 0. };
                    motion::shifted(glyph(on, scaled_icon("arrow-up", theme::on_accent(), on)))
                        .offset(gpui::point(px(0.), px(-7. * lift)))
                        .into_any_element()
                },
            ),
            // Nothing to send and no dictation: Send, waiting for a draft.
            primary_layer(
                "composer-primary-idle",
                action == PrimaryAction::Idle,
                |on| glyph(on, scaled_icon("arrow-up", theme::text_muted(), on)),
            ),
            primary_layer(
                "composer-primary-stop-square",
                action == PrimaryAction::Stop,
                |on| glyph(on, square(theme::text_on_default_button())),
            ),
            primary_layer(
                "composer-primary-recording-square",
                action == PrimaryAction::Recording,
                |on| glyph(on, square(theme::on_accent())),
            ),
            primary_layer(
                "composer-primary-queue",
                action == PrimaryAction::Queue,
                |on| {
                    glyph(
                        on,
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .whitespace_nowrap()
                            .child(tr("chat-queue"))
                            .into_any_element(),
                    )
                },
            ),
        ];
        let busy = (action == PrimaryAction::Busy).then(|| {
            glyph(
                1.,
                spinner_with_id(
                    SharedString::new_static("spinner-composer-primary-busy"),
                    px(16.),
                    theme::text_secondary(),
                ),
            )
        });
        let shortcut = matches!(action, PrimaryAction::Send | PrimaryAction::Queue).then_some("↵");
        let pressable = !matches!(action, PrimaryAction::Busy | PrimaryAction::Idle);
        let button = div()
            .id("composer-primary")
            .debug_selector(move || selector.to_string())
            .when_some(label.clone(), |el, label| {
                el.role(gpui::Role::Button).aria_label(label)
            })
            .when(!pressable, |el| {
                el.a11y_synthetic_children(|tree| tree.parent_node().set_disabled())
            })
            .relative()
            .flex_none()
            .h_8()
            .w(px(32. + 40. * queue))
            .rounded_full()
            .children(layers)
            .children(busy)
            .when(pressable, |el| {
                el.hover(|style| style.opacity(0.9).cursor_pointer())
                    .active(|style| style.opacity(0.75))
                    .when_some(label, |el, label| {
                        el.tooltip(widgets::tooltip(label, shortcut))
                    })
                    .on_click(cx.listener(move |this, _event, _window, cx| match action {
                        PrimaryAction::Dictate | PrimaryAction::Recording => {
                            this.toggle_recording(cx)
                        }
                        PrimaryAction::Send | PrimaryAction::Queue => this.send_inner(cx),
                        PrimaryAction::Stop => this.stop(cx),
                        PrimaryAction::Busy | PrimaryAction::Idle => {}
                    }))
            });
        vim_mark(
            div().child(button),
            selected && matches!(action, PrimaryAction::Dictate | PrimaryAction::Recording),
        )
    }

    /// What the round button waits on while it shows Busy: the words it
    /// is transcribing, the microphone starting, files still being
    /// prepared, or else the composer's own wait (`status_label`).
    fn busy_label(&self, status_label: SharedString) -> SharedString {
        if self.transcribing {
            tr("chat-transcribing")
        } else if self.recording_starting {
            tr("chat-dictate")
        } else if !self.draft_attachments.iter().all(DraftAttachment::ready) {
            tr("chat-files-preparing")
        } else {
            status_label
        }
    }

    /// The mic beside Send while a draft waits: it slides in from under
    /// Send, whose job it held while the box was empty, and folds away the
    /// same way. Its slot eases open, so nothing jumps.
    fn render_mic_aside(
        &mut self,
        shown: bool,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let now = motion::now();
        let (on, moving) = self.mic_aside.step(
            if shown { 1. } else { 0. },
            motion::TOGGLE,
            now,
            cx.reduce_motion(),
        );
        if moving {
            window.request_animation_frame();
        }
        let eased = motion::ease_out_cubic(on);
        let frame = div()
            .id("record-voice-aside")
            .debug_selector(|| "record-voice-aside".to_string())
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::RADIUS_LG)
            .opacity(eased);
        let glyph = icon("mic", px(16.), theme::text_muted());
        let button = if shown {
            frame
                .role(gpui::Role::Button)
                .aria_label(tr("chat-dictate"))
                .hover_bg(gpui::rgb(theme::bg_sidebar_pill()))
                .hover(|style| style.cursor_pointer())
                .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_row_selected())))
                .tooltip(widgets::tooltip(tr("chat-dictate"), None))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.toggle_recording(cx);
                }))
                .child(glyph)
                .into_any_element()
        } else {
            // Folding away: drawn, but no longer a control.
            frame.child(glyph).into_any_element()
        };
        vim_mark(
            div()
                .relative()
                .flex_none()
                .h_8()
                .w(px(32. * eased))
                .mr(px(2. * eased))
                .child(
                    motion::shifted(div().absolute().inset_0().child(button))
                        .offset(gpui::point(px(10. * (1. - eased)), px(0.))),
                ),
            selected && shown,
        )
    }

    /// Dictation's row, in place of `+` and the model while it listens: a
    /// level wave, what it is doing, and for how long.
    fn render_listening(&self, cx: &mut Context<Self>) -> Div {
        let recording = self.recording;
        let since = self.recording_since;
        let label = if recording {
            tr("chat-listening")
        } else {
            tr("chat-transcribing")
        };
        let elapsed = since.filter(|_| recording).map(|since| {
            let seconds = motion::now().saturating_duration_since(since).as_secs();
            format!("{}:{:02}", seconds / 60, seconds % 60)
        });
        if recording {
            self.schedule_recording_tick(cx);
        }
        div()
            .debug_selector(|| "composer-listening".to_string())
            .flex()
            .items_center()
            .gap_2()
            .h_8()
            .px_2()
            .text_xs()
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(gpui::rgb(theme::text_secondary()))
            .when(recording, |row| {
                row.child(motion::ticker(
                    "composer-listening-wave",
                    motion::LISTENING_WAVE,
                    |phase| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(2.))
                            .h(px(18.))
                            .children((0..14).map(|bar| {
                                // A travelling wave: calm, never empty, never
                                // touching the row's edges.
                                let wave = ((phase * std::f32::consts::TAU) + bar as f32 * 0.9)
                                    .sin()
                                    .abs();
                                div()
                                    .w(px(2.))
                                    .h(px(4. + 12. * wave))
                                    .rounded_full()
                                    .bg(gpui::rgb(theme::status_error()))
                            }))
                            .into_any_element()
                    },
                ))
            })
            .when(!recording, |row| {
                row.child(spinner_with_id(
                    SharedString::new_static("spinner-composer-transcribing"),
                    px(12.),
                    theme::text_muted(),
                ))
            })
            .child(gpui::text!(label))
            .children(elapsed.map(|elapsed| {
                div()
                    .font_family(crate::assets::FONT_MONO)
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(elapsed)
            }))
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
                let new = self.model_sections.is_new(model);
                // The badge is drawn content; the row's name carries it for
                // assistive technology.
                let name: SharedString = if new {
                    self.messages
                        .get("chat-model-new-row", [("model", label.clone().into())])
                } else if popular {
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
                if popular || new {
                    let badge = if new {
                        tr("chat-model-new")
                    } else {
                        tr("chat-model-popular")
                    };
                    row.content(
                        div()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().min_w_0().truncate().child(label))
                            .child(model_badge(badge)),
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
                    .hover_bg(gpui::rgb(theme::bg_sidebar_row_hover()))
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
        self.sync_composer_placeholder(cx);
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
        self.sync_composer_placeholder(cx);
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
                    .hover_bg(gpui::rgb(theme::bg_elevated()))
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
            .hover_bg(gpui::rgb(theme::bg_elevated()))
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
            Some((at, generation)) if at.elapsed() < motion::REVEAL.duration * 2 => {
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
                // beside it (`EXPAND_BUTTON_REMS`).
                let (drawn, text) = self.composer.as_ref()?.read(cx).drawn_and_text_height()?;
                let row = text.max(window.rem_size() * EXPAND_BUTTON_REMS);
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
        self.sync_composer_placeholder(cx);
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
        let drafting = has_text || has_attachments;
        let conversation = self.selected_session.is_some();
        let listening = self.recording || self.transcribing;
        let primary = self.primary_action(running, pending_create, disabled, drafting, can_send);
        let usage_tray = self
            .usage_limit_reached()
            .then(|| self.render_usage_tray(cx));
        let queue_chips = self.render_queue(cx);
        let expanded = self.composer_expanded;
        // Layout follows the box while it eases; the icon and tooltip
        // name the state it is heading to.
        let fills = self.composer_fills();
        let now = motion::now();
        let reduced = cx.reduce_motion();
        // A hairline at rest; coral while the box has focus or one of its
        // own menus is open.
        let focused = !disabled && self.composer_focused(conversation, window, cx);
        let (focus, focus_moving) = self.composer_focus_ring.step(
            if focused { 1. } else { 0. },
            motion::TOGGLE,
            now,
            reduced,
        );
        // The expand button shows while the pointer is over the box, for a
        // draft of more than one line, and while expanded. It eases in
        // over ENTER and out, quicker, over HOVER.
        let multiline = self
            .composer
            .as_ref()
            .is_some_and(|input| input.read(cx).height_beyond_one_row() > px(0.5));
        let reveal_expand = self.composer_hovered || multiline || expanded || fills;
        let (expand_shown, expand_moving) = self.expand_reveal.step(
            if reveal_expand { 1. } else { 0. },
            if reveal_expand {
                motion::ENTER
            } else {
                motion::HOVER
            },
            now,
            reduced,
        );
        if focus_moving || expand_moving {
            window.request_animation_frame();
        }
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
        // While dictation listens its row takes the place of `+`, the
        // model and the tokens; otherwise they reveal together once the
        // model is known.
        let (controls, controls_held) = if listening {
            (self.render_listening(cx).into_any_element(), false)
        } else {
            let mut controls = vec![
                self.render_add_button(selected_control == Some(ComposerControl::Add), window, cx)
                    .into_any_element(),
                self.render_model_chip(
                    selected_control == Some(ComposerControl::Model),
                    window,
                    cx,
                )
                .into_any_element(),
            ];
            if !conversation {
                controls.extend(self.render_new_task_tokens(selected_control, window, cx));
            }
            self.reveal_composer_controls(controls)
        };
        let aside_shown = drafting && !listening && !self.recording_starting;
        self.dictation_drawn = self.audio_caps.transcription
            && (aside_shown
                || matches!(primary, PrimaryAction::Dictate | PrimaryAction::Recording));
        let aside = self.audio_caps.transcription.then(|| {
            let shown = aside_shown;
            self.render_mic_aside(
                shown,
                selected_control == Some(ComposerControl::Dictate),
                window,
                cx,
            )
        });
        let primary_button = self.render_primary(
            primary,
            status_label.clone(),
            selected_control == Some(ComposerControl::Dictate),
            window,
            cx,
        );
        let drafts = &self.draft_attachments;
        let upload = self
            .selected_upload()
            .map(|(id, upload)| (id.to_string(), upload.label.clone()));
        let border = gpui::rgb(theme::border()).into();
        let composer_box = div()
            .id("composer-box")
            .w_full()
            .flex()
            .flex_col()
            .relative()
            .debug_selector(|| "composer-box".to_string())
            .when(fills, |container| container.flex_1().min_h_0())
            .rounded(theme::RADIUS_XL)
            .bg(gpui::rgb(theme::bg_input()))
            .border_1()
            .border_color(if disabled {
                border
            } else {
                motion::mix(border, gpui::rgb(theme::composer_focus()).into(), focus)
            })
            // A new task's box floats on the page; a conversation's sits
            // in its frame, which carries the shadow instead.
            .when(!conversation, |container| {
                container.shadow(composer_shadow())
            })
            .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                if this.composer_hovered != *hovered {
                    this.composer_hovered = *hovered;
                    cx.notify();
                }
            }))
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
                    .child(self.render_expand_button(expanded, expand_shown, cx)),
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
                    .children(aside)
                    .child(primary_button),
            );
        if conversation {
            return self.render_composer_frame(composer_box, window, cx);
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

    /// Whether the box draws its focus line: its field has focus, or a menu
    /// opened from inside the box is open (focus is in the menu then). A
    /// conversation's frame sits outside the box, so its menus do not count.
    fn composer_focused(&self, conversation: bool, window: &Window, cx: &gpui::App) -> bool {
        let field = self.composer.as_ref().is_some_and(|input| {
            gpui::Focusable::focus_handle(input.read(cx), cx).contains_focused(window, cx)
        });
        let own_menu = [ChatPopup::Add, ChatPopup::Attach, ChatPopup::Model]
            .iter()
            .any(|popup| self.popup.is_open(popup))
            || (!conversation
                && (self.popup.is_open(&ChatPopup::Integrations)
                    || self.popup.is_open(&ChatPopup::Mode)));
        field || own_menu
    }

    /// The expand button: shown on hover, for a long draft and while
    /// expanded (`shown` eases between them), its glyph turning between
    /// expand and shrink. Hidden, it takes no clicks.
    fn render_expand_button(
        &self,
        expanded: bool,
        shown: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let label = if expanded {
            tr("chat-shrink-composer")
        } else {
            tr("chat-expand-composer")
        };
        let glyphs = expand_glyphs(expanded, shown);
        let button = div()
            .id("composer-expand")
            .debug_selector(|| "composer-expand".to_string())
            .size(gpui::rems(EXPAND_BUTTON_REMS))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::RADIUS_SM);
        if shown < 0.5 {
            // Keeps its place, so the field's width never changes.
            return button.child(glyphs).into_any_element();
        }
        button
            .role(gpui::Role::Button)
            .aria_label(label.clone())
            .hover_bg(gpui::rgb(theme::bg_sidebar_pill()))
            .hover(|style| style.cursor_pointer())
            .tooltip(widgets::tooltip(label, None))
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.toggle_composer_expanded(cx);
            }))
            .child(glyphs)
            .into_any_element()
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

/// One look of the round button, easing on or off by itself.
fn primary_layer(
    id: &'static str,
    on: bool,
    build: impl Fn(f32) -> gpui::AnyElement + 'static,
) -> motion::Animated {
    motion::animated(id, if on { 1. } else { 0. }, motion::TOGGLE, build)
}

/// The round button's fill at `on` strength.
fn fill(on: f32, color: u32) -> Div {
    div()
        .absolute()
        .inset_0()
        .rounded_full()
        .bg(gpui::rgb(color))
        .opacity(on)
}

/// A glyph centered in the round button at `on` strength.
fn glyph(on: f32, child: gpui::AnyElement) -> gpui::AnyElement {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .opacity(on)
        .child(child)
        .into_any_element()
}

/// A 16 px glyph that grows in from 60% as it fades in.
fn scaled_icon(name: &'static str, color: u32, on: f32) -> gpui::AnyElement {
    let scale = 0.6 + 0.4 * on;
    icon(name, px(16.), color)
        .with_transformation(gpui::Transformation::scale(gpui::size(scale, scale)))
        .into_any_element()
}

/// Stop's square.
fn square(color: u32) -> gpui::AnyElement {
    div()
        .size(px(10.))
        .rounded(px(2.5))
        .bg(gpui::rgb(color))
        .into_any_element()
}

/// The expand button's glyphs: expand and shrink cross-fade with a scale
/// as it turns (TOGGLE), and the pair lifts in from 80% as the button
/// reveals.
fn expand_glyphs(expanded: bool, shown: f32) -> Div {
    let lift = 0.8 + 0.2 * shown;
    div().relative().flex_none().size(px(14.)).children(
        [("maximize-2", !expanded), ("minimize-2", expanded)]
            .into_iter()
            .enumerate()
            .map(move |(index, (name, current))| {
                motion::animated(
                    ("composer-expand-glyph", index),
                    if current { 1. } else { 0. },
                    motion::TOGGLE,
                    move |on| {
                        let scale = (0.5 + 0.5 * on) * lift;
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .opacity(on * shown)
                            .child(
                                icon(name, px(14.), theme::text_muted()).with_transformation(
                                    gpui::Transformation::scale(gpui::size(scale, scale)),
                                ),
                            )
                            .into_any_element()
                    },
                )
            }),
    )
}

/// The accent bar application Vim draws under the control it has
/// selected, as on sidebar and transcript rows: along the bottom, without
/// changing the row's layout.
pub(super) fn vim_mark(element: Div, selected: bool) -> Div {
    element.relative().when(selected, |element| {
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
}

/// A new task's token beside the model: a 28 px capsule with a soft fill,
/// warm for a mode that runs everything without asking.
fn token_frame(id: &'static str, warm: bool, open: bool) -> crate::ui::motion::HoverBackground {
    let rest: gpui::Hsla = if warm {
        gpui::rgb(theme::accent_container()).into()
    } else {
        theme::overlay_hover()
    };
    let hover: gpui::Hsla = if warm {
        motion::mix(rest, gpui::rgb(theme::accent()).into(), 0.14)
    } else {
        gpui::rgb(theme::bg_sidebar_pill()).into()
    };
    div()
        .id(id)
        .debug_selector(|| id.to_string())
        .role(gpui::Role::Button)
        // A tab stop, so the keyboard reaches it without application Vim.
        .tab_index(0)
        .focus_visible(widgets::focus_ring_on(hover))
        .on_key_down(ChatScreen::sidebar_button_key_down)
        .flex_none()
        .h_7()
        .pl_2()
        .pr_2p5()
        .flex()
        .items_center()
        .gap_1p5()
        .rounded(px(14.))
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(gpui::rgb(if warm {
            theme::accent_ink()
        } else {
            theme::text_secondary()
        }))
        .bg(if open { hover } else { rest })
        .hover_bg(hover)
        .hover(|style| style.cursor_pointer())
}

/// The lift under a composer that floats on the page: a new task's box,
/// or a conversation's frame.
pub(super) fn composer_shadow() -> Vec<gpui::BoxShadow> {
    let shadow = |alpha: f32, y: f32, blur: f32| gpui::BoxShadow {
        color: if theme::is_light() {
            gpui::hsla(0., 0., 0.09, alpha)
        } else {
            gpui::hsla(0., 0., 0., alpha)
        },
        offset: gpui::point(px(0.), px(y)),
        blur_radius: px(blur),
        spread_radius: px(0.),
        inset: false,
    };
    if theme::is_light() {
        vec![shadow(0.05, 1., 2.), shadow(0.05, 6., 18.)]
    } else {
        vec![shadow(0.22, 6., 18.)]
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
        .hover_bg(gpui::rgb(theme::bg_sidebar_pill()))
        .hover(|style| style.cursor_pointer())
        .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_row_selected())))
        .children(leading.map(|name| icon(name, px(16.), color)))
}

/// A catalog badge on a model row: coral on dark, grey on light (see
/// `badge_fill`), held to 4.5:1 by the contrast test.
fn model_badge(label: SharedString) -> Div {
    div()
        .flex_none()
        .px_1p5()
        .py(px(1.))
        .rounded_full()
        .bg(gpui::rgb(theme::badge_fill()))
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(gpui::rgb(theme::badge_text()))
        .child(label)
}

/// How the catalog presents the models: the panel's sections in display
/// order, each model's section, and its New/Popular model badges. The server
/// owns all three, so a model deployed without an app release lists where
/// its release put it.
#[derive(Clone, Debug, Default)]
pub(super) struct ModelSections {
    groups: Vec<maple_sdk::CatalogGroup>,
    group_of: HashMap<String, String>,
    popular: HashSet<String>,
    new: HashSet<String>,
}

/// The badge string the catalog marks a popular model with.
const POPULAR_BADGE: &str = "Popular";
/// The badge string the catalog marks a newly launched model with.
const NEW_BADGE: &str = "New";

impl ModelSections {
    pub(super) fn from_catalog(catalog: &maple_sdk::ModelCatalogResponse) -> Self {
        Self {
            groups: catalog.groups.clone(),
            group_of: catalog
                .data
                .iter()
                .filter_map(|model| Some((model.id.clone(), model.group.clone()?)))
                .collect(),
            new: catalog
                .data
                .iter()
                .filter(|model| model.badges.iter().any(|badge| badge == NEW_BADGE))
                .map(|model| model.id.clone())
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

    pub(super) fn is_new(&self, model: &str) -> bool {
        self.new.contains(model)
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
        frame_at(start + motion::PANEL.settle - Duration::from_millis(1), cx);
        let landing = composer_height(cx);
        assert_eq!(tail_gap(cx), pinned, "the transcript keeps its tail");
        frame_at(start + motion::PANEL.settle, cx);
        let expanded = composer_height(cx);
        assert!(expanded > rest * 2., "expanded: {expanded:?}");
        assert!(
            (expanded - landing).abs() < px(1.),
            "lands on the resting layout: {landing:?} then {expanded:?}"
        );
        assert_eq!(tail_gap(cx), pinned, "the transcript keeps its tail");

        // Shrink, then expand again halfway down.
        let shrink = start + motion::PANEL.settle * 2;
        motion::set_test_now(Some(shrink));
        toggle(&chat, cx);
        frame_at(shrink + motion::PANEL.settle / 2, cx);
        let partway = composer_height(cx);
        assert!(partway > rest && partway < expanded, "partway: {partway:?}");
        assert_eq!(tail_gap(cx), pinned, "the tail stays put mid-flight");
        toggle(&chat, cx);
        assert_eq!(
            composer_height(cx),
            partway,
            "a reversal starts where it is"
        );
        frame_at(
            shrink + motion::PANEL.settle / 2 + Duration::from_millis(30),
            cx,
        );
        assert!(composer_height(cx) > partway, "heading back up");
        frame_at(shrink + motion::PANEL.settle * 2, cx);
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
        frame_at(start + motion::PANEL.settle / 2, cx);
        let partway = composer_height(cx);
        assert!(partway > rest.size.height, "partway: {partway:?}");
        frame_at(start + motion::PANEL.settle - Duration::from_millis(1), cx);
        let landing = cx.debug_bounds("composer-box").expect("composer");
        frame_at(start + motion::PANEL.settle, cx);
        let expanded = cx.debug_bounds("composer-box").expect("composer");
        assert!(
            (expanded.top() - landing.top()).abs() < px(1.),
            "{landing:?} {expanded:?}"
        );
        assert!(
            (expanded.size.height - landing.size.height).abs() < px(1.),
            "{landing:?} {expanded:?}"
        );

        let shrink = start + motion::PANEL.settle * 2;
        motion::set_test_now(Some(shrink));
        toggle(&chat, cx);
        assert_eq!(cx.debug_bounds("composer-box"), Some(expanded));
        // On the way back the heading rides above the composer at its full
        // size: the folding box never cuts it against the composer's edge.
        for step in 1..8 {
            frame_at(shrink + motion::PANEL.settle * step / 8, cx);
            let composer = cx.debug_bounds("composer-box").expect("composer");
            let title = cx
                .debug_bounds("empty-title")
                .expect("the heading draws on the way back");
            assert!(
                title.bottom() <= composer.top(),
                "the heading stays clear of the composer: {title:?} {composer:?}"
            );
        }
        frame_at(shrink + motion::PANEL.settle - Duration::from_millis(1), cx);
        let landing = cx.debug_bounds("composer-box").expect("composer");
        frame_at(shrink + motion::PANEL.settle, cx);
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
        frame_at(start + motion::PANEL.settle * 2, cx);
        let expanded = cx.debug_bounds("composer-box").expect("composer");
        assert!(
            expanded.size.height > rest.size.height,
            "{rest:?} {expanded:?}"
        );

        let shrink = start + motion::PANEL.settle * 3;
        motion::set_test_now(Some(shrink));
        toggle(&chat, cx);
        assert_eq!(
            cx.debug_bounds("composer-box"),
            Some(expanded),
            "shrinking starts from the expanded box"
        );
        let mut heights = vec![expanded.size.height];
        for step in 1..8 {
            frame_at(shrink + motion::PANEL.settle * step / 8, cx);
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
        frame_at(shrink + motion::PANEL.settle * 2, cx);
        assert_eq!(
            cx.debug_bounds("composer-box"),
            Some(rest),
            "it lands at rest"
        );
        motion::set_test_now(None);
    }

    /// Sending a message several lines long: the composer gets shorter on
    /// that frame, and the slot the sent message shows in opens from the
    /// height it gave up. The newest row holds still on the send frame and
    /// from then on only moves up, to make the room the bubble needs beyond
    /// that. It never drops first.
    #[gpui::test]
    fn sending_a_long_message_never_drops_the_transcript(cx: &mut TestAppContext) {
        let start = Instant::now();
        motion::set_test_now(Some(start));
        cx.update(|cx| cx.set_reduce_motion(false));
        let (chat, cx) = chat_window(cx, 40);
        let text = "one\ntwo\nthree\nfour\nfive";
        chat.update(cx, |chat, cx| {
            let composer = chat.composer.clone().expect("composer");
            composer.update(cx, |input, cx| input.set_text(text, cx));
        });
        cx.run_until_parked();
        let typed = start + motion::PANEL.settle * 2;
        frame_at(typed, cx);
        let newest = |cx: &mut VisualTestContext| {
            cx.debug_bounds("timeline-row-m39")
                .expect("the newest row is on screen")
                .bottom()
        };
        let tall = composer_height(cx);
        let before = newest(cx);

        // What dispatching a send does to the composer and the slot.
        chat.update(cx, |chat, cx| {
            chat.clear_composer_for_send(cx);
            let preview = crate::ui::chat::sending::SendPreview::new(Some("s1".into()), text, &[])
                .capture_timeline(&chat.timeline);
            chat.sending_previews.insert("send-1".into(), preview);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            composer_height(cx) < tall - px(40.),
            "the composer emptied on the send frame"
        );
        let first = newest(cx);
        assert!(
            (first - before).abs() <= px(1.),
            "the newest row holds still on the send frame: {before:?} then {first:?}"
        );
        let mut last = first;
        for step in 1..=12 {
            frame_at(typed + motion::ENTER.settle * step / 6, cx);
            let now = newest(cx);
            assert!(
                now <= last + px(0.5),
                "it only ever moves up: {last:?} then {now:?}"
            );
            last = now;
        }
        assert!(
            last < before,
            "it ends higher, by what the bubble needs beyond the composer's rows"
        );
        // The frame asked for while it moved still runs once; then it rests.
        // gpui's own one-shot fades (the composer's "Sending…" status) read
        // the executor's clock, so that moves on too.
        cx.executor().advance_clock(motion::PANEL.settle * 4);
        frame_at(typed + motion::PANEL.settle * 4, cx);
        frame_at(typed + motion::PANEL.settle * 5, cx);
        assert_eq!(newest(cx), last, "settled");
        assert_eq!(cx.update(|window, app| window.simulate_next_frame(app)), 0);
        motion::set_test_now(None);
    }

    /// The same send while the waiting dots already show under the thread
    /// (a second message before the first reply): the slot is on screen at
    /// rest, so it takes the height the composer gives up on that frame and
    /// opens the rest over the following frames. The newest row never
    /// drops, and the slot does not pop to its full height in one frame.
    #[gpui::test]
    fn a_send_while_the_dots_show_neither_drops_nor_pops(cx: &mut TestAppContext) {
        let start = Instant::now();
        motion::set_test_now(Some(start));
        cx.update(|cx| cx.set_reduce_motion(false));
        let (chat, cx) = chat_window(cx, 40);
        let text = "one\ntwo\nthree\nfour\nfive";
        chat.update(cx, |chat, cx| {
            chat.active_runs.insert("s1".into(), "run-1".into());
            chat.awaiting_first_token = true;
            let composer = chat.composer.clone().expect("composer");
            composer.update(cx, |input, cx| input.set_text(text, cx));
            cx.notify();
        });
        cx.run_until_parked();
        // Long after the dots appeared: a stale start time must not count.
        let typed = start + Duration::from_secs(5);
        frame_at(typed, cx);
        frame_at(typed, cx);
        let newest = |cx: &mut VisualTestContext| {
            cx.debug_bounds("timeline-row-m39")
                .expect("the newest row is on screen")
                .bottom()
        };
        let before = newest(cx);

        chat.update(cx, |chat, cx| {
            chat.clear_composer_for_send(cx);
            let preview = crate::ui::chat::sending::SendPreview::new(Some("s1".into()), text, &[])
                .capture_timeline(&chat.timeline);
            chat.sending_previews.insert("send-2".into(), preview);
            cx.notify();
        });
        cx.run_until_parked();
        let first = newest(cx);
        assert!(
            first <= before + px(1.),
            "the newest row does not drop on the send frame: {before:?} then {first:?}"
        );
        let mut last = first;
        let mut steps = 0;
        for step in 1..=12 {
            frame_at(typed + motion::ENTER.settle * step / 6, cx);
            let now = newest(cx);
            assert!(
                now <= last + px(0.5),
                "it only ever moves up: {last:?} then {now:?}"
            );
            if now < last - px(0.5) {
                steps += 1;
            }
            last = now;
        }
        assert!(
            steps >= 3,
            "the slot opens over several frames, not in one: {steps} moves from {first:?} to {last:?}"
        );
        motion::set_test_now(None);
    }

    /// The composer changes place at once between the empty state and a
    /// conversation: on the first message it is at the foot of the thread
    /// on the next frame and asks for no more, and New Task puts it straight
    /// back. Where the composer sits is not something to animate.
    #[gpui::test]
    fn the_composer_changes_place_at_once_between_a_new_task_and_a_conversation(
        cx: &mut TestAppContext,
    ) {
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
            first.top() > hero.top() + px(100.),
            "at the foot of the thread on the first frame: {hero:?} {first:?}"
        );
        frame_at(start + motion::PANEL.settle, cx);
        let rest = cx.debug_bounds("composer-box").expect("composer");
        assert_eq!(rest, first, "and it stays there");
        assert_eq!(cx.update(|window, app| window.simulate_next_frame(app)), 0);

        chat.update(cx, |chat, cx| {
            chat.replace_timeline(Vec::new());
            cx.notify();
        });
        cx.run_until_parked();
        let back = cx.debug_bounds("composer-box").expect("composer");
        assert!(
            (back.top() - hero.top()).abs() < px(1.),
            "New Task puts it straight back: {hero:?} {back:?}"
        );
        assert_eq!(cx.update(|window, app| window.simulate_next_frame(app)), 0);
        motion::set_test_now(None);
    }
}
