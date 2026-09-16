//! The chat pane chrome: the header and its menus, the slash palette,
//! the plan and side-question cards, and the composer itself.

use crate::settings::PermissionMode;
use std::sync::Arc;

use gpui::{Context, Div, IntoElement, SharedString, div, prelude::*, px};
use maple_agent::agent::{AgentSlashCommand, SideQuestionTurn};

use super::cache::MarkdownKind;
use super::commands::ChatCommand;
use super::transcript::{render_plan_row, render_subagent_row};
use super::{
    COMPOSER_PLACEHOLDER, ChatScreen, DraftImage, OpenSettingsSection, ROOT_MENU_RECENTS,
    SIDE_THREAD_PLACEHOLDER, SIDEBAR_COLLAPSED_INSET, Section,
};
use crate::ui::icons::{icon, spinner};
use crate::ui::markdown;
use crate::ui::motion;
use crate::ui::text_input::vim::VimMode;
use crate::ui::theme;
use crate::ui::titlebar;
use crate::ui::widgets;

impl ChatScreen {
    pub(super) fn render_header(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let title = self.selected_title.clone();
        titlebar::drag_region(
            div()
                .id("chat-header")
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .h(px(40.))
                .flex_none()
                .pl_4()
                .pr_3()
                .when(self.sidebar_collapsed, |row| {
                    row.pl(SIDEBAR_COLLAPSED_INSET)
                }),
        )
        .child(
            div()
                // Sized by its text, like the chips: nowrap gives it a
                // real intrinsic width (a clamped, shrinkable title
                // measured as 0 px and vanished). The cap keeps a long
                // title from pushing the chips out of the pane.
                .flex_none()
                .max_w(gpui::relative(0.6))
                .truncate()
                .text_lg()
                .line_height(px(24.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(gpui::rgb(theme::text_primary()))
                .child(title),
        )
        .child(
            chip(
                "root-picker",
                Some("folder-open"),
                self.project_label.clone(),
                true,
                self.root_menu_open,
                false,
            )
            .flex_none()
            .on_mouse_down(gpui::MouseButton::Left, |_event, _window, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(|this, _event, window, cx| {
                this.execute_command(ChatCommand::ChooseProject, window, cx);
            })),
        )
        .when_some(self.branch_label.clone(), |row, branch| {
            row.child(
                div()
                    .flex_none()
                    .text_xs()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(gpui::rgb(theme::status_error()))
                    .whitespace_nowrap()
                    .child(branch),
            )
        })
        .child(div().flex_1())
    }

    /// Surface for the composer menus. A press outside closes every
    /// composer menu, like the sidebar menus.
    fn menu_panel(cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        div()
            .id("composer-menu-panel")
            .occlude()
            .flex()
            .flex_col()
            .mt_1()
            .p_1()
            .rounded(theme::RADIUS_MD)
            .bg(gpui::rgb(theme::bg_elevated()))
            .border_1()
            .border_color(gpui::rgb(theme::border()))
            .shadow_md()
            .on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
                this.close_composer_menus(cx);
            }))
    }

    /// Close every menu the chip row can open.
    pub(super) fn close_composer_menus(&mut self, cx: &mut Context<Self>) {
        if self.mode_menu_open || self.mcp_menu_open || self.models_menu_open {
            self.mode_menu_open = false;
            self.mcp_menu_open = false;
            self.models_menu_open = false;
            cx.notify();
        }
    }

    /// Anchor point for the open composer menu: floating above the chip
    /// row, bottom-anchored so the panel grows upward over the transcript
    /// instead of pushing the layout around. The panel is rendered after
    /// the composer (see the two `render_composer` call sites), so the
    /// composer's border can never paint over it; the containing block is
    /// the wrapper with `px_4` and `pb_4`, hence the +16 offsets that keep
    /// the panel at the same spot it held as a composer child.
    fn menu_overlay(menu: gpui::Stateful<Div>) -> Div {
        div()
            .absolute()
            .bottom(px(64.))
            .left(px(24.))
            .w(px(480.))
            .max_w_full()
            .debug_selector(|| "composer-menu".to_string())
            .child(motion::rise_in(menu, "composer-menu-reveal"))
    }

    /// The project menu, opened from the header chip. Rendered as an
    /// overlay in the chat pane, right under the header.
    pub(super) fn render_root_menu(&self, cx: &mut Context<Self>) -> Option<Div> {
        if !self.root_menu_open {
            return None;
        }
        let mut menu = Self::menu_panel(cx).w(px(480.)).max_w_full();
        {
            for (index, path) in self.recent_roots.iter().take(ROOT_MENU_RECENTS).enumerate() {
                let is_current = self.project_root.as_deref() == Some(path.as_str());
                menu = menu.child(
                    div()
                        .id(gpui::SharedString::from(format!("root-{}", path)))
                        .px_3()
                        .py_1()
                        .text_sm()
                        .text_color(gpui::rgb(if is_current {
                            theme::accent()
                        } else {
                            theme::text_primary()
                        }))
                        // A path: keep the file name, drop the start.
                        .line_clamp(1)
                        .text_ellipsis_start()
                        .when(self.root_menu_selected == Some(index), |row| {
                            row.bg(gpui::rgb(theme::bg_input()))
                        })
                        .hover(|style| style.bg(gpui::rgb(theme::bg_input())).cursor_pointer())
                        .on_click({
                            let path = path.clone();
                            cx.listener(move |this, _event, _window, cx| {
                                this.select_project_root(path.clone(), cx);
                            })
                        })
                        .child(path.clone()),
                );
            }
            let choose_row = self.recent_roots.len().min(ROOT_MENU_RECENTS);
            menu = menu.child(
                div()
                    .id("root-choose")
                    .px_3()
                    .py_1()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .when(self.root_menu_selected == Some(choose_row), |row| {
                        row.bg(gpui::rgb(theme::bg_input()))
                    })
                    .hover(|style| style.bg(gpui::rgb(theme::bg_input())).cursor_pointer())
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.choose_root_dialog(cx);
                    }))
                    .child("New project…"),
            );
            if let Some(input) = self.root_input.clone() {
                menu = menu
                    .child(
                        div()
                            .px_3()
                            .pb_1()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child("Or type an absolute path:"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .pb_2()
                            .child(div().flex_1().child(input))
                            .child(
                                div()
                                    .id("root-apply")
                                    .px_3()
                                    .py_1()
                                    .rounded(theme::RADIUS_SM)
                                    .bg(gpui::rgb(theme::accent()))
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(gpui::rgb(theme::on_accent()))
                                    .hover(|style| {
                                        style.bg(gpui::rgb(theme::accent_hover())).cursor_pointer()
                                    })
                                    .active(|style| style.bg(gpui::rgb(theme::send_bottom())))
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        if let Some(path) = this
                                            .root_input
                                            .as_ref()
                                            .map(|input| input.read(cx).text())
                                        {
                                            this.select_project_root(path, cx);
                                        }
                                    }))
                                    .child("Go"),
                            ),
                    );
            }
        }
        Some(
            div()
                .absolute()
                .top(px(40.))
                .left_4()
                .when(self.sidebar_collapsed, |menu| {
                    menu.left(SIDEBAR_COLLAPSED_INSET)
                })
                .child(
                    menu.key_context(if self.application_vim_enabled {
                        "RootMenu ApplicationVim"
                    } else {
                        "RootMenu"
                    })
                    .when_some(self.root_menu_focus.clone(), |menu, focus| {
                        menu.track_focus(&focus)
                    }),
                ),
        )
    }

    /// The open composer menu as an overlay. The panel floats above the
    /// chip row, bottom-anchored so it grows upward over the transcript
    /// instead of pushing the layout around.
    pub(super) fn render_menu_panel(&self, cx: &mut Context<Self>) -> Option<Div> {
        let mut menu = Self::menu_panel(cx);
        if self.mode_menu_open {
            for mode in [PermissionMode::Auto, PermissionMode::SmartApprove] {
                let (label, note) = (mode.label(), mode.note());
                let mode_icon = icon(mode.icon(), px(14.), theme::text_secondary());
                let is_current = self.permission_mode == mode;
                menu = menu.child(
                    div()
                        .id(gpui::SharedString::from(format!("mode-{}", mode.as_str())))
                        .px_3()
                        .py_1()
                        .text_sm()
                        .text_color(gpui::rgb(if is_current {
                            theme::accent()
                        } else {
                            theme::text_primary()
                        }))
                        .hover(|style| style.bg(gpui::rgb(theme::bg_input())).cursor_pointer())
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.permission_mode = mode;
                            this.uses_default_permission_mode = false;
                            this.mode_menu_open = false;
                            this.apply_permission_mode(cx);
                            cx.notify();
                        }))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_0()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1p5()
                                        .child(mode_icon)
                                        .child(label),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(gpui::rgb(theme::text_muted()))
                                        .child(note),
                                ),
                        ),
                );
            }
            return Some(Self::menu_overlay(menu));
        }
        if self.mcp_menu_open {
            menu = menu.max_h(px(320.)).overflow_y_scroll();
            menu = menu.child(
                div()
                    .px_3()
                    .pt_1()
                    .pb_2()
                    .text_xs()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child("Integrations"),
            );
            if self.session_mcp.is_empty() {
                menu = menu.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_sm()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child("No integrations available for this task."),
                );
            }
            for server in &self.session_mcp {
                let name = server.name.clone();
                let display_name = server.display_name.clone();
                let enabled = server.enabled;
                let kind = server.kind;
                let available = server.available;
                let row_id = gpui::SharedString::from(format!("mcp-{kind:?}-{name}"));
                let switch_id = gpui::SharedString::from(format!("mcp-toggle-{kind:?}-{name}"));
                menu = menu.child(
                    widgets::menu_row(row_id, true)
                        .flex()
                        .items_center()
                        .gap_3()
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            if available || enabled {
                                this.toggle_session_mcp(name.clone(), kind, !enabled, cx);
                            } else {
                                this.mcp_menu_open = false;
                                cx.emit(OpenSettingsSection(Section::Integrations));
                            }
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(gpui::rgb(theme::text_primary()))
                                        .line_clamp(1)
                                        .text_ellipsis()
                                        .child(display_name),
                                )
                                .when(!server.description.is_empty(), |col| {
                                    col.child(
                                        div()
                                            .text_xs()
                                            .text_color(gpui::rgb(theme::text_muted()))
                                            .line_clamp(2)
                                            .child(server.description.clone()),
                                    )
                                })
                                .when(!available, |col| {
                                    col.child(
                                        div()
                                            .text_xs()
                                            .text_color(gpui::rgb(theme::status_warning()))
                                            .child("Set up in Settings → Integrations"),
                                    )
                                }),
                        )
                        .child(widgets::switch(switch_id, enabled)),
                );
            }
            menu = menu.child(
                widgets::menu_row("mcp-manage", true)
                    .mt_1()
                    .border_t_1()
                    .border_color(gpui::rgb(theme::border_subtle()))
                    .text_color(gpui::rgb(theme::accent()))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.mcp_menu_open = false;
                        cx.emit(OpenSettingsSection(Section::Integrations));
                    }))
                    .child("Manage integrations…"),
            );
            return Some(Self::menu_overlay(menu));
        }
        if self.models_menu_open {
            let selected_model = self.selected_model.clone();
            menu = menu.children(self.models.iter().map(|model| {
                let current = selected_model.as_deref() == Some(model.as_str());
                widgets::menu_row(gpui::SharedString::from(format!("model-{model}")), true)
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_1()
                    .when(current, |row| row.font_weight(gpui::FontWeight::MEDIUM))
                    .on_click({
                        let model = model.clone();
                        cx.listener(move |this, _event, _window, cx| {
                            this.pick_model(model.clone(), cx);
                        })
                    })
                    .child(div().flex_1().child(model.clone()))
                    .when(current, |row| {
                        row.child(icon("check", px(14.), theme::accent()))
                    })
            }));
            return Some(Self::menu_overlay(menu));
        }
        None
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
                    .hover(|style| {
                        style
                            .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                            .cursor_pointer()
                    })
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
        self.plan_collapsed = !self.plan_collapsed;
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
            self.notice = Some("Type /btw followed by a question".into());
            cx.notify();
            return;
        }
        if self.booting {
            self.notice = Some("Agent runtime is still starting".into());
            cx.notify();
            return;
        }
        if self.btw.as_ref().is_some_and(|btw| btw.pending) {
            self.notice = Some("Wait for the current answer first".into());
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
                input.set_placeholder(SIDE_THREAD_PLACEHOLDER, cx)
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
                input.set_placeholder(COMPOSER_PLACEHOLDER, cx)
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
                    .child("btw"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child("Side thread; messages stay here until Esc"),
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
                    .hover(|style| style.bg(gpui::rgb(theme::bg_elevated())))
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
                        .child("Thinking…"),
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
                    .child("Subagents"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(format!("{} running", self.subagents.len())),
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
        if self.plan.is_empty() {
            return None;
        }
        let collapsed = self.plan_collapsed;
        let done = self.plan_done;
        let header = div()
            .id("plan-card-header")
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(theme::bg_elevated())))
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
                    .child("Plan"),
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

    pub(super) fn render_composer(&mut self, cx: &mut Context<Self>) -> Div {
        let running = self.is_run_active();
        let disabled = self.booting;
        let has_text = self.composer_has_text;
        let has_images = !self.draft_images.is_empty();
        let images_ready = self.draft_images.iter().all(DraftImage::ready);
        let can_send = !disabled && images_ready && (has_text || has_images);
        let queue_chips = self.render_queue(cx);
        let expanded = self.composer_expanded;
        let composer = self.composer.clone();
        let vim_badge = composer
            .as_ref()
            .and_then(|input| input.read(cx).vim_status())
            .and_then(|status| {
                let label = match status.mode {
                    VimMode::Normal => "NORMAL",
                    VimMode::Insert => "INSERT",
                    VimMode::Visual => "VISUAL",
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
                                .child(notice.message),
                        )
                    })
            });
        let mcp_enabled = self.mcp_enabled_count;
        let drafts = &self.draft_images;
        let model_label = self
            .selected_model
            .clone()
            .unwrap_or_else(|| "Model".to_string());
        div()
            .w_full()
            .flex()
            .flex_col()
            .relative()
            .debug_selector(|| "composer-box".to_string())
            .when(expanded, |container| container.flex_1().min_h_0())
            .rounded(theme::RADIUS_XL)
            .bg(gpui::rgb(theme::bg_app()))
            .border_1()
            .border_color(gpui::rgb(theme::accent()))
            .when(disabled, |container| {
                container.border_color(gpui::rgb(theme::border()))
            })
            // Files dragged from the desktop land as image attachments.
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
                    this.add_image_paths(paths.paths().to_vec(), cx);
                }),
            )
            .children(queue_chips)
            .when(!drafts.is_empty(), |container| {
                container.child(div().flex().flex_wrap().gap_2().px_4().pt_4().children(
                    drafts.iter().enumerate().map(|(index, image)| {
                        div()
                            .relative()
                            .size_16()
                            .rounded(theme::RADIUS_LG)
                            .border_1()
                            .border_color(gpui::rgb(theme::border()))
                            .bg(gpui::rgb(theme::bg_elevated()))
                            .flex()
                            .items_center()
                            .justify_center()
                            .map(|frame| match &image.thumbnail {
                                // The thumbnail is already a square crop, so
                                // it fills the frame and the corners round.
                                Some(thumbnail) => frame.child(
                                    gpui::img(gpui::ImageSource::Image(Arc::clone(thumbnail)))
                                        .size_full()
                                        .rounded(theme::RADIUS_LG),
                                ),
                                None => {
                                    frame.child(icon("image", px(20.), theme::text_secondary()))
                                }
                            })
                            .child(
                                div()
                                    .id(gpui::SharedString::from(format!("draft-remove-{index}")))
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
                                        if index < this.draft_images.len() {
                                            this.draft_images.remove(index);
                                        }
                                        cx.notify();
                                    }))
                                    .child(icon("x", px(10.), theme::text_primary())),
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
                    .when(expanded, |row| row.flex_1().min_h_0())
                    .child(
                        crate::ui::typography::chat_reading(div())
                            .flex_1()
                            .min_w_0()
                            .when(expanded, |cell| cell.h_full())
                            .text_color(gpui::rgb(theme::text_primary()))
                            .children(composer),
                    )
                    .child(
                        div()
                            .id("composer-expand")
                            .size_6()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(theme::RADIUS_SM)
                            .hover(|style| {
                                style
                                    .bg(gpui::rgb(theme::accent_container()))
                                    .cursor_pointer()
                            })
                            .tooltip(widgets::tooltip(
                                if expanded {
                                    "Shrink composer"
                                } else {
                                    "Expand composer"
                                },
                                None,
                            ))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.toggle_composer_expanded(cx);
                            }))
                            .child(icon(
                                if expanded { "minimize-2" } else { "maximize-2" },
                                px(14.),
                                theme::text_muted(),
                            )),
                    ),
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
                    .child(
                        chip(
                            "model-picker",
                            None,
                            model_label,
                            true,
                            self.models_menu_open,
                            false,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.root_menu_open = false;
                                this.mode_menu_open = false;
                                this.mcp_menu_open = false;
                                this.models_menu_open = !this.models_menu_open;
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        chip(
                            "permission-mode-toggle",
                            Some(self.permission_mode.icon()),
                            self.permission_mode.label().to_string(),
                            true,
                            self.mode_menu_open,
                            false,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.root_menu_open = false;
                                this.models_menu_open = false;
                                this.mcp_menu_open = false;
                                this.mode_menu_open = !this.mode_menu_open;
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        chip(
                            "mcp-menu",
                            Some("puzzle"),
                            match mcp_enabled {
                                0 => "Integrations".to_string(),
                                1 => "1 integration".to_string(),
                                count => format!("{count} integrations"),
                            },
                            false,
                            self.mcp_menu_open,
                            false,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.root_menu_open = false;
                                this.models_menu_open = false;
                                this.mode_menu_open = false;
                                this.mcp_menu_open = !this.mcp_menu_open;
                                if this.mcp_menu_open {
                                    this.refresh_session_mcp(cx);
                                }
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        chip(
                            "web-toggle",
                            Some("globe"),
                            if self.web_enabled { "Web" } else { "Web off" }.to_string(),
                            false,
                            false,
                            self.web_enabled,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                let next = !this.web_enabled;
                                this.set_web_enabled(next, cx);
                            },
                        )),
                    )
                    .child(
                        div()
                            .id("add-images")
                            .size_8()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(theme::RADIUS_SM)
                            .hover(|style| {
                                style
                                    .bg(gpui::rgb(theme::accent_container()))
                                    .cursor_pointer()
                            })
                            .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_pill())))
                            .when(self.image_picking, |el| el.opacity(0.5))
                            .tooltip(widgets::tooltip("Attach images", None))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.pick_images(cx);
                            }))
                            .child(icon("image", px(16.), theme::text_secondary())),
                    )
                    .when(self.audio_caps.transcription, |row| {
                        let recording = self.recording;
                        let transcribing = self.transcribing;
                        row.child(
                            div()
                                .id("record-voice")
                                .size_8()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(theme::RADIUS_SM)
                                .when(recording, |el| el.bg(gpui::rgb(theme::status_error())))
                                .hover(|style| {
                                    style
                                        .bg(gpui::rgb(theme::accent_container()))
                                        .cursor_pointer()
                                })
                                .when(transcribing, |el| el.opacity(0.5))
                                .tooltip(widgets::tooltip(
                                    if recording {
                                        "Stop recording"
                                    } else {
                                        "Dictate"
                                    },
                                    None,
                                ))
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.toggle_recording(cx);
                                }))
                                .child(if transcribing {
                                    spinner("transcribing", px(16.), theme::text_secondary())
                                } else if recording {
                                    icon("square", px(14.), theme::on_accent()).into_any_element()
                                } else {
                                    icon("mic", px(16.), theme::text_secondary()).into_any_element()
                                }),
                        )
                    })
                    .children(vim_badge)
                    .when(disabled, |row| {
                        row.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .px_2()
                                .text_xs()
                                .text_color(gpui::rgb(theme::text_muted()))
                                .child(spinner("runtime-boot", px(12.), theme::text_muted()))
                                .child("Starting Maple…"),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("context-indicator")
                            .flex()
                            .items_center()
                            .mr_1()
                            .child(crate::ui::context_ring::ContextRing::new(
                                self.context_fraction.unwrap_or(0.0),
                            )),
                    )
                    .when(running, |row| {
                        row.child(
                            div()
                                .id("stop-run")
                                .size_8()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(gpui::rgb(theme::status_error()))
                                .hover(|style| style.opacity(0.85).cursor_pointer())
                                .active(|style| style.opacity(0.7))
                                .tooltip(widgets::tooltip("Stop the run", None))
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.stop(cx);
                                }))
                                .child(
                                    div()
                                        .size_3()
                                        .rounded(theme::RADIUS_SM)
                                        .bg(gpui::rgb(theme::bg_app())),
                                ),
                        )
                    })
                    .child(
                        div()
                            .id("send-message")
                            .size_8()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(gpui::linear_gradient(
                                180.,
                                gpui::linear_color_stop(gpui::rgb(theme::send_top()), 0.),
                                gpui::linear_color_stop(gpui::rgb(theme::send_bottom()), 1.),
                            ))
                            .when(!can_send, |el| el.opacity(0.4))
                            .when(can_send, |el| {
                                el.hover(|style| style.opacity(0.9).cursor_pointer())
                                    .active(|style| style.opacity(0.75))
                                    .tooltip(widgets::tooltip(
                                        if running { "Queue message" } else { "Send" },
                                        Some("↵"),
                                    ))
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.send_inner(cx);
                                    }))
                            })
                            .child(if disabled {
                                spinner("send-booting", px(16.), theme::on_accent())
                            } else {
                                icon("arrow-up", px(16.), theme::on_accent()).into_any_element()
                            }),
                    ),
            )
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
        ("btw", "Ask a side question; the task does not see it"),
        ("compact", "Summarize the conversation to free context"),
        ("new", "Start a new task"),
        ("pin", "Pin or unpin this project"),
        ("web", "Toggle web tools for this task"),
        ("model", "Pick the model; add a name to filter"),
        ("help", "Show the available commands"),
    ]
    .into_iter()
    .map(|(name, description)| SlashEntry {
        name: name.to_string(),
        description: description.to_string(),
    })
    .chain(skills.iter().map(|command| SlashEntry {
        name: command.name.clone(),
        description: command.description.clone(),
    }))
    .filter(|entry| entry.name.to_lowercase().starts_with(&query))
    .take(8)
    .collect()
}

/// One control in the composer chip row. `active` means its menu is
/// open; `highlight` means the feature it toggles is on, shown in the
/// accent so the two states never look alike.
fn chip(
    id: &'static str,
    leading: Option<&'static str>,
    label: impl Into<SharedString>,
    chevron: bool,
    active: bool,
    highlight: bool,
) -> gpui::Stateful<Div> {
    let color = if highlight {
        theme::accent()
    } else if active {
        theme::text_primary()
    } else {
        theme::text_secondary()
    };
    div()
        .id(id)
        .h_8()
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .rounded(theme::RADIUS_SM)
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(gpui::rgb(color))
        .when(active, |el| el.bg(gpui::rgb(theme::bg_sidebar_pill())))
        .hover(|style| {
            style
                .bg(gpui::rgb(theme::bg_sidebar_pill()))
                .cursor_pointer()
        })
        .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_row_selected())))
        .children(leading.map(|name| icon(name, px(16.), color)))
        .child(div().whitespace_nowrap().child(label.into()))
        .when(chevron, |el| el.child(icon("chevron-down", px(14.), color)))
}
