//! Host-scoped navigation data. Transcripts, drafts and execution stay in their
//! existing per-computer ChatScreen; only project/task summaries are projected.
use super::*;
use maple_agent::store::{
    NavigationTaskPage, NavigationTaskQuery, PinFilter, TaskSection, TaskSummary,
};

enum PickerProjectResponse {
    Recent(Vec<RecentProjectRoot>),
    LegacyTasks(Vec<TaskSummary>),
}

async fn read_picker_projects(
    backend: &AgentBackend,
    user: &str,
) -> Result<PickerProjectResponse, UiFailure> {
    match backend
        .call_presented("recent_project_roots", (user,))
        .await
    {
        Ok(projects) => Ok(PickerProjectResponse::Recent(projects)),
        Err(error) if error.code == crate::remote::streams::RECENT_PROJECTS_UNSUPPORTED_CODE => {
            // Older hosts can still return their recent task summaries. Use
            // that existing, account-scoped read to recover project activity.
            let query = NavigationTaskQuery {
                section: TaskSection::Recent,
                project: None,
                pins: PinFilter::Ignore,
                cursor: None,
                limit: 50,
            };
            let page: NavigationTaskPage =
                backend.call_presented("list_tasks", (user, query)).await?;
            Ok(PickerProjectResponse::LegacyTasks(page.tasks))
        }
        Err(error) => Err(error.into()),
    }
}

