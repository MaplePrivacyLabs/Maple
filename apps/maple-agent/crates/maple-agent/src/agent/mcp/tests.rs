use super::*;

fn stdio(name: &str, command: &str) -> AgentMcpServer {
    AgentMcpServer {
        name: name.to_string(),
        description: String::new(),
        enabled: true,
        timeout_seconds: 300,
        transport: AgentMcpTransport::Stdio {
            command: command.to_string(),
            environment: Vec::new(),
        },
    }
}

fn pair(key: &str, value: &str) -> AgentMcpKeyValue {
    AgentMcpKeyValue {
        key: key.to_string(),
        value: value.to_string(),
    }
}

#[test]
fn servers_are_trimmed_and_their_names_must_be_distinct_and_usable() {
    let mut server = stdio("  Files  ", "  npx server-files  ");
    server.description = " Reads files ".into();
    let saved = normalize_mcp_servers(vec![server]).unwrap();
    assert_eq!(saved[0].name, "Files");
    assert_eq!(saved[0].description, "Reads files");
    assert!(matches!(
        &saved[0].transport,
        AgentMcpTransport::Stdio { command, .. } if command == "npx server-files"
    ));

    for (servers, error) in [
        (vec![stdio(" ", "x")], "MCP server name cannot be empty"),
        (
            vec![stdio(&"n".repeat(65), "x")],
            "must be 64 characters or fewer",
        ),
        (vec![stdio("Developer", "x")], "is reserved by Maple"),
        (
            vec![stdio("My Server", "x"), stdio("myserver", "y")],
            "conflicts with another configured server",
        ),
        (vec![stdio("files", "  ")], "requires a command"),
        (
            vec![stdio("files", "npx \"server")],
            "has an invalid command",
        ),
    ] {
        let message = normalize_mcp_servers(servers).unwrap_err();
        assert!(message.contains(error), "{message}");
    }
    let mut no_timeout = stdio("files", "x");
    no_timeout.timeout_seconds = 0;
    assert!(normalize_mcp_servers(vec![no_timeout]).is_err());
}

#[test]
fn environment_and_headers_are_named_distinct_and_safe() {
    let http = |environment: Vec<AgentMcpKeyValue>, headers: Vec<AgentMcpKeyValue>| {
        let mut server = stdio("remote", "x");
        server.transport = AgentMcpTransport::StreamableHttp {
            url: " https://mcp.example/ ".into(),
            environment,
            headers,
        };
        normalize_mcp_servers(vec![server])
    };
    let saved = http(
        vec![pair(" TOKEN ", "t")],
        vec![pair("Authorization", "Bearer x")],
    )
    .unwrap();
    assert!(matches!(
        &saved[0].transport,
        AgentMcpTransport::StreamableHttp { url, environment, .. }
            if url == "https://mcp.example/" && environment[0].key == "TOKEN"
    ));
    for (environment, headers, error) in [
        (
            vec![pair("path", "/evil")],
            vec![],
            "cannot override the environment variable path",
        ),
        (
            vec![pair(" ", "x")],
            vec![],
            "has an empty environment variable name",
        ),
        (
            vec![],
            vec![pair("X-A", "1"), pair("x-a", "2")],
            "duplicate HTTP header named x-a",
        ),
        (
            vec![],
            vec![pair("Bad Header", "1")],
            "cannot contain whitespace",
        ),
    ] {
        let message = http(environment, headers).unwrap_err();
        assert!(message.contains(error), "{message}");
    }
}

