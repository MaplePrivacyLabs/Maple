//! Host-owned folder chooser for paired computers. Paths stay opaque here:
//! only the selected daemon resolves them or touches disk.

use std::{cell::RefCell, sync::Arc};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString, Window, div,
    prelude::*, px,
};

use crate::backend::{AgentBackend, ProjectFolderPage, ProjectFolderRequest, SelectedProject};
use crate::ui::{icons::icon, task, text_input::TextInput, theme, widgets};

const PAGE_SIZE: u16 = 100;

pub(super) enum FolderDialogEvent {
    Cancelled,
    Selected(SelectedProject),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Busy {
    Browsing,
    Creating,
    Selecting,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Control {
    Up,
    Path,
    Go,
    Home,
    Hidden,
    List,
    Previous,
    Next,
    NewFolder,
    Name,
    Create,
    CancelCreate,
    Cancel,
    Use,
}

pub(super) struct ProjectFolderDialog {
    backend: Arc<AgentBackend>,
    user_id: String,
    computer: SharedString,
    pub(super) path_input: Entity<TextInput>,
    name_input: Entity<TextInput>,
    page: Option<ProjectFolderPage>,
    // Only one bounded page is retained. Cursors are host-provided names,
    // never paths reconstructed with the controller's path conventions.
    cursors: Vec<Option<String>>,
    list: gpui::ListState,
    highlighted: Option<usize>,
    show_hidden: bool,
    creating: bool,
    busy: Option<Busy>,
    error: Option<SharedString>,
    generation: u64,
    closed: bool,
    initial_focus: bool,
    pending_focus: Option<Control>,
    previous_focus: Option<FocusHandle>,
    controls: Vec<(Control, FocusHandle)>,
    tasks: RefCell<Vec<gpui::Task<()>>>,
}

impl EventEmitter<FolderDialogEvent> for ProjectFolderDialog {}

impl ProjectFolderDialog {
    pub(super) fn new(
        backend: Arc<AgentBackend>,
        user_id: String,
        computer: SharedString,
        application_vim_enabled: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let owner = cx.entity().downgrade();
        let input = |name: bool, cx: &mut Context<Self>| {
            let submit = owner.clone();
            let keys = owner.clone();
            let escape = owner.clone();
            cx.new(|cx| {
                TextInput::new(if name { "Folder name" } else { "Folder path" }, cx)
                    .application_vim(application_vim_enabled)
                    .on_application_escape(move |window, cx| {
                        let escape = escape.clone();
                        window.defer(cx, move |window, cx| {
                            escape.update(cx, |this, cx| this.escape(window, cx)).ok();
                        });
                    })
                    .on_enter(move |text, window, cx| {
                        let submit = submit.clone();
                        window.defer(cx, move |_, cx| {
                            submit
                                .update(cx, |this, cx| {
                                    if name {
                                        this.create_folder(text, cx);
                                    } else {
                                        this.go(text, cx);
                                    }
                                })
                                .ok();
                        });
                    })
                    .on_key(move |event, _, window, cx| {
                        if !matches!(event.keystroke.key.as_str(), "escape" | "tab") {
                            return false;
                        }
                        // The field is borrowed while its handler runs. Defer
                        // before the dialog reads any of its input entities.
                        let event = event.clone();
                        let keys = keys.clone();
                        window.defer(cx, move |window, cx| {
                            keys.update(cx, |this, cx| this.key(&event, window, cx))
                                .ok();
                        });
                        true
                    })
            })
        };
        let path_input = input(false, cx);
        let name_input = input(true, cx);
        cx.observe(&path_input, |_, _, cx| cx.notify()).detach();
        let controls = [
            Control::Up,
            Control::Go,
            Control::Home,
            Control::Hidden,
            Control::List,
            Control::Previous,
            Control::Next,
            Control::NewFolder,
            Control::Create,
            Control::CancelCreate,
            Control::Cancel,
            Control::Use,
        ]
        .into_iter()
        .map(|control| (control, cx.focus_handle()))
        .collect();
        Self {
            backend,
            user_id,
            computer,
            path_input,
            name_input,
            page: None,
            cursors: vec![None],
            list: gpui::ListState::new(0, gpui::ListAlignment::Top, px(100.)),
            highlighted: None,
            show_hidden: false,
            creating: false,
            busy: None,
            error: None,
            generation: 0,
            closed: false,
            initial_focus: true,
            pending_focus: None,
            previous_focus: None,
            controls,
            tasks: RefCell::new(Vec::new()),
        }
    }

    pub(super) fn open_home(&mut self, cx: &mut Context<Self>) {
        self.browse(None, vec![None], cx);
    }

    /// The owner calls this before laying out any background content. A cached
    /// view renders during prepaint, which is too late to change focus after
    /// the composer's accessibility node has already claimed it this frame.
    pub(super) fn sync_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.initial_focus {
            self.initial_focus = false;
            self.previous_focus = window.focused(cx);
            window.focus(&self.focus(Control::Path, cx), cx);
        }
        if let Some(control) = self.pending_focus.take() {
            window.focus(&self.focus(control, cx), cx);
        }
    }

    pub(super) fn set_application_vim_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.path_input.update(cx, |input, cx| {
            input.set_application_vim_enabled(enabled, cx)
        });
        self.name_input.update(cx, |input, cx| {
            input.set_application_vim_enabled(enabled, cx)
        });
    }

