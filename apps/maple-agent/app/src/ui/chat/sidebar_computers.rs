//! The normal sidebar list, grouped by computer and folder when paired hosts
//! exist. This is a summary projection, never a second task store.
use super::*;
use crate::backend::ProjectPageRequest;
use crate::ui::chat::computers::{Computer, Destination, Navigate};
use maple_agent::store::{
    NavigationGroupPage, NavigationGroupQuery, NavigationProjectGroup, NavigationProjectPage,
    NavigationSearchQuery, NavigationTaskPage, NavigationTaskQuery, ProjectSummary, TaskSummary,
};
use maple_agent::store::{PinFilter, TaskSection, TaskState};
use std::hash::{Hash, Hasher};

/// Only requested summary pages are retained. A scope generation fences replies
/// from a previous search, computer selection, or archive visibility setting.
#[derive(Clone, Default)]
pub(super) struct ComputerPages {
    generation: u64,
    scope: Option<(String, Option<String>, bool)>,
    hosts: HashMap<Option<String>, ComputerHostPage>,
    more: Vec<ComputerMore>,
}

impl ComputerPages {
    pub(super) fn project_summaries(&self, computer: &Option<String>) -> Option<&[ProjectSummary]> {
        self.hosts
            .get(computer)
            .map(|page| page.projects.as_slice())
    }

    pub(super) fn own_task_ids(&self, own: Option<&str>) -> HashSet<String> {
        let Some(page) = self
            .hosts
            .iter()
            .find_map(|(key, page)| (key.as_deref() == own).then_some(page))
        else {
            return HashSet::new();
        };
        page.groups
            .iter()
            .flat_map(|group| group.tasks.iter())
            .chain(page.search.iter())
            .chain(page.archived.iter())
            .map(|task| task.uuid.clone())
            .collect()
    }
}

#[derive(Clone, Default)]
struct ComputerHostPage {
    projects: Vec<ProjectSummary>,
    groups: Vec<NavigationProjectGroup>,
    search: Vec<TaskSummary>,
    archived: Vec<TaskSummary>,
    next_groups: Option<String>,
    next_search: Option<String>,
    next_archived: Option<String>,
    next_projects: Option<String>,
    search_roots: Vec<String>,
    search_text: String,
    archived_count: u64,
    loading: HashSet<String>,
    errors: HashMap<String, String>,
    loaded: HashSet<String>,
}

impl ComputerHostPage {
    /// Keep the visible projection until a fresh first page replaces it, but
    /// never use continuation cursors from before the refresh.
    fn invalidate(&mut self) {
        self.next_groups = None;
        self.next_search = None;
        self.next_archived = None;
        self.next_projects = None;
        for group in &mut self.groups {
            group.more = Default::default();
        }
        self.loading.clear();
        self.loaded.clear();
        self.errors.clear();
    }

    fn cursor_is_current(&self, kind: &ComputerPageKind, cursor: &Option<String>) -> bool {
        let expected = match kind {
            ComputerPageKind::Projects => self.next_projects.as_ref(),
            ComputerPageKind::Groups => self.next_groups.as_ref(),
            ComputerPageKind::Search => self.next_search.as_ref(),
            ComputerPageKind::Archived => self.next_archived.as_ref(),
            ComputerPageKind::Project { root, settled } => self
                .groups
                .iter()
                .find(|group| &group.project.root == root)
                .and_then(|group| {
                    if *settled {
                        group.more.settled.as_ref()
                    } else {
                        group.more.active.as_ref()
                    }
                }),
        };
        cursor.as_ref() == expected && (cursor.is_some() || !self.loaded.contains(&kind.key()))
    }
}

#[derive(Clone)]
struct ComputerMore {
    computer: Option<String>,
    generation: u64,
    kind: ComputerPageKind,
    cursor: Option<String>,
    label: SharedString,
    loading: bool,
}

#[derive(Clone)]
enum ComputerPageKind {
    Projects,
    Groups,
    Search,
    Project { root: String, settled: bool },
    Archived,
}

impl ComputerPageKind {
    fn key(&self) -> String {
        match self {
            Self::Projects => "projects".into(),
            Self::Groups => "groups".into(),
            Self::Search => "search".into(),
            Self::Project { root, settled } => format!("project:{settled}:{root}"),
            Self::Archived => "archived".into(),
        }
    }
}

enum ComputerPageReply {
    Projects(NavigationProjectPage),
    Groups(NavigationGroupPage),
    Tasks(NavigationTaskPage),
}

/// The bounded visible projection travels with the desktop's browse state.
/// Exact backend handles and the account fence it from a retired attachment.
pub(super) struct ComputerBrowseState {
    pub(super) user_id: String,
    computers: Arc<Vec<Computer>>,
    pages: ComputerPages,
}

fn same_computer_owner(before: &Computer, after: &Computer) -> bool {
    before.key == after.key
        && before.host_epoch == after.host_epoch
        && matches!((&before.backend, &after.backend), (Some(old), Some(new)) if Arc::ptr_eq(old, new))
}

fn computer_revision(computers: &[Computer]) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    for computer in computers {
        computer.key.hash(&mut hash);
        computer.label.hash(&mut hash);
        computer.backend.as_ref().map(Arc::as_ptr).hash(&mut hash);
        computer.host_epoch.hash(&mut hash);
        computer.loading.hash(&mut hash);
        computer.error.hash(&mut hash);
        // Loading an older page adds summaries to the snapshot. Only a new
        // head or an update to the head invalidates these paging cursors.
        let newest = computer.sessions.iter().map(|task| task.updated_ms).max();
        for task in computer
            .sessions
            .iter()
            .filter(|task| Some(task.updated_ms) == newest)
        {
            task.id.hash(&mut hash);
            task.updated_ms.hash(&mut hash);
            task.title.hash(&mut hash);
            task.message_count.hash(&mut hash);
            format!("{:?}", task.state).hash(&mut hash);
        }
        let mut names = computer.project_names.iter().collect::<Vec<_>>();
        names.sort_unstable();
        names.hash(&mut hash);
    }
    hash.finish()
}

fn existing_snapshot_changed(before: &[Computer], after: &[Computer]) -> bool {
    before.iter().any(|old| {
        let Some(new) = after.iter().find(|computer| computer.key == old.key) else {
            return true;
        };
        old.projects.iter().any(|previous| {
            new.projects
                .iter()
                .find(|current| current.path == previous.path)
                .is_none_or(|current| current.label != previous.label)
        }) || old.sessions.iter().any(|previous| {
            new.sessions
                .iter()
                .find(|current| current.id == previous.id)
                .is_none_or(|current| !session_summary_eq(previous, current))
                || old.pinned.contains(&previous.id) != new.pinned.contains(&previous.id)
        })
    })
}

fn merge_tasks(destination: &mut Vec<TaskSummary>, tasks: Vec<TaskSummary>) {
    for task in tasks {
        if let Some(existing) = destination.iter_mut().find(|row| row.uuid == task.uuid) {
            *existing = task;
        } else {
            destination.push(task);
        }
    }
}

