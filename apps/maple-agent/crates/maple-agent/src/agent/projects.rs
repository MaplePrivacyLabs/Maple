//! Project labels belong to the execution computer and account. Canonical
//! paths, names and revisions live only in its injected store-v1 database.
//! Commands return a single authoritative record; project membership is paged.

use std::path::Path;

use icu_properties::{CodePointSetData, props::DefaultIgnorableCodePoint};

use super::config::*;
use super::{AgentRuntimeHandle, AgentServiceEvent, RecentProjectRoot, emit_agent_event};
use crate::store::{AccountId, ProjectRecord, Store};

const MAX_NAME_CHARS: usize = 128;

fn normalize_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Project name cannot be empty".into());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!(
            "Project name must be {MAX_NAME_CHARS} characters or fewer"
        ));
    }
    if name
        .chars()
        .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
    {
        return Err("Project name must be a single line without control characters".into());
    }
    let invisible = CodePointSetData::new::<DefaultIgnorableCodePoint>();
    if !name
        .chars()
        .any(|ch| !ch.is_whitespace() && !invisible.contains(ch))
    {
        return Err("Project name must contain visible characters".into());
    }
    Ok(name.into())
}

/// Pure owner-side label projection shared by runtime metadata and SQL search.
/// The caller resolves the workspace identity; this never reads the filesystem.
pub(crate) fn owner_project_display_name(
    path: &str,
    custom_name: Option<&str>,
    is_default_workspace: bool,
) -> String {
    if let Some(name) = custom_name {
        return name.into();
    }
    if is_default_workspace {
        return super::MAPLE_WORKSPACE_DISPLAY_NAME.into();
    }
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .into()
}

fn metadata(path: String, record: Option<ProjectRecord>) -> Result<RecentProjectRoot, String> {
    let is_default_workspace = super::is_default_maple_workspace(Path::new(&path));
    let mut root = RecentProjectRoot {
        name: owner_project_display_name(&path, None, is_default_workspace),
        path,
        last_used_ms: 0,
        is_default_workspace,
        name_revision: 0,
    };
    if let Some(record) = record {
        root.last_used_ms = u128::try_from(record.last_used_ms)
            .map_err(|_| "Project metadata has an invalid timestamp")?;
        root.name_revision = record.name_revision;
        if let Some(name) = record.display_name {
            if normalize_name(&name).ok().as_ref() != Some(&name) {
                return Err("Project display name is invalid".into());
            }
            root.name = name;
        }
    }
    Ok(root)
}

fn listed_metadata(
    store: &Store,
    account: AccountId,
    path: &str,
) -> Result<RecentProjectRoot, String> {
    if !structurally_valid_project_root(path) {
        return Err("Project path must be an absolute folder path".into());
    }
    let record = store
        .listed_project(account, path)
        .map_err(|error| error.to_string())?;
    if record.is_some() {
        return metadata(path.into(), record);
    }
    // The owner's implicit workspace is offered even before it exists. Its
    // explicit removal tombstone still wins; no directory is opened or created.
    if super::is_default_maple_workspace(Path::new(path)) {
        let record = store
            .project(account, path)
            .map_err(|error| error.to_string())?;
        if record
            .as_ref()
            .is_none_or(|record| record.removed_ms.is_none())
        {
            return metadata(path.into(), record);
        }
    }
    Err("This project is not in this computer's project list".into())
}

impl AgentRuntimeHandle {
    /// The execution owner's three most recently explicitly selected projects.
    pub async fn recent_project_roots(&self) -> Result<Vec<RecentProjectRoot>, String> {
        self.verify_generation().await?;
        let tasks = self.store()?;
        let roots = tokio::task::spawn_blocking(move || {
            tasks
                .store()
                .recently_used_projects(tasks.account())
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(|record| metadata(record.root.clone(), Some(record)))
                .collect::<Result<Vec<_>, String>>()
        })
        .await
        .map_err(|_| "Project settings worker did not complete".to_owned())??;
        self.verify_generation().await?;
        Ok(roots)
    }

    /// Resolve one owner-authoritative record without loading tasks or a catalog.
    pub async fn project_root_metadata(&self, path: String) -> Result<RecentProjectRoot, String> {
        self.verify_generation().await?;
        let _settings = self.lock_settings().await;
        let tasks = self.store()?;
        let root = tokio::task::spawn_blocking(move || {
            listed_metadata(tasks.store(), tasks.account(), &path)
        })
        .await
        .map_err(|_| "Project settings worker did not complete".to_owned())??;
        self.verify_generation().await?;
        Ok(root)
    }

