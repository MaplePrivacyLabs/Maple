//! The skills that teach a task how to delegate: `/handoff`, `/committee`
//! and `/advisor`. They live in the account's skills folder, which Pi reads
//! for every task, while an external agent is switched on in Settings.

use std::fs;
use std::path::PathBuf;

use crate::agent::AgentPathLayout;
use crate::agent::config::{account_config_dir_path, set_owner_only_dir_permissions};

/// Frontmatter line that marks a skill file as Maple's, so switching the
/// agents off removes only what switching them on wrote.
const MARKER: &str = "maple: external-agents";

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
    Ok(account_config_dir_path(paths, user_id)
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
        let dir = root.join(name);
        let file = dir.join("SKILL.md");
        if enabled {
            let current = fs::read_to_string(&file).ok();
            if current.as_deref() == Some(content) {
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
            set_owner_only_dir_permissions(&dir);
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
        AgentPathLayout::from_app_roots(root.join("config"), root.join("data"))
            .with_home(Some(root.join("home")))
    }

    #[test]
    fn skills_follow_the_switch_and_leave_the_users_own_alone() {
        let temp = tempfile::tempdir().unwrap();
        let paths = layout(temp.path());
        let root = skills_dir(&paths, "user").unwrap();
        let own = root.join("advisor").join("SKILL.md");
        fs::create_dir_all(own.parent().unwrap()).unwrap();
        fs::write(&own, "---\nname: advisor\n---\nmine").unwrap();

        sync(&paths, "user", true).unwrap();
        let handoff = fs::read_to_string(root.join("handoff").join("SKILL.md")).unwrap();
        assert!(handoff.contains(MARKER));
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
