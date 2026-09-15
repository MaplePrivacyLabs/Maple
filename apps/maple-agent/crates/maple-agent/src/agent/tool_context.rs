use super::transient_mcp::TransientMcpRouter;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use tokio_util::sync::CancellationToken;

/// Variables an ACP bridge (Buzz) hands to the agent process. They are
/// scrubbed from every tool environment, in every surface, so a shell tool
/// cannot read the bridge's credentials.
pub(crate) const SENSITIVE_BRIDGE_ENV: [&str; 5] = [
    "BUZZ_RELAY_URL",
    "BUZZ_PRIVATE_KEY",
    "BUZZ_AUTH_TAG",
    "BUZZ_API_TOKEN",
    "BUZZ_ACP_DISPLAY_NAME",
];

/// Tool context for the desktop surface. It keeps the ACP bridge's Buzz
/// credentials out of every tool environment that the desktop app starts.
pub fn default_tool_context_spec() -> Result<AgentToolContextSpec, String> {
    AgentToolContextSpec::try_new(
        BTreeMap::new(),
        SENSITIVE_BRIDGE_ENV
            .into_iter()
            .map(str::to_string)
            .collect(),
        false,
    )
}

const MAX_TOOL_CONTEXT_KEYS: usize = 16;
const MAX_TOOL_CONTEXT_KEY_BYTES: usize = 64;
const MAX_TOOL_CONTEXT_VALUE_BYTES: usize = 16 * 1024;
const MAX_TOOL_CONTEXT_TOTAL_BYTES: usize = 32 * 1024;

/// Validated, transport-neutral context supplied to Maple's developer tools.
///
/// Protocol adapters own their allowlists and decide which values make an
/// invocation ephemeral. The Agent service owns only validation, installation,
/// and revocation.
#[derive(Clone, Default)]
pub struct AgentToolContextSpec {
    values: BTreeMap<String, String>,
    scrub_from_parent: BTreeSet<String>,
    ephemeral: bool,
}

impl AgentToolContextSpec {
    pub fn try_new(
        values: BTreeMap<String, String>,
        scrub_from_parent: BTreeSet<String>,
        ephemeral: bool,
    ) -> Result<Self, String> {
        if values.len() > MAX_TOOL_CONTEXT_KEYS || scrub_from_parent.len() > MAX_TOOL_CONTEXT_KEYS {
            return Err(format!(
                "Agent tool context supports at most {MAX_TOOL_CONTEXT_KEYS} variables"
            ));
        }

        let mut total_bytes = 0usize;
        for key in values.keys().chain(scrub_from_parent.iter()) {
            validate_key(key)?;
        }
        for (key, value) in &values {
            if value.contains('\0') {
                return Err(format!(
                    "Agent tool context variable {key} cannot contain null bytes"
                ));
            }
            let value_bytes = value.len();
            if value_bytes > MAX_TOOL_CONTEXT_VALUE_BYTES {
                return Err(format!(
                    "Agent tool context variable {key} exceeds the {MAX_TOOL_CONTEXT_VALUE_BYTES} byte limit"
                ));
            }
            total_bytes = total_bytes
                .checked_add(key.len())
                .and_then(|total| total.checked_add(value_bytes))
                .ok_or_else(|| "Agent tool context size overflowed".to_string())?;
            if total_bytes > MAX_TOOL_CONTEXT_TOTAL_BYTES {
                return Err(format!(
                    "Agent tool context exceeds the {MAX_TOOL_CONTEXT_TOTAL_BYTES} byte total limit"
                ));
            }
        }

        Ok(Self {
            values,
            scrub_from_parent,
            ephemeral,
        })
    }
}

fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty() || key.contains(['=', '\0']) {
        return Err("Agent tool context variable names are invalid".to_string());
    }
    if key.len() > MAX_TOOL_CONTEXT_KEY_BYTES {
        return Err(format!(
            "Agent tool context variable names must be at most {MAX_TOOL_CONTEXT_KEY_BYTES} bytes"
        ));
    }
    Ok(())
}

struct AgentToolContextState {
    values: BTreeMap<String, String>,
    scrub_from_parent: BTreeSet<String>,
    ephemeral: bool,
    transient_mcp: Option<TransientMcpRouter>,
}

#[derive(Clone)]
pub(crate) struct SharedAgentToolContext {
    state: Arc<RwLock<AgentToolContextState>>,
    revoked: CancellationToken,
    launch_gate: Arc<Mutex<()>>,
}

