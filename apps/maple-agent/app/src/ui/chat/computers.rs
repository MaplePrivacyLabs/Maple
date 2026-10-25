//! Host-scoped navigation data. Transcripts, drafts and execution stay in their
//! existing per-computer ChatScreen; only project/task summaries are projected.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Destination {
    Overview,
    Project(String),
    /// The project selector was opened from New Task. Carry that intent across
    /// computers (and a deferred connection), whose ChatScreens are independent.
    DraftProject(String),
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
    pub running: HashSet<String>,
    pub unread: HashSet<String>,
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
            running: HashSet::new(),
            unread: HashSet::new(),
            pinned: Vec::new(),
            loading: error.is_none(),
            error: error.map(Into::into),
        }
    }
}

impl EventEmitter<Navigate> for ChatScreen {}
impl EventEmitter<ProjectRenameRequest> for ChatScreen {}
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
        result: Result<(), String>,
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
    /// Observe a paired computer without opening its latest conversation or
    /// starting its runtime. The normal event stream keeps these summaries live.
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

    pub(crate) fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        let changed = self.visible != visible;
        self.visible = visible;
        if !visible {
            self.project_folder_dialog = None;
        }
        if changed && visible {
            // Only the mounted sidebar projects every computer. Background
            // views keep their own summaries and the shared catalog handle;
            // they do not rebuild all foreign rows on every update.
            let computers = self.computers.clone();
            self.sidebar
                .update(cx, |sidebar, cx| sidebar.set_computers(computers, cx));
        }
        if visible
            && let Some(id) = &self.selected_session
            && self.completed_unread_sessions.remove(id)
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
            running: self.active_runs.keys().cloned().collect(),
            unread: self.completed_unread_sessions.clone(),
            pinned: self.sidebar.read(cx).pinned_tasks().to_vec(),
            loading: !self.navigation_ready && self.host_recovery.events.is_some(),
            error: self.host_recovery.unavailable.then(|| {
                if self.host_recovery.ended {
                    if self
                        .runtime_error
                        .as_deref()
                        .is_some_and(crate::remote::wire::is_protocol_incompatibility_refusal)
                    {
                        "Update required"
                    } else {
                        "Offline — select to reconnect"
                    }
                } else {
                    "Connecting…"
                }
                .into()
            }),
        }
    }

    pub(crate) fn set_computers(&mut self, computers: Arc<Vec<Computer>>, cx: &mut Context<Self>) {
        self.computers = computers.clone();
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
                    .map(|c| c.label.as_ref())
                    .unwrap_or("Remote computer");
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
        let needs_draft = matches!(destination, Destination::DraftProject(_))
            || (self.navigation_only && !matches!(destination, Destination::Task(_)));
        if needs_draft && !self.prepare_draft_navigation(cx) {
            return;
        }
        if self.navigation_only {
            self.navigation_only = false;
            self.booting = true;
            self.refresh_slash_commands(cx);
            self.start_runtime(cx);
        }
        match destination {
            Destination::Overview => self
                .sidebar
                .update(cx, |s, cx| s.set_project_filter(None, cx)),
            Destination::Task(id) => self.select_session(&id, cx),
            Destination::Project(path) | Destination::DraftProject(path) => {
                self.select_project_root(path, cx);
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

    pub(super) fn finish_navigation_bootstrap(&mut self, cx: &mut Context<Self>) {
        self.navigation_ready = true;
        if let Some((generation, destination)) = self.pending_navigation.take()
            && generation == self.selection_generation
        {
            self.navigate(destination, cx);
        }
    }

    pub(crate) fn navigation_notice(&mut self, message: String, cx: &mut Context<Self>) {
        self.notice = Some(message.into());
        cx.notify();
    }

    pub(super) fn computers_project_menu(&self) -> Menu<Self> {
        let mut menu = Menu::new("computer-project-menu", px(440.))
            .label("Projects")
            .max_height(px(440.))
            .application_vim(self.application_vim_enabled);
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
            for (project_index, project) in computer.projects.iter().enumerate() {
                let request = Navigate {
                    computer: computer.key.clone(),
                    destination: if self.draft {
                        Destination::DraftProject(project.path.clone())
                    } else {
                        Destination::Project(project.path.clone())
                    },
                };
                let mut item = MenuItem::new(
                    SharedString::from(format!("computer-{index}-project-{project_index}")),
                    project.label.clone(),
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
                if computer
                    .projects
                    .iter()
                    .filter(|peer| peer.label == project.label)
                    .count()
                    > 1
                {
                    item = item.note(project.path.clone());
                }
                menu = menu.item(item);
            }
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
                    "New project…",
                    move |_: &mut Self, _, cx| cx.emit(request.clone()),
                )
                .icon("folder-plus"),
            );
        }
        menu.separator().item(
            MenuItem::new(
                "manage-computers",
                "Remote computer settings…",
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

    #[gpui::test]
    fn catalog_marks_only_actual_protocol_refusal_as_update_required(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let cases = [
            (
                Refusal::terminal(Error::Incompatible.to_string()).into_error(),
                "Update required",
            ),
            (
                Refusal::terminal("This computer is no longer paired").into_error(),
                "Offline — select to reconnect",
            ),
            (
                Refusal::terminal(Error::InvalidFrame.to_string()).into_error(),
                "Offline — select to reconnect",
            ),
            (
                Error::Disconnected.to_string(),
                "Offline — select to reconnect",
            ),
        ];
        for (reason, expected) in cases {
            let chat = {
                let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
                let backend = Arc::new(
                    AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap(),
                );
                cx.new(|cx| ChatScreen::new_inner(backend, "account".into(), cx))
            };
            chat.update(cx, |chat, cx| {
                chat.handle_service_event(AgentServiceEvent::RemoteAccessEnded(reason), cx);
                let snapshot = chat.computer_snapshot("Office".into(), cx);
                assert_eq!(snapshot.error.as_deref(), Some(expected));
                assert!(!snapshot.loading);
                // The generic disconnect following a terminal refusal must
                // not hide the reason the authenticated connection ended.
                chat.handle_service_event(AgentServiceEvent::HostDisconnected, cx);
                assert_eq!(
                    chat.computer_snapshot("Office".into(), cx).error.as_deref(),
                    Some(expected)
                );
            });
        }
    }
}
