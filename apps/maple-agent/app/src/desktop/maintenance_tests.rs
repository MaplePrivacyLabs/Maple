//! The desktop must recover its existing chat after password maintenance even
//! when IPC stays connected. This contained host speaks the production framed
//! protocol; only database/password work and inference are synthetic.
use super::*;
use crate::host::{RemoteClient, ipc, protocol};
use maple_agent::agent::{
    AgentRuntimeStatus, AgentServiceEvent, AgentSessionSummary, AgentTaskState,
};
use maple_agent::store::StoreState;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;

const TASK: &str = "maintenance-fixture-task";
const DRAFT: &str = "Keep this unsent draft";

enum HostCommand {
    Finish(Result<Value, String>, StoreState, &'static str),
    State(StoreState),
    ReleaseFlush,
    AutomaticDownloads(bool),
}

struct FixtureHost {
    backend: Arc<AgentBackend>,
    commands: mpsc::UnboundedSender<HostCommand>,
    requests: Arc<Mutex<Vec<String>>>,
    runtime: Option<tokio::runtime::Runtime>,
    _root: tempfile::TempDir,
}

impl Drop for FixtureHost {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(5));
        }
    }
}

fn runtime_status(running: bool) -> AgentRuntimeStatus {
    AgentRuntimeStatus {
        running,
        project_root: Some(
            std::env::temp_dir()
                .join("maple-maintenance-project")
                .to_string_lossy()
                .into_owned(),
        ),
        model: None,
        active_runs: HashMap::new(),
    }
}

fn selected_task() -> AgentSessionSummary {
    AgentSessionSummary {
        id: TASK.into(),
        title: "Existing encrypted chat".into(),
        project_root: runtime_status(false).project_root.unwrap(),
        created_ms: 1,
        updated_ms: 1,
        message_count: 0,
        model: None,
        web_enabled: true,
        state: AgentTaskState::Active,
        acp: false,
    }
}

impl FixtureHost {
    fn new() -> Self {
        Self::with_runtime_outcome(false, false)
    }

    fn with_runtime_outcome(fail_start: bool, runtime_survives: bool) -> Self {
        Self::with_close_outcome(fail_start, runtime_survives, "password", false)
    }

