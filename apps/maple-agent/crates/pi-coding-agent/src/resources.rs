//! Instructions and reusable prompts found on disk: context files (`AGENTS.md`), skills,
//! prompt templates, and `SYSTEM.md` and `APPEND_SYSTEM.md`. Folder names come from the
//! host, so nothing here is tied to one application's layout. A project's own resources
//! load only when the project is trusted (see [`trust`](crate::trust)); context files
//! always do, as in Pi.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// Where resources live.
#[derive(Clone, Debug, PartialEq)]
pub struct ResourcePaths {
    /// The user-level configuration directory.
    pub agent_dir: PathBuf,
    /// The project-level folder name looked up in the working directory, such as `.maple`.
    pub project_dir_name: String,
    /// The user's home folder, where `~/.agents/skills` is; `None` leaves those out.
    pub home_dir: Option<PathBuf>,
}

impl ResourcePaths {
    /// Paths for `agent_dir` and `project_dir_name`, with this user's home folder.
    pub fn new(agent_dir: impl Into<PathBuf>, project_dir_name: &str) -> Self {
        Self {
            agent_dir: agent_dir.into(),
            project_dir_name: project_dir_name.to_string(),
            home_dir: std::env::home_dir(),
        }
    }

    pub fn project_dir(&self, cwd: &Path) -> PathBuf {
        cwd.join(&self.project_dir_name)
    }

    /// `~/.agents/skills`, the skills every agent on this computer shares.
    fn user_agents_skills(&self) -> Option<PathBuf> {
        Some(self.home_dir.as_ref()?.join(".agents").join("skills"))
    }
}

/// A problem found while loading a resource. Loading goes on without it.
#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    pub path: PathBuf,
    pub message: String,
}

fn diagnostic(path: &Path, message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

/// Split YAML frontmatter from a Markdown body. Text without frontmatter is all body.
pub fn parse_frontmatter(text: &str) -> Result<(Value, String), String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let normalized = text.replace("\r\n", "\n");
    let Some(rest) = normalized.strip_prefix("---\n") else {
        return Ok((Value::Object(Default::default()), normalized));
    };
    let (yaml, after) = match rest.strip_prefix("---") {
        Some(after) => ("", after),
        None => match rest.find("\n---") {
            Some(end) => (&rest[..end], &rest[end + 4..]),
            None => return Err("frontmatter is not closed".into()),
        },
    };
    let body = after.strip_prefix('\n').unwrap_or(after);
    let value: Value = if yaml.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        serde_yaml::from_str(yaml).map_err(|error| error.to_string())?
    };
    Ok((value, body.to_string()))
}

/// An instruction file that applies to the working directory.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

const CONTEXT_FILE_NAMES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

fn context_file_in(dir: &Path) -> Option<ContextFile> {
    CONTEXT_FILE_NAMES.iter().find_map(|name| {
        let path = dir.join(name);
        if !path.is_file() {
            return None;
        }
        let content = fs::read_to_string(&path).ok()?;
        Some(ContextFile {
            content: content
                .strip_prefix('\u{feff}')
                .unwrap_or(&content)
                .to_string(),
            path,
        })
    })
}

/// The user-level context file, then one per directory from the root down to `cwd`.
pub fn load_context_files(cwd: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    let mut files: Vec<ContextFile> = context_file_in(agent_dir).into_iter().collect();
    let mut ancestors: Vec<ContextFile> = cwd.ancestors().filter_map(context_file_in).collect();
    ancestors.reverse();
    for file in ancestors {
        if !files.iter().any(|existing| existing.path == file.path) {
            files.push(file);
        }
    }
    files
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceSource {
    User,
    Project,
    Path,
}

/// A skill: instructions for a kind of task that the model loads when it needs them.
#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub source: ResourceSource,
    /// Only explicit `/skill:name` use; the model is not told about it.
    pub disable_model_invocation: bool,
}

const MAX_SKILL_NAME: usize = 64;
const MAX_SKILL_DESCRIPTION: usize = 1_024;

