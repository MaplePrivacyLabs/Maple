//! Actual Settings clicks and private IPC across three disposable profiles.
//! Account validation, persistence and unrelated presentation APIs are fixture effects;
//! every profile uses the production local HTTP manager and router.

use super::*;
use crate::host::{RemoteClient, ipc, protocol};
use crate::local_api::{LocalApiConfig, LocalApiManager, LocalApiState};
use maple_agent::agent::AgentServiceEvent;
use maple_agent::store::StoreState;
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Mutex, time::Duration};

struct FixtureHost {
    backend: Arc<AgentBackend>,
    manager: LocalApiManager,
    requests: Arc<Mutex<Vec<String>>>,
    status_holds: Arc<Mutex<VecDeque<tokio::sync::oneshot::Receiver<()>>>>,
    runtime: Option<tokio::runtime::Runtime>,
    _root: tempfile::TempDir,
}

impl FixtureHost {
    fn new(profile: &str, port: u16) -> Self {
        #[cfg(unix)]
        let root = tempfile::Builder::new()
            .prefix("maple-api-ui-")
            .tempdir_in("/tmp")
            .unwrap();
        #[cfg(windows)]
        let root = tempfile::tempdir().unwrap();
        let mut endpoint = ipc::Endpoint::for_test(root.path().to_path_buf());
        endpoint.profile = format!("local-api-ui-{profile}");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let status_holds = Arc::new(Mutex::new(
            VecDeque::<tokio::sync::oneshot::Receiver<()>>::new(),
        ));
        let pending_holds = status_holds.clone();
        let (backend, manager) = runtime.block_on(async {
            let manager = LocalApiManager::new("http://127.0.0.1:1".into()).unwrap();
            manager
                .configure(
                    LocalApiConfig {
                        enabled: false,
                        port,
                    },
                    "user".into(),
                )
                .await
                .unwrap();
            let service = manager.clone();
            let mut listener = endpoint.bind().unwrap();
            let (connection, accepted) = tokio::join!(endpoint.connect(), listener.accept());
            let connection = connection.unwrap();
            let accepted = accepted.unwrap();
            let welcome = protocol::Welcome {
                version: protocol::VERSION,
                pid: std::process::id(),
                build: protocol::BUILD.into(),
                user_id: Some("user".into()),
                store: StoreState::Open,
            };
            tokio::spawn(async move {
                let (mut input, output) = tokio::io::split(accepted);
                let output = Arc::new(tokio::sync::Mutex::new(output));
                while let Ok(request) = protocol::read::<protocol::Request>(&mut input).await {
                    recorded.lock().unwrap().push(request.method.clone());
                    if request.method == "get_local_api_status" {
                        let hold = pending_holds.lock().unwrap().pop_front();
                        if let Some(hold) = hold {
                            let response = protocol::Message::Response {
                                id: request.id,
                                result: Ok(json!(service.status())),
                            };
                            let output = output.clone();
                            tokio::spawn(async move {
                                let _ = hold.await;
                                let _ = protocol::write(&mut *output.lock().await, &response).await;
                            });
                            continue;
                        }
                    }
                    let result = match request.method.as_str() {
                        "get_local_api_status" => Ok(json!(service.status())),
                        "store_status" => Ok(json!({
                            "state":StoreState::Open,
                            "mode":"plain",
                            "store_id":"fixture",
                            "setup_operation_id":null,
                            "connection_rebind_required":false,
                            "persistence_status":maple_agent::store::PersistenceStatus::default(),
                            "local_api_running":service.stays_running(),
                        })),
                        "set_local_api_config" => {
                            // This boundary deliberately supplies successful
                            // account authorization without an external service.
                            let (user, config): (String, LocalApiConfig) =
                                serde_json::from_value(request.params).unwrap();
                            assert_eq!(user, "user");
                            service
                                .configure(config, user)
                                .await
                                .map(|status| json!(status))
                        }
                        "stop_local_api" => Ok(json!(service.stop().await)),
                        "cancel_oauth" | "set_ui_settings" | "flush_ui_settings" | "detach" => {
                            Ok(Value::Null)
                        }
                        _ => Err("Optional presentation fixture operation unavailable".into()),
                    };
                    if protocol::write(
                        &mut *output.lock().await,
                        &protocol::Message::Response {
                            id: request.id,
                            result,
                        },
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
                service.stop().await;
            });
            (
                Arc::new(AgentBackend::from_host_client_for_test(
                    RemoteClient::attach_for_test(endpoint, connection, welcome),
                )),
                manager,
            )
        });
        Self {
            backend,
            manager,
            requests,
            status_holds,
            runtime: Some(runtime),
            _root: root,
        }
    }

    fn count(&self, method: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|value| value.as_str() == method)
            .count()
    }

    fn api_calls(&self) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|value| value.contains("local_api"))
            .count()
    }

    fn hold_next_status(&self) -> tokio::sync::oneshot::Sender<()> {
        let (release, hold) = tokio::sync::oneshot::channel();
        self.status_holds.lock().unwrap().push_back(hold);
        release
    }
}

