//! The Cua Driver SDK in Maple's process: its permissions, its runtime,
//! and a task's binding to its own CUA session.
//!
//! The runtime belongs to the process and starts on first use. A binding
//! is a trusted connection to the task's account-and-task scoped CUA
//! session; closing it revokes only that connection, so the next run binds
//! the same session again while other tasks keep sharing the runtime.
//!
//! The SDK lets a connection lapse after a fixed idle time and lifetime. A
//! run can outlast both, so the binding replaces its connection whenever
//! either limit is near, and once more when the SDK reports it lapsed anyway.
//!
//! Everything below the permission helpers is platform-neutral. Only the
//! pre-flight permissions differ: macOS grants Accessibility and Screen
//! Recording to the process up front, while portal-based desktops grant
//! capability per session at first use.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use cua_driver_sdk::{
    ConfiguredDriverOptions, CuaDriver, CuaDriverSession, RuntimeAuthorizationOptions,
    SessionPermissionMode, TrustedSessionOptions,
};
use pi_agent_core::{AgentTool, AgentToolResult, ToolError, ToolInvocation};
use pi_coding_agent::ModelRegistry;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::agent::AgentIntegrationPermissionKind;
use crate::agent::AgentIntegrationPermissions;
use crate::agent::integrations::CUA_NAME;

use super::projection::{
    CatalogTool, bound_arguments, parse_catalog, parse_result, result_for_model,
};
use super::{TOOL_PREFIX, missing_permission_message};

// These match Cua Driver's reviewed standard-session policy. They bound one
// connection, not the task: a binding replaces its connection before either
// runs out, so neither ever ends computer use in a run.
const SESSION_TTL_SECONDS: u64 = 8 * 60 * 60;
const SESSION_IDLE_TTL_SECONDS: u64 = 30 * 60;

/// Cap how long binding a task to its CUA session may take.
///
/// Native platform start-up talks to the accessibility and screen-capture
/// services, which can stall behind an operating-system prompt. A run
/// waits on it before its first prompt, so an unbounded wait would hold
/// the run, and Stop, hostage.
const RUNTIME_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// The tool catalog is fixed for the process-wide driver, so it is fetched,
/// parsed and bound once instead of on every run.
static TOOL_CATALOG: tokio::sync::OnceCell<Arc<Vec<CatalogTool>>> =
    tokio::sync::OnceCell::const_new();

static DRIVER: OnceLock<Arc<CuaDriver>> = OnceLock::new();
// `OnceLock::get_or_try_init` is not available on Maple's stable toolchain.
// Serialize the fallible constructor so a race cannot create a second native
// runtime and strand the successful one outside `DRIVER`.
static DRIVER_INITIALIZATION: Mutex<()> = Mutex::new(());

/// Read the host-process permissions attributed to Maple's own identity.
///
/// This is deliberately status-only. Settings owns all prompting and relaunch
/// UX; binding a task must never raise an operating-system permission prompt
/// as a side effect.
pub(super) fn permission_status() -> AgentIntegrationPermissions {
    #[cfg(target_os = "macos")]
    {
        macos_permissions(cua_driver_sdk::current_mac_os_permission_status())
    }
    #[cfg(target_os = "linux")]
    {
        linux_requirements()
    }
    // Windows has no consent gate for accessibility, input or capture: any
    // process at the user's integrity level may use them. Enabling computer
    // use in Settings is the only grant, so nothing is left to read here.
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        AgentIntegrationPermissions::none_required()
    }
}

/// What Linux needs before the runtime is usable.
///
/// Portal-based desktops grant screen capture and input per session at first
/// use, so there is nothing to read up front. GNOME is the exception: Mutter
/// advertises neither the wlroots protocols nor `ext-image-copy-capture-v1`,
/// so an ordinary client can read no window geometry and capture no pixels.
/// Both go through a Shell extension instead, and without it the SDK silently
/// falls back to X11 and fails. Treat the extension as a real requirement so
/// the user is told, rather than meeting a broken screenshot later.
#[cfg(target_os = "linux")]
fn linux_requirements() -> AgentIntegrationPermissions {
    if !gnome_wayland_session() {
        return AgentIntegrationPermissions::none_required();
    }
    AgentIntegrationPermissions::default().with(
        AgentIntegrationPermissionKind::DesktopHelper,
        gnome_helper_loaded(),
    )
}

