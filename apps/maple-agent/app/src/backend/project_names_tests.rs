//! Exercise the shared owner through the real local framing and Iroh request
//! streams. Network authentication/pairing has its own tests and live probes;
//! these fixtures use a saved synthetic account and loopback-only endpoints.
use super::*;
use crate::host::{ipc, protocol};
use crate::remote::{streams, wire};
use iroh::{Endpoint, RelayMode, endpoint::presets};
use serde_json::{Value, json};
use std::time::Duration;

const USER: &str = "project-name-owner";

fn owner(root: &std::path::Path) -> LocalBackend {
    let backend = LocalBackend::isolated_for_test(root);
    assert!(
        backend
            .persisted_auth
            .claim_and_persist(&MapleApiAuthSnapshot {
                user_id: USER.into(),
                access_token: "fixture-token".into(),
                refresh_token: None,
                native_instance_id: "fixture-native".into(),
                session_id: "fixture-session".into(),
                revision: 1,
            })
    );
    backend
}

fn request(method: &str, params: Value) -> protocol::Request {
    protocol::Request {
        id: 1,
        method: method.into(),
        send_id: None,
        params,
    }
}

fn project<'a>(
    roots: &'a [maple_agent::store::ProjectSummary],
    path: &str,
) -> &'a maple_agent::store::ProjectSummary {
    roots.iter().find(|root| root.root == path).unwrap()
}

async fn bootstrap(backend: &LocalBackend) -> crate::backend::paging::NavigationBootstrap {
    serde_json::from_value(
        crate::backend::dispatch(backend, request("navigation_bootstrap", json!([USER])))
            .await
            .unwrap(),
    )
    .unwrap()
}

async fn project_event(
    events: &mut mpsc::UnboundedReceiver<AgentServiceEvent>,
    path: &str,
    name: &str,
) -> AgentServiceEvent {
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            if let AgentServiceEvent::ProjectsChanged { user_id, roots } = &event {
                assert_eq!(user_id, USER);
                assert_eq!(roots.len(), 1, "project events carry only the changed root");
                if roots
                    .iter()
                    .any(|root| root.path == path && root.name == name)
                {
                    return event;
                }
            }
        }
        panic!("project change stream closed");
    })
    .await
    .expect("owner rename did not publish project metadata")
}

#[tokio::test]
async fn project_rename_ipc_preserves_account_boundary_and_recovery_metadata() {
    let storage = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let backend = owner(storage.path());
    let registration = backend
        .select_project_root(USER, folder.path().to_string_lossy().into_owned())
        .await
        .unwrap();
    let path = registration.project_root;
    let default_name = registration.project.name.clone();
    let mut events = backend.take_events().await.unwrap();

    // Unix socket paths need a short temp root; Windows uses an isolated pipe.
    #[cfg(unix)]
    let sockets = tempfile::Builder::new()
        .prefix("maple-names-")
        .tempdir_in("/tmp")
        .unwrap();
    #[cfg(windows)]
    let sockets = tempfile::tempdir().unwrap();
    let endpoint = ipc::Endpoint::for_test(sockets.path().to_path_buf());
    let mut listener = endpoint.bind().unwrap();
    let (client, server) = tokio::join!(endpoint.connect(), listener.accept());
    let mut client = client.unwrap();
    let mut server = server.unwrap();
    let command = request("rename_project_root", json!([USER, path, "Owner project"]));
    let send = async {
        protocol::write(&mut client, &command).await.unwrap();
        protocol::read::<protocol::Message>(&mut client)
            .await
            .unwrap()
    };
    let serve = async {
        let command: protocol::Request = protocol::read(&mut server).await.unwrap();
        let id = command.id;
        let result = crate::backend::dispatch(&backend, command).await;
        protocol::write(&mut server, &protocol::Message::Response { id, result })
            .await
            .unwrap();
    };
    let (message, ()) =
        tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(send, serve) })
            .await
            .unwrap();
    let protocol::Message::Response {
        result: Ok(value), ..
    } = message
    else {
        panic!("rename failed");
    };
    let renamed: RecentProjectRoot = serde_json::from_value(value).unwrap();
    assert_eq!(renamed.name, "Owner project");
    assert!(renamed.name_revision > 0);
    project_event(&mut events, &path, "Owner project").await;

    for method in [
        "rename_project_root",
        "navigation_bootstrap",
        "list_projects",
    ] {
        let params = match method {
            "rename_project_root" => json!(["another-account", path, "Wrong owner"]),
            "list_projects" => json!(["another-account", {"cursor": null, "limit": 1}]),
            _ => json!(["another-account"]),
        };
        assert!(
            crate::backend::dispatch(&backend, request(method, params))
                .await
                .unwrap_err()
                .contains("account changed")
        );
    }
    let snapshot = bootstrap(&backend).await;
    let metadata = project(&snapshot.recent_projects, &path);
    assert_eq!(
        metadata.display_name.as_deref(),
        Some(renamed.name.as_str())
    );
    assert_eq!(metadata.name_revision, renamed.name_revision);
    assert_eq!(metadata.is_default_workspace, renamed.is_default_workspace);
    assert_eq!(snapshot.project_root.as_deref(), Some(path.as_str()));

    let cleared = backend
        .rename_project_root(USER, path.clone(), default_name.clone())
        .await
        .unwrap();
    assert_eq!(cleared.name, default_name);
    assert!(cleared.name_revision > renamed.name_revision);
    drop(events);
    drop(backend);
    let reopened = owner(storage.path());
    let snapshot = bootstrap(&reopened).await;
    let restored = project(&snapshot.recent_projects, &path);
    assert_eq!(
        restored.display_name.as_deref(),
        Some(cleared.name.as_str())
    );
    assert_eq!(restored.name_revision, cleared.name_revision);
    assert!(
        folder.path().is_dir(),
        "a label rename must not move the folder"
    );
}