impl Drop for FixtureHost {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.block_on(self.manager.stop());
            runtime.shutdown_timeout(Duration::from_secs(3));
        }
    }
}

struct AppHost(Entity<MapleApp>);

impl Render for AppHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().w(px(1100.)).h(px(900.)).child(self.0.clone())
    }
}

fn wait(
    cx: &mut gpui::VisualTestContext,
    what: &str,
    mut condition: impl FnMut(&mut gpui::VisualTestContext) -> bool,
) {
    for _ in 0..500 {
        cx.run_until_parked();
        if condition(cx) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("Timed out waiting for {what}");
}

fn select_and_show_settings(app: &Entity<MapleApp>, host: &str, cx: &mut gpui::VisualTestContext) {
    app.update(cx, |app, cx| {
        if let Screen::Settings(settings) = &app.screen {
            let snapshot = settings.read(cx).settings_snapshot();
            app.close_settings(snapshot, cx);
        }
        let chat = app.remote_computers[host].chat.clone().unwrap();
        app.selected_host = Some(host.into());
        app.screen = Screen::Chat(chat);
        app.show_settings(Section::LocalApi, cx);
    });
    wait(cx, "current controller API status", |cx| {
        app.read_with(cx, |app, cx| match &app.screen {
            Screen::Settings(settings) => settings.read(cx).local_api_status_for_test().is_some(),
            _ => false,
        })
    });
}

fn toggle(cx: &mut gpui::VisualTestContext) {
    let bounds = cx
        .debug_bounds("local-api-enabled")
        .expect("local API switch");
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
}

fn assert_socket(port: u16, open: bool) {
    let result = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
        Duration::from_secs(1),
    );
    assert_eq!(result.is_ok(), open, "port {port}");
}

#[gpui::test]
fn selected_execution_computers_cannot_move_settings_listener_off_the_controller(
    cx: &mut gpui::TestAppContext,
) {
    let settings_lock = crate::settings::SETTINGS_IO_LOCK.lock();
    cx.executor().allow_parking();
    // Reserve all three before releasing any, so a port cannot be reused by
    // another fixture merely because the OS selected the same ephemeral port.
    let reservations: Vec<_> = (0..3)
        .map(|_| std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap())
        .collect();
    let ports: Vec<_> = reservations
        .iter()
        .map(|socket| socket.local_addr().unwrap().port())
        .collect();
    drop(reservations);
    let local = FixtureHost::new("controller", ports[0]);
    let east = FixtureHost::new("east", ports[1]);
    let west = FixtureHost::new("west", ports[2]);
    let app = super::tests::project_shell_with_backend(cx, local.backend.clone(), &settings_lock);
    app.update(cx, |app, cx| {
        for (host, backend) in [("east", &east.backend), ("west", &west.backend)] {
            let chat =
                cx.new(|cx| ChatScreen::new_without_start(backend.clone(), "user".into(), cx));
            app.remote_computers.get_mut(host).unwrap().chat = Some(chat);
        }
    });
    let (_, cx) = cx.add_window_view(|window, cx| {
        window.activate_window();
        crate::desktop::register_key_bindings(cx);
        AppHost(app.clone())
    });
    cx.simulate_resize(size(px(1100.), px(900.)));

    select_and_show_settings(&app, "east", cx);
    toggle(cx);
    wait(cx, "controller's real listening socket", |_| {
        local.manager.status().state == LocalApiState::Running
            && local.count("set_local_api_config") == 1
    });
    assert_socket(ports[0], true);
    assert_socket(ports[1], false);
    assert_socket(ports[2], false);
    assert_eq!(east.api_calls(), 0);
    assert_eq!(west.api_calls(), 0);

    select_and_show_settings(&app, "west", cx);
    toggle(cx);
    wait(cx, "controller's Off action", |_| {
        local.manager.status().state == LocalApiState::Stopped
            && local.count("set_local_api_config") == 2
    });
    assert!(!local.manager.status().config.enabled);
    assert_socket(ports[0], false);
    assert_socket(ports[1], false);
    assert_socket(ports[2], false);
    assert!(local.count("get_local_api_status") >= 2);
    assert_eq!(east.api_calls(), 0);
    assert_eq!(west.api_calls(), 0);
}