fn skill_name_problems(name: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if name.len() > MAX_SKILL_NAME {
        problems.push(format!(
            "name exceeds {MAX_SKILL_NAME} characters ({})",
            name.len()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        problems.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)".into(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        problems.push("name must not start or end with a hyphen".into());
    }
    if name.contains("--") {
        problems.push("name must not contain consecutive hyphens".into());
    }
    problems
}

fn load_skill(
    path: &Path,
    source: ResourceSource,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Skill> {
    let declared = path.file_name().is_some_and(|name| name == "SKILL.md");
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            diagnostics.push(diagnostic(path, error.to_string()));
            return None;
        }
    };
    let frontmatter = match parse_frontmatter(&text) {
        Ok((frontmatter, _)) => frontmatter,
        Err(error) => {
            if declared {
                diagnostics.push(diagnostic(path, error));
            }
            return None;
        }
    };
    let description = frontmatter["description"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    if description.is_empty() {
        // Any Markdown file may sit in a skills folder; only SKILL.md must be a skill.
        if declared {
            diagnostics.push(diagnostic(path, "description is required"));
        }
        return None;
    }
    if description.len() > MAX_SKILL_DESCRIPTION {
        diagnostics.push(diagnostic(
            path,
            format!("description exceeds {MAX_SKILL_DESCRIPTION} characters"),
        ));
    }
    let base_dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
    // A SKILL.md is named after its folder, any other file after itself.
    let fallback = if declared {
        base_dir.file_name()
    } else {
        path.file_stem()
    };
    let name = frontmatter["name"]
        .as_str()
        .map(str::to_string)
        .or_else(|| fallback.map(|name| name.to_string_lossy().into_owned()))
        .unwrap_or_default();
    for problem in skill_name_problems(&name) {
        diagnostics.push(diagnostic(path, problem));
    }
    Some(Skill {
        name,
        description,
        file_path: path.to_path_buf(),
        base_dir,
        source,
        disable_model_invocation: frontmatter["disable-model-invocation"] == Value::Bool(true),
    })
}

/// Which Markdown files other than `SKILL.md` can be skills.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LooseSkills {
    /// Those directly in the skills folder, as in the agent's own `skills` folders.
    TopLevel,
    /// Those in its subfolders, as in `.agents/skills`.
    Nested,
}

/// A folder's `SKILL.md` makes the folder a skill; otherwise its subfolders are searched,
/// and other Markdown files with a description count too, where `loose` says.
fn scan_skills(
    dir: &Path,
    source: ResourceSource,
    loose: LooseSkills,
    top: bool,
    skills: &mut Vec<Skill>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let skill_file = dir.join("SKILL.md");
    if skill_file.is_file() {
        skills.extend(load_skill(&skill_file, source, diagnostics));
        return;
    }
    let Ok(read) = fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = read.flatten().map(|entry| entry.path()).collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        if path.is_dir() {
            scan_skills(&path, source, loose, false, skills, diagnostics);
        } else if name.ends_with(".md") {
            let counts = match loose {
                LooseSkills::TopLevel => top,
                LooseSkills::Nested => !top,
            };
            if counts {
                skills.extend(load_skill(&path, source, diagnostics));
            }
        }
    }
}

/// `.agents/skills` in `cwd` and each folder above it, up to the repository root when
/// there is one. The user's own `~/.agents/skills` is not a project's.
fn project_agents_skill_dirs(cwd: &Path, paths: &ResourcePaths) -> Vec<PathBuf> {
    let repo_root = cwd.ancestors().find(|dir| dir.join(".git").exists());
    let user = paths.user_agents_skills();
    let mut dirs = Vec::new();
    for dir in cwd.ancestors() {
        let skills = dir.join(".agents").join("skills");
        if Some(&skills) != user.as_ref() {
            dirs.push(skills);
        }
        if Some(dir) == repo_root {
            break;
        }
    }
    dirs
}