#[tokio::test]
async fn default_workspace_metadata_survives_a_full_first_project_page() {
    let Some(workspace) = maple_agent::agent::default_maple_workspace_path() else {
        return;
    };
    let storage = tempfile::tempdir().unwrap();
    let backend = owner(storage.path());
    let store = backend.store().unwrap();
    let account = store.upsert_account(USER, 1).unwrap();
    for index in 0..55 {
        let path = storage.path().join(format!("project-{index:03}"));
        store
            .remember_project(account, &path.to_string_lossy(), index, 1)
            .unwrap();
    }
    let snapshot = bootstrap(&backend).await;
    assert_eq!(snapshot.recent_projects.len(), 50);
    assert!(snapshot.projects_next.is_some());
    let workspace = workspace.to_string_lossy();
    assert!(
        snapshot
            .recent_projects
            .iter()
            .all(|project| project.root != workspace)
    );
    let anchor = snapshot.default_project.unwrap();
    assert_eq!(anchor.root, workspace);
    assert!(anchor.is_default_workspace);
    assert_eq!(
        anchor.display_name.as_deref(),
        Some(maple_agent::agent::MAPLE_WORKSPACE_DISPLAY_NAME)
    );
}

async fn endpoint() -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .alpns(vec![wire::ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .clear_address_lookup()
        .clear_ip_transports()
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .transport_config(streams::transport_config())
        .bind()
        .await
        .unwrap()
}

async fn remote_call(
    backend: &LocalBackend,
    controller: &iroh::endpoint::Connection,
    execution: &iroh::endpoint::Connection,
    method: wire::Method,
    params: Value,
) -> Result<Value, String> {
    let send = streams::call(
        controller,
        wire::Command {
            id: 1,
            method,
            send_id: None,
            params,
        },
    );
    let serve = async {
        let (mut tx, mut rx) = execution.accept_bi().await.unwrap();
        let header = streams::Header::read(&mut rx).await.unwrap();
        let capacity = streams::Capacity::new();
        let _slot = capacity.acquire(header.method).unwrap();
        let command = header.body(&mut rx).await.unwrap();
        assert_eq!(command.method, method);
        let result = crate::backend::dispatch_remote_inner(
            backend,
            request(&command.method.name(), command.params),
        )
        .await;
        streams::reply(
            &mut tx,
            &protocol::Message::Response {
                id: command.id,
                result,
            },
        )
        .await
        .unwrap();
    };
    let (response, ()) =
        tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(send, serve) })
            .await
            .unwrap();
    match response.unwrap() {
        protocol::Message::Response { result, .. } => result,
        _ => panic!("expected a per-request response"),
    }
}

