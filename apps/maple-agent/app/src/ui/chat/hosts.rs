//! The hosts the chat screen shows tasks from: the local host and every
//! saved remote host, which one new tasks target, the sidebar's host
//! filter, and the header's host chip. Calls about a task go to the host
//! that owns it, whatever host new tasks target.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{Context, Div, SharedString, Window, div, prelude::*, px};
use maple_agent::agent::AgentSessionSummary;
use maple_agent::host::{HostBackend, HostBootstrap, HostEvent, HostId, HostSessionDefaults};
use maple_remote::hosts::SavedHost;
use maple_remote::manager::{HostManagerEvent, HostStatus};

use super::{ChatScreen, OpenSettingsSection, Section, sidebar};
use crate::ui::popup::{Menu, MenuItem};
use crate::ui::theme;

/// What the local host is called wherever hosts are listed.
pub(super) const LOCAL_HOST_NAME: &str = "This computer";

/// One host the chat screen shows tasks from: the local host always, and
/// each remote host as it connects or as a saved entry waiting to.
pub(super) struct ChatHost {
    /// Absent for a saved host that is not connected.
    pub(super) backend: Option<Arc<dyn HostBackend>>,
    pub(super) name: String,
    pub(super) online: bool,
    /// Bumped on every connection state change. A call made on one
    /// connection whose answer lands on another is dropped: the host was
    /// re-read when it came back.
    pub(super) connection: u64,
    /// The host's project context, as its bootstrap reported it and as
    /// the user changed it while the host was the target; adopted again
    /// when the host becomes the target.
    pub(super) project_root: Option<String>,
    pub(super) recent_roots: Vec<String>,
    pub(super) session_defaults: Option<HostSessionDefaults>,
}

impl ChatHost {
    pub(super) fn local(backend: Arc<dyn HostBackend>) -> Self {
        Self {
            backend: Some(backend),
            name: LOCAL_HOST_NAME.to_string(),
            online: true,
            connection: 0,
            project_root: None,
            recent_roots: Vec::new(),
            session_defaults: None,
        }
    }

    pub(super) fn saved(name: String) -> Self {
        Self {
            backend: None,
            name,
            online: false,
            connection: 0,
            project_root: None,
            recent_roots: Vec::new(),
            session_defaults: None,
        }
    }
}

/// What reading a freshly connected host produced.
pub(super) struct RemoteBootstrap {
    pub(super) boot: HostBootstrap,
    /// Why its runtime did not start, if it did not.
    pub(super) start_error: Option<String>,
    /// Stored tool summaries of its latest task, when that task will open.
    pub(super) summaries: HashMap<String, String>,
}

impl ChatScreen {
    /// Every known host, local first, then by name.
    pub(super) fn sorted_hosts(&self) -> Vec<sidebar::SidebarHost> {
        let mut hosts: Vec<sidebar::SidebarHost> = self
            .hosts
            .iter()
            .map(|(id, entry)| sidebar::SidebarHost {
                id: id.clone(),
                name: SharedString::from(entry.name.clone()),
                online: entry.online,
            })
            .collect();
        hosts.sort_by(|a, b| {
            b.id.is_local()
                .cmp(&a.id.is_local())
                .then_with(|| a.name.as_ref().cmp(b.name.as_ref()))
        });
        hosts
    }

    /// `hosts` changed (a host came, went, or was renamed): rebuild what
    /// renders from it.
    pub(super) fn hosts_changed(&mut self) {
        self.host_list = self.sorted_hosts();
        self.hosts_dirty = true;
        self.refresh_target_host_label();
    }

    pub(super) fn refresh_target_host_label(&mut self) {
        self.target_host_label = SharedString::from(self.host_name(&self.target_host));
        // A rename or a first status reaches the badge through here too.
        if let Some((owner, _)) = self.selected_host.clone() {
            self.selected_host = Some((owner.clone(), SharedString::from(self.host_name(&owner))));
        }
    }

    pub(super) fn host_name(&self, id: &HostId) -> String {
        self.hosts
            .get(id)
            .map(|entry| entry.name.clone())
            .unwrap_or_else(|| id.to_string())
    }