    fn focus(&self, control: Control, cx: &App) -> FocusHandle {
        match control {
            Control::Path => self.path_input.read(cx).focus_handle(cx),
            Control::Name => self.name_input.read(cx).focus_handle(cx),
            _ => self
                .controls
                .iter()
                .find(|(kind, _)| *kind == control)
                .unwrap()
                .1
                .clone(),
        }
    }

    fn navigating_allowed(&self) -> bool {
        !self.closed && !matches!(self.busy, Some(Busy::Creating | Busy::Selecting))
    }

    fn can_use(&self, cx: &App) -> bool {
        !self.closed
            && self.busy.is_none()
            && !self.creating
            && self.error.is_none()
            && self
                .page
                .as_ref()
                .is_some_and(|page| self.path_input.read(cx).text_ref() == page.path)
    }

    fn go(&mut self, path: String, cx: &mut Context<Self>) {
        if path.is_empty() {
            self.error = Some("Enter a folder path, or choose Home.".into());
            cx.notify();
        } else {
            self.browse(Some(path), vec![None], cx);
        }
    }

    fn browse(
        &mut self,
        path: Option<String>,
        cursors: Vec<Option<String>>,
        cx: &mut Context<Self>,
    ) {
        if !self.navigating_allowed() {
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        self.busy = Some(Busy::Browsing);
        self.error = None;
        if self.creating {
            self.pending_focus = Some(Control::List);
        }
        self.creating = false;
        let typed = self.path_input.read(cx).text();
        let request = ProjectFolderRequest {
            path,
            after: cursors.last().cloned().flatten(),
            limit: PAGE_SIZE,
            show_hidden: self.show_hidden,
        };
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let bridge = task::call(
            &self.backend,
            async move { backend.browse_project_folders(&user_id, request).await },
            cx,
            move |this, result, cx| this.finish_browse(generation, typed, cursors, result, cx),
        );
        task::retain(&self.tasks, bridge);
        cx.notify();
    }

    fn finish_browse(
        &mut self,
        generation: u64,
        typed: String,
        cursors: Vec<Option<String>>,
        result: Result<ProjectFolderPage, String>,
        cx: &mut Context<Self>,
    ) {
        if self.closed || generation != self.generation {
            return;
        }
        self.busy = None;
        match result {
            Ok(page) => {
                // A late response must not erase edits made while loading.
                if self.path_input.read(cx).text_ref() == typed {
                    self.path_input
                        .update(cx, |input, cx| input.set_text(&page.path, cx));
                }
                self.highlighted = (!page.entries.is_empty()).then_some(0);
                self.list
                    .reset_with_uniform_height(page.entries.len(), px(38.));
                self.page = Some(page);
                self.cursors = cursors;
                self.error = None;
            }
            Err(error) => self.error = Some(format!("Could not open folder: {error}").into()),
        }
        cx.notify();
    }

    fn up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = self.page.as_ref().and_then(|page| page.parent_path.clone()) {
            self.browse(Some(parent), vec![None], cx);
        }
    }

