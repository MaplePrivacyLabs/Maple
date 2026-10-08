//! The Agent Client Protocol server (`maple-agent acp`).
//!
//! ACP moves to the Pi runtime in a later part of phase 2. Until it does,
//! the account's ACP configuration loads as before and serving reports that
//! ACP is not available in this build.

mod config;

pub use config::{AgentAcpConfig, load_acp_config};

use crate::agent::AgentRuntimeHandle;

/// The runtime start an ACP connection shares between its sessions.
pub type SharedRuntimeStart = futures_util::future::Shared<
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>,
>;

/// Build the shared lazy start `serve_stdio` expects from any start future.
pub fn shared_runtime_start<F>(start: F) -> SharedRuntimeStart
where
    F: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    futures_util::FutureExt::shared(Box::pin(start))
}

/// Serve ACP on this process's stdin and stdout for one signed-in account.
pub async fn serve_stdio(
    _agent: AgentRuntimeHandle,
    _config: AgentAcpConfig,
    _runtime_start: SharedRuntimeStart,
) -> Result<(), String> {
    Err("ACP is not available in this build of Maple yet".to_string())
}