    /// The host that owns `session_id`; the local host when unknown.
    pub(super) fn host_of(&self, session_id: &str) -> HostId {
        self.session_hosts
            .get(session_id)
            .cloned()
            .unwrap_or_else(HostId::local)
    }

    /// The backend that owns `session_id`: every call about that task goes
    /// there, whatever host new tasks target. The target's backend stands
    /// in for a task whose host is unknown.
    pub(super) fn backend_for(&self, session_id: &str) -> Arc<dyn HostBackend> {
        let owner = self.host_of(session_id);
        self.hosts
            .get(&owner)
            .and_then(|entry| entry.backend.clone())
            .unwrap_or_else(|| self.host.clone())
    }

    /// The backend of the task on screen; the target's with none.
    pub(super) fn session_backend(&self) -> Arc<dyn HostBackend> {
        match self.selected_session.as_deref() {
            Some(session_id) => self.backend_for(session_id),
            None => self.host.clone(),
        }
    }

    /// File `session_id` under `host` unless it has a host already.
    pub(super) fn file_session(&mut self, session_id: &str, host: &HostId) {
        if self.session_hosts.contains_key(session_id) {
            return;
        }
        self.session_hosts
            .insert(session_id.to_string(), host.clone());
        self.hosts_dirty = true;
    }

    /// Make `host` the target of new tasks. The selected task's host is
    /// the target unless the sidebar filters on one. Returns whether
    /// `host` is the target: an offline or unknown host cannot take new
    /// tasks and is refused.
    pub(super) fn set_target_host(&mut self, host: HostId, cx: &mut Context<Self>) -> bool {
        if self.target_host == host {
            return true;
        }
        let Some((backend, recent_roots, defaults)) = self
            .hosts
            .get(&host)
            .filter(|entry| entry.online)
            .and_then(|entry| {
                Some((
                    entry.backend.clone()?,
                    entry.recent_roots.clone(),
                    entry.session_defaults.clone(),
                ))
            })
        else {
            return false;
        };
        self.target_host = host;
        self.host = backend;
        self.popup.close(cx);
        self.refresh_target_host_label();
        self.recent_roots = recent_roots;
        // New tasks take this host's defaults (web access, permission
        // mode), whichever task is on screen.
        if let Some(defaults) = defaults {
            self.apply_session_defaults(&defaults, cx);
        }
        self.refresh_roots(cx);
        self.refresh_slash_commands(cx);
        self.sync_sidebar(cx);
        true
    }

    /// Show the target host's saved project, for when the target changed
    /// without a task selection.
    pub(super) fn adopt_target_host_context(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self
            .hosts
            .get(&self.target_host)
            .map(|entry| entry.project_root.clone())
        else {
            return;
        };
        self.set_project_context(root, cx);
    }

    /// The sidebar filters on `host` (or on none): new tasks go there. A
    /// filter on an offline host is refused, since it could not take them;
    /// the sidebar then shows the filter that stands.
    pub(super) fn set_host_filter(&mut self, host: Option<HostId>, cx: &mut Context<Self>) {
        let target = match &host {
            Some(host) => host.clone(),
            None => self
                .selected_session
                .as_deref()
                .map(|id| self.host_of(id))
                .filter(|owner| self.hosts.get(owner).is_some_and(|entry| entry.online))
                .unwrap_or_else(HostId::local),
        };
        let changed = target != self.target_host;
        if !self.set_target_host(target.clone(), cx) {
            self.notice = Some(format!("{} is offline", self.host_name(&target)).into());
            self.sync_host_filter(cx);
            cx.notify();
            return;
        }
        self.host_filter = host;
        if changed {
            self.follow_target_change(cx);
        }
        cx.notify();
    }