    fn page(&mut self, next: bool, cx: &mut Context<Self>) {
        let Some(page) = &self.page else {
            return;
        };
        let path = page.path.clone();
        let mut cursors = self.cursors.clone();
        if next {
            let Some(next) = page.next.clone() else {
                return;
            };
            cursors.push(Some(next));
        } else if cursors.len() > 1 {
            cursors.pop();
        } else {
            return;
        }
        self.browse(Some(path), cursors, cx);
    }

    fn enter_folder(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.busy.is_none()
            && let Some(entry) = self.page.as_ref().and_then(|page| page.entries.get(index))
        {
            self.browse(Some(entry.path.clone()), vec![None], cx);
        }
    }

    fn start_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_use(cx) {
            return;
        }
        self.creating = true;
        self.name_input.update(cx, |input, cx| input.clear(cx));
        window.focus(&self.focus(Control::Name, cx), cx);
        cx.notify();
    }

    fn cancel_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy == Some(Busy::Creating) {
            return;
        }
        self.creating = false;
        self.error = None;
        window.focus(&self.focus(Control::NewFolder, cx), cx);
        cx.notify();
    }

    fn create_folder(&mut self, name: String, cx: &mut Context<Self>) {
        if self.busy.is_some() || !self.creating || self.closed {
            return;
        }
        if name.is_empty() {
            self.error = Some("Enter a name for the new folder.".into());
            cx.notify();
            return;
        }
        let Some(page) = &self.page else {
            return;
        };
        let parent = page.path.clone();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.generation += 1;
        let generation = self.generation;
        self.busy = Some(Busy::Creating);
        self.error = None;
        let bridge = task::call(
            &self.backend,
            async move { backend.create_project_folder(&user_id, parent, name).await },
            cx,
            move |this, result, cx| {
                if this.closed || this.generation != generation {
                    return;
                }
                this.busy = None;
                match result {
                    Ok(path) => this.browse(Some(path), vec![None], cx),
                    Err(error) => {
                        this.error = Some(format!("Could not create folder: {error}").into());
                        cx.notify();
                    }
                }
            },
        );
        task::retain(&self.tasks, bridge);
        cx.notify();
    }

    fn choose(&mut self, cx: &mut Context<Self>) {
        if !self.can_use(cx) {
            return;
        }
        let path = self.page.as_ref().unwrap().path.clone();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.generation += 1;
        let generation = self.generation;
        self.busy = Some(Busy::Selecting);
        let bridge = task::call(
            &self.backend,
            async move { backend.select_project_root(&user_id, path).await },
            cx,
            move |this, result, cx| {
                if this.closed || this.generation != generation {
                    return;
                }
                this.busy = None;
                match result {
                    Ok(project) => {
                        this.closed = true;
                        cx.emit(FolderDialogEvent::Selected(project));
                    }
                    Err(error) => {
                        this.error = Some(format!("Could not use folder: {error}").into())
                    }
                }
                cx.notify();
            },
        );
        task::retain(&self.tasks, bridge);
        cx.notify();
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.closed = true;
        self.generation += 1;
        if let Some(focus) = &self.previous_focus {
            window.focus(focus, cx);
        }
        cx.emit(FolderDialogEvent::Cancelled);
        cx.notify();
    }

    pub(super) fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.creating && self.busy != Some(Busy::Creating) {
            self.cancel_create(window, cx);
        } else {
            self.cancel(window, cx);
        }
    }

    fn tab_controls(&self) -> Vec<Control> {
        let mut controls = vec![
            Control::Up,
            Control::Path,
            Control::Go,
            Control::Home,
            Control::Hidden,
            Control::List,
        ];
        if self.cursors.len() > 1 {
            controls.push(Control::Previous);
        }
        if self.page.as_ref().is_some_and(|page| page.next.is_some()) {
            controls.push(Control::Next);
        }
        if self.creating {
            controls.extend([Control::Name, Control::Create, Control::CancelCreate]);
        } else {
            controls.push(Control::NewFolder);
        }
        controls.extend([Control::Cancel, Control::Use]);
        controls
    }

    fn key(&mut self, event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if key == "escape" {
            self.escape(window, cx);
        } else if key == "tab" {
            let controls = self.tab_controls();
            let index = controls
                .iter()
                .position(|control| self.focus(*control, cx).is_focused(window));
            let next = if event.keystroke.modifiers.shift {
                index
                    .unwrap_or(0)
                    .checked_sub(1)
                    .unwrap_or(controls.len() - 1)
            } else {
                index.map_or(0, |i| (i + 1) % controls.len())
            };
            window.focus(&self.focus(controls[next], cx), cx);
        } else if self.focus(Control::Path, cx).is_focused(window)
            || self.focus(Control::Name, cx).is_focused(window)
        {
            return;
        } else if self.focus(Control::List, cx).is_focused(window)
            && matches!(key, "up" | "down" | "home" | "end")
            && self.busy.is_none()
        {
            let count = self.page.as_ref().map_or(0, |page| page.entries.len());
            if count > 0 {
                let index = match key {
                    "up" => self.highlighted.unwrap_or(0).saturating_sub(1),
                    "down" => (self.highlighted.unwrap_or(0) + 1).min(count - 1),
                    "end" => count - 1,
                    _ => 0,
                };
                self.highlighted = Some(index);
                if key == "end" {
                    // GPUI invalidates offscreen height hints after a width
                    // change. End uses its count anchor so even unseen rows
                    // are measured from the bottom of the list.
                    self.list.scroll_to_end();
                } else {
                    self.list.scroll_to_reveal_item(index);
                }
                cx.notify();
            }
        } else if matches!(key, "enter" | "space" | " ") && !event.keystroke.modifiers.modified() {
            let selected = self
                .tab_controls()
                .into_iter()
                .find(|control| self.focus(*control, cx).is_focused(window));
            match selected {
                Some(Control::Go) => self.go(self.path_input.read(cx).text(), cx),
                Some(Control::Home) => self.open_home(cx),
                Some(Control::Up) => self.up(cx),
                Some(Control::Hidden) => self.toggle_hidden(cx),
                Some(Control::List) => {
                    if let Some(index) = self.highlighted {
                        self.enter_folder(index, cx);
                    }
                }
                Some(Control::Previous) => self.page(false, cx),
                Some(Control::Next) => self.page(true, cx),
                Some(Control::NewFolder) => self.start_create(window, cx),
                Some(Control::Create) => self.create_folder(self.name_input.read(cx).text(), cx),
                Some(Control::CancelCreate) => self.cancel_create(window, cx),
                Some(Control::Cancel) => self.cancel(window, cx),
                Some(Control::Use) => self.choose(cx),
                _ => {}
            }
        }
        // The modal owns all remaining keys, including the application's
        // navigation shortcuts and type-to-focus composer behavior.
        cx.stop_propagation();
    }

    fn toggle_hidden(&mut self, cx: &mut Context<Self>) {
        if !self.navigating_allowed() {
            return;
        }
        self.show_hidden = !self.show_hidden;
        self.browse(
            self.page.as_ref().map(|page| page.path.clone()),
            vec![None],
            cx,
        );
    }

    fn render_entry(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(entry) = self.page.as_ref().and_then(|page| page.entries.get(index)) else {
            return div().into_any_element();
        };
        div()
            .id(("project-folder-row", index))
            .debug_selector(move || format!("project-folder-row-{index}"))
            .role(gpui::Role::ListBoxOption)
            .aria_label(entry.name.clone())
            .aria_selected(self.highlighted == Some(index))
            .h(px(38.))
            .w_full()
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .when(self.highlighted == Some(index), |row| {
                row.bg(gpui::rgb(theme::bg_sidebar_row_selected()))
            })
            .hover(|row| row.bg(theme::overlay_hover()).cursor_pointer())
            .on_click(cx.listener(move |this, _, window, cx| {
                window.focus(&this.focus(Control::List, cx), cx);
                this.enter_folder(index, cx);
            }))
            .child(icon("folder", px(16.), theme::text_secondary()))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .child(entry.name.clone()),
            )
            .child(icon("chevron-right", px(12.), theme::text_muted()))
            .into_any_element()
    }
}

