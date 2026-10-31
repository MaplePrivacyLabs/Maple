//! The sidebar's bounded projection across execution computers. Organization
//! changes presentation only; every project and task keeps its owning host.
use super::*;
use crate::backend::ProjectPageRequest;
use crate::ui::chat::computers::{Computer, Destination, Navigate};
use crate::ui::icons::spinner;
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
    revalidation_pending: bool,
}

impl ComputerPages {
    /// Manual order starts from every loaded summary, including folded groups.
    pub(super) fn task_order_seeds(&self) -> Vec<(SidebarItemKey, i64, i64, bool)> {
        self.hosts
            .iter()
            .flat_map(|(computer, page)| {
                page.groups
                    .iter()
                    .flat_map(|group| group.tasks.iter())
                    .chain(page.active.iter())
                    .chain(page.settled.iter())
                    .chain(page.search.iter())
                    .chain(page.archived.iter())
                    .map(|task| {
                        (
                            SidebarItemKey {
                                computer: computer.clone(),
                                id: task.uuid.clone(),
                            },
                            task.created_ms,
                            task.updated_ms,
                            task.pinned_ms.is_some(),
                        )
                    })
            })
            .collect()
    }

    fn manual_reads_pending(&self) -> bool {
        self.hosts.values().any(|page| {
            page.loading
                .iter()
                .any(|key| key != "archive-count" && key != "projects")
        })
    }

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
            .chain(page.active.iter())
            .chain(page.settled.iter())
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
    active: Vec<TaskSummary>,
    settled: Vec<TaskSummary>,
    search: Vec<TaskSummary>,
    archived: Vec<TaskSummary>,
    next_groups: Option<String>,
    next_active: Option<String>,
    next_settled: Option<String>,
    next_search: Option<String>,
    next_archived: Option<String>,
    next_projects: Option<String>,
    search_roots: Vec<String>,
    search_text: String,
    archived_count: u64,
    loading: HashSet<String>,
    errors: HashMap<String, String>,
    loaded: HashSet<String>,
    retained_heads: HashSet<String>,
}

impl ComputerHostPage {
    /// Keep the visible projection until a fresh first page replaces it, but
    /// never use continuation cursors from before the refresh.
    fn invalidate(&mut self) {
        self.next_groups = None;
        self.next_active = None;
        self.next_settled = None;
        self.next_search = None;
        self.next_archived = None;
        self.next_projects = None;
        for group in &mut self.groups {
            group.more = Default::default();
        }
        self.loading.clear();
        self.loaded.clear();
        self.errors.clear();
        self.retained_heads.clear();
    }