    /// A label mutation only: paths, trust and tasks are unchanged. Typing the
    /// owner's default label resets a custom name, retaining a newer revision.
    pub async fn rename_project_root(
        &self,
        path: String,
        name: String,
    ) -> Result<RecentProjectRoot, String> {
        let name = normalize_name(&name)?;
        let _settings = self.lock_settings().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let tasks = self.store()?;
        let (root, changed) = tokio::task::spawn_blocking(move || {
            let root = listed_metadata(tasks.store(), tasks.account(), &path)?;
            let name = (name
                != owner_project_display_name(&root.path, None, root.is_default_workspace))
            .then_some(name);
            let changed = tasks
                .store()
                .set_project_display_name(tasks.account(), &path, name.as_deref(), pi_ai::now_ms())
                .map_err(|error| error.to_string())?;
            let record = tasks
                .store()
                .project(tasks.account(), &path)
                .map_err(|error| error.to_string())?;
            Ok::<_, String>((metadata(path, record)?, changed))
        })
        .await
        .map_err(|_| "Project settings worker did not complete".to_owned())??;
        self.verify_generation().await?;
        if changed {
            emit_agent_event(
                &self.service.host.events,
                AgentServiceEvent::ProjectsChanged {
                    user_id: self.user_id.to_string(),
                    roots: vec![root.clone()],
                },
            );
        }
        Ok(root)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::agent::store::{TaskKind, TaskRow};
    use crate::agent::tests::Recorder;
    use crate::agent::{AgentPathLayout, MapleAgentHostResources, MapleAgentService};
    use crate::store::StorePaths;

    const USER: &str = "project-names@example.com";

    struct Fixture {
        temp: tempfile::TempDir,
        service: MapleAgentService,
        handle: AgentRuntimeHandle,
        recorder: Arc<Recorder>,
        root: String,
    }

    fn service(
        config: &Path,
        data: &Path,
        recorder: Arc<Recorder>,
        store: Arc<Store>,
    ) -> MapleAgentService {
        MapleAgentService::new(
            MapleAgentHostResources::new(
                AgentPathLayout::from_app_roots(config.to_owned(), data.to_owned())
                    .with_home(Some(data.join("home"))),
                recorder,
                "Project names fixture".into(),
            )
            .with_store(store),
        )
    }

    async fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let recorder = Arc::new(Recorder::default());
        let data = temp.path().join("data");
        let service = service(
            &temp.path().join("config"),
            &data,
            recorder.clone(),
            Arc::new(Store::open(StorePaths::new(data.clone())).unwrap()),
        );
        let handle = service.handle_for_user(USER).await.unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let root = handle
            .save_recent_project_root(path_string(&root))
            .await
            .unwrap();
        Fixture {
            temp,
            service,
            handle,
            recorder,
            root,
        }
    }

    #[tokio::test]
    async fn project_name_is_durable_and_reset_retains_a_newer_revision() {
        let Fixture {
            temp,
            service,
            handle,
            recorder,
            root,
        } = fixture().await;
        let first = handle
            .rename_project_root(root.clone(), "  Research  ".into())
            .await
            .unwrap();
        assert_eq!(first.name, "Research");
        assert!(first.name_revision > 0);
        assert!(!first.is_default_workspace);
        assert!(
            matches!(&recorder.events()[0], AgentServiceEvent::ProjectsChanged { user_id, roots }
            if user_id == USER && roots == &vec![first.clone()])
        );
        drop(handle);
        drop(service);
        let data = temp.path().join("data");
        let restarted = service_fn(&temp.path().join("config"), &data);
        let restarted = restarted.handle_for_user(USER).await.unwrap();
        assert_eq!(
            restarted.project_root_metadata(root.clone()).await.unwrap(),
            first
        );
        let reset = restarted
            .rename_project_root(root.clone(), "project".into())
            .await
            .unwrap();
        assert_eq!(reset.name, "project");
        assert!(reset.name_revision > first.name_revision);
        assert_eq!(
            restarted
                .rename_project_root(root, "project".into())
                .await
                .unwrap(),
            reset
        );
    }