impl Render for ProjectFolderDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = (window.viewport_size().width - px(32.)).clamp(px(0.), px(640.));
        let height = (window.viewport_size().height - px(32.)).clamp(px(0.), px(640.));
        let can_use = self.can_use(cx);
        let busy = self.busy.is_some();
        let list_owner = cx.entity().downgrade();
        let list = gpui::list(self.list.clone(), move |index, _, cx| {
            list_owner
                .update(cx, |this, cx| this.render_entry(index, cx))
                .unwrap_or_else(|_| div().into_any_element())
        })
        .size_full();
        let small_button = |id, label, control| {
            widgets::secondary_button(id)
                .aria_label(label)
                .px_3()
                .py_1()
                .track_focus(&self.focus(control, cx))
                .child(label)
        };
        div()
            .id("project-folder-backdrop")
            .absolute()
            .inset_0()
            .size_full()
            .occlude()
            .bg(theme::scrim())
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("project-folder-dialog")
                    .debug_selector(|| "project-folder-dialog".into())
                    .role(gpui::Role::Dialog)
                    .aria_label("Choose a project folder")
                    .key_context("Dialog")
                    .on_key_down(cx.listener(Self::key))
                    .w(width)
                    .h(height)
                    .p_5()
                    .rounded(theme::RADIUS_XL)
                    .shadow_lg()
                    .bg(gpui::rgb(theme::bg_elevated()))
                    .border_1()
                    .border_color(gpui::rgb(theme::border()))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_primary()))
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("Choose a project folder"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(gpui::rgb(theme::text_secondary()))
                                    .child(self.computer.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                widgets::icon_button(
                                    "project-folder-up",
                                    "arrow-left",
                                    "Back to parent folder",
                                    widgets::ROW_ICON,
                                    theme::text_secondary(),
                                )
                                .debug_selector(|| "project-folder-up".into())
                                .track_focus(&self.focus(Control::Up, cx))
                                .when(
                                    self.page
                                        .as_ref()
                                        .is_none_or(|page| page.parent_path.is_none()),
                                    |button| button.opacity(0.5),
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.up(cx))),
                            )
                            .child(
                                widgets::input_frame()
                                    .debug_selector(|| "project-folder-path".into())
                                    .flex_1()
                                    .min_w_0()
                                    .child(self.path_input.clone()),
                            )
                            .child(
                                small_button("project-folder-go", "Go", Control::Go).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.go(this.path_input.read(cx).text(), cx)
                                    }),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                small_button("project-folder-home", "Home", Control::Home)
                                    .on_click(cx.listener(|this, _, _, cx| this.open_home(cx))),
                            )
                            .child(
                                small_button(
                                    "project-folder-hidden",
                                    if self.show_hidden {
                                        "Hide hidden folders"
                                    } else {
                                        "Show hidden folders"
                                    },
                                    Control::Hidden,
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_hidden(cx))),
                            ),
                    )
                    .child(
                        div()
                            .id("project-folder-list")
                            .debug_selector(|| "project-folder-list".into())
                            .role(gpui::Role::ListBox)
                            .aria_label("Folders — Enter opens the highlighted folder")
                            .track_focus(&self.focus(Control::List, cx))
                            .focus_visible(|style| style.border_color(gpui::rgb(theme::accent())))
                            .flex_1()
                            .min_h(px(38.))
                            .min_w_0()
                            .relative()
                            .overflow_hidden()
                            .bg(gpui::rgb(theme::bg_elevated()))
                            .border_1()
                            .border_color(gpui::rgb(theme::border()))
                            .rounded(theme::RADIUS_MD)
                            .when(busy, |list| list.opacity(0.5))
                            .child(list)
                            .when(
                                !busy
                                    && self
                                        .page
                                        .as_ref()
                                        .is_some_and(|page| page.entries.is_empty()),
                                |list| {
                                    list.child(
                                        div()
                                            .absolute()
                                            .inset_0()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_color(gpui::rgb(theme::text_muted()))
                                            .child("No folders here"),
                                    )
                                },
                            ),
                    )
                    .when(
                        self.cursors.len() > 1
                            || self.page.as_ref().is_some_and(|page| page.next.is_some()),
                        |card| {
                            card.child(
                                div()
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .when(self.cursors.len() > 1, |row| {
                                        row.child(
                                            small_button(
                                                "project-folder-previous",
                                                "Previous folders",
                                                Control::Previous,
                                            )
                                            .on_click(
                                                cx.listener(|this, _, _, cx| this.page(false, cx)),
                                            ),
                                        )
                                    })
                                    .when(
                                        self.page.as_ref().is_some_and(|page| page.next.is_some()),
                                        |row| {
                                            row.child(
                                                small_button(
                                                    "project-folder-next",
                                                    "More folders",
                                                    Control::Next,
                                                )
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.page(true, cx)
                                                })),
                                            )
                                        },
                                    ),
                            )
                        },
                    )
                    .when_some(self.busy, |card, busy| {
                        card.child(
                            div()
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(gpui::rgb(theme::text_secondary()))
                                .child(match busy {
                                    Busy::Browsing => "Loading folders…",
                                    Busy::Creating => "Creating folder…",
                                    Busy::Selecting => "Opening project…",
                                }),
                        )
                    })
                    .when_some(self.error.clone(), |card, error| {
                        card.child(
                            div()
                                .id("project-folder-error")
                                .max_h(px(72.))
                                .overflow_y_scroll()
                                .text_xs()
                                .text_color(gpui::rgb(theme::status_error()))
                                .child(error)
                                .when_some(self.page.as_ref(), |error, page| {
                                    error.child(
                                        div()
                                            .text_color(gpui::rgb(theme::text_secondary()))
                                            .child(format!("Folders shown: {}", page.path)),
                                    )
                                }),
                        )
                    })
                    .when(self.creating, |card| {
                        card.child(
                            div()
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .flex_wrap()
                                .gap_2()
                                .child(
                                    widgets::input_frame()
                                        .flex_1()
                                        .min_w(px(100.))
                                        .child(self.name_input.clone()),
                                )
                                .child(
                                    small_button(
                                        "project-folder-create",
                                        "Create",
                                        Control::Create,
                                    )
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.create_folder(this.name_input.read(cx).text(), cx)
                                        },
                                    )),
                                )
                                .child(
                                    small_button(
                                        "project-folder-create-cancel",
                                        "Cancel",
                                        Control::CancelCreate,
                                    )
                                    .aria_label("Cancel new folder")
                                    .on_click(cx.listener(
                                        |this, _, window, cx| this.cancel_create(window, cx),
                                    )),
                                ),
                        )
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .when(!self.creating, |row| {
                                row.child(
                                    widgets::ghost_button("project-folder-new")
                                        .aria_label("New folder")
                                        .debug_selector(|| "project-folder-new".into())
                                        .px_3()
                                        .track_focus(&self.focus(Control::NewFolder, cx))
                                        .when(!can_use, |button| button.opacity(0.5))
                                        .child("New folder")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.start_create(window, cx)
                                        })),
                                )
                            })
                            .child(
                                div()
                                    .ml_auto()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        widgets::secondary_button("project-folder-cancel")
                                            .aria_label("Cancel")
                                            .debug_selector(|| "project-folder-cancel".into())
                                            .track_focus(&self.focus(Control::Cancel, cx))
                                            .child("Cancel")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.cancel(window, cx)
                                            })),
                                    )
                                    .child(
                                        widgets::primary_button("project-folder-use")
                                            .aria_label("Use folder")
                                            .debug_selector(|| "project-folder-use".into())
                                            .track_focus(&self.focus(Control::Use, cx))
                                            .when(!can_use, |button| button.opacity(0.5))
                                            .child("Use folder")
                                            .on_click(
                                                cx.listener(|this, _, _, cx| this.choose(cx)),
                                            ),
                                    ),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::ProjectFolderEntry;

    fn page(path: &str) -> ProjectFolderPage {
        ProjectFolderPage {
            path: path.into(),
            parent_path: Some("/".into()),
            next: None,
            entries: (0..200)
                .map(|index| ProjectFolderEntry {
                    name: format!("Folder {index}"),
                    path: format!("{path}/{index}"),
                })
                .collect(),
        }
    }

    fn dialog(cx: &mut gpui::TestAppContext) -> Entity<ProjectFolderDialog> {
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        cx.new(|cx| {
            ProjectFolderDialog::new(backend, "user".into(), "Test computer".into(), false, cx)
        })
    }

    #[gpui::test]
    fn stale_folder_replies_cannot_replace_the_current_folder(cx: &mut gpui::TestAppContext) {
        let dialog = dialog(cx);
        dialog.update(cx, |this, cx| {
            this.generation = 2;
            this.busy = Some(Busy::Browsing);
            this.finish_browse(1, String::new(), vec![None], Ok(page("/stale")), cx);
            assert!(this.page.is_none());
            assert!(this.busy == Some(Busy::Browsing));
            this.finish_browse(2, String::new(), vec![None], Ok(page("/latest")), cx);
            assert_eq!(this.page.as_ref().unwrap().path, "/latest");
            this.finish_browse(1, String::new(), vec![None], Err("old error".into()), cx);
            assert!(this.error.is_none());
            this.closed = true;
            this.finish_browse(2, "/latest".into(), vec![None], Ok(page("/closed")), cx);
            assert_eq!(this.page.as_ref().unwrap().path, "/latest");
        });
    }

    #[gpui::test]
    fn edited_paths_busy_states_and_errors_cannot_confirm_an_old_folder(
        cx: &mut gpui::TestAppContext,
    ) {
        let dialog = dialog(cx);
        dialog.update(cx, |this, cx| {
            this.finish_browse(0, String::new(), vec![None], Ok(page("/home")), cx);
            assert!(this.can_use(cx));
            this.path_input
                .update(cx, |input, cx| input.set_text("/typed", cx));
            assert!(!this.can_use(cx));
            this.busy = Some(Busy::Browsing);
            this.finish_browse(0, "/home".into(), vec![None], Ok(page("/returned")), cx);
            assert_eq!(
                this.path_input.read(cx).text(),
                "/typed",
                "in-flight edits survive"
            );
            assert!(!this.can_use(cx));
            this.finish_browse(
                0,
                "/typed".into(),
                vec![None],
                Err("permission denied".into()),
                cx,
            );
            assert_eq!(
                this.page.as_ref().unwrap().path,
                "/returned",
                "failed navigation keeps previous rows"
            );
            assert!(!this.can_use(cx));
            this.finish_browse(0, "/typed".into(), vec![None], Ok(page("/home")), cx);
            for busy in [Busy::Browsing, Busy::Creating, Busy::Selecting] {
                this.busy = Some(busy);
                assert!(!this.can_use(cx));
            }
        });
    }

    struct DialogHost {
        dialog: Entity<ProjectFolderDialog>,
    }
    impl Render for DialogHost {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.dialog
                .update(cx, |dialog, cx| dialog.sync_focus(window, cx));
            div().size_full().relative().child(self.dialog.clone())
        }
    }

    fn window(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<ProjectFolderDialog>, &mut gpui::VisualTestContext) {
        let dialog = dialog(cx);
        dialog.update(cx, |this, cx| {
            this.finish_browse(0, String::new(), vec![None], Ok(page("/home")), cx)
        });
        let copy = dialog.clone();
        let (_, cx) = cx.add_window_view(|_, _| DialogHost { dialog: copy });
        (dialog, cx)
    }

    #[gpui::test]
    fn folder_dialog_traps_tab_and_escape_cancels_only_new_folder_first(
        cx: &mut gpui::TestAppContext,
    ) {
        let (dialog, cx) = window(cx);
        cx.simulate_keystrokes("shift-tab");
        assert!(cx.update(|window, cx| dialog.read(cx).focus(Control::Up, cx).is_focused(window)));
        let back = cx.debug_bounds("project-folder-up").unwrap();
        let path = cx.debug_bounds("project-folder-path").unwrap();
        assert!(back.right() <= path.left());
        cx.simulate_keystrokes("shift-tab");
        assert!(cx.update(|window, cx| dialog.read(cx).focus(Control::Use, cx).is_focused(window)));
        cx.simulate_keystrokes("tab");
        assert!(cx.update(|window, cx| dialog.read(cx).focus(Control::Up, cx).is_focused(window)));
        cx.simulate_keystrokes("tab");
        assert!(
            cx.update(|window, cx| dialog.read(cx).focus(Control::Path, cx).is_focused(window))
        );
        cx.update(|window, cx| dialog.update(cx, |this, cx| this.start_create(window, cx)));
        cx.simulate_input("my project");
        cx.simulate_keystrokes("escape");
        dialog.update(cx, |this, _| {
            assert!(!this.creating);
            assert!(!this.closed);
        });
        cx.simulate_keystrokes("escape");
        assert!(dialog.update(cx, |this, _| this.closed));
    }

    #[gpui::test]
    fn folder_dialog_keeps_confirmation_aligned_when_new_folder_opens(
        cx: &mut gpui::TestAppContext,
    ) {
        let (dialog, cx) = window(cx);
        cx.simulate_resize(gpui::size(px(640.), px(600.)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let before = cx.debug_bounds("project-folder-use").unwrap();
        let before_card = cx.debug_bounds("project-folder-dialog").unwrap();
        assert!(before.left() >= before_card.left() && before.right() <= before_card.right());
        cx.update(|window, cx| dialog.update(cx, |this, cx| this.start_create(window, cx)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let after = cx.debug_bounds("project-folder-use").unwrap();
        let after_card = cx.debug_bounds("project-folder-dialog").unwrap();
        assert!(after.left() >= after_card.left() && after.right() <= after_card.right());
        assert_eq!(after.right(), before.right());
        assert!(
            cx.update(|window, cx| { dialog.read(cx).focus(Control::Name, cx).is_focused(window) })
        );
        cx.simulate_keystrokes("escape");
        assert!(!dialog.update(cx, |this, _| this.creating));
        assert!(!dialog.update(cx, |this, _| this.closed));
    }

    #[gpui::test]
    fn folder_dialog_keeps_actions_visible_and_virtualizes_rows(cx: &mut gpui::TestAppContext) {
        let (dialog, cx) = window(cx);
        for (width, height) in [(1200., 800.), (640., 600.), (360., 480.)] {
            cx.simulate_resize(gpui::size(px(width), px(height)));
            let card = cx.debug_bounds("project-folder-dialog").unwrap();
            let list = cx.debug_bounds("project-folder-list").unwrap();
            let confirm = cx.debug_bounds("project-folder-use").unwrap();
            assert!(card.left() >= px(0.) && card.right() <= px(width));
            assert!(card.top() >= px(0.) && card.bottom() <= px(height));
            assert!(confirm.bottom() <= card.bottom());
            assert!(list.bottom() <= confirm.top());
            assert!(
                cx.debug_bounds("project-folder-row-199").is_none(),
                "offscreen rows must be virtualized"
            );
        }
        cx.update(|window, cx| {
            let focus = dialog.read(cx).focus(Control::List, cx);
            window.focus(&focus, cx);
        });
        cx.simulate_keystrokes("end");
        assert_eq!(dialog.update(cx, |this, _| this.highlighted), Some(199));
        // VisualTestContext's bounds are the last painted frame. Flush the
        // requested layout, just as the chat pointer fixtures do before
        // testing an element that has moved into view.
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("project-folder-row-199").is_some(),
            "end must reveal its row: {:?}",
            dialog.update(cx, |this, _| (
                this.list.logical_scroll_top(),
                this.list.bounds_for_item(199)
            ))
        );
        cx.simulate_keystrokes("home");
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert_eq!(dialog.update(cx, |this, _| this.highlighted), Some(0));
        assert!(cx.debug_bounds("project-folder-row-0").is_some());
    }
}
