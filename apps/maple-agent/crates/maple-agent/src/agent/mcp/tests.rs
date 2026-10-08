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
        error.contains("conflicts with the Computer use (CUA) integration"),
        "{error}"
    );
}
