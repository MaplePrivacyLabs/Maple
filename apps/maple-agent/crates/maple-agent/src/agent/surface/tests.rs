//! A calling surface holding tasks: its leases, its runs and its
//! provisional tasks, on the scripted runtime.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use pi_ai::faux::{FauxProvider, faux_tool_call};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::agent::tests::{Harness, finished};
use crate::agent::{
    AgentMcpKeyValue, AgentMcpTransport, AgentRunTerminal, AgentSendMessageRequest,
    AgentServiceEvent, SENSITIVE_BRIDGE_ENV,
};

/// The context an ACP bridge gives its tasks' tools.
fn bridge_context() -> AgentToolContextSpec {
    AgentToolContextSpec::try_new(
        BTreeMap::from([("BUZZ_RELAY_URL".to_string(), "ws://relay".to_string())]),
        SENSITIVE_BRIDGE_ENV
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>(),
        true,
    )
    .unwrap()
}

fn new_task(harness: &Harness, system_prompt: Option<&str>) -> AgentCreateSessionRequest {
    AgentCreateSessionRequest {
        project_root: Some(harness.project.path().to_string_lossy().into_owned()),
        title: None,
        model: Some("glm-5-3".to_string()),
        context_limit: None,
        mcp_server_names: None,
        system_prompt: system_prompt.map(str::to_string),
    }
}

fn prompt(session_id: &str, text: &str) -> AgentSendMessageRequest {
    AgentSendMessageRequest {
        session_id: session_id.to_string(),
        text: text.to_string(),
        model: Some("glm-5-3".to_string()),
        context_limit: None,
        vision_capable: false,
        steer: false,
        queue_id: None,
        attachments: Vec::new(),
    }
}

async fn listed_ids(harness: &Harness) -> Vec<String> {
    harness
        .handle
        .list_sessions(None)
        .await
        .unwrap()
        .into_iter()
        .map(|session| session.id)
        .collect()
}