#[test]
fn commands_split_like_goose_split_them() {
    assert_eq!(
        split_command(r#"npx -y "my server" 'a b' it's"#).unwrap(),
        ["npx", "-y", "my server", "a b", "it's"]
    );
    assert!(split_command("echo 'open").is_err());
}

#[test]
fn only_a_new_server_cannot_take_the_computer_use_name() {
    let previous = vec![stdio("cua_driver", "x")];
    assert!(validate_new_mcp_integration_collisions(&previous, &previous).is_ok());
    let error =
        validate_new_mcp_integration_collisions(&[], &[stdio("Cua Driver", "x")]).unwrap_err();
    assert!(
        error.contains("conflicts with the Computer use integration"),
        "{error}"
    );
}

fn store_layout() -> (
    tempfile::TempDir,
    AgentPathLayout,
    std::sync::Arc<crate::store::Store>,
) {
    let root = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(
        crate::store::Store::open(crate::store::StorePaths::new(root.path().join("data"))).unwrap(),
    );
    let paths =
        AgentPathLayout::from_app_roots(root.path().join("prior-config"), root.path().join("data"))
            .with_store(store.clone())
            .with_home(Some(root.path().join("home")));
    (root, paths, store)
}

#[test]
fn saved_catalog_is_redacted_and_launch_injects_values_from_only_the_new_store() {
    let (root, paths, store) = store_layout();
    let prior = root
        .path()
        .join("prior-config/agent/accounts/synthetic/config.json");
    std::fs::create_dir_all(prior.parent().unwrap()).unwrap();
    std::fs::write(&prior, b"{\"mcpServers\":[{\"name\":\"Previous\"}]}").unwrap();
    assert!(
        load_saved_mcp_servers(&paths, "synthetic")
            .unwrap()
            .is_empty()
    );
    let mut configured = stdio("Example", "synthetic-mcp --serve");
    if let AgentMcpTransport::Stdio { environment, .. } = &mut configured.transport {
        environment.push(pair("TOKEN", "synthetic-host-only"));
    }
    save_saved_mcp_servers(&paths, "synthetic", vec![configured], McpEditOrigin::Local).unwrap();
    let safe = load_saved_mcp_servers(&paths, "synthetic").unwrap();
    assert!(
        !serde_json::to_string(&safe)
            .unwrap()
            .contains("synthetic-host-only")
    );
    assert!(
        matches!(&safe[0].transport,AgentMcpTransport::Stdio { environment,.. } if environment[0].value.is_empty())
    );
    let launch = read_saved_servers(&paths, "synthetic").unwrap();
    assert!(
        matches!(&launch[0].transport,AgentMcpTransport::Stdio { environment,.. } if environment[0].value=="synthetic-host-only")
    );
    // The old form echoes only names/empty values. That means Keep and cannot
    // accidentally overwrite a value with a UI placeholder.
    save_saved_mcp_servers(&paths, "synthetic", safe, McpEditOrigin::Local).unwrap();
    let launch = read_saved_servers(&paths, "synthetic").unwrap();
    assert!(
        matches!(&launch[0].transport,AgentMcpTransport::Stdio { environment,.. } if environment[0].value=="synthetic-host-only")
    );
    assert!(
        load_saved_mcp_servers(&paths, "another-account")
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        std::fs::read(&prior).unwrap(),
        b"{\"mcpServers\":[{\"name\":\"Previous\"}]}"
    );
    let account = store.account_id("synthetic").unwrap().unwrap();
    assert!(store.mcp_definitions(account).unwrap()[0].environment[0].has_value);
}

#[test]
fn remote_catalog_save_requires_replacement_for_redirect_and_allows_removal() {
    let (_root, paths, _store) = store_layout();
    let mut configured = stdio("Example", "synthetic-mcp");
    if let AgentMcpTransport::Stdio { environment, .. } = &mut configured.transport {
        environment.push(pair("TOKEN", "synthetic-local-value"));
    }
    save_saved_mcp_servers(&paths, "synthetic", vec![configured], McpEditOrigin::Remote).unwrap();
    let mut safe = load_saved_mcp_servers(&paths, "synthetic").unwrap();
    safe[0].enabled = false;
    save_saved_mcp_servers(&paths, "synthetic", safe.clone(), McpEditOrigin::Remote).unwrap();
    if let AgentMcpTransport::Stdio { command, .. } = &mut safe[0].transport {
        *command = "synthetic-capture".into();
    }
    assert!(
        save_saved_mcp_servers(&paths, "synthetic", safe.clone(), McpEditOrigin::Remote).is_err()
    );
    assert!(
        matches!(&read_saved_servers(&paths,"synthetic").unwrap()[0].transport,
        AgentMcpTransport::Stdio { command,environment } if command=="synthetic-mcp" && environment[0].value=="synthetic-local-value")
    );
    if let AgentMcpTransport::Stdio { environment, .. } = &mut safe[0].transport {
        environment[0].value = "synthetic-replacement".into();
    }
    save_saved_mcp_servers(&paths, "synthetic", safe, McpEditOrigin::Remote).unwrap();
    assert!(
        matches!(&read_saved_servers(&paths,"synthetic").unwrap()[0].transport,
        AgentMcpTransport::Stdio { command,environment } if command=="synthetic-capture" && environment[0].value=="synthetic-replacement")
    );
    assert!(
        !serde_json::to_string(&load_saved_mcp_servers(&paths, "synthetic").unwrap())
            .unwrap()
            .contains("synthetic-replacement")
    );
    save_saved_mcp_servers(&paths, "synthetic", vec![], McpEditOrigin::Remote).unwrap();
    assert!(
        load_saved_mcp_servers(&paths, "synthetic")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn project_catalog_switches_use_project_rows_and_keep_catalog_defaults() {
    let (root, paths, store) = store_layout();
    save_saved_mcp_servers(
        &paths,
        "synthetic",
        vec![stdio("Example", "synthetic-mcp")],
        McpEditOrigin::Local,
    )
    .unwrap();
    let account = store.account_id("synthetic").unwrap().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    store
        .set_project_mcp_defaults(
            account,
            &path_string(&project),
            &serde_json::json!({"example":false}),
            1,
        )
        .unwrap();
    let defaults = load_saved_mcp_servers(&paths, "synthetic").unwrap();
    let projected =
        servers_with_project_defaults(&paths, "synthetic", &project, defaults.clone()).unwrap();
    assert!(!projected[0].enabled);
    assert!(defaults[0].enabled);
    assert!(
        servers_with_project_defaults(&paths, "another-account", &project, defaults).unwrap()[0]
            .enabled
    );
}
