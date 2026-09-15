//! The task sidebar: an entity of its own, so it renders only when its
//! state changes and the screen embeds it as a cached view.
//!
//! The screen pushes what the sidebar shows (sessions, the selection,
//! which tasks run or finished unread, the recent roots) through the
//! setters below; the sidebar owns everything else: the list, its
//! sections, the search, menus, renames, and its Application-Vim row
//! targets. It talks back through [`SidebarEvent`], never by updating
//! the screen from inside one of its own updates, so nothing re-enters.
//! Clicks that need the window (new task, settings, collapse) call the
//! screen directly from plain closures, which run outside any update.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{
    AnyElement, AppContext, Context, Div, Entity, EventEmitter, Focusable, SharedString, Task,
    WeakEntity, Window, div, prelude::*, px,
};
use maple_agent::agent::{AgentProjectTrustStatus, AgentSessionSummary};

use super::commands::ChatCommand;
use super::navigation::SidebarTarget;
use super::{ChatScreen, section_label};
use crate::backend::AgentBackend;
use crate::ui::icons::{icon, spinner_with_id, wordmark};
use crate::ui::motion;
use crate::ui::text_input::TextInput;
use crate::ui::theme;
use crate::ui::titlebar;
use crate::ui::widgets;

/// What the sidebar asks the screen to do.
#[derive(Clone, Debug)]
pub(super) enum SidebarEvent {
    /// Show a task.
    Select(String),
    /// A session changed on the backend (a rename); the screen owns the
    /// canonical list and pushes it back.
    SessionChanged(AgentSessionSummary),
    /// Archive or restore a task.
    SetArchived { session_id: String, archived: bool },
    /// Remove a project; the screen confirms first.
    RemoveRoot(String),
    /// Trust or untrust a project.
    SetTrust { path: String, trusted: bool },
    /// Open the folder picker for a new project.
    ChooseProject,
    /// Something to tell the user.
    Notice(SharedString),
}

/// Activity a task row indicates beside its title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionActivity {
    Running,
    CompletedUnread,
}

/// What an inline rename edits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RenameTarget {
    Task(String),
    Project(String),
}

/// Strings and element ids one sidebar task row shows, built when the
/// session list changes instead of on every frame.
pub(super) struct SidebarRow {
    pub(super) id: Arc<str>,
    pub(super) element_id: SharedString,
    /// Hover group that reveals the row's action buttons.
    pub(super) group: SharedString,
    pub(super) rename_id: SharedString,
    pub(super) archive_id: SharedString,
    /// Animation id of the running indicator.
    pub(super) spinner_id: SharedString,
    pub(super) pin_id: SharedString,
    pub(super) menu_id: SharedString,
    /// Id of the overflow-menu panel itself.
    pub(super) menu_panel_id: SharedString,
    pub(super) menu_rename_id: SharedString,
    pub(super) menu_pin_id: SharedString,
    pub(super) menu_settle_id: SharedString,
    pub(super) menu_archive_id: SharedString,
    pub(super) menu_python_reset_id: SharedString,
    pub(super) title: SharedString,
    /// Display name of the task's project, shown on every row.
    pub(super) project_name: SharedString,
    /// Lower-cased title, matched against the sidebar filter.
    pub(super) search: String,
}

impl SidebarRow {
    fn build(session: &AgentSessionSummary, project_name: &str) -> Self {
        let id = &session.id;
        Self {
            id: Arc::from(id.as_str()),
            element_id: SharedString::from(format!("session-{id}")),
            group: SharedString::from(format!("task-row-{id}")),
            rename_id: SharedString::from(format!("rename-session-{id}")),
            archive_id: SharedString::from(format!("archive-session-{id}")),
            spinner_id: SharedString::from(format!("spinner-session-{id}")),
            pin_id: SharedString::from(format!("pin-session-{id}")),
            menu_id: SharedString::from(format!("menu-session-{id}")),
            menu_panel_id: SharedString::from(format!("task-menu-{id}")),
            menu_rename_id: SharedString::from(format!("rename-task-{id}")),
            menu_pin_id: SharedString::from(format!("pin-task-{id}")),
            menu_settle_id: SharedString::from(format!("settle-task-{id}")),
            menu_archive_id: SharedString::from(format!("archive-task-{id}")),
            menu_python_reset_id: SharedString::from(format!("reset-python-{id}")),
            title: SharedString::from(session.title.clone()),
            project_name: SharedString::from(project_name.to_string()),
            search: session.title.to_lowercase(),
        }
    }
}

/// The popup menu the sidebar shows. The project menu opens inside the
/// switcher menu, so its variant comes first in `popup`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SidebarPopup {
    Switcher,
    Project(String),
    Task(String),
}

/// A menu can close and reopen for the same task before its status read
/// finishes. The request sequence distinguishes those otherwise equal reads.
#[derive(Clone)]
struct PythonMenuRequest {
    user_id: String,
    session_id: String,
    sequence: u64,
}

impl PythonMenuRequest {
    fn is_current(&self, user_id: &str, task_menu: Option<&str>, sequence: u64) -> bool {
        self.user_id == user_id
            && task_menu == Some(self.session_id.as_str())
            && self.sequence == sequence
    }
}

/// One row of the virtualized sidebar list, in display order. Rebuilt
/// with the sections and when a section folds; the list builds only the
/// rows on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SidebarEntry {
    NewTask,
    /// The project switcher row above the sections.
    ProjectsHeader,
    /// A section label, shown only when the section has rows.
    SectionLabel(SidebarSection),
    Task(SidebarTaskEntry),
    ArchivedHeader,
    /// Placeholder row when no task row is listed.
    Empty(SidebarEmpty),
}

/// A task row: its index into `sessions` plus the section flags the
/// rebuild already decided, so render never re-derives them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SidebarTaskEntry {
    pub(super) session: usize,
    pub(super) pinned: bool,
    pub(super) settled: bool,
    pub(super) archived: bool,
}

/// Why the task list has no rows to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SidebarEmpty {
    /// No tasks exist yet.
    NoTasks,
    /// The search filter matched nothing.
    NoMatches,
}

/// A named section of the inbox: pinned tasks, active work, and the
/// settled rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SidebarSection {
    Pinned,
    Active,
    Settled,
}

impl SidebarSection {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Pinned => "PINNED",
            Self::Active => "ACTIVE",
            Self::Settled => "SETTLED",
        }
    }

    pub(super) fn element_id(self) -> &'static str {
        match self {
            Self::Pinned => "section-toggle-pinned",
            Self::Active => "section-toggle-active",
            Self::Settled => "section-toggle-settled",
        }
    }
}

type MenuAction = Box<dyn Fn(&mut Sidebar, &mut Context<Sidebar>)>;

/// One selectable action of a sidebar popup menu. Building the list once
/// keeps the render path and the Application-Vim Enter key on the same
/// items.
struct SidebarMenuItem {
    id: SharedString,
    icon: &'static str,
    label: &'static str,
    on_click: MenuAction,
}

/// One project the switcher menu lists: the root path, its display name,
/// and the element ids its row and menus use. Built when the sections
/// rebuild, never per frame.
pub(super) struct SwitcherRoot {
    root: String,
    name: SharedString,
    row_id: SharedString,
    row_group: SharedString,
    menu_id: SharedString,
}