#[tokio::test]
async fn a_surface_task_is_unlisted_until_prompted_and_discarded_untouched() {
    let harness = Harness::new().await;
    let created = harness
        .handle
        .create_surface_session(new_task(&harness, None), bridge_context(), Vec::new())
        .await
        .unwrap();
    let untouched = created.detail.session.id.clone();
    assert!(created.detail.session.acp);
    assert_eq!(created.detail.session.title, "Maple ACP");
    assert!(listed_ids(&harness).await.is_empty());
    created.lease.discard_created_if_untouched().await;
    let store = harness.handle.store().unwrap();
    assert!(store.get(&untouched).unwrap().is_none());

    // Prompted, a task stays, under its prompt's title.
    let created = harness
        .handle
        .create_surface_session(new_task(&harness, None), bridge_context(), Vec::new())
        .await
        .unwrap();
    let prompted = created.detail.session.id.clone();
    harness.faux.push_text("Hello from Maple.");
    let mut run = harness
        .handle
        .send_surface_message(
            &created.lease.access(),
            prompt(&prompted, "Say hello"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    assert_eq!(listed_ids(&harness).await, std::slice::from_ref(&prompted));
    created.lease.discard_created_if_untouched().await;
    let row = store.get(&prompted).unwrap().unwrap();
    assert_eq!(row.title, "Say hello");
    assert_eq!(row.message_count, 2);
}

#[tokio::test]
async fn a_surface_task_runs_with_the_surface_context_and_without_desktop_tools() {
    let harness = Harness::new().await;
    let created = harness
        .handle
        .create_surface_session(
            new_task(&harness, Some("You are Buzz's Maple persona.")),
            bridge_context(),
            Vec::new(),
        )
        .await
        .unwrap();
    let task = created.detail.session.id.clone();
    harness.faux.push_message(vec![faux_tool_call(
        "bash",
        json!({"command": "printenv BUZZ_RELAY_URL"}),
    )]);
    harness.faux.push_text("Done.");
    let mut run = harness
        .handle
        .send_surface_message(
            &created.lease.access(),
            prompt(&task, "Where is the relay?"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);

    let requests = harness.faux.requests();
    let declared: Vec<String> = pi_ai::transcript::current_tools(&requests[0].context.messages)
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert!(declared.contains(&"bash".to_string()), "{declared:?}");
    let desktop = ["todo_write", "request_user_input"]
        .into_iter()
        .chain(crate::agent::external_agents::EXTERNAL_AGENT_TOOLS);
    for name in desktop {
        assert!(!declared.contains(&name.to_string()), "{declared:?}");
    }
    // The caller's prompt follows Maple's own.
    let system = pi_ai::transcript::current_system_prompt(&requests[0].context.messages);
    let maple = system.find("You are Maple.").expect("Maple's prompt");
    let persona = system
        .find("You are Buzz's Maple persona.")
        .expect("the caller's prompt");
    assert!(maple < persona, "{system}");
    // The bridge's variable reached the command.
    let output = requests[1]
        .context
        .messages
        .iter()
        .find_map(|message| match message {
            pi_ai::Message::ToolResult(result) => Some(pi_ai::content_text(&result.content)),
            _ => None,
        })
        .unwrap();
    assert!(output.contains("ws://relay"), "{output}");
    // A surface's run is the surface's alone: the host hears none of it.
    assert!(!harness.recorder.events().iter().any(
        |event| matches!(event, AgentServiceEvent::Run { run_id, .. } if *run_id == run.run_id)
    ));
    created.lease.release().await;
}

#[tokio::test]
async fn the_desktop_waits_for_a_surface_to_release_its_task() {
    let harness = Harness::new().await;
    let task = harness.create_task().await;
    let attached = harness
        .handle
        .attach_surface_session(&task, bridge_context(), Vec::new())
        .await
        .unwrap();
    assert_eq!(attached.detail.session.id, task);
    let access = attached.lease.access();
    let refused = harness
        .handle
        .send_message(prompt(&task, "From the desktop"))
        .await
        .err()
        .unwrap();
    assert_eq!(refused, SURFACE_CONTROLLED_ERROR);
    // A second surface cannot hold it either.
    let second = harness
        .handle
        .attach_surface_session(&task, bridge_context(), Vec::new())
        .await
        .err()
        .unwrap();
    assert_eq!(second, SURFACE_CONTROLLED_ERROR);

    let runtime = harness.runtime().await;
    harness.faux.push_text("Loaded.");
    let mut run = harness
        .handle
        .send_surface_message(&access, prompt(&task, "Hi"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    assert!(runtime.loaded_session(&task).await.is_some());

    // Released, the context is gone with the loaded session, and the task
    // is the desktop's again.
    attached.lease.release().await;
    assert!(!runtime.surface_holds(&access));
    assert!(runtime.loaded_session(&task).await.is_none());
    let gone = harness
        .handle
        .send_surface_message(&access, prompt(&task, "Again"), CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert_eq!(gone, AGENT_SURFACE_INACTIVE_ERROR);
    harness.faux.push_text("Back home.");
    let mut run = harness
        .handle
        .send_message(prompt(&task, "From the desktop"))
        .await
        .unwrap();
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let declared: Vec<String> =
        pi_ai::transcript::current_tools(&harness.faux.requests().pop().unwrap().context.messages)
            .into_iter()
            .map(|tool| tool.name)
            .collect();
    assert!(declared.contains(&"todo_write".to_string()), "{declared:?}");
}

#[tokio::test]
async fn cancelling_a_surface_send_stops_its_run() {
    let harness =
        Harness::with_faux(FauxProvider::new().with_chunk_delay(Duration::from_millis(10))).await;
    let created = harness
        .handle
        .create_surface_session(new_task(&harness, None), bridge_context(), Vec::new())
        .await
        .unwrap();
    let task = created.detail.session.id.clone();
    harness.faux.push_hang();
    let cancellation = CancellationToken::new();
    let mut run = harness
        .handle
        .send_surface_message(
            &created.lease.access(),
            prompt(&task, "Wait forever"),
            cancellation.clone(),
        )
        .await
        .unwrap();
    crate::agent::tests::eventually(|| !harness.faux.requests().is_empty()).await;
    cancellation.cancel();
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Cancelled);
    // The next prompt runs once the stopped one has ended.
    harness.faux.push_text("Ready.");
    let mut run = harness
        .handle
        .send_surface_message(
            &created.lease.access(),
            prompt(&task, "Now answer"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    created.lease.release().await;
}

#[tokio::test]
async fn a_surface_brings_its_own_mcp_servers() {
    let harness = Harness::new().await;
    let fake = crate::agent::mcp::fake_server::FakeServer::start(None).await;
    let server = AgentMcpServer {
        name: "Bridge".to_string(),
        description: "ACP session MCP server".to_string(),
        enabled: true,
        timeout_seconds: 30,
        transport: AgentMcpTransport::StreamableHttp {
            url: fake.url.clone(),
            environment: Vec::new(),
            headers: vec![AgentMcpKeyValue {
                key: "Authorization".to_string(),
                value: "Bearer session".to_string(),
            }],
        },
    };
    let created = harness
        .handle
        .create_surface_session(new_task(&harness, None), bridge_context(), vec![server])
        .await
        .unwrap();
    let task = created.detail.session.id.clone();
    harness.faux.push_message(vec![faux_tool_call(
        "mcp__bridge__echo",
        json!({"text": "through the bridge"}),
    )]);
    harness.faux.push_text("Echoed.");
    let mut run = harness
        .handle
        .send_surface_message(
            &created.lease.access(),
            prompt(&task, "Echo"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    assert_eq!(
        fake.calls(),
        [("echo".to_string(), json!({"text": "through the bridge"}))]
    );
    assert!(
        fake.authorization()
            .iter()
            .all(|value| value.as_deref() == Some("Bearer session"))
    );
    created.lease.release().await;
}

#[test]
fn a_surface_server_takes_the_place_of_a_saved_one_of_its_name() {
    let server = |name: &str, url: &str| AgentMcpServer {
        name: name.to_string(),
        description: String::new(),
        enabled: true,
        timeout_seconds: 30,
        transport: AgentMcpTransport::StreamableHttp {
            url: url.to_string(),
            environment: Vec::new(),
            headers: Vec::new(),
        },
    };
    let servers = with_surface_servers(
        vec![
            server("GitHub", "https://saved.example/github"),
            server("Docs", "https://saved.example/docs"),
        ],
        vec![server("github", "https://bridge.example/github")],
    );
    let urls: Vec<String> = servers
        .iter()
        .map(|server| match &server.transport {
            AgentMcpTransport::StreamableHttp { url, .. } => url.clone(),
            AgentMcpTransport::Stdio { .. } => unreachable!(),
        })
        .collect();
    assert_eq!(
        urls,
        [
            "https://saved.example/docs",
            "https://bridge.example/github"
        ]
    );
}

#[tokio::test]
async fn a_start_removes_provisional_tasks_a_crash_stranded() {
    let harness = Harness::new().await;
    let store = harness.handle.store().unwrap();
    let now = pi_ai::now_ms();
    let row = |id: &str, age_ms: i64, provisional: bool| {
        let mut row = TaskRow::new(
            id.to_string(),
            "Maple ACP".to_string(),
            harness.project.path().to_string_lossy().into_owned(),
            TaskKind::Acp,
            None,
            now - age_ms,
        );
        if provisional {
            set_provisional(&mut row);
        }
        row
    };
    store
        .insert(&row("stranded", 11 * 60 * 1000, true))
        .unwrap();
    store.insert(&row("starting", 60 * 1000, true)).unwrap();
    store
        .insert(&row("prompted", 11 * 60 * 1000, false))
        .unwrap();
    sweep_stale_provisional(&store, harness.handle.paths(), &harness.handle.user_id);
    assert!(store.get("stranded").unwrap().is_none());
    assert!(store.get("starting").unwrap().is_some());
    assert!(store.get("prompted").unwrap().is_some());
}
