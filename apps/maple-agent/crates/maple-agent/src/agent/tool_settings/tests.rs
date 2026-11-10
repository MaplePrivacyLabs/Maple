use pi_ai::faux::faux_tool_call;
use serde_json::json;

use super::*;
use crate::agent::tests::{Harness, eventually, finished};
use crate::agent::{AgentRunTerminal, AgentServiceEvent};

fn paths(root: &std::path::Path) -> AgentPathLayout {
    AgentPathLayout::from_app_roots(root.join("config"), root.join("local-data")).with_store(
        std::sync::Arc::new(
            crate::store::Store::open(crate::store::StorePaths::new(root.join("local-data")))
                .unwrap(),
        ),
    )
}

fn request(id: &str, enabled: bool) -> AgentSetToolEnabledRequest {
    AgentSetToolEnabledRequest {
        id: id.to_string(),
        enabled,
    }
}

fn saved(paths: &AgentPathLayout) -> Option<Value> {
    let (store, account) = account_store(paths, "user").unwrap();
    store.account_setting(account, TOOLS_SETTING).unwrap()
}

fn on(switches: &[AgentToolSwitch]) -> Vec<&str> {
    switches
        .iter()
        .filter(|switch| switch.enabled)
        .map(|switch| switch.id.as_str())
        .collect()
}

#[test]
fn the_defaults_leave_python_and_the_search_tools_off() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = paths(temporary.path());
    let listed = project_tools(&tool_switches(&paths, "user"), true);
    let ids: Vec<&str> = listed.iter().map(|switch| switch.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "read",
            "shell",
            "edit",
            "write",
            "grep",
            "find",
            "ls",
            "read_image",
            "todo_write",
            "request_user_input",
            "web",
            "python"
        ]
    );
    assert_eq!(
        on(&listed),
        [
            "read",
            "shell",
            "edit",
            "write",
            "read_image",
            "todo_write",
            "request_user_input",
            "web"
        ]
    );
    assert!(
        listed
            .iter()
            .all(|switch| switch.enabled == switch.enabled_by_default && switch.available)
    );
    assert_eq!(saved(&paths), None, "reading saves nothing");

    // Without the bundled CPython, `python` is listed but not available.
    let listed = project_tools(&ToolSwitches::default(), false);
    assert!(
        listed
            .iter()
            .all(|switch| switch.available == (switch.id != "python"))
    );
}

#[test]
fn only_changes_from_the_defaults_are_saved_and_read_back() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = paths(temporary.path());
    let mut switches = tool_switches(&paths, "user");
    switches.set(ToolSwitch::Python, true);
    switches.set(ToolSwitch::Grep, true);
    switches.set(ToolSwitch::Shell, false);
    switches.set(ToolSwitch::Web, false);
    // Already the default.
    switches.set(ToolSwitch::Read, true);
    save_stored_tools(&paths, "user", &switches).unwrap();
    assert_eq!(
        saved(&paths),
        Some(json!({
            "version": 1,
            "tools": {"grep": true, "python": true, "shell": false, "web": false}
        }))
    );
    let read = tool_switches(&paths, "user");
    assert_eq!(read, switches);
    assert_eq!(
        on(&project_tools(&read, true)),
        [
            "read",
            "edit",
            "write",
            "grep",
            "read_image",
            "todo_write",
            "request_user_input",
            "python"
        ]
    );

    // A switch set back to its default leaves the saved value.
    switches.set(ToolSwitch::Python, false);
    switches.set(ToolSwitch::Shell, true);
    switches.set(ToolSwitch::Web, true);
    save_stored_tools(&paths, "user", &switches).unwrap();
    assert_eq!(
        saved(&paths),
        Some(json!({"version": 1, "tools": {"grep": true}}))
    );
}