    /// Push the host filter to the sidebar, which shows it.
    pub(super) fn sync_host_filter(&mut self, cx: &mut Context<Self>) {
        let filter = self.host_filter.clone();
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.show_host_filter(filter, cx));
    }

    /// `host` cannot be filtered on any more (offline or removed): a
    /// filter naming it goes, on screen and in the sidebar.
    pub(super) fn clear_host_filter_for(&mut self, host: &HostId, cx: &mut Context<Self>) {
        if self.host_filter.as_ref() != Some(host) {
            return;
        }
        self.host_filter = None;
        self.sync_host_filter(cx);
    }

    /// `host` dropped while it was the target: new tasks go to the local
    /// host, and with no task on screen the header shows its project.
    pub(super) fn fall_back_to_local_host(&mut self, host: &HostId, cx: &mut Context<Self>) {
        self.clear_host_filter_for(host, cx);
        if self.target_host != *host {
            return;
        }
        self.set_target_host(HostId::local(), cx);
        if self.selected_session.is_none() {
            self.follow_target_change(cx);
        }
    }

    /// Hosts a settings screen can point at: every connected one.
    pub(crate) fn connected_hosts(&self) -> Vec<crate::ui::settings::SettingsHost> {
        self.host_list
            .iter()
            .filter(|host| host.online)
            .filter_map(|host| {
                Some(crate::ui::settings::SettingsHost {
                    id: host.id.clone(),
                    name: host.name.to_string(),
                    backend: self.hosts.get(&host.id)?.backend.clone()?,
                })
            })
            .collect()
    }

    /// The saved host list changed: list saved hosts that are not
    /// connected as offline, and drop hosts that were removed.
    pub fn set_saved_hosts(&mut self, saved: Vec<SavedHost>, cx: &mut Context<Self>) {
        let keep: HashSet<HostId> = saved
            .iter()
            .map(|host| HostId::new(host.id.clone()))
            .chain(std::iter::once(HostId::local()))
            .collect();
        let removed: Vec<HostId> = self
            .hosts
            .keys()
            .filter(|id| !keep.contains(id))
            .cloned()
            .collect();
        for id in removed {
            self.drop_host_sessions(&id);
            self.hosts.remove(&id);
            self.fall_back_to_local_host(&id, cx);
        }
        // A remembered host that is not saved any more is not coming back.
        if let Some(host) = self
            .restore_host
            .clone()
            .filter(|host| !keep.contains(host))
        {
            self.give_up_restore(&host, cx);
        }
        for host in saved {
            let id = HostId::new(host.id);
            match self.hosts.get_mut(&id) {
                Some(entry) => entry.name = host.name,
                None => {
                    self.hosts.insert(id, ChatHost::saved(host.name));
                }
            }
        }
        self.hosts_changed();
        self.sync_sidebar(cx);
        cx.notify();
    }

    /// A remote host's connection changed. Online: adopt it and read its
    /// tasks. Otherwise its tasks leave the list until it is back; the
    /// task on screen stays readable.
    pub fn set_remote_host_status(
        &mut self,
        host: HostId,
        name: String,
        status: HostStatus,
        backend: Option<Arc<dyn HostBackend>>,
        cx: &mut Context<Self>,
    ) {
        let online = status == HostStatus::Online;
        let entry = self
            .hosts
            .entry(host.clone())
            .or_insert_with(|| ChatHost::saved(name.clone()));
        let was_online = entry.online;
        entry.name = name;
        entry.online = online;
        entry.connection += 1;
        let connection = entry.connection;
        if let Some(backend) = backend {
            entry.backend = Some(backend);
        }
        self.hosts_changed();
        if online {
            self.bootstrap_remote_host(host, connection, cx);
        } else {
            self.drop_host_sessions(&host);
            self.fall_back_to_local_host(&host, cx);
            if let HostStatus::Offline { reason } = status {
                self.give_up_restore(&host, cx);
                // The drop itself is news; the reconnect attempts that
                // follow report the same thing until the host is back.
                if was_online && reason != "removed" {
                    self.notice = Some(format!("{}: {reason}", self.host_name(&host)).into());
                }
            }
            self.sync_sidebar(cx);
        }
        cx.notify();
    }

    /// Forget a host's tasks in the list and their runs. The selected task
    /// keeps its host mapping so its screen stays coherent.
    pub(super) fn drop_host_sessions(&mut self, host: &HostId) {
        let selected = self.selected_session.clone();
        let gone: HashSet<String> = self
            .session_hosts
            .iter()
            .filter(|(_, owner)| *owner == host)
            .map(|(id, _)| id.clone())
            .collect();
        self.sessions.retain(|session| !gone.contains(&session.id));
        for id in &gone {
            self.active_runs.remove(id);
            self.completed_unread_sessions.remove(id);
            if selected.as_deref() != Some(id.as_str()) {
                self.session_hosts.remove(id);
                self.hosts_dirty = true;
            }
        }
    }

    /// Read a freshly connected host: its tasks, roots, and defaults, and
    /// start its runtime so it can run them. `connection` names the
    /// connection the read is for; an answer from an earlier one is stale.
    pub(super) fn bootstrap_remote_host(
        &mut self,
        host: HostId,
        connection: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(backend) = self
            .hosts
            .get(&host)
            .and_then(|entry| entry.backend.clone())
        else {
            return;
        };
        let target = host.clone();
        let restoring = self.restore_host.as_ref() == Some(&host);
        self.call(
            async move {
                let boot = backend.bootstrap().await?;
                let start_error = backend.start_runtime(None).await.err();
                // Only the restored host opens its latest task, so only it
                // needs that task's stored summaries.
                let summaries = match boot.latest.as_ref().filter(|_| restoring) {
                    Some(detail) => backend
                        .tool_summaries(detail.session.id.clone())
                        .await
                        .unwrap_or_else(|error| {
                            log::warn!("Cannot load tool summaries: {error}");
                            HashMap::new()
                        }),
                    None => HashMap::new(),
                };
                Ok::<_, String>(RemoteBootstrap {
                    boot,
                    start_error,
                    summaries,
                })
            },
            cx,
            move |this, result, cx| this.finish_remote_bootstrap(target, connection, result, cx),
        );
    }

    /// Whether `host` is still on the connection a call was made on.
    pub(super) fn on_connection(&self, host: &HostId, connection: u64) -> bool {
        self.hosts
            .get(host)
            .is_some_and(|entry| entry.connection == connection)
    }

    /// The bootstrap of `target` came back. A failure releases a startup
    /// held for that host: it will not open its task. An answer from a
    /// connection that has since dropped or been replaced is stale: the
    /// host's tasks left with it, or its new connection reads it afresh.
    pub(super) fn finish_remote_bootstrap(
        &mut self,
        target: HostId,
        connection: u64,
        result: Result<RemoteBootstrap, String>,
        cx: &mut Context<Self>,
    ) {
        if !self.on_connection(&target, connection) {
            return;
        }
        match result {
            Ok(RemoteBootstrap {
                boot,
                start_error,
                summaries,
            }) => {
                self.apply_remote_bootstrap(target, boot, start_error, summaries, cx);
            }
            Err(message) => {
                self.notice = Some(format!("{}: {message}", self.host_name(&target)).into());
                self.give_up_restore(&target, cx);
            }
        }
        cx.notify();
    }

    /// A remote host answered its bootstrap. When it is the host the last
    /// new task ran on, it becomes the target again and its latest task
    /// opens, unless a task was chosen meanwhile or the user started a
    /// draft: that draft follows the target, its text kept.
    pub(super) fn apply_remote_bootstrap(
        &mut self,
        target: HostId,
        boot: HostBootstrap,
        start_error: Option<String>,
        summaries: HashMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(entry) = self.hosts.get_mut(&target) {
            entry.project_root = boot.project_root.clone();
            entry.recent_roots = boot.recent_roots.clone();
            entry.session_defaults = Some(boot.session_defaults.clone());
        }
        self.apply_host_session_list(&target, boot.sessions, cx);
        if let Some(error) = start_error {
            self.notice = Some(
                format!(
                    "{}: runtime failed to start: {error}",
                    self.host_name(&target)
                )
                .into(),
            );
        }
        let restoring = self.restore_host.as_ref() == Some(&target);
        if restoring {
            self.restore_host = None;
            if self.selected_session.is_none() && self.host_filter.is_none() {
                self.set_target_host(target.clone(), cx);
            }
        }
        if self.target_host == target {
            self.recent_roots = boot.recent_roots;
            self.adopt_target_host_context(cx);
            match boot
                .latest
                .filter(|_| restoring && self.selected_session.is_none() && !self.draft)
            {
                Some(detail) => {
                    let summaries = summaries
                        .into_iter()
                        .map(|(id, summary)| (id, SharedString::from(summary)))
                        .collect();
                    self.upsert_session(detail.session.clone(), cx);
                    self.set_active_session(detail.session, detail.timeline, summaries, cx);
                    self.queue = detail.queue.items;
                }
                // The draft on screen is for this host: its chip lists
                // what a task created there starts with.
                None if self.selected_session.is_none() => self.refresh_draft_mcp(cx),
                None => {}
            }
        }
    }

    /// The remembered host will not come: stop holding startup for it and
    /// let the local auto-select run.
    pub(super) fn give_up_restore(&mut self, host: &HostId, cx: &mut Context<Self>) {
        if self.restore_host.as_ref() != Some(host) {
            return;
        }
        self.restore_host = None;
        if self.selected_session.is_none() {
            self.refresh_sessions(cx);
        }
    }

    /// Everything the connection manager reports, in one batch. A repaint
    /// is requested once per batch, however many events change something.
    pub fn handle_manager_events(&mut self, events: Vec<HostManagerEvent>, cx: &mut Context<Self>) {
        for event in events {
            match event {
                HostManagerEvent::Event { host, event } => {
                    self.handle_remote_host_events(host, vec![event], cx);
                }
                HostManagerEvent::Status {
                    host,
                    name,
                    status,
                    backend,
                } => self.set_remote_host_status(
                    host,
                    name,
                    status,
                    backend.map(|backend| backend as Arc<dyn HostBackend>),
                    cx,
                ),
                HostManagerEvent::HostsChanged(hosts) => self.set_saved_hosts(hosts, cx),
            }
        }
    }

    /// Events from a remote host; dropped once it is offline.
    pub fn handle_remote_host_events(
        &mut self,
        host: HostId,
        events: Vec<HostEvent>,
        cx: &mut Context<Self>,
    ) {
        if !self.hosts.get(&host).is_some_and(|entry| entry.online) {
            return;
        }
        self.apply_host_events(&host, events, cx);
    }

    /// Replace one host's tasks in the merged list. A task the list names
    /// leaves whatever host it was filed under: the list is the truth
    /// about where it lives, and one row per task is the invariant.
    pub(super) fn apply_host_session_list(
        &mut self,
        host: &HostId,
        sessions: Vec<AgentSessionSummary>,
        cx: &mut Context<Self>,
    ) {
        let listed: HashSet<&str> = sessions.iter().map(|session| session.id.as_str()).collect();
        let local = HostId::local();
        let session_hosts = &self.session_hosts;
        self.sessions.retain(|session| {
            !listed.contains(session.id.as_str())
                && session_hosts.get(&session.id).unwrap_or(&local) != host
        });
        // A task the host no longer lists is gone from it; the task on
        // screen keeps its mapping so its calls still know where to go.
        let selected = self.selected_session.clone();
        self.session_hosts.retain(|id, owner| {
            owner != host || listed.contains(id.as_str()) || selected.as_deref() == Some(id)
        });
        for session in &sessions {
            self.session_hosts.insert(session.id.clone(), host.clone());
        }
        self.hosts_dirty = true;
        self.sessions.extend(sessions);
        self.sessions
            .sort_by_key(|session| std::cmp::Reverse(session.updated_ms));
        self.sync_sidebar(cx);
    }

    /// Re-read every connected host's task list. The sidebar groups tasks
    /// by project, so each host lists every root; a task's stored root
    /// remains authoritative when it is opened or run.
    pub(super) fn refresh_sessions(&self, cx: &mut Context<Self>) {
        let generation = self.selection_generation;
        for (id, entry) in &self.hosts {
            let Some(backend) = entry.backend.clone().filter(|_| entry.online) else {
                continue;
            };
            let id = id.clone();
            let connection = entry.connection;
            self.call(
                async move { backend.list_sessions(None).await },
                cx,
                move |this, result, cx| {
                    this.apply_listed_sessions(&id, connection, generation, result, cx)
                },
            );
        }
    }

    /// One host answered `refresh_sessions`. A list from a connection that
    /// has since changed is stale: the host's tasks left with it, or its
    /// new connection lists them again.
    pub(super) fn apply_listed_sessions(
        &mut self,
        host: &HostId,
        connection: u64,
        generation: u64,
        result: Result<Vec<AgentSessionSummary>, String>,
        cx: &mut Context<Self>,
    ) {
        if !self.on_connection(host, connection) {
            return;
        }
        match result {
            Ok(sessions) if host.is_local() => self.apply_session_list(sessions, generation, cx),
            Ok(sessions) => self.apply_host_session_list(host, sessions, cx),
            Err(message) => self.notice = Some(message.into()),
        }
        cx.notify();
    }

    /// The user chose a host in the header chip: new tasks go there, and
    /// the project context follows that host.
    pub(super) fn pick_host(&mut self, host: HostId, cx: &mut Context<Self>) {
        self.popup.close(cx);
        let changed = host != self.target_host;
        if !self.set_target_host(host.clone(), cx) {
            self.notice = Some(format!("{} is offline", self.host_name(&host)).into());
        } else if changed {
            self.follow_target_change(cx);
        }
        cx.notify();
    }

    /// The target moved: show its project and defaults. No task exists
    /// until the first message is sent, so the draft on screen simply
    /// follows, keeping its text; the message will run where the header
    /// says, and its chip lists what a task created there starts with.
    pub(super) fn follow_target_change(&mut self, cx: &mut Context<Self>) {
        if self.selection_is_draft() {
            // An empty task pinned to the old target would take the first
            // message there; leave it and let the send create the task
            // on the new one. What replaces it is a draft: the new host's
            // task list must not open a task over the text typed so far.
            self.clear_selected_session_presentation(cx);
            self.draft = true;
        }
        self.adopt_target_host_context(cx);
        if self.draft {
            self.refresh_draft_mcp(cx);
        }
    }

    pub(super) fn target_host_online(&self) -> bool {
        self.hosts
            .get(&self.target_host)
            .is_some_and(|entry| entry.online)
    }

    /// The host chip's menu: every known host with its state, then a way
    /// to the Hosts settings. Opened through the chat's popup
    /// (`ChatPopup::Host`).
    pub(super) fn host_menu(&self) -> Menu<Self> {
        let mut menu = Menu::new("host-menu", px(300.))
            .label("Hosts")
            .application_vim(self.application_vim_enabled)
            .header("NEW TASKS RUN ON");
        for host in &self.host_list {
            let online = host.online;
            let pick = host.id.clone();
            menu = menu.item(
                MenuItem::new(
                    SharedString::from(format!("host-menu-{}", host.id)),
                    host.name.clone(),
                    move |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                        this.pick_host(pick.clone(), cx);
                    },
                )
                .content(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(status_dot(online))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .line_clamp(1)
                                .text_ellipsis()
                                .child(host.name.clone()),
                        )
                        .when(!online, |row| {
                            row.child(
                                div()
                                    .text_xs()
                                    .text_color(gpui::rgb(theme::text_muted()))
                                    .child("offline"),
                            )
                        }),
                )
                .current(host.id == self.target_host)
                .style(move |row| {
                    row.when(!online, |row| {
                        row.text_color(gpui::rgb(theme::text_muted()))
                    })
                }),
            );
        }
        menu.separator().item(
            MenuItem::new(
                "host-menu-manage",
                "Manage hosts\u{2026}",
                |_: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                    cx.emit(OpenSettingsSection(Section::Hosts));
                },
            )
            .style(|row| row.text_color(gpui::rgb(theme::text_secondary()))),
        )
    }
}

/// A small live-state dot: green online, muted otherwise.
pub(super) fn status_dot(online: bool) -> Div {
    div()
        .flex_none()
        .size(px(8.))
        .rounded_full()
        .bg(gpui::rgb(if online {
            theme::status_success()
        } else {
            theme::text_muted()
        }))
}
