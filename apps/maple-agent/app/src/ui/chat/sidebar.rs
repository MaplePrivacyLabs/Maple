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

#[path = "sidebar_computers.rs"]
mod computers;
#[path = "sidebar_paging.rs"]
mod paging;
use super::computers::{Computer, Destination, Navigate, ProjectRenameRequest, ProjectTarget};
use computers::{ComputerPages, OtherTask, ProjectHeading};
use paging::{PageKind, SidebarPages};

pub(super) fn project_from_summary(
    project: maple_agent::store::ProjectSummary,
) -> RecentProjectRoot {
    RecentProjectRoot {
        name: project
            .display_name
            .unwrap_or_else(|| remote_root_display_name(&project.root)),
        path: project.root,
        last_used_ms: u128::try_from(project.last_used_ms).unwrap_or_default(),
        is_default_workspace: project.is_default_workspace,
        name_revision: project.name_revision,
    }
}

pub(super) fn bootstrap_projects(
    projects: Vec<maple_agent::store::ProjectSummary>,
    default_project: Option<maple_agent::store::ProjectSummary>,
) -> Vec<RecentProjectRoot> {
    let mut roots = projects
        .into_iter()
        .map(project_from_summary)
        .collect::<Vec<_>>();
    if let Some(default_project) = default_project
        && !roots.iter().any(|root| root.path == default_project.root)
    {
        roots.push(project_from_summary(default_project));
    }
    roots
}

/// Browsing belongs to the desktop window; conversations belong to computers.
/// Carry this small view state when swapping the per-computer chat screens.
pub(crate) struct BrowseState {
    search: String,
    archived_expanded: bool,
    scroll: gpui::ListOffset,
    computers: computers::ComputerBrowseState,
}

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{
    AnyElement, AppContext, Context, Div, Entity, EventEmitter, Focusable, SharedString,
    Subscription, Task, WeakEntity, Window, div, prelude::*, px,
};
use maple_agent::agent::{
    AgentProjectTrustStatus, AgentSessionSummary, AgentTaskState, RecentProjectRoot,
};

use super::commands::ChatCommand;
use super::navigation::SidebarTarget;
use super::{ChatScreen, section_label};
use crate::backend::AgentBackend;
use crate::ui::icons::{icon, spinner_with_id, wordmark};
use crate::ui::popup::{Menu, MenuItem, Placement, Popup};
use crate::ui::text_input::TextInput;
use crate::ui::theme;
use crate::ui::titlebar;
use crate::ui::widgets;

pub(super) fn session_from_summary(
    summary: &maple_agent::store::TaskSummary,
) -> AgentSessionSummary {
    AgentSessionSummary {
        id: summary.uuid.clone(),
        title: summary.title.clone(),
        project_root: summary.project_root.clone(),
        created_ms: summary.created_ms,
        updated_ms: summary.updated_ms,
        message_count: usize::try_from(summary.message_count).unwrap_or(usize::MAX),
        model: summary.model.clone(),
        web_enabled: summary.web_enabled,
        state: match summary.state.as_str() {
            "settled" => AgentTaskState::Settled,
            "archived" => AgentTaskState::Archived,
            _ => AgentTaskState::Active,
        },
        acp: summary.kind == "acp",
    }
}

/// What the sidebar asks the screen to do.
#[derive(Clone, Debug)]
pub(super) enum SidebarEvent {
    PagesReady {
        sessions: Vec<AgentSessionSummary>,
        generation: u64,
    },
    ProjectsReady {
        roots: Vec<RecentProjectRoot>,
    },
    Navigate(Navigate),
    RenameProject(ProjectRenameRequest),
    PreferencesChanged,
    ProjectsChanged(Vec<RecentProjectRoot>),
    /// Show a task.
    Select(String),
    /// A session changed on the backend (a rename); the screen owns the
    /// canonical list and pushes it back.
    SessionChanged(AgentSessionSummary),
    /// Move a task between active, settled, and archived. The runtime
    /// owns the state; its updated record moves the row.
    SetState {
        session_id: String,
        state: AgentTaskState,
    },
    /// Delete a task for good; the screen confirms first.
    DeleteTask(String),
    /// Set or clear a task's unread marker, as if a run had just ended
    /// unseen or the task had been opened.
    SetUnread {
        session_id: String,
        unread: bool,
    },
    /// Remove a project; the screen confirms first.
    RemoveRoot(String),
    /// Trust or untrust a project.
    SetTrust {
        path: String,
        trusted: bool,
    },
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
    Project(ProjectTarget),
}

/// Strings and element ids one sidebar task row shows, built when the
/// session list changes instead of on every frame.
pub(super) struct SidebarRow {
    pub(super) id: Arc<str>,
    pub(super) element_id: SharedString,
    /// Hover group that reveals the row's menu button.
    pub(super) group: SharedString,
    /// Animation id of the running indicator.
    pub(super) spinner_id: SharedString,
    pub(super) pin_id: SharedString,
    pub(super) menu_id: SharedString,
    /// Id of the overflow-menu panel itself.
    pub(super) menu_panel_id: SharedString,
    pub(super) menu_rename_id: SharedString,
    pub(super) menu_pin_id: SharedString,
    pub(super) menu_unread_id: SharedString,
    pub(super) menu_folder_id: SharedString,
    pub(super) menu_settle_id: SharedString,
    pub(super) menu_archive_id: SharedString,
    pub(super) menu_delete_id: SharedString,
    pub(super) menu_reopen_id: SharedString,
    menu_actions: Vec<TaskMenuAction>,
    project_root: Arc<str>,
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
            spinner_id: SharedString::from(format!("spinner-session-{id}")),
            pin_id: SharedString::from(format!("pin-session-{id}")),
            menu_id: SharedString::from(format!("menu-session-{id}")),
            menu_panel_id: SharedString::from(format!("task-menu-{id}")),
            menu_rename_id: SharedString::from(format!("rename-task-{id}")),
            menu_pin_id: SharedString::from(format!("pin-task-{id}")),
            menu_unread_id: SharedString::from(format!("unread-task-{id}")),
            menu_folder_id: SharedString::from(format!("open-folder-task-{id}")),
            menu_settle_id: SharedString::from(format!("settle-task-{id}")),
            menu_archive_id: SharedString::from(format!("archive-task-{id}")),
            menu_delete_id: SharedString::from(format!("delete-task-{id}")),
            menu_reopen_id: SharedString::from(format!("reopen-task-{id}")),
            menu_actions: Vec::new(),
            project_root: Arc::from(session.project_root.as_str()),
            title: SharedString::from(session.title.clone()),
            project_name: SharedString::from(project_name.to_string()),
            search: session.title.to_lowercase(),
        }
    }
}

#[derive(Clone, Copy)]
enum TaskMenuAction {
    Rename,
    Pin,
    Unread,
    Folder,
    Move(TaskMove),
    Separator,
}

/// The switcher's "All projects" row.
const ALL_PROJECTS_ID: &str = "switcher-all-projects";

/// The sidebar's popup menus. One is open at a time; a project's own menu
/// opens inside the switcher (see `Sidebar::project_popup`).
#[derive(Clone, Debug, PartialEq, Eq)]
enum SidebarPopup {
    Switcher,
    /// A task row's menu, by the row's shared id. It opens under the row's
    /// "…" button, or at the pointer on a right-click.
    Task(Arc<str>),
}

/// One row of the virtualized sidebar list, in display order. Rebuilt
/// with the sections and when a section folds; the list builds only the
/// rows on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SidebarEntry {
    ComputerProject(usize),
    OtherTask(usize),
    Computer(usize),
    ComputerMore(usize),
    LoadMore(PageKind),
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

/// Where a task sits on the ladder active, settled, archived, deleted.
/// The task menu reaches every later rung and comes back only to active.
/// The pinned section is a view of active tasks, not a rung of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TaskRung {
    Active,
    Settled,
    Archived,
}

