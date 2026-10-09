//! What a task's model is told besides Maple's own instructions, through
//! Pi's resources: instruction files such as `AGENTS.md`, skills, prompt
//! templates, and `SYSTEM.md` and `APPEND_SYSTEM.md`.
//!
//! Pi's folders are the account's folder and a project's `.maple` folder,
//! where Pi has `~/.pi/agent` and `.pi`. Maple also reads the skill folders
//! and the shared `~/.agents/AGENTS.md` its Goose runtime read. A project's
//! skills, templates and prompt files load only once the user trusts the
//! project; its instruction files always do, as in Pi.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use pi_coding_agent::resources::{
    ContextFile, Diagnostic, PromptTemplate, ResourcePaths, Resources, Skill,
    expand_prompt_template, expand_skill_command, load_prompt_templates, load_skills,
    parse_frontmatter,
};

use super::AgentPathLayout;
use super::config::{
    account_config_dir_path, load_agent_config_file, normalize_project_root, project_trust_decision,
};
use super::types::{AgentProjectTrustFeature, AgentSlashCommand};

/// A project's folder of Maple resources, as `.pi` is Pi's.
const PROJECT_DIR_NAME: &str = ".maple";

/// A project's skill folders besides Pi's, which Maple's Goose runtime read.
const PROJECT_SKILL_DIRS: [&[&str]; 2] = [&[".goose", "skills"], &[".claude", "skills"]];

/// Skill folders in the home folder besides Pi's `~/.agents/skills`, which
/// Maple's Goose runtime read.
const USER_SKILL_DIRS: [&[&str]; 2] = [&[".claude", "skills"], &[".config", "agents", "skills"]];

/// Where one account looks for resources, for one working folder.
struct Scope {
    paths: ResourcePaths,
    cwd: PathBuf,
    /// Whether the project's own skills, templates and prompt files load.
    trusted: bool,
    skill_dirs: Vec<PathBuf>,
}

impl Scope {
    /// The scope of `user_id` in `cwd`, as the user's trust decision has it.
    fn new(layout: &AgentPathLayout, user_id: &str, cwd: Option<&Path>) -> Result<Self, String> {
        let trusted = cwd.is_some_and(|cwd| project_trusted(layout, user_id, cwd));
        Self::with_trust(layout, user_id, cwd, trusted)
    }

    /// The scope of `user_id` in `cwd` as if the project were trusted or
    /// not. Without a folder only the user's resources load.
    fn with_trust(
        layout: &AgentPathLayout,
        user_id: &str,
        cwd: Option<&Path>,
        trusted: bool,
    ) -> Result<Self, String> {
        let agent_dir = account_config_dir_path(layout, user_id).map_err(|e| e.to_string())?;
        let home_dir = layout.home().map(Path::to_path_buf);
        let trusted = trusted && cwd.is_some();
        // The account's folder stands in for a missing project, of which
        // nothing then loads.
        let cwd = cwd.map_or_else(|| agent_dir.clone(), Path::to_path_buf);
        let mut skill_dirs: Vec<PathBuf> = Vec::new();
        if trusted {
            skill_dirs.extend(PROJECT_SKILL_DIRS.iter().map(|dir| join(&cwd, dir)));
        }
        if let Some(home) = &home_dir {
            skill_dirs.extend(USER_SKILL_DIRS.iter().map(|dir| join(home, dir)));
        }
        // Pi reports a missing extra folder; these are only looked in.
        skill_dirs.retain(|dir| dir.is_dir());
        Ok(Self {
            paths: ResourcePaths {
                agent_dir,
                project_dir_name: PROJECT_DIR_NAME.to_string(),
                home_dir,
            },
            cwd,
            trusted,
            skill_dirs,
        })
    }

    fn skills(&self) -> Vec<Skill> {
        let (skills, diagnostics) =
            load_skills(&self.cwd, &self.paths, &self.skill_dirs, self.trusted);
        log_diagnostics(&diagnostics);
        skills
    }