    fn with_close_outcome(
        fail_start: bool,
        runtime_survives: bool,
        mode: &'static str,
        hold_flush: bool,
    ) -> Self {
        #[cfg(unix)]
        let root = tempfile::Builder::new()
            .prefix("maple-mnt-")
            .tempdir_in("/tmp")
            .unwrap();
        #[cfg(windows)]
        let root = tempfile::tempdir().unwrap();
        let endpoint = ipc::Endpoint::for_test(root.path().to_path_buf());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let (commands, mut control) = mpsc::unbounded_channel();
        let backend = runtime.block_on(async {
            let mut listener = endpoint.bind().unwrap();
            let connecting = endpoint.connect();
            let (connection, accepted) = tokio::join!(connecting, listener.accept());
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
                let (mut input, mut output) = tokio::io::split(accepted);
                // Commands may arrive during a frame. Keep reads in their own
                // task so select! cannot discard a partially consumed request.
                let (incoming, mut received) = mpsc::channel(32);
                let reader = tokio::spawn(async move {
                    while let Ok(request) = protocol::read::<protocol::Request>(&mut input).await {
                        if incoming.send(request).await.is_err() {
                            break;
                        }
                    }
                });
                let mut state = StoreState::Open;
                let mut store_id = "original";
                let mut pending = None;
                let mut running = runtime_survives;
                let mut pending_flush = None;
                let mut automatic_downloads = true;
                loop {
                    tokio::select! {
                        command = control.recv() => {
                            let Some(command) = command else { break };
                            match command {
                                HostCommand::Finish(result, next, generation) => {
                                    state = next;
                                    store_id = generation;
                                    let id = pending.take().expect("an admitted password operation");
                                    if protocol::write(&mut output, &protocol::Message::Response { id, result }).await.is_err() { break; }
                                }
                                HostCommand::State(next) => state = next,
                                HostCommand::AutomaticDownloads(enabled) => automatic_downloads = enabled,
                                HostCommand::ReleaseFlush => {
                                    let id = pending_flush.take().expect("a deferred settings flush");
                                    if protocol::write(&mut output, &protocol::Message::Response { id, result: Ok(Value::Null) }).await.is_err() { break; }
                                    continue;
                                }
                            }
                            if protocol::write(&mut output, &protocol::Message::Event(Box::new(AgentServiceEvent::StoreStateChanged { state: state.clone() }))).await.is_err() { break; }
                        }
                        request = received.recv() => {
                            let Some(request) = request else { break };
                            recorded.lock().unwrap().push(request.method.clone());
                            if matches!(request.method.as_str(), "set_store_password" | "change_store_password" | "remove_store_password") {
                                assert!(pending.replace(request.id).is_none());
                                running = runtime_survives;
                                state = StoreState::Maintenance;
                                if protocol::write(&mut output, &protocol::Message::Event(Box::new(AgentServiceEvent::StoreStateChanged { state: state.clone() }))).await.is_err() { break; }
                                continue;
                            }
                            if request.method == "flush_ui_settings" && hold_flush {
                                assert!(pending_flush.replace(request.id).is_none());
                                continue;
                            }
                            let result = match request.method.as_str() {
                                "host_status" => Ok(json!({"acp_clients":1,"running_tasks":0,"remote_controllers":0,"background_tasks":0})),
                                "store_status" => Ok(json!({"state":state,"mode":mode,"store_id":store_id,"connection_rebind_required":false,"password_settings_available":true})),
                                "get_ui_settings" => {
                                    let settings = crate::settings::AppSettings {
                                        automatic_update_downloads: automatic_downloads,
                                        ..Default::default()
                                    };
                                    Ok(serde_json::to_value(settings).unwrap())
                                },
                                "dictionary" => Ok(json!({"words":[],"next":null})),
                                "navigation_bootstrap" => Ok(serde_json::to_value(crate::backend::NavigationBootstrap {
                                    runtime_status: runtime_status(running), project_root: runtime_status(running).project_root,
                                    recent_projects: Vec::new(), projects_next: None, default_project: None,
                                    pending_questions: Vec::new(), persistence_status: Default::default(),
                                }).unwrap()),
                                "subscribe" | "set_ui_settings" | "flush_ui_settings" | "cancel_oauth" | "detach" => Ok(Value::Null),
                                "open_task" => Ok(serde_json::to_value(crate::backend::PagedTaskDetail {
                                    session: selected_task(), timeline: Vec::new(), timeline_revision: 0,
                                    before: None, rows_epoch: 1, plan: None, summaries: HashMap::new(),
                                    queue: maple_agent::agent::AgentDesktopQueueSnapshot { revision: 0, items: Vec::new() },
                                    queue_previews: Vec::new(), mcp_errors: Vec::new(), live_has_more: false,
                                }).unwrap()),
                                "start_runtime" => {
                                    assert_eq!(state, StoreState::Open, "runtime start must follow verified Open");
                                    if fail_start {
                                        Err("Synthetic runtime restart failure".into())
                                    } else {
                                        running = true;
                                        Ok(serde_json::to_value(runtime_status(true)).unwrap())
                                    }
                                }
                                "send_message" if running && state == StoreState::Open => Ok(json!("explicit-fixture-run")),
                                "send_message" => Err("Agent runtime is not running".into()),
                                "cancel_store_operation" => Ok(json!(pending.is_some())),
                                "list_tasks" | "pinned_tasks" | "get_tasks" => Ok(json!({"tasks":[]})),
                                "list_projects" => Ok(json!({"projects":[]})),
                                "list_project_groups" => Ok(json!({"groups":[],"archived_count":0})),
                                "list_slash_commands" | "recent_project_roots" => Ok(json!([])),
                                "restore_auth" => Ok(json!("Unavailable")),
                                // These optional presentation refreshes do not belong to the
                                // readiness regression and must never contact a real service.
                                _ => Err("Optional fixture operation unavailable".into()),
                            };
                            if protocol::write(&mut output, &protocol::Message::Response { id:request.id, result }).await.is_err() { break; }
                        }
                    }
                }
                reader.abort();
            });
            Arc::new(AgentBackend::from_host_client_for_test(RemoteClient::attach_for_test(endpoint, connection, welcome)))
        });
        Self {
            backend,
            commands,
            requests,
            runtime: Some(runtime),
            _root: root,
        }
    }

    fn count(&self, method: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|item| item.as_str() == method)
            .count()
    }

    fn begin(&self, method: &'static str) -> tokio::task::JoinHandle<Result<Value, String>> {
        let backend = self.backend.clone();
        self.backend
            .spawn(async move { backend.local_control(method, json!({})).await })
    }

    fn finish(&self, error: Option<&str>) {
        self.commands
            .send(HostCommand::Finish(
                error.map_or_else(|| Ok(json!({})), |message| Err(message.into())),
                StoreState::Open,
                "original",
            ))
            .unwrap();
    }
}

