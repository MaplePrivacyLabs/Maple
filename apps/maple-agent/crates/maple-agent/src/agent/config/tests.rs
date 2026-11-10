use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::store::StorePaths;

const USER: &str = "config@example.test";
const OTHER_USER: &str = "other-config@example.test";

struct Fixture {
    root: tempfile::TempDir,
    layout: AgentPathLayout,
    store: Arc<Store>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(StorePaths::new(root.path().join("data"))).unwrap());
        let layout =
            AgentPathLayout::from_app_roots(root.path().join("config"), root.path().join("data"))
                .with_instruction_config_root(root.path().join("previous-config"))
                .with_store(Arc::clone(&store))
                .with_home(None);
        Self {
            root,
            layout,
            store,
        }
    }

    fn project(&self, name: &str) -> PathBuf {
        let directory = self.root.path().join("projects").join(name);
        fs::create_dir_all(&directory).unwrap();
        directory.canonicalize().unwrap()
    }

    fn account(&self, user_id: &str) -> AccountId {
        account_store(&self.layout, user_id).unwrap().1
    }

    fn roots(&self) -> Vec<RecentProjectRoot> {
        load_recent_project_roots_inner(&self.layout, USER).unwrap()
    }
}

#[test]
fn configuration_requires_an_injected_store_without_creating_appdata() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    let data = root.path().join("data");
    let layout = AgentPathLayout::from_app_roots(config.clone(), data.clone());
    assert!(load_agent_config_inner(&layout, USER).is_err());
    assert!(save_agent_config_inner(&layout, USER, &AgentConfig::default()).is_err());
    assert!(load_recent_project_roots_inner(&layout, USER).is_err());
    assert!(!config.exists());
    assert!(!data.exists());
}

#[test]
fn prior_and_current_json_sidecars_are_never_adopted_or_rewritten() {
    let fixture = Fixture::new();
    let prior = account_instruction_dir_path(&fixture.layout, USER).unwrap();
    let current = account_generated_resource_dir(&fixture.layout, USER).unwrap();
    let local = fixture
        .root
        .path()
        .join("data/agent/accounts")
        .join(account_scope(USER).unwrap());
    let sidecars = [
        prior.join("config.json"),
        prior.join("recent_roots.json"),
        current.join("config.json"),
        current.join("recent_roots.json"),
        local.join("removed_project_roots.json"),
    ];
    for path in &sidecars {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "old state is deliberately not valid JSON").unwrap();
    }
    let config = load_agent_config_inner(&fixture.layout, USER).unwrap();
    assert_eq!(config.default_model, default_agent_model());
    assert!(config.project_trust.is_empty());
    assert!(config.removed_project_roots.is_empty());
    assert!(fixture.roots().is_empty());
    save_agent_config_inner(&fixture.layout, USER, &config).unwrap();
    let project = fixture.project("new-project");
    register_explicit_project_root_inner(&fixture.layout, USER, &project).unwrap();
    save_removed_project_roots_inner(&fixture.layout, USER, &[path_string(&project)]).unwrap();
    for path in &sidecars {
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "old state is deliberately not valid JSON"
        );
    }
}

#[test]
fn defaults_and_trust_use_separate_authoritative_sql_records() {
    let fixture = Fixture::new();
    let project = fixture.project("trusted");
    let removed = fixture.project("removed");
    let mut config = AgentConfig {
        default_project_root: Some(path_string(&project)),
        default_model: "chosen-model".to_string(),
        default_thinking_level: Some(ThinkingLevel::High),
        removed_project_roots: vec![path_string(&removed)],
        ..AgentConfig::default()
    };
    apply_project_trust(&mut config, &project, false);
    save_agent_config_inner(&fixture.layout, USER, &config).unwrap();
    let account = fixture.account(USER);
    assert_eq!(
        fixture
            .store
            .account_setting(account, AGENT_DEFAULTS_KEY)
            .unwrap(),
        Some(json!({
            "defaultProjectRoot": path_string(&project),
            "defaultModel": "chosen-model",
            "defaultThinkingLevel": "high"
        }))
    );
    let project_row = fixture
        .store
        .project(account, &path_string(&project))
        .unwrap()
        .unwrap();
    assert_eq!(project_row.trusted, Some(false));
    assert_eq!(project_row.position, None);
    let loaded = load_agent_config_inner(&fixture.layout, USER).unwrap();
    assert_eq!(loaded.default_project_root, config.default_project_root);
    assert_eq!(loaded.default_model, config.default_model);
    assert_eq!(loaded.default_thinking_level, config.default_thinking_level);
    assert_eq!(loaded.project_trust, config.project_trust);
    assert!(loaded.removed_project_roots.is_empty());
    assert!(fixture.roots().is_empty());
    assert!(!fixture.layout.generated_resource_root.exists());
    assert!(!fixture.root.path().join("data/agent").exists());
}