    fn prompt_templates(&self) -> Vec<PromptTemplate> {
        let (templates, diagnostics) =
            load_prompt_templates(&self.cwd, &self.paths, &[], self.trusted);
        log_diagnostics(&diagnostics);
        templates
    }
}

fn join(base: &Path, parts: &[&str]) -> PathBuf {
    parts
        .iter()
        .fold(base.to_path_buf(), |path, part| path.join(part))
}

/// Pi's findings about a resource: an unreadable file, a name another
/// resource has, a name outside Pi's rules.
fn log_diagnostics(diagnostics: &[Diagnostic]) {
    for diagnostic in diagnostics {
        log::debug!(
            "Agent resource {}: {}",
            diagnostic.path.display(),
            diagnostic.message
        );
    }
}

/// Whether `user_id` trusts the project in `cwd`. Reads the saved decision
/// without taking the settings lock, since it writes nothing; a read that
/// fails trusts nothing.
fn project_trusted(layout: &AgentPathLayout, user_id: &str, cwd: &Path) -> bool {
    let Ok(root) = normalize_project_root(cwd) else {
        return false;
    };
    let config = account_config_dir_path(layout, user_id)
        .and_then(|dir| load_agent_config_file(&dir.join("config.json")));
    match config {
        Ok(config) => project_trust_decision(&config, &root) == Some(true),
        Err(error) => {
            log::warn!(
                "Failed to read Agent Mode project trust; keeping the project's skills off: {error}"
            );
            false
        }
    }
}

/// The resources of a task of `user_id` in `cwd`.
pub(super) fn load(
    layout: &AgentPathLayout,
    user_id: &str,
    cwd: &Path,
) -> Result<Resources, String> {
    let scope = Scope::new(layout, user_id, Some(cwd))?;
    let mut resources = Resources::load(
        &scope.cwd,
        &scope.paths,
        &scope.skill_dirs,
        &[],
        scope.trusted,
    );
    log_diagnostics(&resources.diagnostics);
    if let Some(shared) = layout.home().and_then(shared_instructions)
        && !resources
            .context_files
            .iter()
            .any(|file| file.path == shared.path)
    {
        resources.context_files.insert(0, shared);
    }
    Ok(resources)
}

/// `~/.agents/AGENTS.md`, the instructions every agent on the computer
/// shares, before the account's own.
fn shared_instructions(home: &Path) -> Option<ContextFile> {
    let path = home.join(".agents").join("AGENTS.md");
    let content = fs::read_to_string(&path).ok()?;
    Some(ContextFile {
        content: content
            .strip_prefix('\u{feff}')
            .unwrap_or(&content)
            .to_string(),
        path,
    })
}

/// The name a skill or template is typed as, or `None` when it cannot be
/// typed as one word.
fn command_name(name: &str) -> Option<String> {
    let name = name.trim_start_matches('/').to_lowercase();
    (!name.is_empty() && !name.contains('/') && !name.contains(char::is_whitespace)).then_some(name)
}

/// The composer's `/` commands of `user_id`'s skills, then of the prompt
/// templates whose names no skill has.
pub(super) fn slash_commands(
    layout: &AgentPathLayout,
    user_id: &str,
    cwd: Option<&Path>,
) -> Vec<AgentSlashCommand> {
    let scope = match Scope::new(layout, user_id, cwd) {
        Ok(scope) => scope,
        Err(error) => {
            log::warn!("Failed to list the account's skills: {error}");
            return Vec::new();
        }
    };
    let mut commands: Vec<AgentSlashCommand> = Vec::new();
    let skills = scope.skills().into_iter().map(|skill| {
        let input_hint = skill_argument_hint(&skill.file_path);
        (skill.name, skill.description, input_hint)
    });
    let templates = scope
        .prompt_templates()
        .into_iter()
        .map(|template| (template.name, template.description, template.argument_hint));
    for (name, description, input_hint) in skills.chain(templates) {
        let Some(name) = command_name(&name) else {
            continue;
        };
        if !commands.iter().any(|command| command.name == name) {
            commands.push(AgentSlashCommand {
                name,
                description,
                input_hint,
            });
        }
    }
    commands
}

