//! The desktop app as a client of remote hosts.
//!
//! Builds the connection manager for the signed-in account: this device's
//! static key, the saved hosts file under the account, and the hello every
//! connection sends. The manager runs on the backend's Tokio runtime and
//! reports through one channel that the desktop shell pumps into the chat
//! screen.

use std::sync::Arc;

use maple_remote::client::ClientConfig;
use maple_remote::hosts::HostsStore;
use maple_remote::keys::StaticKey;
use maple_remote::manager::{HostManager, HostManagerEvent};
use maple_remote::wire::{ClientHello, DeviceInfo, PROTOCOL_VERSION, features};
use tokio::sync::mpsc;

use crate::backend::AgentBackend;

/// This device's static key, generated on first use. It sits beside the
/// host key: one machine, two roles.
pub fn device_key() -> Result<StaticKey, String> {
    StaticKey::load_or_create(&super::remote_dir()?.join("device_key.json"))
}

/// The saved hosts of one account, in the account's local data directory
/// so removing the account removes them.
fn hosts_store(user_id: &str) -> Result<HostsStore, String> {
    let dir = maple_agent::agent::account_local_data_dir(&crate::backend::agent_paths(), user_id)?;
    Ok(HostsStore::new(dir.join("hosts.json")))
}

/// The hello this device sends to every host.
fn client_hello(device: &StaticKey, user_id: &str) -> Result<ClientHello, String> {
    Ok(ClientHello {
        protocol: PROTOCOL_VERSION,
        app_version: crate::env::APP_VERSION.to_string(),
        build: crate::env::build_hash().map(str::to_string),
        pcr_environment: format!(
            "{:?}",
            maple_agent::open_secret_config::configured_pcr0_environment()?
        ),
        features: features(),
        device: DeviceInfo {
            public_key: device.public_id(),
            name: crate::env::hostname(),
            user_id: Some(user_id.to_string()),
        },
    })
}

/// Start connecting to every saved host of `user_id`. Connectors run on
/// the backend runtime; events arrive on the returned channel.
pub fn start_manager(
    backend: &Arc<AgentBackend>,
    user_id: &str,
) -> Result<(Arc<HostManager>, mpsc::UnboundedReceiver<HostManagerEvent>), String> {
    let device = device_key()?;
    let hello = client_hello(&device, user_id)?;
    let store = Arc::new(hosts_store(user_id)?);
    let (manager, events) = HostManager::new(device, hello, store, ClientConfig::default());
    let _runtime = backend.runtime_handle().enter();
    manager.start();
    Ok((manager, events))
}