pub(super) struct Sidebar {
    backend: Arc<AgentBackend>,
    user_id: String,
    chat: WeakEntity<ChatScreen>,
    // What the screen pushes in.
    sessions: Vec<AgentSessionSummary>,
    selected: Option<String>,
    /// Tasks with a run in flight.
    running: HashSet<String>,
    /// Tasks that finished while not on screen.
    unread: HashSet<String>,
    recent_roots: Vec<String>,
    application_vim_enabled: bool,
    /// Application Vim's region is the sidebar, so rows show its
    /// selection.
    vim_active: bool,
    // The list and its sections.
    list: gpui::ListState,
    entries: Vec<SidebarEntry>,
    rows: Vec<SidebarRow>,
    pinned_rows: Vec<usize>,
    active_rows: Vec<usize>,
    settled_rows: Vec<usize>,
    archived_rows: Vec<usize>,
    pinned_expanded: bool,
    active_expanded: bool,
    settled_expanded: bool,
    archived_expanded: bool,
    switcher_roots: Vec<SwitcherRoot>,
    scope_label: SharedString,
    project_filter: Option<String>,
    // Menus.
    switcher_menu_open: bool,
    menu_selected: Option<usize>,
    task_menu: Option<String>,
    project_menu: Option<String>,
    menu_trust: Option<AgentProjectTrustStatus>,
    python_menu_sequence: u64,
    menu_python_resettable: bool,
    // Search and rename.
    filter: String,
    search_input: Entity<TextInput>,
    rename: Option<RenameTarget>,
    rename_input: Option<Entity<TextInput>>,
    rename_focus_pending: bool,
    // Persisted in the app settings.
    pinned_tasks: Vec<String>,
    settled_tasks: HashSet<String>,
    unsettled_tasks: HashSet<String>,
    project_names: HashMap<String, String>,
    // Application Vim's view of the rows.
    vim_selected: Option<SidebarTarget>,
    vim_by_row: Vec<Option<SidebarTarget>>,
    vim_order: Vec<SidebarTarget>,
    /// Tasks bridging backend futures; dropped on this thread.
    bridged_tasks: RefCell<Vec<Task<()>>>,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Sidebar {
    pub(super) fn new(
        backend: Arc<AgentBackend>,
        user_id: String,
        chat: WeakEntity<ChatScreen>,
        settings: &crate::settings::AppSettings,
        cx: &mut Context<Self>,
    ) -> Self {
        let application_vim_enabled = settings.application_vim_enabled;
        let search_chat = chat.clone();
        let search_input = cx.new(move |cx| {
            TextInput::new("Search tasks", cx)
                .with_tab_index(2)
                .application_vim(application_vim_enabled)
                .on_application_escape(move |window, cx| {
                    if let Some(chat) = search_chat.upgrade() {
                        chat.update(cx, |chat, cx| chat.focus_application_vim(window, cx));
                    }
                })
        });
        cx.observe(&search_input, |this, input, cx| {
            this.search_changed(&input, cx)
        })
        .detach();
        Self {
            backend,
            user_id,
            chat,
            sessions: Vec::new(),
            selected: None,
            running: HashSet::new(),
            unread: HashSet::new(),
            recent_roots: Vec::new(),
            application_vim_enabled,
            vim_active: false,
            list: gpui::ListState::new(0, gpui::ListAlignment::Top, px(200.)),
            entries: Vec::new(),
            rows: Vec::new(),
            pinned_rows: Vec::new(),
            active_rows: Vec::new(),
            settled_rows: Vec::new(),
            archived_rows: Vec::new(),
            pinned_expanded: true,
            active_expanded: true,
            settled_expanded: true,
            archived_expanded: false,
            switcher_roots: Vec::new(),
            scope_label: "All projects".into(),
            project_filter: None,
            switcher_menu_open: false,
            menu_selected: None,
            task_menu: None,
            project_menu: None,
            menu_trust: None,
            python_menu_sequence: 0,
            menu_python_resettable: false,
            filter: String::new(),
            search_input,
            rename: None,
            rename_input: None,
            rename_focus_pending: false,
            pinned_tasks: settings.pinned_tasks.clone(),
            settled_tasks: settings.settled_tasks.iter().cloned().collect(),
            unsettled_tasks: settings.unsettled_tasks.iter().cloned().collect(),
            project_names: settings.project_names.clone(),
            vim_selected: None,
            vim_by_row: Vec::new(),
            vim_order: Vec::new(),
            bridged_tasks: RefCell::new(Vec::new()),
        }
    }

    // ---- Inputs from the screen ------------------------------------------

    /// Replace the session list. The screen owns the canonical list and
    /// pushes it on every sync, so a list that did not change is a no-op:
    /// no rebuild, no repaint.
    pub(super) fn set_sessions(
        &mut self,
        sessions: Vec<AgentSessionSummary>,
        cx: &mut Context<Self>,
    ) {
        let unchanged = self.sessions.len() == sessions.len()
            && self
                .sessions
                .iter()
                .zip(&sessions)
                .all(|(current, next)| session_summary_eq(current, next));
        if unchanged {
            return;
        }
        self.sessions = sessions;
        self.rebuild_sections();
        cx.notify();
    }

    pub(super) fn set_selected(&mut self, selected: Option<String>, cx: &mut Context<Self>) {
        if self.selected == selected {
            return;
        }
        self.selected = selected;
        if self.application_vim_enabled {
            self.vim_ensure_selection();
        }
        cx.notify();
    }

    /// Which tasks run and which finished unread; both drive the row
    /// indicator and the active section.
    pub(super) fn set_activity(
        &mut self,
        running: HashSet<String>,
        unread: HashSet<String>,
        cx: &mut Context<Self>,
    ) {
        if self.running == running && self.unread == unread {
            return;
        }
        let sections_changed = self.running != running;
        self.running = running;
        self.unread = unread;
        if sections_changed {
            self.rebuild_sections();
        }
        cx.notify();
    }

    pub(super) fn set_recent_roots(&mut self, roots: Vec<String>, cx: &mut Context<Self>) {
        if self.recent_roots == roots {
            return;
        }
        self.recent_roots = roots;
        self.rebuild_sections();
        cx.notify();
    }

    pub(super) fn set_application_vim_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        for input in [Some(&self.search_input), self.rename_input.as_ref()]
            .into_iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>()
        {
            input.update(cx, |input, cx| {
                input.set_application_vim_enabled(enabled, cx)
            });
        }
        if self.application_vim_enabled == enabled {
            return;
        }
        self.application_vim_enabled = enabled;
        if enabled {
            self.rebuild_vim_targets();
            self.vim_reconcile();
        } else {
            self.vim_selected = None;
            self.vim_by_row.clear();
            self.vim_order.clear();
        }
        cx.notify();
    }

