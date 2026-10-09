use std::sync::atomic::{AtomicUsize, Ordering};

use rmcp::model::ContentBlock;
use serde_json::json;

use super::super::fake_server::FakeServer;
use super::*;

fn key_value(key: &str, value: &str) -> AgentMcpKeyValue {
    AgentMcpKeyValue {
        key: key.to_string(),
        value: value.to_string(),
    }
}

fn http_config(url: &str, timeout_seconds: u64) -> AgentMcpServer {
    AgentMcpServer {
        name: "Fake".to_string(),
        description: String::new(),
        enabled: true,
        timeout_seconds,
        transport: AgentMcpTransport::StreamableHttp {
            url: url.to_string(),
            environment: Vec::new(),
            headers: Vec::new(),
        },
    }
}

fn stdio_config(command: &str) -> AgentMcpServer {
    AgentMcpServer {
        name: "Local".to_string(),
        description: String::new(),
        enabled: true,
        timeout_seconds: 5,
        transport: AgentMcpTransport::Stdio {
            command: command.to_string(),
            environment: Vec::new(),
        },
    }
}

fn server(config: AgentMcpServer) -> McpServer {
    McpServer::new(config, &std::env::temp_dir(), None, Arc::new(|| {}))
}

fn arguments(value: serde_json::Value) -> JsonObject {
    value.as_object().cloned().unwrap()
}

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn environment_entries_fill_in_endpoints_and_headers() {
    let environment = [
        key_value("TOKEN", "s3cret"),
        key_value("HOST", "example.com"),
    ];
    assert_eq!(substitute("Bearer ${TOKEN}", &environment), "Bearer s3cret");
    assert_eq!(
        substitute("https://$HOST/mcp?t=${ TOKEN }&x=$HOSTNAME", &environment),
        "https://example.com/mcp?t=s3cret&x=$HOSTNAME"
    );
    // Unknown names, a lone dollar and an unclosed brace stay as written;
    // a value is not read again for names.
    let looping = [key_value("A", "$B"), key_value("B", "b")];
    assert_eq!(substitute("$A ${C} $ ${A", &looping), "$B ${C} $ ${A");
}

#[test]
fn a_leading_tilde_is_the_home_folder() {
    assert_eq!(expand_home("npx"), "npx");
    assert_eq!(expand_home("~user/bin"), "~user/bin");
    assert_eq!(expand_home("a~/b"), "a~/b");
    if let Some(home) = super::super::super::config::home_dir() {
        assert_eq!(
            expand_home("~/bin/server"),
            format!("{}/bin/server", home.display())
        );
    }
}