#[cfg(target_os = "linux")]
fn gnome_wayland_session() -> bool {
    let Some(desktop) = std::env::var_os("XDG_CURRENT_DESKTOP") else {
        return false;
    };
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        && desktop
            .to_string_lossy()
            .split(':')
            .any(|entry| entry.eq_ignore_ascii_case("GNOME"))
}

/// Install the compositor helper if this desktop needs one and has none.
///
/// Only an explicit user setup action reaches this. It is idempotent, so
/// pressing setup again after an upgrade refreshes the embedded copy.
#[cfg(target_os = "linux")]
pub(super) async fn install_desktop_helper() -> Result<(), String> {
    if !gnome_wayland_session() || gnome_helper_loaded() {
        return Ok(());
    }
    tokio::task::spawn_blocking(super::gnome_helper::install)
        .await
        .map_err(|error| format!("Could not install the GNOME helper extension: {error}"))??;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(super) async fn install_desktop_helper() -> Result<(), String> {
    Ok(())
}

/// One sentence describing what is left to do about the compositor helper,
/// or `None` when this desktop needs none or already has it working.
///
/// The remedy differs by how far along the install is, and saying "install it"
/// to somebody who already did is how a user concludes the thing is broken.
pub(super) fn desktop_helper_hint() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        use super::gnome_helper::GnomeHelperState;

        if !gnome_wayland_session() {
            return None;
        }
        match super::gnome_helper::state(gnome_helper_loaded()) {
            GnomeHelperState::Loaded => None,
            GnomeHelperState::NeedsSessionRestart => Some(
                "The GNOME helper extension is installed but not loaded. Log out and back in: \
                 GNOME reads extensions only when the session starts."
                    .to_string(),
            ),
            GnomeHelperState::Missing => Some(
                "GNOME needs a helper extension for window geometry and screen capture. \
                 Use Set up to install it, then log out and back in."
                    .to_string(),
            ),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Whether a setup action can still do something on this desktop.
///
/// Once the helper is written, the only step left is restarting the session,
/// which Maple cannot do for the user. Offering a button then would repeat
/// work they already did and hide the step that actually matters.
pub(super) fn desktop_setup_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        use super::gnome_helper::GnomeHelperState;

        gnome_wayland_session()
            && super::gnome_helper::state(gnome_helper_loaded()) == GnomeHelperState::Missing
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Whether the compositor helper is loaded right now.
///
/// Installing the extension is not enough, because GNOME loads extensions only
/// when the session starts. Owning the bus name is the only signal that says
/// the helper will actually answer.
#[cfg(target_os = "linux")]
fn gnome_helper_loaded() -> bool {
    const HELPER_BUS_NAME: &str = "org.cua.WinRects";

    // zbus's blocking API drives its own runtime, and a runtime cannot be
    // started from a thread that is already driving one. This check is reached
    // from both async and plain call sites, so give it a thread of its own
    // instead of making every caller prove where it runs.
    std::thread::spawn(|| {
        let connection = zbus::blocking::Connection::session().ok()?;
        let proxy = zbus::blocking::fdo::DBusProxy::new(&connection).ok()?;
        proxy.name_has_owner(HELPER_BUS_NAME.try_into().ok()?).ok()
    })
    .join()
    .ok()
    .flatten()
    .unwrap_or(false)
}

/// Ask the operating system for the grants Maple's embedded CUA runtime needs.
///
/// Call this only from an explicit user setup action. The SDK performs the
/// requests in Maple's process, so grants belong to Maple rather than to a
/// separately installed CuaDriver application.
pub(super) fn request_permissions() -> AgentIntegrationPermissions {
    #[cfg(target_os = "macos")]
    {
        macos_permissions(cua_driver_sdk::request_mac_os_permissions())
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Nothing here can be requested on the user's behalf: a portal grant
        // is answered when a tool first runs, and a Shell extension is
        // installed by the user. Report the current state instead.
        permission_status()
    }
}

/// macOS attributes Accessibility and Screen Recording to the running process,
/// so both can be read before the runtime starts. Accessibility comes first
/// because it is the grant the user is asked for first.
#[cfg(target_os = "macos")]
fn macos_permissions(status: cua_driver_sdk::MacOsPermissionStatus) -> AgentIntegrationPermissions {
    AgentIntegrationPermissions::default()
        .with(
            AgentIntegrationPermissionKind::Accessibility,
            status.accessibility,
        )
        .with(
            AgentIntegrationPermissionKind::ScreenRecording,
            status.screen_recording,
        )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionIdentity {
    public_session: String,
    transport_session: String,
}

/// The task's CUA session and the transport bound to it: opaque, stable for
/// the account and task, and distinct between accounts and tasks.
fn session_identity(account_scope: &str, session_id: &str) -> Result<SessionIdentity, String> {
    let account_scope = account_scope.trim();
    if account_scope.is_empty() {
        return Err("Cannot create embedded CUA tools without an account scope".to_string());
    }
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return Err("Cannot create embedded CUA tools for an empty task ID".to_string());
    }

    fn digest(domain: &[u8], account_scope: &str, session_id: &str) -> String {
        let mut hasher = Sha256::new();
        for value in [domain, account_scope.as_bytes(), session_id.as_bytes()] {
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value);
        }
        format!("{:x}", hasher.finalize())
    }

    Ok(SessionIdentity {
        public_session: format!(
            "maple-task-{}",
            digest(
                b"maple-agent:embedded-cua:public:v2",
                account_scope,
                session_id
            )
        ),
        transport_session: format!(
            "maple-transport-{}",
            digest(
                b"maple-agent:embedded-cua:transport:v2",
                account_scope,
                session_id
            )
        ),
    })
}

fn process_driver() -> Result<Arc<CuaDriver>, String> {
    if let Some(driver) = DRIVER.get() {
        return Ok(Arc::clone(driver));
    }

    let _initialization = DRIVER_INITIALIZATION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(driver) = DRIVER.get() {
        return Ok(Arc::clone(driver));
    }

    let driver = CuaDriver::create_configured(driver_options(
        SESSION_TTL_SECONDS,
        SESSION_IDLE_TTL_SECONDS,
    ))
    .map_err(|error| format!("Failed to initialize embedded CUA runtime: {error}"))?;

    // The initialization mutex makes this infallible unless this module's
    // ownership invariant is violated. Still avoid panicking if that changes.
    if DRIVER.set(Arc::clone(&driver)).is_err() {
        return DRIVER
            .get()
            .map(Arc::clone)
            .ok_or_else(|| "Embedded CUA runtime initialization raced".to_string());
    }
    Ok(driver)
}

fn driver_options(max_session_ttl: u64, max_idle_ttl: u64) -> ConfiguredDriverOptions {
    ConfiguredDriverOptions {
        claude_code_compatibility: false,
        authorization: RuntimeAuthorizationOptions {
            allowed_modes: vec![SessionPermissionMode::Standard],
            compatibility_mode: SessionPermissionMode::Standard,
            compatibility_capability_manifest_path: None,
            compatibility_bounded_manifest_path: None,
            unrestricted_acknowledged: false,
            max_session_ttl_seconds: max_session_ttl,
            max_idle_ttl_seconds: max_idle_ttl,
        },
    }
}

async fn tool_catalog(driver: &CuaDriver) -> Result<Arc<Vec<CatalogTool>>, String> {
    TOOL_CATALOG
        .get_or_try_init(|| async {
            let raw = driver
                .list_tools_json()
                .await
                .map_err(|error| format!("Failed to load embedded CUA tool catalog: {error}"))?;
            parse_catalog(&raw).map(Arc::new)
        })
        .await
        .map(Arc::clone)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How long the SDK keeps one trusted connection: its lifetime, and how long
/// it may go unused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Lease {
    ttl_seconds: u64,
    idle_ttl_seconds: u64,
}

impl Lease {
    const STANDARD: Self = Self {
        ttl_seconds: SESSION_TTL_SECONDS,
        idle_ttl_seconds: SESSION_IDLE_TTL_SECONDS,
    };

    /// Whether a connection this old and this long unused is close enough to
    /// either limit to replace before the next call. The margin, a sixth of
    /// the idle time (five minutes on the standard lease), absorbs the gap
    /// between Maple's clock and the SDK's.
    fn due(self, age: Duration, unused: Duration) -> bool {
        let idle_ttl = Duration::from_secs(self.idle_ttl_seconds);
        let margin = idle_ttl / 6;
        unused + margin >= idle_ttl || age + margin >= Duration::from_secs(self.ttl_seconds)
    }
}

/// The binding's current trusted connection, and Maple's reading of the two
/// clocks the SDK keeps for it. Both readings are never fresher than the
/// SDK's own, so a renewal comes early rather than late.
struct Connection {
    session: Arc<CuaDriverSession>,
    /// Taken before the SDK made the connection.
    opened: Instant,
    /// When the latest successful call began. The SDK counts a call as use
    /// once it admits it, which is no earlier.
    used: Instant,
}

/// A task's trusted connection to its CUA session, and the tools it offers.
///
/// The connection is replaced, never the binding: a run keeps the same tools
/// while the binding renews what lies under them. Only the task's own
/// identity is ever reconnected, so a renewal grants nothing the run did not
/// already have.
pub(super) struct Binding {
    driver: Arc<CuaDriver>,
    identity: SessionIdentity,
    lease: Lease,
    pub(super) catalog: Arc<Vec<CatalogTool>>,
    connection: Mutex<Connection>,
    /// One renewal at a time; a call that waited uses the one just made.
    renewing: tokio::sync::Mutex<()>,
    /// The task was unbound: its tools, which stay with the session, fail.
    closed: AtomicBool,
}

impl Binding {
    /// Bind the task to its CUA session: the runtime started if it has not,
    /// a trusted connection made, and the session started, or revived when
    /// its idle time ran out between runs.
    pub(super) async fn open(account_scope: &str, session_id: &str) -> Result<Self, String> {
        let identity = session_identity(account_scope, session_id)?;
        let permissions = permission_status();
        if !permissions.ready() {
            return Err(missing_permission_message(&permissions));
        }

        let prepare = async move {
            // Native start-up is synchronous and can take seconds on its first
            // call, so it must not occupy an async worker thread.
            let driver = tokio::task::spawn_blocking(process_driver)
                .await
                .map_err(|error| format!("Embedded CUA runtime start-up failed: {error}"))??;
            let catalog = tool_catalog(&driver).await?;
            Self::open_on(driver, catalog, identity, Lease::STANDARD).await
        };
        tokio::time::timeout(RUNTIME_STARTUP_TIMEOUT, prepare)
            .await
            .map_err(|_| STARTUP_TIMEOUT_MESSAGE.to_string())?
    }

    /// Bind on a given runtime and lease, without the permission check.
    async fn open_on(
        driver: Arc<CuaDriver>,
        catalog: Arc<Vec<CatalogTool>>,
        identity: SessionIdentity,
        lease: Lease,
    ) -> Result<Self, String> {
        let opened = Instant::now();
        let session = connect(&driver, &identity, lease).await?;
        Ok(Self {
            driver,
            identity,
            lease,
            catalog,
            connection: Mutex::new(Connection {
                session,
                opened,
                used: opened,
            }),
            renewing: tokio::sync::Mutex::new(()),
            closed: AtomicBool::new(false),
        })
    }

    /// Revoke this connection. The task's CUA session stays for the next
    /// binding, and the runtime for every task.
    pub(super) fn close(&self) {
        // Set before taking the connection, so a renewal that finishes after
        // this either sees it or has its new connection closed here.
        self.closed.store(true, Ordering::Release);
        lock(&self.connection).session.close();
    }

    fn not_connected() -> String {
        format!("{CUA_NAME} is not connected to this task.")
    }

    /// The connection to call on, renewed first when a limit is near.
    async fn current(&self) -> Result<Arc<CuaDriverSession>, String> {
        let (session, due) = {
            let connection = lock(&self.connection);
            let now = Instant::now();
            (
                Arc::clone(&connection.session),
                self.lease.due(
                    now.saturating_duration_since(connection.opened),
                    now.saturating_duration_since(connection.used),
                ),
            )
        };
        if due {
            self.renew(&session).await
        } else {
            Ok(session)
        }
    }

    fn is_current(&self, session: &Arc<CuaDriverSession>) -> bool {
        Arc::ptr_eq(&lock(&self.connection).session, session)
    }

    /// Record a successful call that began at `began` on `session`.
    fn mark_used(&self, session: &Arc<CuaDriverSession>, began: Instant) {
        let mut connection = lock(&self.connection);
        if Arc::ptr_eq(&connection.session, session) && began > connection.used {
            connection.used = began;
        }
    }

    /// Replace `stale` with a new connection to the same CUA session. A call
    /// that finds another call already replaced it uses that one.
    async fn renew(&self, stale: &Arc<CuaDriverSession>) -> Result<Arc<CuaDriverSession>, String> {
        let _renewing = self.renewing.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(Self::not_connected());
        }
        {
            let connection = lock(&self.connection);
            if !Arc::ptr_eq(&connection.session, stale) {
                return Ok(Arc::clone(&connection.session));
            }
        }
        // The SDK refuses a second connection to a session that still holds
        // a live one, so release this one first.
        stale.close();
        let opened = Instant::now();
        let session = tokio::time::timeout(
            RUNTIME_STARTUP_TIMEOUT,
            connect(&self.driver, &self.identity, self.lease),
        )
        .await
        .map_err(|_| STARTUP_TIMEOUT_MESSAGE.to_string())?
        .map_err(|error| format!("{CUA_NAME} could not reconnect to this task: {error}"))?;
        let mut connection = lock(&self.connection);
        if self.closed.load(Ordering::Acquire) {
            drop(connection);
            session.close();
            return Err(Self::not_connected());
        }
        *connection = Connection {
            session: Arc::clone(&session),
            opened,
            used: opened,
        };
        log::info!("Built-in CUA renewed its connection");
        Ok(session)
    }

    /// Call one of the catalog's tools; the SDK's canonical result JSON.
    async fn call(
        &self,
        name: &str,
        arguments: Map<String, Value>,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        self.call_result(name, arguments, cancel)
            .await
            .map(|result| result.raw_json)
    }

    async fn call_result(
        &self,
        name: &str,
        arguments: Map<String, Value>,
        cancel: &CancellationToken,
    ) -> Result<cua_driver_sdk::ToolResult, String> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Self::not_connected());
        }
        if cancel.is_cancelled() {
            return Err("Cancelled".to_string());
        }
        let arguments = serde_json::to_string(&bound_arguments(arguments))
            .map_err(|_| "Embedded CUA arguments are not valid JSON".to_string())?;
        let mut session = self.current().await?;
        let mut retried = false;
        loop {
            let began = Instant::now();
            let result = {
                let call = session.call_tool(name.to_string(), arguments.clone());
                tokio::pin!(call);
                // Dropping the SDK future cancels cooperative asynchronous
                // work. This is necessarily best-effort: an OS action that
                // already reached the target application cannot be rolled back.
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err("Cancelled".to_string()),
                    result = &mut call => result,
                }
            };
            // The connection lapsed under the call, or a renewal closed it
            // while the call was on its way. Either way the SDK refused the
            // call before acting, so it is made once more on a new one.
            let lapsed = match &result {
                Ok(result) => connection_lapsed(result),
                Err(cua_driver_sdk::DriverError::Shutdown) => {
                    self.closed.load(Ordering::Acquire) || !self.is_current(&session)
                }
                Err(_) => false,
            };
            if lapsed && !retried {
                retried = true;
                session = self.renew(&session).await?;
                continue;
            }
            // Never log the result: screenshots and accessibility content
            // are the user's private data.
            let result = result.map_err(driver_error)?;
            if !result.is_error {
                self.mark_used(&session, began);
            }
            return Ok(result);
        }
    }
}