fn snapshot_page(computer: &Computer, query: &str, archived_expanded: bool) -> ComputerHostPage {
    let mut page = ComputerHostPage {
        projects: computer
            .projects
            .iter()
            .map(|project| ProjectSummary {
                root: project.path.clone(),
                display_name: Some(project.label.to_string()),
                position: None,
                last_used_ms: 0,
                name_revision: 0,
                is_default_workspace: false,
                truncated: false,
            })
            .collect(),
        ..ComputerHostPage::default()
    };
    for session in &computer.sessions {
        if session.message_count == 0 && !computer.running.contains(&session.id) {
            continue;
        }
        let project = computer
            .projects
            .iter()
            .find(|p| p.path == session.project_root);
        let state = match session.state {
            AgentTaskState::Active => "active",
            AgentTaskState::Settled => "settled",
            AgentTaskState::Archived => "archived",
        };
        let task = TaskSummary {
            uuid: session.id.clone(),
            title: session.title.clone(),
            project_root: session.project_root.clone(),
            project_display_name: project.map(|p| p.label.to_string()),
            state: state.into(),
            kind: if session.acp { "acp" } else { "desktop" }.into(),
            pinned_ms: computer
                .pinned
                .iter()
                .position(|id| id == &session.id)
                .map(|index| i64::MAX - i64::try_from(index).unwrap_or(i64::MAX)),
            web_enabled: session.web_enabled,
            model: session.model.clone(),
            message_count: u64::try_from(session.message_count).unwrap_or(u64::MAX),
            created_ms: session.created_ms,
            updated_ms: session.updated_ms,
            truncated: false,
        };
        if state == "archived" {
            page.archived_count = page.archived_count.saturating_add(1);
            if archived_expanded {
                page.archived.push(task.clone());
            }
        }
        if !query.is_empty() {
            if session.title.to_lowercase().contains(query)
                || computer.label.to_lowercase().contains(query)
                || project.is_some_and(|p| p.label.to_lowercase().contains(query))
                || computer
                    .project_names
                    .get(&session.project_root)
                    .is_some_and(|name| name.to_lowercase().contains(query))
            {
                page.search.push(task);
            }
        } else if state != "archived" {
            if let Some(group) = page
                .groups
                .iter_mut()
                .find(|g| g.project.root == session.project_root)
            {
                group.tasks.push(task);
            } else {
                page.groups.push(NavigationProjectGroup {
                    project: maple_agent::store::ProjectSummary {
                        root: session.project_root.clone(),
                        display_name: project.map(|p| p.label.to_string()),
                        position: None,
                        last_used_ms: session.updated_ms,
                        name_revision: 0,
                        is_default_workspace: false,
                        truncated: false,
                    },
                    tasks: vec![task],
                    more: Default::default(),
                });
            }
        }
    }
    page
}

pub(super) struct ProjectHeading {
    request: Navigate,
    id: SharedString,
    label: SharedString,
    remote: bool,
}
pub(super) struct OtherTask {
    pub(super) request: Navigate,
    pub(super) archived: bool,
    id: SharedString,
    title: SharedString,
    activity: Option<SessionActivity>,
    pinned: bool,
}

impl Sidebar {
    pub(in crate::ui::chat) fn set_computers(
        &mut self,
        computers: Arc<Vec<Computer>>,
        cx: &mut Context<Self>,
    ) {
        let changed = computer_revision(&self.computers) != computer_revision(&computers)
            || existing_snapshot_changed(&self.computers, &computers);
        if changed {
            self.group_pages.hosts.retain(|key, _| {
                self.computers
                    .iter()
                    .find(|computer| &computer.key == key)
                    .zip(computers.iter().find(|computer| &computer.key == key))
                    .is_some_and(|(old, new)| same_computer_owner(old, new))
            });
        }
        self.computers = computers;
        if changed {
            self.revalidate_computer_pages(cx);
        } else {
            self.ensure_computer_pages(cx);
        }
        self.rebuild_computer_switcher_roots();
        self.rebuild_entries();
        self.list.remeasure_items(0..self.entries.len());
        cx.notify();
    }

    pub(super) fn computer_browse_state(&self) -> ComputerBrowseState {
        ComputerBrowseState {
            user_id: self.user_id.clone(),
            computers: self.computers.clone(),
            pages: self.group_pages.clone(),
        }
    }