/// Skills from the project's and the user's folders and any extra paths, in that order:
/// the project folder's `skills` and `.agents/skills` from the working directory up
/// (only for a trusted project), the agent folder's `skills`, `~/.agents/skills`, then
/// the extra paths. The first skill with a name wins, so a project's skill overrides the
/// user's; later ones are reported as collisions.
pub fn load_skills(
    cwd: &Path,
    paths: &ResourcePaths,
    extra: &[PathBuf],
    project_trusted: bool,
) -> (Vec<Skill>, Vec<Diagnostic>) {
    let mut found = Vec::new();
    let mut diagnostics = Vec::new();
    if project_trusted {
        scan_skills(
            &paths.project_dir(cwd).join("skills"),
            ResourceSource::Project,
            LooseSkills::TopLevel,
            true,
            &mut found,
            &mut diagnostics,
        );
        for dir in project_agents_skill_dirs(cwd, paths) {
            scan_skills(
                &dir,
                ResourceSource::Project,
                LooseSkills::Nested,
                true,
                &mut found,
                &mut diagnostics,
            );
        }
    }
    scan_skills(
        &paths.agent_dir.join("skills"),
        ResourceSource::User,
        LooseSkills::TopLevel,
        true,
        &mut found,
        &mut diagnostics,
    );
    if let Some(dir) = paths.user_agents_skills() {
        scan_skills(
            &dir,
            ResourceSource::User,
            LooseSkills::Nested,
            true,
            &mut found,
            &mut diagnostics,
        );
    }
    for path in extra {
        let path = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        if path.is_dir() {
            scan_skills(
                &path,
                ResourceSource::Path,
                LooseSkills::TopLevel,
                true,
                &mut found,
                &mut diagnostics,
            );
        } else if path.is_file() && path.extension().is_some_and(|extension| extension == "md") {
            found.extend(load_skill(&path, ResourceSource::Path, &mut diagnostics));
        } else {
            diagnostics.push(diagnostic(
                &path,
                "skill path does not exist or is not a Markdown file",
            ));
        }
    }
    let mut skills: Vec<Skill> = Vec::new();
    let mut files = HashSet::new();
    for skill in found {
        let real = fs::canonicalize(&skill.file_path).unwrap_or_else(|_| skill.file_path.clone());
        if !files.insert(real) {
            continue;
        }
        if let Some(winner) = skills.iter().find(|existing| existing.name == skill.name) {
            diagnostics.push(diagnostic(
                &skill.file_path,
                format!(
                    "name \"{}\" collides with {}",
                    skill.name,
                    winner.file_path.display()
                ),
            ));
        } else {
            skills.push(skill);
        }
    }
    (skills, diagnostics)
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The skills section of a system prompt. `how_to_load` tells the model how to read a
/// skill file, for example "Use the read tool to load a skill's file".
pub fn format_skills_for_prompt(skills: &[Skill], how_to_load: &str) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "The following skills provide specialized instructions for specific tasks.".to_string(),
        format!("{how_to_load} when the task matches its description."),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".into(),
        String::new(),
        "<available_skills>".into(),
    ];
    for skill in visible {
        lines.push("  <skill>".into());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path.to_string_lossy())
        ));
        lines.push("  </skill>".into());
    }
    lines.push("</available_skills>".into());
    lines.join("\n")
}

/// Expand `/skill:name args` into the skill's instructions followed by the arguments.
/// Other text, and unknown or unreadable skills, pass through unchanged.
pub fn expand_skill_command(text: &str, skills: &[Skill]) -> String {
    let Some(rest) = text.strip_prefix("/skill:") else {
        return text.to_string();
    };
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let Some(skill) = skills.iter().find(|skill| skill.name == name) else {
        return text.to_string();
    };
    let Ok(Ok((_, body))) =
        fs::read_to_string(&skill.file_path).map(|content| parse_frontmatter(&content))
    else {
        return text.to_string();
    };
    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name,
        skill.file_path.display(),
        skill.base_dir.display(),
        body.trim()
    );
    let args = args.trim();
    if args.is_empty() {
        block
    } else {
        format!("{block}\n\n{args}")
    }
}

/// A reusable prompt invoked as `/name args`.
#[derive(Clone, Debug, PartialEq)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub content: String,
    pub file_path: PathBuf,
    pub source: ResourceSource,
}

