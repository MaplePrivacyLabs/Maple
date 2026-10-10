use serde_json::json;

use super::*;
use crate::agent::config::save_agent_config_inner;
use crate::agent::{AgentConfig, AgentMcpTransport, DEFAULT_MCP_TIMEOUT_SECONDS};

fn paths(root: &std::path::Path) -> AgentPathLayout {
    AgentPathLayout::from_app_roots(root.join("config"), root.join("local-data"))
}

fn detections(codex: CliDetection, claude: CliDetection, cua: CuaDetection) -> Detections {
    Detections { cua, codex, claude }
}

fn nothing_installed() -> Detections {
    detections(
        CliDetection::default(),
        CliDetection::default(),
        CuaDetection::not_available(),
    )
}

fn ready_cua() -> CuaDetection {
    CuaDetection {
        availability: AgentIntegrationAvailability::Available,
        permissions: Some(AgentIntegrationPermissions::none_required()),
        setup_available: true,
        detail: None,
    }
}

fn installed_codex(version: &str, problem: Option<&str>) -> CliDetection {
    CliDetection {
        executable: Some(PathBuf::from("codex")),
        version: Some(version.to_string()),
        signed_in: Some(false),
        problem: problem.map(str::to_string),
    }
}

fn request(id: &str, enabled: bool) -> AgentSetIntegrationEnabledRequest {
    AgentSetIntegrationEnabledRequest {
        id: id.to_string(),
        enabled,
    }
}

fn http_server(name: &str) -> AgentMcpServer {
    AgentMcpServer {
        name: name.to_string(),
        description: String::new(),
        enabled: true,
        timeout_seconds: DEFAULT_MCP_TIMEOUT_SECONDS,
        transport: AgentMcpTransport::StreamableHttp {
            url: "https://example.com/mcp".to_string(),
            environment: Vec::new(),
            headers: Vec::new(),
        },
    }
}

fn card<'a>(cards: &'a [AgentIntegration], id: &str) -> &'a AgentIntegration {
    cards.iter().find(|card| card.id == id).unwrap()
}

#[test]
fn the_catalog_lists_cua_then_the_external_agents() {
    let temporary = tempfile::tempdir().unwrap();
    let cards =
        project_integrations(&paths(temporary.path()), "user", &nothing_installed()).unwrap();
    let ids: Vec<&str> = cards.iter().map(|card| card.id.as_str()).collect();
    assert_eq!(ids, ["cua-driver", "codex", "claude"]);
    let cua = &cards[0];
    assert_eq!(cua.name, "Cua");
    assert_eq!(cua.availability, AgentIntegrationAvailability::NotDetected);
    assert_eq!(cua.backend, None);
    assert!(!cua.enabled_for_new_tasks && !cua.setup_available);
    assert!(!cua.is_external_agent());
    assert!(cards[1].is_external_agent() && cards[2].is_external_agent());
    assert!(
        cards[1]
            .detail
            .as_deref()
            .unwrap()
            .contains("Install the Codex CLI")
    );
    assert!(require_known_integration(" codex ").is_ok());
    assert_eq!(
        require_known_integration("other").unwrap_err(),
        "Unknown integration 'other'"
    );
    assert!(
        begin_integration_setup(&AgentSetupIntegrationRequest {
            id: "other".to_string()
        })
        .unwrap_err()
        .contains("Unknown integration")
    );
}

#[test]
fn codex_is_switched_on_only_when_it_can_run_and_off_always() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = paths(temporary.path());
    let usable = detections(
        installed_codex("codex-cli 0.150.0", None),
        CliDetection::default(),
        CuaDetection::not_available(),
    );
    let cards = project_integrations(&paths, "user", &usable).unwrap();
    let codex = card(&cards, "codex");
    assert_eq!(codex.availability, AgentIntegrationAvailability::Available);
    assert_eq!(codex.version.as_deref(), Some("codex-cli 0.150.0"));
    assert!(codex.detail.as_deref().unwrap().contains("codex login"));
    assert!(!codex.enabled_for_new_tasks);

    let cards = set_integration_default(&paths, "user", &request("codex", true), &usable).unwrap();
    assert!(card(&cards, "codex").enabled_for_new_tasks);
    assert_eq!(
        card(&cards, "codex").backend,
        Some(AgentIntegrationBackend::Embedded)
    );

    // Switching off works with Codex gone.
    let cards = set_integration_default(
        &paths,
        "user",
        &request("codex", false),
        &nothing_installed(),
    )
    .unwrap();
    assert!(!card(&cards, "codex").enabled_for_new_tasks);

    let error = set_integration_default(
        &paths,
        "user",
        &request("codex", true),
        &nothing_installed(),
    )
    .unwrap_err();
    assert!(error.contains("Install the Codex CLI"), "{error}");
    let old = detections(
        installed_codex(
            "0.100.0",
            Some("Codex 0.100.0 is older than the 0.143.0 that Maple needs."),
        ),
        CliDetection::default(),
        CuaDetection::not_available(),
    );
    let error = set_integration_default(&paths, "user", &request("codex", true), &old).unwrap_err();
    assert!(error.contains("older"), "{error}");
    let cards = project_integrations(&paths, "user", &old).unwrap();
    assert_eq!(
        card(&cards, "codex").availability,
        AgentIntegrationAvailability::SetupRequired
    );
}