    pub(super) fn restore_computer_browse_state(
        &mut self,
        mut state: ComputerBrowseState,
        cx: &mut Context<Self>,
    ) {
        let scope = (
            self.filter.clone(),
            self.project_filter.clone(),
            self.archived_expanded,
        );
        if state.user_id != self.user_id || state.pages.scope.as_ref() != Some(&scope) {
            return;
        }
        // Pending callbacks belong to the previous Sidebar entity. Fence this
        // entity's earlier requests, and restart any inherited initial reads.
        self.group_pages.generation = self.group_pages.generation.wrapping_add(1);
        state.pages.hosts.retain(|key, page| {
            let owners = state
                .computers
                .iter()
                .find(|computer| &computer.key == key)
                .zip(self.computers.iter().find(|computer| &computer.key == key));
            let Some((old, new)) = owners.filter(|(old, new)| same_computer_owner(old, new)) else {
                return false;
            };
            let refreshing = self
                .group_pages
                .hosts
                .get(key)
                .is_some_and(|current| !current.loading.is_empty());
            page.loading.clear();
            if refreshing
                || computer_revision(std::slice::from_ref(old))
                    != computer_revision(std::slice::from_ref(new))
                || existing_snapshot_changed(std::slice::from_ref(old), std::slice::from_ref(new))
            {
                page.invalidate();
            }
            true
        });
        self.group_pages.hosts = state.pages.hosts;
        self.group_pages.scope = Some(scope);
        self.group_pages.more.clear();
        // Own-host rows use the normal session projection. Carry their bounded
        // summaries too so changing the selected computer cannot hide them.
        let own_tasks = self
            .group_pages
            .hosts
            .get(&self.backend.remote_host().map(str::to_owned))
            .map(|page| {
                page.groups
                    .iter()
                    .flat_map(|group| group.tasks.iter())
                    .chain(page.search.iter())
                    .chain(page.archived.iter())
                    .map(|task| {
                        self.sessions
                            .iter()
                            .chain(
                                self.computers
                                    .iter()
                                    .filter(|computer| {
                                        computer.key.as_deref() == self.backend.remote_host()
                                    })
                                    .flat_map(|computer| computer.sessions.iter()),
                            )
                            .filter(|session| {
                                session.id == task.uuid && session.updated_ms >= task.updated_ms
                            })
                            .max_by_key(|session| session.updated_ms)
                            .cloned()
                            .unwrap_or_else(|| session_from_summary(task))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.merge_page_sessions(own_tasks);
        self.ensure_computer_pages(cx);
        self.rebuild_computer_switcher_roots();
    }

    pub(super) fn revalidate_computer_pages(&mut self, cx: &mut Context<Self>) {
        self.group_pages.generation = self.group_pages.generation.wrapping_add(1);
        for page in self.group_pages.hosts.values_mut() {
            page.invalidate();
        }
        self.ensure_computer_pages(cx);
    }

    pub(super) fn refresh_computer_pages(&mut self, cx: &mut Context<Self>) {
        self.group_pages.generation = self.group_pages.generation.wrapping_add(1);
        self.group_pages.hosts.clear();
        self.group_pages.more.clear();
        self.group_pages.scope = None;
        self.ensure_computer_pages(cx);
    }

    pub(super) fn ensure_computer_pages(&mut self, cx: &mut Context<Self>) {
        if self.computers.len() <= 1 {
            return;
        }
        let scope = (
            self.filter.clone(),
            self.project_filter.clone(),
            self.archived_expanded,
        );
        if self.group_pages.scope.as_ref() != Some(&scope) {
            self.group_pages.generation = self.group_pages.generation.wrapping_add(1);
            self.group_pages.hosts.clear();
            self.group_pages.scope = Some(scope);
        }
        let computers = self.computers.clone();
        for computer in computers.iter() {
            self.group_pages
                .hosts
                .entry(computer.key.clone())
                .or_insert_with(|| snapshot_page(computer, &self.filter, self.archived_expanded));
        }
        let hosts = computers
            .iter()
            .filter(|computer| computer.backend.is_some())
            .map(|computer| computer.key.clone())
            .collect::<Vec<_>>();
        for computer in hosts {
            let page = self.group_pages.hosts.entry(computer.clone()).or_default();
            if !page.loaded.contains("projects")
                && !page.loading.contains("projects")
                && !page.errors.contains_key("projects")
            {
                self.load_computer_page(
                    ComputerMore {
                        computer: computer.clone(),
                        generation: self.group_pages.generation,
                        kind: ComputerPageKind::Projects,
                        cursor: None,
                        label: "Loading projects…".into(),
                        loading: false,
                    },
                    cx,
                );
            }
            if self.project_filter.is_some() {
                continue;
            }
            let page = self.group_pages.hosts.entry(computer.clone()).or_default();
            let kind = if self.filter.is_empty() {
                ComputerPageKind::Groups
            } else {
                ComputerPageKind::Search
            };
            let key = kind.key();
            if !page.loaded.contains(&key)
                && !page.loading.contains(&key)
                && !page.errors.contains_key(&key)
            {
                self.load_computer_page(
                    ComputerMore {
                        computer: computer.clone(),
                        generation: self.group_pages.generation,
                        kind,
                        cursor: None,
                        label: "Loading tasks…".into(),
                        loading: false,
                    },
                    cx,
                );
            }
            if self.archived_expanded && self.filter.is_empty() {
                let page = self.group_pages.hosts.entry(computer.clone()).or_default();
                if !page.loaded.contains("archived")
                    && !page.loading.contains("archived")
                    && !page.errors.contains_key("archived")
                {
                    self.load_computer_page(
                        ComputerMore {
                            computer,
                            generation: self.group_pages.generation,
                            kind: ComputerPageKind::Archived,
                            cursor: None,
                            label: "Loading archived tasks…".into(),
                            loading: false,
                        },
                        cx,
                    );
                }
            }
        }
    }

    fn load_computer_page(&mut self, action: ComputerMore, cx: &mut Context<Self>) {
        if action.generation != self.group_pages.generation {
            return;
        }
        let Some(computer) = self.computers.iter().find(|c| c.key == action.computer) else {
            return;
        };
        let Some(backend) = computer.backend.clone() else {
            return;
        };
        let key = action.kind.key();
        let page = self
            .group_pages
            .hosts
            .entry(action.computer.clone())
            .or_default();
        if !page.cursor_is_current(&action.kind, &action.cursor) {
            return;
        }
        if !page.loading.insert(key.clone()) {
            return;
        }
        page.errors.remove(&key);
        let user = self.user_id.clone();
        let query = self.filter.clone();
        let project = self.project_filter.clone();
        if matches!(action.kind, ComputerPageKind::Search) && action.cursor.is_none() {
            // Matching a computer name means browsing that whole computer.
            // Freeze the request's custom-name roots with its cursor scope.
            page.search_text = if computer.label.to_lowercase().contains(&query) {
                String::new()
            } else {
                query.clone()
            };
            page.search_roots = if page.search_text.is_empty() {
                Vec::new()
            } else {
                computer
                    .project_names
                    .iter()
                    .filter(|(_, name)| name.to_lowercase().contains(&query))
                    .map(|(root, _)| root.clone())
                    .collect()
            };
            page.search_roots.sort_unstable();
            page.search_roots.dedup();
        }
        let matched_roots = page.search_roots.clone();
        let search_text = page.search_text.clone();
        let request = action.clone();
        self.call(
            async move {
                match &request.kind {
                    ComputerPageKind::Projects => backend
                        .list_projects(
                            &user,
                            ProjectPageRequest {
                                cursor: request.cursor.clone(),
                                limit: 50,
                            },
                        )
                        .await
                        .map(ComputerPageReply::Projects),
                    ComputerPageKind::Groups => backend
                        .list_project_groups(
                            &user,
                            NavigationGroupQuery {
                                limit_groups: 20,
                                per_project: 20,
                                cursor: request.cursor.clone(),
                            },
                        )
                        .await
                        .map(ComputerPageReply::Groups),
                    ComputerPageKind::Search => backend
                        .search_tasks(
                            &user,
                            NavigationSearchQuery {
                                text: search_text,
                                project,
                                project_roots: matched_roots,
                                cursor: request.cursor.clone(),
                                limit: 100,
                            },
                        )
                        .await
                        .map(ComputerPageReply::Tasks),
                    ComputerPageKind::Project { root, settled } => backend
                        .list_tasks(
                            &user,
                            NavigationTaskQuery {
                                section: TaskSection::State(if *settled {
                                    TaskState::Settled
                                } else {
                                    TaskState::Active
                                }),
                                project: Some(root.clone()),
                                pins: PinFilter::Ignore,
                                cursor: request.cursor.clone(),
                                limit: 50,
                            },
                        )
                        .await
                        .map(ComputerPageReply::Tasks),
                    ComputerPageKind::Archived => backend
                        .list_tasks(
                            &user,
                            NavigationTaskQuery {
                                section: TaskSection::State(TaskState::Archived),
                                project,
                                pins: PinFilter::Ignore,
                                cursor: request.cursor.clone(),
                                limit: 50,
                            },
                        )
                        .await
                        .map(ComputerPageReply::Tasks),
                }
            },
            cx,
            move |this, result, cx| this.apply_computer_page(action, result, cx),
        );
        self.rebuild_entries();
        cx.notify();
    }

    fn apply_computer_page(
        &mut self,
        action: ComputerMore,
        result: Result<ComputerPageReply, String>,
        cx: &mut Context<Self>,
    ) {
        let key = action.kind.key();
        if action.generation != self.group_pages.generation {
            return;
        }
        let own = action.computer.as_deref() == self.backend.remote_host();
        let page = self
            .group_pages
            .hosts
            .entry(action.computer.clone())
            .or_default();
        page.loading.remove(&key);
        let result = match result {
            Ok(reply) => reply,
            Err(error) => {
                page.errors.insert(key, error);
                self.rebuild_entries();
                cx.notify();
                return;
            }
        };
        page.loaded.insert(key);
        let mut own_tasks = Vec::new();
        let mut own_labels = Vec::new();
        match result {
            ComputerPageReply::Projects(reply) => {
                if action.cursor.is_none() {
                    page.projects.clear();
                }
                page.next_projects = reply.next_cursor;
                for project in reply.projects {
                    if let Some(existing) =
                        page.projects.iter_mut().find(|p| p.root == project.root)
                    {
                        *existing = project;
                    } else {
                        page.projects.push(project);
                    }
                }
            }
            ComputerPageReply::Groups(reply) => {
                if action.cursor.is_none() {
                    page.groups.clear();
                }
                page.archived_count = reply.archived_count;
                page.next_groups = reply.next_cursor;
                for group in reply.groups {
                    if own {
                        own_tasks.extend(group.tasks.iter().map(session_from_summary));
                        own_labels.extend(group.tasks.iter().map(|task| {
                            (task.project_root.clone(), task.project_display_name.clone())
                        }));
                    }
                    if let Some(existing) = page
                        .groups
                        .iter_mut()
                        .find(|g| g.project.root == group.project.root)
                    {
                        merge_tasks(&mut existing.tasks, group.tasks);
                        existing.more = group.more;
                        existing.project = group.project;
                    } else {
                        page.groups.push(group);
                    }
                }
            }
            ComputerPageReply::Tasks(reply) => {
                if own {
                    own_tasks.extend(reply.tasks.iter().map(session_from_summary));
                    own_labels.extend(reply.tasks.iter().map(|task| {
                        (task.project_root.clone(), task.project_display_name.clone())
                    }));
                }
                match action.kind {
                    ComputerPageKind::Search => {
                        if action.cursor.is_none() {
                            page.search.clear();
                        }
                        page.next_search = reply.next_cursor;
                        merge_tasks(&mut page.search, reply.tasks);
                    }
                    ComputerPageKind::Archived => {
                        if action.cursor.is_none() {
                            page.archived.clear();
                        }
                        page.next_archived = reply.next_cursor;
                        merge_tasks(&mut page.archived, reply.tasks);
                        page.archived_count = page
                            .archived_count
                            .max(u64::try_from(page.archived.len()).unwrap_or(u64::MAX));
                    }
                    ComputerPageKind::Project { root, settled } => {
                        if let Some(group) = page.groups.iter_mut().find(|g| g.project.root == root)
                        {
                            if settled {
                                group.more.settled = reply.next_cursor;
                            } else {
                                group.more.active = reply.next_cursor;
                            }
                            merge_tasks(&mut group.tasks, reply.tasks);
                        }
                    }
                    ComputerPageKind::Groups | ComputerPageKind::Projects => {}
                }
            }
        }
        if own {
            self.remember_task_project_labels(own_labels);
            self.merge_page_sessions(own_tasks);
        }
        self.rebuild_computer_switcher_roots();
        self.rebuild_entries();
        self.list.remeasure_items(0..self.entries.len());
        cx.notify();
    }

    fn computer_more_entry(
        &mut self,
        entries: &mut Vec<SidebarEntry>,
        computer: &Option<String>,
        kind: ComputerPageKind,
        cursor: Option<String>,
        label: &str,
    ) {
        let Some(page) = self.group_pages.hosts.get(computer) else {
            return;
        };
        let key = kind.key();
        let loading = page.loading.contains(&key);
        let failed = page.errors.contains_key(&key);
        if cursor.is_none() && !loading && !failed {
            return;
        }
        let index = self.group_pages.more.len();
        self.group_pages.more.push(ComputerMore {
            computer: computer.clone(),
            generation: self.group_pages.generation,
            kind,
            cursor,
            loading,
            label: if failed {
                format!("Could not load tasks — retry {label}").into()
            } else if loading {
                "Loading tasks…".into()
            } else {
                label.to_string().into()
            },
        });
        entries.push(SidebarEntry::ComputerMore(index));
    }

    pub(super) fn computer_entries(&mut self) -> Vec<SidebarEntry> {
        self.computer_projects.clear();
        self.other_tasks.clear();
        self.group_pages.more.clear();
        self.computer_archived_count = 0;
        let mut entries = vec![
            SidebarEntry::NewTask,
            SidebarEntry::Search,
            SidebarEntry::ProjectsHeader,
        ];
        let mut archived_entries = Vec::new();
        // These small clones let rendering append row projections without
        // borrowing the per-host paging cache for the whole rebuild.
        let computers = self.computers.clone();
        for (computer_index, computer) in computers.iter().enumerate() {
            let own = computer.key.as_deref() == self.backend.remote_host();
            if self.project_filter.is_some() && !own {
                continue;
            }
            let Some(page) = self.group_pages.hosts.get(&computer.key) else {
                entries.push(SidebarEntry::Computer(computer_index));
                continue;
            };
            self.computer_archived_count = self
                .computer_archived_count
                .saturating_add(usize::try_from(page.archived_count).unwrap_or(usize::MAX));
            let searching = !self.filter.is_empty();
            let mut groups = page.groups.clone();
            let search = page.search.clone();
            let archived = page.archived.clone();
            let next = if searching {
                page.next_search.clone()
            } else {
                page.next_groups.clone()
            };
            let next_archived = page.next_archived.clone();
            if searching {
                groups.clear();
                for task in search {
                    if task.state == "archived" && !self.archived_expanded {
                        continue;
                    }
                    if let Some(group) = groups
                        .iter_mut()
                        .find(|g| g.project.root == task.project_root)
                    {
                        group.tasks.push(task);
                    } else {
                        let project = computer
                            .projects
                            .iter()
                            .find(|p| p.path == task.project_root);
                        groups.push(NavigationProjectGroup {
                            project: maple_agent::store::ProjectSummary {
                                root: task.project_root.clone(),
                                display_name: computer
                                    .project_names
                                    .get(&task.project_root)
                                    .cloned()
                                    .or_else(|| task.project_display_name.clone())
                                    .or_else(|| project.map(|p| p.label.to_string())),
                                position: None,
                                last_used_ms: task.updated_ms,
                                name_revision: 0,
                                is_default_workspace: false,
                                truncated: false,
                            },
                            tasks: vec![task],
                            more: Default::default(),
                        });
                    }
                }
            }
            if groups.is_empty() && (computer.backend.is_none() || computer.error.is_some()) {
                entries.push(SidebarEntry::Computer(computer_index));
            }
            for mut group in groups {
                if own
                    && self
                        .project_filter
                        .as_ref()
                        .is_some_and(|root| root != &group.project.root)
                {
                    continue;
                }
                let root = group.project.root.clone();
                self.push_computer_heading(&mut entries, computer_index, computer, &group.project);
                group.tasks.sort_by_key(|task| {
                    (
                        task.state == "archived",
                        task.state == "settled",
                        task.pinned_ms.is_none(),
                        std::cmp::Reverse(task.pinned_ms),
                        std::cmp::Reverse(task.updated_ms),
                    )
                });
                for task in &group.tasks {
                    self.push_computer_task(&mut entries, computer_index, computer, task);
                }
                if !searching {
                    self.computer_more_entry(
                        &mut entries,
                        &computer.key,
                        ComputerPageKind::Project {
                            root: root.clone(),
                            settled: false,
                        },
                        group.more.active,
                        "more active tasks",
                    );
                    self.computer_more_entry(
                        &mut entries,
                        &computer.key,
                        ComputerPageKind::Project {
                            root,
                            settled: true,
                        },
                        group.more.settled,
                        "more settled tasks",
                    );
                }
            }
            self.computer_more_entry(
                &mut entries,
                &computer.key,
                if searching {
                    ComputerPageKind::Search
                } else {
                    ComputerPageKind::Groups
                },
                next,
                if searching {
                    "more matching tasks"
                } else {
                    "more projects"
                },
            );
            if self.archived_expanded && !searching {
                let mut archived_groups: Vec<(ProjectSummary, Vec<&TaskSummary>)> = Vec::new();
                for task in &archived {
                    if let Some((_, tasks)) = archived_groups
                        .iter_mut()
                        .find(|(project, _)| project.root == task.project_root)
                    {
                        tasks.push(task);
                    } else {
                        archived_groups.push((
                            ProjectSummary {
                                root: task.project_root.clone(),
                                display_name: task.project_display_name.clone(),
                                position: None,
                                last_used_ms: task.updated_ms,
                                name_revision: 0,
                                is_default_workspace: false,
                                truncated: false,
                            },
                            vec![task],
                        ));
                    }
                }
                for (project, tasks) in archived_groups {
                    self.push_computer_heading(
                        &mut archived_entries,
                        computer_index,
                        computer,
                        &project,
                    );
                    for task in tasks {
                        self.push_computer_task(
                            &mut archived_entries,
                            computer_index,
                            computer,
                            task,
                        );
                    }
                }
                self.computer_more_entry(
                    &mut archived_entries,
                    &computer.key,
                    ComputerPageKind::Archived,
                    next_archived,
                    "more archived tasks",
                );
            }
        }
        if self.computer_archived_count > 0 || !self.filter.is_empty() {
            entries.push(SidebarEntry::ArchivedHeader);
            entries.extend(archived_entries);
        }
        entries
    }

    fn push_computer_heading(
        &mut self,
        entries: &mut Vec<SidebarEntry>,
        computer_index: usize,
        computer: &Computer,
        project: &ProjectSummary,
    ) {
        let heading = self.computer_projects.len();
        let root = &project.root;
        let label = self.computer_project_name(computer, project);
        self.computer_projects.push(ProjectHeading {
            request: Navigate {
                computer: computer.key.clone(),
                destination: Destination::Project(root.clone()),
            },
            id: format!("computer-{computer_index}-folder-{heading}").into(),
            label: format!("{} · {}", computer.label, label).into(),
            remote: computer.key.is_some(),
        });
        entries.push(SidebarEntry::ComputerProject(heading));
    }

    pub(super) fn computer_project_name(
        &self,
        computer: &Computer,
        project: &ProjectSummary,
    ) -> String {
        let target = ProjectTarget {
            computer: computer.key.clone(),
            path: project.root.clone(),
        };
        self.pending_project_names
            .get(&target)
            .cloned()
            .or_else(|| {
                (target.computer.as_deref() == self.backend.remote_host())
                    .then(|| self.project_metadata.get(&project.root))
                    .flatten()
                    .filter(|metadata| metadata.name_revision >= project.name_revision)
                    .map(|metadata| metadata.name.clone())
            })
            .or_else(|| {
                // Snapshot-only fallbacks have no owner revision. A refreshed
                // computer snapshot can supply their newest owner label.
                (project.name_revision == 0)
                    .then(|| computer.project_names.get(&project.root).cloned())
                    .flatten()
            })
            .or_else(|| project.display_name.clone())
            .unwrap_or_else(|| self.project_name(&target))
    }

    fn push_computer_task(
        &mut self,
        entries: &mut Vec<SidebarEntry>,
        computer_index: usize,
        computer: &Computer,
        task: &TaskSummary,
    ) {
        let own = computer.key.as_deref() == self.backend.remote_host();
        if own {
            if let Some(index) = self.sessions.iter().position(|s| s.id == task.uuid) {
                let state = self.sessions[index].state;
                if state == AgentTaskState::Archived && !self.archived_expanded {
                    return;
                }
                entries.push(SidebarEntry::Task(SidebarTaskEntry {
                    session: index,
                    pinned: task.pinned_ms.is_some(),
                    settled: state == AgentTaskState::Settled,
                    archived: state == AgentTaskState::Archived,
                }));
            }
        } else {
            let latest = computer
                .sessions
                .iter()
                .find(|session| session.id == task.uuid)
                .filter(|session| session.updated_ms >= task.updated_ms);
            let archived = latest.map_or(task.state == "archived", |session| {
                session.state == AgentTaskState::Archived
            });
            if archived && !self.archived_expanded {
                return;
            }
            let index = self.other_tasks.len();
            self.other_tasks.push(OtherTask {
                archived,
                request: Navigate {
                    computer: computer.key.clone(),
                    destination: Destination::Task(task.uuid.clone()),
                },
                id: format!("computer-{computer_index}-task-{}", task.uuid).into(),
                title: latest
                    .map_or_else(|| task.title.clone(), |session| session.title.clone())
                    .into(),
                pinned: task.pinned_ms.is_some(),
                activity: if computer.running.contains(&task.uuid) {
                    Some(SessionActivity::Running)
                } else if computer.unread.contains(&task.uuid) {
                    Some(SessionActivity::CompletedUnread)
                } else {
                    None
                },
            });
            entries.push(SidebarEntry::OtherTask(index));
        }
    }

    pub(in crate::ui::chat) fn computer_target(
        &self,
        entry: SidebarEntry,
    ) -> Option<SidebarTarget> {
        if let SidebarEntry::ComputerMore(index) = entry {
            return Some(SidebarTarget::LoadMore(self.computer_more_id(index)));
        }
        let request = match entry {
            SidebarEntry::ComputerProject(index) => {
                self.computer_projects.get(index)?.request.clone()
            }
            SidebarEntry::OtherTask(index) => self.other_tasks.get(index)?.request.clone(),
            SidebarEntry::Computer(index) => Navigate {
                computer: self.computers.get(index)?.key.clone(),
                destination: Destination::Overview,
            },
            _ => return None,
        };
        Some(SidebarTarget::Computer(request))
    }

    pub(super) fn computer_more_id(&self, index: usize) -> String {
        let action = &self.group_pages.more[index];
        format!(
            "computer:{}",
            serde_json::to_string(&(
                action.generation,
                &action.computer,
                action.kind.key(),
                &action.cursor,
            ))
            .expect("computer paging identity is serializable")
        )
    }

    pub(super) fn activate_computer_more(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        let Some(index) =
            (0..self.group_pages.more.len()).find(|index| self.computer_more_id(*index) == id)
        else {
            return false;
        };
        let action = self.group_pages.more[index].clone();
        if !action.loading {
            self.load_computer_page(action, cx);
        }
        true
    }

    pub(super) fn render_computer_entry(
        &self,
        entry: SidebarEntry,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match entry {
            SidebarEntry::ComputerMore(index) => {
                let action = self.group_pages.more[index].clone();
                let label = action.label.clone();
                let loading = action.loading;
                div()
                    .id(SharedString::from(format!("computer-page-{index}")))
                    .role(gpui::Role::Button)
                    .aria_label(label.clone())
                    .px_8()
                    .py_2()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .when(selected, |row| {
                        row.aria_active_descendant()
                            .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                    })
                    .when(!loading, |row| {
                        row.hover(|s| {
                            s.cursor_pointer()
                                .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                        })
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !loading {
                            this.load_computer_page(action.clone(), cx);
                        }
                    }))
                    .child(label)
                    .into_any_element()
            }
            SidebarEntry::ComputerProject(index) => {
                let project = &self.computer_projects[index];
                let request = project.request.clone();
                div()
                    .id(project.id.clone())
                    .role(gpui::Role::Button)
                    .aria_label(project.label.clone())
                    .mt_3()
                    .px_4()
                    .py_1p5()
                    .flex()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .when(selected, |row| {
                        row.aria_active_descendant()
                            .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                    })
                    .hover(|s| {
                        s.bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                            .cursor_pointer()
                    })
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(SidebarEvent::Navigate(request.clone()))
                    }))
                    .child(icon(
                        if project.remote { "globe" } else { "folder" },
                        px(14.),
                        theme::text_muted(),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(project.label.clone()),
                    )
                    .into_any_element()
            }
            SidebarEntry::OtherTask(index) => {
                let task = &self.other_tasks[index];
                let request = task.request.clone();
                div()
                    .id(task.id.clone())
                    .role(gpui::Role::ListBoxOption)
                    .aria_label(task.title.clone())
                    .aria_selected(false)
                    .pl_8()
                    .pr_6()
                    .py_1()
                    .flex()
                    .gap_2()
                    .items_center()
                    .rounded(theme::RADIUS_MD)
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_primary()))
                    .when(selected, |row| {
                        row.aria_active_descendant()
                            .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                    })
                    .hover(|s| {
                        s.bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                            .cursor_pointer()
                    })
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(SidebarEvent::Navigate(request.clone()))
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .line_clamp(1)
                            .text_ellipsis()
                            .child(task.title.clone()),
                    )
                    .when_some(task.activity, |row, activity| {
                        row.child(activity_indicator(&task.id, activity))
                    })
                    .when(task.pinned, |row| {
                        row.child(icon("pin", px(13.), theme::accent()))
                    })
                    .into_any_element()
            }
            SidebarEntry::Computer(index) => {
                let computer = &self.computers[index];
                let request = Navigate {
                    computer: computer.key.clone(),
                    destination: Destination::Overview,
                };
                div()
                    .id(SharedString::from(format!("computer-{index}")))
                    .role(gpui::Role::Button)
                    .aria_label(computer.label.clone())
                    .mt_3()
                    .px_4()
                    .py_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .hover(|s| {
                        s.bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                            .cursor_pointer()
                    })
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(SidebarEvent::Navigate(request.clone()))
                    }))
                    .child(computer.label.clone())
                    .child(computer.error.clone().unwrap_or_else(|| {
                        if computer.loading {
                            "Connecting…"
                        } else {
                            "No projects yet"
                        }
                        .into()
                    }))
                    .into_any_element()
            }
            _ => div().into_any_element(),
        }
    }

    pub(super) fn computer_switcher_menu(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Menu<Self> {
        let mut menu = Menu::new("switcher-menu", px(330.))
            .label("Projects")
            .max_height(px(440.))
            .application_vim(self.application_vim_enabled)
            .item(
                MenuItem::new(ALL_PROJECTS_ID, "All projects", |this: &mut Self, _, cx| {
                    this.set_project_filter(None, cx)
                })
                .current(self.project_filter.is_none()),
            );
        for (index, computer) in self.computers.iter().enumerate() {
            menu = menu.child(
                div()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(computer.label.clone()),
            );
            let page = self.group_pages.hosts.get(&computer.key);
            for root in &self.computer_switcher_roots[index] {
                let target = root.target.clone();
                let own = target.computer.as_deref() == self.backend.remote_host();
                let group = root.row_group.clone();
                let mut item = MenuItem::new(
                    root.row_id.clone(),
                    root.name.clone(),
                    move |this: &mut Self, _, cx| {
                        if own {
                            this.set_project_filter(Some(target.path.clone()), cx);
                        } else {
                            cx.emit(SidebarEvent::Navigate(Navigate {
                                computer: target.computer.clone(),
                                destination: Destination::Project(target.path.clone()),
                            }));
                        }
                    },
                )
                .icon(if root.target.computer.is_some() {
                    "globe"
                } else {
                    "folder"
                })
                .current(own && self.project_filter.as_deref() == Some(root.root.as_str()))
                .style(move |row| row.group(group))
                .trailing(self.project_menu_trigger(root, window, cx));
                if let Some(field) = self.project_rename_field(&root.target) {
                    item = item.content(field);
                }
                menu = menu.item(item);
            }
            if let Some(page) = page {
                let loading = page.loading.contains("projects");
                let failed = page.errors.contains_key("projects");
                if page.next_projects.is_some() || loading || failed {
                    let action = ComputerMore {
                        computer: computer.key.clone(),
                        generation: self.group_pages.generation,
                        kind: ComputerPageKind::Projects,
                        cursor: page.next_projects.clone(),
                        label: "Load more projects".into(),
                        loading,
                    };
                    menu = menu.item(
                        MenuItem::new(
                            SharedString::from(format!("switcher-computer-{index}-more-projects")),
                            if loading {
                                "Loading projects…"
                            } else if failed {
                                "Could not load projects — retry"
                            } else {
                                "Load more projects"
                            },
                            move |this: &mut Self, _, cx| {
                                this.load_computer_page(action.clone(), cx)
                            },
                        )
                        .enabled(!loading),
                    );
                }
            }
            let request = Navigate {
                computer: computer.key.clone(),
                destination: Destination::NewProject,
            };
            menu = menu.item(
                MenuItem::new(
                    SharedString::from(format!("switcher-computer-{index}-new-project")),
                    "New project…",
                    move |_: &mut Self, _, cx| cx.emit(SidebarEvent::Navigate(request.clone())),
                )
                .icon("folder-plus"),
            );
        }
        menu
    }
}