#[tokio::test]
async fn project_folder_ipc_and_iroh_use_owner_paths_and_preserve_account_guards() {
    let storage = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let backend = owner(storage.path());
    let path = folder.path().to_str().unwrap();
    std::fs::create_dir(folder.path().join("existing")).unwrap();
    std::fs::write(
        folder.path().join("private-file"),
        b"not directory metadata",
    )
    .unwrap();
    let query = json!({"path": path, "after": null, "limit": 50, "showHidden": false});

    // Serialize the exact local command/reply through the production framing.
    let (mut client, mut server) = tokio::io::duplex(64 * 1024);
    let send = async {
        protocol::write(
            &mut client,
            &request("browse_project_folders", json!([USER, query])),
        )
        .await
        .unwrap();
        protocol::read::<protocol::Message>(&mut client)
            .await
            .unwrap()
    };
    let serve = async {
        let command: protocol::Request = protocol::read(&mut server).await.unwrap();
        let id = command.id;
        let result = crate::backend::dispatch(&backend, command).await;
        protocol::write(&mut server, &protocol::Message::Response { id, result })
            .await
            .unwrap();
    };
    let (response, ()) = tokio::join!(send, serve);
    let protocol::Message::Response {
        result: Ok(value), ..
    } = response
    else {
        panic!("folder browse failed");
    };
    let page: ProjectFolderPage = serde_json::from_value(value).unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].name, "existing");
    assert_eq!(
        page.path,
        folder.path().canonicalize().unwrap().to_str().unwrap()
    );

    let controller = endpoint().await;
    let execution = endpoint().await;
    let (client, server) = tokio::join!(controller.connect(execution.addr(), wire::ALPN), async {
        execution.accept().await.unwrap().await.unwrap()
    });
    let client = client.unwrap();
    let created = remote_call(
        &backend,
        &client,
        &server,
        wire::Method::CreateProjectFolder,
        json!([USER, page.path, "new folder"]),
    )
    .await
    .unwrap();
    let created: String = serde_json::from_value(created).unwrap();
    assert!(std::path::Path::new(&created).is_dir());
    let listing = remote_call(
        &backend,
        &client,
        &server,
        wire::Method::BrowseProjectFolders,
        json!([USER, query]),
    )
    .await
    .unwrap();
    let listing: ProjectFolderPage = serde_json::from_value(listing).unwrap();
    assert_eq!(
        listing
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["existing", "new folder"]
    );
    let selected = remote_call(
        &backend,
        &client,
        &server,
        wire::Method::SelectProjectRoot,
        json!([USER, created]),
    )
    .await
    .unwrap();
    let selected: SelectedProject = serde_json::from_value(selected).unwrap();
    assert_eq!(selected.project_root, created);

    for (method, params) in [
        (
            wire::Method::BrowseProjectFolders,
            json!(["another-account", query]),
        ),
        (
            wire::Method::CreateProjectFolder,
            json!(["another-account", page.path, "unauthorized"]),
        ),
    ] {
        assert!(
            crate::backend::dispatch(&backend, request(&method.name(), params.clone()))
                .await
                .is_err()
        );
        assert!(
            remote_call(&backend, &client, &server, method, params)
                .await
                .is_err()
        );
    }
    assert!(
        crate::backend::dispatch(
            &backend,
            request("create_scratch_project", json!(["another-account"]))
        )
        .await
        .is_err()
    );
    assert!(
        remote_call(
            &backend,
            &client,
            &server,
            wire::Method::CreateScratchProject,
            json!(["another-account"])
        )
        .await
        .is_err()
    );
    assert!(!folder.path().join("unauthorized").exists());
    assert!(
        remote_call(
            &backend,
            &client,
            &server,
            wire::Method::CreateProjectFolder,
            json!([USER, page.path, "existing"])
        )
        .await
        .is_err()
    );
    assert_eq!(
        std::fs::read(folder.path().join("private-file")).unwrap(),
        b"not directory metadata"
    );
    controller.close().await;
    execution.close().await;
}