#[test]
fn unknown_names_are_kept_and_unreadable_values_read_as_the_defaults() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = paths(temporary.path());
    let (store, account) = account_store(&paths, "user").unwrap();
    let save = |value: &Value| {
        store
            .set_account_setting(account, TOOLS_SETTING, value, 1)
            .unwrap()
    };

    // A name this build does not know is ignored, and kept for the build
    // that saved it when another switch changes.
    save(&json!({"version": 1, "tools": {"python": true, "telepathy": true}}));
    let mut switches = tool_switches(&paths, "user");
    assert!(switches.enabled(ToolSwitch::Python));
    assert_eq!(project_tools(&switches, true).len(), ToolSwitch::ALL.len());
    switches.set(ToolSwitch::Ls, true);
    save_stored_tools(&paths, "user", &switches).unwrap();
    assert_eq!(
        saved(&paths),
        Some(json!({"version": 1, "tools": {"ls": true, "python": true, "telepathy": true}}))
    );

    for value in [
        json!({"version": 2, "tools": {"python": true}}),
        json!({"tools": {"python": true}}),
        json!({"version": "1", "tools": {"python": true}}),
        json!({"version": 1, "tools": {"python": "yes"}}),
        json!({"version": 1, "tools": ["python"]}),
        json!("python"),
    ] {
        save(&value);
        assert!(load_stored_tools(&paths, "user").is_err(), "{value}");
        assert_eq!(
            tool_switches(&paths, "user"),
            ToolSwitches::default(),
            "{value}"
        );
        // The value is left alone until a switch changes.
        assert_eq!(saved(&paths), Some(value));
    }
}

#[tokio::test]
async fn the_host_checks_what_it_is_asked_to_switch() {
    let harness = Harness::new().await;
    let handle = &harness.handle;
    assert_eq!(
        handle
            .set_tool_enabled(request("telepathy", true))
            .await
            .unwrap_err(),
        "Unknown tool 'telepathy'"
    );

    // `python` can be switched on only where the build bundles CPython, and
    // off anywhere.
    let bundled = tools::PythonWorkers::is_bundled();
    let python = |switches: &[AgentToolSwitch]| {
        switches
            .iter()
            .find(|switch| switch.id == "python")
            .cloned()
            .unwrap()
    };
    assert_eq!(
        python(&handle.list_tools().await.unwrap()).available,
        bundled
    );
    match handle.set_tool_enabled(request(" python ", true)).await {
        Ok(switches) => {
            assert!(bundled);
            assert!(python(&switches).enabled);
        }
        Err(error) => {
            assert!(!bundled);
            assert_eq!(error, "The python tool is not available on this computer");
        }
    }
    let switches = handle
        .set_tool_enabled(request("python", false))
        .await
        .unwrap();
    assert!(!python(&switches).enabled);
    assert_eq!(handle.list_tools().await.unwrap(), switches);
}