fn legacy_picker_projects(
    tasks: Vec<TaskSummary>,
    names: Option<&HashMap<String, String>>,
) -> Vec<RecentProjectRoot> {
    let mut seen = HashSet::new();
    // The host orders its task page by last update, newest first.
    tasks
        .into_iter()
        .filter(|task| task.message_count > 0 && task.state != "archived")
        .filter(|task| !task.project_root.is_empty() && seen.insert(task.project_root.clone()))
        .take(3)
        .map(|task| RecentProjectRoot {
            name: task
                .project_display_name
                .or_else(|| {
                    names
                        .and_then(|names| names.get(&task.project_root))
                        .cloned()
                })
                .unwrap_or_else(|| sidebar::remote_root_display_name(&task.project_root)),
            path: task.project_root,
            last_used_ms: u128::try_from(task.updated_ms).unwrap_or_default(),
            is_default_workspace: false,
            name_revision: 0,
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Destination {
    Overview,
    Project(String),
    /// The project selector was opened from New Task. Carry that intent across
    /// computers (and a deferred connection), whose ChatScreens are independent.
    DraftProject(String),
    /// An explicit picker choice also records use of an already-selected root.
    DraftRecentProject(String),
    RecentProject(String),
    ScratchProject,
    Task(String),
    NewProject,
    DraftNewProject,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Navigate {
    pub computer: Option<String>,
    pub destination: Destination,
}
/// A folder belongs to its execution computer, even while another is selected.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ProjectTarget {
    pub computer: Option<String>,
    pub path: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ProjectRenameRequest {
    pub target: ProjectTarget,
    pub name: String,
}

#[derive(Clone, Debug)]
pub(crate) enum TaskAction {
    Rename(String),
    SetState(AgentTaskState),
    Pin(bool),
    Delete,
}

#[derive(Clone, Debug)]
pub(crate) struct TaskActionRequest {
    pub target: crate::settings::SidebarItemKey,
    pub action: TaskAction,
}

pub(crate) struct NavigationChanged;

#[derive(Clone)]
pub(crate) struct Project {
    pub path: String,
    pub label: SharedString,
}
#[derive(Clone)]
pub(crate) struct Computer {
    pub key: Option<String>,
    pub label: SharedString,
    /// Summary queries go to the owning host; a disconnected computer has no
    /// handle until its catalog has reconnected.
    pub backend: Option<Arc<AgentBackend>>,
    /// Recovery fences cached navigation even when the backend handle survives.
    pub host_epoch: u64,
    pub project_names: HashMap<String, String>,
    pub projects: Vec<Project>,
    pub sessions: Vec<AgentSessionSummary>,
    pub activity: TaskActivity,
    pub pinned: Vec<String>,
    pub loading: bool,
    pub error: Option<SharedString>,
}
impl Computer {
    pub fn pending(key: String, label: String, error: Option<String>) -> Self {
        Self {
            key: Some(key),
            label: label.into(),
            backend: None,
            host_epoch: 0,
            project_names: HashMap::new(),
            projects: Vec::new(),
            sessions: Vec::new(),
            activity: TaskActivity::default(),
            pinned: Vec::new(),
            loading: error.is_none(),
            error: error.map(Into::into),
        }
    }
}

impl EventEmitter<Navigate> for ChatScreen {}
impl EventEmitter<ProjectRenameRequest> for ChatScreen {}
impl EventEmitter<TaskActionRequest> for ChatScreen {}
impl EventEmitter<NavigationChanged> for ChatScreen {}

impl ChatScreen {
    pub(crate) fn project_names_available(&self) -> bool {
        self.navigation_ready && !self.host_recovery.unavailable && !self.host_recovery.ended
    }

    pub(crate) fn apply_project_names(
        &mut self,
        roots: Vec<RecentProjectRoot>,
        cx: &mut Context<Self>,
    ) {
        self.merge_project_metadata(roots, cx);
        cx.emit(NavigationChanged);
        cx.notify();
    }

    pub(crate) fn set_sidebar_task_pin(
        &mut self,
        session_id: &str,
        pinned: bool,
        cx: &mut Context<Self>,
    ) {
        let was_pinned = self
            .sidebar
            .read(cx)
            .pinned_tasks()
            .iter()
            .any(|id| id == session_id);
        if was_pinned != pinned {
            self.toggle_task_pin(session_id, cx);
        }
    }

    pub(crate) fn apply_sidebar_task_action(
        &mut self,
        session_id: &str,
        action: &TaskAction,
        session: Option<AgentSessionSummary>,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = session {
            let root = session.project_root.clone();
            self.upsert_session(session, cx);
            if matches!(action, TaskAction::SetState(AgentTaskState::Archived))
                && self.selected_session.as_deref() == Some(session_id)
            {
                self.leave_selected_session(cx);
                let next = self
                    .sessions
                    .iter()
                    .find(|session| {
                        session.state != AgentTaskState::Archived && session.project_root == root
                    })
                    .map(|session| session.id.clone());
                if let Some(id) = next {
                    self.select_session(&id, cx);
                }
            }
        } else if matches!(action, TaskAction::Delete) {
            self.remove_session(session_id, cx);
        }
        cx.notify();
    }

    pub(crate) fn sidebar_task_action_notice(&mut self, message: UiText, cx: &mut Context<Self>) {
        self.notice = Some(message);
        cx.notify();
    }

    pub(crate) fn refresh_sidebar_task_actions(&mut self, cx: &mut Context<Self>) {
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.refresh_computer_pages(cx));
    }

    #[cfg(test)]
    pub(crate) fn begin_scratch_send_for_test(&mut self, cx: &mut Context<Self>) {
        self.booting = false;
        self.navigation_ready = true;
        self.select_scratch_project(cx);
        self.send_text("Create a new project".into(), cx);
    }

    #[cfg(test)]
    pub(crate) fn scratch_send_pending_for_test(&self) -> bool {
        self.scratch_creation.is_some() && self.pending_first_send.is_some()
    }

    #[cfg(test)]
    pub(crate) fn seed_projects_for_test(
        &mut self,
        roots: Vec<RecentProjectRoot>,
        cx: &mut Context<Self>,
    ) {
        for root in roots {
            self.restore_selected_project(root, cx);
        }
    }

    pub(crate) fn finish_project_rename(
        &mut self,
        target: ProjectTarget,
        result: Result<(), UiText>,
        cx: &mut Context<Self>,
    ) {
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.finish_project_rename(target, result, cx)
        });
    }

    pub(crate) fn sidebar_browse_state(&self, cx: &gpui::App) -> sidebar::BrowseState {
        self.sidebar.read(cx).browse_state(cx)
    }

    pub(crate) fn restore_sidebar_browse_state(
        &mut self,
        state: sidebar::BrowseState,
        cx: &mut Context<Self>,
    ) {
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.restore_browse_state(state, cx));
        // The keyboard focus stayed on the computer this one replaced, which
        // is no longer drawn.
        self.screen_focus_pending = true;
    }

    /// Name updates are pushed. Refresh the small folder list periodically for
    /// project additions/removals and external filesystem changes. Reuse the
    /// app's status timer, not one timer per computer, and never reload a
    /// conversation here.
    pub(crate) fn refresh_navigation_if_due(&mut self, cx: &mut Context<Self>) {
        let now = std::time::Instant::now();
        if now < self.next_navigation_refresh || self.host_recovery.ended {
            return;
        }
        self.next_navigation_refresh = now + std::time::Duration::from_secs(30);
        if self.navigation_only && !self.navigation_ready && self.host_recovery.events.is_none() {
            self.recover_host(cx);
        } else if !self.host_recovery.unavailable {
            self.refresh_roots(cx);
        }
    }

    pub(crate) fn navigation_generation(&self) -> u64 {
        self.selection_generation
    }

    /// Warm a paired computer without opening a conversation or running a task.
    /// The normal event stream keeps its navigation summaries live.
    pub(crate) fn new_catalog(
        backend: Arc<AgentBackend>,
        user_id: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::new_mounted(backend, user_id, cx);
        this.navigation_only = true;
        this.visible = false;
        this.booting = false;
        this.recover_host(cx);
        this
    }

    /// Leaving for another computer abandons a first send whose folder is
    /// still being created; the late result lands in no context. Hiding
    /// the screen for Settings is not a switch and keeps the send.
    pub(crate) fn cancel_scratch_send(&mut self, cx: &mut Context<Self>) {
        if self.scratch_creation.is_none() {
            return;
        }
        self.begin_navigation();
        self.pending_first_send = None;
        self.notice = Some(UiText::key("chat-scratch-switched-computers"));
        cx.notify();
    }

    pub(crate) fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        let changed = self.visible != visible;
        self.visible = visible;
        if !visible {
            self.project_folder_dialog = None;
        }
        if changed && visible && !self.computers.is_empty() {
            // Only the mounted sidebar projects every computer. Background
            // views keep their own summaries and the shared catalog handle;
            // they do not rebuild all foreign rows on every update.
            // An empty list is provisional startup state; an authoritative
            // inventory always includes This computer.
            let computers = self.computers.clone();
            self.sidebar
                .update(cx, |sidebar, cx| sidebar.set_computers(computers, cx));
        }
        if visible
            && let Some(id) = self.selected_session.clone()
            && self.mark_seen(&id)
        {
            self.sync_sidebar(cx);
        }
    }

    pub(crate) fn computer_snapshot(&self, label: SharedString, cx: &gpui::App) -> Computer {
        let mut roots = self.sidebar.read(cx).navigation_roots();
        for session in self.sessions.iter().filter(|session| {
            !self
                .sidebar
                .read(cx)
                .project_is_removed(&session.project_root)
        }) {
            if !roots.contains(&session.project_root) {
                roots.push(session.project_root.clone());
            }
        }
        Computer {
            key: self.backend.remote_host().map(str::to_owned),
            label,
            backend: Some(self.backend.clone()),
            host_epoch: self.host_recovery.epoch,
            project_names: self.sidebar.read(cx).project_names(),
            projects: roots
                .into_iter()
                .map(|path| Project {
                    label: self.sidebar.read(cx).root_name(&path).into(),
                    path,
                })
                .collect(),
            sessions: self
                .sidebar
                .read(cx)
                .navigation_sessions()
                .into_iter()
                .filter(|session| {
                    !self
                        .sidebar
                        .read(cx)
                        .project_is_removed(&session.project_root)
                })
                .collect(),
            activity: self.task_activity(),
            pinned: self.sidebar.read(cx).pinned_tasks().to_vec(),
            loading: (!self.navigation_ready && self.host_recovery.events.is_some())
                || (self.booting && !self.host_recovery.unavailable),
            error: (self.host_recovery.unavailable && self.host_recovery.events.is_none()).then(
                || {
                    if self.host_recovery.ended {
                        if self
                            .runtime_error_reason
                            .as_deref()
                            .is_some_and(crate::remote::wire::is_protocol_incompatibility_refusal)
                        {
                            tr("chat-computer-update-required")
                        } else {
                            tr("chat-computer-reconnect")
                        }
                    } else {
                        tr("chat-connecting")
                    }
                },
            ),
        }
    }

    pub(crate) fn set_computers(&mut self, computers: Arc<Vec<Computer>>, cx: &mut Context<Self>) {
        self.computers = computers.clone();
        if let Some(dialog) = &self.project_folder_dialog
            && let Some(computer) = computers
                .iter()
                .find(|computer| computer.key.as_deref() == self.backend.remote_host())
        {
            dialog.update(cx, |dialog, cx| {
                dialog.set_computer_label(computer.label.to_string().into(), cx)
            });
        }
        self.refresh_project_caption(cx);
        if self.visible {
            self.sidebar
                .update(cx, |sidebar, cx| sidebar.set_computers(computers, cx));
            cx.notify();
        }
    }

    pub(super) fn refresh_project_caption(&mut self, cx: &gpui::App) {
        let project = self.project_label(cx);
        self.project_label = match self.backend.remote_host() {
            Some(host) => {
                let name = self
                    .computers
                    .iter()
                    .find(|c| c.key.as_deref() == Some(host))
                    .map(|c| c.label.clone())
                    .unwrap_or_else(|| tr("chat-remote-computer"));
                format!("{name} · {project}").into()
            }
            None => project.into(),
        };
    }

    pub(crate) fn navigate(&mut self, destination: Destination, cx: &mut Context<Self>) {
        if !self.navigation_ready {
            if self.navigation_only && self.host_recovery.events.is_none() {
                self.recover_host(cx);
            }
            self.pending_navigation = Some((self.selection_generation, destination));
            return;
        }
        if matches!(
            destination,
            Destination::NewProject | Destination::DraftNewProject
        ) {
            // A chooser is not navigation until Use folder succeeds. In
            // particular, do not activate a catalog and let its runtime
            // refresh open a task behind the pending modal.
            self.choose_root_dialog_with_intent(
                self.navigation_only || matches!(destination, Destination::DraftNewProject),
                cx,
            );
            return;
        }
        // Opening a remote computer's projects is not a request to open its
        // newest task. Establish the draft before runtime refreshes can
        // auto-select a session. Explicit task clicks still win.
        let needs_draft = matches!(
            destination,
            Destination::DraftProject(_)
                | Destination::DraftRecentProject(_)
                | Destination::ScratchProject
        ) || (self.navigation_only
            && !matches!(destination, Destination::Task(_)));
        if needs_draft && !self.prepare_draft_navigation(cx) {
            return;
        }
        self.leave_catalog(cx);
        match destination {
            Destination::Overview => self
                .sidebar
                .update(cx, |s, cx| s.set_project_filter(None, cx)),
            Destination::Task(id) => self.select_session(&id, cx),
            Destination::ScratchProject => self.select_scratch_project(cx),
            Destination::RecentProject(path) | Destination::DraftRecentProject(path) => {
                self.select_project_root(path, cx);
            }
            Destination::Project(path) | Destination::DraftProject(path) => {
                // The current workspace is already registered and selected.
                // A new draft there needs no owner request or sidebar reload.
                if !self.draft || self.project_root.as_deref() != Some(path.as_str()) {
                    self.select_project_root(path, cx);
                }
            }
            Destination::NewProject | Destination::DraftNewProject => self.choose_root_dialog(cx),
        }
        cx.notify();
    }

    fn prepare_draft_navigation(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.draft {
            self.new_session(cx);
        }
        // New Task refuses to leave a first send or project selection in
        // flight. Do not proceed to a different project if it was refused.
        self.draft && !self.session_setup_pending && !self.root_selecting
    }

    /// Stop filtering transcript events before this computer is used for work.
    /// Recovery already warms its runtime; activation must preserve that boot
    /// state instead of restarting setup on the first click or send.
    pub(super) fn leave_catalog(&mut self, _cx: &mut Context<Self>) {
        self.navigation_only = false;
    }

    pub(super) fn finish_navigation_bootstrap(&mut self, cx: &mut Context<Self>) {
        self.navigation_ready = true;
        let Some((generation, destination)) = self.pending_navigation.take() else {
            return;
        };
        if generation == self.selection_generation {
            self.navigate(destination, cx);
        } else if self.visible {
            // A newer choice on this screen (New Task, a reconnect) replaced
            // the destination, but the computer was still opened for work.
            self.leave_catalog(cx);
        }
    }

    pub(crate) fn navigation_notice(&mut self, message: UiText, cx: &mut Context<Self>) {
        self.notice = Some(message);
        cx.notify();
    }

    pub(super) fn install_picker_projects(
        &mut self,
        mut projects: Vec<(Option<String>, RecentProjectRoot)>,
    ) {
        projects.sort_by(|(a_host, a), (b_host, b)| {
            b.last_used_ms
                .cmp(&a.last_used_ms)
                .then_with(|| a_host.cmp(b_host))
                .then_with(|| a.path.cmp(&b.path))
        });
        let mut seen = HashSet::new();
        let mut counts = HashMap::new();
        projects.retain(|(host, project)| {
            let count = counts.entry(host.clone()).or_insert(0);
            if *count >= 3 || !seen.insert((host.clone(), project.path.clone())) {
                return false;
            }
            *count += 1;
            true
        });
        self.picker_projects = projects;
    }

    pub(super) fn refresh_picker_projects(&mut self, cx: &mut Context<Self>) {
        self.picker_projects_generation += 1;
        self.picker_projects.clear();
        #[cfg(test)]
        if let Some(projects) = self.picker_projects_fixture.clone() {
            self.install_picker_projects(projects);
            cx.notify();
            return;
        }
        let generation = self.picker_projects_generation;
        let user = self.user_id.clone();
        let epoch = self.host_recovery.epoch;
        let multiple = self.computers.len() > 1;
        let sources = if multiple {
            self.computers
                .iter()
                .filter_map(|computer| {
                    Some((
                        computer.key.clone(),
                        computer.backend.clone()?,
                        computer.host_epoch,
                    ))
                })
                .collect::<Vec<_>>()
        } else {
            vec![(
                self.backend.remote_host().map(str::to_owned),
                self.backend.clone(),
                epoch,
            )]
        };
        for (key, backend, source_epoch) in sources {
            let user = user.clone();
            let request_user = user.clone();
            let request_backend = backend.clone();
            self.call_semantic(
                async move { read_picker_projects(&request_backend, &request_user).await },
                cx,
                move |this, result, cx| {
                    if this.user_id != user
                        || this.host_recovery.epoch != epoch
                        || this.picker_projects_generation != generation
                        || (this.computers.len() > 1) != multiple
                    {
                        return;
                    }
                    let current = if multiple {
                        this.computers.iter().any(|computer| {
                            computer.key == key
                                && computer.host_epoch == source_epoch
                                && computer
                                    .backend
                                    .as_ref()
                                    .is_some_and(|active| Arc::ptr_eq(active, &backend))
                        })
                    } else {
                        Arc::ptr_eq(&this.backend, &backend)
                    };
                    if !current {
                        return;
                    }
                    match result {
                        Ok(PickerProjectResponse::Recent(roots)) => {
                            this.merge_picker_projects(key, roots);
                        }
                        Ok(PickerProjectResponse::LegacyTasks(tasks)) => {
                            let names = this.computers.iter().find(|computer| computer.key == key);
                            let roots =
                                legacy_picker_projects(tasks, names.map(|c| &c.project_names));
                            this.merge_picker_projects(key, roots);
                        }
                        Err(error) => this.notice = Some(error.into()),
                    }
                    cx.notify();
                },
            );
        }
    }

    pub(super) fn merge_picker_projects(
        &mut self,
        key: Option<String>,
        roots: Vec<RecentProjectRoot>,
    ) {
        let mut projects = std::mem::take(&mut self.picker_projects);
        projects.extend(roots.into_iter().map(|root| (key.clone(), root)));
        self.install_picker_projects(projects);
    }

    pub(super) fn computers_project_menu(&self) -> Menu<Self> {
        let mut menu = Menu::new("computer-project-menu", px(320.))
            .label(tr("chat-projects"))
            .max_height(px(440.))
            .application_vim(self.application_vim_enabled);
        for (index, computer) in self.computers.iter().enumerate() {
            if index > 0 {
                menu = menu.child(
                    div()
                        .w_full()
                        .flex_none()
                        .my_1()
                        .h(px(1.))
                        .bg(gpui::rgb(theme::border())),
                );
            }
            menu = menu.child(
                div()
                    .px_3()
                    .pt_1()
                    .pb_1()
                    .text_xs()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(gpui::text!(
                        id = SharedString::from(format!("computer-{index}-heading")),
                        computer.label.clone()
                    )),
            );
            menu = menu.item(self.scratch_project_item(
                SharedString::from(format!("computer-{index}-scratch")),
                computer.key.clone(),
            ));
            let request = Navigate {
                computer: computer.key.clone(),
                destination: if self.draft {
                    Destination::DraftNewProject
                } else {
                    Destination::NewProject
                },
            };
            menu = menu.item(
                MenuItem::new(
                    SharedString::from(format!("computer-{index}-new-project")),
                    tr("chat-choose-folder-menu"),
                    move |_: &mut Self, _, cx| cx.emit(request.clone()),
                )
                .icon("folder-open"),
            );
            if self
                .picker_projects
                .iter()
                .any(|(key, _)| key == &computer.key)
            {
                menu = menu.child(
                    div()
                        .px_3()
                        .pt_2()
                        .pb_1()
                        .text_xs()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(gpui::text!(
                            id = SharedString::from(format!("computer-{index}-recents")),
                            tr("chat-recent-projects")
                        )),
                );
            }
            for (project_index, (_, project)) in self
                .picker_projects
                .iter()
                .filter(|(key, _)| key == &computer.key)
                .enumerate()
            {
                let request = Navigate {
                    computer: computer.key.clone(),
                    destination: if self.draft {
                        Destination::DraftRecentProject(project.path.clone())
                    } else {
                        Destination::RecentProject(project.path.clone())
                    },
                };
                let mut item = MenuItem::new(
                    SharedString::from(format!("computer-{index}-project-{project_index}")),
                    project.name.clone(),
                    move |_: &mut Self, _, cx| cx.emit(request.clone()),
                )
                .icon(if computer.key.is_some() {
                    "globe"
                } else {
                    "folder"
                })
                .current(
                    computer.key.as_deref() == self.backend.remote_host()
                        && self.project_root.as_deref() == Some(project.path.as_str()),
                );
                if self
                    .picker_projects
                    .iter()
                    .filter(|(key, peer)| key == &computer.key && peer.name == project.name)
                    .count()
                    > 1
                {
                    item = item.note(project.path.clone());
                }
                menu = menu.item(item);
            }
        }
        menu.separator().item(
            MenuItem::new(
                "manage-computers",
                tr("chat-remote-computer-settings"),
                |_: &mut Self, _, cx| {
                    cx.emit(OpenSettingsSection(Section::Remote));
                },
            )
            .icon("settings"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::wire::{Error, Refusal};
    use gpui::TestAppContext;

    #[test]
    fn legacy_recents_use_distinct_projects_with_recent_task_activity() {
        let task = |path: &str, updated_ms, state: &str, message_count| TaskSummary {
            uuid: String::new(),
            title: String::new(),
            project_root: path.into(),
            project_display_name: None,
            state: state.into(),
            kind: "desktop".into(),
            pinned_ms: None,
            web_enabled: false,
            permission_mode: None,
            model: None,
            message_count,
            created_ms: 1,
            updated_ms,
            truncated: false,
        };
        let names = HashMap::from([("/first".into(), "First Project".into())]);
        let roots = legacy_picker_projects(
            vec![
                task("/removed", 60, "archived", 1),
                task("/draft", 55, "active", 0),
                task("/first", 50, "active", 2),
                task("/first", 40, "settled", 3),
                task("C:\\work\\Second", 30, "settled", 1),
                task("/third", 20, "active", 1),
                task("/fourth", 10, "active", 1),
            ],
            Some(&names),
        );
        assert_eq!(
            roots
                .iter()
                .map(|root| root.name.as_str())
                .collect::<Vec<_>>(),
            ["First Project", "Second", "third"]
        );
        assert_eq!(roots[0].last_used_ms, 50);
    }

    #[gpui::test]
    fn catalog_marks_only_actual_protocol_refusal_as_update_required(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let cases = [
            (
                Refusal::terminal(Error::Incompatible.to_string()).into_error(),
                tr("chat-computer-update-required"),
            ),
            (
                Refusal::terminal("This computer is no longer paired").into_error(),
                tr("chat-computer-reconnect"),
            ),
            (
                Refusal::terminal(Error::InvalidFrame.to_string()).into_error(),
                tr("chat-computer-reconnect"),
            ),
            (
                Error::Disconnected.to_string(),
                tr("chat-computer-reconnect"),
            ),
        ];
        for (reason, expected) in cases {
            let chat = {
                let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
                let backend = Arc::new(
                    AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap(),
                );
                cx.new(|cx| ChatScreen::new_inner_for_test(backend, "account".into(), cx))
            };
            chat.update(cx, |chat, cx| {
                chat.handle_service_event(AgentServiceEvent::RemoteAccessEnded(reason), cx);
                let snapshot = chat.computer_snapshot("Office".into(), cx);
                assert_eq!(snapshot.error.as_deref(), Some(expected.as_ref()));
                assert!(!snapshot.loading);
                // The generic disconnect following a terminal refusal must
                // not hide the reason the authenticated connection ended.
                chat.handle_service_event(AgentServiceEvent::HostDisconnected, cx);
                assert_eq!(
                    chat.computer_snapshot("Office".into(), cx).error.as_deref(),
                    Some(expected.as_ref())
                );
            });
        }
    }
}
