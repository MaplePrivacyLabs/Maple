//! The connection manager against a host on a real loopback port: pairing
//! records what the host announced, and the record outlives the
//! connection.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{FakeHost, client_config, hello, start_host};
use maple_remote::hosts::HostsStore;
use maple_remote::keys::StaticKey;
use maple_remote::manager::{HostManager, HostManagerEvent, HostStatus, HostVersion};
use maple_remote::pairing::PairingCode;
use tokio::sync::mpsc;

async fn next_status(
    events: &mut mpsc::UnboundedReceiver<HostManagerEvent>,
    wanted: impl Fn(&HostStatus) -> bool,
) -> HostStatus {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("an event in time")
            .expect("the manager is alive");
        if let HostManagerEvent::Status { status, .. } = event
            && wanted(&status)
        {
            return status;
        }
    }
}

#[tokio::test]
async fn pairing_records_the_hosts_version_and_keeps_it_once_offline() {
    let fake = FakeHost::new(1);
    let host = start_host(Arc::clone(&fake)).await;
    let dir = std::env::temp_dir().join(format!("maple-manager-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let device = StaticKey::generate().unwrap();
    let mut client_hello = hello();
    client_hello.device.public_key = device.public_id();
    let store = Arc::new(HostsStore::new(dir.join("hosts.json")));
    let (manager, mut events) = HostManager::new(device, client_hello, store, client_config());

    let code = PairingCode::generate();
    host.pending.publish(&code).unwrap();
    let saved = manager
        .pair(&host.address, code, None)
        .await
        .expect("pairing");
    assert_eq!(saved.id, host.key.public_id());
    // The hello is what `identity()` in the fixture announces.
    let expected = HostVersion {
        version: "0.1.0".to_string(),
        build: Some("abc1234".to_string()),
    };
    assert_eq!(saved.last_seen_version.as_deref(), Some("0.1.0"));
    assert_eq!(saved.last_seen_build.as_deref(), Some("abc1234"));

    next_status(&mut events, |status| *status == HostStatus::Online).await;
    assert!(manager.is_online(&saved.id));
    assert_eq!(manager.host_version(&saved.id), Some(expected.clone()));
    assert_eq!(expected.label(), "0.1.0 (abc1234)");
    let record = manager.store().get(&saved.id).unwrap().unwrap();
    assert_eq!(record.last_seen_version.as_deref(), Some("0.1.0"));
    assert_eq!(record.last_seen_build.as_deref(), Some("abc1234"));

    // The host goes away: the live version goes with the connection, the
    // saved record keeps what it last announced.
    host.shutdown.cancel();
    drop(host);
    next_status(&mut events, |status| {
        matches!(status, HostStatus::Offline { .. })
    })
    .await;
    assert!(!manager.is_online(&saved.id));
    assert_eq!(manager.host_version(&saved.id), None);
    let record = manager.store().get(&saved.id).unwrap().unwrap();
    assert_eq!(record.last_seen_version.as_deref(), Some("0.1.0"));
    assert_eq!(record.last_seen_build.as_deref(), Some("abc1234"));

    manager.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}