#[cfg(test)]
mod paging_tests {
    use super::*;
    use crate::ui::chat::computers::Project;

    fn task(id: &str) -> TaskSummary {
        TaskSummary {
            uuid: id.into(),
            title: id.into(),
            project_root: "/tmp/project".into(),
            project_display_name: None,
            state: "active".into(),
            kind: "desktop".into(),
            pinned_ms: None,
            web_enabled: false,
            model: None,
            message_count: 1,
            created_ms: 1,
            updated_ms: 2,
            truncated: false,
        }
    }

    fn computer() -> Computer {
        Computer {
            key: None,
            label: "This computer".into(),
            backend: None,
            host_epoch: 0,
            project_names: HashMap::new(),
            projects: Vec::new(),
            sessions: vec![session_from_summary(&task("head"))],
            running: HashSet::new(),
            unread: HashSet::new(),
            pinned: Vec::new(),
            loading: false,
            error: None,
        }
    }

    fn test_sidebar(
        cx: &mut gpui::TestAppContext,
        backend: Arc<AgentBackend>,
        user: &str,
    ) -> Entity<Sidebar> {
        let chat = cx.new(|cx| ChatScreen::new_without_start(backend, user.into(), cx));
        chat.read_with(cx, |chat, _| chat.sidebar.clone())
    }

