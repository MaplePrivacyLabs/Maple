use std::sync::Mutex;

use async_trait::async_trait;

use super::*;
use crate::extensions::NoUi;

fn paths(root: &Path) -> ResourcePaths {
    ResourcePaths {
        agent_dir: root.join("agent"),
        project_dir_name: ".maple".into(),
        home_dir: Some(root.join("home")),
    }
}

#[test]
fn a_folder_inherits_the_nearest_decision_above_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = normalize(dir.path());
    let store = ProjectTrustStore::new(&root.join("agent"));
    let project = root.join("work/repo/app");
    fs::create_dir_all(&project).unwrap();
    assert_eq!(store.get(&project).unwrap(), None);

    store.set(&root.join("work"), Some(true)).unwrap();
    assert_eq!(store.get(&project).unwrap(), Some(true));
    store.set(&root.join("work/repo"), Some(false)).unwrap();
    assert_eq!(
        store.get_entry(&project).unwrap(),
        Some(ProjectTrustEntry {
            path: root.join("work/repo"),
            decision: false
        })
    );
    store.set(&root.join("work/repo"), None).unwrap();
    assert_eq!(store.get(&project).unwrap(), Some(true));
    assert!(fs::read_to_string(store.path()).unwrap().ends_with("}\n"));
}

#[test]
fn a_damaged_store_is_an_error_not_a_decision() {
    let dir = tempfile::tempdir().unwrap();
    let store = ProjectTrustStore::new(dir.path());
    fs::write(store.path(), r#"{"/work": 1}"#).unwrap();
    let error = store.get(dir.path()).unwrap_err();
    assert!(
        error.to_string().starts_with("Invalid trust store"),
        "{error}"
    );
}

#[test]
fn only_a_projects_own_resources_need_trust() {
    let dir = tempfile::tempdir().unwrap();
    let root = normalize(dir.path());
    let paths = paths(&root);
    let cwd = root.join("work/app");
    fs::create_dir_all(&cwd).unwrap();
    assert!(!has_trust_requiring_project_resources(&cwd, &paths));

    // The user's own skills are not a project's, even in the home folder.
    fs::create_dir_all(root.join("home/.agents/skills")).unwrap();
    assert!(!has_trust_requiring_project_resources(
        &root.join("home"),
        &paths
    ));

    fs::create_dir_all(root.join("work/.agents/skills")).unwrap();
    assert!(has_trust_requiring_project_resources(&cwd, &paths));
    fs::remove_dir_all(root.join("work/.agents")).unwrap();
    fs::create_dir_all(cwd.join(".maple")).unwrap();
    fs::write(cwd.join(".maple/SYSTEM.md"), "Be terse.").unwrap();
    assert!(has_trust_requiring_project_resources(&cwd, &paths));
}

#[test]
fn the_answers_offered_are_pis() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = normalize(dir.path()).join("app");
    let labels: Vec<String> = project_trust_options(&cwd, true)
        .into_iter()
        .map(|option| option.label)
        .collect();
    assert_eq!(
        labels,
        [
            "Trust".to_string(),
            format!("Trust parent folder ({})", cwd.parent().unwrap().display()),
            "Trust (this session only)".to_string(),
            "Do not trust".to_string(),
            "Do not trust (this session only)".to_string(),
        ]
    );
    assert_eq!(project_trust_options(&cwd, false).len(), 3);
}

/// An interface that picks one answer and remembers the question.
struct Picks {
    choice: Option<usize>,
    asked: Mutex<Vec<(String, Vec<String>)>>,
}

#[async_trait]
impl ExtensionUi for Picks {
    fn has_ui(&self) -> bool {
        true
    }

    async fn select(&self, title: &str, options: &[String]) -> Option<usize> {
        self.asked
            .lock()
            .unwrap()
            .push((title.to_string(), options.to_vec()));
        self.choice
    }
}

#[tokio::test]
async fn trust_is_resolved_in_pis_order() {
    let dir = tempfile::tempdir().unwrap();
    let root = normalize(dir.path());
    let paths = paths(&root);
    let store = ProjectTrustStore::new(&paths.agent_dir);
    let cwd = root.join("work/app");
    fs::create_dir_all(cwd.join(".maple/skills")).unwrap();
    let request = |trust_override, default_trust| ProjectTrustRequest {
        cwd: &cwd,
        paths: &paths,
        store: &store,
        trust_override,
        default_trust,
        app_name: "Maple",
    };

    let overridden = request(Some(false), DefaultProjectTrust::Always);
    assert!(!resolve_project_trusted(overridden, &NoUi).await.unwrap());
    let always = request(None, DefaultProjectTrust::Always);
    assert!(resolve_project_trusted(always, &NoUi).await.unwrap());
    let never = request(None, DefaultProjectTrust::Never);
    assert!(!resolve_project_trusted(never, &NoUi).await.unwrap());
    // With no one to ask, the folder is not trusted and nothing is remembered.
    let ask = request(None, DefaultProjectTrust::Ask);
    assert!(!resolve_project_trusted(ask, &NoUi).await.unwrap());
    assert_eq!(store.get(&cwd).unwrap(), None);

    // Trusting the parent folder is remembered for it.
    let ui = Picks {
        choice: Some(1),
        asked: Mutex::new(Vec::new()),
    };
    let ask = request(None, DefaultProjectTrust::Ask);
    assert!(resolve_project_trusted(ask, &ui).await.unwrap());
    let (question, answers) = ui.asked.lock().unwrap()[0].clone();
    assert_eq!(
        question,
        format!(
            "Trust project folder?\n{}\n\nThis allows Maple to load .maple settings and resources.",
            cwd.display()
        )
    );
    assert_eq!(answers.len(), 5);
    assert_eq!(
        store.get_entry(&cwd).unwrap().unwrap().path,
        root.join("work")
    );
    let never = request(None, DefaultProjectTrust::Never);
    assert!(resolve_project_trusted(never, &NoUi).await.unwrap());

    // An answer for this session only is not remembered.
    let other = root.join("elsewhere/app");
    fs::create_dir_all(other.join(".maple/prompts")).unwrap();
    let ui = Picks {
        choice: Some(2),
        asked: Mutex::new(Vec::new()),
    };
    let ask = ProjectTrustRequest {
        cwd: &other,
        ..request(None, DefaultProjectTrust::Ask)
    };
    assert!(resolve_project_trusted(ask, &ui).await.unwrap());
    assert_eq!(store.get(&other).unwrap(), None);

    // A folder without resources of its own needs no decision.
    let plain = root.join("plain");
    fs::create_dir_all(&plain).unwrap();
    let never = ProjectTrustRequest {
        cwd: &plain,
        ..request(None, DefaultProjectTrust::Never)
    };
    assert!(resolve_project_trusted(never, &NoUi).await.unwrap());
}
