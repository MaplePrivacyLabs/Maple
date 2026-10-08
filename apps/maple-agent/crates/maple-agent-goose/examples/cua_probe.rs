//! Manual probe for the embedded CUA runtime on this desktop session.
//!
//! Run it inside a real graphical session:
//!
//! ```sh
//! cargo run -p maple-agent-goose --example cua_probe
//! ```
//!
//! It reports which backend the SDK selected, then exercises window
//! enumeration and screen capture so a backend-selection fault is visible
//! without driving the whole application.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("cua_probe is a Linux desktop probe");
}

#[cfg(target_os = "linux")]
fn main() {
    // Exactly what the application does as the first statement of `main`.
    // SAFETY: no other thread has started yet.
    unsafe { maple_agent_goose::prepare_process_environment() };

    run();
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn run() {
    use cua_driver_sdk::{
        ConfiguredDriverOptions, CuaDriver, RuntimeAuthorizationOptions, SessionPermissionMode,
        TrustedSessionOptions,
    };

    for name in [
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "XDG_SESSION_TYPE",
        "CUA_DRIVER_RS_ENABLE_WAYLAND",
    ] {
        println!("{name}={:?}", std::env::var(name).ok());
    }

    match maple_agent_goose::agent::begin_integration_setup(
        &maple_agent_goose::agent::AgentSetupIntegrationRequest {
            id: "cua-driver".to_string(),
        },
    ) {
        Ok(permissions) => {
            println!("requirements ready={}", permissions.ready());
            for requirement in &permissions.required {
                println!(
                    "  {} granted={}",
                    requirement.kind.label(),
                    requirement.granted
                );
            }
        }
        Err(error) => println!("requirements unavailable: {error}"),
    }

    let driver = match CuaDriver::create_configured(ConfiguredDriverOptions {
        claude_code_compatibility: false,
        authorization: RuntimeAuthorizationOptions {
            allowed_modes: vec![SessionPermissionMode::Standard],
            compatibility_mode: SessionPermissionMode::Standard,
            compatibility_capability_manifest_path: None,
            compatibility_bounded_manifest_path: None,
            unrestricted_acknowledged: false,
            max_session_ttl_seconds: 3600,
            max_idle_ttl_seconds: 1800,
        },
    }) {
        Ok(driver) => driver,
        Err(error) => {
            println!("RUNTIME CREATE FAILED: {error}");
            return;
        }
    };

    let catalog = driver.list_tools_json().await.expect("tool catalog");
    let parsed: serde_json::Value = serde_json::from_str(&catalog).expect("catalog json");
    let names = parsed["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    println!("tools: {}", names.join(", "));

    let session = driver
        .create_trusted_session(TrustedSessionOptions {
            public_session: "cua-probe".to_string(),
            mode: SessionPermissionMode::Standard,
            ttl_seconds: 3600,
            idle_ttl_seconds: 1800,
            capability_manifest_path: None,
            bounded_manifest_path: None,
        })
        .expect("trusted session");

    let probes: Vec<(&str, serde_json::Value)> = match std::env::var("CUA_PROBE_ONLY") {
        Ok(only) => only
            .split(',')
            .map(|tool| {
                let args: serde_json::Value = std::env::var("CUA_PROBE_ARGS")
                    .ok()
                    .and_then(|raw| serde_json::from_str(&raw).ok())
                    .unwrap_or_else(|| serde_json::json!({}));
                (
                    Box::leak(tool.trim().to_string().into_boxed_str()) as &str,
                    args,
                )
            })
            .collect(),
        Err(_) => vec![
            ("get_screen_size", serde_json::json!({})),
            ("list_windows", serde_json::json!({})),
            ("list_apps", serde_json::json!({})),
            ("get_desktop_state", serde_json::json!({})),
        ],
    };
    for (tool, arguments) in probes {
        if !names.iter().any(|name| name == tool) {
            println!("\n== {tool}: not advertised ==");
            continue;
        }
        println!("\n== {tool} ==");
        match session
            .call_tool(tool.to_string(), arguments.to_string())
            .await
        {
            Ok(result) => summarize(&result.raw_json),
            Err(error) => println!("ERROR: {error}"),
        }
    }
}

/// Print the shape of a tool result without dumping a screenshot into stdout.
#[cfg(target_os = "linux")]
fn summarize(raw_json: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw_json) else {
        println!("unparseable result ({} bytes)", raw_json.len());
        return;
    };
    println!("is_error: {:?}", value["isError"].as_bool());
    if let Some(blocks) = value["content"].as_array() {
        for block in blocks {
            match block["type"].as_str() {
                Some("image") => println!(
                    "  image {} bytes, mime {:?}",
                    block["data"].as_str().map(str::len).unwrap_or(0),
                    block["mimeType"].as_str()
                ),
                Some("text") => {
                    let text = block["text"].as_str().unwrap_or_default();
                    let head = text.chars().take(6000).collect::<String>();
                    println!("  text: {head}");
                }
                other => println!("  block {other:?}"),
            }
        }
    }
    if let Some(structured) = value.get("structuredContent") {
        let windows = structured["windows"].as_array().map(Vec::len);
        println!("  structured windows: {windows:?}");
        if let Some(list) = structured["windows"].as_array() {
            for window in list.iter().take(8) {
                println!(
                    "    pid={:?} title={:?} bounds={:?}",
                    window["pid"], window["title"], window["bounds"]
                );
            }
        }
    }
}