    fn service_fn(config: &Path, data: &Path) -> MapleAgentService {
        service(
            config,
            data,
            Arc::new(Recorder::default()),
            Arc::new(Store::open(StorePaths::new(data.to_owned())).unwrap()),
        )
    }

    #[tokio::test]
    async fn project_names_do_not_roam_or_cross_accounts() {
        let fixture = fixture().await;
        fixture
            .handle
            .rename_project_root(fixture.root.clone(), "Device one".into())
            .await
            .unwrap();
        let other = fixture
            .service
            .handle_for_user("other@example.com")
            .await
            .unwrap();
        assert!(
            other
                .rename_project_root(fixture.root.clone(), "Denied".into())
                .await
                .is_err()
        );
        other
            .save_recent_project_root(fixture.root.clone())
            .await
            .unwrap();
        assert_eq!(
            other
                .project_root_metadata(fixture.root.clone())
                .await
                .unwrap()
                .name,
            "project"
        );
        other
            .rename_project_root(fixture.root.clone(), "Other account".into())
            .await
            .unwrap();
        assert_eq!(
            fixture
                .handle
                .project_root_metadata(fixture.root.clone())
                .await
                .unwrap()
                .name,
            "Device one"
        );
        // Identical config with a distinct store is another execution computer.
        let second = service_fn(
            &fixture.temp.path().join("config"),
            &fixture.temp.path().join("second-data"),
        );
        let second = second.handle_for_user(USER).await.unwrap();
        second
            .save_recent_project_root(fixture.root.clone())
            .await
            .unwrap();
        assert_eq!(
            second
                .project_root_metadata(fixture.root)
                .await
                .unwrap()
                .name,
            "project"
        );
        assert!(
            !fixture.temp.path().join("config").exists(),
            "labels never create legacy config sidecars"
        );
    }

    #[tokio::test]
    async fn pending_history_observers_do_not_reject_committed_project_renames() {
        let fixture = fixture().await;
        // Maintenance observes this same state from a blocking worker. A reader
        // of `None` is not retained history and must not reject ordinary work.
        let observed = fixture.service.state.shutdown_pending.read().await;
        assert!(observed.is_none());
        let root = fixture
            .handle
            .rename_project_root(fixture.root.clone(), "While observed".into())
            .await
            .unwrap();
        assert_eq!(root.name, "While observed");
        assert_eq!(
            fixture
                .handle
                .project_root_metadata(fixture.root.clone())
                .await
                .unwrap(),
            root
        );
        assert!(matches!(
            fixture.recorder.events().as_slice(),
            [AgentServiceEvent::ProjectsChanged { roots, .. }]
                if roots.as_slice() == std::slice::from_ref(&root)
        ));
        drop(observed);

        // An exclusive transition still blocks admission even before it has
        // installed retained history. No rename is committed or published.
        let transition = fixture.service.state.shutdown_pending.write().await;
        assert_eq!(
            fixture
                .handle
                .rename_project_root(fixture.root.clone(), "During transition".into())
                .await
                .unwrap_err(),
            super::super::RUNTIME_SAVE_PENDING_ERROR
        );
        drop(transition);
        assert_eq!(
            fixture
                .handle
                .project_root_metadata(fixture.root)
                .await
                .unwrap(),
            root
        );
        assert_eq!(fixture.recorder.events().len(), 1);
    }

    #[tokio::test]
    async fn concurrent_renames_publish_single_committed_records_in_order() {
        let fixture = fixture().await;
        let second = fixture.temp.path().join("second");
        std::fs::create_dir(&second).unwrap();
        let second = fixture
            .handle
            .save_recent_project_root(path_string(&second))
            .await
            .unwrap();
        let (first, other) = tokio::join!(
            fixture
                .handle
                .rename_project_root(fixture.root.clone(), "First label".into()),
            fixture
                .handle
                .rename_project_root(second, "Second label".into())
        );
        let (first, other) = (first.unwrap(), other.unwrap());
        assert_ne!(first.name_revision, other.name_revision);
        let events = fixture.recorder.events();
        assert_eq!(events.len(), 2);
        let mut revisions = Vec::new();
        for event in events {
            let AgentServiceEvent::ProjectsChanged { roots, .. } = event else {
                panic!("unexpected event")
            };
            assert_eq!(roots.len(), 1, "only a bounded metadata delta is published");
            revisions.push(roots[0].name_revision);
        }
        assert!(revisions[0] < revisions[1]);
        let mut config = fixture.handle.load_config().await.unwrap();
        config.default_model = "fixture-model".into();
        let (saved, renamed) = tokio::join!(
            fixture.handle.save_config(config),
            fixture
                .handle
                .rename_project_root(fixture.root, "Changed".into())
        );
        saved.unwrap();
        renamed.unwrap();
        assert_eq!(
            fixture.handle.load_config().await.unwrap().default_model,
            "fixture-model"
        );
    }