impl Drop for Binding {
    fn drop(&mut self) {
        // Idempotent and synchronous for an embedded session.
        lock(&self.connection).session.close();
    }
}

const STARTUP_TIMEOUT_MESSAGE: &str = "Computer use did not finish starting. Check that Maple still holds its screen and input permissions.";

/// Make a trusted connection to the task's CUA session and start it. That
/// creates the session on first use and revives it when the SDK reclaimed
/// it while unused; a live session is kept as it is, snapshots included.
async fn connect(
    driver: &Arc<CuaDriver>,
    identity: &SessionIdentity,
    lease: Lease,
) -> Result<Arc<CuaDriverSession>, String> {
    let driver = Arc::clone(driver);
    let identity = identity.clone();
    // Native session set-up is synchronous, so keep it off the async workers.
    let session = tokio::task::spawn_blocking(move || {
        driver.create_trusted_session_for_transport(
            trusted_session_options(identity.public_session, lease),
            &identity.transport_session,
        )
    })
    .await
    .map_err(|error| format!("Embedded CUA task session start-up failed: {error}"))?
    .map_err(|error| format!("Failed to create embedded CUA task session: {error}"))?;

    // Maple, rather than the model, owns this lifecycle boundary.
    let started = session
        .call_tool("start_session".to_string(), "{}".to_string())
        .await
        .map_err(|error| format!("Failed to start embedded CUA task session: {error}"))?;
    if started.is_error {
        session.close();
        return Err(format!(
            "Failed to start embedded CUA task session: {}",
            started.text
        ));
    }
    Ok(session)
}

