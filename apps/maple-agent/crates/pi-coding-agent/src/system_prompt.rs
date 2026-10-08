//! The system prompt as named sections.
//!
//! Each section other than the untagged preamble is wrapped in a tag of its name. The
//! transcript's system messages carry the sections, so a change to one section (a new
//! skill, another working directory) is sent as a patch of that section alone.

use indexmap::IndexMap;

use crate::resources::{ContextFile, Skill, format_skills_for_prompt};

/// A tool's contribution to the prompt.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolPromptInfo {
    pub name: String,
    /// A one-line description for the tool list; tools without one are not listed.
    pub snippet: Option<String>,
    /// Rules that apply while the tool is active.
    pub guidelines: Vec<String>,
}

/// What the system prompt is built from. Extensions can change it before each prompt.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SystemPromptOptions {
    /// The product the assistant runs in, named in the default preamble.
    pub app_name: String,
    /// Replaces the default preamble, tool list and rules.
    pub custom_prompt: Option<String>,
    /// Replaces the whole prompt, sections included.
    pub force_prompt: Option<String>,
    /// The active tools, in order.
    pub tools: Vec<ToolPromptInfo>,
    /// Extra rules after the tools' own.
    pub guidelines: Vec<String>,
    /// User text placed before the project context.
    pub append: String,
    /// Extra sections by tag name, after the built-in ones.
    pub sections: IndexMap<String, String>,
    pub cwd: String,
    pub context_files: Vec<ContextFile>,
    pub skills: Vec<Skill>,
    /// How the model reads a skill file. Without a way to read files, skills are left out.
    pub skill_load_hint: Option<String>,
}