/// The `argument-hint` a skill's frontmatter gives, at the top level or in
/// its `metadata`, as Maple's bundled skills give it.
fn skill_argument_hint(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let (frontmatter, _) = parse_frontmatter(&text).ok()?;
    [
        &frontmatter["argument-hint"],
        &frontmatter["metadata"]["argument-hint"],
    ]
    .into_iter()
    .find_map(|hint| hint.as_str())
    .map(str::trim)
    .filter(|hint| !hint.is_empty())
    .map(str::to_string)
}

/// Expand `/command args` into the prompt that runs it: a skill's
/// instructions as Pi's `/skill:name` gives them, or a prompt template
/// filled with the arguments. `None` when no skill or template has the
/// name, which is matched regardless of case.
pub(super) fn resolve_slash_command(
    layout: &AgentPathLayout,
    user_id: &str,
    cwd: Option<&Path>,
    command: &str,
    args: &str,
) -> Result<Option<String>, String> {
    let Some(command) = command_name(command) else {
        return Ok(None);
    };
    let scope = Scope::new(layout, user_id, cwd)?;
    let typed = |name: &str, prefix: &str| {
        format!("/{prefix}{name} {}", args.trim())
            .trim_end()
            .to_string()
    };
    let skills = scope.skills();
    if let Some(skill) = skills
        .iter()
        .find(|skill| command_name(&skill.name).as_deref() == Some(command.as_str()))
    {
        let text = typed(&skill.name, "skill:");
        let expanded = expand_skill_command(&text, std::slice::from_ref(skill));
        // Pi leaves a skill it cannot read as it was typed.
        if expanded == text {
            return Err(format!("The /{command} skill could not be read"));
        }
        return Ok(Some(expanded));
    }
    let templates = scope.prompt_templates();
    Ok(templates
        .iter()
        .find(|template| command_name(&template.name).as_deref() == Some(command.as_str()))
        .map(|template| {
            expand_prompt_template(&typed(&template.name, ""), std::slice::from_ref(template))
        }))
}

/// What trusting the project in `root` would add to its tasks: skills and
/// prompt templates that load only then, and its `SYSTEM.md` or
/// `APPEND_SYSTEM.md`.
pub(super) fn protected_features(
    layout: &AgentPathLayout,
    user_id: &str,
    root: &Path,
) -> Vec<AgentProjectTrustFeature> {
    let scope = |trusted| Scope::with_trust(layout, user_id, Some(root), trusted);
    let (trusted, untrusted) = match (scope(true), scope(false)) {
        (Ok(trusted), Ok(untrusted)) => (trusted, untrusted),
        (Err(error), _) | (_, Err(error)) => {
            log::warn!("Failed to look for the project's skills: {error}");
            return Vec::new();
        }
    };
    let mut features = Vec::new();
    let user_skills: HashSet<PathBuf> = untrusted
        .skills()
        .into_iter()
        .map(|skill| skill.file_path)
        .collect();
    if trusted
        .skills()
        .iter()
        .any(|skill| !user_skills.contains(&skill.file_path))
    {
        features.push(AgentProjectTrustFeature::Skills);
    }
    let user_templates: HashSet<PathBuf> = untrusted
        .prompt_templates()
        .into_iter()
        .map(|template| template.file_path)
        .collect();
    if trusted
        .prompt_templates()
        .iter()
        .any(|template| !user_templates.contains(&template.file_path))
    {
        features.push(AgentProjectTrustFeature::PromptTemplates);
    }
    let project_dir = root.join(PROJECT_DIR_NAME);
    if ["SYSTEM.md", "APPEND_SYSTEM.md"]
        .iter()
        .any(|name| project_dir.join(name).is_file())
    {
        features.push(AgentProjectTrustFeature::SystemPrompt);
    }
    features
}

#[cfg(test)]
mod tests;