#[tokio::test]
async fn an_http_server_connects_and_runs_its_tools() {
    let fake = FakeServer::start(Some("Use echo to repeat things.")).await;
    let mut config = http_config(&format!("{}?key=${{KEY}}", fake.url), 30);
    config.transport = AgentMcpTransport::StreamableHttp {
        url: format!("{}?key=${{KEY}}", fake.url),
        environment: vec![key_value("KEY", "abc"), key_value("TOKEN", "s3cret")],
        headers: vec![key_value("Authorization", "Bearer $TOKEN")],
    };
    let server = server(config);

    let listing = server.connect().await.unwrap();
    let names: Vec<&str> = listing
        .tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect();
    assert_eq!(names, ["echo", "fail", "slow", "progress", "grow"]);
    assert_eq!(
        listing.instructions.as_deref(),
        Some("Use echo to repeat things.")
    );
    assert_eq!(
        fake.methods()[..2],
        ["initialize", "notifications/initialized"]
    );
    assert!(
        fake.authorization()
            .iter()
            .all(|value| value.as_deref() == Some("Bearer s3cret"))
    );
    assert!(
        fake.queries()
            .iter()
            .all(|query| query.as_deref() == Some("key=abc"))
    );

    let echoed = server
        .call_tool(
            "echo",
            arguments(json!({ "text": "hi" })),
            CancellationToken::new(),
            pi_agent_core::ToolUpdates::none(),
        )
        .await
        .unwrap();
    assert_eq!(text(&echoed), "hi");
    assert_ne!(echoed.is_error, Some(true));
    let failed = server
        .call_tool(
            "fail",
            JsonObject::new(),
            CancellationToken::new(),
            pi_agent_core::ToolUpdates::none(),
        )
        .await
        .unwrap();
    assert_eq!(failed.is_error, Some(true));
    let unknown = server
        .call_tool(
            "missing",
            JsonObject::new(),
            CancellationToken::new(),
            pi_agent_core::ToolUpdates::none(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        unknown,
        "MCP server \"Fake\" failed to run missing: Unknown tool: missing"
    );
    // A call that reports progress still answers.
    let progressed = server
        .call_tool(
            "progress",
            JsonObject::new(),
            CancellationToken::new(),
            pi_agent_core::ToolUpdates::none(),
        )
        .await
        .unwrap();
    assert_eq!(text(&progressed), "done");
    assert_eq!(
        fake.calls()
            .iter()
            .map(|(tool, _)| tool.as_str())
            .collect::<Vec<_>>(),
        ["echo", "fail", "missing", "progress"]
    );
    assert_eq!(fake.calls()[0].1, json!({ "text": "hi" }));

    server.shutdown().await;
    assert_eq!(server.connect().await.unwrap_err(), "it was stopped");
}

#[tokio::test]
async fn a_changed_tool_list_is_read_again() {
    let fake = FakeServer::start(None).await;
    let changes = Arc::new(AtomicUsize::new(0));
    let changed = Arc::new(tokio::sync::Notify::new());
    let tools_changed: ToolsChanged = {
        let (changes, changed) = (Arc::clone(&changes), Arc::clone(&changed));
        Arc::new(move || {
            changes.fetch_add(1, Ordering::SeqCst);
            changed.notify_one();
        })
    };
    let server = McpServer::new(
        http_config(&fake.url, 30),
        &std::env::temp_dir(),
        None,
        tools_changed,
    );
    assert_eq!(server.connect().await.unwrap().tools.len(), 5);
    let grown = server
        .call_tool(
            "grow",
            JsonObject::new(),
            CancellationToken::new(),
            pi_agent_core::ToolUpdates::none(),
        )
        .await
        .unwrap();
    assert_eq!(text(&grown), "grown");
    tokio::time::timeout(Duration::from_secs(5), changed.notified())
        .await
        .expect("the server's notice arrives");
    assert_eq!(changes.load(Ordering::SeqCst), 1);
    let listing = server.refresh().await.unwrap();
    assert_eq!(listing.tools.len(), 6);
    assert_eq!(listing.tools[5].name, "added");
    server.shutdown().await;
}

#[tokio::test]
async fn a_stopped_call_tells_the_server() {
    let fake = FakeServer::start(None).await;
    let server = server(http_config(&fake.url, 30));
    let cancel = CancellationToken::new();
    let stop = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel.cancel();
        })
    };
    let error = server
        .call_tool(
            "slow",
            JsonObject::new(),
            cancel,
            pi_agent_core::ToolUpdates::none(),
        )
        .await
        .unwrap_err();
    stop.await.unwrap();
    assert_eq!(error, "slow was cancelled");
    for _ in 0..50 {
        if !fake.cancelled().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(fake.cancelled().len(), 1);
    server.shutdown().await;
}

#[tokio::test]
async fn a_call_without_an_answer_times_out() {
    let fake = FakeServer::start(None).await;
    let server = server(http_config(&fake.url, 1));
    let error = server
        .call_tool(
            "slow",
            JsonObject::new(),
            CancellationToken::new(),
            pi_agent_core::ToolUpdates::none(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "MCP server \"Fake\" failed to run slow: it did not answer within 1 seconds"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn a_server_that_cannot_start_says_why() {
    let missing = server(stdio_config("maple-test-no-such-mcp-server --stdio"));
    let error = missing.connect().await.unwrap_err();
    assert!(
        error.starts_with("could not start maple-test-no-such-mcp-server:"),
        "{error}"
    );
    // A connection that failed is tried again on the next use.
    assert!(missing.connect().await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn a_server_that_exits_shows_its_error_output() {
    let broken = server(stdio_config(
        "sh -c 'echo \"no token configured\" >&2; exit 3'",
    ));
    let error = broken.connect().await.unwrap_err();
    assert!(error.ends_with("\nno token configured"), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn stopping_a_stdio_server_ends_everything_it_started() {
    // The shell starts a child, prints its pid and waits; neither reads its
    // input, so only the signal to the process group ends them.
    let command = "sh -c 'sleep 300 & echo $!; wait'";
    let mut process = server(stdio_config(command)).spawn(command, &[]).unwrap();
    let mut stdout = process.child.stdout.take().unwrap();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while stdout.read(&mut byte).await.unwrap() == 1 && byte[0] != b'\n' {
        line.push(byte[0]);
    }
    let sleeper: u32 = String::from_utf8(line).unwrap().trim().parse().unwrap();
    drop(process.child.stdin.take());
    process.stop().await;
    // Running, and not a zombie waiting for whoever adopted it to reap it.
    let alive = |pid: u32| {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
        !state.is_empty() && !state.starts_with('Z')
    };
    for _ in 0..50 {
        if !alive(sleeper) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!alive(sleeper), "the server's child was left running");
}
