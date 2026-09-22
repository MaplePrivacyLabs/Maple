//! The project picker: one dialog for every host, with a search box over
//! the target host's recent projects and folder suggestions, driven by
//! the keyboard.

use gpui::{Context, Div, SharedString, div, prelude::*, px};

use super::ChatScreen;
use super::sidebar::root_display_name;
use crate::ui::icons::icon;
use crate::ui::text_input::TextInput;
use crate::ui::theme;
use crate::ui::widgets;

/// What one row of the project picker stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PickerRowKind {
    /// A project this host used before.
    Recent,
    /// A folder the host found for the typed text.
    Suggestion,
    /// The typed text itself, when it looks like a path.
    OpenPath,
}

#[derive(Clone)]
pub(super) struct PickerRow {
    pub(super) kind: PickerRowKind,
    pub(super) path: String,
    pub(super) title: SharedString,
    pub(super) subtitle: Option<SharedString>,
}

/// The project picker while it is open: one search box over the target
/// host's recent projects and folders, keyboard-navigable.
pub(super) struct ProjectPicker {
    pub(super) rows: Vec<PickerRow>,
    pub(super) selected: usize,
    /// Bumped per folder request; a late answer is dropped.
    generation: u64,
    suggestions: Vec<maple_agent::host::DirectorySuggestion>,
    /// What the rows were built from: the typed text, or the path an
    /// arrow filled in.
    query: String,
    /// The text an arrow just filled in; the box reporting it back is not
    /// a new query, so the rows stay put while the highlight moves.
    filled: Option<String>,
}

impl ChatScreen {
    /// The project chip or its shortcut: open the picker, or close it when
    /// it is open.
    pub(super) fn toggle_project_picker(&mut self, cx: &mut Context<Self>) {
        if self.project_picker.is_some() {
            self.close_project_picker(cx);
        } else {
            self.open_project_picker(cx);
        }
    }

    /// Open the project picker: a search box over the target host's recent
    /// projects and folders, the one way to choose a project on any host.
    /// The search box is created once and reused.
    pub(super) fn open_project_picker(&mut self, cx: &mut Context<Self>) {
        self.popup.close(cx);
        if self.root_input.is_none() {
            let chat = cx.entity().downgrade();
            let key_chat = chat.clone();
            let arrow_chat = chat.clone();
            let application_vim_enabled = self.application_vim_enabled;
            let input = cx.new(move |cx| {
                TextInput::new("Search folders or enter a path\u{2026}", cx)
                    .with_tab_index(0)
                    .application_vim(application_vim_enabled)
                    .on_key(move |event, _text, _window, cx| {
                        let Some(chat) = key_chat.upgrade() else {
                            return false;
                        };
                        match event.keystroke.key.as_str() {
                            "enter" => chat.update(cx, |chat, cx| chat.project_picker_confirm(cx)),
                            "escape" => chat.update(cx, |chat, cx| chat.close_project_picker(cx)),
                            _ => return false,
                        }
                        true
                    })
                    .on_vertical(move |delta, _window, cx| {
                        // The move writes the highlighted path back into
                        // this input, which is mid-update here: defer it.
                        let chat = arrow_chat.clone();
                        cx.defer(move |cx| {
                            if let Some(chat) = chat.upgrade() {
                                chat.update(cx, |chat, cx| chat.project_picker_move(delta, cx));
                            }
                        });
                        true
                    })
                    .on_application_escape(move |_window, cx| {
                        if let Some(chat) = chat.upgrade() {
                            chat.update(cx, |chat, cx| chat.close_project_picker(cx));
                        }
                    })
            });
            cx.observe(&input, |this, input, cx| {
                let query = input.read(cx).text();
                // The box reports every change, not only to its text. The
                // rows already show this text when it is the query they
                // were built from, or the path an arrow filled in.
                if this.project_picker.as_ref().is_some_and(|picker| {
                    picker.query == query || picker.filled.as_deref() == Some(query.as_str())
                }) {
                    return;
                }
                this.refresh_project_picker(query, cx);
            })
            .detach();
            self.root_input = Some(input);
        } else if let Some(input) = self.root_input.clone() {
            input.update(cx, |input, cx| input.set_text("", cx));
        }
        self.project_picker = Some(ProjectPicker {
            rows: Vec::new(),
            selected: 0,
            generation: 0,
            suggestions: Vec::new(),
            query: String::new(),
            filled: None,
        });
        self.root_input_focus_pending = true;
        self.refresh_project_picker(String::new(), cx);
        cx.notify();
    }

