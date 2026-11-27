//! The skills that teach a task how to delegate: `/handoff`, `/committee`
//! and `/advisor`. They live in the account's skills folder, which Pi reads
//! for every task, while an external agent is switched on in Settings.

use std::fs;
use std::path::PathBuf;

use crate::agent::AgentPathLayout;
use crate::agent::config::account_generated_resource_dir;

/// Frontmatter line that marks a skill file as Maple's, so switching the
/// agents off removes only what switching them on wrote.
const MARKER: &str = "maple: external-agents";
/// Where a skill names the agents Maple can start.
const AGENTS: &str = "{agents}";

const SKILLS: [(&str, &str); 3] = [
    (
        "handoff",
        include_str!("../../../resources/skills/handoff/SKILL.md"),
    ),
    (
        "committee",
        include_str!("../../../resources/skills/committee/SKILL.md"),
    ),
    (
        "advisor",
        include_str!("../../../resources/skills/advisor/SKILL.md"),
    ),
];

/// The account's skills folder, Pi's `~/.pi/agent/skills`.
fn skills_dir(paths: &AgentPathLayout, user_id: &str) -> Result<PathBuf, String> {
    Ok(account_generated_resource_dir(paths, user_id)
        .map_err(|error| error.to_string())?
        .join("skills"))
}

/// Install or remove the delegation skills so they match `enabled`. A file
/// without Maple's marker is the user's and is never changed.
pub(in crate::agent) fn sync(
    paths: &AgentPathLayout,
    user_id: &str,
    enabled: bool,
) -> Result<(), String> {
    let root = skills_dir(paths, user_id)?;
    for (name, content) in SKILLS {
        debug_assert!(content.contains(MARKER));
        let content = content.replace(AGENTS, &super::catalog::example_names());
        let dir = root.join(name);
        let file = dir.join("SKILL.md");
        // This namespace is new and app-owned. Never follow a planted link
        // from a generated skill to an earlier app directory or user file.
        let scope = crate::maple_api::account_scope(user_id)?;
        let names = ["accounts", &scope, "skills", name];
        let verified = if enabled {
            paths
                .store()?
                .paths()
                .prepare_resource_directory(&names)
                .map_err(|error| error.to_string())?
        } else {
            let Some(directory) = paths
                .store()?
                .paths()
                .existing_resource_directory(&names)
                .map_err(|error| error.to_string())?
            else {
                continue;
            };
            directory
        };
        if verified != dir {
            return Err("Generated skill path is outside the current store".into());
        }
        match fs::symlink_metadata(&file) {
            Ok(metadata) => {
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err("Generated skill must be a regular file".into());
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.nlink() != 1 {
                        return Err("Generated skill must not be a hard link".into());
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        if enabled {
            let current = fs::read_to_string(&file).ok();
            if current.as_deref() == Some(content.as_str()) {
                continue;
            }
            if current.is_some_and(|current| !current.contains(MARKER)) {
                log::warn!(
                    "Leaving the user's own skill in place at {}",
                    file.display()
                );
                continue;
            }
            crate::private_file::write_private_file(&file, content.as_bytes())
                .map_err(|error| format!("Failed to install the {name} skill: {error}"))?;
        } else {
            let Ok(current) = fs::read_to_string(&file) else {
                continue;
            };
            if !current.contains(MARKER) {
                continue;
            }
            fs::remove_file(&file)
                .map_err(|error| format!("Failed to remove the {name} skill: {error}"))?;
            // Only the folder Maple made; a user's extra files keep it.
            let _ = fs::remove_dir(&dir);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(root: &std::path::Path) -> AgentPathLayout {
        let store = std::sync::Arc::new(
            crate::store::Store::open(crate::store::StorePaths::new(root.join("data"))).unwrap(),
        );
        AgentPathLayout::from_resource_root(store.paths().resources())
            .with_store(store)
            .with_home(Some(root.join("home")))
    }

    #[test]
    fn skills_follow_the_switch_and_leave_the_users_own_alone() {
        let temp = tempfile::tempdir().unwrap();
        let paths = layout(temp.path());
        let root = skills_dir(&paths, "user").unwrap();
        let own = root.join("advisor").join("SKILL.md");
        let scope = crate::maple_api::account_scope("user").unwrap();
        paths
            .store()
            .unwrap()
            .paths()
            .prepare_resource_directory(&["accounts", &scope, "skills", "advisor"])
            .unwrap();
        fs::write(&own, "---\nname: advisor\n---\nmine").unwrap();

        sync(&paths, "user", true).unwrap();
        let handoff = fs::read_to_string(root.join("handoff").join("SKILL.md")).unwrap();
        assert!(handoff.contains(MARKER));
        // The skills name the agents of the catalog.
        assert!(!handoff.contains(AGENTS));
        assert!(handoff.contains("Grok Build or Antigravity"), "{handoff}");
        assert!(root.join("committee").join("SKILL.md").is_file());
        assert_eq!(
            fs::read_to_string(&own).unwrap(),
            "---\nname: advisor\n---\nmine"
        );
        // Installing again changes nothing.
        sync(&paths, "user", true).unwrap();

        sync(&paths, "user", false).unwrap();
        assert!(!root.join("handoff").exists());
        assert!(!root.join("committee").exists());
        assert!(own.is_file());
    }
}