    fn cursor_is_current(&self, kind: &ComputerPageKind, cursor: &Option<String>) -> bool {
        let expected = match kind {
            ComputerPageKind::ArchiveCount => None,
            ComputerPageKind::Projects => self.next_projects.as_ref(),
            ComputerPageKind::Groups => self.next_groups.as_ref(),
            ComputerPageKind::Tasks { settled: false } => self.next_active.as_ref(),
            ComputerPageKind::Tasks { settled: true } => self.next_settled.as_ref(),
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
    ArchiveCount,
    Projects,
    Groups,
    Tasks { settled: bool },
    Search,
    Project { root: String, settled: bool },
    Archived,
}

impl ComputerPageKind {
    fn key(&self) -> String {
        match self {
            Self::ArchiveCount => "archive-count".into(),
            Self::Projects => "projects".into(),
            Self::Groups => "groups".into(),
            Self::Tasks { settled } => format!("tasks:{settled}"),
            Self::Search => "search".into(),
            Self::Project { root, settled } => format!("project:{settled}:{root}"),
            Self::Archived => "archived".into(),
        }
    }
}

enum ComputerPageReply {
    ArchiveCount(u64),
    Projects(NavigationProjectPage),
    Groups(NavigationGroupPage),
    Tasks(NavigationTaskPage),
}

async fn computer_task_page(
    reader: catalog::Reader,
    backend: Arc<AgentBackend>,
    user: String,
    query: catalog::TaskQuery,
    manual: Option<catalog::ManualOrder>,
) -> Result<NavigationTaskPage, String> {
    if let Some(order) = manual {
        return reader.page(backend, user, query, order).await;
    }
    match query {
        catalog::TaskQuery::Tasks(query) => backend.list_tasks(&user, query).await,
        catalog::TaskQuery::Search(query) => backend.search_tasks(&user, query).await,
        catalog::TaskQuery::Pinned(query) => backend.pinned_tasks(&user, query).await,
    }
}

/// Project headers keep their host cursor. Task prefixes and their independent
/// section continuations are projected from the desktop's complete inventory.
async fn manual_computer_groups(
    reader: catalog::Reader,
    backend: Arc<AgentBackend>,
    user: String,
    mut page: NavigationGroupPage,
    order: catalog::ManualOrder,
) -> Result<NavigationGroupPage, String> {
    let mut remaining = 200usize;
    for group in &mut page.groups {
        let budget = remaining.min(20) as u16;
        let active_query = NavigationTaskQuery {
            section: TaskSection::State(TaskState::Active),
            project: Some(group.project.root.clone()),
            pins: PinFilter::Ignore,
            cursor: None,
            limit: budget,
        };
        let settled_query = NavigationTaskQuery {
            section: TaskSection::State(TaskState::Settled),
            ..active_query.clone()
        };
        // At most two scans run concurrently for this execution computer.
        let (active, settled) = tokio::try_join!(
            reader.clone().page(
                backend.clone(),
                user.clone(),
                catalog::TaskQuery::Tasks(active_query.clone()),
                order.clone(),
            ),
            reader.clone().page(
                backend.clone(),
                user.clone(),
                catalog::TaskQuery::Tasks(settled_query.clone()),
                order.clone(),
            ),
        )?;
        let mut candidates = active
            .tasks
            .iter()
            .chain(settled.tasks.iter())
            .cloned()
            .collect::<Vec<_>>();
        order.sort(&mut candidates);
        candidates.truncate(usize::from(budget));
        let settled_count = candidates
            .iter()
            .filter(|task| task.state == "settled")
            .count();
        let active_count = candidates.len() - settled_count;
        group.tasks = candidates;
        remaining -= group.tasks.len();
        // Derive each continuation from this exact candidate snapshot. A
        // section contributing no rows keeps a local start cursor when needed.
        group.more.active = reader.prefix_cursor(
            &backend,
            &user,
            &catalog::TaskQuery::Tasks(active_query),
            &order,
            &active,
            active_count,
        )?;
        group.more.settled = reader.prefix_cursor(
            &backend,
            &user,
            &catalog::TaskQuery::Tasks(settled_query),
            &order,
            &settled,
            settled_count,
        )?;
    }
    Ok(page)
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
        computer.pinned.hash(&mut hash);
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
    page.groups = page
        .projects
        .iter()
        .cloned()
        .map(|project| NavigationProjectGroup {
            project,
            tasks: Vec::new(),
            more: Default::default(),
        })
        .collect();
    for session in &computer.sessions {
        if session.message_count == 0 && !computer.activity.running.contains(&session.id) {
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
        } else if state == "settled" {
            page.settled.push(task.clone());
        } else {
            page.active.push(task.clone());
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

pub(super) struct OtherTask {
    pub(super) target: Arc<SidebarItemKey>,
    pub(super) row: SidebarRow,
    pub(super) rung: TaskRung,
    pub(super) request: Navigate,
    pub(super) archived: bool,
    pub(super) session: AgentSessionSummary,
    pub(super) metadata: SharedString,
    id: SharedString,
    title: SharedString,
    activity: Option<SessionActivity>,
    pub(super) pinned: bool,
}

impl Sidebar {
    #[cfg(test)]
    pub(in crate::ui::chat) fn navigation_page_state_for_test(&self) -> (u64, usize, bool) {
        (
            self.group_pages.generation,
            self.group_pages
                .hosts
                .values()
                .map(|page| {
                    page.groups
                        .iter()
                        .map(|group| group.tasks.len())
                        .sum::<usize>()
                })
                .sum(),
            self.pages.any_loading()
                || self
                    .group_pages
                    .hosts
                    .values()
                    .any(|page| !page.loading.is_empty()),
        )
    }

    /// View fixtures supply bounded, verified owner snapshots rather than
    /// asynchronous pages from the fixtures' deliberately empty stores.
    #[cfg(test)]
    pub(crate) fn seed_computer_snapshot_pages_for_test(&mut self) {
        self.group_pages.generation = self.group_pages.generation.wrapping_add(1);
        self.manual_catalog.cancel_pending();
        self.group_pages.scope = Some((
            self.filter.clone(),
            self.project_filter.clone(),
            self.archived_expanded,
        ));
        self.group_pages.hosts = self
            .computers
            .iter()
            .map(|computer| {
                assert!(computer.backend.is_some(), "fixture needs a verified owner");
                let mut page = snapshot_page(computer, &self.filter, self.archived_expanded);
                page.loaded.extend(
                    [
                        "archive-count",
                        "projects",
                        "groups",
                        "tasks:false",
                        "tasks:true",
                        "search",
                        "archived",
                    ]
                    .into_iter()
                    .map(str::to_owned),
                );
                (computer.key.clone(), page)
            })
            .collect();
        self.rebuild_sections();
    }

    fn computer_task_pin(&self, computer: &Computer, task: &TaskSummary) -> Option<i64> {
        if computer.key.is_none() {
            return task.pinned_ms;
        }
        // Remote pins belong to the viewer, independently of pins in the
        // execution computer's store. Prefer this view's immediate edits.
        let pins = if computer.key.as_deref() == self.backend.remote_host() {
            &self.pinned_tasks
        } else {
            &computer.pinned
        };
        pins.iter()
            .position(|id| id == &task.uuid)
            .map(|rank| i64::MAX - i64::try_from(rank).unwrap_or(i64::MAX))
    }

    /// Saved pins are already hydrated by the owning sidebar's bounded GetTasks
    /// batches. Keep those verified summaries visible beyond the first page.
    fn computer_pinned_summaries(&self, computer: &Computer) -> Vec<TaskSummary> {
        let own = computer.key.as_deref() == self.backend.remote_host();
        let pins = if own {
            &self.pinned_tasks
        } else {
            &computer.pinned
        };
        let ranks = pins
            .iter()
            .enumerate()
            .map(|(rank, id)| (id.as_str(), rank))
            .collect::<HashMap<_, _>>();
        let mut tasks = HashMap::<String, TaskSummary>::new();
        for session in computer
            .sessions
            .iter()
            .chain(self.sessions.iter().filter(|_| own))
        {
            if self
                .project_filter
                .as_ref()
                .is_some_and(|root| !own || root != &session.project_root)
                || (own && self.removed_projects.contains(&session.project_root))
            {
                continue;
            }
            let Some(rank) = ranks.get(session.id.as_str()) else {
                continue;
            };
            if tasks
                .get(&session.id)
                .is_some_and(|task| task.updated_ms >= session.updated_ms)
            {
                continue;
            }
            let project = computer
                .projects
                .iter()
                .find(|project| project.path == session.project_root);
            let task = TaskSummary {
                uuid: session.id.clone(),
                title: session.title.clone(),
                project_root: session.project_root.clone(),
                project_display_name: project.map(|project| project.label.to_string()),
                state: match session.state {
                    AgentTaskState::Active => "active",
                    AgentTaskState::Settled => "settled",
                    AgentTaskState::Archived => "archived",
                }
                .into(),
                kind: if session.acp { "acp" } else { "desktop" }.into(),
                pinned_ms: Some(i64::MAX - i64::try_from(*rank).unwrap_or(i64::MAX)),
                web_enabled: session.web_enabled,
                model: session.model.clone(),
                message_count: u64::try_from(session.message_count).unwrap_or(u64::MAX),
                created_ms: session.created_ms,
                updated_ms: session.updated_ms,
                truncated: false,
            };
            let task = self.current_computer_task(computer, &task);
            if task.state != "archived"
                && (task.message_count > 0
                    || computer.activity.running.contains(&task.uuid)
                    || (own && self.activity.running.contains(&task.uuid)))
            {
                tasks.insert(task.uuid.clone(), task);
            }
        }
        tasks
            .into_values()
            .filter(|task| self.computer_task_matches(computer, task))
            .collect()
    }

    fn computer_task_matches(&self, computer: &Computer, task: &TaskSummary) -> bool {
        self.filter.is_empty()
            || task.title.to_lowercase().contains(&self.filter)
            || computer.label.to_lowercase().contains(&self.filter)
            || self
                .computer_project_name(
                    computer,
                    &ProjectSummary {
                        root: task.project_root.clone(),
                        display_name: task.project_display_name.clone(),
                        position: None,
                        last_used_ms: task.updated_ms,
                        name_revision: 0,
                        is_default_workspace: false,
                        truncated: false,
                    },
                )
                .to_lowercase()
                .contains(&self.filter)
    }

    pub(super) fn has_added_computer(&self) -> bool {
        self.computers.iter().any(|computer| computer.key.is_some())
    }

    pub(super) fn uses_computer_projection(&self) -> bool {
        // Project navigation reads the owning host's scoped task pages. The
        // cross-computer pages represent the unfiltered sidebar instead.
        self.project_filter.is_none() && self.has_added_computer()
    }

    pub(in crate::ui::chat) fn set_computers(
        &mut self,
        computers: Arc<Vec<Computer>>,
        cx: &mut Context<Self>,
    ) {
        let scope_changed = self.has_added_computer()
            && !computers.iter().any(|computer| computer.key.is_some())
            && self.project_filter.take().is_some();
        let changed = computer_revision(&self.computers) != computer_revision(&computers)
            || existing_snapshot_changed(&self.computers, &computers);
        let owners_changed = self.computers.len() != computers.len()
            || self.computers.iter().any(|old| {
                computers
                    .iter()
                    .find(|new| old.key == new.key)
                    .is_none_or(|new| !same_computer_owner(old, new))
            });
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
        if self.view.group == SidebarGroup::Connection && !self.connection_grouping_available() {
            self.view.group = SidebarGroup::Project;
            let view = self.view;
            crate::settings::update_settings_in_background(move |settings| {
                settings.sidebar_layout = view;
            });
        }
        if scope_changed && self.pages.enabled {
            self.refresh_pages(cx);
        } else if changed {
            if owners_changed {
                self.start_computer_revalidation(cx);
            } else {
                self.revalidate_computer_pages(cx);
            }
        } else {
            self.ensure_computer_pages(cx);
        }
        self.rebuild_sections();
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
        let mut inventory_changed = false;
        state.pages.hosts.retain(|key, page| {
            let owners = state
                .computers
                .iter()
                .find(|computer| &computer.key == key)
                .zip(self.computers.iter().find(|computer| &computer.key == key));
            let Some((old, new)) = owners.filter(|(old, new)| same_computer_owner(old, new)) else {
                inventory_changed = true;
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
                inventory_changed = true;
                page.invalidate();
            }
            true
        });
        self.manual_catalog.cancel_pending();
        if inventory_changed {
            self.manual_catalog.invalidate();
        }
        self.group_pages.revalidation_pending = false;
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
                    .chain(page.active.iter())
                    .chain(page.settled.iter())
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
        self.rebuild_computer_project_roots();
    }

    pub(super) fn revalidate_computer_pages(&mut self, cx: &mut Context<Self>) {
        // Let the current Manual snapshot finish. Repeated activity only asks
        // for one fresh trailing snapshot, rather than repeatedly cancelling
        // useful work or filling the catalog's two scan slots with stale work.
        if self.view.sort == SidebarSort::Manual && self.group_pages.manual_reads_pending() {
            self.group_pages.revalidation_pending = true;
            return;
        }
        self.start_computer_revalidation(cx);
    }

    fn finish_computer_revalidation(&mut self, cx: &mut Context<Self>) {
        if self.group_pages.revalidation_pending && !self.group_pages.manual_reads_pending() {
            self.start_computer_revalidation(cx);
        }
    }

    fn start_computer_revalidation(&mut self, cx: &mut Context<Self>) {
        self.group_pages.revalidation_pending = false;
        self.manual_catalog.cancel_pending();
        self.manual_catalog.invalidate();
        self.group_pages.generation = self.group_pages.generation.wrapping_add(1);
        for page in self.group_pages.hosts.values_mut() {
            page.invalidate();
        }
        self.ensure_computer_pages(cx);
    }

    pub(in crate::ui::chat) fn refresh_computer_pages(&mut self, cx: &mut Context<Self>) {
        self.manual_catalog.cancel_pending();
        self.group_pages.revalidation_pending = false;
        self.group_pages.generation = self.group_pages.generation.wrapping_add(1);
        // Ordering edits invalidate cursors, not the summaries already loaded
        // beyond page one. A scope/owner change still discards them separately.
        for page in self.group_pages.hosts.values_mut() {
            let mut retained = page.retained_heads.clone();
            retained.extend(page.loaded.iter().cloned());
            page.invalidate();
            page.retained_heads = retained;
        }
        self.group_pages.more.clear();
        self.ensure_computer_pages(cx);
    }

    pub(super) fn ensure_computer_pages(&mut self, cx: &mut Context<Self>) {
        if !self.uses_computer_projection() {
            return;
        }
        let scope = (
            self.filter.clone(),
            self.project_filter.clone(),
            self.archived_expanded,
        );
        if self.group_pages.scope.as_ref() != Some(&scope) {
            self.manual_catalog.cancel_pending();
            self.group_pages.revalidation_pending = false;
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
            if !page.loaded.contains("archive-count")
                && !page.loading.contains("archive-count")
                && !page.errors.contains_key("archive-count")
            {
                self.load_computer_page(
                    ComputerMore {
                        computer: computer.clone(),
                        generation: self.group_pages.generation,
                        kind: ComputerPageKind::ArchiveCount,
                        cursor: None,
                        label: "Loading archived task count…".into(),
                        loading: false,
                    },
                    cx,
                );
            }
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
            let kinds = if !self.filter.is_empty() {
                vec![ComputerPageKind::Search]
            } else if matches!(self.view.group, SidebarGroup::Status | SidebarGroup::List) {
                vec![
                    ComputerPageKind::Tasks { settled: false },
                    ComputerPageKind::Tasks { settled: true },
                ]
            } else {
                vec![ComputerPageKind::Groups]
            };
            for kind in kinds {
                let page = self.group_pages.hosts.entry(computer.clone()).or_default();
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
        if self.order_query_dirty {
            self.refresh_task_order(cx);
            return;
        }
        if action.generation != self.group_pages.generation {
            return;
        }
        let Some(computer) = self.computers.iter().find(|c| c.key == action.computer) else {
            return;
        };
        let Some(backend) = computer.backend.clone() else {
            return;
        };
        let manual = self.local_manual_order(&action.computer);
        let reader = self.manual_catalog.reader();
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
                    ComputerPageKind::ArchiveCount => backend
                        .list_project_groups(
                            &user,
                            NavigationGroupQuery {
                                limit_groups: 1,
                                per_project: 1,
                                cursor: None,
                            },
                        )
                        .await
                        .map(|page| ComputerPageReply::ArchiveCount(page.archived_count)),
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
                    ComputerPageKind::Groups => {
                        let groups = backend
                            .list_project_groups(
                                &user,
                                NavigationGroupQuery {
                                    limit_groups: 20,
                                    per_project: 20,
                                    cursor: request.cursor.clone(),
                                },
                            )
                            .await?;
                        let groups = if let Some(order) = manual {
                            manual_computer_groups(reader, backend, user, groups, order).await?
                        } else {
                            groups
                        };
                        Ok(ComputerPageReply::Groups(groups))
                    }
                    ComputerPageKind::Search => computer_task_page(
                        reader,
                        backend,
                        user,
                        catalog::TaskQuery::Search(NavigationSearchQuery {
                            text: search_text,
                            project,
                            project_roots: matched_roots,
                            cursor: request.cursor.clone(),
                            limit: 100,
                        }),
                        manual,
                    )
                    .await
                    .map(ComputerPageReply::Tasks),
                    ComputerPageKind::Tasks { settled }
                    | ComputerPageKind::Project { settled, .. } => computer_task_page(
                        reader,
                        backend,
                        user,
                        catalog::TaskQuery::Tasks(NavigationTaskQuery {
                            section: TaskSection::State(if *settled {
                                TaskState::Settled
                            } else {
                                TaskState::Active
                            }),
                            project: match &request.kind {
                                ComputerPageKind::Project { root, .. } => Some(root.clone()),
                                _ => project,
                            },
                            pins: PinFilter::Ignore,
                            cursor: request.cursor.clone(),
                            limit: 50,
                        }),
                        manual,
                    )
                    .await
                    .map(ComputerPageReply::Tasks),
                    ComputerPageKind::Archived => computer_task_page(
                        reader,
                        backend,
                        user,
                        catalog::TaskQuery::Tasks(NavigationTaskQuery {
                            section: TaskSection::State(TaskState::Archived),
                            project,
                            pins: PinFilter::Ignore,
                            cursor: request.cursor.clone(),
                            limit: 50,
                        }),
                        manual,
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
            Err(error) if error == catalog::CANCELLED => {
                self.load_computer_page(action, cx);
                return;
            }
            Err(error) => {
                page.errors.insert(key, error);
                self.finish_computer_revalidation(cx);
                self.rebuild_entries();
                cx.notify();
                return;
            }
        };
        let retain_head = action.cursor.is_none() && page.retained_heads.remove(&key);
        page.loaded.insert(key);
        let mut own_tasks = Vec::new();
        let mut own_labels = Vec::new();
        match result {
            ComputerPageReply::ArchiveCount(count) => page.archived_count = count,
            ComputerPageReply::Projects(reply) => {
                if action.cursor.is_none() && !retain_head {
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
                if action.cursor.is_none() && !retain_head {
                    page.groups.clear();
                }
                if !page.loaded.contains("archive-count") {
                    page.archived_count = reply.archived_count;
                }
                page.next_groups = reply.next_cursor;
                for group in reply.groups {
                    // Group replies are authoritative project metadata too.
                    // Install it before the independent metadata page arrives
                    // so every visible header immediately has its actions.
                    if let Some(existing) = page
                        .projects
                        .iter_mut()
                        .find(|project| project.root == group.project.root)
                    {
                        if existing.name_revision <= group.project.name_revision {
                            *existing = group.project.clone();
                        }
                    } else {
                        page.projects.push(group.project.clone());
                    }
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
                    ComputerPageKind::Tasks { settled } => {
                        let (tasks, next) = if settled {
                            (&mut page.settled, &mut page.next_settled)
                        } else {
                            (&mut page.active, &mut page.next_active)
                        };
                        if action.cursor.is_none() && !retain_head {
                            tasks.clear();
                        }
                        *next = reply.next_cursor;
                        merge_tasks(tasks, reply.tasks);
                    }
                    ComputerPageKind::Search => {
                        if action.cursor.is_none() && !retain_head {
                            page.search.clear();
                        }
                        page.next_search = reply.next_cursor;
                        merge_tasks(&mut page.search, reply.tasks);
                    }
                    ComputerPageKind::Archived => {
                        if action.cursor.is_none() && !retain_head {
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
                    ComputerPageKind::ArchiveCount
                    | ComputerPageKind::Groups
                    | ComputerPageKind::Projects => {}
                }
            }
        }
        if own {
            self.remember_task_project_labels(own_labels);
            self.merge_page_sessions(own_tasks);
        }
        self.load_missing_group_states(cx);
        self.finish_computer_revalidation(cx);
        self.rebuild_computer_project_roots();
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

    /// Resolve a summary against a newer verified live snapshot before it
    /// participates in ordering or status grouping.
    fn current_computer_task(&self, computer: &Computer, task: &TaskSummary) -> TaskSummary {
        let own = computer.key.as_deref() == self.backend.remote_host();
        let latest = computer
            .sessions
            .iter()
            .chain(self.sessions.iter().filter(|_| own))
            .filter(|session| session.id == task.uuid && session.updated_ms >= task.updated_ms)
            .max_by_key(|session| session.updated_ms);
        let mut task = task.clone();
        if let Some(session) = latest {
            task.title.clone_from(&session.title);
            task.created_ms = session.created_ms;
            task.updated_ms = session.updated_ms;
            task.message_count = u64::try_from(session.message_count).unwrap_or(u64::MAX);
            task.state = match session.state {
                AgentTaskState::Active => "active",
                AgentTaskState::Settled => "settled",
                AgentTaskState::Archived => "archived",
            }
            .into();
        }
        if computer.activity.running.contains(&task.uuid)
            || (own && self.activity.running.contains(&task.uuid))
        {
            task.state = "active".into();
        }
        task.pinned_ms = self.computer_task_pin(computer, &task);
        task
    }

    fn computer_project_groups(
        &self,
        computer: &Computer,
        page: &ComputerHostPage,
    ) -> Vec<NavigationProjectGroup> {
        let searching = !self.filter.is_empty();
        let own = computer.key.as_deref() == self.backend.remote_host();
        let mut groups = if searching {
            Vec::new()
        } else {
            page.groups.clone()
        };
        // Project metadata is independent of tasks. Keep empty projects visible
        // even when their task page has not arrived, without claiming it empty.
        for project in &page.projects {
            let matches = !searching
                || computer.label.to_lowercase().contains(&self.filter)
                || self
                    .computer_project_name(computer, project)
                    .to_lowercase()
                    .contains(&self.filter);
            if matches
                && !groups
                    .iter()
                    .any(|group| group.project.root == project.root)
            {
                groups.push(NavigationProjectGroup {
                    project: project.clone(),
                    tasks: Vec::new(),
                    more: Default::default(),
                });
            }
        }
        if searching {
            for summary in &page.search {
                let task = self.current_computer_task(computer, summary);
                if task.state == "archived" {
                    continue;
                }
                if let Some(group) = groups
                    .iter_mut()
                    .find(|group| group.project.root == task.project_root)
                {
                    group.tasks.push(task);
                } else {
                    groups.push(NavigationProjectGroup {
                        project: ProjectSummary {
                            root: task.project_root.clone(),
                            display_name: task.project_display_name.clone(),
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
        } else {
            for group in &mut groups {
                group.tasks = group
                    .tasks
                    .iter()
                    .map(|task| self.current_computer_task(computer, task))
                    .filter(|task| task.state != "archived")
                    .collect();
            }
        }
        for task in self.computer_pinned_summaries(computer) {
            if let Some(group) = groups
                .iter_mut()
                .find(|group| group.project.root == task.project_root)
            {
                if !group
                    .tasks
                    .iter()
                    .any(|existing| existing.uuid == task.uuid)
                {
                    group.tasks.push(task);
                }
            } else {
                groups.push(NavigationProjectGroup {
                    project: page
                        .projects
                        .iter()
                        .find(|project| project.root == task.project_root)
                        .cloned()
                        .unwrap_or_else(|| ProjectSummary {
                            root: task.project_root.clone(),
                            display_name: task.project_display_name.clone(),
                            position: None,
                            last_used_ms: task.updated_ms,
                            name_revision: 0,
                            is_default_workspace: false,
                            truncated: false,
                        }),
                    tasks: vec![task],
                    more: Default::default(),
                });
            }
        }
        groups.retain(|group| {
            (!own || !self.removed_projects.contains(&group.project.root))
                && self
                    .project_filter
                    .as_ref()
                    .is_none_or(|root| own && root == &group.project.root)
        });
        groups
    }

    fn compare_computer_tasks(
        &self,
        left: &Option<String>,
        a: &TaskSummary,
        right: &Option<String>,
        b: &TaskSummary,
    ) -> std::cmp::Ordering {
        let key = |computer, task: &TaskSummary| {
            if task.state != "archived" && task.pinned_ms.is_some() {
                self.pinned_task_order_key(computer, &task.uuid, task.created_ms, task.updated_ms)
            } else {
                self.task_order_key(computer, &task.uuid, task.created_ms, task.updated_ms)
            }
        };
        key(left, a)
            .cmp(&key(right, b))
            .then_with(|| left.cmp(right))
            .then_with(|| a.uuid.cmp(&b.uuid))
    }

    fn sort_computer_tasks(&self, tasks: &mut [(usize, TaskSummary)]) {
        tasks.sort_by(|(left, a), (right, b)| {
            self.compare_computer_tasks(
                &self.computers[*left].key,
                a,
                &self.computers[*right].key,
                b,
            )
        });
    }

    fn computer_group_prefix(
        &self,
        computer: &Computer,
        group: &NavigationProjectGroup,
    ) -> prefix::Prefix<bool> {
        let page = self.group_pages.hosts.get(&computer.key);
        // The continuation belongs to the received snapshot, before live state
        // changes or Pinned extraction alter its presentation.
        let source = page
            .and_then(|page| {
                page.groups
                    .iter()
                    .find(|stored| stored.project.root == group.project.root)
            })
            .map_or(group.tasks.as_slice(), |stored| stored.tasks.as_slice());
        prefix::Prefix::new(
            [false, true].map(|settled| {
                let key = ComputerPageKind::Project {
                    root: group.project.root.clone(),
                    settled,
                }
                .key();
                prefix::Stream {
                    source: settled,
                    end: source
                        .iter()
                        .filter(|task| (task.state == "settled") == settled)
                        .map(|task| {
                            self.task_prefix_position(
                                &computer.key,
                                &task.uuid,
                                task.created_ms,
                                task.updated_ms,
                            )
                        })
                        .max(),
                    complete: (if settled {
                        group.more.settled.is_none()
                    } else {
                        group.more.active.is_none()
                    }) && page.is_none_or(|page| {
                        !page.loading.contains(&key) && !page.errors.contains_key(&key)
                    }),
                }
            }),
            self.view.sort == SidebarSort::Manual,
        )
    }

    fn computer_flat_prefix(
        &self,
        hosts: &[usize],
        state: Option<bool>,
    ) -> prefix::Prefix<(usize, bool)> {
        let streams = hosts.iter().flat_map(|&index| {
            let computer = &self.computers[index];
            let page = self.group_pages.hosts.get(&computer.key);
            [false, true]
                .into_iter()
                .filter(move |settled| {
                    state.is_none_or(|state| state == *settled) && computer.backend.is_some()
                })
                .map(move |settled| {
                    let kind = ComputerPageKind::Tasks { settled };
                    let key = kind.key();
                    let tasks = page.map(|page| if settled { &page.settled } else { &page.active });
                    prefix::Stream {
                        source: (index, settled),
                        end: tasks
                            .into_iter()
                            .flatten()
                            .map(|task| {
                                self.task_prefix_position(
                                    &computer.key,
                                    &task.uuid,
                                    task.created_ms,
                                    task.updated_ms,
                                )
                            })
                            .max(),
                        complete: page.is_some_and(|page| {
                            !page.loading.contains(&key)
                                && !page.errors.contains_key(&key)
                                && if settled {
                                    page.next_settled.is_none()
                                } else {
                                    page.next_active.is_none()
                                }
                        }),
                    }
                })
        });
        prefix::Prefix::new(streams, self.view.sort == SidebarSort::Manual)
    }

    fn prefix_exempt_task(&self, computer: &Computer, task: &TaskSummary) -> bool {
        task.pinned_ms.is_some()
            || computer.activity.running.contains(&task.uuid)
            || (computer.key.as_deref() == self.backend.remote_host()
                && (self.selected.as_ref() == Some(&task.uuid)
                    || self.activity.running.contains(&task.uuid)
                    || self.pages.is_live(&task.uuid)))
    }

    /// A group first page can spend its budget on just Active tasks. Read the
    /// missing Settled prefix before presenting a merged list; zero-budget
    /// groups retain their explicit More action instead of causing background
    /// scans of projects past the visible row budget.
    fn load_missing_group_states(&mut self, cx: &mut Context<Self>) {
        if !self.filter.is_empty()
            || matches!(self.view.group, SidebarGroup::Status | SidebarGroup::List)
        {
            return;
        }
        let mut actions = Vec::new();
        for computer in self
            .computers
            .iter()
            .filter(|computer| computer.backend.is_some())
        {
            let Some(page) = self.group_pages.hosts.get(&computer.key) else {
                continue;
            };
            for group in page.groups.iter().filter(|group| !group.tasks.is_empty()) {
                for settled in [false, true] {
                    let cursor = if settled {
                        &group.more.settled
                    } else {
                        &group.more.active
                    };
                    let kind = ComputerPageKind::Project {
                        root: group.project.root.clone(),
                        settled,
                    };
                    if cursor.is_some()
                        && !page.loading.contains(&kind.key())
                        && !page.errors.contains_key(&kind.key())
                        && !group
                            .tasks
                            .iter()
                            .any(|task| (task.state == "settled") == settled)
                    {
                        actions.push(ComputerMore {
                            computer: computer.key.clone(),
                            generation: self.group_pages.generation,
                            kind,
                            cursor: cursor.clone(),
                            label: "Loading tasks…".into(),
                            loading: false,
                        });
                    }
                }
            }
        }
        for action in actions {
            self.load_computer_page(action, cx);
        }
    }

    fn push_computer_group(
        &mut self,
        entries: &mut Vec<SidebarEntry>,
        computer: &Computer,
        mut group: NavigationProjectGroup,
    ) {
        let target = ProjectTarget {
            computer: computer.key.clone(),
            path: group.project.root.clone(),
        };
        self.push_computer_heading(entries, computer, &group.project);
        if self.collapsed_projects.contains(&target) {
            return;
        }
        let prefix = self.computer_group_prefix(computer, &group);
        if self.filter.is_empty() {
            group.tasks.retain(|task| {
                self.prefix_exempt_task(computer, task)
                    || prefix.allows(&self.task_prefix_position(
                        &computer.key,
                        &task.uuid,
                        task.created_ms,
                        task.updated_ms,
                    ))
            });
        }
        group
            .tasks
            .sort_by(|a, b| self.compare_computer_tasks(&computer.key, a, &computer.key, b));
        for task in &group.tasks {
            self.push_computer_task(entries, computer, task);
        }
        if self.filter.is_empty() {
            for (settled, cursor) in [(false, group.more.active), (true, group.more.settled)] {
                if prefix.next.first() != Some(&settled) {
                    continue;
                }
                self.computer_more_entry(
                    entries,
                    &computer.key,
                    ComputerPageKind::Project {
                        root: target.path.clone(),
                        settled,
                    },
                    cursor,
                    "more tasks",
                );
            }
        }
    }

    fn push_computer_flat_more(
        &mut self,
        entries: &mut Vec<SidebarEntry>,
        computer_index: usize,
        settled: Option<bool>,
    ) {
        let computer = self.computers[computer_index].clone();
        let Some(page) = self.group_pages.hosts.get(&computer.key) else {
            return;
        };
        let (kind, cursor) = if !self.filter.is_empty() {
            (ComputerPageKind::Search, page.next_search.clone())
        } else if let Some(settled) = settled {
            (
                ComputerPageKind::Tasks { settled },
                if settled {
                    page.next_settled.clone()
                } else {
                    page.next_active.clone()
                },
            )
        } else {
            (ComputerPageKind::Groups, page.next_groups.clone())
        };
        self.computer_more_entry(
            entries,
            &computer.key,
            kind,
            cursor,
            &format!("more from {}", computer.label),
        );
    }

    pub(super) fn computer_entries(&mut self) -> Vec<SidebarEntry> {
        self.computer_projects.clear();
        self.other_tasks.clear();
        self.group_pages.more.clear();
        self.computer_archived_count = 0;
        let mut entries = vec![SidebarEntry::NewTask];
        if self.search_visible {
            entries.push(SidebarEntry::Search);
        }
        // A mounted view projects only bounded, owner-fenced summary pages.
        // Drafts, transcripts and execution remain in their owning ChatScreen.
        let computers = self.computers.clone();
        let mut hosts = (0..computers.len())
            .filter(|&index| {
                self.project_filter.is_none()
                    || computers[index].key.as_deref() == self.backend.remote_host()
            })
            .collect::<Vec<_>>();
        hosts.sort_by_cached_key(|&index| {
            let computer = &computers[index].key;
            (
                self.order
                    .connections
                    .iter()
                    .position(|saved| saved == computer)
                    .unwrap_or(usize::MAX),
                computer.clone(),
            )
        });
        let mut groups = Vec::new();
        let mut tasks = Vec::new();
        let mut archived = Vec::new();
        let mut unavailable = Vec::new();
        let searching = !self.filter.is_empty();
        let flat = matches!(self.view.group, SidebarGroup::Status | SidebarGroup::List);
        for &index in &hosts {
            let computer = &computers[index];
            let Some(page) = self.group_pages.hosts.get(&computer.key) else {
                unavailable.push(index);
                continue;
            };
            if computer.backend.is_none() || computer.error.is_some() {
                unavailable.push(index);
            }
            self.computer_archived_count = self
                .computer_archived_count
                .saturating_add(usize::try_from(page.archived_count).unwrap_or(usize::MAX));
            // A disconnected attachment has no verified owner handle. Do not
            // promote its snapshot into actionable task/project rows.
            if computer.backend.is_none() {
                continue;
            }
            if flat {
                let pins = self.computer_pinned_summaries(computer);
                let source = if searching {
                    page.search.iter().chain(pins.iter()).collect::<Vec<_>>()
                } else {
                    page.active
                        .iter()
                        .chain(&page.settled)
                        .chain(pins.iter())
                        .collect::<Vec<_>>()
                };
                let mut seen = HashSet::new();
                for summary in source {
                    let task = self.current_computer_task(computer, summary);
                    if task.state != "archived" && seen.insert(task.uuid.clone()) {
                        tasks.push((index, task));
                    }
                }
            } else {
                groups.extend(
                    self.computer_project_groups(computer, page)
                        .into_iter()
                        .map(|group| (index, group)),
                );
            }
            if self.archived_expanded || searching {
                let source = if searching {
                    &page.search
                } else {
                    &page.archived
                };
                for summary in source {
                    let task = self.current_computer_task(computer, summary);
                    if task.state == "archived" {
                        archived.push((index, task));
                    }
                }
            }
        }
        if searching {
            self.computer_archived_count = archived.len();
        }
        let flat_prefix = self.computer_flat_prefix(&hosts, None);
        let active_prefix = self.computer_flat_prefix(&hosts, Some(false));
        let settled_prefix = self.computer_flat_prefix(&hosts, Some(true));
        if flat && !searching {
            tasks.retain(|(index, task)| {
                let computer = &computers[*index];
                let prefix = if self.view.group == SidebarGroup::Status {
                    if task.state == "settled" {
                        &settled_prefix
                    } else {
                        &active_prefix
                    }
                } else {
                    &flat_prefix
                };
                self.prefix_exempt_task(computer, task)
                    || prefix.allows(&self.task_prefix_position(
                        &computer.key,
                        &task.uuid,
                        task.created_ms,
                        task.updated_ms,
                    ))
            });
        }
        // Pins are a desktop view across computers, above every grouping.
        // Keep the task's original project and saved order for unpinning.
        let mut pinned = Vec::new();
        let mut seen_pins = HashSet::new();
        let mut take_pin = |index: usize, task: &TaskSummary| {
            if task.pinned_ms.is_none() {
                return true;
            }
            if seen_pins.insert((index, task.uuid.clone())) {
                pinned.push((index, task.clone()));
            }
            false
        };
        for (index, group) in &mut groups {
            group.tasks.retain(|task| take_pin(*index, task));
        }
        tasks.retain(|(index, task)| take_pin(*index, task));
        self.sort_computer_tasks(&mut pinned);
        let has_pins = !pinned.is_empty();
        if has_pins {
            entries.push(SidebarEntry::SectionLabel(SidebarSection::Pinned));
            if self.section_expanded(SidebarSection::Pinned) {
                for (index, task) in &pinned {
                    self.push_computer_task(&mut entries, &computers[*index], task);
                }
            }
        }
        entries.push(SidebarEntry::ProjectsHeader);
        let group_start = entries.len();
        groups.sort_by_cached_key(|(index, group)| {
            let target = ProjectTarget {
                computer: computers[*index].key.clone(),
                path: group.project.root.clone(),
            };
            (
                self.project_order_rank(&target),
                target.computer,
                group.project.position.unwrap_or(i64::MAX),
                target.path,
            )
        });
        self.sort_computer_tasks(&mut tasks);
        self.sort_computer_tasks(&mut archived);
        if self.projects_expanded {
            match self.view.group {
                SidebarGroup::Project => {
                    for (index, group) in groups {
                        self.push_computer_group(&mut entries, &computers[index], group);
                    }
                    for &index in &hosts {
                        self.push_computer_flat_more(&mut entries, index, None);
                    }
                    entries.extend(unavailable.iter().copied().map(SidebarEntry::Computer));
                }
                SidebarGroup::Connection => {
                    for &index in &hosts {
                        entries.push(SidebarEntry::Computer(index));
                        if self.collapsed_computers.contains(&computers[index].key) {
                            continue;
                        }
                        for (owner, group) in groups.iter().filter(|(owner, _)| *owner == index) {
                            self.push_computer_group(
                                &mut entries,
                                &computers[*owner],
                                group.clone(),
                            );
                        }
                        self.push_computer_flat_more(&mut entries, index, None);
                    }
                }
                SidebarGroup::Status => {
                    for (section, settled) in [
                        (SidebarSection::Active, false),
                        (SidebarSection::Settled, true),
                    ] {
                        let visible = tasks
                            .iter()
                            .filter(|(_, task)| (task.state == "settled") == settled);
                        if visible.clone().next().is_some() {
                            entries.push(SidebarEntry::SectionLabel(section));
                        }
                        if !self.section_expanded(section) {
                            continue;
                        }
                        for (index, task) in visible {
                            self.push_computer_task(&mut entries, &computers[*index], task);
                        }
                        if !searching {
                            let prefix = if settled {
                                &settled_prefix
                            } else {
                                &active_prefix
                            };
                            if let Some(&(index, state)) = prefix.next.first() {
                                self.push_computer_flat_more(&mut entries, index, Some(state));
                            }
                        }
                    }
                    if searching {
                        for &index in &hosts {
                            self.push_computer_flat_more(&mut entries, index, None);
                        }
                    }
                    entries.extend(unavailable.iter().copied().map(SidebarEntry::Computer));
                }
                SidebarGroup::List => {
                    for (index, task) in &tasks {
                        self.push_computer_task(&mut entries, &computers[*index], task);
                    }
                    if searching {
                        for &index in &hosts {
                            self.push_computer_flat_more(&mut entries, index, None);
                        }
                    } else if let Some(&(index, state)) = flat_prefix.next.first() {
                        self.push_computer_flat_more(&mut entries, index, Some(state));
                    }
                    entries.extend(unavailable.iter().copied().map(SidebarEntry::Computer));
                }
            }
        }
        let has_archived = !archived.is_empty() || (!searching && self.computer_archived_count > 0);
        if has_archived {
            entries.push(SidebarEntry::ArchivedHeader);
        }
        for &index in &hosts {
            let computer = &computers[index];
            if self
                .group_pages
                .hosts
                .get(&computer.key)
                .is_some_and(|page| page.errors.contains_key("archive-count"))
            {
                self.computer_more_entry(
                    &mut entries,
                    &computer.key,
                    ComputerPageKind::ArchiveCount,
                    None,
                    &format!("archived task count from {}", computer.label),
                );
            }
        }
        if has_archived && self.archived_expanded {
            for (index, task) in archived {
                self.push_computer_task(&mut entries, &computers[index], &task);
            }
            if !searching {
                for &index in &hosts {
                    let computer = &computers[index];
                    let next = self
                        .group_pages
                        .hosts
                        .get(&computer.key)
                        .and_then(|page| page.next_archived.clone());
                    self.computer_more_entry(
                        &mut entries,
                        &computer.key,
                        ComputerPageKind::Archived,
                        next,
                        &format!("more archived tasks from {}", computer.label),
                    );
                }
            }
        }
        if searching && entries.len() == group_start && !has_pins && self.projects_expanded {
            entries.push(SidebarEntry::Empty(SidebarEmpty::NoMatches));
        }
        entries
    }

    fn push_computer_heading(
        &mut self,
        entries: &mut Vec<SidebarEntry>,
        computer: &Computer,
        project: &ProjectSummary,
    ) {
        let heading = self.computer_projects.len();
        let root = &project.root;
        let label = self.computer_project_name(computer, project);
        let target = ProjectTarget {
            computer: computer.key.clone(),
            path: root.clone(),
        };
        self.computer_projects
            .push(ProjectRow::new(target, label.into()));
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
                    pinned: self.computer_task_pin(computer, task).is_some(),
                    settled: state == AgentTaskState::Settled
                        && !self.activity.running.contains(&task.uuid),
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
            // Rename, delete and the hover step read the newest summary too.
            let session = latest
                .cloned()
                .unwrap_or_else(|| session_from_summary(task));
            let pinned = self.computer_task_pin(computer, task).is_some();
            let metadata = if self.show_task_metadata(pinned, archived) {
                one_line(&format!(
                    "{} · {}",
                    self.computer_project_name(
                        computer,
                        &ProjectSummary {
                            root: task.project_root.clone(),
                            display_name: task.project_display_name.clone(),
                            position: None,
                            last_used_ms: task.updated_ms,
                            name_revision: 0,
                            is_default_workspace: false,
                            truncated: false,
                        }
                    ),
                    computer.label,
                ))
            } else {
                SharedString::default()
            };
            let id: SharedString = format!("computer-task-{:?}-{}", computer.key, task.uuid).into();
            let mut row = SidebarRow::build_scoped(&session, "", "", &id);
            row.title =
                one_line(latest.map_or(task.title.as_str(), |session| session.title.as_str()));
            row.metadata = metadata.clone();
            row.group = id.clone();
            row.element_id = id.clone();
            let rung = if archived {
                TaskRung::Archived
            } else if session.state == AgentTaskState::Settled
                && !computer.activity.running.contains(&task.uuid)
            {
                TaskRung::Settled
            } else {
                TaskRung::Active
            };
            self.other_tasks.push(OtherTask {
                target: Arc::new(SidebarItemKey {
                    computer: computer.key.clone(),
                    id: task.uuid.clone(),
                }),
                row,
                rung,
                archived,
                session,
                metadata,
                request: Navigate {
                    computer: computer.key.clone(),
                    destination: Destination::Task(task.uuid.clone()),
                },
                id,
                title: one_line(
                    latest.map_or(task.title.as_str(), |session| session.title.as_str()),
                ),
                pinned,
                activity: computer.activity.of(&task.uuid),
            });
            entries.push(SidebarEntry::OtherTask(index));
        }
    }

    /// The title on another computer's task row.
    #[cfg(test)]
    pub(in crate::ui::chat) fn other_task_title(&self, index: usize) -> Option<&str> {
        Some(self.other_tasks.get(index)?.title.as_ref())
    }

    /// What another computer's task row indicates.
    #[cfg(test)]
    pub(in crate::ui::chat) fn other_task_activity(&self, index: usize) -> Option<SessionActivity> {
        self.other_tasks.get(index)?.activity
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
                let target = &self.computer_projects.get(index)?.target;
                Navigate {
                    computer: target.computer.clone(),
                    destination: Destination::Project(target.path.clone()),
                }
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

    pub(super) fn computer_more_scope(
        &self,
        index: usize,
    ) -> Option<(Option<String>, Option<String>)> {
        let action = self.group_pages.more.get(index)?;
        let project = match &action.kind {
            ComputerPageKind::Project { root, .. } => Some(root.clone()),
            _ => None,
        };
        Some((action.computer.clone(), project))
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

    pub(in crate::ui::chat) fn toggle_computer_group(
        &mut self,
        request: &Navigate,
        cx: &mut Context<Self>,
    ) -> bool {
        match &request.destination {
            Destination::Project(path) => {
                let target = ProjectTarget {
                    computer: request.computer.clone(),
                    path: path.clone(),
                };
                if !self.collapsed_projects.remove(&target) {
                    self.collapsed_projects.insert(target);
                }
            }
            Destination::Overview if self.view.group == SidebarGroup::Connection => {
                if !self.collapsed_computers.remove(&request.computer) {
                    self.collapsed_computers.insert(request.computer.clone());
                }
            }
            _ => return false,
        }
        self.rebuild_entries();
        cx.notify();
        true
    }

    pub(super) fn render_computer_entry(
        &self,
        entry: SidebarEntry,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hover_bg = if selected {
            theme::bg_sidebar_row_selected_hover()
        } else {
            theme::bg_sidebar_row_hover()
        };
        let dragging = cx.has_active_drag();
        match entry {
            SidebarEntry::ComputerMore(index) => {
                let action = self.group_pages.more[index].clone();
                let label = action.label.clone();
                let loading = action.loading;
                div()
                    .id(SharedString::from(format!("computer-page-{index}")))
                    .role(gpui::Role::Button)
                    .aria_label(label.clone())
                    .mx_2()
                    .my(px(1.))
                    .px_6()
                    .py_2()
                    .rounded(ROW_RADIUS)
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .when(selected, |row| {
                        row.aria_active_descendant()
                            .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                    })
                    .when(!loading, |row| {
                        row.hover(|s| s.cursor_pointer().bg(gpui::rgb(hover_bg)))
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
                self.render_project_header(&self.computer_projects[index], window, cx)
            }
            SidebarEntry::OtherTask(index) => {
                let task = &self.other_tasks[index];
                let row = &task.row;
                let request = task.request.clone();
                let rename_field = self.other_task_rename_field(&task.target, cx);
                let renaming = rename_field.is_some();
                let pin_target = TaskTarget::Other(task.target.clone());
                let row_bg = if selected {
                    theme::bg_sidebar_row_hover()
                } else {
                    theme::bg_sidebar()
                };
                let hover_group = (!renaming && !dragging).then_some(&row.group);
                div()
                    .relative()
                    .id(task.id.clone())
                    .debug_selector(|| task.id.to_string())
                    .group(task.id.clone())
                    .role(gpui::Role::ListBoxOption)
                    .aria_label(task.title.clone())
                    .aria_selected(false)
                    .mx_2()
                    .my(px(1.))
                    .pl(task_row_padding(window))
                    .pr_4()
                    .py_1()
                    .flex()
                    .gap_1p5()
                    .items_center()
                    .rounded(ROW_RADIUS)
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_sidebar()))
                    .when(selected, |row| {
                        row.aria_active_descendant()
                            .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                    })
                    .hover(move |s| {
                        if dragging || renaming {
                            s
                        } else {
                            s.bg(gpui::rgb(hover_bg)).cursor_pointer()
                        }
                    })
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(SidebarEvent::Navigate(request.clone()))
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .map(|column| match rename_field {
                                Some(field) => column.child(field),
                                None => column.child(row_line(
                                    task.title.clone(),
                                    hover_group,
                                    row_bg,
                                    hover_bg,
                                )),
                            })
                            .when(!task.metadata.is_empty(), |column| {
                                column.child(
                                    row_line(task.metadata.clone(), hover_group, row_bg, hover_bg)
                                        .text_xs()
                                        .text_color(gpui::rgb(theme::text_secondary())),
                                )
                            }),
                    )
                    .when_some(task.activity, |row, activity| {
                        row.child(activity_indicator(&task.id, activity))
                    })
                    .when(task.pinned, |row| {
                        row.child(
                            row_button(
                                task.row.pin_id.clone(),
                                &task.row.group,
                                "pin",
                                "Unpin task",
                            )
                            .opacity(1.)
                            .tooltip(widgets::tooltip("Unpin", None))
                            .on_click(cx.listener(
                                move |this, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.toggle_task_target_pin(&pin_target, true, cx);
                                },
                            )),
                        )
                    })
                    .when(!renaming && !dragging, |element| {
                        element.child(self.task_hover_actions(
                            row,
                            TaskTarget::Other(task.target.clone()),
                            (task.pinned, task.rung),
                            hover_bg,
                            window,
                            cx,
                        ))
                    })
                    .into_any_element()
            }
            SidebarEntry::Computer(index) => {
                let computer = &self.computers[index];
                let collapsible = self.view.group == SidebarGroup::Connection;
                let expanded = !self.collapsed_computers.contains(&computer.key);
                let status = computer.error.clone().or_else(|| {
                    if computer.loading {
                        None
                    } else if computer.backend.is_none() {
                        Some("Offline".into())
                    } else if computer.projects.is_empty() {
                        Some("No projects yet".into())
                    } else {
                        None
                    }
                });
                let request = Navigate {
                    computer: computer.key.clone(),
                    destination: Destination::Overview,
                };
                let action_group: SharedString =
                    format!("computer-header-actions-{:?}", computer.key).into();
                let new_project = Navigate {
                    computer: computer.key.clone(),
                    destination: Destination::NewProject,
                };
                let new_project_label: SharedString =
                    format!("New project on {}", computer.label).into();
                div()
                    .id(SharedString::from(format!("computer-{:?}", computer.key)))
                    .group(action_group.clone())
                    .role(gpui::Role::Button)
                    .aria_label(computer.label.clone())
                    .when(collapsible, |row| row.aria_expanded(expanded))
                    .mx_2()
                    .my(px(1.))
                    .mt_3()
                    .px(FOLDER_HEADER_PADDING - gpui::rems(0.5))
                    .py_2()
                    .rounded(ROW_RADIUS)
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .hover(move |s| {
                        if dragging {
                            s
                        } else {
                            s.bg(gpui::rgb(hover_bg)).cursor_pointer()
                        }
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.toggle_computer_group(&request, cx) {
                            cx.emit(SidebarEvent::Navigate(request.clone()));
                        } else {
                            this.hover_heading(SidebarEntry::Computer(index), cx);
                        }
                    }))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap(FOLDER_HEADER_GAP)
                            .child(icon("monitor", FOLDER_ICON_SIZE, theme::text_muted()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(div().min_w_0().truncate().child(computer.label.clone()))
                                    .when(collapsible, |row| {
                                        row.child(self.section_chevron(
                                            expanded,
                                            "computer-disclosure",
                                            SidebarHoverSection::Computer(index),
                                        ))
                                    }),
                            )
                            .child(div().flex_none().w(px(12.)).h(px(12.)).when(
                                computer.loading,
                                |slot| {
                                    slot.child(spinner(
                                        "computer-loading",
                                        px(12.),
                                        theme::text_muted(),
                                    ))
                                },
                            ))
                            .when(
                                collapsible && !dragging && !self.sidebar_menu_open(),
                                |row| {
                                    row.child(
                                        div()
                                            .flex_none()
                                            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                                cx.stop_propagation();
                                            })
                                            .child(
                                                header_button(
                                                    "connection-new-project",
                                                    "folder-plus",
                                                    "New project",
                                                    false,
                                                )
                                                .aria_label(new_project_label)
                                                .opacity(0.)
                                                .group_hover(action_group, |style| {
                                                    style.opacity(1.)
                                                })
                                                .on_click(cx.listener(move |_, _, _, cx| {
                                                    cx.stop_propagation();
                                                    cx.emit(SidebarEvent::Navigate(
                                                        new_project.clone(),
                                                    ));
                                                })),
                                            ),
                                    )
                                },
                            ),
                    )
                    .when_some(status, |row, status| row.child(status))
                    .into_any_element()
            }
            _ => div().into_any_element(),
        }
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
            activity: TaskActivity::default(),
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
        let sidebar = chat.read_with(cx, |chat, _| chat.sidebar.clone());
        sidebar.update(cx, |sidebar, _| {
            sidebar.reset_persisted_for_test();
            sidebar.view = SidebarView {
                group: SidebarGroup::Project,
                sort: SidebarSort::Manual,
            };
        });
        sidebar
    }

    fn seed_browse_pages(sidebar: &mut Sidebar) {
        // These cache-transfer fixtures use exact desktop Manual cursors. The
        // separate merged-prefix tests cover the host timestamp frontier.
        sidebar.view.sort = SidebarSort::Manual;
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
            page.loaded
                .extend(["archive-count".into(), "groups".into(), "projects".into()]);
            page.next_groups = Some(format!("next-{id}"));
            page.groups[0].more.active = Some(format!("active-{id}"));
            sidebar.group_pages.hosts.insert(computer.key, page);
        }
        sidebar.rebuild_sections();
    }

    #[gpui::test]
    fn archived_header_uses_known_counts_without_hiding_count_retries(
        cx: &mut gpui::TestAppContext,
    ) {
        let _settings = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let sidebar = test_sidebar(cx, backend, "user");
        sidebar.update(cx, |sidebar, _| {
            seed_browse_pages(sidebar);
            assert!(!sidebar.entries.contains(&SidebarEntry::ArchivedHeader));

            let remote = Some("remote".into());
            sidebar.group_pages.hosts.get_mut(&remote).unwrap().archived_count = 3;
            sidebar.rebuild_sections();
            assert!(sidebar.entries.contains(&SidebarEntry::ArchivedHeader));
            assert!(sidebar.group_pages.hosts[&remote].archived.is_empty());
            assert!(!sidebar.archived_expanded);

            let page = sidebar.group_pages.hosts.get_mut(&remote).unwrap();
            page.archived_count = 0;
            page.errors.insert("archive-count".into(), "Unavailable".into());
            sidebar.rebuild_sections();
            assert!(!sidebar.entries.contains(&SidebarEntry::ArchivedHeader));
            assert_eq!(
                sidebar
                    .entries
                    .iter()
                    .filter(|entry| matches!(entry, SidebarEntry::ComputerMore(index)
                        if matches!(sidebar.group_pages.more[*index].kind, ComputerPageKind::ArchiveCount)))
                    .count(),
                1,
                "count errors remain retryable without an empty heading"
            );

            let page = sidebar.group_pages.hosts.get_mut(&remote).unwrap();
            page.errors.clear();
            page.archived_count = 3;
            sidebar.filter = "needle".into();
            sidebar.rebuild_sections();
            assert!(!sidebar.entries.contains(&SidebarEntry::ArchivedHeader));

            sidebar.group_pages.hosts.get_mut(&remote).unwrap().search.push(TaskSummary {
                title: "needle archive".into(),
                state: "archived".into(),
                ..task("archived-hit")
            });
            sidebar.rebuild_sections();
            assert!(sidebar.entries.contains(&SidebarEntry::ArchivedHeader));
            assert_eq!(sidebar.computer_archived_count, 1);
            assert!(!sidebar.archived_expanded);
        });
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
            // Explicit order/scope refreshes replace pending request authority
            // immediately. Label-only activity is deliberately coalesced in
            // Manual mode and is covered separately below.
            sidebar.refresh_computer_pages(cx);
            sidebar.rebuild_sections();
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
            sidebar.rebuild_sections();
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
        computer.activity.running.insert("head".into());
        let page = snapshot_page(&computer, "", false);
        assert_eq!(page.groups.len(), 1);
        assert_eq!(page.groups[0].tasks[0].uuid, "head");
    }
}

#[cfg(test)]
mod revalidation_tests {
    use super::*;

    #[gpui::test]
    fn manual_activity_coalesces_into_one_trailing_snapshot(cx: &mut gpui::TestAppContext) {
        let _settings = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let chat = cx.new(|cx| ChatScreen::new_without_start(backend, "user".into(), cx));
        let sidebar = chat.read_with(cx, |chat, _| chat.sidebar.clone());
        sidebar.update(cx, |sidebar, cx| {
            sidebar.reset_persisted_for_test();
            sidebar.view = SidebarView {
                group: SidebarGroup::Project,
                sort: SidebarSort::Manual,
            };
            let mut local = Computer::pending("local".into(), "This Computer".into(), None);
            local.key = None;
            local.backend = Some(sidebar.backend.clone());
            let mut remote = Computer::pending("remote".into(), "Remote".into(), None);
            remote.backend = Some(Arc::new(AgentBackend::for_remote_test("remote").unwrap()));
            sidebar.computers = Arc::new(vec![local, remote]);
            sidebar.group_pages.scope = Some((String::new(), None, false));
            let generation = sidebar.group_pages.generation;
            for key in [None, Some("remote".into())] {
                sidebar.group_pages.hosts.insert(
                    key,
                    ComputerHostPage {
                        loading: HashSet::from(["groups".into()]),
                        ..Default::default()
                    },
                );
            }
            for revision in 0..20 {
                let mut computers = sidebar.computers.as_ref().clone();
                computers[0].label = format!("Updated computer {revision}").into();
                sidebar.set_computers(Arc::new(computers), cx);
            }
            assert_eq!(sidebar.group_pages.generation, generation);
            assert!(sidebar.group_pages.revalidation_pending);
            let action = |computer| ComputerMore {
                computer,
                generation,
                kind: ComputerPageKind::Groups,
                cursor: None,
                label: "Loading tasks…".into(),
                loading: true,
            };
            sidebar.apply_computer_page(
                action(None),
                Ok(ComputerPageReply::Groups(Default::default())),
                cx,
            );
            assert_eq!(sidebar.group_pages.generation, generation);
            assert!(sidebar.group_pages.revalidation_pending);
            sidebar.apply_computer_page(
                action(Some("remote".into())),
                Ok(ComputerPageReply::Groups(Default::default())),
                cx,
            );
            assert_eq!(sidebar.group_pages.generation, generation.wrapping_add(1));
            assert!(!sidebar.group_pages.revalidation_pending);
            assert!(
                sidebar.group_pages.manual_reads_pending(),
                "one trailing reload started"
            );
            // An older callback cannot refill the replacement generation.
            sidebar.apply_computer_page(
                action(Some("remote".into())),
                Err("stale failure".into()),
                cx,
            );
            assert!(
                sidebar
                    .group_pages
                    .hosts
                    .values()
                    .all(|page| page.errors.is_empty())
            );
        });
    }

    #[test]
    fn metadata_reads_do_not_delay_a_trailing_manual_snapshot() {
        let pages = ComputerPages {
            hosts: HashMap::from([(
                None,
                ComputerHostPage {
                    loading: HashSet::from(["projects".into(), "archive-count".into()]),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        assert!(!pages.manual_reads_pending());
    }
}

#[cfg(test)]
mod merged_prefix_tests {
    use super::*;

    fn task(id: &str, state: &str, updated: i64) -> TaskSummary {
        TaskSummary {
            uuid: id.into(),
            title: id.into(),
            project_root: "/tmp/project".into(),
            project_display_name: None,
            state: state.into(),
            kind: "desktop".into(),
            pinned_ms: None,
            web_enabled: false,
            model: None,
            message_count: 1,
            created_ms: updated,
            updated_ms: updated,
            truncated: false,
        }
    }

    fn shown(sidebar: &Sidebar) -> Vec<String> {
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

    #[gpui::test]
    fn unread_remote_rows_do_not_fill_gaps_in_another_computers_list(
        cx: &mut gpui::TestAppContext,
    ) {
        let _settings = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let chat = cx.new(|cx| ChatScreen::new_without_start(backend, "user".into(), cx));
        let sidebar = chat.read_with(cx, |chat, _| chat.sidebar.clone());
        sidebar.update(cx, |sidebar, _| {
            sidebar.reset_persisted_for_test();
            sidebar.view = SidebarView {
                group: SidebarGroup::List,
                sort: SidebarSort::Updated,
            };
            sidebar.selected = None;
            sidebar.activity.running.clear();
            let mut local = Computer::pending("local".into(), "This Computer".into(), None);
            local.key = None;
            local.backend = Some(sidebar.backend.clone());
            let mut remote = Computer::pending("remote".into(), "Remote".into(), None);
            remote.backend = Some(Arc::new(AgentBackend::for_remote_test("remote").unwrap()));
            sidebar.computers = Arc::new(vec![local, remote]);
            let local_tasks = vec![
                task("local-900", "active", 900),
                task("local-800", "active", 800),
            ];
            sidebar.sessions = local_tasks.iter().map(session_from_summary).collect();
            sidebar.group_pages.hosts.insert(
                None,
                ComputerHostPage {
                    active: local_tasks,
                    ..Default::default()
                },
            );
            let remote_key = Some("remote".into());
            sidebar.group_pages.hosts.insert(
                remote_key.clone(),
                ComputerHostPage {
                    active: vec![
                        task("remote-1000", "active", 1000),
                        task("remote-950", "active", 950),
                    ],
                    next_active: Some("next".into()),
                    ..Default::default()
                },
            );
            sidebar.rebuild_sections();
            let before = shown(sidebar);
            assert_eq!(before, ["remote-1000"]);
            assert_eq!(sidebar.group_pages.more.len(), 1);
            assert_eq!(sidebar.group_pages.more[0].computer, remote_key);
            assert!(matches!(
                sidebar.group_pages.more[0].kind,
                ComputerPageKind::Tasks { settled: false }
            ));
            let remote = sidebar.group_pages.hosts.get_mut(&remote_key).unwrap();
            remote.active.extend([
                task("remote-940", "active", 940),
                task("remote-850", "active", 850),
            ]);
            remote.next_active = None;
            sidebar.rebuild_sections();
            let after = shown(sidebar);
            assert_eq!(&after[..before.len()], before);
            assert_eq!(
                after,
                [
                    "remote-1000",
                    "remote-950",
                    "remote-940",
                    "local-900",
                    "remote-850",
                    "local-800"
                ]
            );
            assert!(sidebar.group_pages.more.is_empty());
        });
    }

    #[gpui::test]
    fn project_more_advances_only_the_state_limiting_its_merged_prefix(
        cx: &mut gpui::TestAppContext,
    ) {
        let _settings = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let chat = cx.new(|cx| ChatScreen::new_without_start(backend, "user".into(), cx));
        let sidebar = chat.read_with(cx, |chat, _| chat.sidebar.clone());
        sidebar.update(cx, |sidebar, _| {
            sidebar.reset_persisted_for_test();
            sidebar.view = SidebarView::default();
            sidebar.selected = None;
            let mut remote = Computer::pending("remote".into(), "Remote".into(), None);
            remote.backend = Some(Arc::new(AgentBackend::for_remote_test("remote").unwrap()));
            sidebar.computers = Arc::new(vec![remote]);
            let remote_key = Some("remote".into());
            let mut group = NavigationProjectGroup {
                project: ProjectSummary {
                    root: "/tmp/project".into(),
                    display_name: None,
                    position: None,
                    last_used_ms: 110,
                    name_revision: 0,
                    is_default_workspace: false,
                    truncated: false,
                },
                tasks: vec![
                    task("active-80", "active", 80),
                    task("active-70", "active", 70),
                    task("settled-110", "settled", 110),
                    task("settled-100", "settled", 100),
                ],
                more: Default::default(),
            };
            group.more.settled = Some("next".into());
            sidebar.group_pages.hosts.insert(
                remote_key.clone(),
                ComputerHostPage {
                    groups: vec![group],
                    ..Default::default()
                },
            );
            sidebar.rebuild_sections();
            assert_eq!(shown(sidebar), ["settled-110"]);
            assert_eq!(sidebar.group_pages.more.len(), 1);
            assert!(matches!(
                sidebar.group_pages.more[0].kind,
                ComputerPageKind::Project { settled: true, .. }
            ));
            let group = &mut sidebar
                .group_pages
                .hosts
                .get_mut(&remote_key)
                .unwrap()
                .groups[0];
            group.tasks.push(task("settled-90", "settled", 90));
            group.more.settled = None;
            sidebar.rebuild_sections();
            assert_eq!(
                shown(sidebar),
                [
                    "settled-110",
                    "settled-100",
                    "settled-90",
                    "active-80",
                    "active-70"
                ]
            );
            assert!(sidebar.group_pages.more.is_empty());
        });
    }
}