    pub(super) fn close_project_picker(&mut self, cx: &mut Context<Self>) {
        if self.project_picker.take().is_some() {
            // The composer takes the keyboard back.
            self.screen_focus_pending = true;
            cx.notify();
        }
    }

    /// The search text changed: rebuild the rows now from what is known
    /// and ask the host for folders that match.
    pub(super) fn refresh_project_picker(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(picker) = self.project_picker.as_mut() else {
            return;
        };
        picker.query = query.clone();
        picker.filled = None;
        picker.selected = 0;
        picker.generation += 1;
        let generation = picker.generation;
        self.rebuild_picker_rows();
        // With nothing typed, the current project's row starts highlighted.
        if query.trim().is_empty()
            && let Some(current) = self.project_root.as_deref()
            && let Some(picker) = self.project_picker.as_mut()
            && let Some(index) = picker.rows.iter().position(|row| row.path == current)
        {
            picker.selected = index;
        }
        let host = self.host.clone();
        self.call(
            async move { host.suggest_directories(query).await },
            cx,
            move |this, result, cx| {
                let Some(picker) = this.project_picker.as_mut() else {
                    return;
                };
                if picker.generation != generation {
                    return;
                }
                if let Ok(suggestions) = result {
                    picker.suggestions = suggestions;
                    this.rebuild_picker_rows();
                    cx.notify();
                }
            },
        );
    }

    /// Rows in display order: the typed path itself when it looks like
    /// one, recent projects that match, then the host's folders.
    pub(super) fn rebuild_picker_rows(&mut self) {
        let Some(mut picker) = self.project_picker.take() else {
            return;
        };
        let query = picker.query.trim().to_string();
        let needle = query.to_lowercase();
        let mut rows: Vec<PickerRow> = Vec::new();
        for root in &self.recent_roots {
            if !needle.is_empty() && !root.to_lowercase().contains(&needle) {
                continue;
            }
            rows.push(PickerRow {
                kind: PickerRowKind::Recent,
                path: root.clone(),
                title: SharedString::from(root_display_name(root)),
                subtitle: Some(SharedString::from(root.clone())),
            });
        }
        for suggestion in &picker.suggestions {
            if rows.iter().any(|row| row.path == suggestion.path) {
                continue;
            }
            rows.push(PickerRow {
                kind: PickerRowKind::Suggestion,
                path: suggestion.path.clone(),
                title: SharedString::from(suggestion.name.clone()),
                subtitle: Some(SharedString::from(suggestion.path.clone())),
            });
        }
        let looks_like_path = query.starts_with('/') || query.starts_with('~');
        let typed = query.trim_end_matches('/').to_string();
        if looks_like_path && !typed.is_empty() && !rows.iter().any(|row| row.path == typed) {
            rows.insert(
                0,
                PickerRow {
                    kind: PickerRowKind::OpenPath,
                    path: typed.clone(),
                    title: "Open this path".into(),
                    subtitle: Some(SharedString::from(typed)),
                },
            );
        }
        picker.selected = picker.selected.min(rows.len().saturating_sub(1));
        picker.rows = rows;
        self.project_picker = Some(picker);
    }

