//! Local API controls must never become selected-computer operations.

use super::*;
use crate::local_api::LocalApiConfig;

fn fixture() -> Arc<AgentBackend> {
    Arc::new(AgentBackend::for_test("http://127.0.0.1:1".into(), String::new()).unwrap())
}

#[test]
fn selected_remote_cannot_read_configure_or_stop_a_local_api() {
    let controller = fixture();
    let runtime = controller.runtime_handle();
    runtime.block_on(async {
        let remote = controller
            .with_remote_connection("fixture-peer".into(), || async {
                let mut backend = LocalBackend::new(controller.api_url.clone(), String::new())?;
                backend.ui_fixture = true;
                Ok(BackendClient::InProcess(Box::new(backend)))
            })
            .await
            .unwrap();

        for result in [
            remote.get_local_api_status().await,
            remote
                .set_local_api_config(
                    "fixture-user",
                    LocalApiConfig {
                        enabled: true,
                        port: 0,
                    },
                )
                .await,
            remote
                .set_local_api_config(
                    "fixture-user",
                    LocalApiConfig {
                        enabled: false,
                        port: 0,
                    },
                )
                .await,
            remote.stop_local_api().await,
        ] {
            let error = result.expect_err("a selected remote must not receive API controls");
            assert!(
                error.to_ascii_lowercase().contains("local"),
                "the facade must reject before validating a port or account: {error}"
            );
        }
    });
}

#[test]
fn installation_retirement_rejects_local_api_enable_before_account_or_port_validation() {
    let controller = fixture();
    let runtime = controller.runtime_handle();
    controller.retire_after_update();
    let result = runtime.block_on(controller.set_local_api_config(
        "fixture-user",
        LocalApiConfig {
            enabled: true,
            port: 0,
        },
    ));
    assert_eq!(result.err(), Some(RETIRED_AFTER_UPDATE.into()));
}