    #[tokio::test]
    async fn names_survive_register_remove_restore_without_changing_paths() {
        let fixture = fixture().await;
        let expected = fixture
            .handle
            .rename_project_root(fixture.root.clone(), "Persistent label".into())
            .await
            .unwrap();
        assert_eq!(
            fixture
                .handle
                .save_recent_project_root(fixture.root.clone())
                .await
                .unwrap(),
            fixture.root
        );
        let selected = fixture
            .handle
            .project_root_metadata(fixture.root.clone())
            .await
            .unwrap();
        assert!(selected.last_used_ms >= expected.last_used_ms);
        let expected = RecentProjectRoot {
            last_used_ms: selected.last_used_ms,
            ..expected
        };
        assert_eq!(selected, expected);
        fixture
            .handle
            .remove_project_root(fixture.root.clone(), None)
            .await
            .unwrap();
        assert!(
            fixture
                .handle
                .project_root_metadata(fixture.root.clone())
                .await
                .is_err()
        );
        assert!(
            fixture
                .handle
                .rename_project_root(fixture.root.clone(), "Hidden".into())
                .await
                .is_err()
        );
        fixture
            .handle
            .save_recent_project_root(fixture.root.clone())
            .await
            .unwrap();
        let restored = fixture
            .handle
            .project_root_metadata(fixture.root.clone())
            .await
            .unwrap();
        assert_eq!(restored.name, expected.name);
        assert_eq!(restored.name_revision, expected.name_revision);
        assert!(Path::new(&fixture.root).is_dir());
        assert!(
            !Path::new(&fixture.root)
                .with_file_name("Persistent label")
                .exists()
        );
    }

    #[tokio::test]
    async fn task_only_roots_have_shared_names_and_invalid_names_do_not_publish() {
        let fixture = fixture().await;
        let acp = fixture.temp.path().join("acp-only");
        std::fs::create_dir(&acp).unwrap();
        let acp = path_string(&acp.canonicalize().unwrap());
        fixture
            .handle
            .store()
            .unwrap()
            .insert(&TaskRow::new(
                "10000000-0000-7000-8000-000000000001".into(),
                "ACP task".into(),
                acp.clone(),
                TaskKind::Acp,
                None,
                1,
            ))
            .unwrap();
        // Empty task roots are not yet listed. Once acknowledged history exists,
        // navigation and the rename point-query share the same membership rule.
        assert!(
            fixture
                .handle
                .rename_project_root(acp.clone(), "Too soon".into())
                .await
                .is_err()
        );
        let tasks = fixture.handle.store().unwrap();
        let task = tasks
            .store()
            .task(tasks.account(), "10000000-0000-7000-8000-000000000001")
            .unwrap()
            .unwrap();
        let mut facts = task.facts;
        facts.message_count = 1;
        tasks
            .store()
            .refresh_task_facts(
                tasks.account(),
                "10000000-0000-7000-8000-000000000001",
                &facts,
            )
            .unwrap();
        assert_eq!(
            fixture
                .handle
                .rename_project_root(acp, "ACP project".into())
                .await
                .unwrap()
                .name,
            "ACP project"
        );
        let before = fixture.recorder.events().len();
        for name in [
            "",
            " \t ",
            "two\nlines",
            "x\u{2028}y",
            "\u{200b}",
            &"x".repeat(129),
        ] {
            assert!(
                fixture
                    .handle
                    .rename_project_root(fixture.root.clone(), name.into())
                    .await
                    .is_err()
            );
        }
        assert!(
            fixture
                .handle
                .rename_project_root(format!("{}-unknown", fixture.root), "Valid".into())
                .await
                .is_err()
        );
        assert_eq!(fixture.recorder.events().len(), before);
        assert!(normalize_name(&"é".repeat(128)).is_ok());
    }