#[test]
fn claude_reports_its_sign_in_without_it_gating_availability() {
    for (signed_in, detail) in [
        (Some(true), "Signed in."),
        (Some(false), detect::CLAUDE_SIGN_IN_HINT),
        (
            None,
            "Could not check sign-in. Run `claude auth status` in a terminal.",
        ),
    ] {
        let card = ExternalAgent::Claude.card(
            &CliDetection {
                executable: Some(PathBuf::from("claude")),
                signed_in,
                ..CliDetection::default()
            },
            None,
        );
        assert_eq!(card.detail.as_deref(), Some(detail));
        assert_eq!(card.availability, AgentIntegrationAvailability::Available);
    }
}

#[test]
fn cua_is_enabled_only_once_set_up_and_never_over_a_custom_server() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = paths(temporary.path());
    let error = set_integration_default(
        &paths,
        "user",
        &request("cua-driver", true),
        &nothing_installed(),
    )
    .unwrap_err();
    assert!(
        error.contains("Accessibility and Screen Recording"),
        "{error}"
    );
    // Setup that cannot finish leaves the choice alone.
    let cards = select_embedded_backend(&paths, "user", &nothing_installed()).unwrap();
    assert_eq!(cards[0].backend, None);

    let ready = detections(
        CliDetection::default(),
        CliDetection::default(),
        ready_cua(),
    );
    let cards = select_embedded_backend(&paths, "user", &ready).unwrap();
    assert_eq!(cards[0].backend, Some(AgentIntegrationBackend::Embedded));
    assert!(
        !cards[0].enabled_for_new_tasks,
        "setup alone does not enable"
    );
    let cards =
        set_integration_default(&paths, "user", &request("cua-driver", true), &ready).unwrap();
    assert!(cards[0].enabled_for_new_tasks);
    let cards = set_integration_default(
        &paths,
        "user",
        &request("cua-driver", false),
        &nothing_installed(),
    )
    .unwrap();
    assert!(!cards[0].enabled_for_new_tasks);
    assert_eq!(
        cards[0].backend,
        Some(AgentIntegrationBackend::Embedded),
        "switching off keeps the setup"
    );

    save_agent_config_inner(
        &paths,
        "user",
        &AgentConfig {
            mcp_servers: vec![http_server("Cua Driver")],
            ..AgentConfig::default()
        },
    )
    .unwrap();
    let error =
        set_integration_default(&paths, "user", &request("cua-driver", true), &ready).unwrap_err();
    assert!(error.contains("Rename or remove"), "{error}");
}

#[test]
fn a_cua_named_server_already_saved_does_not_block_other_saves() {
    for name in ["cua-driver", "Cua Driver", "cua_driver", CUA_NAME] {
        assert!(is_cua_identity(name), "{name}");
    }
    let legacy = http_server("Cua Driver");
    assert!(
        validate_new_mcp_integration_collisions(
            std::slice::from_ref(&legacy),
            &[legacy.clone(), http_server("Docs")],
        )
        .is_ok()
    );
    assert!(
        validate_new_mcp_integration_collisions(&[http_server("Docs")], &[legacy])
            .unwrap_err()
            .contains("Rename or remove")
    );
}

#[test]
fn saved_choices_of_other_versions_or_retired_backends() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = paths(temporary.path());
    let path = integrations_path(&paths, "user").unwrap();
    // A newer build's file is reported, and left as it is.
    write_device_local_json_file(&path, &json!({ "version": 99, "integrations": [] })).unwrap();
    let error = project_integrations(&paths, "user", &nothing_installed()).unwrap_err();
    assert!(error.contains("Unsupported"), "{error}");
    assert!(path.exists());

    // The standalone CuaDriver's entry is dropped; the others stay.
    write_device_local_json_file(
        &path,
        &json!({
            "version": INTEGRATIONS_FILE_VERSION,
            "integrations": [
                { "id": "cua-driver", "enabled": true, "backend": "external" },
                { "id": "codex", "enabled": true, "backend": "embedded" }
            ],
        }),
    )
    .unwrap();
    let cards = project_integrations(&paths, "user", &nothing_installed()).unwrap();
    assert_eq!(cards[0].backend, None);
    assert!(!cards[0].enabled_for_new_tasks);
    assert!(card(&cards, "codex").enabled_for_new_tasks);

    write_device_local_json_file(
        &path,
        &json!({
            "version": INTEGRATIONS_FILE_VERSION,
            "integrations": [{ "id": "other", "enabled": true, "backend": "embedded" }],
        }),
    )
    .unwrap();
    let error = project_integrations(&paths, "user", &nothing_installed()).unwrap_err();
    assert!(error.contains("unknown or duplicate"), "{error}");
}

