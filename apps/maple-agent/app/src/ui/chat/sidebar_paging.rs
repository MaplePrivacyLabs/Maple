//! Sidebar page ownership and request generations. The window retains summaries
//! only for loaded pages and live tasks; history remains in the host store.
use std::collections::VecDeque;

use maple_agent::store::{NavigationSearchQuery, NavigationTaskQuery, TaskSummary};
use maple_agent::store::{PinFilter, TaskSection, TaskState};

use super::*;

const MAX_LIVE_SUMMARIES: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::ui::chat) enum PageKind {
    Pinned,
    Active,
    Settled,
    Archived,
    Search,
    Projects,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str, state: AgentTaskState) -> AgentSessionSummary {
        AgentSessionSummary {
            id: id.into(),
            title: id.into(),
            project_root: std::env::temp_dir().to_string_lossy().into_owned(),
            created_ms: 1,
            updated_ms: 1,
            message_count: 1,
            model: None,
            web_enabled: true,
            state,
            acp: false,
        }
    }

    fn page_summary(id: &str) -> TaskSummary {
        let summary = summary(id, AgentTaskState::Active);
        TaskSummary {
            uuid: summary.id,
            title: summary.title,
            project_root: summary.project_root,
            project_display_name: None,
            state: "active".into(),
            kind: "desktop".into(),
            pinned_ms: None,
            web_enabled: summary.web_enabled,
            model: summary.model,
            message_count: summary.message_count as u64,
            created_ms: summary.created_ms,
            updated_ms: summary.updated_ms,
            truncated: false,
        }
    }

    #[gpui::test]
    fn empty_task_pages_keep_navigation_without_an_empty_message(cx: &mut gpui::TestAppContext) {
        let _settings = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let chat = cx.new(|cx| ChatScreen::new_without_start(backend, "user".into(), cx));
        let sidebar = chat.read_with(cx, |chat, _| chat.sidebar.clone());
        sidebar.update(cx, |sidebar, _| {
            sidebar.reset_persisted_for_test();
            sidebar.pages.enabled = true;
            for complete in [false, true] {
                if complete {
                    sidebar.pages.ready.extend([
                        PageKind::Active,
                        PageKind::Settled,
                        PageKind::Pinned,
                    ]);
                }
                for group in [
                    SidebarGroup::Project,
                    SidebarGroup::Status,
                    SidebarGroup::List,
                ] {
                    sidebar.view.group = group;
                    sidebar.rebuild_sections();
                    assert_eq!(
                        sidebar.entries,
                        [
                            SidebarEntry::NewTask,
                            SidebarEntry::ProjectsHeader,
                            SidebarEntry::ArchivedHeader,
                        ]
                    );
                }
            }
        });
    }

    #[gpui::test]
    fn fresh_empty_pages_do_not_publish_retained_display_rows(cx: &mut gpui::TestAppContext) {
        let _settings = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let chat = cx.new(|cx| ChatScreen::new_without_start(backend, "user".into(), cx));
        let published = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        chat.update(cx, |chat, cx| {
            let published = published.clone();
            cx.subscribe(&chat.sidebar, move |_, _, event: &SidebarEvent, _| {
                if let SidebarEvent::PagesReady { sessions, .. } = event {
                    published.borrow_mut().push(sessions.clone());
                }
            })
            .detach();
        });
        let sidebar = chat.read_with(cx, |chat, _| chat.sidebar.clone());
        sidebar.update(cx, |sidebar, cx| {
            sidebar.selected = None;
            sidebar.running.clear();
            // The prior grouped projection stays visible during refresh; the
            // new authoritative ordinary pages have all completed empty.
            sidebar.sessions = vec![summary("display-only", AgentTaskState::Active)];
            sidebar.pages = SidebarPages {
                enabled: true,
                ready: HashSet::from([PageKind::Active, PageKind::Settled, PageKind::Pinned]),
                autoselect_generation: Some(0),
                ..SidebarPages::default()
            };
            sidebar.pages_ready(cx);
        });
        let published = published.borrow();
        assert_eq!(published.len(), 1);
        assert!(published[0].is_empty());
    }

    #[gpui::test]
    fn known_empty_folders_wait_for_complete_task_pages(cx: &mut gpui::TestAppContext) {
        let _settings = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let chat = cx.new(|cx| ChatScreen::new_without_start(backend, "user".into(), cx));
        let sidebar = chat.read_with(cx, |chat, _| chat.sidebar.clone());
        sidebar.update(cx, |sidebar, cx| {
            sidebar.reset_persisted_for_test();
            sidebar.view = SidebarView::default();
            sidebar.pages.enabled = true;
            sidebar.set_application_vim_enabled(true, cx);
            sidebar.set_project_name_for_test("/empty", "Empty");
            let note = SidebarEntry::ProjectEmpty(0);
            assert!(
                !sidebar.entries.contains(&note),
                "unrequested pages are unknown"
            );
            for kind in [PageKind::Active, PageKind::Settled, PageKind::Pinned] {
                sidebar.pages.ready.insert(kind);
                sidebar.rebuild_sections();
                assert_eq!(sidebar.entries.contains(&note), kind == PageKind::Pinned);
            }
            let row = sidebar
                .entries
                .iter()
                .position(|entry| *entry == note)
                .unwrap();
            assert!(sidebar.vim_by_row[row].is_none());
            assert!(sidebar.entry_sections[row].is_none());
            for kind in [PageKind::Active, PageKind::Settled, PageKind::Pinned] {
                sidebar.pages.next.insert(kind, "more".into());
                sidebar.rebuild_sections();
                assert!(!sidebar.entries.contains(&note), "continuation is unknown");
                sidebar.pages.next.remove(&kind);
                sidebar.pages.loading.insert(kind);
                sidebar.rebuild_sections();
                assert!(
                    !sidebar.entries.contains(&note),
                    "in-flight pages are unknown"
                );
                sidebar.pages.loading.remove(&kind);
                sidebar.pages.failed.insert(kind);
                sidebar.rebuild_sections();
                assert!(!sidebar.entries.contains(&note), "failed pages are unknown");
                sidebar.pages.failed.remove(&kind);
            }
            sidebar.pages.pin_batches.push_back(vec!["pin".into()]);
            sidebar.rebuild_sections();
            assert!(!sidebar.entries.contains(&note));
            sidebar.pages.pin_batches.clear();
            sidebar.set_project_expanded("/empty", Some(false), cx);
            assert!(!sidebar.entries.contains(&note));
            sidebar.set_project_expanded("/empty", Some(true), cx);
            assert!(sidebar.entries.contains(&note));
            sidebar.set_projects_expanded(false, cx);
            assert!(!sidebar.entries.contains(&note));
            sidebar.set_projects_expanded(true, cx);
            sidebar.set_filter_for_test("empty", cx);
            assert!(
                !sidebar.entries.contains(&note),
                "search never shows the note"
            );
            sidebar.set_filter_for_test("", cx);
            assert!(sidebar.entries.contains(&note));
            sidebar.apply_task_page(
                PageKind::Pinned,
                vec![TaskSummary {
                    project_root: "/empty".into(),
                    ..page_summary("pin")
                }],
                false,
            );
            sidebar.rebuild_sections();
            assert!(
                !sidebar.entries.contains(&note),
                "a pinned-only project is not empty"
            );
        });
    }

    #[gpui::test]
    fn scoped_pages_exclude_retained_tasks_from_other_projects(cx: &mut gpui::TestAppContext) {
        let _settings = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let chat = cx.new(|cx| ChatScreen::new_without_start(backend, "user".into(), cx));
        let sidebar = chat.read_with(cx, |chat, _| chat.sidebar.clone());
        sidebar.update(cx, |sidebar, cx| {
            sidebar.reset_persisted_for_test();
            sidebar.view = SidebarView {
                group: SidebarGroup::Status,
                ..SidebarView::default()
            };
            sidebar.computers = Arc::new(vec![
                Computer::pending("one".into(), "One".into(), None),
                Computer::pending("two".into(), "Two".into(), None),
            ]);
            for state in [
                AgentTaskState::Active,
                AgentTaskState::Settled,
                AgentTaskState::Archived,
            ] {
                sidebar.sessions = vec![
                    AgentSessionSummary {
                        project_root: "/a".into(),
                        ..summary("running-a", AgentTaskState::Active)
                    },
                    AgentSessionSummary {
                        project_root: "/a".into(),
                        ..summary("selected-a", state)
                    },
                ];
                sidebar.running = HashSet::from(["running-a".into()]);
                sidebar.selected = Some("selected-a".into());
                sidebar.project_filter = Some("/b".into());
                sidebar.refresh_pages(cx);
                assert_eq!(sidebar.sessions.len(), 2, "both A tasks remain cached");
                sidebar.pinned_tasks = vec!["running-a".into()];
                sidebar.apply_task_page(
                    PageKind::Active,
                    vec![TaskSummary {
                        project_root: "/b".into(),
                        ..page_summary("b")
                    }],
                    false,
                );
                // Active is the only nonempty task reply in this scoped fixture.
                sidebar.pages.loading.clear();
                sidebar
                    .pages
                    .ready
                    .extend([PageKind::Active, PageKind::Settled, PageKind::Pinned]);
                sidebar.archived_expanded = true;
                sidebar.rebuild_sections();
                let shown: Vec<_> = sidebar
                    .entries
                    .iter()
                    .filter_map(|entry| match entry {
                        SidebarEntry::Task(task) => {
                            Some(sidebar.sessions[task.session].id.as_str())
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(shown, ["b"]);
                assert!(sidebar.pinned_rows.is_empty());
                assert!(sidebar.archived_rows.is_empty());
                assert!(sidebar.grouped_project_names().is_empty());
                assert_eq!(sidebar.active_rows(), [2]);
                assert!(
                    sidebar
                        .entries
                        .contains(&SidebarEntry::SectionLabel(SidebarSection::Active))
                );
                let generation = sidebar.pages.generation;
                sidebar.set_computers(Arc::new(Vec::new()), cx);
                assert!(sidebar.project_filter.is_none());
                assert_eq!(sidebar.pages.generation, generation + 1);
                assert!(!sidebar.pages.contains("b", false));
                assert_eq!(
                    sidebar.sessions.len(),
                    2,
                    "only live A tasks survive refresh"
                );
                assert_eq!(sidebar.view.group, SidebarGroup::Status);
                assert!(sidebar.grouped_project_names().is_empty());
                assert_eq!(
                    sidebar
                        .project_roots
                        .iter()
                        .map(|project| project.root.as_str())
                        .collect::<Vec<_>>(),
                    ["/a"]
                );
                sidebar.set_computers(
                    Arc::new(vec![
                        Computer::pending("one".into(), "One".into(), None),
                        Computer::pending("two".into(), "Two".into(), None),
                    ]),
                    cx,
                );
            }
        });
    }

    #[gpui::test]
    fn metadata_only_project_pages_add_visible_folders(cx: &mut gpui::TestAppContext) {
        let _settings = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let chat = cx.new(|cx| ChatScreen::new_without_start(backend, "user".into(), cx));
        let sidebar = chat.read_with(cx, |chat, _| chat.sidebar.clone());
        sidebar.update(cx, |sidebar, cx| {
            sidebar.reset_persisted_for_test();
            sidebar.view = SidebarView::default();
            sidebar.pages.enabled = true;
            let project = |root: &str| maple_agent::store::ProjectSummary {
                root: root.into(),
                display_name: None,
                name_revision: 1,
                last_used_ms: 1,
                is_default_workspace: false,
                position: None,
                truncated: false,
            };
            sidebar.apply_project_page(
                maple_agent::store::NavigationProjectPage {
                    projects: vec![project("/a")],
                    next_cursor: Some("next".into()),
                },
                false,
                cx,
            );
            sidebar.rebuild_sections();
            assert_eq!(sidebar.grouped_project_names(), ["a"]);
            assert!(
                sidebar
                    .entries
                    .contains(&SidebarEntry::SectionLabel(SidebarSection::Project(0)))
            );
            assert!(
                sidebar
                    .entries
                    .contains(&SidebarEntry::LoadMore(PageKind::Projects))
            );
            sidebar.apply_project_page(
                maple_agent::store::NavigationProjectPage {
                    projects: vec![project("/b")],
                    next_cursor: None,
                },
                true,
                cx,
            );
            sidebar.rebuild_sections();
            assert_eq!(sidebar.grouped_project_names(), ["a", "b"]);
            assert_eq!(
                sidebar
                    .entries
                    .iter()
                    .filter(|entry| matches!(
                        entry,
                        SidebarEntry::SectionLabel(SidebarSection::Project(_))
                    ))
                    .count(),
                2
            );
            assert!(
                !sidebar
                    .entries
                    .iter()
                    .any(|entry| matches!(entry, SidebarEntry::Empty(_)))
            );
            for group in [SidebarGroup::Status, SidebarGroup::List] {
                sidebar.view.group = group;
                sidebar.rebuild_sections();
                assert!(
                    !sidebar
                        .entries
                        .iter()
                        .any(|entry| matches!(entry, SidebarEntry::Empty(_)))
                );
            }
            sidebar
                .pages
                .ready
                .extend([PageKind::Active, PageKind::Settled, PageKind::Pinned]);
            for group in [SidebarGroup::Status, SidebarGroup::List] {
                sidebar.view.group = group;
                sidebar.rebuild_sections();
                assert!(
                    !sidebar
                        .entries
                        .iter()
                        .any(|entry| matches!(entry, SidebarEntry::Empty(_)))
                );
            }
            sidebar.view.group = SidebarGroup::Project;
            sidebar.apply_task_page(
                PageKind::Active,
                vec![TaskSummary {
                    project_root: "/b".into(),
                    ..page_summary("b-task")
                }],
                false,
            );
            sidebar.rebuild_sections();
            assert_eq!(
                sidebar.grouped_project_names(),
                ["a", "b"],
                "loading tasks does not reorder project containers"
            );
            sidebar.set_filter_for_test("a", cx);
            assert_eq!(sidebar.grouped_project_names(), ["a"]);
            sidebar.set_filter_for_test("unmatched", cx);
            assert!(sidebar.grouped_project_names().is_empty());
        });
    }

    /// Folder projection must not consume cursors or strand tasks between pages.
    #[gpui::test]
    fn folder_pages_preserve_continuations_and_fold_boundaries(cx: &mut gpui::TestAppContext) {
        let _settings = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let chat = cx.new(|cx| ChatScreen::new_without_start(backend, "user".into(), cx));
        let sidebar = chat.read_with(cx, |chat, _| chat.sidebar.clone());
        sidebar.update(cx, |sidebar, cx| {
            sidebar.reset_persisted_for_test();
            sidebar.view = SidebarView::default();
            sidebar.pages = SidebarPages {
                enabled: true,
                ..SidebarPages::default()
            };
            sidebar.selected = None;
            let row = |id: &str, root: &str| TaskSummary {
                project_root: root.into(),
                updated_ms: match id {
                    "a1" => 40,
                    "a2" => 30,
                    "b1" => 20,
                    "c1" => 10,
                    _ => 1,
                },
                ..page_summary(id)
            };
            sidebar.apply_task_page(PageKind::Pinned, vec![row("pin", "/a")], false);
            sidebar.apply_task_page(PageKind::Active, vec![row("a1", "/a")], false);
            sidebar.apply_task_page(
                PageKind::Settled,
                vec![TaskSummary {
                    state: "settled".into(),
                    ..row("b1", "/b")
                }],
                false,
            );
            sidebar.apply_task_page(
                PageKind::Archived,
                vec![TaskSummary {
                    state: "archived".into(),
                    ..row("old", "/a")
                }],
                false,
            );
            for kind in [
                PageKind::Pinned,
                PageKind::Active,
                PageKind::Settled,
                PageKind::Archived,
                PageKind::Projects,
            ] {
                sidebar.pages.next.insert(kind, "continuation".into());
            }
            sidebar.archived_expanded = true;
            sidebar.rebuild_sections();
            assert_eq!(sidebar.grouped_project_names(), ["a", "b"]);
            assert_eq!(sidebar.pinned_rows.len(), 1);
            assert_eq!(sidebar.archived_rows.len(), 1);
            sidebar.apply_task_page(
                PageKind::Active,
                vec![row("a2", "/a"), row("c1", "/c")],
                true,
            );
            sidebar.rebuild_sections();
            assert_eq!(sidebar.grouped_project_names(), ["a", "b", "c"]);
            assert_eq!(
                sidebar
                    .sections
                    .iter()
                    .map(|(_, rows)| rows.len())
                    .sum::<usize>(),
                2,
                "rows beyond the unread Settled frontier stay cached"
            );
            let more = |sidebar: &Sidebar| {
                sidebar
                    .entries
                    .iter()
                    .filter_map(|entry| match entry {
                        SidebarEntry::LoadMore(kind) => Some(*kind),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                more(sidebar),
                [
                    PageKind::Pinned,
                    PageKind::Settled,
                    PageKind::Projects,
                    PageKind::Archived
                ]
            );
            sidebar.set_projects_expanded(false, cx);
            assert_eq!(more(sidebar), [PageKind::Pinned, PageKind::Archived]);
            assert!(
                !sidebar
                    .entries
                    .iter()
                    .any(|entry| matches!(entry, SidebarEntry::Empty(_)))
            );
            sidebar.set_projects_expanded(true, cx);
            assert_eq!(sidebar.pages.ids[&PageKind::Active], ["a1", "a2", "c1"]);
            assert_eq!(sidebar.pages.next[&PageKind::Active], "continuation");
            sidebar.view.group = SidebarGroup::Status;
            sidebar.rebuild_sections();
            sidebar.toggle_section_expanded(SidebarSection::Active, cx);
            assert!(!more(sidebar).contains(&PageKind::Active));
            assert!(more(sidebar).contains(&PageKind::Settled));
            sidebar.view.group = SidebarGroup::List;
            sidebar.rebuild_sections();
            assert_eq!(sidebar.pages.ids[&PageKind::Settled], ["b1"]);
            assert!(more(sidebar).contains(&PageKind::Settled));
            assert!(!more(sidebar).contains(&PageKind::Active));
            assert_eq!(sidebar.section_rows(SidebarSection::List).len(), 2);
        });
    }

    #[gpui::test]
    fn live_task_insert_preserves_the_loaded_cursor_boundary(cx: &mut gpui::TestAppContext) {
        let _settings = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let chat = cx.new(|cx| ChatScreen::new_without_start(backend, "user".into(), cx));
        let sidebar = chat.read_with(cx, |chat, _| chat.sidebar.clone());
        for count in [50, 100] {
            for has_more in [false, true] {
                sidebar.update(cx, |sidebar, cx| {
                    sidebar.reset_persisted_for_test();
                    sidebar.pages = SidebarPages {
                        enabled: true,
                        ..SidebarPages::default()
                    };
                    sidebar.selected = None;
                    sidebar.running.clear();
                    let original: Vec<_> = (0..count).map(|i| format!("loaded-{i}")).collect();
                    sidebar.sessions = original
                        .iter()
                        .map(|id| summary(id, AgentTaskState::Active))
                        .collect();
                    sidebar.pages.ids.insert(PageKind::Active, original.clone());
                    if has_more {
                        sidebar
                            .pages
                            .next
                            .insert(PageKind::Active, "continuation".into());
                    }
                    // Moving a task from another section and then updating an
                    // already loaded task must leave the old cursor row visible.
                    sidebar
                        .pages
                        .ids
                        .insert(PageKind::Settled, vec!["moved".into()]);
                    sidebar
                        .sessions
                        .push(summary("moved", AgentTaskState::Settled));
                    sidebar.apply_live_summary(summary("moved", AgentTaskState::Active), cx);
                    sidebar.apply_live_summary(summary("new", AgentTaskState::Active), cx);
                    sidebar.pinned_tasks.push("pinned".into());
                    sidebar.apply_live_summary(summary("pinned", AgentTaskState::Active), cx);
                    sidebar.apply_live_summary(summary(&original[0], AgentTaskState::Active), cx);
                    // The first-page read began before the pushed move. Its
                    // older snapshot must not erase the newer membership.
                    sidebar.apply_task_page(
                        PageKind::Active,
                        original.iter().map(|id| page_summary(id)).collect(),
                        false,
                    );
                    sidebar.rebuild_sections();
                    let loaded = &sidebar.pages.ids[&PageKind::Active];
                    assert_eq!(loaded.len(), count + 3);
                    for id in &original {
                        assert!(loaded.contains(id), "lost loaded boundary {id}");
                        assert!(sidebar.sessions.iter().any(|row| &row.id == id));
                    }
                    assert_eq!(sidebar.active_rows().len(), count + 2);
                    assert_eq!(sidebar.pinned_rows.len(), 1);
                    assert_eq!(sidebar.pages.next.contains_key(&PageKind::Active), has_more);
                    assert!(!sidebar.pages.ids[&PageKind::Settled].contains(&"moved".into()));
                });
            }
        }
        sidebar.update(cx, |sidebar, cx| {
            // A long-lived window keeps only a bounded live overlay. Reaching
            // its budget must replace the cursor along with page membership.
            let generation = sidebar.pages.generation;
            sidebar.pages.live = (0..MAX_LIVE_SUMMARIES)
                .map(|i| {
                    let id = format!("overlay-{i}");
                    (id.clone(), summary(&id, AgentTaskState::Active))
                })
                .collect();
            sidebar
                .pages
                .next
                .insert(PageKind::Active, "old-continuation".into());
            sidebar.apply_live_summary(summary("overflow", AgentTaskState::Active), cx);
            assert_ne!(sidebar.pages.generation, generation);
            assert!(sidebar.pages.live.is_empty());
            assert!(!sidebar.pages.next.contains_key(&PageKind::Active));
            assert!(!sidebar.pages.ids.contains_key(&PageKind::Active));
        });
    }
}

impl PageKind {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Pinned => "pinned tasks",
            Self::Active => "active tasks",
            Self::Settled => "settled tasks",
            Self::Archived => "archived tasks",
            Self::Search => "search results",
            Self::Projects => "projects",
        }
    }
}

#[derive(Default)]
pub(super) struct SidebarPages {
    pub(super) enabled: bool,
    generation: u64,
    next: HashMap<PageKind, String>,
    ids: HashMap<PageKind, Vec<String>>,
    loading: HashSet<PageKind>,
    started: HashSet<PageKind>,
    failed: HashSet<PageKind>,
    search: Option<NavigationSearchQuery>,
    ready: HashSet<PageKind>,
    autoselect_generation: Option<u64>,
    live: HashMap<String, AgentSessionSummary>,
    deleted: HashSet<String>,
    pin_batches: VecDeque<Vec<String>>,
    project_generation: u64,
    retain_loaded: bool,
}

impl SidebarPages {
    pub(super) fn kind_contains(&self, kind: PageKind, id: &str) -> bool {
        self.ids
            .get(&kind)
            .is_some_and(|ids| ids.iter().any(|current| current == id))
    }

    pub(super) fn kind_complete(&self, kind: PageKind) -> bool {
        !self.has_more(kind)
    }

    pub(super) fn is_live(&self, id: &str) -> bool {
        self.live.contains_key(id)
    }

    pub(super) fn has_more(&self, kind: PageKind) -> bool {
        self.next.contains_key(&kind)
            || self.failed.contains(&kind)
            || self.loading.contains(&kind)
            || (kind == PageKind::Pinned && !self.pin_batches.is_empty())
    }
    /// Full coverage for the tasks shown in project folders, including pins.
    pub(super) fn non_archived_complete(&self) -> bool {
        !self.enabled
            || [PageKind::Active, PageKind::Settled, PageKind::Pinned]
                .into_iter()
                .all(|kind| self.ready.contains(&kind) && !self.has_more(kind))
    }
    pub(super) fn loading(&self, kind: PageKind) -> bool {
        self.loading.contains(&kind)
    }
    pub(super) fn has_started(&self, kind: PageKind) -> bool {
        self.started.contains(&kind)
    }
    pub(super) fn any_loading(&self) -> bool {
        !self.loading.is_empty()
    }
    pub(super) fn latest_summary(&self, summary: AgentSessionSummary) -> AgentSessionSummary {
        self.live.get(&summary.id).cloned().unwrap_or(summary)
    }
    pub(super) fn is_deleted(&self, id: &str) -> bool {
        self.deleted.contains(id)
    }
    pub(super) fn contains(&self, id: &str, searching: bool) -> bool {
        self.ids.iter().any(|(kind, ids)| {
            (*kind == PageKind::Search) == searching && ids.iter().any(|current| current == id)
        })
    }
}

impl Sidebar {
    /// Explicit owner-confirmed removal is stronger than absence from a
    /// bounded page. Fence older reads before dropping the workspace anchor.
    pub(in crate::ui::chat) fn forget_project(&mut self, root: &str, cx: &mut Context<Self>) {
        self.removed_projects.insert(root.to_owned());
        self.recent_roots.retain(|path| path != root);
        self.pages.project_generation = self.pages.project_generation.wrapping_add(1);
        self.pages.next.remove(&PageKind::Projects);
        self.pages.started.remove(&PageKind::Projects);
        self.pages.loading.remove(&PageKind::Projects);
        self.pages.failed.remove(&PageKind::Projects);
        self.refresh_computer_pages(cx);
        self.rebuild_sections();
        cx.notify();
    }

    pub(in crate::ui::chat) fn restore_project(
        &mut self,
        project: RecentProjectRoot,
        cx: &mut Context<Self>,
    ) {
        self.removed_projects.remove(&project.path);
        if !self.recent_roots.contains(&project.path) {
            self.recent_roots.push(project.path.clone());
        }
        self.merge_project_metadata(vec![project], cx);
        self.rebuild_sections();
        cx.notify();
    }

    /// Bootstrap includes a separate owner lookup for the default workspace.
    /// Its absence is authoritative, unlike absence from an ordinary page.
    pub(in crate::ui::chat) fn reconcile_default_project(
        &mut self,
        default_project: Option<RecentProjectRoot>,
        cx: &mut Context<Self>,
    ) {
        let removed = self
            .project_metadata
            .values()
            .filter(|root| {
                root.is_default_workspace
                    && default_project
                        .as_ref()
                        .is_none_or(|listed| listed.path != root.path)
            })
            .map(|root| root.path.clone())
            .collect::<Vec<_>>();
        for path in removed {
            self.forget_project(&path, cx);
        }
        if let Some(project) = default_project {
            self.restore_project(project, cx);
        }
    }

    pub(in crate::ui::chat) fn refresh_pages_for_generation(
        &mut self,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        self.refresh_pages(cx);
        self.pages.autoselect_generation = Some(generation);
    }

    /// Runtime startup can change summaries without changing their owner.
    pub(in crate::ui::chat) fn refresh_visible_pages_for_generation(
        &mut self,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        self.refresh_pages_with_display(true, cx);
        self.pages.autoselect_generation = Some(generation);
    }

    fn pages_ready(&mut self, cx: &mut Context<Self>) {
        if ![PageKind::Active, PageKind::Settled, PageKind::Pinned]
            .iter()
            .all(|kind| self.pages.ready.contains(kind))
        {
            return;
        }
        let Some(generation) = self.pages.autoselect_generation.take() else {
            return;
        };
        // A normal refresh may keep grouped rows visible while its owner read
        // is pending. Those display-only rows cannot become fresh navigation
        // authority or auto-selection candidates after empty first pages.
        let mut sessions = self
            .sessions
            .iter()
            .filter(|session| {
                self.pages.contains(&session.id, false)
                    || self.running.contains(&session.id)
                    || self.selected.as_ref() == Some(&session.id)
            })
            .cloned()
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| std::cmp::Reverse((session.updated_ms, session.id.clone())));
        sessions.truncate(50);
        cx.emit(SidebarEvent::PagesReady {
            sessions,
            generation,
        });
    }

    /// Also used after reconnect: deeper pages are discarded and visible first
    /// pages are re-authorized on the current host/account.
    /// Ordering changes replace desktop page positions while keeping the
    /// existing task-summary API unchanged. Retain visible membership so a
    /// drop in a later loaded page keeps its scroll anchor.
    pub(super) fn refresh_task_order(&mut self, cx: &mut Context<Self>) {
        self.manual_catalog.cancel_pending();
        self.order_query_dirty = false;
        if self.pages.enabled {
            self.pages.generation = self.pages.generation.wrapping_add(1);
            self.pages.next.clear();
            self.pages.loading.clear();
            self.pages.started.clear();
            self.pages.failed.clear();
            self.pages.ready.clear();
            self.pages.search = None;
            self.pages.retain_loaded = true;
            if self.filter.is_empty() {
                self.load_page(PageKind::Active, cx);
                self.load_page(PageKind::Settled, cx);
                self.load_page(PageKind::Pinned, cx);
                if self.archived_expanded {
                    self.load_page(PageKind::Archived, cx);
                }
            } else {
                self.load_page(PageKind::Search, cx);
            }
        }
        self.refresh_computer_pages(cx);
    }

    pub(super) fn refresh_pages(&mut self, cx: &mut Context<Self>) {
        self.refresh_pages_with_display(false, cx);
    }

    fn refresh_pages_with_display(&mut self, preserve_display: bool, cx: &mut Context<Self>) {
        self.manual_catalog.cancel_pending();
        self.manual_catalog.invalidate();
        let retained = if preserve_display {
            self.group_pages.own_task_ids(self.backend.remote_host())
        } else {
            HashSet::new()
        };
        let generation = self.pages.generation.wrapping_add(1);
        self.pages = SidebarPages {
            enabled: true,
            generation,
            ..SidebarPages::default()
        };
        self.sessions.retain(|s| {
            retained.contains(&s.id)
                || self.running.contains(&s.id)
                || self.selected.as_ref() == Some(&s.id)
        });
        if self.filter.is_empty() {
            self.load_page(PageKind::Active, cx);
            self.load_page(PageKind::Settled, cx);
            if self.archived_expanded {
                self.load_page(PageKind::Archived, cx);
            }
        } else if self.backend.remote_host().is_none() {
            self.load_page(PageKind::Search, cx);
        }
        self.load_page(PageKind::Projects, cx);
        self.load_sidebar_preferences(cx);
        if self.backend.remote_host().is_none() {
            self.load_page(PageKind::Pinned, cx);
        }
        if preserve_display {
            self.revalidate_computer_pages(cx);
        } else {
            self.refresh_computer_pages(cx);
        }
        self.rebuild_sections();
        cx.notify();
    }

    pub(super) fn merge_live_sessions(
        &mut self,
        summaries: Vec<AgentSessionSummary>,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;
        for summary in summaries {
            if let Some(current) = self
                .sessions
                .iter_mut()
                .find(|current| current.id == summary.id)
            {
                if !session_summary_eq(current, &summary) {
                    *current = summary;
                    changed = true;
                }
            } else if self.running.contains(&summary.id)
                || self.selected.as_ref() == Some(&summary.id)
            {
                self.sessions.push(summary);
                changed = true;
            }
        }
        if changed {
            self.manual_catalog.invalidate();
            self.rebuild_sections();
            cx.notify();
        }
    }

    pub(super) fn forget_page_tasks(&mut self, ids: &[String]) {
        self.manual_catalog.invalidate();
        self.pages.deleted.extend(ids.iter().cloned());
        self.sessions.retain(|session| !ids.contains(&session.id));
        for members in self.pages.ids.values_mut() {
            members.retain(|id| !ids.contains(id));
        }
    }

    pub(in crate::ui::chat) fn activate_more(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.activate_computer_more(id, cx) {
            return;
        }
        for kind in [
            PageKind::Pinned,
            PageKind::Active,
            PageKind::Settled,
            PageKind::Archived,
            PageKind::Search,
            PageKind::Projects,
        ] {
            if id == format!("page:{}", kind.label()) {
                self.load_page(kind, cx);
                return;
            }
        }
    }

    pub(super) fn render_load_more(&self, kind: PageKind, cx: &mut Context<Self>) -> AnyElement {
        let loading = self.pages.loading(kind);
        widgets::ghost_button(SharedString::from(format!("sidebar-more-{}", kind.label())))
            .mx_4()
            .my_1()
            .text_xs()
            .on_click(cx.listener(move |this, _event, _window, cx| this.load_page(kind, cx)))
            .child(if loading {
                format!("Loading {}…", kind.label())
            } else if self.pages.failed.contains(&kind) {
                format!("Retry {}", kind.label())
            } else {
                format!("Load more {}", kind.label())
            })
            .into_any_element()
    }

    /// One pushed summary can move between already loaded pages. UUID remains
    /// its identity even when recency, section, or project changes.
    pub(in crate::ui::chat) fn apply_live_summary(
        &mut self,
        summary: AgentSessionSummary,
        cx: &mut Context<Self>,
    ) {
        if !self.pages.enabled {
            return;
        }
        self.manual_catalog.invalidate();
        let id = summary.id.clone();
        self.pages.live.insert(id.clone(), summary.clone());
        let target = match summary.state {
            AgentTaskState::Active => PageKind::Active,
            AgentTaskState::Settled => PageKind::Settled,
            AgentTaskState::Archived => PageKind::Archived,
        };
        let visible = summary.message_count > 0
            && self
                .project_filter
                .as_ref()
                .is_none_or(|root| root == &summary.project_root);
        for kind in [PageKind::Active, PageKind::Settled, PageKind::Archived] {
            if let Some(ids) = self.pages.ids.get_mut(&kind) {
                ids.retain(|current| current != &id);
            }
        }
        if visible
            && self.filter.is_empty()
            && (target != PageKind::Archived || self.archived_expanded)
        {
            let ids = self.pages.ids.entry(target).or_default();
            ids.insert(0, id.clone());
            // The continuation cursor is after the last loaded row. Dropping
            // that row here would leave it on neither side of the next page.
            // Retain loaded coverage plus pushed tasks until an explicit page
            // refresh replaces both the membership and its cursor.
        }
        self.merge_page_sessions(vec![summary]);
        if self.pages.live.len() > MAX_LIVE_SUMMARIES {
            // Refresh both coverage and cursor rather than dropping a boundary
            // row behind an unchanged continuation. Loaded deeper pages can be
            // requested again, while a long-lived view's live overlay is capped.
            self.refresh_pages(cx);
            return;
        }
        self.trim_page_cache();
        self.rebuild_sections();
        cx.notify();
    }

    pub(super) fn load_page(&mut self, kind: PageKind, cx: &mut Context<Self>) {
        if self.order_query_dirty {
            self.refresh_task_order(cx);
            return;
        }
        if !self.pages.enabled || self.pages.loading.contains(&kind) {
            return;
        }
        if self.pages.started.contains(&kind)
            && !self.pages.next.contains_key(&kind)
            && !self.pages.failed.contains(&kind)
        {
            return;
        }
        if kind == PageKind::Projects {
            self.load_project_page(cx);
            return;
        }
        if kind == PageKind::Pinned && self.backend.remote_host().is_some() {
            self.load_remote_pin_batch(cx);
            return;
        }
        let cursor = self.pages.next.get(&kind).cloned();
        let append = self.pages.retain_loaded || self.pages.started.contains(&kind);
        let generation = self.pages.generation;
        self.pages.failed.remove(&kind);
        self.pages.loading.insert(kind);
        self.pages.started.insert(kind);
        let backend = self.backend.clone();
        let user = self.user_id.clone();
        let project = self.project_filter.clone();
        let filter = self.filter.clone();
        let computer = self.backend.remote_host().map(str::to_owned);
        let manual = if kind == PageKind::Pinned {
            self.local_pinned_manual_order(&computer)
        } else {
            self.local_manual_order(&computer)
        }
        .map(|order| (self.manual_catalog.reader(), order));
        let search = if kind == PageKind::Search {
            if self.pages.search.is_none() {
                let roots = self
                    .project_metadata
                    .iter()
                    .filter(|(_, root)| root.name.to_lowercase().contains(&filter))
                    .map(|(root, _)| root.clone())
                    .collect();
                self.pages.search = Some(NavigationSearchQuery {
                    text: filter,
                    project: project.clone(),
                    project_roots: roots,
                    cursor: None,
                    limit: 50,
                });
            }
            let mut search = self.pages.search.clone().expect("search initialized above");
            search.cursor = cursor.clone();
            Some(search)
        } else {
            None
        };
        let pins = if backend.remote_host().is_some() {
            PinFilter::Ignore
        } else {
            PinFilter::Exclude
        };
        self.call(
            async move {
                let query = if kind == PageKind::Search {
                    catalog::TaskQuery::Search(search.ok_or("Search is unavailable")?)
                } else if kind == PageKind::Pinned {
                    catalog::TaskQuery::Pinned(crate::backend::PinnedTasksRequest {
                        project,
                        cursor,
                        limit: 200,
                    })
                } else {
                    let section = TaskSection::State(match kind {
                        PageKind::Archived => TaskState::Archived,
                        PageKind::Settled => TaskState::Settled,
                        _ => TaskState::Active,
                    });
                    catalog::TaskQuery::Tasks(NavigationTaskQuery {
                        section,
                        project,
                        pins,
                        cursor,
                        limit: 50,
                    })
                };
                if let Some((reader, order)) = manual {
                    reader.page(backend, user, query, order).await
                } else {
                    match query {
                        catalog::TaskQuery::Search(query) => {
                            backend.search_tasks(&user, query).await
                        }
                        catalog::TaskQuery::Pinned(query) => {
                            backend.pinned_tasks(&user, query).await
                        }
                        catalog::TaskQuery::Tasks(query) => backend.list_tasks(&user, query).await,
                    }
                }
            },
            cx,
            move |this, result, cx| {
                if generation != this.pages.generation {
                    return;
                }
                this.pages.loading.remove(&kind);
                match result {
                    Ok(page) => {
                        this.pages.ready.insert(kind);
                        this.pages.next.remove(&kind);
                        if let Some(cursor) = page.next_cursor {
                            this.pages.next.insert(kind, cursor);
                        }
                        this.apply_task_page(kind, page.tasks, append);
                        // Pins are a complete preference projection, fetched using
                        // bounded replies rather than a first-200 cutoff.
                        if kind == PageKind::Pinned && this.pages.has_more(kind) {
                            this.load_page(kind, cx);
                        }
                    }
                    Err(message) if message == catalog::CANCELLED => {
                        if !this.pages.next.contains_key(&kind) {
                            this.pages.started.remove(&kind);
                        }
                        this.load_page(kind, cx);
                        return;
                    }
                    Err(message) => {
                        if !append {
                            this.pages.started.remove(&kind);
                        }
                        this.pages.failed.insert(kind);
                        cx.emit(SidebarEvent::Notice(message.into()));
                    }
                }
                this.pages_ready(cx);
                this.rebuild_sections();
                cx.notify();
            },
        );
    }

    fn apply_task_page(&mut self, kind: PageKind, rows: Vec<TaskSummary>, append: bool) {
        let rows: Vec<_> = rows
            .into_iter()
            .filter(|row| !self.pages.deleted.contains(&row.uuid))
            .collect();
        let live = &self.pages.live;
        let ids = self.pages.ids.entry(kind).or_default();
        if !append {
            // A pushed update may have arrived while this first-page read was
            // in flight. Only its snapshot membership can be replaced here.
            ids.retain(|id| live.contains_key(id));
        }
        for row in &rows {
            if !ids.contains(&row.uuid) {
                ids.push(row.uuid.clone());
            }
        }
        if kind == PageKind::Pinned && self.backend.remote_host().is_none() {
            self.pinned_tasks = ids.clone();
        }
        self.remember_task_project_labels(
            rows.iter()
                .map(|row| (row.project_root.clone(), row.project_display_name.clone())),
        );
        self.merge_page_sessions(
            rows.iter()
                .map(|row| {
                    self.pages
                        .live
                        .get(&row.uuid)
                        .cloned()
                        .unwrap_or_else(|| session_from_summary(row))
                })
                .collect(),
        );
        self.trim_page_cache();
    }

    pub(super) fn remember_task_project_labels(
        &mut self,
        labels: impl IntoIterator<Item = (String, Option<String>)>,
    ) {
        for (root, label) in labels {
            if let Some(label) = label {
                self.task_project_labels.insert(root, label);
            } else {
                self.task_project_labels.remove(&root);
            }
        }
    }

    #[cfg(test)]
    pub(in crate::ui::chat) fn apply_search_page_for_test(
        &mut self,
        query: &str,
        project: Option<String>,
        rows: Vec<TaskSummary>,
        cx: &mut Context<Self>,
    ) {
        self.pages.enabled = true;
        self.filter = query.to_lowercase();
        self.project_filter = project;
        self.apply_task_page(PageKind::Search, rows, false);
        self.rebuild_sections();
        cx.notify();
    }

    fn trim_page_cache(&mut self) {
        let own = self.group_pages.own_task_ids(self.backend.remote_host());
        self.sessions.retain(|session| {
            self.pages.ids.values().any(|ids| ids.contains(&session.id))
                || own.contains(&session.id)
                || self.running.contains(&session.id)
                || self.selected.as_ref() == Some(&session.id)
        });
        self.task_project_labels.retain(|root, _| {
            self.sessions
                .iter()
                .any(|session| &session.project_root == root)
        });
        if self.pages.loading.is_empty() {
            self.pages
                .live
                .retain(|id, _| self.sessions.iter().any(|session| &session.id == id));
        }
    }

    fn load_sidebar_preferences(&mut self, cx: &mut Context<Self>) {
        if self.backend.remote_host().is_none() {
            return;
        }
        let backend = self.backend.clone();
        let user = self.user_id.clone();
        let computer = self.backend.remote_host().unwrap_or_default().to_owned();
        let generation = self.pages.generation;
        self.call(
            async move {
                backend
                    .get_remote_sidebars(&user)
                    .await
                    .map(|mut settings| settings.remove(&computer).unwrap_or_default())
            },
            cx,
            move |this, result, cx| {
                if generation != this.pages.generation {
                    return;
                }
                match result {
                    Ok(settings) => {
                        this.pinned_tasks = settings.pinned_tasks;
                        this.pages.pin_batches = this
                            .pinned_tasks
                            .chunks(200)
                            .map(<[String]>::to_vec)
                            .collect();
                        if this.pages.pin_batches.is_empty() {
                            this.pages.ready.insert(PageKind::Pinned);
                            this.pages_ready(cx);
                        }
                        this.load_remote_pin_batch(cx);
                        // Re-run the query after remote pin preferences load.
                        if !this.filter.is_empty() {
                            this.pages.started.remove(&PageKind::Search);
                            this.pages.next.remove(&PageKind::Search);
                            this.pages.search = None;
                            this.load_page(PageKind::Search, cx);
                        }
                        this.revalidate_computer_pages(cx);
                    }
                    Err(message) => {
                        this.pages.ready.insert(PageKind::Pinned);
                        this.pages_ready(cx);
                        if !this.filter.is_empty() {
                            this.load_page(PageKind::Search, cx);
                        }
                        cx.emit(SidebarEvent::Notice(message.into()));
                    }
                }
                this.rebuild_sections();
                cx.notify();
            },
        );
    }

    fn load_remote_pin_batch(&mut self, cx: &mut Context<Self>) {
        if self.pages.loading.contains(&PageKind::Pinned) {
            return;
        }
        let Some(ids) = self.pages.pin_batches.pop_front() else {
            return;
        };
        let backend = self.backend.clone();
        let user = self.user_id.clone();
        let generation = self.pages.generation;
        let retry_ids = ids.clone();
        self.pages.loading.insert(PageKind::Pinned);
        self.call(
            async move {
                backend
                    .get_tasks(&user, crate::backend::GetTasksRequest { tasks: ids })
                    .await
            },
            cx,
            move |this, result, cx| {
                if generation != this.pages.generation {
                    return;
                }
                this.pages.loading.remove(&PageKind::Pinned);
                match result {
                    Ok(reply) => {
                        this.pages.ready.insert(PageKind::Pinned);
                        if !reply.remaining_ids.is_empty() {
                            this.pages.pin_batches.push_front(reply.remaining_ids);
                        }
                        this.apply_task_page(PageKind::Pinned, reply.tasks, true);
                        this.load_remote_pin_batch(cx);
                    }
                    Err(message) => {
                        this.pages.pin_batches.push_front(retry_ids);
                        cx.emit(SidebarEvent::Notice(message.into()));
                    }
                }
                this.pages_ready(cx);
                this.rebuild_sections();
                cx.notify();
            },
        );
    }

    fn load_project_page(&mut self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let user = self.user_id.clone();
        let generation = self.pages.generation;
        self.pages.project_generation = self.pages.project_generation.wrapping_add(1);
        let project_generation = self.pages.project_generation;
        let cursor = self.pages.next.get(&PageKind::Projects).cloned();
        let append = self.pages.started.contains(&PageKind::Projects);
        self.pages.loading.insert(PageKind::Projects);
        self.pages.started.insert(PageKind::Projects);
        self.pages.failed.remove(&PageKind::Projects);
        self.call(
            async move {
                backend
                    .list_projects(
                        &user,
                        crate::backend::ProjectPageRequest { cursor, limit: 50 },
                    )
                    .await
            },
            cx,
            move |this, result, cx| {
                if generation != this.pages.generation
                    || project_generation != this.pages.project_generation
                {
                    return;
                }
                this.pages.loading.remove(&PageKind::Projects);
                match result {
                    Ok(page) => this.apply_project_page(page, append, cx),
                    Err(message) => {
                        if !append {
                            this.pages.started.remove(&PageKind::Projects);
                        }
                        this.pages.failed.insert(PageKind::Projects);
                        cx.emit(SidebarEvent::Notice(message.into()));
                    }
                }
                this.rebuild_sections();
                cx.notify();
            },
        );
    }

    fn apply_project_page(
        &mut self,
        page: maple_agent::store::NavigationProjectPage,
        append: bool,
        cx: &mut Context<Self>,
    ) {
        let workspace = if !append {
            let workspace = self
                .recent_roots
                .iter()
                .filter(|path| {
                    self.project_metadata
                        .get(*path)
                        .is_some_and(|root| root.is_default_workspace)
                })
                .cloned()
                .collect::<Vec<_>>();
            self.recent_roots.clear();
            workspace
        } else {
            Vec::new()
        };
        self.pages.next.remove(&PageKind::Projects);
        if let Some(cursor) = page.next_cursor {
            self.pages.next.insert(PageKind::Projects, cursor);
        }
        let mut roots = page
            .projects
            .into_iter()
            .map(project_from_summary)
            .collect::<Vec<_>>();
        self.preserve_newer_project_names(&mut roots);
        for project in roots {
            self.removed_projects.remove(&project.path);
            if !self.recent_roots.contains(&project.path) {
                self.recent_roots.push(project.path.clone());
            }
            self.project_metadata.insert(project.path.clone(), project);
        }
        for path in workspace {
            if !self.recent_roots.contains(&path) {
                self.recent_roots.push(path);
            }
        }
        cx.emit(SidebarEvent::ProjectsReady {
            roots: self
                .recent_roots
                .iter()
                .filter_map(|path| self.project_metadata.get(path).cloned())
                .collect(),
        });
    }

    #[cfg(test)]
    pub(in crate::ui::chat) fn apply_project_page_for_test(
        &mut self,
        page: maple_agent::store::NavigationProjectPage,
        cx: &mut Context<Self>,
    ) {
        self.pages.enabled = true;
        self.apply_project_page(page, false, cx);
        self.rebuild_sections();
    }

    pub(in crate::ui::chat) fn refresh_project_roots(&mut self, cx: &mut Context<Self>) {
        if !self.pages.enabled {
            return;
        }
        self.pages.next.remove(&PageKind::Projects);
        self.pages.started.remove(&PageKind::Projects);
        self.pages.loading.remove(&PageKind::Projects);
        self.pages.failed.remove(&PageKind::Projects);
        self.load_project_page(cx);
    }
}

#[cfg(test)]
mod merged_prefix_tests {
    use super::*;

    fn summary(id: &str, state: &str, updated: i64) -> TaskSummary {
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
                _ => None,
            })
            .collect()
    }

    #[gpui::test]
    fn loading_more_appends_to_the_merged_list_instead_of_filling_gaps(
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
            sidebar.running.clear();
            sidebar.computers = Arc::new(Vec::new());
            sidebar.pages.enabled = true;
            for (kind, state, newest) in [
                (PageKind::Active, "active", 200),
                (PageKind::Settled, "settled", 1000),
            ] {
                sidebar.apply_task_page(
                    kind,
                    (0..50)
                        .map(|ix| summary(&format!("{state}-{ix}"), state, newest - ix))
                        .collect(),
                    false,
                );
                sidebar.pages.next.insert(kind, "next".into());
            }
            sidebar.rebuild_sections();
            let first = shown(sidebar);
            assert_eq!(first.len(), 49);
            assert!(first.iter().all(|id| id.starts_with("settled")));
            let more = sidebar
                .entries
                .iter()
                .filter_map(|entry| match entry {
                    SidebarEntry::LoadMore(kind) => Some(*kind),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(more, [PageKind::Settled]);
            // A later Settled page belongs ahead of the fetched Active rows.
            sidebar.apply_task_page(
                PageKind::Settled,
                (50..100)
                    .map(|ix| summary(&format!("settled-{ix}"), "settled", 1000 - ix))
                    .collect(),
                true,
            );
            sidebar.pages.next.remove(&PageKind::Settled);
            sidebar.rebuild_sections();
            let second = shown(sidebar);
            assert_eq!(&second[..first.len()], first);
            assert_eq!(second.len(), 149);
            assert!(second[..100].iter().all(|id| id.starts_with("settled")));
            assert!(
                sidebar
                    .entries
                    .contains(&SidebarEntry::LoadMore(PageKind::Active))
            );
            assert!(
                !sidebar
                    .entries
                    .contains(&SidebarEntry::LoadMore(PageKind::Settled))
            );
            // Exhausting Active releases the complete remaining merged tail.
            sidebar.apply_task_page(
                PageKind::Active,
                (50..100)
                    .map(|ix| summary(&format!("active-{ix}"), "active", 200 - ix))
                    .collect(),
                true,
            );
            sidebar.pages.next.remove(&PageKind::Active);
            sidebar.rebuild_sections();
            let third = shown(sidebar);
            assert_eq!(&third[..second.len()], second);
            assert_eq!(third.len(), 200);
        });
    }

    #[gpui::test]
    fn cursor_timestamp_ties_are_sorted_once_when_the_boundary_is_complete(
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
                group: SidebarGroup::Project,
                sort: SidebarSort::Updated,
            };
            sidebar.selected = None;
            sidebar.running.clear();
            sidebar.computers = Arc::new(Vec::new());
            sidebar.pages.enabled = true;
            sidebar.apply_task_page(PageKind::Active, vec![summary("z", "active", 100)], false);
            sidebar.apply_task_page(
                PageKind::Settled,
                vec![summary("settled", "settled", 101)],
                false,
            );
            sidebar.pages.next.insert(PageKind::Active, "tie".into());
            sidebar.rebuild_sections();
            assert_eq!(shown(sidebar), ["settled"]);
            sidebar.apply_task_page(PageKind::Active, vec![summary("a", "active", 100)], true);
            sidebar.rebuild_sections();
            assert_eq!(shown(sidebar), ["settled"]);
            sidebar.pages.next.remove(&PageKind::Active);
            sidebar.rebuild_sections();
            assert_eq!(shown(sidebar), ["settled", "a", "z"]);
            // A selected/running task remains reachable outside the prefix.
            sidebar.pages.next.insert(PageKind::Settled, "more".into());
            sidebar.selected = Some("a".into());
            sidebar.running.insert("z".into());
            sidebar.rebuild_sections();
            assert_eq!(shown(sidebar), ["a", "z"]);
        });
    }
}