    fn seed_browse_pages(sidebar: &mut Sidebar) {
        let mut local = computer();
        local.backend = Some(sidebar.backend.clone());
        local.sessions.clear();
        let mut remote = local.clone();
        remote.key = Some("remote".into());
        remote.backend = Some(Arc::new(AgentBackend::for_remote_test("remote").unwrap()));
        remote.label = "Remote computer".into();
        sidebar.computers = Arc::new(vec![local.clone(), remote.clone()]);
        sidebar.pages.enabled = true;
        sidebar.group_pages.scope = Some((String::new(), None, false));
        sidebar.sessions = vec![session_from_summary(&task("local-cached"))];
        for (mut computer, id) in [(local, "local-cached"), (remote, "remote-cached")] {
            computer.sessions.push(session_from_summary(&task(id)));
            let mut page = snapshot_page(&computer, "", false);
            page.loaded.extend(["groups".into(), "projects".into()]);
            page.next_groups = Some(format!("next-{id}"));
            page.groups[0].more.active = Some(format!("active-{id}"));
            sidebar.group_pages.hosts.insert(computer.key, page);
        }
        sidebar.rebuild_sections();
    }

    fn visible_tasks(sidebar: &Sidebar) -> Vec<String> {
        sidebar
            .entries
            .iter()
            .filter_map(|entry| match entry {
                SidebarEntry::Task(task) => Some(sidebar.sessions[task.session].id.clone()),
                SidebarEntry::OtherTask(index) => {
                    let Destination::Task(id) = &sidebar.other_tasks[*index].request.destination
                    else {
                        return None;
                    };
                    Some(id.clone())
                }
                _ => None,
            })
            .collect()
    }