/// Where a menu item sends a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TaskMove {
    Active,
    Settled,
    Archived,
    Deleted,
}

impl TaskRung {
    pub(super) fn of(task: SidebarTaskEntry) -> Self {
        if task.archived {
            Self::Archived
        } else if task.settled {
            Self::Settled
        } else {
            Self::Active
        }
    }
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

/// One action of a sidebar menu, as the task and project menus list
/// them.
struct SidebarMenuItem {
    id: SharedString,
    icon: &'static str,
    label: &'static str,
    /// The letter that picks the item while its menu is open.
    shortcut: Option<&'static str>,
    /// Shown in the error color (Delete).
    destructive: bool,
    on_click: MenuAction,
}

/// One project the switcher menu lists: the root path, its display name,
/// and the element ids its row and menus use. Built when the sections
/// rebuild, never per frame.
pub(super) struct SwitcherRoot {
    target: ProjectTarget,
    root: String,
    name: SharedString,
    row_id: SharedString,
    row_group: SharedString,
    menu_id: SharedString,
}

pub(super) struct Sidebar {
    pages: SidebarPages,
    group_pages: ComputerPages,
    computers: Arc<Vec<Computer>>,
    computer_projects: Vec<ProjectHeading>,
    other_tasks: Vec<OtherTask>,
    computer_archived_count: usize,
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
    computer_switcher_roots: Vec<Vec<SwitcherRoot>>,
    scope_label: SharedString,
    project_filter: Option<String>,
    // Menus.
    popup: Popup<Sidebar, SidebarPopup>,
    /// Where a right-click opened the task menu; `None` hangs it under the
    /// row's "…" button.
    task_menu_at: Option<gpui::Point<gpui::Pixels>>,
    /// A project's overflow menu, keyed by execution computer and path.
    project_popup: Popup<Sidebar, ProjectTarget>,
    /// Trust status of the project whose menu is open, once it loads.
    menu_trust: Option<AgentProjectTrustStatus>,
    // Search and rename.
    filter: String,
    search_input: Entity<TextInput>,
    rename: Option<RenameTarget>,
    rename_input: Option<Entity<TextInput>>,
    rename_focus_pending: bool,
    /// Commits a task's rename when its field loses focus, the way a
    /// click anywhere else ends one in a native list.
    rename_focus_out: Option<Subscription>,
    /// Task renames sent to the backend, for tests.
    #[cfg(test)]
    renames_sent: Vec<(String, String)>,
    // Persisted in the app settings.
    pinned_tasks: Vec<String>,
    /// Latest owner metadata seen per path, independent of current membership.
    /// A late list can omit a newly restored project without erasing its revision.
    /// This cache is scoped to this account/computer view, never UI settings.
    project_metadata: HashMap<String, RecentProjectRoot>,
    /// Owner labels returned with loaded task pages, before that project's
    /// revisioned metadata page has been requested. Never a settings override.
    task_project_labels: HashMap<String, String>,
    /// Explicit removals survive page refreshes until an owner reply lists or
    /// selects that project again. Cached task rows cannot recreate membership.
    removed_projects: HashSet<String>,
    pending_project_names: HashMap<ProjectTarget, String>,
    // Application Vim's view of the rows.
    vim_selected: Option<SidebarTarget>,
    vim_by_row: Vec<Option<SidebarTarget>>,
    vim_order: Vec<SidebarTarget>,
    /// Tasks bridging backend futures; dropped on this thread.
    bridged_tasks: RefCell<Vec<Task<()>>>,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl ProjectTarget {
    fn element_id(&self, prefix: &str) -> SharedString {
        match &self.computer {
            Some(computer) => format!("{prefix}-computer-{computer}-{}", self.path).into(),
            None => format!("{prefix}-{}", self.path).into(),
        }
    }
}

impl SwitcherRoot {
    fn new(target: ProjectTarget, name: SharedString) -> Self {
        Self {
            root: target.path.clone(),
            row_id: target.element_id("switcher"),
            row_group: target.element_id("switcher-project-row"),
            menu_id: target.element_id("menu-project"),
            target,
            name,
        }
    }
}

impl Sidebar {
    fn project_target(&self, path: &str) -> ProjectTarget {
        ProjectTarget {
            computer: self.backend.remote_host().map(str::to_owned),
            path: path.to_owned(),
        }
    }

    fn project_name(&self, target: &ProjectTarget) -> String {
        if target.computer.as_deref() == self.backend.remote_host() {
            return self.root_name(&target.path);
        }
        self.pending_project_names
            .get(target)
            .cloned()
            .or_else(|| {
                self.computers
                    .iter()
                    .find(|computer| computer.key == target.computer)
                    .and_then(|computer| {
                        computer
                            .projects
                            .iter()
                            .find(|project| project.path == target.path)
                    })
                    .map(|project| project.label.to_string())
            })
            .unwrap_or_else(|| remote_root_display_name(&target.path))
    }

    fn rebuild_computer_switcher_roots(&mut self) {
        self.computer_switcher_roots = self
            .computers
            .iter()
            .map(|computer| {
                if let Some(projects) = self.group_pages.project_summaries(&computer.key) {
                    projects
                        .iter()
                        .map(|project| {
                            let target = ProjectTarget {
                                computer: computer.key.clone(),
                                path: project.root.clone(),
                            };
                            SwitcherRoot::new(
                                target,
                                self.computer_project_name(computer, project).into(),
                            )
                        })
                        .collect()
                } else {
                    computer
                        .projects
                        .iter()
                        .map(|project| {
                            let target = ProjectTarget {
                                computer: computer.key.clone(),
                                path: project.path.clone(),
                            };
                            let name = self.project_name(&target).into();
                            SwitcherRoot::new(target, name)
                        })
                        .collect()
                }
            })
            .collect();
    }

    pub(super) fn finish_project_rename(
        &mut self,
        target: ProjectTarget,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        self.pending_project_names.remove(&target);
        if let Err(message) = result {
            cx.emit(SidebarEvent::Notice(message.into()));
        }
        self.rebuild_sections();
        cx.emit(SidebarEvent::PreferencesChanged);
        cx.notify();
    }

    pub(crate) fn browse_state(&self, cx: &gpui::App) -> BrowseState {
        BrowseState {
            search: self.search_input.read(cx).text(),
            archived_expanded: self.archived_expanded,
            scroll: self.list.logical_scroll_top(),
            computers: self.computer_browse_state(),
        }
    }

    pub(crate) fn restore_browse_state(&mut self, state: BrowseState, cx: &mut Context<Self>) {
        if state.computers.user_id != self.user_id {
            return;
        }
        let filter = state.search.trim().to_lowercase();
        let scope_changed = self.filter != filter
            || self.archived_expanded != state.archived_expanded
            || self.project_filter.is_some();
        self.search_input
            .update(cx, |input, cx| input.set_text(&state.search, cx));
        self.filter = filter;
        self.archived_expanded = state.archived_expanded;
        // A project filter belongs to the previous execution computer.
        self.project_filter = None;
        if self.pages.enabled && scope_changed {
            self.refresh_pages(cx);
        }
        self.restore_computer_browse_state(state.computers, cx);
        self.rebuild_sections();
        self.list.scroll_to(state.scroll);
        cx.notify();
    }