    /// Whether Application Vim's region is the sidebar.
    pub(super) fn set_vim_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.vim_active != active {
            self.vim_active = active;
            cx.notify();
        }
    }

    /// A fresh completion is new activity: it wakes a task the user
    /// settled away earlier.
    pub(super) fn wake_task(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if self.settled_tasks.remove(session_id) {
            self.rebuild_sections();
            cx.notify();
        }
    }

    /// Forget tasks that left the app (their project was removed).
    pub(super) fn forget_tasks(&mut self, ids: &[String], cx: &mut Context<Self>) {
        self.pinned_tasks
            .retain(|candidate| !ids.contains(candidate));
        self.settled_tasks
            .retain(|candidate| !ids.contains(candidate));
        self.unsettled_tasks
            .retain(|candidate| !ids.contains(candidate));
        self.rebuild_sections();
        cx.notify();
    }

    // ---- Reads for the screen --------------------------------------------

    /// The text inputs the sidebar owns, for focus bookkeeping.
    pub(super) fn inputs(&self) -> Vec<Entity<TextInput>> {
        [Some(&self.search_input), self.rename_input.as_ref()]
            .into_iter()
            .flatten()
            .cloned()
            .collect()
    }

    /// Display name for a project root: the saved name, else the folder name.
    pub(super) fn root_name(&self, root: &str) -> String {
        self.project_names
            .get(root)
            .filter(|name| !name.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| root_display_name(root))
    }

    /// The task `delta` rows away from the selected one, as the sidebar
    /// row it sits on and its id. Stepping walks the flat sidebar list
    /// and stops at its ends.
    pub(super) fn step_target(&self, delta: isize) -> Option<(usize, String)> {
        let rows: Vec<(usize, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(row, entry)| match entry {
                SidebarEntry::Task(task) if !task.archived => Some((row, task.session)),
                _ => None,
            })
            .collect();
        if rows.is_empty() {
            return None;
        }
        let selected = self.selected.as_deref();
        let current = selected.and_then(|id| {
            rows.iter().position(|(_, session)| {
                self.sessions
                    .get(*session)
                    .is_some_and(|summary| summary.id == id)
            })
        });
        let target = match current {
            Some(position) => {
                let next = position as isize + delta;
                if next < 0 || next as usize >= rows.len() {
                    return None;
                }
                next as usize
            }
            None if delta > 0 => 0,
            None => rows.len() - 1,
        };
        let (row, session) = rows[target];
        let id = self.sessions.get(session)?.id.clone();
        Some((row, id))
    }

    pub(super) fn reveal_row(&self, row: usize) {
        self.list.scroll_to_reveal_item(row);
    }

    #[cfg(test)]
    pub(super) fn entries(&self) -> &[SidebarEntry] {
        &self.entries
    }

    #[cfg(test)]
    pub(super) fn pinned_rows(&self) -> &[usize] {
        &self.pinned_rows
    }

    #[cfg(test)]
    pub(super) fn active_rows(&self) -> &[usize] {
        &self.active_rows
    }

    #[cfg(test)]
    pub(super) fn settled_rows(&self) -> &[usize] {
        &self.settled_rows
    }

    #[cfg(test)]
    pub(super) fn archived_rows(&self) -> &[usize] {
        &self.archived_rows
    }

    #[cfg(test)]
    pub(super) fn rows(&self) -> &[SidebarRow] {
        &self.rows
    }

    #[cfg(test)]
    pub(super) fn project_filter(&self) -> Option<&str> {
        self.project_filter.as_deref()
    }

    #[cfg(test)]
    pub(super) fn switcher_menu_open(&self) -> bool {
        self.switcher_menu_open
    }

    #[cfg(test)]
    pub(super) fn task_menu(&self) -> Option<&str> {
        self.task_menu.as_deref()
    }

    #[cfg(test)]
    pub(super) fn settled_tasks(&self) -> &HashSet<String> {
        &self.settled_tasks
    }

    #[cfg(test)]
    pub(super) fn set_project_name_for_test(&mut self, root: &str, name: &str) {
        self.project_names
            .insert(root.to_string(), name.to_string());
        self.rebuild_sections();
    }

    #[cfg(test)]
    pub(super) fn set_filter_for_test(&mut self, filter: &str) {
        self.filter = filter.to_lowercase();
        self.rebuild_sections();
    }

    #[cfg(test)]
    pub(super) fn set_pinned_tasks_for_test(&mut self, pinned: Vec<String>) {
        self.pinned_tasks = pinned;
        self.rebuild_sections();
    }

    #[cfg(test)]
    pub(super) fn set_settled_for_test(&mut self, settled: HashSet<String>) {
        self.settled_tasks = settled;
        self.rebuild_sections();
    }

    /// Other tests persist their fixture ids into the settings file this
    /// process reads; a fixture starts from nothing persisted.
    #[cfg(test)]
    pub(super) fn reset_persisted_for_test(&mut self) {
        self.pinned_tasks.clear();
        self.settled_tasks.clear();
        self.unsettled_tasks.clear();
        self.project_names.clear();
        self.rebuild_sections();
    }

    #[cfg(test)]
    pub(super) fn rename_target(&self) -> Option<RenameTarget> {
        self.rename.clone()
    }

    #[cfg(test)]
    pub(super) fn archived_expanded(&self) -> bool {
        self.archived_expanded
    }

    #[cfg(test)]
    pub(super) fn open_task_menu_for_test(&mut self, session_id: &str, cx: &mut Context<Self>) {
        self.task_menu = Some(session_id.to_string());
        cx.notify();
    }

    #[cfg(test)]
    pub(super) fn set_python_resettable_for_test(&mut self, resettable: bool) {
        self.menu_python_resettable = resettable;
    }

    #[cfg(test)]
    pub(super) fn task_menu_labels_for_test(&self, session_id: &str) -> Vec<&'static str> {
        self.task_menu_entry(session_id)
            .map(|task| {
                self.task_menu_items(task)
                    .iter()
                    .map(|item| item.label)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(super) fn set_unsettled_for_test(&mut self, unsettled: HashSet<String>) {
        self.unsettled_tasks = unsettled;
        self.rebuild_sections();
    }

    #[cfg(test)]
    pub(super) fn list(&self) -> &gpui::ListState {
        &self.list
    }

    #[cfg(test)]
    pub(super) fn search_focus_handle(&self, cx: &gpui::App) -> gpui::FocusHandle {
        self.search_input.read(cx).focus_handle(cx)
    }

    /// Reveal the switcher root paths for tests.
    #[cfg(test)]
    pub(super) fn switcher_root_paths(&self) -> Vec<&str> {
        self.switcher_roots
            .iter()
            .map(|root| root.root.as_str())
            .collect()
    }

    #[cfg(test)]
    pub(super) fn vim_projection_is_empty(&self) -> bool {
        self.vim_selected.is_none() && self.vim_by_row.is_empty() && self.vim_order.is_empty()
    }

    // ---- Sections --------------------------------------------------------

    pub(super) fn session_activity(&self, session_id: &str) -> Option<SessionActivity> {
        if self.running.contains(session_id) {
            Some(SessionActivity::Running)
        } else if self.unread.contains(session_id) {
            Some(SessionActivity::CompletedUnread)
        } else {
            None
        }
    }

    pub(super) fn set_archived_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        if self.archived_expanded == expanded {
            return;
        }
        self.archived_expanded = expanded;
        self.rebuild_entries();
        cx.notify();
    }

    pub(super) fn toggle_archived_visibility(&mut self, cx: &mut Context<Self>) {
        self.set_archived_expanded(!self.archived_expanded, cx);
    }

    pub(super) fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
        cx.notify();
    }

    /// Rebuild the sidebar sections: pinned tasks first, then the active
    /// inbox, then the settled rest. Rows are one flat list across
    /// projects and each row names its project. The order is derived,
    /// never written back, and changes only when activity or the user
    /// changes it. Called when sessions, roots, names, or the filter
    /// change, not per render.
    fn rebuild_sections(&mut self) {
        self.sync_rows();
        let filter = self.filter.as_str();
        let filtering = !filter.is_empty();
        let scoped = self.project_filter.as_deref();
        let pinned_ids: HashSet<&str> = self.pinned_tasks.iter().map(String::as_str).collect();
        let mut pinned = Vec::new();
        let mut active = Vec::new();
        let mut settled = Vec::new();
        let mut archived = Vec::new();
        let mut session_roots: Vec<&str> = Vec::new();
        let mut seen_roots: HashSet<&str> = HashSet::new();
        let mut root_search: HashMap<&str, String> = HashMap::new();
        for (index, session) in self.sessions.iter().enumerate() {
            let root = session.project_root.as_str();
            if seen_roots.insert(root) {
                session_roots.push(root);
            }
            if session.message_count == 0 {
                continue;
            }
            if scoped.is_some_and(|scoped| scoped != root) {
                continue;
            }
            let matches = !filtering
                || self.rows[index].search.contains(filter)
                || root_search
                    .entry(root)
                    .or_insert_with(|| self.root_name(root).to_lowercase())
                    .contains(filter);
            if !matches {
                continue;
            }
            if session.archived {
                archived.push(index);
            } else if pinned_ids.contains(session.id.as_str()) {
                pinned.push(index);
            } else if self.session_is_active(session) {
                active.push(index);
            } else {
                settled.push(index);
            }
        }
        let by_update = |ix: &usize| {
            std::cmp::Reverse(
                self.sessions
                    .get(*ix)
                    .map(|session| session.updated_ms)
                    .unwrap_or(0),
            )
        };
        pinned.sort_by_cached_key(by_update);
        active.sort_by_cached_key(|ix| {
            let unsettled = self
                .sessions
                .get(*ix)
                .is_some_and(|session| self.unsettled_tasks.contains(&session.id));
            (std::cmp::Reverse(unsettled), by_update(ix))
        });
        settled.sort_by_cached_key(by_update);
        self.pinned_rows = pinned;
        self.active_rows = active;
        self.settled_rows = settled;
        self.archived_rows = archived;
        self.scope_label = self
            .project_filter
            .as_deref()
            .map(|root| self.root_name(root))
            .map(SharedString::from)
            .unwrap_or_else(|| "All projects".into());
        let recent_roots: HashSet<&str> = self.recent_roots.iter().map(String::as_str).collect();
        let mut fresh_roots: Vec<&str> = session_roots
            .iter()
            .copied()
            .filter(|root| !recent_roots.contains(root))
            .collect();
        fresh_roots.sort_unstable();
        self.switcher_roots = self
            .recent_roots
            .iter()
            .map(String::as_str)
            .chain(fresh_roots.iter().copied())
            .filter(|root| seen_roots.contains(root) || recent_roots.contains(root))
            .map(|root| SwitcherRoot {
                name: self.root_name(root).into(),
                root: root.to_string(),
                row_id: SharedString::from(format!("switcher-{root}")),
                row_group: SharedString::from(format!("switcher-project-row-{root}")),
                menu_id: SharedString::from(format!("menu-project-{root}")),
            })
            .collect();
        self.rebuild_entries();
    }

    /// Whether a task belongs to the active inbox. Every task stays
    /// active until it is settled away by hand; a run in flight is
    /// activity, so it wakes even a settled task.
    fn session_is_active(&self, session: &AgentSessionSummary) -> bool {
        self.running.contains(&session.id) || !self.settled_tasks.contains(&session.id)
    }

    /// Flatten the sections into list rows. The list splices only the
    /// span that changed, so the scroll position and the measured
    /// heights of untouched rows survive every rebuild.
    fn rebuild_entries(&mut self) {
        let mut entries = vec![SidebarEntry::NewTask, SidebarEntry::ProjectsHeader];
        let tasks = |entries: &mut Vec<SidebarEntry>,
                     section: SidebarSection,
                     rows: &[usize],
                     expanded: bool| {
            if rows.is_empty() {
                return;
            }
            entries.push(SidebarEntry::SectionLabel(section));
            if expanded {
                entries.extend(rows.iter().map(|&session| {
                    SidebarEntry::Task(SidebarTaskEntry {
                        session,
                        pinned: section == SidebarSection::Pinned,
                        settled: section == SidebarSection::Settled,
                        archived: false,
                    })
                }));
            }
        };
        tasks(
            &mut entries,
            SidebarSection::Pinned,
            &self.pinned_rows,
            self.section_expanded(SidebarSection::Pinned),
        );
        tasks(
            &mut entries,
            SidebarSection::Active,
            &self.active_rows,
            self.section_expanded(SidebarSection::Active),
        );
        tasks(
            &mut entries,
            SidebarSection::Settled,
            &self.settled_rows,
            self.section_expanded(SidebarSection::Settled),
        );
        if !self.archived_rows.is_empty() {
            entries.push(SidebarEntry::ArchivedHeader);
            if self.archived_expanded {
                entries.extend(self.archived_rows.iter().map(|&session| {
                    SidebarEntry::Task(SidebarTaskEntry {
                        session,
                        archived: true,
                        pinned: false,
                        settled: false,
                    })
                }));
            }
        }
        if !entries
            .iter()
            .any(|entry| matches!(entry, SidebarEntry::Task(_) | SidebarEntry::ArchivedHeader))
        {
            entries.push(SidebarEntry::Empty(if self.filter.is_empty() {
                SidebarEmpty::NoTasks
            } else {
                SidebarEmpty::NoMatches
            }));
        }
        let old = std::mem::replace(&mut self.entries, entries);
        if self.application_vim_enabled {
            self.rebuild_vim_targets();
        }
        let common = old.len().min(self.entries.len());
        let mut prefix = 0;
        while prefix < common && old[prefix] == self.entries[prefix] {
            prefix += 1;
        }
        let mut suffix = 0;
        while suffix < common - prefix
            && old[old.len() - 1 - suffix] == self.entries[self.entries.len() - 1 - suffix]
        {
            suffix += 1;
        }
        if prefix + suffix < old.len() || prefix + suffix < self.entries.len() {
            self.list.splice(
                prefix..old.len() - suffix,
                self.entries.len() - prefix - suffix,
            );
        }
        if self.application_vim_enabled {
            self.vim_reconcile();
        }
    }

    /// A row changed its height (a rename field came or went): let the
    /// list measure that one row again. The rest of the list keeps its
    /// scroll and its cached heights.
    fn remeasure(&self, target: &RenameTarget) {
        let row = match target {
            RenameTarget::Task(id) => self.entries.iter().position(|entry| {
                matches!(entry, SidebarEntry::Task(task)
                    if self
                        .sessions
                        .get(task.session)
                        .is_some_and(|session| session.id == *id))
            }),
            RenameTarget::Project(_) => self
                .entries
                .iter()
                .position(|entry| matches!(entry, SidebarEntry::ProjectsHeader)),
        };
        if let Some(row) = row {
            self.list.remeasure_items(row..row + 1);
        }
    }

    /// Rebuild the per-session strings when the session list moved under
    /// them; a plain filter change reuses them.
    fn sync_rows(&mut self) {
        let fresh = self.rows.len() == self.sessions.len()
            && self.rows.iter().zip(&self.sessions).all(|(row, session)| {
                *row.id == *session.id
                    && row.title.as_ref() == session.title
                    && self.root_name_matches(&session.project_root, &row.project_name)
            });
        if fresh {
            return;
        }
        self.rows = self
            .sessions
            .iter()
            .map(|session| SidebarRow::build(session, &self.root_name(&session.project_root)))
            .collect();
    }

    fn search_changed(&mut self, input: &Entity<TextInput>, cx: &mut Context<Self>) {
        let filter = input.read(cx).text_ref().trim().to_lowercase();
        if filter != self.filter {
            self.filter = filter;
            self.rebuild_sections();
            cx.notify();
        }
    }

    /// Clear the search. Reports whether one was active.
    pub(super) fn clear_search(&mut self, cx: &mut Context<Self>) -> bool {
        if self.filter.is_empty() && self.search_input.read(cx).text_ref().is_empty() {
            return false;
        }
        self.search_input.update(cx, |input, cx| input.clear(cx));
        self.filter.clear();
        self.rebuild_sections();
        cx.notify();
        true
    }

    /// Close whichever popup menu is open. Reports whether one was.
    pub(super) fn close_popups(&mut self, cx: &mut Context<Self>) -> bool {
        if self.project_menu.is_none() && !self.switcher_menu_open && self.task_menu.is_none() {
            return false;
        }
        self.project_menu = None;
        self.switcher_menu_open = false;
        self.task_menu = None;
        self.menu_selected = None;
        cx.notify();
        true
    }

    /// Run a backend future and hand its result back on this thread.
    fn call<T, F>(
        &self,
        future: F,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
    ) where
        T: Send + 'static,
        F: std::future::Future<Output = Result<T, String>> + Send + 'static,
    {
        let task = self.backend.spawn(future);
        let bridge = cx.spawn(async move |this, cx| {
            let result = task.await.unwrap_or_else(|error| {
                log::debug!("sidebar task failed: {error:?}");
                Err("The task was cancelled".to_string())
            });
            this.update(cx, |this, cx| then(this, result, cx)).ok();
        });
        crate::ui::task::retain(&self.bridged_tasks, bridge);
    }

    /// Pin or unpin a task. Pinned tasks stay at the top of the sidebar
    /// and persist in the app settings.
    pub(super) fn toggle_task_pin(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if self.pinned_tasks.iter().any(|pinned| pinned == session_id) {
            self.pinned_tasks.retain(|pinned| pinned != session_id);
        } else {
            self.pinned_tasks.push(session_id.to_string());
        }
        self.rebuild_sections();
        cx.notify();
        let pinned = self.pinned_tasks.clone();
        persist_settings(move |settings| settings.pinned_tasks = pinned);
    }

    /// Move a task to the settled section: it leaves the active inbox
    /// until new activity wakes it again.
    pub(super) fn settle_task(&mut self, session_id: &str, cx: &mut Context<Self>) {
        self.unsettled_tasks.remove(session_id);
        if self.settled_tasks.insert(session_id.to_string()) {
            self.rebuild_sections();
            cx.notify();
            persist_task_sets(self.settled_tasks.clone(), self.unsettled_tasks.clone());
        }
    }

    /// Whether a collapsible section shows its rows.
    fn section_expanded(&self, section: SidebarSection) -> bool {
        match section {
            SidebarSection::Pinned => self.pinned_expanded,
            SidebarSection::Active => self.active_expanded,
            SidebarSection::Settled => self.settled_expanded,
        }
    }

    fn toggle_section_expanded(&mut self, section: SidebarSection, cx: &mut Context<Self>) {
        match section {
            SidebarSection::Pinned => self.pinned_expanded = !self.pinned_expanded,
            SidebarSection::Active => self.active_expanded = !self.active_expanded,
            SidebarSection::Settled => self.settled_expanded = !self.settled_expanded,
        }
        self.rebuild_entries();
        cx.notify();
    }

    /// Move a task back into the active inbox at the top. It stays
    /// active until it is settled away by hand again.
    pub(super) fn unsettle_task(&mut self, session_id: &str, cx: &mut Context<Self>) {
        self.settled_tasks.remove(session_id);
        if self.unsettled_tasks.insert(session_id.to_string()) {
            self.rebuild_sections();
            cx.notify();
            persist_task_sets(self.settled_tasks.clone(), self.unsettled_tasks.clone());
        }
    }

    /// Open or close the project switcher menu.
    pub(super) fn toggle_switcher_menu(&mut self, cx: &mut Context<Self>) {
        self.project_menu = None;
        self.set_switcher_menu_open(!self.switcher_menu_open, cx);
    }

    /// Deterministically open or close the project switcher menu.
    /// Application Vim uses this setter; pointer clicks toggle above.
    pub(super) fn set_switcher_menu_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.menu_selected = None;
        if self.switcher_menu_open == open {
            return;
        }
        self.switcher_menu_open = open;
        if open {
            self.project_menu = None;
        }
        cx.notify();
    }

    /// Scope the sidebar to one project, or show every project again.
    pub(super) fn set_project_filter(&mut self, root: Option<String>, cx: &mut Context<Self>) {
        self.menu_selected = None;
        self.project_filter = root.filter(|root| !root.is_empty());
        self.switcher_menu_open = false;
        self.rebuild_sections();
        cx.notify();
    }

    /// Whether `name` is the display name of `root`, without building the
    /// name the way `root_name` does.
    fn root_name_matches(&self, root: &str, name: &str) -> bool {
        match self
            .project_names
            .get(root)
            .filter(|name| !name.trim().is_empty())
        {
            Some(stored) => stored == name,
            None => match std::path::Path::new(root).file_name() {
                Some(file) => file.to_string_lossy().as_ref() == name,
                None => root == name,
            },
        }
    }

    // ---- Rename ------------------------------------------------------------

    /// Start an inline rename of a task or project in the sidebar.
    pub(super) fn begin_rename(&mut self, target: RenameTarget, cx: &mut Context<Self>) {
        let current = match &target {
            RenameTarget::Task(id) => self
                .sessions
                .iter()
                .find(|session| &session.id == id)
                .map(|session| session.title.clone())
                .unwrap_or_default(),
            RenameTarget::Project(root) => self.root_name(root),
        };
        let chat = self.chat.clone();
        let sidebar = cx.entity().downgrade();
        let application_vim_enabled = self.application_vim_enabled;
        let input = cx.new(move |cx| {
            let mut input = TextInput::new("Name", cx)
                .with_tab_index(0)
                .application_vim(application_vim_enabled);
            input.set_text(&current, cx);
            input
                .on_application_escape(move |window, cx| {
                    if let Some(chat) = chat.upgrade() {
                        chat.update(cx, |chat, cx| chat.focus_application_vim(window, cx));
                    }
                })
                .on_enter(move |_text, _, cx| {
                    let sidebar = sidebar.clone();
                    cx.defer(move |cx| {
                        if let Some(sidebar) = sidebar.upgrade() {
                            sidebar.update(cx, |sidebar, cx| sidebar.commit_rename(cx));
                        }
                    });
                })
        });
        self.project_menu = None;
        self.rename = Some(target);
        self.rename_input = Some(input);
        self.rename_focus_pending = true;
        if let Some(target) = self.rename.as_ref() {
            self.remeasure(target);
        }
        cx.notify();
    }

    /// Drop the rename field. Reports whether one was open.
    pub(super) fn cancel_rename(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(target) = self.rename.take() else {
            return false;
        };
        self.remeasure(&target);
        self.rename_input = None;
        cx.notify();
        true
    }

    pub(super) fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.rename.take() else {
            return;
        };
        let name = self
            .rename_input
            .take()
            .map(|input| input.read(cx).text())
            .unwrap_or_default();
        let name = name.trim().to_string();
        self.remeasure(&target);
        cx.notify();
        if name.is_empty() {
            return;
        }
        match target {
            RenameTarget::Task(session_id) => {
                let backend = self.backend.clone();
                let user_id = self.user_id.clone();
                self.call(
                    async move { backend.rename_session(&user_id, &session_id, name).await },
                    cx,
                    |_this, result, cx| match result {
                        Ok(session) => cx.emit(SidebarEvent::SessionChanged(session)),
                        Err(message) => cx.emit(SidebarEvent::Notice(message.into())),
                    },
                );
            }
            RenameTarget::Project(root) => {
                if name == root_display_name(&root) {
                    self.project_names.remove(&root);
                } else {
                    self.project_names.insert(root.clone(), name);
                }
                self.rebuild_sections();
                let names = self.project_names.clone();
                persist_settings(move |settings| settings.project_names = names);
            }
        }
    }

    // ---- Menus -------------------------------------------------------------

    /// Show the project's folder in the file manager.
    fn open_folder(&mut self, root: &str, cx: &mut Context<Self>) {
        self.project_menu = None;
        let path = root.to_string();
        cx.notify();
        self.call(
            async move {
                tokio::task::spawn_blocking(move || crate::platform::reveal_folder(&path))
                    .await
                    .map_err(|error| format!("Could not open the folder: {error}"))?
            },
            cx,
            |_this, result, cx| {
                if let Err(message) = result {
                    cx.emit(SidebarEvent::Notice(message.into()));
                }
            },
        );
    }

    fn toggle_project_menu(&mut self, root: &str, cx: &mut Context<Self>) {
        self.task_menu = None;
        self.menu_selected = None;
        if self.project_menu.as_deref() == Some(root) {
            self.project_menu = None;
        } else {
            self.project_menu = Some(root.to_string());
            let backend = self.backend.clone();
            let user_id = self.user_id.clone();
            let path = root.to_string();
            self.call(
                async move { backend.project_trust(&user_id, path).await },
                cx,
                |this, result, cx| {
                    if let Ok(status) = result
                        && this.project_menu.as_deref() == Some(status.path.as_str())
                    {
                        this.menu_trust = Some(status);
                        cx.notify();
                    }
                },
            );
        }
        cx.notify();
    }

    /// Open or close the overflow menu of one task row.
    fn toggle_task_menu(&mut self, session_id: &str, cx: &mut Context<Self>) {
        self.menu_selected = None;
        self.menu_python_resettable = false;
        self.python_menu_sequence = self.python_menu_sequence.wrapping_add(1);
        if self.task_menu.as_deref() == Some(session_id) {
            self.task_menu = None;
        } else {
            self.task_menu = Some(session_id.to_string());
            let request = PythonMenuRequest {
                user_id: self.user_id.clone(),
                session_id: session_id.to_string(),
                sequence: self.python_menu_sequence,
            };
            let requested = request.clone();
            let backend = self.backend.clone();
            self.call(
                async move {
                    backend
                        .python_status(&requested.user_id, &requested.session_id)
                        .await
                },
                cx,
                move |this, result, cx| {
                    if !request.is_current(
                        &this.user_id,
                        this.task_menu.as_deref(),
                        this.python_menu_sequence,
                    ) || !this
                        .sessions
                        .iter()
                        .any(|task| task.id == request.session_id)
                    {
                        return;
                    }
                    if let Ok(status) = result
                        && this.menu_python_resettable != status.resettable
                    {
                        this.menu_python_resettable = status.resettable;
                        cx.notify();
                    }
                },
            );
        }
        cx.notify();
    }

    fn reset_python(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.iter().find(|task| task.id == session_id) else {
            return;
        };
        let title = session.title.clone();
        let user_id = self.user_id.clone();
        let task_id = session_id.to_string();
        let requested_user = user_id.clone();
        let requested_task = task_id.clone();
        let backend = self.backend.clone();
        self.task_menu = None;
        self.menu_python_resettable = false;
        cx.notify();
        self.call(
            async move { backend.reset_python(&requested_user, &requested_task).await },
            cx,
            move |this, result, cx| {
                if this.user_id != user_id || !this.sessions.iter().any(|task| task.id == task_id) {
                    return;
                }
                let message = match result {
                    Ok(()) => format!("Python reset for “{title}”. The next call starts fresh."),
                    Err(error) => format!("Could not reset Python: {error}"),
                };
                cx.emit(SidebarEvent::Notice(message.into()));
            },
        );
    }

    /// The overflow menu also offers explicit reclamation of retained Python
    /// state. The backend resolves current authority and state at click time.
    fn task_menu_items(&self, task: SidebarTaskEntry) -> Vec<SidebarMenuItem> {
        let Some(row) = self.rows.get(task.session) else {
            return Vec::new();
        };
        let rename_id = row.id.to_string();
        let pinned_id = row.id.to_string();
        let settle_id = row.id.to_string();
        let archive_id = row.id.to_string();
        let pinned = task.pinned;
        let archived = task.archived;
        let active = !archived && (self.running.contains(&*row.id) || !task.settled);
        let mut items = vec![
            SidebarMenuItem {
                id: row.menu_rename_id.clone(),
                icon: "pencil",
                label: "Rename task",
                on_click: Box::new(move |this, cx| {
                    this.begin_rename(RenameTarget::Task(rename_id.clone()), cx)
                }),
            },
            SidebarMenuItem {
                id: row.menu_pin_id.clone(),
                icon: "pin",
                label: if pinned { "Unpin task" } else { "Pin task" },
                on_click: Box::new(move |this, cx| this.toggle_task_pin(&pinned_id, cx)),
            },
            SidebarMenuItem {
                id: row.menu_settle_id.clone(),
                icon: if active { "check" } else { "undo-2" },
                label: if active {
                    "Settle task"
                } else {
                    "Unsettle task"
                },
                on_click: Box::new(move |this, cx| {
                    if active {
                        this.settle_task(&settle_id, cx)
                    } else {
                        this.unsettle_task(&settle_id, cx)
                    }
                }),
            },
            SidebarMenuItem {
                id: row.menu_archive_id.clone(),
                icon: if archived {
                    "archive-restore"
                } else {
                    "archive"
                },
                label: if archived {
                    "Restore task"
                } else {
                    "Archive task"
                },
                on_click: Box::new(move |this, cx| {
                    this.task_menu = None;
                    cx.emit(SidebarEvent::SetArchived {
                        session_id: archive_id.clone(),
                        archived: !archived,
                    });
                    cx.notify();
                }),
            },
        ];
        if self.menu_python_resettable && self.task_menu.as_deref() == Some(row.id.as_ref()) {
            let python_id = row.id.to_string();
            items.push(SidebarMenuItem {
                id: row.menu_python_reset_id.clone(),
                icon: "undo-2",
                label: "Reset Python",
                on_click: Box::new(move |this, cx| this.reset_python(&python_id, cx)),
            });
        }
        items
    }

    /// The overflow menu of one switcher project row.
    fn project_menu_items(&self, root: &str) -> Vec<SidebarMenuItem> {
        let rename_root = root.to_string();
        let open_root = root.to_string();
        let remove_root = root.to_string();
        let mut items = vec![
            SidebarMenuItem {
                id: SharedString::from(format!("rename-project-{root}")),
                icon: "pencil",
                label: "Rename project",
                on_click: Box::new(move |this, cx| {
                    this.begin_rename(RenameTarget::Project(rename_root.clone()), cx)
                }),
            },
            SidebarMenuItem {
                id: SharedString::from(format!("open-project-{root}")),
                icon: "folder-open",
                label: "Open folder",
                on_click: Box::new(move |this, cx| this.open_folder(&open_root, cx)),
            },
        ];
        if let Some(status) = self
            .menu_trust
            .as_ref()
            .filter(|status| status.available && status.path == root)
        {
            let trusted = status.decision == Some(true);
            let path = status.path.clone();
            items.push(SidebarMenuItem {
                id: SharedString::from(format!("trust-project-{root}")),
                icon: if trusted { "shield-check" } else { "lock" },
                label: if trusted {
                    "Untrust project"
                } else {
                    "Trust project"
                },
                on_click: Box::new(move |this, cx| {
                    this.project_menu = None;
                    cx.emit(SidebarEvent::SetTrust {
                        path: path.clone(),
                        trusted: !trusted,
                    });
                    cx.notify();
                }),
            });
        }
        items.push(SidebarMenuItem {
            id: SharedString::from(format!("remove-project-{root}")),
            icon: "trash-2",
            label: "Remove project",
            on_click: Box::new(move |this, cx| {
                this.project_menu = None;
                cx.emit(SidebarEvent::RemoveRoot(remove_root.clone()));
                cx.notify();
            }),
        });
        items
    }

    /// The popup menu the sidebar shows, if any. The project menu opens
    /// inside the switcher, so it takes priority; the task menu and the
    /// switcher never share the screen with each other.
    fn popup(&self) -> Option<SidebarPopup> {
        if let Some(root) = &self.project_menu {
            return Some(SidebarPopup::Project(root.clone()));
        }
        if let Some(session) = &self.task_menu {
            return Some(SidebarPopup::Task(session.clone()));
        }
        if self.switcher_menu_open {
            return Some(SidebarPopup::Switcher);
        }
        None
    }

    /// The task entry whose overflow menu is open.
    fn task_menu_entry(&self, session_id: &str) -> Option<SidebarTaskEntry> {
        self.entries.iter().find_map(|entry| match entry {
            SidebarEntry::Task(task) => self
                .sessions
                .get(task.session)
                .filter(|session| session.id == session_id)
                .map(|_| *task),
            _ => None,
        })
    }

    fn popup_rows(&self) -> Option<usize> {
        match self.popup() {
            Some(SidebarPopup::Switcher) => Some(self.switcher_roots.len() + 2),
            Some(SidebarPopup::Project(root)) => Some(self.project_menu_items(&root).len()),
            Some(SidebarPopup::Task(session)) => Some(
                self.task_menu_entry(&session)
                    .map(|task| self.task_menu_items(task).len())
                    .unwrap_or(0),
            ),
            None => None,
        }
    }

    /// The Application-Vim row highlight of whichever popup menu is open.
    fn menu_selection(&self) -> Option<usize> {
        (self.application_vim_enabled && self.popup().is_some())
            .then_some(self.menu_selected)
            .flatten()
    }

    /// Move the Application-Vim selection inside the open popup menu.
    /// Reports whether one was open.
    pub(super) fn step_popup(
        &mut self,
        delta: isize,
        count: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(rows) = self.popup_rows().filter(|rows| *rows > 0) else {
            return false;
        };
        for _ in 0..count {
            let next = match self.menu_selected {
                Some(current) => (current as isize + delta).rem_euclid(rows as isize),
                None if delta < 0 => rows as isize - 1,
                None => 0,
            };
            self.menu_selected = Some(next as usize);
        }
        cx.notify();
        true
    }

    /// `gg` and `G` inside the open popup menu. Reports whether one was
    /// open.
    pub(super) fn popup_edge(&mut self, first: bool, cx: &mut Context<Self>) -> bool {
        let Some(rows) = self.popup_rows().filter(|rows| *rows > 0) else {
            return false;
        };
        self.menu_selected = Some(if first { 0 } else { rows - 1 });
        cx.notify();
        true
    }

    /// Enter inside the open popup menu: run the highlighted item's
    /// action. Reports whether one was open.
    pub(super) fn activate_popup(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(popup) = self.popup() else {
            return false;
        };
        let Some(rows) = self.popup_rows().filter(|rows| *rows > 0) else {
            return false;
        };
        let index = match self.menu_selected {
            Some(index) => index.min(rows - 1),
            None => return true,
        };
        match popup {
            SidebarPopup::Switcher => {
                if index == 0 {
                    self.set_project_filter(None, cx);
                } else if let Some(root) = self.switcher_roots.get(index - 1) {
                    let root = root.root.clone();
                    self.set_project_filter(Some(root), cx);
                } else if index == self.switcher_roots.len() + 1 {
                    self.set_switcher_menu_open(false, cx);
                    cx.emit(SidebarEvent::ChooseProject);
                }
            }
            SidebarPopup::Project(root) => {
                if let Some(item) = self.project_menu_items(&root).into_iter().nth(index) {
                    let SidebarMenuItem { on_click, .. } = item;
                    on_click(self, cx);
                }
            }
            SidebarPopup::Task(session) => {
                if let Some(task) = self.task_menu_entry(&session)
                    && let Some(item) = self.task_menu_items(task).into_iter().nth(index)
                {
                    let SidebarMenuItem { on_click, .. } = item;
                    on_click(self, cx);
                }
            }
        }
        true
    }

    // ---- Application Vim ---------------------------------------------------

    fn rebuild_vim_targets(&mut self) {
        self.vim_by_row = self
            .entries
            .iter()
            .map(|entry| match *entry {
                SidebarEntry::NewTask => Some(SidebarTarget::NewTask),
                SidebarEntry::ProjectsHeader => Some(SidebarTarget::Projects),
                SidebarEntry::SectionLabel(_) | SidebarEntry::Empty(_) => None,
                SidebarEntry::Task(task) => self
                    .sessions
                    .get(task.session)
                    .map(|session| SidebarTarget::Task(session.id.clone())),
                SidebarEntry::ArchivedHeader => Some(SidebarTarget::Archived),
            })
            .collect();
    }

    fn vim_targets(&self) -> impl DoubleEndedIterator<Item = (usize, &SidebarTarget)> {
        self.vim_by_row
            .iter()
            .enumerate()
            .filter_map(|(row, target)| target.as_ref().map(|target| (row, target)))
    }

    pub(super) fn vim_target(&self, row: usize) -> Option<&SidebarTarget> {
        self.vim_by_row.get(row).and_then(Option::as_ref)
    }

    fn vim_target_row(&self, selected: &SidebarTarget) -> Option<usize> {
        self.vim_targets()
            .find_map(|(row, target)| (target == selected).then_some(row))
    }

    pub(super) fn vim_selected(&self) -> Option<&SidebarTarget> {
        self.vim_selected.as_ref()
    }

    /// Give Application Vim a row when it has none: the open task, else
    /// the first target.
    pub(super) fn vim_ensure_selection(&mut self) {
        if self
            .vim_selected
            .as_ref()
            .is_some_and(|selected| self.vim_target_row(selected).is_some())
        {
            return;
        }
        let selected_task = self.selected.as_deref().and_then(|task_id| {
            self.vim_targets().find_map(|(_, target)| match target {
                SidebarTarget::Task(candidate) if candidate == task_id => Some(target.clone()),
                _ => None,
            })
        });
        self.vim_selected =
            selected_task.or_else(|| self.vim_targets().next().map(|(_, target)| target.clone()));
    }

    /// Step the selection by `count` targets. Reports whether there was
    /// anything to select.
    pub(super) fn vim_move(
        &mut self,
        direction: isize,
        count: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let targets = &self.vim_order;
        if targets.is_empty() {
            return false;
        }
        let current = self
            .vim_selected
            .as_ref()
            .and_then(|selected| targets.iter().position(|target| target == selected));
        let index = super::navigation::stepped_index(current, targets.len(), direction, count);
        let target = targets[index].clone();
        let row = self.vim_target_row(&target);
        self.vim_selected = Some(target);
        if let Some(row) = row {
            self.list.scroll_to_reveal_item(row);
        }
        cx.notify();
        true
    }

    /// Select the first or last target. Reports whether there was one.
    pub(super) fn vim_edge(&mut self, first: bool, cx: &mut Context<Self>) -> bool {
        let selected = if first {
            self.vim_targets().next()
        } else {
            self.vim_targets().next_back()
        }
        .map(|(row, target)| (row, target.clone()));
        let Some((row, target)) = selected else {
            return false;
        };
        self.vim_selected = Some(target);
        self.list.scroll_to_reveal_item(row);
        cx.notify();
        true
    }

    /// The pointer picked a row while Application Vim is on.
    pub(super) fn vim_select(&mut self, target: SidebarTarget, cx: &mut Context<Self>) {
        self.vim_selected = Some(target);
        cx.notify();
    }

    /// Scroll the selected target into view.
    pub(super) fn vim_reveal(&self) {
        if let Some(row) = self
            .vim_selected
            .as_ref()
            .and_then(|target| self.vim_target_row(target))
        {
            self.list.scroll_to_reveal_item(row);
        }
    }

    /// Fold or unfold the selected target where that means something.
    pub(super) fn vim_set_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        match self.vim_selected.clone() {
            Some(SidebarTarget::Projects) => self.set_switcher_menu_open(expanded, cx),
            Some(SidebarTarget::Archived) => self.set_archived_expanded(expanded, cx),
            _ => {}
        }
        self.vim_reveal();
    }

    fn vim_reconcile(&mut self) {
        if !self.application_vim_enabled {
            return;
        }
        let next = self
            .vim_targets()
            .map(|(_, target)| target.clone())
            .collect::<Vec<_>>();
        self.vim_selected = super::navigation::reconcile_stable_selection(
            self.vim_selected.as_ref(),
            &self.vim_order,
            &next,
        )
        .or_else(|| next.first().cloned());
        self.vim_order = next;
    }

    fn vim_selects_row(&self, row: usize) -> bool {
        self.application_vim_enabled
            && self.vim_active
            && self.vim_target(row) == self.vim_selected.as_ref()
    }

    // ---- Render ------------------------------------------------------------

    /// One row of the sidebar list.
    fn render_entry(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.selected.as_deref();
        let chat = self.chat.clone();
        let entry = match self.entries.get(ix).copied() {
            Some(SidebarEntry::NewTask) => div()
                .id("new-task")
                .role(gpui::Role::Button)
                .aria_label("New task")
                .w_full()
                .mb_3()
                .px_4()
                .py_1p5()
                .rounded(theme::RADIUS_SM)
                .flex()
                .items_center()
                .gap_2()
                .text_sm()
                .text_color(gpui::rgb(theme::accent()))
                .hover(|style| {
                    style
                        .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                        .cursor_pointer()
                })
                .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_row_selected())))
                .tooltip(widgets::tooltip_for_action(
                    "Start a new task",
                    &super::NewTask,
                ))
                .on_click(move |_event, window, cx| {
                    if let Some(chat) = chat.upgrade() {
                        chat.update(cx, |chat, cx| {
                            chat.execute_command(ChatCommand::NewTask, window, cx);
                        });
                    }
                })
                .child(icon("square-pen", px(16.), theme::accent()))
                .child("New Task")
                .into_any_element(),
            Some(SidebarEntry::ProjectsHeader) => self.render_projects_header(cx),
            Some(SidebarEntry::Task(task)) => {
                let application_selected = self.vim_selects_row(ix);
                let row = self.render_task_row(task, selected, application_selected, cx);
                let last_of_section = !task.archived
                    && !matches!(
                        self.entries.get(ix + 1),
                        Some(SidebarEntry::Task(task)) if !task.archived
                    );
                div()
                    .when(last_of_section, |row| row.mb_2())
                    .child(row)
                    .into_any_element()
            }
            Some(SidebarEntry::SectionLabel(section)) => {
                let expanded = self.section_expanded(section);
                div()
                    .id(gpui::SharedString::from(section.element_id()))
                    .mt_4()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .mb_1()
                    .px_4()
                    .py_1()
                    .rounded(theme::RADIUS_SM)
                    .hover(|style| {
                        style
                            .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                            .cursor_pointer()
                    })
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.toggle_section_expanded(section, cx);
                    }))
                    .child(icon(
                        if expanded {
                            "chevron-down"
                        } else {
                            "chevron-right"
                        },
                        px(14.),
                        theme::text_secondary(),
                    ))
                    .child(section_label(section.label()))
                    .into_any_element()
            }
            Some(SidebarEntry::Empty(reason)) => {
                let (title, hint) = match reason {
                    SidebarEmpty::NoTasks => ("No tasks yet", "Start one with ⌘N"),
                    SidebarEmpty::NoMatches => ("Nothing matches", "Try another search"),
                };
                div()
                    .mt_6()
                    .px_4()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .when(reason == SidebarEmpty::NoMatches, |column| {
                        column.child(icon("search", px(18.), theme::text_faint()))
                    })
                    .child(div().mt_1().child(title))
                    .child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(hint),
                    )
                    .when(reason == SidebarEmpty::NoMatches, |column| {
                        column.child(
                            widgets::ghost_button("clear-search-empty")
                                .mt_2()
                                .text_xs()
                                .py_1()
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.clear_search(cx);
                                }))
                                .child("Clear search"),
                        )
                    })
                    .into_any_element()
            }
            Some(SidebarEntry::ArchivedHeader) => {
                let expanded = self.archived_expanded;
                let count = self.archived_rows.len();
                div()
                    .id("archived-toggle")
                    .role(gpui::Role::Button)
                    .aria_label(format!("Archived tasks, {count}"))
                    .aria_expanded(expanded)
                    .mt_5()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .mb_1()
                    .px_4()
                    .py_1()
                    .rounded(theme::RADIUS_SM)
                    .hover(|style| {
                        style
                            .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                            .cursor_pointer()
                    })
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.toggle_archived_visibility(cx);
                    }))
                    .child(icon(
                        if expanded {
                            "chevron-down"
                        } else {
                            "chevron-right"
                        },
                        px(14.),
                        theme::text_secondary(),
                    ))
                    .child(section_label("ARCHIVED"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(count.to_string()),
                    )
                    .into_any_element()
            }
            None => div().into_any_element(),
        };
        if !self.application_vim_enabled {
            return entry;
        }
        let application_selected = self.vim_selects_row(ix);
        let application_target = self.vim_target(ix).cloned();
        let chat = self.chat.clone();
        div()
            .w_full()
            .when_some(application_target, |row, target| {
                row.on_mouse_down(gpui::MouseButton::Left, move |_event, window, cx| {
                    if let Some(chat) = chat.upgrade() {
                        let target = target.clone();
                        chat.update(cx, |chat, cx| {
                            chat.select_sidebar_from_pointer(target, window, cx);
                        });
                    }
                })
            })
            .when(application_selected, |row| {
                row.rounded(theme::RADIUS_SM)
                    .border_l_2()
                    .border_color(gpui::rgb(theme::accent()))
            })
            .child(entry)
            .into_any_element()
    }

    /// The project switcher row: names the scope the task list shows and
    /// opens the menu that picks it. The overflow menu of one project
    /// hangs off its row inside the switcher.
    fn render_projects_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let menu = self
            .switcher_menu_open
            .then(|| self.render_switcher_menu(cx));
        div()
            .relative()
            .w_full()
            .mb_3()
            .child(
                div()
                    .id("projects-header")
                    .role(gpui::Role::Button)
                    .aria_label("Projects")
                    .aria_expanded(self.switcher_menu_open)
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .w_full()
                    .px_4()
                    .py_1()
                    .rounded(theme::RADIUS_SM)
                    .text_sm()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(gpui::rgb(theme::text_primary()))
                    .hover(|style| {
                        style
                            .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                            .cursor_pointer()
                    })
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        cx.stop_propagation();
                        this.toggle_switcher_menu(cx);
                    }))
                    .child(icon("folder", px(16.), theme::text_secondary()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .line_clamp(1)
                            .text_ellipsis()
                            .child(self.scope_label.clone()),
                    )
                    .when(self.switcher_menu_open, |row| {
                        row.child(icon("chevron-down", px(14.), theme::text_secondary()))
                    })
                    .when(!self.switcher_menu_open, |row| {
                        row.child(icon("chevron-right", px(14.), theme::text_secondary()))
                    })
                    .child(
                        div()
                            .id("new-project")
                            .size_6()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(theme::RADIUS_SM)
                            .hover(|style| {
                                style
                                    .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                                    .cursor_pointer()
                            })
                            .on_click(cx.listener(|_this, _event, _window, cx| {
                                cx.stop_propagation();
                                cx.emit(SidebarEvent::ChooseProject);
                            }))
                            .child(icon("folder-plus", px(16.), theme::text_secondary())),
                    ),
            )
            .children(menu)
            .into_any_element()
    }

    /// The menu the project switcher opens: every project, each with an
    /// overflow menu of its own, plus a way back to all projects.
    fn render_switcher_menu(&self, cx: &mut Context<Self>) -> gpui::Deferred {
        let selected = self.menu_selection();
        let mut items = vec![
            div()
                .id("switcher-all-projects")
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1p5()
                .text_sm()
                .text_color(gpui::rgb(theme::text_primary()))
                .when(selected == Some(0), |row| {
                    row.bg(gpui::rgb(theme::bg_input()))
                })
                .hover(|style| {
                    style
                        .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                        .cursor_pointer()
                })
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.set_project_filter(None, cx);
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .line_clamp(1)
                        .text_ellipsis()
                        .child("All projects"),
                )
                .when(self.project_filter.is_none(), |row| {
                    row.child(icon("check", px(14.), theme::accent()))
                }),
        ];
        for (index, root) in self.switcher_roots.iter().enumerate() {
            let is_current = self.project_filter.as_deref() == Some(root.root.as_str());
            let has_menu = self.project_menu.as_deref() == Some(root.root.as_str());
            let rename_field = self.project_rename_field(&root.root);
            let not_renaming = rename_field.is_none();
            items.push(
                div()
                    .id(root.row_id.clone())
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_primary()))
                    .when(
                        selected.is_some_and(|selected| selected == index + 1),
                        |row| row.bg(gpui::rgb(theme::bg_input())),
                    )
                    .hover(|style| {
                        style
                            .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                            .cursor_pointer()
                    })
                    .on_click({
                        let root = root.root.clone();
                        cx.listener(move |this, _event, _window, cx| {
                            let root = root.clone();
                            this.set_project_filter(Some(root), cx);
                        })
                    })
                    .when_some(rename_field, |row, field| row.child(field))
                    .when(not_renaming, |row| {
                        row.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .line_clamp(1)
                                .text_ellipsis()
                                .child(root.name.clone()),
                        )
                    })
                    .when(is_current, |row| {
                        row.child(icon("check", px(14.), theme::accent()))
                    })
                    .child(row_action(
                        root.menu_id.clone(),
                        &root.row_group,
                        "ellipsis",
                        "Project options",
                        {
                            let root = root.root.clone();
                            cx.listener(move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                this.toggle_project_menu(&root, cx);
                            })
                        },
                    ))
                    .children(has_menu.then(|| self.render_project_menu(&root.root.clone(), cx))),
            );
        }
        let new_project_row = self.switcher_roots.len() + 1;
        items.push(
            div()
                .id("switcher-new-project")
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1p5()
                .text_sm()
                .text_color(gpui::rgb(theme::text_secondary()))
                .when(
                    selected.is_some_and(|selected| selected == new_project_row),
                    |row| row.bg(gpui::rgb(theme::bg_input())),
                )
                .hover(|style| {
                    style
                        .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                        .cursor_pointer()
                })
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.set_switcher_menu_open(false, cx);
                    cx.emit(SidebarEvent::ChooseProject);
                }))
                .child(icon("folder-plus", px(14.), theme::text_secondary()))
                .child("New project…"),
        );
        gpui::deferred(motion::fade_in(
            div()
                .id("switcher-menu")
                .absolute()
                .top(px(30.))
                .left_0()
                .min_w(px(220.))
                .max_h(px(360.))
                .overflow_y_scroll()
                .p_1()
                .rounded(theme::RADIUS_SM)
                .bg(gpui::rgb(theme::bg_elevated()))
                .border_1()
                .border_color(gpui::rgb(theme::border()))
                .shadow_md()
                .flex()
                .flex_col()
                .on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
                    this.set_switcher_menu_open(false, cx);
                }))
                .children(items),
            "switcher-menu-reveal",
        ))
    }

    /// Overflow menu for a task row: pin, settle, and archive, beside the
    /// rename the pencil offers.
    fn render_task_menu(&self, task: SidebarTaskEntry, cx: &mut Context<Self>) -> gpui::Deferred {
        let Some(row) = self.rows.get(task.session) else {
            return gpui::deferred(div());
        };
        let selected = self.menu_selection();
        let mut menu = div()
            .id(row.menu_panel_id.clone())
            .absolute()
            .top(px(30.))
            .right_0()
            .w(px(180.))
            .py_1()
            .rounded(theme::RADIUS_SM)
            .bg(gpui::rgb(theme::bg_elevated()))
            .border_1()
            .border_color(gpui::rgb(theme::border()))
            .shadow_md()
            .flex()
            .flex_col()
            .on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
                this.task_menu = None;
                cx.notify();
            }));
        for (index, item) in self.task_menu_items(task).into_iter().enumerate() {
            menu = menu.child(popup_menu_row(item, selected == Some(index), cx));
        }
        gpui::deferred(motion::fade_in(menu, "sidebar-menu-reveal"))
    }

    /// Overflow menu for a project row.
    fn render_project_menu(&self, root: &str, cx: &mut Context<Self>) -> gpui::Deferred {
        let selected = self.menu_selection();
        let mut menu = div()
            .id(SharedString::from(format!("project-menu-{root}")))
            .absolute()
            .top(px(30.))
            .right_0()
            .w(px(180.))
            .py_1()
            .rounded(theme::RADIUS_SM)
            .bg(gpui::rgb(theme::bg_elevated()))
            .border_1()
            .border_color(gpui::rgb(theme::border()))
            .shadow_md()
            .flex()
            .flex_col()
            .on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
                this.project_menu = None;
                cx.notify();
            }));
        for (index, item) in self.project_menu_items(root).into_iter().enumerate() {
            menu = menu.child(popup_menu_row(item, selected == Some(index), cx));
        }
        gpui::deferred(motion::fade_in(menu, "sidebar-menu-reveal"))
    }

    /// The rename field for the task being edited, if it is this one.
    /// Renders per visible row per frame, so it compares borrowed ids
    /// instead of building a `RenameTarget` to match against.
    fn task_rename_field(&self, session_id: &str) -> Option<gpui::Stateful<Div>> {
        match self.rename.as_ref() {
            Some(RenameTarget::Task(target)) if target == session_id => self.rename_field(),
            _ => None,
        }
    }

    /// The rename field for the project being edited, if it is this one.
    fn project_rename_field(&self, root: &str) -> Option<gpui::Stateful<Div>> {
        match self.rename.as_ref() {
            Some(RenameTarget::Project(target)) if target == root => self.rename_field(),
            _ => None,
        }
    }

    /// The shared rename input, wrapped for a row.
    fn rename_field(&self) -> Option<gpui::Stateful<Div>> {
        let input = self.rename_input.clone()?;
        Some(
            div()
                .id("rename-field")
                .flex_1()
                .min_w_0()
                .on_click(|_event, _window, cx| cx.stop_propagation())
                .child(input),
        )
    }

    /// One task row: title, its project, and hover actions. Archived rows
    /// show a restore button; live rows show an archive button on hover.
    fn render_task_row(
        &self,
        task: SidebarTaskEntry,
        selected: Option<&str>,
        application_selected: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let SidebarTaskEntry {
            session: index,
            pinned: is_pinned,
            settled,
            archived,
        } = task;
        let row = &self.rows[index];
        let is_selected = selected == Some(&*row.id);
        let activity = (!archived)
            .then(|| self.session_activity(&row.id))
            .flatten();
        let session_id = Arc::clone(&row.id);
        let action_id = Arc::clone(&row.id);
        let rename_id = Arc::clone(&row.id);
        let menu_id = Arc::clone(&row.id);
        let pin_id = row.pin_id.clone();
        let rename_field = self.task_rename_field(&row.id);
        let renaming = rename_field.is_some();
        let task_menu_open = self.task_menu.as_deref() == Some(&*row.id);
        div()
            .relative()
            .w_full()
            .id(row.element_id.clone())
            .role(gpui::Role::ListBoxOption)
            .accessibility_id(row.id.to_string())
            .aria_label(row.title.clone())
            .aria_description(row.project_name.clone())
            .aria_selected(is_selected)
            .when(application_selected, |row| row.aria_active_descendant())
            .group(row.group.clone())
            .flex()
            .items_center()
            .gap_1p5()
            .when(archived, |row| row.pl_6())
            .when(!archived, |row| row.pl_8())
            .pr_6()
            .py_1()
            .rounded(theme::RADIUS_MD)
            .text_sm()
            .when(is_selected, |row| {
                row.bg(gpui::rgb(theme::bg_sidebar_row_selected()))
                    .font_weight(gpui::FontWeight::MEDIUM)
            })
            .text_color(gpui::rgb(theme::text_primary()))
            .hover(|style| {
                style
                    .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                    .cursor_pointer()
            })
            .on_click(cx.listener(move |_this, _event, _window, cx| {
                cx.emit(SidebarEvent::Select(session_id.to_string()));
            }))
            .when_some(rename_field, |row, field| row.child(field))
            .when(!renaming, |el| {
                el.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().line_clamp(1).text_ellipsis().child(row.title.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(gpui::rgb(if is_selected {
                                    theme::text_secondary()
                                } else {
                                    theme::text_muted()
                                }))
                                .line_clamp(1)
                                .text_ellipsis()
                                .child(row.project_name.clone()),
                        ),
                )
            })
            .when_some(activity, |row_element, activity| {
                row_element.child(activity_indicator(&row.spinner_id, activity))
            })
            .when(is_pinned, |row| {
                row.child(
                    div()
                        .id(pin_id.clone())
                        .size_5()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(theme::RADIUS_SM)
                        .hover(|style| {
                            style
                                .bg(gpui::rgb(theme::bg_sidebar_row_selected()))
                                .cursor_pointer()
                        })
                        .tooltip(widgets::tooltip("Unpin", None))
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            cx.stop_propagation();
                            this.toggle_task_pin(&pin_id, cx);
                        }))
                        .child(icon("pin", px(13.), theme::accent())),
                )
            })
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .when(is_pinned, |overlay| overlay.right(px(26.)))
                    .when(!is_pinned, |overlay| {
                        overlay.right_0().rounded_r(theme::RADIUS_MD)
                    })
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .pl_2()
                    .pr_6()
                    .group_hover(row.group.clone(), |style| {
                        style.bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                    })
                    .child(row_action(
                        row.menu_id.clone(),
                        &row.group,
                        "ellipsis",
                        "More",
                        cx.listener(move |this, _event, _window, cx| {
                            cx.stop_propagation();
                            this.toggle_task_menu(menu_id.as_ref(), cx);
                        }),
                    ))
                    .child(row_action(
                        row.rename_id.clone(),
                        &row.group,
                        "pencil",
                        "Rename",
                        cx.listener(move |this, _event, _window, cx| {
                            cx.stop_propagation();
                            this.begin_rename(RenameTarget::Task(rename_id.to_string()), cx);
                        }),
                    ))
                    .child(row_action(
                        row.archive_id.clone(),
                        &row.group,
                        if settled { "undo-2" } else { "check" },
                        if settled { "Reopen" } else { "Settle" },
                        cx.listener(move |this, _event, _window, cx| {
                            cx.stop_propagation();
                            if settled {
                                this.unsettle_task(&action_id, cx);
                            } else {
                                this.settle_task(&action_id, cx);
                            }
                        }),
                    )),
            )
            .children(task_menu_open.then(|| self.render_task_menu(task, cx)))
    }

    /// The collapse button in the top row.
    fn render_toggle(&self) -> gpui::Stateful<Div> {
        let chat = self.chat.clone();
        div()
            .id("sidebar-toggle")
            .flex_none()
            .size_7()
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::RADIUS_SM)
            .hover(|style| {
                style
                    .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                    .cursor_pointer()
            })
            .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_row_selected())))
            // Inside a drag region: a press here is a click, not a drag.
            .on_mouse_down(gpui::MouseButton::Left, |_event, _window, cx| {
                cx.stop_propagation();
            })
            .tooltip(widgets::tooltip_for_action(
                "Toggle sidebar",
                &super::ToggleSidebar,
            ))
            .on_click(move |_event, window, cx| {
                if let Some(chat) = chat.upgrade() {
                    chat.update(cx, |chat, cx| {
                        chat.execute_command(ChatCommand::ToggleSidebar, window, cx);
                    });
                }
            })
            .child(icon("panel-left", px(16.), theme::text_secondary()))
    }

    /// Sidebar footer: just the settings gear.
    fn render_footer(&self) -> Div {
        let chat = self.chat.clone();
        let gear = div()
            .id("open-settings")
            .flex_none()
            .size_8()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .hover(|style| {
                style
                    .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                    .cursor_pointer()
            })
            .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_row_selected())))
            .tooltip(widgets::tooltip_for_action(
                "Settings",
                &super::OpenAppSettings,
            ))
            .on_click(move |_event, window, cx| {
                if let Some(chat) = chat.upgrade() {
                    chat.update(cx, |chat, cx| {
                        chat.execute_command(ChatCommand::OpenSettings, window, cx);
                    });
                }
            })
            .child(icon("settings", px(16.), theme::text_secondary()));
        div().flex().items_center().px_3().py_2().child(gear)
    }
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.rename_focus_pending {
            self.rename_focus_pending = false;
            if let Some(input) = self.rename_input.clone() {
                let handle = input.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
        }
        let entity = cx.entity().downgrade();
        let list = gpui::list(self.list.clone(), move |ix, _window, cx| {
            let Some(sidebar) = entity.upgrade() else {
                return div().into_any_element();
            };
            sidebar.update(cx, |sidebar, cx| sidebar.render_entry(ix, cx))
        })
        .size_full();
        let active = !self.filter.is_empty();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(gpui::rgb(theme::bg_sidebar()))
            .child(titlebar::drag_region(
                div()
                    .id("sidebar-top-row")
                    .flex()
                    .items_center()
                    .justify_between()
                    .pl_4()
                    .pr_3()
                    // The wordmark row sits under the traffic lights, not
                    // beside them; the space above it is still the bar.
                    .pt(titlebar::top_row_top(px(12.)))
                    .pb_2()
                    .child(wordmark(px(16.), theme::text_primary()))
                    .child(self.render_toggle()),
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .mx_4()
                    .mt_3()
                    .px_2()
                    .py_1()
                    .rounded(theme::RADIUS_SM)
                    .bg(gpui::rgb(theme::bg_sidebar_pill()))
                    .border_1()
                    .border_color(gpui::rgb(if active {
                        theme::accent()
                    } else {
                        theme::border_subtle()
                    }))
                    .text_sm()
                    .child(icon("search", px(14.), theme::text_muted()))
                    .child(div().flex_1().min_w_0().child(self.search_input.clone()))
                    .when(active, |row| {
                        row.child(
                            div()
                                .id("search-clear")
                                .size_5()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(theme::RADIUS_SM)
                                .hover(|style| {
                                    style
                                        .bg(gpui::rgb(theme::bg_sidebar_row_selected()))
                                        .cursor_pointer()
                                })
                                .tooltip(widgets::tooltip_for_action(
                                    "Clear search",
                                    &super::ChatEscape,
                                ))
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.clear_search(cx);
                                }))
                                .child(icon("x", px(12.), theme::text_secondary())),
                        )
                    }),
            )
            .child(
                div()
                    .id("session-list")
                    .role(gpui::Role::ListBox)
                    .aria_label("Tasks")
                    .aria_orientation(gpui::Orientation::Vertical)
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .pt_6()
                    .child(list)
                    .child(crate::ui::scrollbar::scrollbar(
                        "sidebar-scrollbar",
                        self.list.clone(),
                    )),
            )
            .child(self.render_footer())
    }
}