    fn group_reply_action(sidebar: &Sidebar) -> ComputerMore {
        ComputerMore {
            computer: None,
            generation: sidebar.group_pages.generation,
            kind: ComputerPageKind::Groups,
            cursor: None,
            label: "Loading tasks…".into(),
            loading: true,
        }
    }

    #[gpui::test]
    fn switching_sidebars_keeps_loaded_groups_and_own_rows(cx: &mut gpui::TestAppContext) {
        let _settings = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let first = test_sidebar(cx, backend, "user");
        first.update(cx, |sidebar, _| seed_browse_pages(sidebar));
        let computers = first.read_with(cx, |sidebar, _| sidebar.computers.clone());
        let remote_backend = computers[1].backend.clone().unwrap();
        let second = test_sidebar(cx, remote_backend, "user");
        second.update(cx, |sidebar, _| {
            sidebar.computers = computers;
            sidebar.pages.enabled = true;
            let mut latest = session_from_summary(&task("remote-cached"));
            latest.title = "Fresh owner title".into();
            latest.updated_ms += 1;
            latest.state = AgentTaskState::Settled;
            sidebar.sessions.push(latest);
        });
        for (from, to) in [
            (&first, &second),
            (&second, &first),
            (&first, &second),
            (&second, &first),
        ] {
            let state = from.read_with(cx, |sidebar, cx| sidebar.browse_state(cx));
            to.update(cx, |sidebar, cx| {
                let stale = group_reply_action(sidebar);
                sidebar.restore_browse_state(state, cx);
                sidebar.apply_computer_page(
                    stale,
                    Ok(ComputerPageReply::Groups(Default::default())),
                    cx,
                );
                assert_eq!(visible_tasks(sidebar), ["local-cached", "remote-cached"]);
                assert!(
                    !sidebar.pages.any_loading(),
                    "unchanged browsing must not reload own task pages"
                );
                let local = &sidebar.group_pages.hosts[&None];
                assert_eq!(local.next_groups.as_deref(), Some("next-local-cached"));
                assert!(local.loading.is_empty());
                if to.entity_id() == second.entity_id() {
                    let own = sidebar
                        .sessions
                        .iter()
                        .find(|session| session.id == "remote-cached")
                        .unwrap();
                    assert_eq!(own.title, "Fresh owner title");
                    assert_eq!(own.state, AgentTaskState::Settled);
                    assert!(sidebar.entries.iter().any(|entry| matches!(entry,
                        SidebarEntry::Task(task) if sidebar.sessions[task.session].id == "remote-cached")));
                }
            });
        }
    }