fn valid_section_name(name: &str) -> bool {
    let mut chars = name.chars();
    name != "preamble"
        && chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn rules(options: &SystemPromptOptions) -> String {
    let mut rules: Vec<&str> = Vec::new();
    let all = options
        .tools
        .iter()
        .flat_map(|tool| tool.guidelines.iter())
        .chain(&options.guidelines)
        .map(|rule| rule.trim())
        .chain([
            "Be concise in your responses",
            "Show file paths clearly when working with files",
        ]);
    for rule in all {
        if !rule.is_empty() && !rules.contains(&rule) {
            rules.push(rule);
        }
    }
    rules
        .iter()
        .map(|rule| format!("- {rule}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The ordered sections of the prompt.
pub fn build_sections(options: &SystemPromptOptions) -> Result<IndexMap<String, String>, String> {
    if let Some(name) = options
        .sections
        .keys()
        .find(|name| !valid_section_name(name))
    {
        return Err(format!("Invalid system prompt section name: {name}"));
    }
    let mut raw: IndexMap<String, String> = IndexMap::new();
    match &options.custom_prompt {
        Some(custom) => {
            raw.insert("preamble".into(), custom.clone());
        }
        None => {
            raw.insert(
                "preamble".into(),
                format!(
                    "You are an expert assistant operating inside {}. You help users by reading files, running commands, editing code, and using the tools available to you.",
                    options.app_name
                ),
            );
            let listed: Vec<String> = options
                .tools
                .iter()
                .filter_map(|tool| {
                    tool.snippet
                        .as_ref()
                        .map(|snippet| format!("- {}: {snippet}", tool.name))
                })
                .collect();
            let tools = if listed.is_empty() {
                "(none)".to_string()
            } else {
                listed.join("\n")
            };
            raw.insert(
                "tools".into(),
                format!("{tools}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project."),
            );
            raw.insert("rules".into(), rules(options));
        }
    }
    if !options.append.is_empty() {
        raw.insert("addendum".into(), options.append.clone());
    }
    if !options.context_files.is_empty() {
        let files: Vec<String> = options
            .context_files
            .iter()
            .map(|file| {
                format!(
                    "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                    file.path.display(),
                    file.content
                )
            })
            .collect();
        raw.insert(
            "project_context".into(),
            format!(
                "Project-specific instructions and guidelines:\n\n{}",
                files.join("\n\n")
            ),
        );
    }
    if let Some(hint) = &options.skill_load_hint {
        let skills = format_skills_for_prompt(&options.skills, hint);
        if !skills.is_empty() {
            raw.insert("skills".into(), skills);
        }
    }
    raw.insert("cwd".into(), options.cwd.replace('\\', "/"));
    for (name, content) in &options.sections {
        if !content.is_empty() {
            raw.insert(name.clone(), content.clone());
        }
    }
    Ok(raw
        .into_iter()
        .map(|(name, content)| {
            let text = if name == "preamble" {
                content
            } else {
                format!("<{name}>\n{content}\n</{name}>")
            };
            (name, text)
        })
        .collect())
}

/// The content and sections of a leading system message for `options`. A forced prompt
/// is opaque content without sections.
pub fn build_prompt_state(
    options: &SystemPromptOptions,
) -> Result<(String, IndexMap<String, String>), String> {
    match &options.force_prompt {
        Some(prompt) => Ok((prompt.clone(), IndexMap::new())),
        None => Ok((String::new(), build_sections(options)?)),
    }
}

/// The prompt as one text.
pub fn render(options: &SystemPromptOptions) -> Result<String, String> {
    let (content, sections) = build_prompt_state(options)?;
    let mut parts: Vec<&str> = Vec::new();
    if !content.is_empty() {
        parts.push(&content);
    }
    parts.extend(sections.values().map(String::as_str));
    Ok(parts.join("\n\n"))
}

/// The patch that turns the sections the model has into `current`; `None` when equal.
pub fn diff_sections(
    previous: &IndexMap<String, Option<String>>,
    current: &IndexMap<String, String>,
) -> Option<IndexMap<String, Option<String>>> {
    let mut patch = IndexMap::new();
    for (name, text) in current {
        if previous.get(name).and_then(Option::as_ref) != Some(text) {
            patch.insert(name.clone(), Some(text.clone()));
        }
    }
    for name in previous.keys() {
        if !current.contains_key(name) {
            patch.insert(name.clone(), None);
        }
    }
    (!patch.is_empty()).then_some(patch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn options() -> SystemPromptOptions {
        SystemPromptOptions {
            app_name: "Maple".into(),
            tools: vec![
                ToolPromptInfo {
                    name: "read".into(),
                    snippet: Some("Read files".into()),
                    guidelines: vec![
                        "Read before editing".into(),
                        "Be concise in your responses".into(),
                    ],
                },
                ToolPromptInfo {
                    name: "secret".into(),
                    snippet: None,
                    guidelines: Vec::new(),
                },
            ],
            cwd: "C:\\work".into(),
            context_files: vec![ContextFile {
                path: PathBuf::from("/work/AGENTS.md"),
                content: "Use tabs.".into(),
            }],
            ..SystemPromptOptions::default()
        }
    }

    #[test]
    fn the_default_prompt_names_the_host_and_lists_tools_and_rules() {
        let sections = build_sections(&options()).unwrap();
        let names: Vec<&str> = sections.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            ["preamble", "tools", "rules", "project_context", "cwd"]
        );
        assert!(sections["preamble"].contains("inside Maple"));
        assert!(sections["tools"].starts_with("<tools>\n- read: Read files\n\nIn addition"));
        assert_eq!(
            sections["rules"],
            "<rules>\n- Read before editing\n- Be concise in your responses\n- Show file paths clearly when working with files\n</rules>"
        );
        assert!(
            sections["project_context"]
                .contains("<project_instructions path=\"/work/AGENTS.md\">\nUse tabs.")
        );
        assert_eq!(sections["cwd"], "<cwd>\nC:/work\n</cwd>");
        let text = render(&options()).unwrap();
        assert!(!text.to_lowercase().contains(" pi"));
    }

    #[test]
    fn custom_and_forced_prompts_replace_the_defaults() {
        let mut custom = options();
        custom.custom_prompt = Some("You review code.".into());
        custom.sections.insert("team".into(), "Team rules".into());
        let sections = build_sections(&custom).unwrap();
        assert_eq!(sections["preamble"], "You review code.");
        assert!(!sections.contains_key("tools"));
        assert_eq!(sections["team"], "<team>\nTeam rules\n</team>");

        custom.force_prompt = Some("Exactly this.".into());
        assert_eq!(render(&custom).unwrap(), "Exactly this.");

        custom.sections.insert("Bad Name".into(), "x".into());
        assert!(build_sections(&custom).is_err());
    }

    #[test]
    fn diffs_patch_changed_and_removed_sections() {
        let mut previous: IndexMap<String, Option<String>> = IndexMap::new();
        previous.insert("preamble".into(), Some("a".into()));
        previous.insert("cwd".into(), Some("<cwd>\n/one\n</cwd>".into()));
        previous.insert("skills".into(), Some("<skills/>".into()));
        let mut current: IndexMap<String, String> = IndexMap::new();
        current.insert("preamble".into(), "a".into());
        current.insert("cwd".into(), "<cwd>\n/two\n</cwd>".into());
        let patch = diff_sections(&previous, &current).unwrap();
        assert_eq!(patch.len(), 2);
        assert_eq!(patch["cwd"].as_deref(), Some("<cwd>\n/two\n</cwd>"));
        assert_eq!(patch["skills"], None);
        current.insert("skills".into(), "<skills/>".into());
        current.shift_remove("cwd");
        current.insert("cwd".into(), "<cwd>\n/one\n</cwd>".into());
        assert_eq!(diff_sections(&previous, &current), None);
    }
}