/// Apply `update` to the settings file off the UI thread.
fn persist_settings(update: impl FnOnce(&mut crate::settings::AppSettings) + Send + 'static) {
    crate::settings::update_settings_in_background(update);
}

/// Write the settle/unsettle sets in one background update.
fn persist_task_sets(settled: HashSet<String>, unsettled: HashSet<String>) {
    crate::settings::update_settings_in_background(move |settings| {
        settings.settled_tasks = settled.into_iter().collect();
        settings.unsettled_tasks = unsettled.into_iter().collect();
    });
}

/// A spinner while the task runs, a dot once it completed unseen. The
/// spinner keeps the sidebar repainting while any task runs; `spinner_id`
/// is prebuilt so render allocates nothing.
fn activity_indicator(spinner_id: &SharedString, activity: SessionActivity) -> AnyElement {
    match activity {
        SessionActivity::Running => spinner_with_id(spinner_id.clone(), px(13.), theme::accent()),
        SessionActivity::CompletedUnread => div()
            .size(px(8.))
            .flex_none()
            .rounded_full()
            .bg(gpui::rgb(theme::status_success()))
            .into_any_element(),
    }
}

/// Small icon button that shows only while the pointer is over its row.
pub(super) fn row_action(
    id: SharedString,
    group: &SharedString,
    icon_name: &'static str,
    label: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label)
        .flex_none()
        .size_5()
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::RADIUS_SM)
        .opacity(0.)
        .group_hover(group.clone(), |style| style.opacity(1.))
        .hover(|style| {
            style
                .bg(gpui::rgb(theme::bg_sidebar_row_selected()))
                .cursor_pointer()
        })
        .active(|style| style.bg(gpui::rgb(theme::border())))
        .tooltip(widgets::tooltip(label, None))
        .on_click(on_click)
        .child(icon(icon_name, px(14.), theme::text_secondary()))
}