#[gpui::test]
fn desktop_coalesces_api_pushes_and_retires_old_host_callbacks(cx: &mut gpui::TestAppContext) {
    let settings_lock = crate::settings::SETTINGS_IO_LOCK.lock();
    cx.executor().allow_parking();
    let reservation = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let local = FixtureHost::new("refresh-controller", port);
    let app = super::tests::project_shell_with_backend(cx, local.backend.clone(), &settings_lock);
    let (_, cx) = cx.add_window_view(|window, _cx| {
        window.activate_window();
        AppHost(app.clone())
    });
    cx.simulate_resize(size(px(1100.), px(900.)));

    let first = local.hold_next_status();
    app.update(cx, |app, cx| {
        app.handle_service_events(vec![AgentServiceEvent::LocalApiChanged; 100], cx);
    });
    wait(cx, "one admitted status query", |_| {
        local.count("get_local_api_status") == 1
    });
    app.read_with(cx, |app, _| {
        assert!(app.local_api_refreshing);
        assert!(app.local_api_refresh_again);
    });
    first.send(()).unwrap();
    wait(cx, "one coalesced status query", |cx| {
        local.count("get_local_api_status") == 2
            && app.read_with(cx, |app, _| !app.local_api_refreshing)
    });
    assert_eq!(local.count("get_local_api_status"), 2);

    let old_reply = local.hold_next_status();
    let new_reply = local.hold_next_status();
    let old_bridge = app.update(cx, |app, cx| {
        let index = app.remote_bridges.borrow().len();
        app.local_api_revision = 100;
        app.refresh_local_api(cx);
        index
    });
    wait(cx, "old host's held query", |_| {
        local.count("get_local_api_status") == 3
    });
    let previous_epoch = app.read_with(cx, |app, _| app.local_api_host_epoch);
    app.update(cx, |app, cx| {
        app.handle_service_events(vec![AgentServiceEvent::HostReconnected], cx);
    });
    wait(cx, "replacement host's held query", |_| {
        local.count("get_local_api_status") == 4
    });
    old_reply.send(()).unwrap();
    wait(cx, "retired old-host callback", |cx| {
        app.read_with(cx, |app, _| {
            app.remote_bridges.borrow()[old_bridge].is_ready()
        })
    });
    app.read_with(cx, |app, _| {
        assert_eq!(app.local_api_host_epoch, previous_epoch + 1);
        assert_eq!(app.local_api_revision, 0);
        assert!(
            app.local_api_refreshing,
            "an old callback cannot clear the replacement host's in-flight query"
        );
        assert!(!app.local_api_refresh_again);
    });
    assert_eq!(local.count("get_local_api_status"), 4);
    new_reply.send(()).unwrap();
    wait(cx, "replacement host status", |cx| {
        app.read_with(cx, |app, _| {
            !app.local_api_refreshing && app.local_api_revision == local.manager.status().revision
        })
    });
    assert_eq!(local.count("get_local_api_status"), 4);
}