#[tokio::test]
async fn project_folder_operations_require_an_open_store() {
    let storage = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let mut backend = owner(storage.path());
    let locked = maple_agent::store::Store::unavailable(
        maple_agent::store::StorePaths::new(storage.path().join("locked")),
        maple_agent::store::StoreState::Locked,
    )
    .unwrap();
    backend.store = Some(Arc::new(locked));
    let path = folder.path().to_str().unwrap();
    for (method, params) in [
        (
            "browse_project_folders",
            json!([USER, {"path":path,"after":null,"limit":10}]),
        ),
        ("create_project_folder", json!([USER, path, "locked-child"])),
        ("create_scratch_project", json!([USER])),
    ] {
        assert!(
            crate::backend::dispatch(&backend, request(method, params))
                .await
                .unwrap_err()
                .contains("not open")
        );
    }
    assert!(!folder.path().join("locked-child").exists());
}

#[tokio::test]
async fn project_names_cross_iroh_in_both_directions_and_recover_from_snapshot() {
    let storage = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let backend = owner(storage.path());
    let path = backend
        .select_project_root(USER, folder.path().to_string_lossy().into_owned())
        .await
        .unwrap()
        .project_root;
    let mut events = backend.take_events().await.unwrap();
    let a = endpoint().await;
    let b = endpoint().await;
    let (controller, execution) = tokio::join!(
        async { a.connect(b.addr(), wire::ALPN).await.unwrap() },
        async { b.accept().await.unwrap().await.unwrap() },
    );
    let exercise = async {
        let response = remote_call(
            &backend,
            &controller,
            &execution,
            wire::Method::RenameProjectRoot,
            json!([USER, path, "From controller"]),
        )
        .await
        .unwrap();
        let renamed: RecentProjectRoot = serde_json::from_value(response).unwrap();
        assert_eq!(renamed.name, "From controller");
        let snapshot = bootstrap(&backend).await;
        assert_eq!(
            project(&snapshot.recent_projects, &path)
                .display_name
                .as_deref(),
            Some("From controller")
        );
        project_event(&mut events, &path, "From controller").await;

        backend
            .rename_project_root(USER, path.clone(), "From owner desktop".into())
            .await
            .unwrap();
        let event = project_event(&mut events, &path, "From owner desktop").await;
        let (mut tx, _rx) = execution.open_bi().await.unwrap();
        let message = protocol::Message::Event(Box::new(event));
        let sent = async {
            wire::write(&mut tx, &message, wire::MAX_FRAME)
                .await
                .unwrap();
        };
        let received = async {
            let (_tx, mut rx) = controller.accept_bi().await.unwrap();
            wire::read::<protocol::Message>(&mut rx, wire::MAX_FRAME)
                .await
                .unwrap()
        };
        let ((), received) = tokio::join!(sent, received);
        assert!(
            matches!(received, protocol::Message::Event(event) if matches!(*event, AgentServiceEvent::ProjectsChanged { ref user_id, ref roots } if user_id == USER && roots.len() == 1 && roots[0].path == path && roots[0].name == "From owner desktop"))
        );

        let denied = remote_call(
            &backend,
            &controller,
            &execution,
            wire::Method::RenameProjectRoot,
            json!(["another-account", path, "Cross-account"]),
        )
        .await
        .unwrap_err();
        assert!(denied.contains("account changed"));
        controller.close(0u32.into(), b"snapshot recovery");
        let (controller, execution) = tokio::join!(
            async { a.connect(b.addr(), wire::ALPN).await.unwrap() },
            async { b.accept().await.unwrap().await.unwrap() },
        );
        let recovered = remote_call(
            &backend,
            &controller,
            &execution,
            wire::Method::NavigationBootstrap,
            json!([USER]),
        )
        .await
        .unwrap();
        let snapshot: crate::backend::paging::NavigationBootstrap =
            serde_json::from_value(recovered).unwrap();
        let recovered_name = project(&snapshot.recent_projects, &path);
        assert_eq!(
            recovered_name.display_name.as_deref(),
            Some("From owner desktop")
        );
        assert!(recovered_name.name_revision > renamed.name_revision);
        controller.close(0u32.into(), b"done");
    };
    tokio::time::timeout(Duration::from_secs(30), exercise)
        .await
        .unwrap();
    a.close().await;
    b.close().await;
}