fn load_templates_in(
    dir: &Path,
    source: ResourceSource,
    templates: &mut Vec<PromptTemplate>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Ok(read) = fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = read
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file() && path.extension().is_some_and(|extension| extension == "md")
        })
        .collect();
    paths.sort();
    for path in paths {
        let parsed = fs::read_to_string(&path)
            .map_err(|error| error.to_string())
            .and_then(|text| parse_frontmatter(&text));
        let (frontmatter, body) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                diagnostics.push(diagnostic(&path, error));
                continue;
            }
        };
        let description = match frontmatter["description"].as_str() {
            Some(description) => description.to_string(),
            None => {
                let first = body
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or_default();
                let short: String = first.chars().take(60).collect();
                if first.chars().count() > 60 {
                    format!("{short}...")
                } else {
                    short
                }
            }
        };
        templates.push(PromptTemplate {
            name: path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default(),
            description,
            argument_hint: frontmatter["argument-hint"].as_str().map(str::to_string),
            content: body,
            file_path: path,
            source,
        });
    }
}

/// Templates from the project's (only when trusted) and the user's `prompts` folders and
/// any extra folders. The first template with a name wins, so a project's template
/// overrides the user's; later ones are reported as collisions.
pub fn load_prompt_templates(
    cwd: &Path,
    paths: &ResourcePaths,
    extra: &[PathBuf],
    project_trusted: bool,
) -> (Vec<PromptTemplate>, Vec<Diagnostic>) {
    let mut templates = Vec::new();
    let mut diagnostics = Vec::new();
    if project_trusted {
        load_templates_in(
            &paths.project_dir(cwd).join("prompts"),
            ResourceSource::Project,
            &mut templates,
            &mut diagnostics,
        );
    }
    load_templates_in(
        &paths.agent_dir.join("prompts"),
        ResourceSource::User,
        &mut templates,
        &mut diagnostics,
    );
    for path in extra {
        let path = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        load_templates_in(
            &path,
            ResourceSource::Path,
            &mut templates,
            &mut diagnostics,
        );
    }
    let mut unique: Vec<PromptTemplate> = Vec::new();
    for template in templates {
        if let Some(winner) = unique
            .iter()
            .find(|existing| existing.name == template.name)
        {
            diagnostics.push(diagnostic(
                &template.file_path,
                format!(
                    "name \"{}\" collides with {}",
                    template.name,
                    winner.file_path.display()
                ),
            ));
        } else {
            unique.push(template);
        }
    }
    (unique, diagnostics)
}

/// Split arguments on whitespace, keeping quoted strings together.
pub fn parse_command_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for ch in input.chars() {
        match quote {
            Some(open) if ch == open => quote = None,
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\{(\d+|ARGUMENTS|@):-([^}]*)\}|\$\{@:(\d+)(?::(\d+))?\}|\$(ARGUMENTS|@|\d+)")
        .expect("placeholder pattern compiles")
});

/// Fill a template: `$1`, `$@`/`$ARGUMENTS`, `${N:-default}`, `${@:-default}`, and
/// `${@:N}`/`${@:N:L}` slices. Values are not expanded again.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all = args.join(" ");
    PLACEHOLDER
        .replace_all(content, |captures: &regex::Captures<'_>| {
            let nth = |position: &str| -> String {
                position
                    .parse::<usize>()
                    .ok()
                    .and_then(|position| args.get(position.wrapping_sub(1)))
                    .cloned()
                    .unwrap_or_default()
            };
            if let Some(target) = captures.get(1) {
                let value = match target.as_str() {
                    "@" | "ARGUMENTS" => all.clone(),
                    position => nth(position),
                };
                return if value.is_empty() {
                    captures[2].to_string()
                } else {
                    value
                };
            }
            if let Some(start) = captures.get(3) {
                // A start too large to parse is past every argument.
                let start = start
                    .as_str()
                    .parse::<usize>()
                    .unwrap_or(usize::MAX)
                    .saturating_sub(1);
                let rest = args.iter().skip(start);
                return match captures
                    .get(4)
                    .and_then(|length| length.as_str().parse::<usize>().ok())
                {
                    Some(length) => rest.take(length).cloned().collect::<Vec<_>>().join(" "),
                    None => rest.cloned().collect::<Vec<_>>().join(" "),
                };
            }
            match &captures[5] {
                "@" | "ARGUMENTS" => all.clone(),
                position => nth(position),
            }
        })
        .into_owned()
}