/// The SDK's refusals for a connection that ran out (`permission_denied`
/// with one of these messages) as opposed to anything about the call. They
/// come from `cua-driver-core`'s admission check (the prefixed message) and
/// its dispatch commit, and both run before any action.
const LAPSED_MESSAGES: [&str; 4] = [
    "Permission denied: authorization context expired",
    "authorization context expired",
    "authorization context idle timeout exceeded",
    "authorization context has been revoked",
];

/// Whether the SDK refused a call because its connection lapsed. A lapsed
/// connection reads as revoked once any task binds, because binding sweeps
/// lapsed connections out; until then it reads as expired.
fn connection_lapsed(result: &cua_driver_sdk::ToolResult) -> bool {
    if !result.is_error {
        return false;
    }
    match result.error_code.as_deref() {
        Some("authorization_revoked") => true,
        Some("permission_denied") => result
            .structured_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<Value>(json).ok())
            .and_then(|value| {
                value
                    .pointer("/refusal/message")
                    .and_then(Value::as_str)
                    .map(|message| LAPSED_MESSAGES.contains(&message))
            })
            .unwrap_or(false),
        _ => false,
    }
}

fn trusted_session_options(public_session: String, lease: Lease) -> TrustedSessionOptions {
    TrustedSessionOptions {
        public_session,
        mode: SessionPermissionMode::Standard,
        ttl_seconds: lease.ttl_seconds,
        idle_ttl_seconds: lease.idle_ttl_seconds,
        capability_manifest_path: None,
        bounded_manifest_path: None,
    }
}