fn shell(
    host: &FixtureHost,
    cx: &mut gpui::TestAppContext,
    busy: bool,
    settings: &parking_lot::MutexGuard<'_, ()>,
) -> Entity<MapleApp> {
    let app = super::tests::password_settings_shell_with_backend(
        cx,
        busy,
        host.backend.clone(),
        settings,
    );
    app.update(cx, |app, cx| {
        app.local_chat.as_ref().unwrap().update(cx, |chat, cx| {
            chat.seed_store_maintenance_for_test(Some(TASK.into()), DRAFT, cx)
        });
        let backend = app.backend.clone();
        let source = backend.clone();
        let rx = backend.spawn(async move { source.take_events().await });
        cx.spawn(async move |this, cx| {
            let Some(mut rx) = rx.await.ok().flatten() else {
                return;
            };
            while let Some(event) = rx.recv().await {
                let mut batch = vec![event];
                while batch.len() < 256 {
                    let Ok(next) = rx.try_recv() else { break };
                    batch.push(next);
                }
                if this
                    .update(cx, |app, cx| app.handle_service_events(batch, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    });
    app
}

struct AppHost(Entity<MapleApp>);
impl Render for AppHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

fn window<'a>(
    app: &Entity<MapleApp>,
    cx: &'a mut gpui::TestAppContext,
) -> &'a mut gpui::VisualTestContext {
    let (_, cx) = cx.add_window_view(|_, _| AppHost(app.clone()));
    cx.simulate_resize(size(px(1100.), px(850.)));
    cx
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

fn chat_ready(app: &Entity<MapleApp>, cx: &mut gpui::VisualTestContext) -> bool {
    app.update(cx, |app, cx| {
        app.local_chat.as_ref().is_some_and(|chat| {
            chat.update(cx, |chat, cx| {
                let (_, _, booting, error) = chat.store_maintenance_snapshot_for_test(cx);
                !booting && error.is_none()
            })
        })
    })
}

fn settle(cx: &mut gpui::VisualTestContext) {
    // RemoteClient and the UI executor run on different schedulers. Let
    // duplicate pushes cross the socket before checking request counts.
    for _ in 0..10 {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_preserved(
    app: &Entity<MapleApp>,
    chat: &Entity<ChatScreen>,
    host: &FixtureHost,
    cx: &mut gpui::VisualTestContext,
) {
    app.update(cx, |app, cx| {
        assert!(matches!(app.screen, Screen::Settings(_)));
        assert_eq!(
            app.local_chat.as_ref().unwrap().entity_id(),
            chat.entity_id()
        );
        assert_eq!(app.user_id.as_deref(), Some("user"));
        assert_eq!(app.store_id.as_deref(), Some("original"));
        chat.update(cx, |chat, cx| {
            let (selected, draft, booting, error) = chat.store_maintenance_snapshot_for_test(cx);
            assert_eq!(selected.as_deref(), Some(TASK));
            assert_eq!(draft.as_deref(), Some(DRAFT));
            assert!(!booting);
            assert!(error.is_none(), "{error:?}");
        });
    });
    assert_eq!(host.count("start_runtime"), 1);
    assert_eq!(
        host.count("send_message"),
        0,
        "recovery cannot replay a prompt"
    );
    assert_eq!(
        host.count("create_session"),
        0,
        "recovery cannot create a new chat"
    );
}

fn assert_explicit_send_ready(host: &FixtureHost, cx: &mut gpui::VisualTestContext) {
    let backend = host.backend.clone();
    let send = host.backend.spawn(async move {
        backend
            .send_message_identified(
                "user",
                maple_agent::agent::AgentSendMessageRequest {
                    session_id: TASK.into(),
                    text: "Explicit post-maintenance request".into(),
                    model: None,
                    context_limit: None,
                    vision_capable: false,
                    steer: false,
                    queue_id: None,
                    attachments: Vec::new(),
                },
                uuid::Uuid::new_v4().to_string(),
            )
            .await
    });
    wait(cx, "explicit post-maintenance send", |_| send.is_finished());
    assert_eq!(
        host.runtime
            .as_ref()
            .unwrap()
            .block_on(send)
            .unwrap()
            .unwrap(),
        "explicit-fixture-run"
    );
    assert_eq!(host.count("send_message"), 1);
}

#[gpui::test]
fn successful_password_operations_restart_once_and_preserve_parked_chat(
    cx: &mut gpui::TestAppContext,
) {
    let settings = crate::settings::SETTINGS_IO_LOCK.lock();
    for method in [
        "set_store_password",
        "change_store_password",
        "remove_store_password",
    ] {
        let host = FixtureHost::new();
        let app = shell(&host, cx, true, &settings);
        let chat = app.read_with(cx, |app, _| app.local_chat.clone().unwrap());
        let cx = window(&app, cx);
        let operation = host.begin(method);
        wait(cx, "password maintenance", |cx| {
            app.update(cx, |app, cx| {
                app.password_settings_refresh_pending
                    && chat.update(cx, |chat, cx| {
                        chat.store_maintenance_snapshot_for_test(cx).2
                    })
            })
        });
        host.finish(None);
        wait(cx, "runtime recovery", |cx| {
            operation.is_finished() && host.count("start_runtime") == 1 && chat_ready(&app, cx)
        });
        // Repeated final-state pushes must not start a second recovery.
        host.commands
            .send(HostCommand::State(StoreState::Open))
            .unwrap();
        settle(cx);
        assert_preserved(&app, &chat, &host, cx);
        assert_explicit_send_ready(&host, cx);
        drop(host);
        settle(cx);
    }
}

#[gpui::test]
fn rejected_password_and_accepted_cancellation_restore_runtime(cx: &mut gpui::TestAppContext) {
    let settings = crate::settings::SETTINGS_IO_LOCK.lock();
    for (method, cancelled) in [
        ("change_store_password", false),
        ("remove_store_password", false),
        ("change_store_password", true),
    ] {
        let host = FixtureHost::new();
        let app = shell(&host, cx, true, &settings);
        let chat = app.read_with(cx, |app, _| app.local_chat.clone().unwrap());
        let cx = window(&app, cx);
        let operation = host.begin(method);
        wait(cx, "rejected operation maintenance barrier", |cx| {
            app.update(cx, |app, cx| {
                app.password_settings_refresh_pending
                    && chat.update(cx, |chat, cx| {
                        chat.store_maintenance_snapshot_for_test(cx).2
                    })
            })
        });
        if cancelled {
            let control = host.backend.clone();
            let cancel = host.backend.spawn(async move {
                control
                    .local_control::<bool>("cancel_store_operation", ())
                    .await
            });
            wait(cx, "accepted cancellation", |_| cancel.is_finished());
            assert!(
                host.runtime
                    .as_ref()
                    .unwrap()
                    .block_on(cancel)
                    .unwrap()
                    .unwrap()
            );
        }
        host.finish(Some(if cancelled {
            "The local store operation was cancelled."
        } else {
            "Incorrect current unlock password."
        }));
        wait(cx, "failed operation runtime recovery", |cx| {
            operation.is_finished() && host.count("start_runtime") == 1 && chat_ready(&app, cx)
        });
        assert!(
            host.runtime
                .as_ref()
                .unwrap()
                .block_on(operation)
                .unwrap()
                .is_err()
        );
        assert_preserved(&app, &chat, &host, cx);
        assert_explicit_send_ready(&host, cx);
        drop(host);
        settle(cx);
    }
}

#[gpui::test]
fn closing_an_unsubmitted_password_form_does_not_restart_or_send(cx: &mut gpui::TestAppContext) {
    let settings = crate::settings::SETTINGS_IO_LOCK.lock();
    let host = FixtureHost::new();
    let app = shell(&host, cx, false, &settings);
    app.update(cx, |app, cx| {
        let Screen::Settings(settings) = &app.screen else {
            panic!("settings")
        };
        settings.update(cx, |settings, _| settings.set_password_form_for_test(None));
    });
    let cx = window(&app, cx);
    host.commands
        .send(HostCommand::State(StoreState::Open))
        .unwrap();
    wait(cx, "unsubmitted form close", |cx| chat_ready(&app, cx));
    settle(cx);
    assert_eq!(host.count("navigation_bootstrap"), 0);
    assert_eq!(host.count("start_runtime"), 0);
    assert_eq!(host.count("send_message"), 0);
    drop(host);
    settle(cx);
}

#[gpui::test]
fn locked_store_retires_old_chat_without_runtime_recovery(cx: &mut gpui::TestAppContext) {
    let settings = crate::settings::SETTINGS_IO_LOCK.lock();
    let host = FixtureHost::new();
    let app = shell(&host, cx, true, &settings);
    let cx = window(&app, cx);
    let operation = host.begin("change_store_password");
    wait(cx, "maintenance", |cx| {
        app.update(cx, |app, _| app.password_settings_refresh_pending)
    });
    host.commands
        .send(HostCommand::Finish(
            Err("Store locked during maintenance".into()),
            StoreState::Locked,
            "original",
        ))
        .unwrap();
    wait(cx, "locked gate", |cx| {
        operation.is_finished()
            && app.update(cx, |app, _| matches!(app.screen, Screen::StoreGate(_)))
    });
    assert!(operation.is_finished());
    app.update(cx, |app, _| {
        assert!(app.local_chat.is_none());
        assert!(app.parked_chat.is_none());
        assert!(app.user_id.is_none());
    });
    assert_eq!(host.count("start_runtime"), 0);
    drop(host);
    settle(cx);
}

#[gpui::test]
fn password_maintenance_recovers_local_chat_when_a_remote_chat_is_parked(
    cx: &mut gpui::TestAppContext,
) {
    let settings = crate::settings::SETTINGS_IO_LOCK.lock();
    let host = FixtureHost::new();
    let app = shell(&host, cx, true, &settings);
    let (local, remote) = app.update(cx, |app, cx| {
        let local = app.local_chat.clone().unwrap();
        let remote = app.remote_computers["east"].chat.clone().unwrap();
        remote.update(cx, |chat, cx| {
            chat.seed_store_maintenance_for_test(
                Some("remote-task".into()),
                "Remote unsent draft",
                cx,
            )
        });
        app.selected_host = Some("east".into());
        app.parked_chat = Some(remote.clone());
        (local, remote)
    });
    let cx = window(&app, cx);
    let operation = host.begin("set_store_password");
    wait(cx, "local maintenance", |cx| {
        local.update(cx, |chat, cx| {
            chat.store_maintenance_snapshot_for_test(cx).2
        })
    });
    host.finish(None);
    wait(cx, "local recovery behind remote Settings", |cx| {
        operation.is_finished() && host.count("start_runtime") == 1 && chat_ready(&app, cx)
    });
    assert_preserved(&app, &local, &host, cx);
    app.update(cx, |app, cx| {
        assert_eq!(app.selected_host.as_deref(), Some("east"));
        assert_eq!(
            app.parked_chat.as_ref().unwrap().entity_id(),
            remote.entity_id()
        );
        remote.update(cx, |chat, cx| {
            let (selected, draft, booting, error) = chat.store_maintenance_snapshot_for_test(cx);
            assert_eq!(selected.as_deref(), Some("remote-task"));
            assert_eq!(draft.as_deref(), Some("Remote unsent draft"));
            assert!(!booting);
            assert!(error.is_none());
        });
    });
    assert_eq!(
        host.count("navigation_bootstrap"),
        1,
        "only this computer's chat recovers"
    );
    drop(host);
    settle(cx);
}

#[gpui::test]
fn failed_runtime_recovery_preserves_encrypted_store_and_unsent_draft(
    cx: &mut gpui::TestAppContext,
) {
    let settings = crate::settings::SETTINGS_IO_LOCK.lock();
    let host = FixtureHost::with_runtime_outcome(true, false);
    let app = shell(&host, cx, true, &settings);
    let chat = app.read_with(cx, |app, _| app.local_chat.clone().unwrap());
    let cx = window(&app, cx);
    let operation = host.begin("set_store_password");
    wait(cx, "maintenance before restart failure", |cx| {
        chat.update(cx, |chat, cx| {
            chat.store_maintenance_snapshot_for_test(cx).2
        })
    });
    host.finish(None);
    wait(cx, "runtime recovery failure", |cx| {
        chat.update(cx, |chat, cx| {
            let (_, _, booting, error) = chat.store_maintenance_snapshot_for_test(cx);
            !booting
                && error
                    .is_some_and(|message| message.contains("Synthetic runtime restart failure"))
        })
    });
    assert!(
        host.runtime
            .as_ref()
            .unwrap()
            .block_on(operation)
            .unwrap()
            .is_ok(),
        "password change already committed"
    );
    app.update(cx, |app, cx| {
        assert_eq!(app.backend.store_state(), StoreState::Open);
        assert_eq!(app.store_id.as_deref(), Some("original"));
        assert_eq!(
            app.local_chat.as_ref().unwrap().entity_id(),
            chat.entity_id()
        );
        assert!(matches!(app.screen, Screen::Settings(_)));
        chat.update(cx, |chat, cx| {
            let (selected, draft, _, _) = chat.store_maintenance_snapshot_for_test(cx);
            assert_eq!(selected.as_deref(), Some(TASK));
            assert_eq!(draft.as_deref(), Some(DRAFT));
        });
    });
    assert_eq!(host.count("start_runtime"), 1);
    assert_eq!(host.count("send_message"), 0);
    drop(host);
    settle(cx);
}

#[gpui::test]
fn rejection_before_runtime_shutdown_clears_barrier_without_restarting(
    cx: &mut gpui::TestAppContext,
) {
    let settings = crate::settings::SETTINGS_IO_LOCK.lock();
    let host = FixtureHost::with_runtime_outcome(false, true);
    let app = shell(&host, cx, true, &settings);
    let chat = app.read_with(cx, |app, _| app.local_chat.clone().unwrap());
    let cx = window(&app, cx);
    let operation = host.begin("change_store_password");
    wait(cx, "maintenance before early rejection", |cx| {
        chat.update(cx, |chat, cx| {
            chat.store_maintenance_snapshot_for_test(cx).2
        })
    });
    host.finish(Some("Wait for running tasks to finish."));
    wait(cx, "surviving runtime readiness", |cx| {
        operation.is_finished() && chat_ready(&app, cx)
    });
    app.update(cx, |app, cx| {
        assert!(matches!(app.screen, Screen::Settings(_)));
        assert_eq!(
            app.local_chat.as_ref().unwrap().entity_id(),
            chat.entity_id()
        );
        chat.update(cx, |chat, cx| {
            let (selected, draft, booting, error) = chat.store_maintenance_snapshot_for_test(cx);
            assert_eq!(selected.as_deref(), Some(TASK));
            assert_eq!(draft.as_deref(), Some(DRAFT));
            assert!(!booting);
            assert!(error.is_none());
        });
    });
    assert_eq!(host.count("navigation_bootstrap"), 1);
    assert_eq!(host.count("start_runtime"), 0);
    assert_eq!(host.count("send_message"), 0);
    assert_explicit_send_ready(&host, cx);
    drop(host);
    settle(cx);
}

fn close_lifecycle(host: &FixtureHost) -> DesktopLifecycle {
    DesktopLifecycle {
        backend: host.backend.clone(),
        updater: crate::update::Controller::new(false),
        remote_enabled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        store_resident: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        can_background: false,
        closing: Default::default(),
        quit_completed: Default::default(),
        quit_requested: Default::default(),
        frontend_detached: Default::default(),
        #[cfg(target_os = "windows")]
        update_restart_pending: Default::default(),
    }
}

#[gpui::test]
fn updater_bootstrap_waits_for_open_store_and_uses_saved_download_preference(
    cx: &mut gpui::TestAppContext,
) {
    let settings = crate::settings::SETTINGS_IO_LOCK.lock();
    let host = FixtureHost::new();
    let app = shell(&host, cx, false, &settings);
    let updater = crate::update::Controller::new(false);
    updater.set_auto_download(true);
    app.update(cx, |app, _| app.updater = updater.clone());
    let cx = window(&app, cx);
    host.commands
        .send(HostCommand::State(StoreState::Locked))
        .unwrap();
    wait(cx, "locked store gate", |cx| {
        app.update(cx, |app, _| matches!(app.screen, Screen::StoreGate(_)))
    });
    assert_eq!(updater.startup_policy_for_test(), (false, false));
    host.commands
        .send(HostCommand::AutomaticDownloads(false))
        .unwrap();
    host.commands
        .send(HostCommand::State(StoreState::Open))
        .unwrap();
    wait(cx, "verified settings bootstrap", |cx| {
        app.update(cx, |app, _| app.store_ready) && updater.startup_policy_for_test().0
    });
    assert!(!updater.startup_policy_for_test().1);
    assert!(host.count("get_ui_settings") > 0);
    updater.shutdown();
    drop(host);
    settle(cx);
}

#[gpui::test]
fn pending_quit_cannot_finish_an_update_installation(cx: &mut gpui::TestAppContext) {
    cx.executor().allow_parking();
    let backend =
        Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
    let updater = crate::update::Controller::new(false);
    cx.update(|cx| {
        cx.set_global(DesktopLifecycle {
            backend,
            updater: updater.clone(),
            remote_enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            store_resident: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            can_background: true,
            closing: Default::default(),
            quit_completed: Default::default(),
            quit_requested: Default::default(),
            frontend_detached: Default::default(),
            #[cfg(target_os = "windows")]
            update_restart_pending: Default::default(),
        });
        updater.set_test_state(crate::update::State::Ready);
        assert!(may_finish_quit(cx));
        updater.set_test_state(crate::update::State::Installing);
        assert!(!may_finish_quit(cx));
        assert!(!cx.global::<DesktopLifecycle>().quit_completed.get());
        updater.set_test_state(crate::update::State::Error);
        assert!(may_finish_quit(cx));
    });
}

#[gpui::test]
fn committed_restart_retry_quit_and_close_do_not_contact_the_retired_host(
    cx: &mut gpui::TestAppContext,
) {
    let settings = crate::settings::SETTINGS_IO_LOCK.lock();
    let host = FixtureHost::with_close_outcome(false, true, "plain", false);
    let app = shell(&host, cx, false, &settings);
    let lifecycle = close_lifecycle(&host);
    let path = PathBuf::from("/Applications/Maple Agent.app");
    lifecycle.backend.retire_after_update();
    lifecycle.updater.set_test_installed(path.clone());
    app.update(cx, |app, cx| {
        app.updater = lifecycle.updater.clone();
        cx.set_global(lifecycle.clone());
    });
    for _ in 0..2 {
        let mut restarted = cx.expect_restart();
        cx.update(|cx| restart_installed_update(path.clone(), cx));
        assert_eq!(restarted.try_recv().unwrap(), Some(Some(path.clone())));
    }
    let cx = window(&app, cx);
    host.requests.lock().unwrap().clear();
    cx.update(close_window);
    cx.update(|_, cx| quit_app(cx));
    settle(cx);
    assert!(lifecycle.quit_completed.get());
    assert!(!lifecycle.closing.get());
    assert!(!lifecycle.frontend_detached.get());
    for method in [
        "store_status",
        "host_status",
        "flush_ui_settings",
        "cancel_oauth",
        "detach",
    ] {
        assert_eq!(
            host.count(method),
            0,
            "terminal update must bypass {method}"
        );
    }
    assert_eq!(
        lifecycle.updater.snapshot().state,
        crate::update::State::RestartRequired
    );
    #[cfg(target_os = "macos")]
    native_quit::reset_frontend_detach_for_test();
    drop(host);
    settle(cx);
}

#[gpui::test]
fn plain_window_close_detaches_without_stopping_other_clients(cx: &mut gpui::TestAppContext) {
    let settings = crate::settings::SETTINGS_IO_LOCK.lock();
    let settings_root = tempfile::tempdir().unwrap();
    let settings_store = Arc::new(
        maple_agent::store::Store::open(maple_agent::store::StorePaths::new(settings_root.path()))
            .unwrap(),
    );
    crate::settings::install_test_store(settings_store.clone());
    let host = FixtureHost::with_close_outcome(false, true, "plain", false);
    let app = shell(&host, cx, false, &settings);
    let lifecycle = close_lifecycle(&host);
    app.update(cx, |_, cx| cx.set_global(lifecycle.clone()));
    let cx = window(&app, cx);
    cx.update(close_window);
    wait(cx, "intentional frontend detach", |_| {
        host.count("detach") == 1
    });
    assert!(lifecycle.frontend_detached.get());
    assert!(
        !lifecycle.quit_completed.get(),
        "detach never proves host ownership release"
    );
    assert_eq!(
        host.count("host_status"),
        0,
        "closing cannot enter explicit Quit confirmation"
    );
    assert_eq!(host.count("flush_ui_settings"), 1);
    assert_eq!(host.count("cancel_oauth"), 1);
    // Once detach commits, a late Quit does not turn closing this frontend
    // into a profile-wide stop that ends other clients' work.
    cx.update(|_, cx| quit_app(cx));
    settle(cx);
    assert_eq!(host.count("host_status"), 0);
    assert!(!lifecycle.quit_completed.get());
    #[cfg(target_os = "macos")]
    native_quit::reset_frontend_detach_for_test();
    drop(host);
    settle(cx);
    crate::settings::discard_decrypted_cache();
    settings_store.close().unwrap();
}

#[gpui::test]
fn explicit_quit_before_soft_detach_commit_keeps_host_stop_priority(cx: &mut gpui::TestAppContext) {
    let settings = crate::settings::SETTINGS_IO_LOCK.lock();
    let settings_root = tempfile::tempdir().unwrap();
    let settings_store = Arc::new(
        maple_agent::store::Store::open(maple_agent::store::StorePaths::new(settings_root.path()))
            .unwrap(),
    );
    crate::settings::install_test_store(settings_store.clone());
    let host = FixtureHost::with_close_outcome(false, true, "plain", true);
    let app = shell(&host, cx, false, &settings);
    let lifecycle = close_lifecycle(&host);
    app.update(cx, |_, cx| cx.set_global(lifecycle.clone()));
    let cx = window(&app, cx);
    cx.update(close_window);
    wait(cx, "soft-close save barrier", |_| {
        host.count("flush_ui_settings") == 1
    });
    cx.update(|_, cx| quit_app(cx));
    assert!(lifecycle.quit_requested.get());
    assert!(!lifecycle.frontend_detached.get());
    host.commands.send(HostCommand::ReleaseFlush).unwrap();
    wait(cx, "explicit Quit activity check", |_| {
        host.count("host_status") == 1
    });
    settle(cx);
    assert_eq!(host.count("detach"), 0);
    assert!(!lifecycle.frontend_detached.get());
    assert!(!lifecycle.quit_completed.get());
    // The fixture reports another attached editor. Its explicit Quit now
    // waits for confirmation; the test never approves stopping any host.
    drop(host);
    settle(cx);
    crate::settings::discard_decrypted_cache();
    settings_store.close().unwrap();
}