/// Expand `/name args` when `name` is a template; other text passes through.
pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
    let Some(rest) = text.strip_prefix('/') else {
        return text.to_string();
    };
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    match templates.iter().find(|template| template.name == name) {
        Some(template) => substitute_args(&template.content, &parse_command_args(args)),
        None => text.to_string(),
    }
}

/// `name` from the project folder when the project is trusted and has it, else from the
/// agent folder.
fn load_prompt_file(
    cwd: &Path,
    paths: &ResourcePaths,
    name: &str,
    project_trusted: bool,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<ContextFile> {
    let project = project_trusted.then(|| paths.project_dir(cwd).join(name));
    let path = project
        .into_iter()
        .chain([paths.agent_dir.join(name)])
        .find(|path| path.is_file())?;
    match fs::read_to_string(&path) {
        Ok(content) => Some(ContextFile {
            content: content
                .strip_prefix('\u{feff}')
                .unwrap_or(&content)
                .to_string(),
            path,
        }),
        Err(error) => {
            diagnostics.push(diagnostic(&path, error.to_string()));
            None
        }
    }
}

/// Everything loaded for a working directory.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Resources {
    pub context_files: Vec<ContextFile>,
    pub skills: Vec<Skill>,
    pub prompt_templates: Vec<PromptTemplate>,
    /// `SYSTEM.md`: replaces the default preamble, tool list and rules.
    pub system_prompt: Option<ContextFile>,
    /// `APPEND_SYSTEM.md`: added after the rest of the prompt's text.
    pub append_system_prompt: Option<ContextFile>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Resources {
    /// The resources for `cwd`. A project that is not trusted contributes only its
    /// context files.
    pub fn load(
        cwd: &Path,
        paths: &ResourcePaths,
        skill_paths: &[PathBuf],
        prompt_paths: &[PathBuf],
        project_trusted: bool,
    ) -> Self {
        let (skills, mut diagnostics) = load_skills(cwd, paths, skill_paths, project_trusted);
        let (prompt_templates, template_diagnostics) =
            load_prompt_templates(cwd, paths, prompt_paths, project_trusted);
        diagnostics.extend(template_diagnostics);
        let system_prompt =
            load_prompt_file(cwd, paths, "SYSTEM.md", project_trusted, &mut diagnostics);
        let append_system_prompt = load_prompt_file(
            cwd,
            paths,
            "APPEND_SYSTEM.md",
            project_trusted,
            &mut diagnostics,
        );
        Self {
            context_files: load_context_files(cwd, &paths.agent_dir),
            skills,
            prompt_templates,
            system_prompt,
            append_system_prompt,
            diagnostics,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn layout() -> (tempfile::TempDir, PathBuf, ResourcePaths) {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("repo/app");
        fs::create_dir_all(&cwd).unwrap();
        let paths = ResourcePaths {
            agent_dir: root.path().join("home/agent"),
            project_dir_name: ".maple".into(),
            home_dir: Some(root.path().join("home")),
        };
        (root, cwd, paths)
    }

    #[test]
    fn frontmatter_is_split_from_the_body() {
        let (meta, body) = parse_frontmatter("---\nname: a\ndescription: b\n---\nBody\n").unwrap();
        assert_eq!(meta["name"], "a");
        assert_eq!(body, "Body\n");
        let (meta, body) = parse_frontmatter("No frontmatter").unwrap();
        assert!(meta.as_object().unwrap().is_empty());
        assert_eq!(body, "No frontmatter");
        let (meta, body) = parse_frontmatter("---\n---\nBody").unwrap();
        assert!(meta.as_object().unwrap().is_empty());
        assert_eq!(body, "Body");
        assert!(parse_frontmatter("---\nname: a\n").is_err());
    }

    #[test]
    fn context_files_load_from_the_user_dir_then_root_to_cwd() {
        let (root, cwd, paths) = layout();
        write(&paths.agent_dir.join("AGENTS.md"), "global");
        write(&root.path().join("repo/AGENTS.md"), "repo");
        write(&cwd.join("CLAUDE.md"), "app");
        let files: Vec<String> = load_context_files(&cwd, &paths.agent_dir)
            .into_iter()
            .map(|file| file.content)
            .collect();
        assert_eq!(files, ["global", "repo", "app"]);
    }

    #[test]
    fn skills_are_found_validated_and_deduplicated() {
        let (_root, cwd, paths) = layout();
        write(
            &paths.agent_dir.join("skills/review/SKILL.md"),
            "---\ndescription: Review code\n---\nSteps",
        );
        write(
            &paths.project_dir(&cwd).join("skills/review/SKILL.md"),
            "---\ndescription: Project review\n---\n",
        );
        write(
            &paths
                .project_dir(&cwd)
                .join("skills/deploy/nested/SKILL.md"),
            "---\nname: Deploy_It\ndescription: Deploy\ndisable-model-invocation: true\n---\n",
        );
        write(
            &paths.project_dir(&cwd).join("skills/broken/SKILL.md"),
            "---\nname: broken\n---\n",
        );
        write(
            &paths.project_dir(&cwd).join("skills/notes.md"),
            "just notes",
        );
        write(
            &paths.project_dir(&cwd).join("skills/commit.md"),
            "---\ndescription: Write a commit message\n---\n",
        );

        let (skills, diagnostics) = load_skills(&cwd, &paths, &[], true);
        let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, ["commit", "Deploy_It", "review"]);
        // The project's skill overrides the user's.
        assert_eq!(skills[2].description, "Project review");
        assert!(diagnostics.iter().any(|d| d.message.contains("collides")));
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("invalid characters"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message == "description is required")
        );

        let prompt = format_skills_for_prompt(&skills, "Use the read tool to load a skill's file");
        assert!(prompt.contains("<name>review</name>"));
        assert!(!prompt.contains("Deploy_It"));
    }

    #[test]
    fn a_skill_command_inlines_the_instructions() {
        let (_root, cwd, paths) = layout();
        write(
            &paths.project_dir(&cwd).join("skills/review/SKILL.md"),
            "---\ndescription: Review\n---\nCheck the diff.\n",
        );
        let (skills, _) = load_skills(&cwd, &paths, &[], true);
        let expanded = expand_skill_command("/skill:review src/lib.rs", &skills);
        assert!(expanded.starts_with("<skill name=\"review\""));
        assert!(expanded.contains("Check the diff.\n</skill>\n\nsrc/lib.rs"));
        assert_eq!(
            expand_skill_command("/skill:missing", &skills),
            "/skill:missing"
        );
    }

    #[test]
    fn templates_substitute_positional_and_sliced_arguments() {
        let args = parse_command_args(r#"one "two words" three"#);
        assert_eq!(args, ["one", "two words", "three"]);
        assert_eq!(
            substitute_args("$1|$2|$9|$@", &args),
            "one|two words||one two words three"
        );
        assert_eq!(
            substitute_args("${4:-none} ${@:2} ${@:2:1}", &args),
            "none two words three two words"
        );
        assert_eq!(substitute_args("${ARGUMENTS:-empty}", &[]), "empty");
        assert_eq!(substitute_args("$1", &["$2".into(), "x".into()]), "$2");
        assert_eq!(substitute_args("${@:99999999999999999999}", &args), "");
    }

    #[test]
    fn templates_load_and_expand_by_name() {
        let (_root, cwd, paths) = layout();
        write(
            &paths.project_dir(&cwd).join("prompts/fix.md"),
            "---\nargument-hint: <issue>\n---\nFix issue $1 carefully.",
        );
        write(
            &paths.agent_dir.join("prompts/explain.md"),
            "Explain $@ in simple terms",
        );
        write(
            &paths.project_dir(&cwd).join("prompts/explain.md"),
            "Explain $@ in detail",
        );
        let (templates, diagnostics) = load_prompt_templates(&cwd, &paths, &[], true);
        let names: Vec<&str> = templates
            .iter()
            .map(|template| template.name.as_str())
            .collect();
        assert_eq!(names, ["explain", "fix"]);
        // The project's template overrides the user's.
        assert_eq!(templates[0].description, "Explain $@ in detail");
        assert!(diagnostics.iter().any(|d| d.message.contains("collides")));
        assert_eq!(templates[1].argument_hint.as_deref(), Some("<issue>"));
        assert_eq!(
            expand_prompt_template("/fix 42", &templates),
            "Fix issue 42 carefully."
        );
        assert_eq!(
            expand_prompt_template("/unknown x", &templates),
            "/unknown x"
        );
    }

    #[test]
    fn agents_skills_load_from_the_project_up_to_its_repository_and_from_home() {
        let (root, cwd, paths) = layout();
        fs::create_dir_all(root.path().join("repo/.git")).unwrap();
        let skill = |description: &str| format!("---\ndescription: {description}\n---\n");
        let agents = cwd.join(".agents/skills");
        write(&agents.join("near/SKILL.md"), &skill("Near"));
        // Loose files count below the top level of .agents/skills, not at it.
        write(&agents.join("group/nested.md"), &skill("Nested"));
        write(&agents.join("loose.md"), &skill("Loose"));
        write(
            &root.path().join("repo/.agents/skills/far/SKILL.md"),
            &skill("Far"),
        );
        // Above the repository: not this project's.
        write(
            &root.path().join(".agents/skills/outside/SKILL.md"),
            &skill("Outside"),
        );
        write(
            &root.path().join("home/.agents/skills/mine/SKILL.md"),
            &skill("Mine"),
        );

        let (skills, _) = load_skills(&cwd, &paths, &[], true);
        let found: Vec<(&str, ResourceSource)> = skills
            .iter()
            .map(|skill| (skill.name.as_str(), skill.source))
            .collect();
        assert_eq!(
            found,
            [
                ("nested", ResourceSource::Project),
                ("near", ResourceSource::Project),
                ("far", ResourceSource::Project),
                ("mine", ResourceSource::User),
            ]
        );
        let (skills, _) = load_skills(&cwd, &paths, &[], false);
        let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, ["mine"]);
    }

    #[test]
    fn an_untrusted_project_keeps_only_its_context_files() {
        let (_root, cwd, paths) = layout();
        let project = paths.project_dir(&cwd);
        write(
            &project.join("skills/x/SKILL.md"),
            "---\ndescription: X\n---\n",
        );
        write(&project.join("prompts/p.md"), "Do it");
        write(&project.join("SYSTEM.md"), "\u{feff}Project system");
        write(&cwd.join("AGENTS.md"), "Context");
        write(&paths.agent_dir.join("SYSTEM.md"), "User system");
        write(&paths.agent_dir.join("APPEND_SYSTEM.md"), "User append");

        let trusted = Resources::load(&cwd, &paths, &[], &[], true);
        let text = |file: &Option<ContextFile>| file.as_ref().map(|file| file.content.clone());
        assert_eq!(
            text(&trusted.system_prompt).as_deref(),
            Some("Project system")
        );
        assert_eq!(
            text(&trusted.append_system_prompt).as_deref(),
            Some("User append")
        );
        assert_eq!(trusted.skills.len(), 1);
        assert_eq!(trusted.prompt_templates.len(), 1);

        let untrusted = Resources::load(&cwd, &paths, &[], &[], false);
        assert_eq!(
            text(&untrusted.system_prompt).as_deref(),
            Some("User system")
        );
        assert!(untrusted.skills.is_empty());
        assert!(untrusted.prompt_templates.is_empty());
        assert_eq!(untrusted.context_files.len(), 1);
        assert_eq!(untrusted.context_files[0].content, "Context");
    }
}
