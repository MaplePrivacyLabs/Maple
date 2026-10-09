use super::super::super::store::TaskKind;
use super::super::fake_server::FakeServer;
use super::*;

fn stdio(name: &str, command: &str, enabled: bool) -> AgentMcpServer {
    AgentMcpServer {
        name: name.to_string(),
        description: format!("{name} tools"),
        enabled,
        timeout_seconds: 30,
        transport: AgentMcpTransport::Stdio {
            command: command.to_string(),
            environment: Vec::new(),
        },
    }
}

fn http(name: &str, url: &str) -> AgentMcpServer {
    AgentMcpServer {
        name: name.to_string(),
        description: String::new(),
        enabled: true,
        timeout_seconds: 30,
        transport: AgentMcpTransport::StreamableHttp {
            url: url.to_string(),
            environment: Vec::new(),
            headers: Vec::new(),
        },
    }
}

fn row_with(names: &[&str]) -> TaskRow {
    let mut row = TaskRow::new(
        "task".into(),
        "Task".into(),
        "/tmp".into(),
        TaskKind::Desktop,
        None,
        0,
    );
    set_chosen_servers(
        &mut row,
        names.iter().map(|name| name.to_string()).collect(),
    );
    row
}

const MISSING_COMMAND: &str = "maple-test-no-such-mcp-server";

#[test]
fn new_tasks_get_the_servers_switched_on_or_named() {
    let saved = [
        stdio("GitHub", "gh-mcp", true),
        stdio("Docs", "docs-mcp", false),
        stdio("Cua Driver", "cua-driver mcp", true),
    ];
    assert_eq!(servers_for_new_task(&saved, None).unwrap(), ["GitHub"]);
    let named: Vec<String> = ["docs", " Docs ", "cua-driver", "GitHub"]
        .iter()
        .map(|name| name.to_string())
        .collect();
    assert_eq!(
        servers_for_new_task(&saved, Some(&named)).unwrap(),
        ["Docs", "GitHub"]
    );
    assert!(servers_for_new_task(&saved, Some(&[])).unwrap().is_empty());
    assert_eq!(
        servers_for_new_task(&saved, Some(&["Gone".to_string()])).unwrap_err(),
        "MCP server 'Gone' is no longer configured. Reopen the MCP menu and try again."
    );
}

#[test]
fn a_tasks_rows_list_the_saved_servers_then_the_missing_ones() {
    let saved = [
        stdio("GitHub", "gh-mcp", true),
        http("Docs", "https://docs"),
    ];
    let row = row_with(&["github", "Old"]);
    let rows: Vec<(String, bool, bool, String)> = session_rows(&saved, &row)
        .into_iter()
        .map(|row| (row.name, row.enabled, row.available, row.transport))
        .collect();
    assert_eq!(
        rows,
        [
            ("GitHub".to_string(), true, true, "stdio".to_string()),
            (
                "Docs".to_string(),
                false,
                true,
                "streamable_http".to_string()
            ),
            ("Old".to_string(), true, false, "unconfigured".to_string()),
        ]
    );
    // Only chosen servers that are still saved run, with today's settings.
    let running: Vec<String> = task_servers(&saved, &row)
        .into_iter()
        .map(|server| server.name)
        .collect();
    assert_eq!(running, ["GitHub"]);
    assert!(
        chosen_servers(&TaskRow::new(
            "t".into(),
            "T".into(),
            "/".into(),
            TaskKind::Desktop,
            None,
            0
        ))
        .is_empty()
    );
}

#[test]
fn failures_are_named_briefly() {
    let failed: Vec<(String, String)> = ["a", "b", "c", "d"]
        .iter()
        .map(|name| (name.to_string(), format!("{name} broke")))
        .collect();
    assert_eq!(
        failures_notice(&failed),
        "Some MCP servers could not connect: a: a broke; b: b broke; c: c broke; and 1 more"
    );
    let long = vec![("s".to_string(), "x".repeat(300))];
    assert!(failures_notice(&long).ends_with(&format!("{}…", "x".repeat(200))));
}

#[tokio::test]
async fn servers_connect_and_failures_are_reported_once() {
    let fake = FakeServer::start(Some("Read the docs first.")).await;
    let mcp = TaskMcp::new(&std::env::temp_dir(), None);
    mcp.sync(vec![
        http("Docs", &fake.url),
        stdio("Broken", MISSING_COMMAND, true),
    ]);
    assert!(mcp.wait(STARTUP_WAIT).await);
    let notice = mcp.take_notice().unwrap();
    assert!(
        notice.starts_with(&format!(
            "Some MCP servers could not connect: Broken: could not start {MISSING_COMMAND}"
        )),
        "{notice}"
    );
    assert_eq!(mcp.take_notice(), None);
    assert!(mcp.last_used().is_some());

    // A later run tries the failed server again without waiting for it,
    // and does not report the same failure again.
    mcp.sync(vec![
        http("Docs", &fake.url),
        stdio("Broken", MISSING_COMMAND, true),
    ]);
    assert!(mcp.wait(Duration::ZERO).await);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(mcp.take_notice(), None);

    // Unchosen servers stop; the docs server connected only once.
    mcp.sync(Vec::new());
    assert!(mcp.last_used().is_none());
    assert_eq!(
        fake.methods()
            .iter()
            .filter(|method| *method == "initialize")
            .count(),
        1
    );
}

#[tokio::test]
async fn a_server_switched_on_connects_at_once() {
    let fake = FakeServer::start(None).await;
    let mcp = TaskMcp::new(&std::env::temp_dir(), None);
    mcp.enable(http("Docs", &fake.url)).await.unwrap();
    assert_eq!(fake.methods()[0], "initialize");
    let error = mcp
        .enable(stdio("Broken", MISSING_COMMAND, true))
        .await
        .unwrap_err();
    assert!(error.starts_with("could not start"), "{error}");
    // The failure was the switch's to report, not the next run's.
    assert!(mcp.take_notice().is_none(), "{:?}", mcp.take_notice());
    mcp.disable("broken").await;
    mcp.stop_all().await;
    assert!(mcp.last_used().is_none());
}
