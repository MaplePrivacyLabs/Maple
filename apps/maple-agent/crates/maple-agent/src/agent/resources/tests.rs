use super::*;
use crate::agent::AgentConfig;
use crate::agent::config::{apply_project_trust, save_agent_config_inner};

const USER: &str = "skills@example.com";

/// An account, a home folder and a project, each of its own.
struct Fixture {
    root: tempfile::TempDir,
    layout: AgentPathLayout,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let layout =
            AgentPathLayout::from_app_roots(root.path().join("config"), root.path().join("local"))
                .with_home(Some(root.path().join("home")));
        fs::create_dir_all(root.path().join("project")).unwrap();
        Self { root, layout }
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn project(&self) -> PathBuf {
        self.root.path().join("project").canonicalize().unwrap()
    }

    fn account(&self) -> PathBuf {
        account_config_dir_path(&self.layout, USER).unwrap()
    }

    fn write(&self, path: PathBuf, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn skill(&self, dir: PathBuf, name: &str) {
        self.write(
            dir.join(name).join("SKILL.md"),
            &format!("---\nname: {name}\ndescription: The {name} skill\n---\nDo {name}."),
        );
    }

    fn trust(&self, trusted: bool) {
        let mut config = AgentConfig::default();
        apply_project_trust(&mut config, &self.project(), trusted);
        save_agent_config_inner(&self.layout, USER, &config).unwrap();
    }

    fn commands(&self) -> Vec<AgentSlashCommand> {
        slash_commands(&self.layout, USER, Some(&self.project()))
    }

    fn command_names(&self) -> Vec<String> {
        self.commands()
            .into_iter()
            .map(|command| command.name)
            .collect()
    }

    fn resolve(&self, command: &str, args: &str) -> Option<String> {
        resolve_slash_command(&self.layout, USER, Some(&self.project()), command, args).unwrap()
    }

    fn features(&self) -> Vec<AgentProjectTrustFeature> {
        protected_features(&self.layout, USER, &self.project())
    }
}

/// A fixture with skills and templates in every folder Maple reads.
fn everywhere() -> Fixture {
    let fixture = Fixture::new();
    let (account, home, project) = (fixture.account(), fixture.home(), fixture.project());
    fixture.write(
        account.join("skills/deploy/SKILL.md"),
        "---\nname: deploy\ndescription: Deploy the app\nmetadata:\n  argument-hint: \"<env>\"\n---\nDeploy to the named environment.",
    );
    fixture.skill(home.join(".agents/skills"), "review");
    fixture.skill(home.join(".claude/skills"), "claude-one");
    fixture.skill(home.join(".config/agents/skills"), "shared");
    fixture.skill(project.join(".maple/skills"), "maple-one");
    fixture.skill(project.join(".agents/skills"), "build");
    fixture.skill(project.join(".goose/skills"), "goosey");
    fixture.skill(project.join(".claude/skills"), "lint");
    fixture.write(
        account.join("prompts/fix.md"),
        "---\ndescription: Fix an issue\nargument-hint: <issue>\n---\nFix issue $1 carefully.",
    );
    // A skill has this name already.
    fixture.write(account.join("prompts/deploy.md"), "Deploy, as a template.");
    fixture.write(project.join(".maple/prompts/ship.md"), "Ship $@ today.");
    fixture
}

#[test]
fn skills_and_templates_become_commands_and_a_projects_need_trust() {
    let fixture = everywhere();
    assert_eq!(
        fixture.command_names(),
        ["deploy", "review", "claude-one", "shared", "fix"]
    );
    let commands = fixture.commands();
    assert_eq!(commands[0].description, "Deploy the app");
    assert_eq!(commands[0].input_hint.as_deref(), Some("<env>"));
    assert_eq!(commands[4].input_hint.as_deref(), Some("<issue>"));

    // A trusted project's own come first, as they win a name.
    fixture.trust(true);
    assert_eq!(
        fixture.command_names(),
        [
            "maple-one",
            "build",
            "deploy",
            "review",
            "goosey",
            "lint",
            "claude-one",
            "shared",
            "ship",
            "fix",
        ]
    );
    fixture.trust(false);
    assert_eq!(fixture.command_names().len(), 5);

    // Without a project, the user's alone.
    assert_eq!(
        slash_commands(&fixture.layout, USER, None).len(),
        fixture.command_names().len()
    );
}

#[test]
fn a_command_expands_to_its_skill_or_fills_its_template() {
    let fixture = everywhere();
    let skill = fixture.resolve("Deploy", "staging").unwrap();
    assert!(
        skill.starts_with("<skill name=\"deploy\" location=\""),
        "{skill}"
    );
    assert!(
        skill.ends_with("Deploy to the named environment.\n</skill>\n\nstaging"),
        "{skill}"
    );
    assert!(!fixture.resolve("review", "").unwrap().ends_with('\n'));
    assert_eq!(
        fixture.resolve("fix", "42").as_deref(),
        Some("Fix issue 42 carefully.")
    );
    assert_eq!(fixture.resolve("unknown", ""), None);
    assert_eq!(fixture.resolve("", ""), None);

    // A project's skills and templates run once it is trusted.
    assert_eq!(fixture.resolve("build", ""), None);
    assert_eq!(fixture.resolve("ship", "it"), None);
    fixture.trust(true);
    assert!(fixture.resolve("build", "").unwrap().contains("Do build."));
    assert_eq!(
        fixture.resolve("ship", "it now").as_deref(),
        Some("Ship it now today.")
    );
}

#[test]
fn instruction_files_always_load_and_a_projects_skills_once_trusted() {
    let fixture = Fixture::new();
    let (account, home, project) = (fixture.account(), fixture.home(), fixture.project());
    fixture.write(home.join(".agents/AGENTS.md"), "Shared rules.");
    fixture.write(account.join("AGENTS.md"), "Account rules.");
    fixture.write(project.join("AGENTS.md"), "Project rules.");
    fixture.skill(project.join(".agents/skills"), "build");
    fixture.write(project.join(".maple/SYSTEM.md"), "You are a release bot.");

    let resources = load(&fixture.layout, USER, &project).unwrap();
    let files: Vec<(&Path, &str)> = resources
        .context_files
        .iter()
        .map(|file| (file.path.as_path(), file.content.as_str()))
        .collect();
    assert_eq!(
        files,
        [
            (home.join(".agents/AGENTS.md").as_path(), "Shared rules."),
            (account.join("AGENTS.md").as_path(), "Account rules."),
            (project.join("AGENTS.md").as_path(), "Project rules."),
        ]
    );
    assert!(resources.skills.is_empty());
    assert_eq!(resources.system_prompt, None);

    fixture.trust(true);
    let resources = load(&fixture.layout, USER, &project).unwrap();
    assert_eq!(resources.skills[0].name, "build");
    assert_eq!(
        resources.system_prompt.unwrap().content,
        "You are a release bot."
    );
}

#[test]
fn the_trust_prompt_names_what_trust_would_add() {
    // A project skill counts in any folder trust opens.
    for folder in [
        ".agents/skills",
        ".maple/skills",
        ".goose/skills",
        ".claude/skills",
    ] {
        let fixture = Fixture::new();
        fixture.skill(fixture.project().join(folder), "lint");
        assert_eq!(
            fixture.features(),
            [AgentProjectTrustFeature::Skills],
            "{folder}"
        );
    }

    let fixture = Fixture::new();
    let project = fixture.project();
    // The user's own skills are not the project's.
    fixture.skill(fixture.home().join(".agents/skills"), "review");
    assert_eq!(fixture.features(), []);
    // One of Pi's project folders wins a name over the user's; the extra
    // folders come after the user's, as Pi's extra paths do.
    fixture.skill(project.join(".claude/skills"), "review");
    assert_eq!(fixture.features(), []);
    fixture.skill(project.join(".agents/skills"), "review");
    assert_eq!(fixture.features(), [AgentProjectTrustFeature::Skills]);
    fixture.write(project.join(".maple/prompts/ship.md"), "Ship it.");
    // Maple's opening instructions take APPEND_SYSTEM.md's place.
    fixture.write(project.join(".maple/APPEND_SYSTEM.md"), "Be brief.");
    assert_eq!(
        fixture.features(),
        [
            AgentProjectTrustFeature::Skills,
            AgentProjectTrustFeature::PromptTemplates,
        ]
    );
    fixture.write(project.join(".maple/SYSTEM.md"), "You ship releases.");
    assert_eq!(
        fixture.features(),
        [
            AgentProjectTrustFeature::Skills,
            AgentProjectTrustFeature::PromptTemplates,
            AgentProjectTrustFeature::SystemPrompt,
        ]
    );
    // The answer does not change with the decision.
    fixture.trust(true);
    assert_eq!(fixture.features().len(), 3);
}