    pub(super) fn new(
        backend: Arc<AgentBackend>,
        user_id: String,
        chat: WeakEntity<ChatScreen>,
        settings: &crate::settings::AppSettings,
        cx: &mut Context<Self>,
    ) -> Self {
        let remote_settings = backend.remote_host().map(|host| {
            settings
                .remote_sidebars
                .get(host)
                .cloned()
                .unwrap_or_default()
        });
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
        let list = gpui::ListState::new(0, gpui::ListAlignment::Top, px(200.));
        // A menu hangs off its row: scrolling the list closes it, so it
        // never drifts from its row or leaves hover state behind.
        let sidebar = cx.weak_entity();
        list.set_scroll_handler(move |_, _, cx| {
            sidebar
                .update(cx, |sidebar, cx| {
                    sidebar.close_popups(cx);
                })
                .ok();
        });
        Self {
            pages: SidebarPages::default(),
            group_pages: ComputerPages::default(),
            computers: Arc::new(Vec::new()),
            computer_projects: Vec::new(),
            other_tasks: Vec::new(),
            computer_archived_count: 0,
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
            list,
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
            computer_switcher_roots: Vec::new(),
            scope_label: "All projects".into(),
            project_filter: None,
            popup: Popup::new(|this| &mut this.popup, cx),
            task_menu_at: None,
            project_popup: Popup::new(|this| &mut this.project_popup, cx),
            menu_trust: None,
            filter: String::new(),
            search_input,
            rename: None,
            rename_input: None,
            rename_focus_pending: false,
            rename_focus_out: None,
            #[cfg(test)]
            renames_sent: Vec::new(),
            pinned_tasks: remote_settings
                .as_ref()
                .map_or_else(|| settings.pinned_tasks.clone(), |s| s.pinned_tasks.clone()),
            project_metadata: HashMap::new(),
            task_project_labels: HashMap::new(),
            removed_projects: HashSet::new(),
            pending_project_names: HashMap::new(),
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
        if self.pages.enabled {
            // The screen supplies current/live summaries. Keep the bounded
            // pages as the navigation source and reconcile events by UUID.
            self.merge_live_sessions(sessions, cx);
            return;
        }
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

    pub(super) fn set_recent_roots(
        &mut self,
        mut roots: Vec<RecentProjectRoot>,
        cx: &mut Context<Self>,
    ) {
        if self.pages.enabled {
            self.merge_project_metadata(roots, cx);
            return;
        }
        self.preserve_newer_project_names(&mut roots);
        let paths = roots
            .iter()
            .map(|root| root.path.clone())
            .collect::<Vec<_>>();
        if self.recent_roots == paths
            && roots
                .iter()
                .all(|root| self.project_metadata.get(&root.path) == Some(root))
        {
            return;
        }
        self.recent_roots = paths;
        self.project_metadata
            .extend(roots.into_iter().map(|root| (root.path.clone(), root)));
        self.rebuild_sections();
        cx.notify();
    }

    pub(super) fn preserve_newer_project_names(&self, roots: &mut [RecentProjectRoot]) {
        for root in roots {
            if let Some(current) = self.project_metadata.get(&root.path)
                && current.name_revision > root.name_revision
            {
                root.name.clone_from(&current.name);
                root.name_revision = current.name_revision;
            }
        }
    }

    /// A successful reconnect reauthorizes only the bounded owner snapshot.
    /// Unloaded cached labels may have changed while events were unavailable;
    /// fresh task pages will supply them again. Buffered pushes replay afterward.
    pub(super) fn prepare_project_cache_for_recovery(&mut self, roots: &[RecentProjectRoot]) {
        let listed: HashSet<&str> = roots.iter().map(|root| root.path.as_str()).collect();
        // Keep the old anchor long enough for reconcile_default_project to
        // explicitly purge it when the separate owner lookup reported None.
        self.project_metadata
            .retain(|path, root| root.is_default_workspace || listed.contains(path.as_str()));
        self.task_project_labels.clear();
    }

    /// Merge owner-issued point updates without replacing paged membership.
    pub(super) fn merge_project_metadata(
        &mut self,
        mut roots: Vec<RecentProjectRoot>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.preserve_newer_project_names(&mut roots);
        let changed = roots
            .iter()
            .any(|root| self.project_metadata.get(&root.path) != Some(root));
        if !changed {
            return false;
        }
        self.project_metadata
            .extend(roots.into_iter().map(|root| (root.path.clone(), root)));
        self.rebuild_sections();
        cx.notify();
        true
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

    /// Forget tasks that left the app (their project was removed).
    pub(super) fn forget_tasks(&mut self, ids: &[String], cx: &mut Context<Self>) {
        self.forget_page_tasks(ids);
        let before = self.pinned_tasks.len();
        self.pinned_tasks
            .retain(|candidate| !ids.contains(candidate));
        if self.pinned_tasks.len() != before {
            for id in ids {
                self.persist_pin(id.clone(), false, cx);
            }
        }
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
        self.pending_project_names
            .get(&self.project_target(root))
            .cloned()
            .or_else(|| {
                self.project_metadata
                    .get(root)
                    .map(|root| root.name.clone())
            })
            .or_else(|| self.task_project_labels.get(root).cloned())
            .unwrap_or_else(|| {
                if self.backend.remote_host().is_some() {
                    remote_root_display_name(root)
                } else {
                    root_display_name(root)
                }
            })
    }

    pub(super) fn pinned_tasks(&self) -> &[String] {
        &self.pinned_tasks
    }

    pub(super) fn project_names(&self) -> HashMap<String, String> {
        let mut names = self.task_project_labels.clone();
        names.extend(
            self.project_metadata
                .iter()
                .map(|(path, root)| (path.clone(), root.name.clone())),
        );
        names
    }

    pub(super) fn navigation_sessions(&self) -> Vec<AgentSessionSummary> {
        self.sessions.clone()
    }

    pub(super) fn navigation_roots(&self) -> Vec<String> {
        self.recent_roots.clone()
    }

    pub(super) fn project_is_removed(&self, root: &str) -> bool {
        self.removed_projects.contains(root)
    }

    pub(super) fn merge_page_sessions(&mut self, summaries: Vec<AgentSessionSummary>) {
        for summary in summaries {
            if self.pages.is_deleted(&summary.id) {
                continue;
            }
            let summary = self.pages.latest_summary(summary);
            if let Some(current) = self
                .sessions
                .iter_mut()
                .find(|current| current.id == summary.id)
            {
                *current = summary;
            } else {
                self.sessions.push(summary);
            }
        }
        self.sync_rows();
    }

    /// The task `delta` rows away from the selected one, as the sidebar
    /// row it sits on and its computer-scoped destination. Stepping walks the flat sidebar list
    /// and stops at its ends.
    pub(super) fn step_target(&self, delta: isize) -> Option<(usize, Navigate)> {
        let rows: Vec<(usize, Navigate)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(row, entry)| match entry {
                SidebarEntry::Task(task) if !task.archived => Some((
                    row,
                    Navigate {
                        computer: self.backend.remote_host().map(str::to_owned),
                        destination: Destination::Task(self.sessions.get(task.session)?.id.clone()),
                    },
                )),
                SidebarEntry::OtherTask(index) if !self.other_tasks.get(*index)?.archived => {
                    Some((row, self.other_tasks.get(*index)?.request.clone()))
                }
                _ => None,
            })
            .collect();
        if rows.is_empty() {
            return None;
        }
        let selected = self.selected.as_deref();
        let current = selected.and_then(|id| {
            rows.iter().position(|(_, request)| {
                request.computer.as_deref() == self.backend.remote_host()
                    && matches!(&request.destination, Destination::Task(task) if task == id)
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
        Some(rows[target].clone())
    }

    /// Scroll `row` into view. A menu hangs off its row, so moving the
    /// list closes one that is open, as a wheel scroll does.
    pub(super) fn reveal_row(&mut self, row: usize, cx: &mut Context<Self>) {
        self.close_popups(cx);
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

    pub(super) fn switcher_menu_open(&self) -> bool {
        self.popup.is_open(&SidebarPopup::Switcher)
    }

    #[cfg(test)]
    pub(super) fn task_menu(&self) -> Option<&str> {
        match self.popup.open_key() {
            Some(SidebarPopup::Task(session)) => Some(&**session),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) fn project_name_for_test(&self, target: &ProjectTarget) -> String {
        self.project_name(target)
    }

    #[cfg(test)]
    pub(super) fn project_menu_labels_for_test(&self, target: &ProjectTarget) -> Vec<&'static str> {
        self.project_menu_items(target)
            .into_iter()
            .map(|item| item.label)
            .collect()
    }

    #[cfg(test)]
    pub(super) fn project_menu(&self) -> Option<&str> {
        self.project_popup
            .open_key()
            .map(|target| target.path.as_str())
    }

    #[cfg(test)]
    pub(super) fn popup_highlight(&self) -> Option<gpui::ElementId> {
        self.popup.highlighted().cloned()
    }

    #[cfg(test)]
    pub(super) fn set_project_name_for_test(&mut self, root: &str, name: &str) {
        self.project_metadata.insert(
            root.to_string(),
            RecentProjectRoot {
                path: root.to_string(),
                name: name.to_string(),
                last_used_ms: 0,
                is_default_workspace: false,
                name_revision: 1,
            },
        );
        self.rebuild_sections();
    }

    #[cfg(test)]
    pub(super) fn set_filter_for_test(&mut self, filter: &str, cx: &mut Context<Self>) {
        self.filter = filter.to_lowercase();
        self.ensure_computer_pages(cx);
        self.rebuild_sections();
    }

    #[cfg(test)]
    pub(super) fn set_pinned_tasks_for_test(&mut self, pinned: Vec<String>) {
        self.pinned_tasks = pinned;
        self.rebuild_sections();
    }

    /// Other tests persist their fixture ids into the settings file this
    /// process reads; a fixture starts from nothing persisted.
    #[cfg(test)]
    pub(super) fn reset_persisted_for_test(&mut self) {
        self.pinned_tasks.clear();
        self.project_metadata.clear();
        self.task_project_labels.clear();
        self.removed_projects.clear();
        self.pending_project_names.clear();
        self.rebuild_sections();
    }

    #[cfg(test)]
    pub(super) fn renames_sent(&self) -> &[(String, String)] {
        &self.renames_sent
    }

    #[cfg(test)]
    pub(super) fn rename_target(&self) -> Option<RenameTarget> {
        self.rename.clone()
    }

    #[cfg(test)]
    pub(super) fn project_rename_pending(&self, root: &str) -> bool {
        self.pending_project_names
            .contains_key(&self.project_target(root))
    }

    #[cfg(test)]
    pub(super) fn archived_expanded(&self) -> bool {
        self.archived_expanded
    }

    #[cfg(test)]
    pub(super) fn open_task_menu_for_test(&mut self, session_id: &str, cx: &mut Context<Self>) {
        self.open_task_menu(session_id.into(), None, cx);
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
        if expanded && self.pages.enabled {
            self.load_page(PageKind::Archived, cx);
        }
        self.refresh_computer_pages(cx);
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
            if self.pages.enabled
                && !self.pages.contains(&session.id, filtering)
                && !self.running.contains(&session.id)
                && self.selected.as_ref() != Some(&session.id)
            {
                continue;
            }
            let root = session.project_root.as_str();
            if session.state != AgentTaskState::Archived
                && !self.removed_projects.contains(root)
                && seen_roots.insert(root)
            {
                session_roots.push(root);
            }
            // The cached count updates when a run ends. Its first run
            // already makes the task visible while that count is still zero.
            if session.message_count == 0 && !self.running.contains(&session.id) {
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
            if session.state == AgentTaskState::Archived {
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
        active.sort_by_cached_key(by_update);
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
            .map(|root| SwitcherRoot::new(self.project_target(root), self.root_name(root).into()))
            .collect();
        self.rebuild_computer_switcher_roots();
        self.rebuild_entries();
    }

    /// Whether a task belongs to the active inbox. The runtime makes a
    /// task active the moment a run starts on it; the running set covers
    /// the frame before its updated record lands.
    fn session_is_active(&self, session: &AgentSessionSummary) -> bool {
        self.running.contains(&session.id) || session.state != AgentTaskState::Settled
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
        if !self.archived_rows.is_empty() || (self.pages.enabled && self.filter.is_empty()) {
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
        if self.pages.enabled {
            for (kind, expanded) in [
                (PageKind::Pinned, self.pinned_expanded),
                (PageKind::Active, self.active_expanded),
                (PageKind::Settled, self.settled_expanded),
                (PageKind::Archived, self.archived_expanded),
                (PageKind::Search, !self.filter.is_empty()),
            ] {
                if expanded && self.pages.has_more(kind) {
                    entries.push(SidebarEntry::LoadMore(kind));
                }
            }
        }
        if !entries.iter().any(|entry| {
            matches!(entry, SidebarEntry::Task(_))
                || (!self.pages.enabled && matches!(entry, SidebarEntry::ArchivedHeader))
        }) && !self.pages.any_loading()
        {
            entries.push(SidebarEntry::Empty(if self.filter.is_empty() {
                SidebarEmpty::NoTasks
            } else {
                SidebarEmpty::NoMatches
            }));
        }
        if self.computers.len() > 1 && self.project_filter.is_none() {
            entries = self.computer_entries();
        }
        for entry in &entries {
            if let SidebarEntry::Task(task) = entry {
                let rung = TaskRung::of(*task);
                let mut actions = vec![TaskMenuAction::Rename];
                if rung == TaskRung::Active {
                    actions.push(TaskMenuAction::Pin);
                }
                if rung != TaskRung::Archived {
                    actions.push(TaskMenuAction::Unread);
                }
                if self.backend.remote_host().is_none() {
                    actions.extend([TaskMenuAction::Separator, TaskMenuAction::Folder]);
                }
                actions.extend([
                    TaskMenuAction::Separator,
                    TaskMenuAction::Move(if rung == TaskRung::Active {
                        TaskMove::Settled
                    } else {
                        TaskMove::Active
                    }),
                ]);
                if rung != TaskRung::Archived {
                    actions.push(TaskMenuAction::Move(TaskMove::Archived));
                }
                actions.push(TaskMenuAction::Move(TaskMove::Deleted));
                self.rows[task.session].menu_actions = actions;
            }
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
        if old.is_empty() {
            // An empty first layout can anchor at item 0. Splicing into
            // 0..0 shifts that anchor past every inserted row, so initialize
            // the first contents at the top instead of preserving it.
            self.list.reset(self.entries.len());
        } else if prefix + suffix < old.len() || prefix + suffix < self.entries.len() {
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
                    && *row.project_root == *session.project_root
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
            if self.pages.enabled {
                self.refresh_pages(cx);
                return;
            }
            self.ensure_computer_pages(cx);
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
        if self.pages.enabled {
            self.refresh_pages(cx);
            return true;
        }
        self.ensure_computer_pages(cx);
        self.rebuild_sections();
        cx.notify();
        true
    }

    /// Whether any of the sidebar's menus is open.
    fn menu_open(&self) -> bool {
        self.popup.open_key().is_some() || self.project_popup.open_key().is_some()
    }

    /// Close whichever popup menu is open. Reports whether one was.
    pub(super) fn close_popups(&mut self, cx: &mut Context<Self>) -> bool {
        let project = self.project_popup.close(cx);
        self.popup.close(cx) || project
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
        let pinned = self.pinned_tasks.iter().any(|pinned| pinned == session_id);
        self.set_task_pinned(session_id, !pinned, cx);
    }

    fn set_task_pinned(&mut self, session_id: &str, pinned: bool, cx: &mut Context<Self>) {
        let is_pinned = self.pinned_tasks.iter().any(|pinned| pinned == session_id);
        if is_pinned == pinned {
            return;
        }
        if pinned {
            self.pinned_tasks.push(session_id.to_string());
        } else {
            self.pinned_tasks.retain(|pinned| pinned != session_id);
        }
        self.rebuild_sections();
        cx.notify();
        self.persist_pin(session_id.to_owned(), pinned, cx);
        cx.emit(SidebarEvent::PreferencesChanged);
    }

    fn persist_pin(&self, task: String, pinned: bool, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let user = self.user_id.clone();
        let computer = backend.remote_host().map(str::to_owned);
        self.call(
            async move {
                if let Some(computer) = computer {
                    let patch = if pinned {
                        serde_json::json!({"pinned_tasks":{"add":[task]}})
                    } else {
                        serde_json::json!({"pinned_tasks":{"remove":[task]}})
                    };
                    backend.set_remote_sidebar(&user, computer, patch).await
                } else {
                    backend.set_task_pinned(&user, &task, pinned).await
                }
            },
            cx,
            |this, result, cx| match result {
                Ok(()) => this.refresh_pages(cx),
                Err(error) => cx.emit(SidebarEvent::Notice(error.into())),
            },
        );
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

    /// Open or close the project switcher menu from its header.
    pub(super) fn toggle_switcher_menu(&mut self, cx: &mut Context<Self>) {
        self.project_popup.close(cx);
        self.popup.toggle(SidebarPopup::Switcher, cx);
    }

    /// Open or close the project switcher from the keyboard. Opening
    /// highlights the current scope, like a native popup button.
    pub(super) fn set_switcher_menu_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if open == self.switcher_menu_open() {
            return;
        }
        self.project_popup.close(cx);
        if open {
            let current = self
                .project_filter
                .as_deref()
                .and_then(|filter| self.switcher_roots.iter().find(|root| root.root == filter))
                .map_or_else(
                    || SharedString::from(ALL_PROJECTS_ID),
                    |root| root.row_id.clone(),
                );
            self.popup
                .open_highlighted(SidebarPopup::Switcher, current, cx);
        } else {
            self.popup.close(cx);
        }
    }

    /// Scope the sidebar to one project, or show every project again.
    pub(super) fn set_project_filter(&mut self, root: Option<String>, cx: &mut Context<Self>) {
        self.project_filter = root.filter(|root| !root.is_empty());
        self.project_popup.close(cx);
        if self.switcher_menu_open() {
            self.popup.close(cx);
        }
        if self.pages.enabled {
            self.refresh_pages(cx);
            return;
        }
        self.rebuild_sections();
        cx.notify();
    }

    /// Whether a cached row still has the current owner name or pending preview.
    fn root_name_matches(&self, root: &str, name: &str) -> bool {
        self.root_name(root) == name
    }

    // ---- Rename ------------------------------------------------------------

    /// Start an inline rename of a task or project in the sidebar.
    pub(super) fn begin_rename(&mut self, target: RenameTarget, cx: &mut Context<Self>) {
        if let RenameTarget::Project(root) = &target
            && self.pending_project_names.contains_key(root)
        {
            return;
        }
        let current = match &target {
            RenameTarget::Task(id) => self
                .sessions
                .iter()
                .find(|session| &session.id == id)
                .map(|session| session.title.clone())
                .unwrap_or_default(),
            RenameTarget::Project(target) => self.project_name(target),
        };
        let chat = self.chat.clone();
        let sidebar = cx.entity().downgrade();
        let escape = sidebar.clone();
        // A project's field sits in the switcher, which keeps the keyboard
        // when the rename ends.
        let in_switcher = matches!(target, RenameTarget::Project(_));
        let application_vim_enabled = self.application_vim_enabled;
        let input = cx.new(move |cx| {
            let mut input = TextInput::new("Name", cx)
                .with_tab_index(0)
                .application_vim(application_vim_enabled);
            input.set_text(&current, cx);
            // The old name opens selected: typing replaces it, an arrow
            // key keeps it.
            input.select_all_text(cx);
            input
                .on_application_escape(move |window, cx| {
                    if in_switcher {
                        let escape = escape.clone();
                        cx.defer(move |cx| {
                            if let Some(sidebar) = escape.upgrade() {
                                sidebar.update(cx, |sidebar, cx| sidebar.cancel_rename(cx));
                            }
                        });
                    } else {
                        // Escape discards: cancel before focus moves, or
                        // the field losing focus would commit the rename.
                        let escape = escape.clone();
                        let chat = chat.clone();
                        window.defer(cx, move |window, cx| {
                            if let Some(sidebar) = escape.upgrade() {
                                sidebar.update(cx, |sidebar, cx| sidebar.cancel_rename(cx));
                            }
                            if let Some(chat) = chat.upgrade() {
                                chat.update(cx, |chat, cx| chat.focus_application_vim(window, cx));
                            }
                        });
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
        self.project_popup.close(cx);
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
        self.end_rename(&target, cx);
        true
    }

    /// Put things back once a rename is over: the row's height, and the
    /// switcher's focus, which a project's field had.
    fn end_rename(&mut self, target: &RenameTarget, cx: &mut Context<Self>) {
        self.rename_input = None;
        self.rename_focus_out = None;
        self.remeasure(target);
        if matches!(target, RenameTarget::Project(_)) {
            self.popup.refocus(cx);
        }
        cx.notify();
    }

    /// The pointer or focus left a task's rename field: commit it, unless
    /// the field's own right-click menu took them (Paste, Cut).
    fn leave_rename(&mut self, cx: &mut Context<Self>) {
        let menu_open = self
            .rename_input
            .as_ref()
            .is_some_and(|input| input.read(cx).menu_open());
        if !menu_open {
            self.commit_rename(cx);
        }
    }

    pub(super) fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.rename.take() else {
            return;
        };
        let name = self
            .rename_input
            .as_ref()
            .map(|input| input.read(cx).text())
            .unwrap_or_default();
        let name = name.trim().to_string();
        self.end_rename(&target, cx);
        if name.is_empty() {
            return;
        }
        match target {
            RenameTarget::Task(session_id) => {
                #[cfg(test)]
                self.renames_sent.push((session_id.clone(), name.clone()));
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
                if self.pending_project_names.contains_key(&root) {
                    return;
                }
                self.pending_project_names
                    .insert(root.clone(), name.clone());
                self.rebuild_sections();
                cx.emit(SidebarEvent::PreferencesChanged);
                cx.notify();
                let backend = self.backend.clone();
                let user_id = self.user_id.clone();
                if root.computer.as_deref() != self.backend.remote_host() {
                    cx.emit(SidebarEvent::RenameProject(ProjectRenameRequest {
                        target: root,
                        name,
                    }));
                    return;
                }
                let path = root.path.clone();
                self.call(
                    async move { backend.rename_project_root(&user_id, path, name).await },
                    cx,
                    move |this, result, cx| {
                        this.pending_project_names.remove(&root);
                        match result {
                            Ok(root) => {
                                this.merge_project_metadata(vec![root.clone()], cx);
                                cx.emit(SidebarEvent::ProjectsChanged(vec![root]));
                            }
                            Err(message) => cx.emit(SidebarEvent::Notice(message.into())),
                        }
                        this.rebuild_sections();
                        cx.emit(SidebarEvent::PreferencesChanged);
                        cx.notify();
                    },
                );
            }
        }
    }

    // ---- Menus -------------------------------------------------------------

    /// Show the project's folder in the file manager.
    fn open_folder(&mut self, root: &str, cx: &mut Context<Self>) {
        let path = root.to_string();
        self.call(
            async move {
                tokio::task::spawn_blocking(move || {
                    // The listed Maple workspace exists only once it is used.
                    if maple_agent::agent::is_default_maple_workspace(std::path::Path::new(&path)) {
                        maple_agent::agent::ensure_default_maple_workspace()?;
                    }
                    crate::platform::reveal_folder(&path)
                })
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

    /// Open or close a project's overflow menu. Its trust row appears once
    /// the project's trust status loads.
    fn toggle_project_menu(&mut self, root: &ProjectTarget, cx: &mut Context<Self>) {
        if !self.project_popup.toggle(root.clone(), cx) {
            return;
        }
        self.menu_trust = None;
        if root.computer.as_deref() != self.backend.remote_host() {
            return;
        }
        let target = root.clone();
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let path = root.path.clone();
        self.call(
            async move { backend.project_trust(&user_id, path).await },
            cx,
            move |this, result, cx| {
                if let Ok(status) = result
                    && this.project_popup.is_open(&target)
                {
                    this.menu_trust = Some(status);
                    cx.notify();
                }
            },
        );
    }

    /// Open or close a task row's menu from its "…" button.
    fn toggle_task_menu(&mut self, session_id: Arc<str>, cx: &mut Context<Self>) {
        self.task_menu_at = None;
        self.popup.toggle(SidebarPopup::Task(session_id), cx);
    }

    /// Open a task row's menu, at `at` where the row was right-clicked or
    /// under its "…" button.
    fn open_task_menu(
        &mut self,
        session_id: Arc<str>,
        at: Option<gpui::Point<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        self.task_menu_at = at;
        self.popup.open(SidebarPopup::Task(session_id), cx);
    }

    /// Build an item from the cached action order, keeping stateful labels live.
    fn task_menu_item(
        &self,
        task: SidebarTaskEntry,
        action: TaskMenuAction,
    ) -> Option<SidebarMenuItem> {
        let row = self.rows.get(task.session)?;
        let mark_unread = !self.unread.contains(&*row.id);
        let (id, icon, label, key) = match action {
            TaskMenuAction::Rename => (&row.menu_rename_id, "pencil", "Rename", "R"),
            TaskMenuAction::Pin => (
                &row.menu_pin_id,
                "pin",
                if task.pinned { "Unpin" } else { "Pin" },
                "P",
            ),
            TaskMenuAction::Unread => (
                &row.menu_unread_id,
                "eye",
                if mark_unread {
                    "Mark as unread"
                } else {
                    "Mark as read"
                },
                "U",
            ),
            TaskMenuAction::Folder => (&row.menu_folder_id, "folder-open", "Open folder", "O"),
            TaskMenuAction::Move(to) => match to {
                TaskMove::Settled => (&row.menu_settle_id, "check", "Settle", "S"),
                TaskMove::Active if TaskRung::of(task) == TaskRung::Archived => {
                    (&row.menu_reopen_id, "archive-restore", "Unarchive", "A")
                }
                TaskMove::Active => (&row.menu_reopen_id, "undo-2", "Reopen", "S"),
                TaskMove::Archived => (&row.menu_archive_id, "archive", "Archive", "A"),
                TaskMove::Deleted => (&row.menu_delete_id, "trash-2", "Delete", "D"),
            },
            TaskMenuAction::Separator => return None,
        };
        let session_id = row.id.clone();
        let on_click: MenuAction = match action {
            TaskMenuAction::Rename => Box::new(move |this, cx| {
                this.begin_rename(RenameTarget::Task(session_id.to_string()), cx)
            }),
            TaskMenuAction::Pin => Box::new(move |this, cx| this.toggle_task_pin(&session_id, cx)),
            TaskMenuAction::Unread => Box::new(move |_, cx| {
                cx.emit(SidebarEvent::SetUnread {
                    session_id: session_id.to_string(),
                    unread: mark_unread,
                });
            }),
            TaskMenuAction::Folder => {
                let root = row.project_root.clone();
                Box::new(move |this, cx| this.open_folder(&root, cx))
            }
            TaskMenuAction::Move(to) => {
                Box::new(move |this, cx| this.move_task(&session_id, to, cx))
            }
            TaskMenuAction::Separator => return None,
        };
        Some(SidebarMenuItem {
            id: id.clone(),
            icon,
            label,
            shortcut: Some(key),
            destructive: matches!(action, TaskMenuAction::Move(TaskMove::Deleted)),
            on_click,
        })
    }

    /// Send a task to a rung. The runtime owns the state, so every move
    /// but delete asks it and the row follows its updated record. Pinned
    /// is a view of active work: leaving active drops the pin at once.
    pub(super) fn move_task(&mut self, session_id: &str, to: TaskMove, cx: &mut Context<Self>) {
        let Some(current) = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .map(|session| session.state)
        else {
            return;
        };
        let target = match to {
            TaskMove::Active => AgentTaskState::Active,
            TaskMove::Settled => AgentTaskState::Settled,
            TaskMove::Archived => AgentTaskState::Archived,
            TaskMove::Deleted => {
                cx.emit(SidebarEvent::DeleteTask(session_id.to_string()));
                cx.notify();
                return;
            }
        };
        if target != AgentTaskState::Active {
            self.set_task_pinned(session_id, false, cx);
        }
        if current != target {
            cx.emit(SidebarEvent::SetState {
                session_id: session_id.to_string(),
                state: target,
            });
            cx.notify();
        }
    }

    #[cfg(test)]
    pub(super) fn task_menu_labels_for_test(&self, session_id: &str) -> Vec<&'static str> {
        self.task_menu_entry(session_id)
            .map(|task| {
                self.rows[task.session]
                    .menu_actions
                    .iter()
                    .filter_map(|&action| self.task_menu_item(task, action))
                    .map(|item| item.label)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The overflow menu of one switcher project row.
    fn project_menu_items(&self, target: &ProjectTarget) -> Vec<SidebarMenuItem> {
        let rename_target = target.clone();
        let root = &target.path;
        let mut items = vec![SidebarMenuItem {
            id: target.element_id("rename-project"),
            icon: "pencil",
            label: "Rename project",
            shortcut: None,
            destructive: false,
            on_click: Box::new(move |this, cx| {
                this.begin_rename(RenameTarget::Project(rename_target.clone()), cx)
            }),
        }];
        // Other actions are still scoped to the active ChatScreen. Never offer
        // them against a foreign folder or reveal its path on this computer.
        if target.computer.as_deref() != self.backend.remote_host() {
            return items;
        }
        let open_root = root.clone();
        let remove_root = root.clone();
        items.push(SidebarMenuItem {
            id: target.element_id("open-project"),
            icon: "folder-open",
            label: "Open folder",
            shortcut: None,
            destructive: false,
            on_click: Box::new(move |this, cx| this.open_folder(&open_root, cx)),
        });
        if let Some(status) = self
            .menu_trust
            .as_ref()
            .filter(|status| status.available && &status.path == root)
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
                shortcut: None,
                destructive: false,
                on_click: Box::new(move |_, cx| {
                    cx.emit(SidebarEvent::SetTrust {
                        path: path.clone(),
                        trusted: !trusted,
                    });
                }),
            });
        }
        items.push(SidebarMenuItem {
            id: SharedString::from(format!("remove-project-{root}")),
            icon: "trash-2",
            label: "Remove project",
            shortcut: None,
            destructive: false,
            on_click: Box::new(move |_, cx| {
                cx.emit(SidebarEvent::RemoveRoot(remove_root.clone()));
            }),
        });
        items
    }

    /// The task entry whose overflow menu is open.
    #[cfg(test)]
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

    // ---- Application Vim ---------------------------------------------------

    fn rebuild_vim_targets(&mut self) {
        self.vim_by_row = self
            .entries
            .iter()
            .map(|entry| match *entry {
                entry @ (SidebarEntry::ComputerProject(_)
                | SidebarEntry::OtherTask(_)
                | SidebarEntry::Computer(_)
                | SidebarEntry::ComputerMore(_)) => self.computer_target(entry),
                SidebarEntry::NewTask => Some(SidebarTarget::NewTask),
                SidebarEntry::ProjectsHeader => Some(SidebarTarget::Projects),
                SidebarEntry::SectionLabel(_) | SidebarEntry::Empty(_) => None,
                SidebarEntry::LoadMore(kind) => {
                    Some(SidebarTarget::LoadMore(format!("page:{}", kind.label())))
                }
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
    fn render_entry(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.selected.as_deref();
        let chat = self.chat.clone();
        let entry = match self.entries.get(ix).copied() {
            Some(
                entry @ (SidebarEntry::ComputerProject(_)
                | SidebarEntry::OtherTask(_)
                | SidebarEntry::Computer(_)
                | SidebarEntry::ComputerMore(_)),
            ) => self.render_computer_entry(entry, self.vim_selects_row(ix), cx),
            Some(SidebarEntry::LoadMore(kind)) => self.render_load_more(kind, cx),
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
            Some(SidebarEntry::ProjectsHeader) => self.render_projects_header(window, cx),
            Some(SidebarEntry::Task(task)) => {
                let application_selected = self.vim_selects_row(ix);
                let row = self.render_task_row(task, selected, application_selected, window, cx);
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
                let count = if self.computers.len() > 1 && self.project_filter.is_none() {
                    self.computer_archived_count
                } else {
                    self.archived_rows.len()
                };
                let count_label = if self.pages.enabled && self.project_filter.is_some()
                    || (self.pages.enabled && self.computers.len() <= 1)
                {
                    if self.pages.has_more(PageKind::Archived) {
                        format!("{count}+")
                    } else if !self.pages.has_started(PageKind::Archived) {
                        String::new()
                    } else {
                        count.to_string()
                    }
                } else {
                    count.to_string()
                };
                div()
                    .id("archived-toggle")
                    .role(gpui::Role::Button)
                    .aria_label(format!("Archived tasks {count_label}"))
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
                            .child(count_label),
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
    fn render_projects_header(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let open = self.switcher_menu_open();
        let menu = open.then(|| {
            let menu = self.switcher_menu(window, cx);
            self.popup.render(menu, Placement::BelowStart, window, cx)
        });
        let header = div()
            .id("projects-header")
            .role(gpui::Role::Button)
            .aria_label("Projects")
            .debug_selector(|| "projects-header".to_string())
            .flex()
            .items_center()
            .gap_1p5()
            .w_full()
            .pl_4()
            // Room for the new-project button, which sits over this end.
            .pr(px(46.))
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
            .child(icon("folder", px(16.), theme::text_secondary()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .line_clamp(1)
                    .text_ellipsis()
                    .child(self.scope_label.clone()),
            )
            .child(icon(
                if open {
                    "chevron-down"
                } else {
                    "chevron-right"
                },
                px(14.),
                theme::text_secondary(),
            ));
        div()
            .relative()
            .w_full()
            .mb_3()
            .child(
                self.popup
                    .trigger(SidebarPopup::Switcher, header, cx, |this, _, cx| {
                        this.toggle_switcher_menu(cx)
                    }),
            )
            .child(
                // A sibling over the header's end, not a button inside a
                // button. It blocks the pointer, so a press on it never
                // reaches the header behind it.
                div()
                    .id("new-project")
                    .role(gpui::Role::Button)
                    .aria_label("New project")
                    .occlude()
                    .absolute()
                    .right_4()
                    .top_0()
                    .bottom_0()
                    .my_auto()
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
                    .tooltip(widgets::tooltip("New project", None))
                    .on_click(cx.listener(|_this, _event, _window, cx| {
                        cx.stop_propagation();
                        cx.emit(SidebarEvent::ChooseProject);
                    }))
                    .child(icon("folder-plus", px(16.), theme::text_secondary())),
            )
            .children(menu)
            .into_any_element()
    }

    /// The menu the project switcher opens: every project, each with an
    /// overflow menu of its own, plus a way back to all projects.
    fn switcher_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> Menu<Self> {
        if self.computers.len() > 1 {
            return self.computer_switcher_menu(window, cx);
        }
        let mut menu = Menu::new("switcher-menu", px(260.))
            .label("Projects")
            .max_height(px(360.))
            .application_vim(self.application_vim_enabled)
            .item(
                MenuItem::new(
                    ALL_PROJECTS_ID,
                    "All projects",
                    |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                        this.set_project_filter(None, cx);
                    },
                )
                .current(self.project_filter.is_none()),
            );
        for root in &self.switcher_roots {
            let pick = root.root.clone();
            let group = root.row_group.clone();
            let mut item = MenuItem::new(
                root.row_id.clone(),
                root.name.clone(),
                move |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                    this.set_project_filter(Some(pick.clone()), cx);
                },
            )
            .current(self.project_filter.as_deref() == Some(root.root.as_str()))
            // The row's overflow button shows while the row is hovered.
            .style(move |row| row.group(group))
            .trailing(self.project_menu_trigger(root, window, cx));
            if let Some(field) = self.project_rename_field(&root.target) {
                item = item.content(field);
            }
            menu = menu.item(item);
        }
        if self.pages.has_more(PageKind::Projects) {
            menu = menu.item(MenuItem::new(
                "switcher-more-projects",
                "Load more projects",
                |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                    this.load_page(PageKind::Projects, cx)
                },
            ));
        }
        menu.item(
            MenuItem::new(
                "switcher-new-project",
                "New project…",
                |_: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                    cx.emit(SidebarEvent::ChooseProject);
                },
            )
            .icon("folder-plus"),
        )
    }

    /// A switcher row's overflow button, with the project's menu under it
    /// while open.
    fn project_menu_trigger(
        &self,
        root: &SwitcherRoot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let open = self.project_popup.is_open(&root.target);
        let menu = open.then(|| {
            let menu = Menu::new(root.target.element_id("project-menu"), px(180.))
                .label("Project options")
                .level(2)
                .application_vim(self.application_vim_enabled)
                .items(
                    self.project_menu_items(&root.target)
                        .into_iter()
                        .map(menu_item),
                );
            self.project_popup
                .render(menu, Placement::BelowEnd, window, cx)
        });
        let toggle = root.target.clone();
        div()
            .relative()
            .child(
                self.project_popup.trigger(
                    root.target.clone(),
                    row_menu_button(
                        root.menu_id.clone(),
                        &root.row_group,
                        "Project options",
                        open,
                    )
                    .when(!open, |button| {
                        button.tooltip(widgets::tooltip("Project options", None))
                    }),
                    cx,
                    move |this, _, cx| this.toggle_project_menu(&toggle, cx),
                ),
            )
            .children(menu)
    }

    /// A task row's "…" button, with the task's menu while open: under
    /// the button, or where a right-click on the row opened it.
    fn task_menu_trigger(
        &self,
        row: &SidebarRow,
        task: SidebarTaskEntry,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let menu = open.then(|| {
            let mut menu = Menu::new(row.menu_panel_id.clone(), px(200.))
                .label("Task options")
                .application_vim(self.application_vim_enabled);
            for &action in &row.menu_actions {
                menu = match self.task_menu_item(task, action) {
                    Some(item) => menu.item(menu_item(item)),
                    None => menu.separator(),
                };
            }
            let placement = self.task_menu_at.map_or(Placement::BelowEnd, Placement::At);
            self.popup.render(menu, placement, window, cx)
        });
        let session = row.id.clone();
        div()
            .relative()
            .child(self.popup.trigger(
                SidebarPopup::Task(row.id.clone()),
                row_menu_button(row.menu_id.clone(), &row.group, "Task options", open),
                cx,
                move |this, _, cx| this.toggle_task_menu(session.clone(), cx),
            ))
            .children(menu)
    }

    /// The rename field for the task being edited, if it is this one.
    /// Renders per visible row per frame, so it compares borrowed ids
    /// instead of building a `RenameTarget` to match against.
    /// A press anywhere outside it commits the rename, even where nothing
    /// takes focus (another row, the list's empty space).
    fn task_rename_field(
        &self,
        session_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<Div>> {
        match self.rename.as_ref() {
            Some(RenameTarget::Task(target)) if target == session_id => {
                self.rename_field().map(|field| {
                    field.on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
                        this.leave_rename(cx);
                    }))
                })
            }
            _ => None,
        }
    }

    /// The rename field for the project being edited, if it is this one.
    fn project_rename_field(&self, root: &ProjectTarget) -> Option<gpui::Stateful<Div>> {
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
                .debug_selector(|| "rename-field".to_string())
                .id("rename-field")
                .flex_1()
                .min_w_0()
                .on_click(|_event, _window, cx| cx.stop_propagation())
                .child(input),
        )
    }

    /// A task row with its menu and inline rename field.
    fn render_task_row(
        &self,
        task: SidebarTaskEntry,
        selected: Option<&str>,
        application_selected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let SidebarTaskEntry {
            session: index,
            pinned: is_pinned,
            settled: _,
            archived,
        } = task;
        let row = &self.rows[index];
        let is_selected = selected == Some(&*row.id);
        let activity = (!archived)
            .then(|| self.session_activity(&row.id))
            .flatten();
        let session_id = Arc::clone(&row.id);
        let menu_id = Arc::clone(&row.id);
        let unpin_id = Arc::clone(&row.id);
        let rename_field = self.task_rename_field(&row.id, cx);
        let renaming = rename_field.is_some();
        let menu_open =
            matches!(self.popup.open_key(), Some(SidebarPopup::Task(open)) if *open == row.id);
        // The menu owns hover until it closes.
        let hover = !renaming && !self.menu_open();
        div()
            .relative()
            .w_full()
            .debug_selector(|| row.element_id.to_string())
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
            .when(menu_open, |row| {
                row.bg(gpui::rgb(theme::bg_sidebar_row_hover()))
            })
            .text_color(gpui::rgb(theme::text_primary()))
            // A row being renamed is a text field: no hover highlight and
            // no hover actions over it, so the edit is plain to see.
            .when(hover, |el| {
                el.hover(|style| {
                    style
                        .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                        .cursor_pointer()
                })
            })
            .on_click(cx.listener(move |_this, _event, _window, cx| {
                cx.emit(SidebarEvent::Select(session_id.to_string()));
            }))
            // A field's right-click is its own (Paste, Cut).
            .when(!renaming, |el| {
                el.on_mouse_down(
                    gpui::MouseButton::Right,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, _window, cx| {
                        this.open_task_menu(menu_id.clone(), Some(event.position), cx);
                    }),
                )
            })
            // The rename field takes the title's line; the project line
            // stays under it.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .map(|column| match rename_field {
                        Some(field) => column.child(field),
                        None => column
                            .child(div().line_clamp(1).text_ellipsis().child(row.title.clone())),
                    })
                    .when(self.computers.len() <= 1, |column| {
                        column.child(
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
                        )
                    }),
            )
            .when_some(activity, |row_element, activity| {
                row_element.child(activity_indicator(&row.spinner_id, activity))
            })
            .when(hover || menu_open, |el| {
                el.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right_0()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .pl_2()
                        .pr_6()
                        .rounded_r(theme::RADIUS_MD)
                        .group_hover(row.group.clone(), |style| {
                            style.bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                        })
                        .when(menu_open, |overlay| {
                            overlay.bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                        })
                        .child(self.task_menu_trigger(row, task, menu_open, window, cx))
                        // The Pinned section names a pinned row at rest; on
                        // hover the pin unpins it.
                        .when(is_pinned, |overlay| {
                            overlay.child(
                                row_button(row.pin_id.clone(), &row.group, "pin", "Unpin")
                                    .tooltip(widgets::tooltip("Unpin", None))
                                    .on_click(cx.listener(move |this, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.toggle_task_pin(&unpin_id, cx);
                                    })),
                            )
                        }),
                )
            })
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

impl Sidebar {
    pub(super) fn sync_popup_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.popup.sync_focus(window, cx);
        self.project_popup.sync_focus(window, cx);
    }
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // First, so the rename field's focus request below wins over a menu
        // handing focus back.
        self.sync_popup_focus(window, cx);
        // A project's rename field lives in the switcher and ends with it,
        // however the switcher closed.
        if matches!(self.rename, Some(RenameTarget::Project(_))) && !self.switcher_menu_open() {
            self.rename = None;
            self.rename_input = None;
        }
        if self.rename_focus_pending {
            self.rename_focus_pending = false;
            if let Some(input) = self.rename_input.clone() {
                let handle = input.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
                // A project's field ends with the switcher around it; a
                // task's field ends where focus goes next. Deferred, so the
                // commit never drops this subscription inside its own call.
                if matches!(self.rename, Some(RenameTarget::Task(_))) {
                    self.rename_focus_out =
                        Some(
                            cx.on_focus_out(&handle, window, |_this, _event, window, cx| {
                                cx.defer_in(window, |this, _window, cx| this.leave_rename(cx));
                            }),
                        );
                }
            }
        }
        let entity = cx.entity().downgrade();
        let list = gpui::list(self.list.clone(), move |ix, window, cx| {
            let Some(sidebar) = entity.upgrade() else {
                return div().into_any_element();
            };
            sidebar.update(cx, |sidebar, cx| sidebar.render_entry(ix, window, cx))
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

/// A row's overflow button: [`row_button`]'s look, but it stays visible
/// and pressed while its menu is open.
fn row_menu_button(
    id: SharedString,
    group: &SharedString,
    label: &'static str,
    open: bool,
) -> gpui::Stateful<Div> {
    row_button(id, group, "ellipsis", label).when(open, |button| {
        button
            .opacity(1.)
            .bg(gpui::rgb(theme::bg_sidebar_row_selected()))
    })
}

/// The small icon button a sidebar row shows while it is hovered.
fn row_button(
    id: SharedString,
    group: &SharedString,
    icon_name: &'static str,
    label: &'static str,
) -> gpui::Stateful<Div> {
    div()
        .debug_selector(|| id.to_string())
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
        .child(icon(icon_name, px(14.), theme::text_secondary()))
}

/// A task or project menu action as a popup menu item.
fn menu_item(item: SidebarMenuItem) -> MenuItem<Sidebar> {
    let SidebarMenuItem {
        id,
        icon,
        label,
        shortcut,
        destructive,
        on_click,
    } = item;
    let item = MenuItem::new(
        id,
        label,
        move |this: &mut Sidebar, _: &mut Window, cx: &mut Context<Sidebar>| on_click(this, cx),
    )
    .icon(icon);
    let item = match shortcut {
        Some(key) => item.shortcut(key),
        None => item,
    };
    if destructive {
        item.destructive()
    } else {
        item
    }
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
        && a.web_enabled == b.web_enabled
        && a.state == b.state
}

pub(super) fn root_display_name(root: &str) -> String {
    if maple_agent::agent::is_default_maple_workspace(std::path::Path::new(root)) {
        return maple_agent::agent::MAPLE_WORKSPACE_DISPLAY_NAME.to_string();
    }
    std::path::Path::new(root)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| root.to_string())
}

/// Interpret Windows paths on a Unix controller without changing the path sent
/// back to the owner, or treating a backslash in a Unix filename as a separator.
fn remote_root_display_name(root: &str) -> String {
    let windows = root.starts_with(r"\\")
        || (root.as_bytes().get(1) == Some(&b':') && root.as_bytes().get(2) == Some(&b'\\'));
    if windows {
        root.trim_end_matches(['\\', '/'])
            .rsplit(['\\', '/'])
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(root)
            .to_string()
    } else {
        root_display_name(root)
    }
}

#[cfg(test)]
mod tests {
    use super::{remote_root_display_name, root_display_name};

    #[test]
    fn remote_project_labels_use_the_owners_path_syntax() {
        assert_eq!(remote_root_display_name(r"C:\Users\Alex\Maple"), "Maple");
        assert_eq!(
            remote_root_display_name(r"\\office\projects\Maple\"),
            "Maple"
        );
        assert_eq!(remote_root_display_name("/home/alex/Maple"), "Maple");
        if cfg!(unix) {
            assert_eq!(remote_root_display_name(r"/home/alex/a\b"), r"a\b");
        }
    }

    #[test]
    fn the_default_workspace_is_labeled_maple_workspace() {
        let Some(workspace) = maple_agent::agent::default_maple_workspace_path() else {
            return;
        };
        assert_eq!(
            root_display_name(&workspace.to_string_lossy()),
            "Maple Workspace"
        );
        let elsewhere = workspace.with_file_name("Notes");
        assert_eq!(root_display_name(&elsewhere.to_string_lossy()), "Notes");
    }
}