    /// Move the highlight and put the highlighted path in the search box,
    /// as a shell completes.
    pub(super) fn project_picker_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(picker) = self.project_picker.as_mut() else {
            return;
        };
        let len = picker.rows.len();
        if len == 0 {
            return;
        }
        picker.selected = (picker.selected as isize + delta).rem_euclid(len as isize) as usize;
        let text = picker.rows[picker.selected].path.clone();
        picker.filled = Some(text.clone());
        if let Some(input) = self.root_input.clone() {
            input.update(cx, |input, cx| input.set_text(&text, cx));
        }
        cx.notify();
    }

    pub(super) fn project_picker_confirm(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.project_picker.as_ref().map(|picker| picker.selected) else {
            return;
        };
        self.activate_picker_row(index, cx);
    }

    /// Open the project a row names.
    pub(super) fn activate_picker_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(path) = self
            .project_picker
            .as_ref()
            .and_then(|picker| picker.rows.get(index))
            .map(|row| row.path.clone())
        else {
            return;
        };
        self.close_project_picker(cx);
        self.select_project_root(path, cx);
    }

    /// The project picker: a centered dialog over the pane with a search
    /// box, the rows it matched, and the keys that drive it.
    pub(super) fn render_project_picker(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let picker = self.project_picker.as_ref();
        let rows: &[PickerRow] = picker.map(|picker| picker.rows.as_slice()).unwrap_or(&[]);
        let selected = picker.map(|picker| picker.selected).unwrap_or(0);
        let searching = picker.is_some_and(|picker| !picker.query.trim().is_empty());
        let mut list = div()
            .id("project-picker-rows")
            .flex()
            .flex_col()
            .max_h(px(380.))
            .overflow_y_scroll();
        if rows.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_3()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(if searching {
                        "No folders match"
                    } else {
                        "No recent projects on this host yet; type a path"
                    }),
            );
        }
        for (index, row) in rows.iter().enumerate() {
            let is_selected = index == selected;
            let glyph = match row.kind {
                PickerRowKind::Recent => "folder-open",
                PickerRowKind::Suggestion => "folder",
                PickerRowKind::OpenPath => "search",
            };
            list = list.child(
                div()
                    .id(SharedString::from(format!("project-picker-row-{index}")))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .rounded(theme::RADIUS_SM)
                    .when(is_selected, |row| row.bg(gpui::rgb(theme::bg_input())))
                    .hover(|style| style.bg(gpui::rgb(theme::bg_input())).cursor_pointer())
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.activate_picker_row(index, cx);
                    }))
                    .child(icon(glyph, px(16.), theme::text_muted()))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .flex_1()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(gpui::rgb(theme::text_primary()))
                                    .line_clamp(1)
                                    .text_ellipsis()
                                    .child(row.title.clone()),
                            )
                            .when_some(row.subtitle.clone(), |col, subtitle| {
                                col.child(
                                    div()
                                        .text_xs()
                                        .text_color(gpui::rgb(theme::text_muted()))
                                        .line_clamp(1)
                                        .text_ellipsis()
                                        .child(subtitle),
                                )
                            }),
                    )
                    .when(is_selected, |row| {
                        row.child(
                            div()
                                .text_xs()
                                .text_color(gpui::rgb(theme::text_muted()))
                                .child("Enter"),
                        )
                    }),
            );
        }
        let hint = |keys: &'static str, label: &'static str| {
            div()
                .flex()
                .items_center()
                .gap_1()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(
                    div()
                        .px_1()
                        .rounded(theme::RADIUS_SM)
                        .bg(gpui::rgb(theme::bg_input()))
                        .font_family(crate::assets::FONT_MONO)
                        .child(keys),
                )
                .child(label)
        };
        div()
            .id("project-picker-backdrop")
            .absolute()
            .size_full()
            .top_0()
            .left_0()
            .occlude()
            .bg(theme::scrim())
            .flex()
            .items_start()
            .justify_center()
            .pt(px(96.))
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.close_project_picker(cx);
            }))
            .child(
                div()
                    .id("project-picker")
                    .role(gpui::Role::Dialog)
                    .aria_label("Choose a project")
                    .w(px(640.))
                    .max_w_full()
                    .rounded(theme::RADIUS_XL)
                    .shadow_lg()
                    .bg(gpui::rgb(theme::bg_elevated()))
                    .border_1()
                    .border_color(gpui::rgb(theme::border()))
                    .flex()
                    .flex_col()
                    .on_click(|_event, _window, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .px_4()
                            .pt_4()
                            .pb_2()
                            .child(
                                div()
                                    .flex()
                                    .items_baseline()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_lg()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(gpui::rgb(theme::text_primary()))
                                            .child("Choose a project"),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .gap_1()
                                            .text_sm()
                                            .text_color(gpui::rgb(theme::text_muted()))
                                            .child("on")
                                            .child(self.target_host_label.clone()),
                                    ),
                            )
                            .when_some(self.root_input.clone(), |col, input| {
                                col.child(widgets::input_frame().text_sm().child(input))
                            }),
                    )
                    .child(div().px_2().pb_2().child(list))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_4()
                            .px_4()
                            .py_2()
                            .border_t_1()
                            .border_color(gpui::rgb(theme::border()))
                            .child(hint("\u{2191}\u{2193}", "Navigate"))
                            .child(hint("Enter", "Open"))
                            .child(hint("Esc", "Close")),
                    ),
            )
    }
}