/// What a request declared to the model.
fn declared(request: &pi_ai::faux::FauxRequest) -> Vec<String> {
    pi_ai::transcript::current_tools(&request.context.messages)
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

fn has(tools: &[String], name: &str) -> bool {
    tools.iter().any(|tool| tool == name)
}

/// The question `task` waits on, once it asks.
fn question(harness: &Harness, task: &str) -> Option<String> {
    harness
        .recorder
        .events()
        .into_iter()
        .find_map(|event| match event {
            AgentServiceEvent::Question {
                session_id,
                request_id,
                ..
            } if session_id == task => Some(request_id),
            _ => None,
        })
}

/// Have the model ask a question, wait until `task` asks it, run `meanwhile`,
/// then answer, and let the run finish.
async fn change_while_asking(
    harness: &Harness,
    task: &str,
    meanwhile: impl std::future::Future<Output = ()>,
) {
    harness.faux.push_message(vec![faux_tool_call(
        "request_user_input",
        json!({"question": "Go on?", "options": [{"label": "Yes"}]}),
    )]);
    harness.faux.push_text("Done.");
    let mut run = harness.send(task, "Ask me first").await;
    eventually(|| question(harness, task).is_some()).await;
    meanwhile.await;
    let answer = r#"{"answers":{"question_0":{"answers":["Yes"]}}}"#;
    assert!(
        harness
            .service
            .answer_question(&question(harness, task).unwrap(), answer.to_string())
            .await
    );
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
}

#[tokio::test]
async fn new_and_reopened_tasks_are_built_with_the_switches() {
    let harness = Harness::new().await;
    let handle = &harness.handle;
    let runtime = harness.runtime().await;
    for (id, enabled) in [("grep", true), ("shell", false), ("read_image", false)] {
        handle.set_tool_enabled(request(id, enabled)).await.unwrap();
    }
    let task = harness.create_task().await;
    harness.faux.push_text("Done.");
    let mut run = harness.send(&task, "Look around").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let first = declared(&harness.faux.requests()[0]);
    for name in ["read", "edit", "write", "grep", "todo_write"] {
        assert!(has(&first, name), "{first:?}");
    }
    for name in ["bash", "powershell", "read_image", "find", "ls", "python"] {
        assert!(!has(&first, name), "{first:?}");
    }

    // Reopened, the task follows the switches as they are now, not the tools
    // its transcript declared.
    runtime.unload_task(&task).await.unwrap();
    handle
        .set_tool_enabled(request("grep", false))
        .await
        .unwrap();
    handle
        .set_tool_enabled(request("shell", true))
        .await
        .unwrap();
    harness.faux.push_text("Done again.");
    let mut run = harness.send(&task, "Look again").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let reopened = declared(harness.faux.requests().last().unwrap());
    assert!(!has(&reopened, "grep"), "{reopened:?}");
    assert!(
        has(&reopened, "bash") || has(&reopened, "powershell"),
        "{reopened:?}"
    );
    assert!(!has(&reopened, "read_image"), "{reopened:?}");
}

#[tokio::test]
async fn a_change_reaches_loaded_tasks_from_their_next_request() {
    let harness = Harness::new().await;
    let handle = &harness.handle;
    let runtime = harness.runtime().await;
    let task = harness.create_task().await;
    let row = runtime.store.get(&task).unwrap().unwrap();
    let session = runtime
        .task_session(
            &row,
            runtime.pi_model(&runtime.model, None, Some(false)).await,
        )
        .await
        .unwrap();
    let before = session.active_tools();
    assert!(has(&before, "read_image") && !has(&before, "grep"));

    // Only the switched tool changes in the loaded session.
    handle
        .set_tool_enabled(request("grep", true))
        .await
        .unwrap();
    handle
        .set_tool_enabled(request("read_image", false))
        .await
        .unwrap();
    let after = session.active_tools();
    let expected: Vec<String> = before
        .iter()
        .filter(|tool| *tool != "read_image")
        .cloned()
        .chain(std::iter::once("grep".to_string()))
        .collect();
    assert_eq!(after, expected);
    // A tool the session never registered is left out.
    tools::set_tools_active(&session, &["telepathy"], true);
    assert_eq!(session.active_tools(), after);

    // During a run, the request after the change declares it.
    change_while_asking(&harness, &task, async {
        handle
            .set_tool_enabled(request("find", true))
            .await
            .unwrap();
    })
    .await;
    let requests = harness.faux.requests();
    let asked = declared(&requests[requests.len() - 2]);
    let answered = declared(&requests[requests.len() - 1]);
    assert!(
        has(&asked, "grep") && !has(&asked, "read_image"),
        "{asked:?}"
    );
    assert!(!has(&asked, "find"), "{asked:?}");
    assert!(has(&answered, "find"), "{answered:?}");
}

/// The web switch moves `web_search` and `open_url` together. In a loaded
/// task, the request after the change declares neither, even during a run,
/// and the next request after it is back on declares both.
#[tokio::test]
async fn the_web_switch_moves_both_web_tools_from_the_next_request() {
    let harness = Harness::new().await;
    let handle = &harness.handle;
    let task = harness.create_task().await;
    change_while_asking(&harness, &task, async {
        handle
            .set_tool_enabled(request("web", false))
            .await
            .unwrap();
    })
    .await;
    let requests = harness.faux.requests();
    let asked = declared(&requests[requests.len() - 2]);
    let answered = declared(&requests[requests.len() - 1]);
    for name in WEB_TOOL_NAMES {
        assert!(has(&asked, name), "{asked:?}");
        assert!(!has(&answered, name), "{answered:?}");
    }

    handle.set_tool_enabled(request("web", true)).await.unwrap();
    harness.faux.push_text("Found it.");
    let mut run = harness.send(&task, "Look it up").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let again = declared(harness.faux.requests().last().unwrap());
    for name in WEB_TOOL_NAMES {
        assert!(has(&again, name), "{again:?}");
    }
}