    #[gpui::test]
    fn refreshing_groups_preserves_rows_and_rejects_stale_replies(cx: &mut gpui::TestAppContext) {
        let _settings = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let sidebar = test_sidebar(cx, backend, "user");
        sidebar.update(cx, |sidebar, cx| {
            seed_browse_pages(sidebar);
            let browse = sidebar.browse_state(cx);
            sidebar.refresh_visible_pages_for_generation(0, cx);
            assert_eq!(visible_tasks(sidebar), ["local-cached", "remote-cached"]);
            let before_transfer = group_reply_action(sidebar);
            sidebar.restore_browse_state(browse, cx);
            assert!(sidebar.group_pages.hosts[&None].loading.contains("groups"));
            sidebar.apply_computer_page(
                before_transfer,
                Ok(ComputerPageReply::Groups(Default::default())),
                cx,
            );
            assert_eq!(visible_tasks(sidebar), ["local-cached", "remote-cached"]);
            let stale = group_reply_action(sidebar);
            let mut computers = sidebar.computers.as_ref().clone();
            computers[0].label = "Updated computer".into();
            sidebar.set_computers(Arc::new(computers), cx);
            assert_eq!(visible_tasks(sidebar), ["local-cached", "remote-cached"]);
            for page in sidebar.group_pages.hosts.values() {
                assert!(page.next_groups.is_none());
                assert!(page.groups[0].more.active.is_none());
                assert!(page.loading.contains("groups"));
            }
            sidebar.apply_computer_page(
                stale,
                Ok(ComputerPageReply::Groups(Default::default())),
                cx,
            );
            assert_eq!(visible_tasks(sidebar), ["local-cached", "remote-cached"]);
            // An empty current first page is authoritative, including removal.
            let current = group_reply_action(sidebar);
            sidebar.apply_computer_page(
                current,
                Ok(ComputerPageReply::Groups(Default::default())),
                cx,
            );
            assert_eq!(visible_tasks(sidebar), ["remote-cached"]);
        });
    }

