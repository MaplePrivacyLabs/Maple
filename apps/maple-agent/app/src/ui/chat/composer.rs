//! The chat pane chrome: the header and its menus, the slash palette,
//! the plan and side-question cards, and the composer itself.

use crate::settings::PermissionMode;
use std::sync::Arc;

use gpui::{Context, Div, IntoElement, SharedString, Window, div, prelude::*, px};
use maple_agent::agent::{AgentSlashCommand, SideQuestionTurn};

use super::cache::MarkdownKind;
use super::commands::ChatCommand;
use super::transcript::{render_plan_row, render_subagent_row};
use super::{
    COMPOSER_PLACEHOLDER, ChatPopup, ChatScreen, DraftImage, OpenSettingsSection,
    ROOT_MENU_RECENTS, SIDE_THREAD_PLACEHOLDER, SIDEBAR_COLLAPSED_INSET, Section,
};
use crate::ui::icons::{icon, spinner};
use crate::ui::markdown;
use crate::ui::motion;
use crate::ui::popup::{Menu, MenuItem, Placement};
use crate::ui::text_input::vim::VimMode;
use crate::ui::theme;
use crate::ui::titlebar;
use crate::ui::widgets;

impl ChatScreen {
    pub(super) fn render_header(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
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
            self.with_menu(
                ChatPopup::Project,
                chip(
                    "root-picker",
                    Some("folder-open"),
                    self.project_label.clone(),
                    true,
                    self.popup.is_open(&ChatPopup::Project),
                    false,
                )
                // The header is a window drag region; a press here is the
                // chip's.
                .on_mouse_down(gpui::MouseButton::Left, |_event, _window, cx| {
                    cx.stop_propagation();
                }),
                Placement::BelowStart,
                window,
                cx,
                Self::project_menu,
            )
            .flex_none(),
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

    /// `button` as the opener of `popup`, with `menu` attached below or
    /// above it while the popup is open.
    fn with_menu(
        &self,
        popup: ChatPopup,
        button: gpui::Stateful<Div>,
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

    /// A press on a chip: the header's project chip runs the command it
    /// shares with its shortcut; a composer chip opens or closes its menu.
    fn press_chip(&mut self, popup: ChatPopup, window: &mut Window, cx: &mut Context<Self>) {
        match popup {
            ChatPopup::Project => self.execute_command(ChatCommand::ChooseProject, window, cx),
            popup => self.toggle_popup(popup, cx),
        }
    }

    /// The header chip's menu: recent projects, then "New project…", then
    /// manual entry when the native folder picker is unavailable.
    fn project_menu(&self, cx: &mut Context<Self>) -> Menu<Self> {
        let mut menu = Menu::new("project-menu", px(480.))
            .label("Projects")
            .application_vim(self.application_vim_enabled);
        for path in self.recent_roots.iter().take(ROOT_MENU_RECENTS) {
            let pick = path.clone();
            menu = menu.item(
                MenuItem::new(
                    SharedString::from(format!("root-{path}")),
                    path.clone(),
                    move |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                        this.select_project_root(pick.clone(), cx);
                    },
                )
                .truncate_start()
                .current(self.project_root.as_deref() == Some(path.as_str())),
            );
        }
        menu = menu.item(
            MenuItem::new(
                "root-choose",
                "New project…",
                |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                    this.choose_root_dialog(cx);
                },
            )
            .icon("folder-plus"),
        );
        let Some(input) = self.root_input.clone() else {
            return menu;
        };
        menu.child(
            div()
                .px_3()
                .pt_1()
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
                .child(
                    div()
                        .flex_1()
                        .debug_selector(|| "root-path-field".to_string())
                        .child(input),
                )
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
                        .hover(|style| style.bg(gpui::rgb(theme::accent_hover())).cursor_pointer())
                        .active(|style| style.bg(gpui::rgb(theme::send_bottom())))
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            if let Some(path) =
                                this.root_input.as_ref().map(|input| input.read(cx).text())
                            {
                                this.select_project_root(path, cx);
                            }
                        }))
                        .child("Go"),
                ),
        )
    }

    fn model_menu(&self) -> Menu<Self> {
        let selected = self.selected_model.as_deref();
        Menu::new("model-menu", px(320.))
            .label("Model")
            .max_height(px(320.))
            .application_vim(self.application_vim_enabled)
            .items(self.models.iter().map(|model| {
                let pick = model.clone();
                MenuItem::new(
                    SharedString::from(format!("model-{model}")),
                    model.clone(),
                    move |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                        this.pick_model(pick.clone(), cx);
                    },
                )
                .current(selected == Some(model.as_str()))
            }))
    }

    fn mode_menu(&self) -> Menu<Self> {
        Menu::new("mode-menu", px(320.))
            .label("Approval mode")
            .application_vim(self.application_vim_enabled)
            .items(
                [PermissionMode::Auto, PermissionMode::SmartApprove].map(|mode| {
                    MenuItem::new(
                        SharedString::from(format!("mode-{}", mode.as_str())),
                        mode.label(),
                        move |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                            this.permission_mode = mode;
                            this.uses_default_permission_mode = false;
                            this.apply_permission_mode(cx);
                            cx.notify();
                        },
                    )
                    .icon(mode.icon())
                    .note(mode.note())
                    .current(self.permission_mode == mode)
                }),
            )
    }

    /// The task's integrations, each with a switch. A server that still
    /// needs setup opens Settings instead.
    fn integrations_menu(&self) -> Menu<Self> {
        let mut menu = Menu::new("integrations-menu", px(360.))
            .label("Integrations")
            .max_height(px(320.))
            .application_vim(self.application_vim_enabled)
            .header("Integrations");
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
                            .child(server.description.clone()),
                    )
                })
                .when(!available, |column| {
                    column.child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::status_warning()))
                            .child("Set up in Settings → Integrations"),
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
                "Manage integrations…",
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

    pub(super) fn render_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
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
                    .child(self.with_menu(
                        ChatPopup::Model,
                        chip(
                            "model-picker",
                            None,
                            model_label,
                            true,
                            self.popup.is_open(&ChatPopup::Model),
                            false,
                        ),
                        Placement::AboveStart,
                        window,
                        cx,
                        |this, _| this.model_menu(),
                    ))
                    .child(self.with_menu(
                        ChatPopup::Mode,
                        chip(
                            "permission-mode-toggle",
                            Some(self.permission_mode.icon()),
                            self.permission_mode.label().to_string(),
                            true,
                            self.popup.is_open(&ChatPopup::Mode),
                            false,
                        ),
                        Placement::AboveStart,
                        window,
                        cx,
                        |this, _| this.mode_menu(),
                    ))
                    .child(self.with_menu(
                        ChatPopup::Integrations,
                        chip(
                            "mcp-menu",
                            Some("puzzle"),
                            match mcp_enabled {
                                0 => "Integrations".to_string(),
                                1 => "1 integration".to_string(),
                                count => format!("{count} integrations"),
                            },
                            false,
                            self.popup.is_open(&ChatPopup::Integrations),
                            false,
                        ),
                        Placement::AboveStart,
                        window,
                        cx,
                        |this, _| this.integrations_menu(),
                    ))
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
    let label = label.into();
    div()
        .id(id)
        .debug_selector(|| id.to_string())
        .role(gpui::Role::Button)
        .aria_label(label.clone())
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
        .child(div().whitespace_nowrap().child(label))
        .when(chevron, |el| el.child(icon("chevron-down", px(14.), color)))
}