#[test]
fn config_and_projects_are_isolated_between_accounts() {
    let fixture = Fixture::new();
    let project = fixture.project("private-project");
    let mut config = AgentConfig {
        default_model: "private-model".to_string(),
        ..AgentConfig::default()
    };
    apply_project_trust(&mut config, &project, true);
    save_agent_config_inner(&fixture.layout, USER, &config).unwrap();
    register_explicit_project_root_inner(&fixture.layout, USER, &project).unwrap();
    save_removed_project_roots_inner(&fixture.layout, USER, &[path_string(&project)]).unwrap();
    let other = load_agent_config_inner(&fixture.layout, OTHER_USER).unwrap();
    assert_eq!(other.default_model, default_agent_model());
    assert!(other.project_trust.is_empty());
    assert!(other.removed_project_roots.is_empty());
    assert!(
        load_recent_project_roots_inner(&fixture.layout, OTHER_USER)
            .unwrap()
            .is_empty()
    );
    assert!(
        fixture
            .store
            .account_projects(fixture.account(OTHER_USER))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn registering_an_existing_project_preserves_its_position_name_and_timestamp() {
    let fixture = Fixture::new();
    let first = fixture.project("first");
    let second = fixture.project("second");
    register_explicit_project_root_inner(&fixture.layout, USER, &first).unwrap();
    register_explicit_project_root_inner(&fixture.layout, USER, &second).unwrap();
    let account = fixture.account(USER);
    fixture
        .store
        .set_project_display_name(account, &path_string(&first), Some("Custom name"), 1)
        .unwrap();
    let before = fixture.roots();
    assert_eq!(before[0].path, path_string(&second));
    assert_eq!(before[1].name, "Custom name");
    let after = register_explicit_project_root_inner(&fixture.layout, USER, &first).unwrap();
    assert_eq!(after, before);
}

#[test]
fn project_removal_and_explicit_restore_preserve_trust_and_mcp_choices() {
    let fixture = Fixture::new();
    let first = fixture.project("first");
    let second = fixture.project("second");
    register_explicit_project_root_inner(&fixture.layout, USER, &first).unwrap();
    register_explicit_project_root_inner(&fixture.layout, USER, &second).unwrap();
    let account = fixture.account(USER);
    fixture
        .store
        .set_project_trust(account, &path_string(&first), Some(false), 1)
        .unwrap();
    fixture
        .store
        .set_project_mcp_defaults(account, &path_string(&first), &json!({"tools": true}), 1)
        .unwrap();
    save_removed_project_roots_inner(&fixture.layout, USER, &[path_string(&first)]).unwrap();
    assert_eq!(fixture.roots().len(), 1);
    let staged = restore_explicit_project_root_inner(&fixture.layout, USER, &first).unwrap();
    assert_eq!(staged[0].path, path_string(&first));
    assert_eq!(
        fixture.roots().len(),
        1,
        "restore must retain its tombstone until committed"
    );
    let row = fixture
        .store
        .project(account, &path_string(&first))
        .unwrap()
        .unwrap();
    assert!(row.removed_ms.is_some());
    assert_eq!(row.position, Some(0));
    save_removed_project_roots_inner(&fixture.layout, USER, &[]).unwrap();
    assert_eq!(fixture.roots()[0].path, path_string(&first));
    let row = fixture
        .store
        .project(account, &path_string(&first))
        .unwrap()
        .unwrap();
    assert_eq!(row.trusted, Some(false));
    assert_eq!(row.mcp_defaults, json!({"tools": true}));
    assert_eq!(row.removed_ms, None);
}

#[test]
fn stale_project_order_is_rejected_without_losing_existing_roots() {
    let fixture = Fixture::new();
    let first = fixture.project("first");
    let second = fixture.project("second");
    register_explicit_project_root_inner(&fixture.layout, USER, &first).unwrap();
    register_explicit_project_root_inner(&fixture.layout, USER, &second).unwrap();
    let before = fixture.roots();
    assert!(
        save_project_root_order_inner(&fixture.layout, USER, vec![path_string(&first)]).is_err()
    );
    assert_eq!(fixture.roots(), before);
    let reordered = save_project_root_order_inner(
        &fixture.layout,
        USER,
        vec![
            path_string(&first),
            path_string(&second),
            path_string(&first),
        ],
    )
    .unwrap();
    assert_eq!(
        reordered
            .iter()
            .map(|root| root.path.as_str())
            .collect::<Vec<_>>(),
        [path_string(&first).as_str(), path_string(&second).as_str()]
    );
    assert_eq!(reordered[0].last_used_ms, before[1].last_used_ms);
}

#[test]
fn model_compatibility_updates_only_the_new_sql_default_record() {
    let fixture = Fixture::new();
    let account = fixture.account(USER);
    fixture
        .store
        .set_account_setting(
            account,
            AGENT_DEFAULTS_KEY,
            &json!({
                "defaultProjectRoot": null,
                "defaultModel": LEGACY_AGENT_DEFAULT_MODEL
            }),
            1,
        )
        .unwrap();
    assert_eq!(
        load_agent_config_inner(&fixture.layout, USER)
            .unwrap()
            .default_model,
        default_agent_model()
    );
    assert_eq!(
        fixture
            .store
            .account_setting(account, AGENT_DEFAULTS_KEY)
            .unwrap()
            .unwrap()["defaultModel"],
        default_agent_model()
    );
    assert!(!fixture.layout.generated_resource_root.exists());
}

#[test]
fn bounded_defaults_and_target_state_do_not_collect_or_replace_other_project_maps() {
    let fixture = Fixture::new();
    let default = fixture.project("default");
    let target = fixture.project("target");
    let unrelated = fixture.project("unrelated");
    let config = AgentConfig {
        default_project_root: Some(path_string(&default)),
        default_model: "custom-model".into(),
        default_thinking_level: Some(ThinkingLevel::High),
        ..AgentConfig::default()
    };
    save_agent_config_inner(&fixture.layout, USER, &config).unwrap();
    let account = fixture.account(USER);
    for (root, trusted) in [(&default, false), (&target, true), (&unrelated, true)] {
        fixture
            .store
            .set_project_trust(account, &path_string(root), Some(trusted), 1)
            .unwrap();
    }
    fixture
        .store
        .remove_project(account, &path_string(&default), 2)
        .unwrap();
    fixture
        .store
        .remove_project(account, &path_string(&unrelated), 2)
        .unwrap();

    let defaults = load_agent_config_inner(&fixture.layout, USER).unwrap();
    assert_eq!(defaults.project_trust.len(), 1);
    assert_eq!(defaults.project_trust[0].path, path_string(&default));
    assert_eq!(defaults.removed_project_roots, vec![path_string(&default)]);
    // A save of the bounded defaults projection must not clear trust belonging
    // to projects that were deliberately absent from that projection.
    save_agent_config_inner(&fixture.layout, USER, &defaults).unwrap();
    let selected = load_project_config_inner(&fixture.layout, USER, &target).unwrap();
    assert_eq!(selected.project_trust.len(), 2);
    assert_eq!(project_trust_decision(&selected, &target, None), Some(true));
    fixture
        .store
        .set_project_trust(account, &path_string(&target), Some(false), 3)
        .unwrap();
    assert_eq!(
        project_trust_decision_inner(&fixture.layout, USER, &target).unwrap(),
        Some(false)
    );

    save_default_project_root_inner(&fixture.layout, USER, Some(path_string(&target))).unwrap();
    let changed = load_agent_config_inner(&fixture.layout, USER).unwrap();
    assert_eq!(changed.default_project_root, Some(path_string(&target)));
    assert_eq!(changed.default_model, "custom-model");
    assert_eq!(changed.default_thinking_level, Some(ThinkingLevel::High));
    assert_eq!(project_trust_decision(&changed, &target, None), Some(false));
    let untouched = fixture
        .store
        .project(account, &path_string(&unrelated))
        .unwrap()
        .unwrap();
    assert_eq!(untouched.trusted, Some(true));
    assert_eq!(untouched.removed_ms, Some(2));
    assert!(
        fixture
            .store
            .project(account, &path_string(&default))
            .unwrap()
            .unwrap()
            .removed_ms
            .is_some()
    );
}

#[test]
fn invalid_default_or_trust_save_cannot_partially_replace_saved_configuration() {
    let fixture = Fixture::new();
    let project = fixture.project("trusted");
    let mut config = AgentConfig {
        default_model: "saved-model".to_string(),
        ..AgentConfig::default()
    };
    apply_project_trust(&mut config, &project, true);
    save_agent_config_inner(&fixture.layout, USER, &config).unwrap();
    let account = fixture.account(USER);
    let before = fixture
        .store
        .account_setting(account, AGENT_DEFAULTS_KEY)
        .unwrap();
    let invalid_trust = vec![("relative/project".to_string(), false)];
    assert!(
        fixture
            .store
            .save_agent_defaults(
                account,
                &json!({
                    "defaultProjectRoot": null,
                    "defaultModel": "replacement"
                }),
                &invalid_trust,
                10
            )
            .is_err()
    );
    assert_eq!(
        fixture
            .store
            .account_setting(account, AGENT_DEFAULTS_KEY)
            .unwrap(),
        before
    );
    assert_eq!(
        fixture
            .store
            .project(account, &path_string(&project))
            .unwrap()
            .unwrap()
            .trusted,
        Some(true)
    );
    fixture
        .store
        .set_account_setting(
            account,
            AGENT_DEFAULTS_KEY,
            &json!({
                "defaultProjectRoot": null,
                "defaultModel": 42
            }),
            11,
        )
        .unwrap();
    assert!(load_agent_config_inner(&fixture.layout, USER).is_err());
    assert!(!fixture.layout.generated_resource_root.exists());
}