#[test]
fn versions_read_from_what_the_clis_print() {
    assert_eq!(
        detect::parse_version("codex-cli 0.153.4"),
        Some((0, 153, 4))
    );
    assert_eq!(detect::parse_version("0.153.4"), Some((0, 153, 4)));
    assert_eq!(detect::parse_version("v1.2"), Some((1, 2, 0)));
    assert_eq!(detect::parse_version("2.1.0-beta+build"), Some((2, 1, 0)));
    assert_eq!(
        detect::parse_version("2.0.14 (Claude Code)"),
        Some((2, 0, 14))
    );
    assert_eq!(detect::parse_version("unknown"), None);
}

#[test]
fn executables_are_found_on_the_given_search_path() {
    let temporary = tempfile::tempdir().unwrap();
    let name = &detect::executable_candidates("maple-probe-test")[0];
    std::fs::write(temporary.path().join(name), "").unwrap();
    let search_path = std::env::join_paths([
        temporary.path().join("missing"),
        temporary.path().to_path_buf(),
    ])
    .unwrap()
    .into_string()
    .unwrap();
    assert_eq!(
        detect::find_executable("maple-probe-test", Some(&search_path)),
        Some(temporary.path().join(name))
    );
    assert_eq!(
        detect::find_executable("maple-probe-missing", Some(&search_path)),
        None
    );
}

#[cfg(windows)]
#[test]
fn windows_prefers_a_real_executable_over_a_shim() {
    let candidates = detect::executable_candidates("codex");
    assert_eq!(candidates[0], "codex.exe");
    assert!(candidates.contains(&"codex.cmd".to_string()));
    assert_eq!(detect::executable_candidates("codex.cmd"), ["codex.cmd"]);
}

#[cfg(unix)]
mod probes {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;

    /// A fake command line on a search path of its own, followed by the
    /// process's PATH so its script finds the usual tools.
    fn fake_cli(dir: &Path, name: &str, script: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let mut entries = vec![dir.to_path_buf()];
        entries.extend(std::env::split_paths(&inherited));
        std::env::join_paths(entries)
            .unwrap()
            .into_string()
            .unwrap()
    }

    #[tokio::test]
    async fn codex_is_detected_by_its_version() {
        let temporary = tempfile::tempdir().unwrap();
        let path = fake_cli(temporary.path(), "codex", "echo 'codex-cli 0.150.0'");
        let found = detect::detect_codex(Some(&path)).await;
        assert_eq!(found.executable, Some(temporary.path().join("codex")));
        assert_eq!(found.version.as_deref(), Some("codex-cli 0.150.0"));
        assert_eq!(found.problem, None);
        assert!(found.signed_in.is_some());

        let path = fake_cli(temporary.path(), "codex", "echo 'codex-cli 0.100.0'");
        let found = detect::detect_codex(Some(&path)).await;
        assert!(
            found
                .problem
                .as_deref()
                .unwrap()
                .contains("older than the 0.143.0"),
            "{found:?}"
        );

        let path = fake_cli(temporary.path(), "codex", "exit 3");
        let found = detect::detect_codex(Some(&path)).await;
        assert!(
            found.problem.as_deref().unwrap().contains("it exited with"),
            "{found:?}"
        );
    }

    #[tokio::test]
    async fn a_cli_that_hangs_is_stopped_with_what_it_started() {
        let temporary = tempfile::tempdir().unwrap();
        let marker = temporary.path().join("still-running");
        let path = fake_cli(
            temporary.path(),
            "codex",
            &format!("(sleep 5; touch '{}') &\nsleep 30", marker.display()),
        );
        let started = std::time::Instant::now();
        let found = detect::detect_codex(Some(&path)).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(
            found
                .problem
                .as_deref()
                .unwrap()
                .contains("did not finish in time"),
            "{found:?}"
        );
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        assert!(!marker.exists(), "the probe's children were stopped too");
    }

    #[tokio::test]
    async fn claude_reports_whether_it_is_signed_in() {
        let temporary = tempfile::tempdir().unwrap();
        for (status, exit, signed_in) in [
            ("true", 0, Some(true)),
            ("false", 1, Some(false)),
            ("true", 1, None),
        ] {
            let path = fake_cli(
                temporary.path(),
                "claude",
                &format!(
                    "if [ \"$1\" = --version ]; then echo '2.0.14 (Claude Code)'; exit 0; fi\n\
                     echo '{{\"loggedIn\": {status}, \"email\": \"someone@example.com\"}}'; exit {exit}"
                ),
            );
            let found = detect::detect_claude(Some(&path)).await;
            assert_eq!(found.version.as_deref(), Some("2.0.14 (Claude Code)"));
            assert_eq!(found.signed_in, signed_in, "{status} {exit}");
            assert_eq!(found.problem, None);
        }
    }
}
