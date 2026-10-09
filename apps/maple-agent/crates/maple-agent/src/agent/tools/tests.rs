use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use super::*;
use crate::agent::AgentToolContextSpec;

fn spawn(env: &[(&str, &str)]) -> BashSpawnContext {
    BashSpawnContext {
        command: "true".to_string(),
        cwd: PathBuf::from("/work"),
        env: env
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
    }
}

#[test]
fn commands_get_the_login_path_the_task_id_and_its_tool_context() {
    let context = SharedAgentToolContext::new(
        AgentToolContextSpec::try_new(
            BTreeMap::from([("FOR_TOOLS".to_string(), "value".to_string())]),
            BTreeSet::from(["BUZZ_AUTH_TAG".to_string()]),
            false,
        )
        .unwrap(),
    );
    let tools = task_tools("task-1", context, Some("/login/bin:/usr/bin".to_string()));
    let hook = tools.options.bash.spawn_hook.unwrap();
    let started = hook(spawn(&[
        ("PATH", "/usr/bin"),
        ("BUZZ_AUTH_TAG", "secret"),
        ("HOME", "/home/me"),
    ]));
    assert_eq!(
        started.env.get("PATH").map(String::as_str),
        Some("/login/bin:/usr/bin")
    );
    assert_eq!(
        started.env.get("AGENT_SESSION_ID").map(String::as_str),
        Some("task-1")
    );
    assert_eq!(
        started.env.get("FOR_TOOLS").map(String::as_str),
        Some("value")
    );
    assert_eq!(
        started.env.get("HOME").map(String::as_str),
        Some("/home/me")
    );
    assert!(!started.env.contains_key("BUZZ_AUTH_TAG"));
}

#[test]
fn a_revoked_context_stops_adding_its_values_but_keeps_scrubbing() {
    let context = SharedAgentToolContext::new(
        AgentToolContextSpec::try_new(
            BTreeMap::from([("TOKEN".to_string(), "secret".to_string())]),
            BTreeSet::from(["TOKEN".to_string()]),
            true,
        )
        .unwrap(),
    );
    let tools = task_tools("task-1", context.clone(), None);
    context.revoke();
    let started = tools.options.bash.spawn_hook.unwrap()(spawn(&[("TOKEN", "inherited")]));
    assert!(!started.env.contains_key("TOKEN"));
}

#[test]
fn the_model_gets_read_a_shell_edit_and_write() {
    let tools = task_tools(
        "task-1",
        SharedAgentToolContext::new(AgentToolContextSpec::default()),
        None,
    );
    let shell = if cfg!(windows) {
        tools.builtin[1].as_str()
    } else {
        "bash"
    };
    assert_eq!(tools.builtin, ["read", shell, "edit", "write"]);
    assert!(tools.maple.is_empty());
    assert!(tools.options.powershell.spawn_hook.is_some());
}