    #[gpui::test]
    fn browse_cache_does_not_cross_accounts_owners_or_query_scopes(cx: &mut gpui::TestAppContext) {
        let _settings = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let replacement =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let source = test_sidebar(cx, backend.clone(), "user");
        let other_account = test_sidebar(cx, backend.clone(), "other-user");
        let destination = test_sidebar(cx, backend, "user");
        source.update(cx, |sidebar, _| seed_browse_pages(sidebar));
        let state = source.read_with(cx, |sidebar, cx| sidebar.browse_state(cx));
        other_account.update(cx, |sidebar, cx| {
            sidebar.restore_browse_state(state, cx);
            assert!(sidebar.group_pages.hosts.is_empty());
            assert!(sidebar.sessions.is_empty());
        });
        let mut computers = source.read_with(cx, |sidebar, _| sidebar.computers.as_ref().clone());
        computers[0].host_epoch += 1;
        destination.update(cx, |sidebar, _| {
            sidebar.computers = Arc::new(computers.clone())
        });
        let state = source.read_with(cx, |sidebar, cx| sidebar.browse_state(cx));
        destination.update(cx, |sidebar, cx| {
            sidebar.restore_browse_state(state, cx);
            assert!(sidebar.group_pages.hosts[&None].groups.is_empty());
            assert_eq!(visible_tasks(sidebar), ["remote-cached"]);
        });
        computers[0].backend = Some(replacement);
        computers.pop(); // Unpairing must not resurrect the removed host.
        destination.update(cx, |sidebar, _| sidebar.computers = Arc::new(computers));
        let state = source.read_with(cx, |sidebar, cx| sidebar.browse_state(cx));
        destination.update(cx, |sidebar, cx| {
            sidebar.restore_browse_state(state, cx);
            assert!(sidebar.group_pages.hosts.is_empty());
            assert!(sidebar.sessions.is_empty());
        });
        source.update(cx, |sidebar, cx| {
            sidebar.filter = "different query".into();
            sidebar.ensure_computer_pages(cx);
            assert!(
                sidebar
                    .group_pages
                    .hosts
                    .values()
                    .all(|page| page.groups.is_empty())
            );
            assert!(visible_tasks(sidebar).iter().all(|id| id != "local-cached"));
        });
    }

    #[test]
    fn loading_an_older_page_does_not_invalidate_group_cursors() {
        let mut computer = computer();
        let original = computer.clone();
        let before = computer_revision(std::slice::from_ref(&computer));
        let mut older = task("older");
        older.updated_ms = 1;
        computer.sessions.push(session_from_summary(&older));
        computer.projects.push(Project {
            path: "/tmp/old-project".into(),
            label: "Old project".into(),
        });
        assert_eq!(before, computer_revision(std::slice::from_ref(&computer)));
        assert!(!existing_snapshot_changed(
            std::slice::from_ref(&original),
            std::slice::from_ref(&computer)
        ));
        computer.sessions[0].updated_ms = 3;
        assert_ne!(before, computer_revision(std::slice::from_ref(&computer)));
        assert!(existing_snapshot_changed(
            std::slice::from_ref(&original),
            std::slice::from_ref(&computer)
        ));
    }

    #[test]
    fn removing_an_older_loaded_task_invalidates_grouped_rows() {
        let mut before = computer();
        let mut older = task("deleted");
        older.updated_ms = 1;
        before.sessions.push(session_from_summary(&older));
        let mut after = before.clone();
        after.sessions.retain(|task| task.id != "deleted");
        assert_eq!(
            computer_revision(std::slice::from_ref(&before)),
            computer_revision(std::slice::from_ref(&after))
        );
        assert!(existing_snapshot_changed(
            std::slice::from_ref(&before),
            std::slice::from_ref(&after)
        ));
    }

    #[test]
    fn duplicate_page_delivery_updates_rows_without_duplicating_them() {
        let mut rows = vec![task("same")];
        let mut renamed = task("same");
        renamed.title = "Renamed".into();
        merge_tasks(&mut rows, vec![renamed, task("next")]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "Renamed");
    }

    #[test]
    fn current_host_projection_excludes_other_hosts() {
        let mut pages = ComputerPages::default();
        pages.hosts.insert(
            None,
            ComputerHostPage {
                search: vec![task("own")],
                ..ComputerHostPage::default()
            },
        );
        pages.hosts.insert(
            Some("remote".into()),
            ComputerHostPage {
                search: vec![task("foreign")],
                ..ComputerHostPage::default()
            },
        );
        assert_eq!(pages.own_task_ids(None), HashSet::from(["own".into()]));
        assert_eq!(
            pages.own_task_ids(Some("remote")),
            HashSet::from(["foreign".into()])
        );
    }

    #[test]
    fn stale_load_more_actions_cannot_rewind_a_continuation() {
        let mut page = ComputerHostPage::default();
        assert!(page.cursor_is_current(&ComputerPageKind::Groups, &None));
        page.loaded.insert("groups".into());
        page.next_groups = Some("second-page".into());
        assert!(!page.cursor_is_current(&ComputerPageKind::Groups, &None));
        assert!(page.cursor_is_current(&ComputerPageKind::Groups, &Some("second-page".into())));
        page.next_groups = Some("third-page".into());
        assert!(!page.cursor_is_current(&ComputerPageKind::Groups, &Some("second-page".into())));
    }

    #[test]
    fn bounded_snapshot_keeps_running_empty_tasks_and_hides_idle_empty_tasks() {
        let mut computer = computer();
        computer.sessions[0].message_count = 0;
        assert!(snapshot_page(&computer, "", false).groups.is_empty());
        computer.running.insert("head".into());
        let page = snapshot_page(&computer, "", false);
        assert_eq!(page.groups.len(), 1);
        assert_eq!(page.groups[0].tasks[0].uuid, "head");
    }
}
