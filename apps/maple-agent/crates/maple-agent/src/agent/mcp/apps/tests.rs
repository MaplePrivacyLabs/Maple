use super::super::catalog::{McpCatalogCommand, McpCatalogConnection};
use super::*;
use crate::agent::tests::Harness;

fn entry(command: &str, arguments: &[&str]) -> McpCatalogEntry {
    McpCatalogEntry {
        id: "fake".into(),
        registry: None,
        name: "Fake App".into(),
        summary: "Does fake things".into(),
        category: super::super::catalog::McpCatalogCategory::Developer,
        connect: McpCatalogConnection::Command(McpCatalogCommand {
            command: command.into(),
            arguments: arguments.iter().map(|word| word.to_string()).collect(),
        }),
        auth: super::super::catalog::McpCatalogAuth::None,
        requires: Vec::new(),
        platforms: Vec::new(),
        connect_seconds: 10,
        help: Some("Open Fake App first.".into()),
    }
}

fn needs(name: &str, check: &[&str], min_version: Option<&str>) -> McpCatalogRequirement {
    McpCatalogRequirement {
        name: name.into(),
        check: check.iter().map(|word| word.to_string()).collect(),
        min_version: min_version.map(str::to_string),
        get_url: "https://example.com/get".into(),
    }
}

#[test]
fn versions_compare_as_numbers() {
    assert!(at_least("v24.22.2\n", "18"));
    assert!(at_least("v18.0.0", "18"));
    assert!(at_least("20.19.0", "20.19"));
    assert!(at_least("node 10.20.1", "10.9"));
    assert!(!at_least("v16.20.2", "18"));
    assert!(!at_least("v20.18.9", "20.19"));
    assert!(!at_least("not a version", "18"));
}

#[test]
fn the_card_says_what_is_missing() {
    let missing = McpAppMissing {
        name: "Node.js".into(),
        min_version: Some("18".into()),
        get_url: String::new(),
    };
    assert_eq!(missing.message(), "Needs Node.js 18 or newer.");
    assert_eq!(missing.action(), "Get Node.js");
    let missing = McpAppMissing {
        min_version: None,
        ..missing
    };
    assert_eq!(missing.message(), "Needs Node.js.");
}

#[tokio::test]
async fn a_missing_requirement_is_named_and_nothing_starts_or_saves() {
    let harness = Harness::new().await;
    let mut app = entry("maple-should-never-start", &[]);
    app.requires = vec![needs("Fake Tool", &["maple-no-such-command"], None)];
    let outcome = harness.handle.connect_catalog_app(&app).await.unwrap();
    assert_eq!(
        outcome,
        McpAppConnection::Missing(McpAppMissing {
            name: "Fake Tool".into(),
            min_version: None,
            get_url: "https://example.com/get".into(),
        })
    );
    assert!(
        harness
            .handle
            .list_mcp_definitions()
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn an_unknown_app_cannot_be_connected() {
    let harness = Harness::new().await;
    assert!(
        harness
            .handle
            .connect_mcp_app("nothing-here")
            .await
            .is_err()
    );
    assert!(
        harness
            .handle
            .list_mcp_definitions()
            .await
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
mod running {
    use super::*;

    /// Answers the handshake and an empty tool list, as a real server would.
    const SERVER: &str = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1"}}}\n' "$id" ;;
    *'"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[]}}\n' "$id" ;;
  esac
done"#;

    fn script(dir: &tempfile::TempDir, body: &str) -> String {
        let path = dir.path().join("server.sh");
        std::fs::write(&path, body).unwrap();
        path.display().to_string()
    }

    #[tokio::test]
    async fn an_old_version_counts_as_missing() {
        let harness = Harness::new().await;
        let mut app = entry("maple-should-never-start", &[]);
        app.requires = vec![needs(
            "Fake Tool",
            &["sh", "-c", "echo v16.2.0"],
            Some("18"),
        )];
        assert!(matches!(
            harness.handle.connect_catalog_app(&app).await.unwrap(),
            McpAppConnection::Missing(missing) if missing.min_version.as_deref() == Some("18")
        ));
    }

    #[tokio::test]
    async fn a_working_app_is_saved_on_after_it_starts() {
        let harness = Harness::new().await;
        let dir = tempfile::tempdir().unwrap();
        let server = script(&dir, SERVER);
        let mut app = entry("sh", &[&server]);
        app.requires = vec![needs("A Shell", &["sh", "-c", "echo 5.9"], Some("5"))];
        let McpAppConnection::Connected(saved) =
            harness.handle.connect_catalog_app(&app).await.unwrap()
        else {
            panic!("the app should connect");
        };
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "Fake App");
        assert!(saved[0].enabled);
        assert_eq!(saved[0].target, app.target());
        // The check is only for connecting: tasks get the normal limit.
        assert_eq!(saved[0].timeout_seconds, default_mcp_timeout_seconds());
    }

    #[tokio::test]
    async fn connecting_leaves_other_servers_alone() {
        let harness = Harness::new().await;
        let dir = tempfile::tempdir().unwrap();
        let server = script(&dir, SERVER);
        let existing = crate::store::records::McpServerEdit {
            key: "mine".into(),
            name: "Mine".into(),
            description: String::new(),
            enabled: false,
            timeout_seconds: 30,
            target: crate::store::records::McpTarget::Stdio {
                command: "my-server --stdio".into(),
            },
            environment: Vec::new(),
            headers: Vec::new(),
        };
        harness
            .handle
            .save_mcp_definition_edits(vec![existing])
            .await
            .unwrap();
        let McpAppConnection::Connected(saved) = harness
            .handle
            .connect_catalog_app(&entry("sh", &[&server]))
            .await
            .unwrap()
        else {
            panic!("the app should connect");
        };
        let names: Vec<_> = saved.iter().map(|server| server.name.as_str()).collect();
        assert_eq!(names, ["Mine", "Fake App"]);
        assert!(!saved[0].enabled);
    }

    #[tokio::test]
    async fn an_app_that_will_not_start_says_what_to_do_and_is_not_saved() {
        let harness = Harness::new().await;
        let error = harness
            .handle
            .connect_catalog_app(&entry("false", &[]))
            .await
            .unwrap_err();
        assert_eq!(error, "Open Fake App first.");
        assert!(
            harness
                .handle
                .list_mcp_definitions()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn an_app_that_never_answers_runs_out_of_time() {
        let harness = Harness::new().await;
        let dir = tempfile::tempdir().unwrap();
        let server = script(&dir, "sleep 60\n");
        let mut app = entry("sh", &[&server]);
        app.connect_seconds = 1;
        let error = tokio::time::timeout(
            Duration::from_secs(20),
            harness.handle.connect_catalog_app(&app),
        )
        .await
        .expect("it should give up by itself")
        .unwrap_err();
        assert_eq!(error, "Open Fake App first.");
    }

    #[tokio::test]
    async fn connecting_can_be_stopped() {
        let harness = Harness::new().await;
        let dir = tempfile::tempdir().unwrap();
        let server = script(&dir, "sleep 60\n");
        let app = entry("sh", &[&server]);
        let (result, ()) = tokio::join!(harness.handle.connect_catalog_app(&app), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            harness.handle.cancel_mcp_sign_in(&app.name);
        });
        assert_eq!(result.unwrap_err(), SIGN_IN_CANCELLED);
        assert!(
            harness
                .handle
                .list_mcp_definitions()
                .await
                .unwrap()
                .is_empty()
        );
    }
}