/// What the model is told when the SDK fails. A bad target, an expired
/// trusted session and a transport fault need different next actions, so
/// the reasons stay apart. They are bounded diagnostics, not tool output, so
/// they carry no screenshot or accessibility content.
fn driver_error(error: cua_driver_sdk::DriverError) -> String {
    use cua_driver_sdk::DriverError;
    match error {
        DriverError::InvalidArguments { reason, .. } => {
            format!("Invalid embedded CUA arguments: {reason}")
        }
        DriverError::Shutdown => "The embedded CUA runtime has shut down".to_string(),
        DriverError::ActionInterrupted { reason, .. } => {
            format!("Embedded CUA action was interrupted: {reason}")
        }
        error => {
            log::warn!("Embedded CUA runtime failed: {error}");
            format!("Embedded CUA runtime failed: {error}")
        }
    }
}

/// One CUA tool, as `cua-driver__<tool>`, on one binding.
pub(super) struct CuaTool {
    declaration: pi_ai::Tool,
    name: String,
    binding: Arc<Binding>,
    /// Whether the task's model sees images; without vision, screenshots
    /// are described for it.
    vision: bool,
    models: ModelRegistry,
    session_id: String,
}

impl CuaTool {
    pub(super) fn new(
        tool: &CatalogTool,
        binding: Arc<Binding>,
        vision: bool,
        models: ModelRegistry,
        session_id: String,
    ) -> Self {
        Self {
            declaration: pi_ai::Tool::new(
                format!("{TOOL_PREFIX}{}", tool.name),
                tool.description.clone(),
                tool.input_schema.clone(),
            ),
            name: tool.name.clone(),
            binding,
            vision,
            models,
            session_id,
        }
    }
}