/// One row of a sidebar popup menu: icon, label, and the hover and
/// Application-Vim backgrounds.
fn popup_menu_row(
    item: SidebarMenuItem,
    selected: bool,
    cx: &mut Context<Sidebar>,
) -> gpui::Stateful<Div> {
    div()
        .id(item.id)
        .role(gpui::Role::MenuItem)
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_1p5()
        .text_sm()
        .text_color(gpui::rgb(theme::text_primary()))
        .when(selected, |row| row.bg(gpui::rgb(theme::bg_input())))
        .hover(|style| {
            style
                .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                .cursor_pointer()
        })
        .on_click(cx.listener(move |this, _event, _window, cx| {
            cx.stop_propagation();
            (item.on_click)(this, cx);
        }))
        .child(icon(item.icon, px(14.), theme::text_secondary()))
        .child(item.label)
}

/// Field-wise equality for session rows; the summary type has no
/// `PartialEq` of its own.
pub(super) fn session_summary_eq(a: &AgentSessionSummary, b: &AgentSessionSummary) -> bool {
    a.id == b.id
        && a.title == b.title
        && a.project_root == b.project_root
        && a.created_ms == b.created_ms
        && a.updated_ms == b.updated_ms
        && a.message_count == b.message_count
        && a.model == b.model
        && a.mode == b.mode
        && a.web_enabled == b.web_enabled
        && a.archived == b.archived
}

pub(super) fn root_display_name(root: &str) -> String {
    std::path::Path::new(root)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| root.to_string())
}

#[cfg(test)]
mod python_menu_tests {
    use super::*;

    #[test]
    fn status_reply_requires_the_same_account_task_and_menu_opening() {
        let request = PythonMenuRequest {
            user_id: "account-a".into(),
            session_id: "task-a".into(),
            sequence: 7,
        };
        assert!(request.is_current("account-a", Some("task-a"), 7));
        assert!(!request.is_current("account-b", Some("task-a"), 7));
        assert!(!request.is_current("account-a", Some("task-b"), 7));
        assert!(!request.is_current("account-a", None, 7));
        // Closing and reopening the same task must reject the earlier read.
        assert!(!request.is_current("account-a", Some("task-a"), 9));
    }
}