    #[tokio::test]
    async fn default_workspace_keeps_owner_identity_through_rename_and_reset() {
        let fixture = fixture().await;
        let Some(workspace) = super::super::default_maple_workspace_path() else {
            return;
        };
        let path = path_string(&workspace);
        let initial = fixture
            .handle
            .project_root_metadata(path.clone())
            .await
            .unwrap();
        assert_eq!(initial.name, "Maple Workspace");
        assert!(initial.is_default_workspace);
        let renamed = fixture
            .handle
            .rename_project_root(path.clone(), "My workspace".into())
            .await
            .unwrap();
        assert!(renamed.is_default_workspace);
        let reset = fixture
            .handle
            .rename_project_root(path.clone(), "Maple Workspace".into())
            .await
            .unwrap();
        assert!(reset.is_default_workspace);
        assert_eq!(reset.name, initial.name);
        assert!(reset.name_revision > renamed.name_revision);
        fixture
            .handle
            .remove_project_root(path.clone(), None)
            .await
            .unwrap();
        assert!(
            fixture
                .handle
                .project_root_metadata(path.clone())
                .await
                .is_err()
        );
        let tasks = fixture.handle.store().unwrap();
        assert!(
            tasks
                .store()
                .project(tasks.account(), &path)
                .unwrap()
                .unwrap()
                .removed_ms
                .is_some()
        );
        assert!(
            matches!(fixture.recorder.events().last(), Some(AgentServiceEvent::ProjectRemoved { user_id, project_root }) if user_id == USER && project_root == &path)
        );
        // Re-reading after another owner handle attaches cannot resurrect it.
        let attached = fixture.service.handle_for_user(USER).await.unwrap();
        assert!(attached.project_root_metadata(path.clone()).await.is_err());
        let tasks = fixture.handle.store().unwrap();
        tasks
            .store()
            .register_explicit_project(tasks.account(), &path, 2)
            .unwrap();
        assert!(
            attached
                .project_root_metadata(path)
                .await
                .unwrap()
                .is_default_workspace
        );
    }

    #[tokio::test]
    async fn exhausted_revision_does_not_publish_or_replace_a_label() {
        let fixture = fixture().await;
        let original = fixture
            .handle
            .rename_project_root(fixture.root.clone(), "Preserved".into())
            .await
            .unwrap();
        let tasks = fixture.handle.store().unwrap();
        tasks
            .store()
            .set_account_setting(
                tasks.account(),
                "project.names.revision",
                &serde_json::json!(i64::MAX),
                1,
            )
            .unwrap();
        let event_count = fixture.recorder.events().len();
        assert!(
            fixture
                .handle
                .rename_project_root(fixture.root.clone(), "Replacement".into())
                .await
                .is_err()
        );
        assert_eq!(
            fixture
                .handle
                .project_root_metadata(fixture.root)
                .await
                .unwrap(),
            original
        );
        assert_eq!(fixture.recorder.events().len(), event_count);
    }

    #[tokio::test]
    async fn successful_archive_publishes_one_project_removal_and_rejection_publishes_none() {
        let fixture = fixture().await;
        assert!(
            fixture
                .handle
                .remove_project_root(fixture.root.clone(), Some(fixture.root.clone()))
                .await
                .is_err()
        );
        assert!(fixture.recorder.events().is_empty());
        fixture
            .handle
            .archive_project_root(fixture.root.clone(), None)
            .await
            .unwrap();
        let events = fixture.recorder.events();
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], AgentServiceEvent::ProjectRemoved { user_id, project_root } if user_id == USER && project_root == &fixture.root)
        );
        assert!(
            fixture
                .handle
                .project_root_metadata(fixture.root)
                .await
                .is_err()
        );
    }

    #[test]
    fn default_workspace_and_plain_folder_names_are_owner_resolved() {
        let Some(workspace) = super::super::default_maple_workspace_path() else {
            return;
        };
        let root = metadata(path_string(&workspace), None).unwrap();
        assert_eq!(root.name, "Maple Workspace");
        assert!(root.is_default_workspace);
        let decoy = metadata("/different/Maple".into(), None).unwrap();
        assert_eq!(decoy.name, "Maple");
        assert!(!decoy.is_default_workspace);
    }
}