impl SharedAgentToolContext {
    pub(crate) fn new(spec: AgentToolContextSpec) -> Self {
        Self {
            state: Arc::new(RwLock::new(AgentToolContextState {
                values: spec.values,
                scrub_from_parent: spec.scrub_from_parent,
                ephemeral: spec.ephemeral,
                transient_mcp: None,
            })),
            revoked: CancellationToken::new(),
            launch_gate: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn snapshot(&self) -> AgentToolContextSnapshot {
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        AgentToolContextSnapshot {
            values: state.values.clone(),
            scrub_from_parent: state.scrub_from_parent.clone(),
            ephemeral: state.ephemeral,
            revoked: self.revoked.clone(),
            launch_gate: Arc::clone(&self.launch_gate),
        }
    }

    pub(crate) fn lifetime_token(&self) -> CancellationToken {
        self.revoked.clone()
    }

    pub(crate) fn install_transient_mcp(&self, router: TransientMcpRouter) -> Result<(), String> {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.revoked.is_cancelled() {
            return Err("Agent tool context was revoked during MCP setup".to_string());
        }
        if state.transient_mcp.is_some() {
            return Err("Agent tool context already has transient MCP tools".to_string());
        }
        if !router.is_empty() {
            state.transient_mcp = Some(router);
        }
        Ok(())
    }

    pub(crate) fn transient_mcp(&self) -> Option<TransientMcpRouter> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .transient_mcp
            .clone()
    }

    pub(crate) fn revoke(&self) {
        // Linearize revocation with command construction and spawn. Once this
        // method returns, no snapshot taken before revocation can launch a new
        // process with its copied values.
        let _launch = self
            .launch_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.revoked.cancel();
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.values.clear();
        state.ephemeral = false;
        state.transient_mcp.take();
        // Retain inherited-key scrubbing after revocation. A removed explicit
        // credential must never reveal a same-named ambient process value.
    }

    pub(crate) fn cancel_run(&self, run: &CancellationToken) {
        // A run cancellation and a tool launch share the same fence. Once this
        // method returns, a snapshot from this context cannot cross the launch
        // boundary for that run.
        let _launch = self
            .launch_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        run.cancel();
    }

    pub(crate) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    pub(crate) fn is_revoked(&self) -> bool {
        self.revoked.is_cancelled()
    }
}

#[derive(Clone)]
pub(crate) struct AgentToolContextSnapshot {
    pub(crate) values: BTreeMap<String, String>,
    pub(crate) scrub_from_parent: BTreeSet<String>,
    pub(crate) ephemeral: bool,
    pub(crate) revoked: CancellationToken,
    launch_gate: Arc<Mutex<()>>,
}

impl AgentToolContextSnapshot {
    pub(crate) fn begin_process_launch(
        &self,
        run: &CancellationToken,
    ) -> Result<MutexGuard<'_, ()>, String> {
        let guard = self
            .launch_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.revoked.is_cancelled() {
            Err("Agent tool context was revoked before command launch".to_string())
        } else if run.is_cancelled() {
            Err("Agent run was cancelled before command launch".to_string())
        } else {
            Ok(guard)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revocation_clears_values_but_retains_inherited_scrubbing() {
        let context = SharedAgentToolContext::new(
            AgentToolContextSpec::try_new(
                BTreeMap::from([("TOKEN".to_string(), "secret".to_string())]),
                BTreeSet::from(["TOKEN".to_string()]),
                true,
            )
            .unwrap(),
        );

        context.revoke();
        let snapshot = context.snapshot();
        assert!(snapshot.values.is_empty());
        assert_eq!(
            snapshot.scrub_from_parent,
            BTreeSet::from(["TOKEN".to_string()])
        );
        assert!(!snapshot.ephemeral);
        assert!(snapshot.revoked.is_cancelled());
    }

    #[test]
    fn validation_is_generic_and_bounded() {
        assert!(
            AgentToolContextSpec::try_new(
                BTreeMap::from([("CUSTOM_TOKEN".to_string(), "value".to_string())]),
                BTreeSet::new(),
                false,
            )
            .is_ok()
        );
        assert!(
            AgentToolContextSpec::try_new(
                BTreeMap::from([("BAD=KEY".to_string(), "value".to_string())]),
                BTreeSet::new(),
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn run_cancellation_returns_only_after_the_process_launch_fence() {
        let context = SharedAgentToolContext::new(AgentToolContextSpec::default());
        let snapshot = context.snapshot();
        let run = CancellationToken::new();
        let launch = snapshot.begin_process_launch(&run).unwrap();
        let cancellation_context = context.clone();
        let cancellation_run = run.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let task = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            cancellation_context.cancel_run(&cancellation_run);
            finished_tx.send(()).unwrap();
        });

        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("cancellation thread should start");
        assert!(
            finished_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err()
        );
        drop(launch);
        finished_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("cancellation should finish after launch releases the fence");
        task.join().unwrap();

        assert!(run.is_cancelled());
        assert!(snapshot.begin_process_launch(&run).is_err());
    }
}