#[async_trait]
impl AgentTool for CuaTool {
    fn declaration(&self) -> &pi_ai::Tool {
        &self.declaration
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        let arguments = match invocation.args {
            Value::Object(arguments) => arguments,
            _ => Map::new(),
        };
        let raw = match self
            .binding
            .call(&self.name, arguments, &invocation.cancel)
            .await
        {
            Ok(raw) => raw,
            Err(error) => return Ok(AgentToolResult::error(error)),
        };
        let result = match parse_result(&raw) {
            Ok(result) => result,
            Err(error) => return Ok(AgentToolResult::error(error)),
        };
        let describe = |screenshot: super::projection::Screenshot| {
            let models = self.models.clone();
            let session_id = self.session_id.clone();
            let tool_name = self.name.clone();
            let cancel = invocation.cancel.clone();
            async move {
                crate::agent::side_models::describe_screenshot(
                    &models,
                    &session_id,
                    &tool_name,
                    &screenshot.context,
                    (screenshot.index, screenshot.count),
                    (screenshot.data, screenshot.mime_type),
                    cancel,
                )
                .await
            }
        };
        Ok(result_for_model(
            &self.name,
            result,
            self.vision,
            &invocation.cancel,
            describe,
        )
        .await
        .unwrap_or_else(AgentToolResult::error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_identity_is_stable_opaque_and_account_scoped() {
        let first = session_identity("account-a", "20260904_1").unwrap();
        assert_eq!(
            first,
            session_identity(" account-a ", " 20260904_1 ").unwrap()
        );
        assert_ne!(first, session_identity("account-b", "20260904_1").unwrap());
        assert_ne!(first, session_identity("account-a", "20260904_2").unwrap());
        assert_ne!(first.public_session, first.transport_session);
        for identity in [&first.public_session, &first.transport_session] {
            assert!(!identity.contains("account-a"));
            assert!(!identity.contains("20260904_1"));
        }
        assert!(session_identity("", "task").is_err());
        assert!(session_identity("account", "").is_err());
    }

    /// The SDK's own runtime, with lifecycle tools only: a connection that
    /// closes leaves the task's session for the next one, a foreign
    /// transport cannot revive it, and an ended session revives.
    #[tokio::test]
    async fn a_stable_transport_reconnects_and_revives_only_its_task_session() {
        let driver = CuaDriver::create_configured(driver_options(60, 30)).unwrap();
        let identity = session_identity("test-account", "test-task").unwrap();
        let options = || {
            trusted_session_options(
                identity.public_session.clone(),
                Lease {
                    ttl_seconds: 60,
                    idle_ttl_seconds: 30,
                },
            )
        };

        let first = driver
            .create_trusted_session_for_transport(options(), &identity.transport_session)
            .unwrap();
        let started = first
            .call_tool("start_session".to_string(), "{}".to_string())
            .await
            .unwrap();
        assert!(!started.is_error, "{}", started.text);
        first.close();
        drop(first);

        let second = driver
            .create_trusted_session_for_transport(options(), &identity.transport_session)
            .unwrap();
        let inspected = second
            .call_tool("get_session".to_string(), "{}".to_string())
            .await
            .unwrap();
        assert!(!inspected.is_error, "{}", inspected.text);
        let ended = second
            .call_tool("end_session".to_string(), "{}".to_string())
            .await
            .unwrap();
        assert!(!ended.is_error, "{}", ended.text);
        second.close();
        drop(second);

        let foreign = driver
            .create_trusted_session_for_transport(options(), "foreign-transport")
            .unwrap();
        let refused = foreign
            .call_tool("start_session".to_string(), "{}".to_string())
            .await
            .unwrap();
        assert!(refused.is_error, "a different transport revived the task");
        foreign.close();
        drop(foreign);

        let replacement = driver
            .create_trusted_session_for_transport(options(), &identity.transport_session)
            .unwrap();
        let unavailable = replacement
            .call_tool("get_session".to_string(), "{}".to_string())
            .await
            .unwrap();
        assert!(
            unavailable.is_error,
            "ended lifecycle accepted an ordinary call"
        );
        let revived = replacement
            .call_tool("start_session".to_string(), "{}".to_string())
            .await
            .unwrap();
        assert!(!revived.is_error, "{}", revived.text);
        let structured: Value = serde_json::from_str(revived.structured_json.as_deref().unwrap())
            .expect("start_session should return structured lifecycle state");
        assert_eq!(structured["revived"], true);

        replacement.close();
        driver.shutdown().await.unwrap();
    }

    #[test]
    fn a_connection_is_renewed_before_either_sdk_limit() {
        let lease = Lease::STANDARD;
        let minutes = |count: u64| Duration::from_secs(count * 60);
        assert!(!lease.due(Duration::ZERO, Duration::ZERO));
        // Five minutes ahead of the half-hour idle limit.
        assert!(!lease.due(minutes(60), minutes(24)));
        assert!(lease.due(minutes(60), minutes(25)));
        // Five minutes ahead of the eight-hour lifetime, however busy.
        assert!(!lease.due(minutes(474), Duration::ZERO));
        assert!(lease.due(minutes(475), Duration::ZERO));
    }

    /// The SDK's own runtime with a one-second idle limit in place of the
    /// standard half hour: a run keeps computer use however long it leaves
    /// it unused, and only an unbound task loses it.
    #[tokio::test]
    async fn a_binding_keeps_computer_use_past_the_sdk_limits() {
        let lease = Lease {
            ttl_seconds: 60,
            idle_ttl_seconds: 1,
        };
        let past_idle_limit = Duration::from_millis(1_100);
        let driver =
            CuaDriver::create_configured(driver_options(lease.ttl_seconds, lease.idle_ttl_seconds))
                .unwrap();
        let open = |task: &'static str| {
            Binding::open_on(
                Arc::clone(&driver),
                Arc::new(Vec::new()),
                session_identity("test-account", task).unwrap(),
                lease,
            )
        };
        let binding = open("renewing-task").await.unwrap();
        let cancel = CancellationToken::new();
        let call = || binding.call_result("health_report", Map::new(), &cancel);
        let current = || Arc::clone(&lock(&binding.connection).session);

        let first = call().await.unwrap();
        assert!(!connection_lapsed(&first), "{}", first.text);

        // Unused past the idle limit, the connection is refused, and the
        // binding replaces it before the next call.
        tokio::time::sleep(past_idle_limit).await;
        let stale = current();
        let refused = stale
            .call_tool("health_report".to_string(), "{}".to_string())
            .await
            .unwrap();
        assert!(connection_lapsed(&refused), "{}", refused.text);
        let renewed = call().await.unwrap();
        assert!(!connection_lapsed(&renewed), "{}", renewed.text);
        assert!(!binding.is_current(&stale));

        // Should Maple's clock miss a lapse, the refusal renews instead:
        // expired, and revoked once another task binds in the meantime.
        for (other_task_binds, code) in [
            (false, "permission_denied"),
            (true, "authorization_revoked"),
        ] {
            tokio::time::sleep(past_idle_limit).await;
            let stale = {
                let mut connection = lock(&binding.connection);
                connection.opened = Instant::now();
                connection.used = Instant::now();
                Arc::clone(&connection.session)
            };
            if other_task_binds {
                open("other-task").await.unwrap().close();
            }
            let refused = stale
                .call_tool("health_report".to_string(), "{}".to_string())
                .await
                .unwrap();
            assert_eq!(
                refused.error_code.as_deref(),
                Some(code),
                "{}",
                refused.text
            );
            assert!(connection_lapsed(&refused), "{}", refused.text);
            let renewed = call().await.unwrap();
            assert!(!connection_lapsed(&renewed), "{}", renewed.text);
            assert!(!binding.is_current(&stale));
        }

        // An unbound task never reconnects.
        binding.close();
        assert_eq!(call().await.unwrap_err(), Binding::not_connected());
        drop(binding);
        driver.shutdown().await.unwrap();
    }
}
