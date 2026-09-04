use crate::{
    native_transport_root::TransportRootState,
    open_secret_config::{configured_pcr0_environment, normalize_api_url},
};
use anyhow::{anyhow, Result};
use axum::{
    body::Body,
    http::{header::ORIGIN, Method, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use maple_proxy::{create_app, Config};
use serde::{Deserialize, Serialize};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::Path;
use std::path::PathBuf;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tower_http::cors::{AllowHeaders, Any, CorsLayer};

#[cfg(any(target_os = "macos", target_os = "linux"))]
const MAPLE_APP_IDENTIFIER: &str = "cloud.opensecret.maple";
#[cfg(any(target_os = "macos", target_os = "linux"))]
static LEGACY_CONFIG_MIGRATION_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub host: String,
    pub port: u16,
    pub api_key: String,
    pub enabled: bool,
    #[serde(default = "default_cors")]
    pub enable_cors: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_url: Option<String>,
    #[serde(default)]
    pub auto_start: bool,
}

fn default_cors() -> bool {
    false
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8080,
            api_key: String::new(),
            enabled: false,
            enable_cors: false,
            backend_url: None,
            auto_start: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProxyStatus {
    pub running: bool,
    pub config: ProxyConfig,
    pub error: Option<String>,
}

pub struct ProxyState {
    handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    config: Arc<Mutex<ProxyConfig>>,
    running: Arc<Mutex<bool>>,
    lifecycle: Arc<Mutex<()>>,
}

impl ProxyState {
    pub fn new() -> Self {
        Self {
            handle: Arc::new(Mutex::new(None)),
            config: Arc::new(Mutex::new(ProxyConfig::default())),
            running: Arc::new(Mutex::new(false)),
            lifecycle: Arc::new(Mutex::new(())),
        }
    }

    pub async fn status(&self) -> ProxyStatus {
        ProxyStatus {
            running: *self.running.lock().await,
            config: self.config.lock().await.clone(),
            error: None,
        }
    }
}

// On Windows the proxy config lives in the roaming %APPDATA% profile, so a
// plaintext api_key could sync across machines in a domain/AAD environment.
// Store it in Windows Credential Manager instead and keep it out of the JSON.
// The Tauri identifier scopes both the config directory and credential entry,
// so managed workspace builds cannot read or overwrite production's key. The
// production identifier remains the legacy service name, requiring no migration.
// macOS/Linux keep their local plaintext-with-0o600 behavior unchanged.
#[cfg(target_os = "windows")]
const KEYRING_USER: &str = "proxy_api_key";

/// Persist the API key in Windows Credential Manager. An empty key clears the
/// entry. Returns `Ok(true)` when the key was stored (or cleared), `Ok(false)`
/// when no secure storage is available (caller keeps the plaintext fallback),
/// and `Err` when a clear was requested but the stale credential could not be
/// removed (caller must not scrub the JSON, or the old key would be resurrected).
#[cfg(target_os = "windows")]
fn store_api_key(app_handle: &AppHandle, key: &str) -> Result<bool> {
    let service = app_handle.config().identifier.clone();
    let entry = match keyring::Entry::new(&service, KEYRING_USER) {
        Ok(entry) => entry,
        Err(e) => {
            if key.is_empty() {
                return Err(anyhow!(
                    "Failed to access Credential Manager while clearing the API key: {e}"
                ));
            }
            log::warn!("Credential Manager unavailable, keeping plaintext config: {e}");
            return Ok(false);
        }
    };

    if key.is_empty() {
        return match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(true),
            // A hard delete failure leaves the old credential in place. Surface
            // it instead of reporting success: the caller must not scrub the
            // JSON, or the stale key would be resurrected on the next load.
            Err(e) => Err(anyhow!(
                "Failed to clear API key from Credential Manager: {e}"
            )),
        };
    }

    match entry.set_password(key) {
        Ok(()) => Ok(true),
        Err(keyring::Error::PlatformFailure(_)) | Err(keyring::Error::NoStorageAccess(_)) => {
            log::warn!("No secure credential storage available; keeping plaintext config");
            Ok(false)
        }
        Err(e) => Err(anyhow!(
            "Failed to store API key in Credential Manager: {e}"
        )),
    }
}

/// Load the API key from Windows Credential Manager.
/// - `Ok(Some(key))` — Credential Manager is available (`key` may be empty).
/// - `Ok(None)` — unavailable; caller should fall back to the JSON value.
#[cfg(target_os = "windows")]
fn load_api_key(app_handle: &AppHandle) -> Result<Option<String>> {
    let service = app_handle.config().identifier.clone();
    let entry = match keyring::Entry::new(&service, KEYRING_USER) {
        Ok(entry) => entry,
        Err(_) => return Ok(None),
    };

    match entry.get_password() {
        Ok(key) => Ok(Some(key)),
        Err(keyring::Error::NoEntry) => Ok(Some(String::new())),
        Err(keyring::Error::PlatformFailure(_)) | Err(keyring::Error::NoStorageAccess(_)) => {
            Ok(None)
        }
        Err(e) => Err(anyhow!(
            "Failed to read API key from Credential Manager: {e}"
        )),
    }
}

#[tauri::command]
pub async fn start_proxy(
    app_handle: AppHandle,
    state: State<'_, ProxyState>,
    transport_roots: State<'_, TransportRootState>,
    config: ProxyConfig,
) -> Result<ProxyStatus, String> {
    let _lifecycle_guard = state.lifecycle.lock().await;
    start_proxy_inner(app_handle, &state, &transport_roots, config).await
}

async fn start_proxy_inner(
    app_handle: AppHandle,
    state: &ProxyState,
    transport_roots: &TransportRootState,
    config: ProxyConfig,
) -> Result<ProxyStatus, String> {
    log::info!(
        "Starting proxy on {}:{} (cors={}, saved_credential_fallback={}, auto_start={})",
        config.host,
        config.port,
        config.enable_cors,
        !config.enable_cors,
        config.auto_start
    );

    // Check if proxy is already running
    let mut running = state.running.lock().await;
    if *running {
        drop(running);
        return Ok(state.status().await);
    }

    // Use backend URL from config or fall back to production
    let backend_url = config
        .backend_url
        .clone()
        .unwrap_or_else(|| "https://enclave.trymaple.ai".to_string());
    let backend_url = normalize_api_url(&backend_url)?;
    let cache_namespace_root = transport_roots.require(&backend_url).await?;

    let proxy_config = build_proxy_server_config(&config, backend_url)?
        .with_cache_namespace_root(cache_namespace_root);

    // Try to bind to the address first to check if port is available
    let addr = proxy_config
        .socket_addr()
        .map_err(|e| format!("Invalid address: {e}"))?;

    let listener = match TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(e) => {
            return Err(format!(
                "Failed to bind to {}:{} - {}",
                config.host, config.port, e
            ));
        }
    };

    // Starting successfully means the exact credential/configuration is also
    // durable. In particular, do not hide Credential Manager or disk failures
    // behind a running in-memory proxy that will change after restart.
    save_proxy_config(&app_handle, &config)
        .await
        .map_err(|error| format!("Failed to save proxy config: {error}"))?;
    *state.config.lock().await = config.clone();

    // maple-proxy owns the OpenAI-compatible transport, including the shared
    // 50 MiB request limit needed by Goose's image tool. Provider responses are
    // passed through unchanged.
    let app = apply_proxy_access_policy(proxy_config, config.enable_cors);

    // Spawn the proxy server
    let handle = tokio::spawn(async move {
        log::info!("Maple proxy server running on http://{addr}");
        if let Err(e) = axum::serve(listener, app).await {
            log::error!("Proxy server error: {e}");
        }
    });

    // Store the handle
    let mut handle_guard = state.handle.lock().await;
    *handle_guard = Some(handle);
    drop(handle_guard); // Release handle lock early

    *running = true;

    Ok(ProxyStatus {
        running: true,
        config,
        error: None,
    })
}

fn build_proxy_server_config(config: &ProxyConfig, backend_url: String) -> Result<Config, String> {
    let proxy_config = Config::new(config.host.clone(), config.port, backend_url)
        .with_pcr0_environment(configured_pcr0_environment()?)
        .with_debug(false)
        // Maple owns the browser boundary below so it can both list the
        // non-wildcard Authorization header and reject browser origins when
        // CORS is disabled. Do not install maple-proxy's permissive layer.
        .with_cors(false);

    // CORS deliberately allows every browser origin for compatibility. Never
    // combine that reachability with Maple's saved credential: browser-enabled
    // clients must provide their own Authorization header on every request.
    if config.enable_cors {
        Ok(proxy_config)
    } else {
        Ok(proxy_config.with_api_key(config.api_key.clone()))
    }
}

fn apply_proxy_access_policy(proxy_config: Config, enable_cors: bool) -> Router {
    let app = create_app(proxy_config);

    if enable_cors {
        app.layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
                // Authorization is not covered by `*` under Fetch. Mirroring
                // the preflight list supports it plus SDK-specific headers
                // without trusting a request that lacks a valid bearer key.
                .allow_headers(AllowHeaders::mirror_request()),
        )
    } else {
        app.layer(middleware::from_fn(reject_browser_request))
    }
}

async fn reject_browser_request(request: Request<Body>, next: Next) -> Response {
    // Disabling CORS alone only hides responses. A no-cors browser POST can
    // still reach loopback and spend the saved credential, so fail closed on
    // forbidden browser headers before maple-proxy sees the body. Fetch omits
    // Origin on some no-cors GET/HEAD requests but still sends Sec-Fetch-Site.
    if request.headers().contains_key(ORIGIN) || request.headers().contains_key("sec-fetch-site") {
        return StatusCode::FORBIDDEN.into_response();
    }

    next.run(request).await
}

#[tauri::command]
pub async fn stop_proxy(state: State<'_, ProxyState>) -> Result<ProxyStatus, String> {
    let _lifecycle_guard = state.lifecycle.lock().await;
    stop_proxy_inner(&state).await
}

async fn stop_proxy_inner(state: &ProxyState) -> Result<ProxyStatus, String> {
    log::info!("Stopping proxy");

    let mut running = state.running.lock().await;
    if !*running {
        drop(running);
        return Ok(state.status().await);
    }

    // Abort the proxy task
    let handle = state.handle.lock().await.take();
    if let Some(handle) = handle {
        handle.abort();
        // Await cancellation while lifecycle serialization is still held so a
        // subsequent start cannot race the old listener's teardown.
        let _ = handle.await;
    }

    *running = false;

    let config = state.config.lock().await.clone();

    // Config persists even when stopped (we don't auto-start anyway)

    Ok(ProxyStatus {
        running: false,
        config,
        error: None,
    })
}

#[tauri::command]
pub async fn get_proxy_status(state: State<'_, ProxyState>) -> Result<ProxyStatus, String> {
    let _lifecycle_guard = state.lifecycle.lock().await;
    Ok(state.status().await)
}

#[tauri::command]
pub async fn load_proxy_config(
    app_handle: AppHandle,
    state: State<'_, ProxyState>,
) -> Result<ProxyConfig, String> {
    let _lifecycle_guard = state.lifecycle.lock().await;
    load_saved_proxy_config(&app_handle)
        .await
        .map_err(|e| format!("Failed to load proxy config: {e}"))
}

#[tauri::command]
pub async fn save_proxy_settings(
    app_handle: AppHandle,
    state: State<'_, ProxyState>,
    config: ProxyConfig,
) -> Result<(), String> {
    let _lifecycle_guard = state.lifecycle.lock().await;
    save_proxy_config(&app_handle, &config)
        .await
        .map_err(|e| format!("Failed to save proxy config: {e}"))
}

#[tauri::command]
pub async fn stop_and_reset_proxy(
    app_handle: AppHandle,
    state: State<'_, ProxyState>,
) -> Result<ProxyStatus, String> {
    let _lifecycle_guard = state.lifecycle.lock().await;
    stop_proxy_inner(&state).await?;

    // Clear account-bound state without discarding app/workspace routing such
    // as the managed proxy port or backend URL.
    let mut config = match load_saved_proxy_config(&app_handle).await {
        Ok(config) => config,
        Err(_) => state.config.lock().await.clone(),
    };
    config.api_key.clear();
    config.enabled = false;
    config.auto_start = false;
    save_proxy_config(&app_handle, &config)
        .await
        .map_err(|error| format!("Failed to reset proxy config: {error}"))?;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if app_handle.config().identifier == MAPLE_APP_IDENTIFIER {
        scrub_legacy_proxy_config(
            &legacy_proxy_config_path()
                .map_err(|error| format!("Failed to locate legacy proxy config: {error}"))?,
            InvalidLegacyConfigPolicy::Remove,
        )
        .await
        .map_err(|error| format!("Failed to reset legacy proxy config: {error}"))?;
    }

    *state.config.lock().await = config.clone();

    Ok(ProxyStatus {
        running: false,
        config,
        error: None,
    })
}

#[tauri::command]
pub async fn test_proxy_port(host: String, port: u16) -> Result<bool, String> {
    // Try to bind to the address to check if it's available
    let addr = format!("{host}:{port}");
    match TcpListener::bind(&addr).await {
        Ok(_) => Ok(true), // Port is available
        Err(e) => {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                Ok(false) // Port is in use
            } else {
                Err(format!("Failed to test port: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::CONTENT_TYPE;

    #[test]
    fn new_and_legacy_unspecified_configs_disable_cors() {
        assert!(!ProxyConfig::default().enable_cors);

        let config: ProxyConfig = serde_json::from_value(serde_json::json!({
            "host": "127.0.0.1",
            "port": 8080,
            "api_key": "saved-key",
            "enabled": false
        }))
        .unwrap();

        assert!(!config.enable_cors);
    }

    #[test]
    fn explicit_cors_keeps_browser_access_but_removes_saved_credential_fallback() {
        let config = ProxyConfig {
            api_key: "saved-key".to_string(),
            enable_cors: true,
            ..ProxyConfig::default()
        };

        let server_config =
            build_proxy_server_config(&config, "https://example.invalid".to_string()).unwrap();

        assert!(!server_config.enable_cors);
        assert!(server_config.default_api_key.is_none());
        assert_eq!(
            server_config.pcr0_environment,
            configured_pcr0_environment().unwrap()
        );
    }

    #[test]
    fn cors_disabled_keeps_saved_credential_fallback_for_local_clients() {
        let config = ProxyConfig {
            api_key: "saved-key".to_string(),
            ..ProxyConfig::default()
        };

        let server_config =
            build_proxy_server_config(&config, "https://example.invalid".to_string()).unwrap();

        assert!(!server_config.enable_cors);
        assert_eq!(server_config.default_api_key.as_deref(), Some("saved-key"));
    }

    async fn serve_test_app(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), server)
    }

    #[tokio::test]
    async fn cors_disabled_rejects_preflight_free_browser_posts_before_using_saved_key() {
        let config = ProxyConfig {
            api_key: "saved-key".to_string(),
            ..ProxyConfig::default()
        };
        let server_config =
            build_proxy_server_config(&config, "https://example.invalid".to_string()).unwrap();
        let app = apply_proxy_access_policy(server_config, config.enable_cors);
        let (base_url, server) = serve_test_app(app).await;

        let response = reqwest::Client::new()
            .post(format!("{base_url}/v1/chat/completions"))
            .header(ORIGIN, "https://attacker.example")
            .header(CONTENT_TYPE, "text/plain")
            .body(r#"{"model":"test","messages":[]}"#)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        server.abort();
    }

    #[tokio::test]
    async fn cors_disabled_rejects_originless_browser_gets_using_fetch_metadata() {
        let config = ProxyConfig {
            api_key: "saved-key".to_string(),
            ..ProxyConfig::default()
        };
        let server_config =
            build_proxy_server_config(&config, "https://example.invalid".to_string()).unwrap();
        let app = apply_proxy_access_policy(server_config, config.enable_cors);
        let (base_url, server) = serve_test_app(app).await;

        let response = reqwest::Client::new()
            .get(format!("{base_url}/v1/models"))
            .header("sec-fetch-site", "cross-site")
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        server.abort();
    }

    #[tokio::test]
    async fn cors_enabled_preflight_explicitly_allows_bearer_authentication() {
        let config = ProxyConfig {
            api_key: "saved-key".to_string(),
            enable_cors: true,
            ..ProxyConfig::default()
        };
        let server_config =
            build_proxy_server_config(&config, "https://example.invalid".to_string()).unwrap();
        let app = apply_proxy_access_policy(server_config, config.enable_cors);
        let (base_url, server) = serve_test_app(app).await;

        let response = reqwest::Client::new()
            .request(
                reqwest::Method::OPTIONS,
                format!("{base_url}/v1/chat/completions"),
            )
            .header(ORIGIN, "https://browser.example")
            .header("access-control-request-method", "POST")
            .header(
                "access-control-request-headers",
                "authorization,content-type,x-stainless-retry-count,x-stainless-lang",
            )
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .unwrap(),
            "*"
        );
        let allowed_headers = response
            .headers()
            .get("access-control-allow-headers")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(allowed_headers.contains("authorization"));
        assert!(allowed_headers.contains("content-type"));
        assert!(allowed_headers.contains("x-stainless-retry-count"));
        assert!(allowed_headers.contains("x-stainless-lang"));
        server.abort();
    }

    #[tokio::test]
    async fn stop_waits_for_aborted_server_task() {
        struct DropFlag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let state = ProxyState::new();
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let task_dropped = Arc::clone(&dropped);
        let task = tokio::spawn(async move {
            let _drop_flag = DropFlag(task_dropped);
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        started_rx.await.unwrap();
        *state.handle.lock().await = Some(task);
        *state.running.lock().await = true;

        let status = stop_proxy_inner(&state).await.unwrap();

        assert!(!status.running);
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(state.handle.lock().await.is_none());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn config_migration_test_dir() -> PathBuf {
        let counter = LEGACY_CONFIG_MIGRATION_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "maple-proxy-config-migration-{}-{counter}",
            std::process::id()
        ))
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn migrates_legacy_config_once_and_scrubs_source() {
        use std::os::unix::fs::PermissionsExt;

        let root = config_migration_test_dir();
        let legacy_path = root.join("legacy/proxy_config.json");
        let target_path = root.join("app/proxy_config.json");
        tokio::fs::create_dir_all(legacy_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::create_dir_all(target_path.parent().unwrap())
            .await
            .unwrap();

        let original = ProxyConfig {
            host: "127.0.0.2".to_string(),
            port: 8765,
            api_key: "legacy-key".to_string(),
            enabled: true,
            enable_cors: false,
            backend_url: Some("https://example.invalid".to_string()),
            auto_start: true,
        };
        tokio::fs::write(&legacy_path, serde_json::to_vec(&original).unwrap())
            .await
            .unwrap();

        migrate_legacy_proxy_config(&legacy_path, &target_path)
            .await
            .unwrap();
        assert!(legacy_path.exists());
        let migrated: ProxyConfig =
            serde_json::from_slice(&tokio::fs::read(&target_path).await.unwrap()).unwrap();
        assert_eq!(migrated.port, 8765);
        assert_eq!(migrated.api_key, "legacy-key");
        assert!(migrated.enabled);
        assert!(migrated.auto_start);
        assert_eq!(
            tokio::fs::metadata(&target_path)
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let scrubbed: ProxyConfig =
            serde_json::from_slice(&tokio::fs::read(&legacy_path).await.unwrap()).unwrap();
        assert_eq!(scrubbed.host, original.host);
        assert_eq!(scrubbed.port, original.port);
        assert_eq!(scrubbed.enable_cors, original.enable_cors);
        assert_eq!(scrubbed.backend_url, original.backend_url);
        assert!(scrubbed.api_key.is_empty());
        assert!(!scrubbed.enabled);
        assert!(!scrubbed.auto_start);
        assert_eq!(
            tokio::fs::metadata(&legacy_path)
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let changed = ProxyConfig {
            port: 9999,
            api_key: "reintroduced-key".to_string(),
            enabled: true,
            auto_start: true,
            ..original
        };
        tokio::fs::write(&legacy_path, serde_json::to_vec(&changed).unwrap())
            .await
            .unwrap();
        migrate_legacy_proxy_config(&legacy_path, &target_path)
            .await
            .unwrap();
        let still_migrated: ProxyConfig =
            serde_json::from_slice(&tokio::fs::read(&target_path).await.unwrap()).unwrap();
        assert_eq!(still_migrated.port, 8765);
        let rescrubbed: ProxyConfig =
            serde_json::from_slice(&tokio::fs::read(&legacy_path).await.unwrap()).unwrap();
        assert_eq!(rescrubbed.port, 9999);
        assert!(rescrubbed.api_key.is_empty());
        assert!(!rescrubbed.enabled);
        assert!(!rescrubbed.auto_start);

        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn skips_invalid_legacy_config_without_blocking_fresh_state() {
        let root = config_migration_test_dir();
        let legacy_path = root.join("legacy/proxy_config.json");
        let target_path = root.join("app/proxy_config.json");
        tokio::fs::create_dir_all(legacy_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::create_dir_all(target_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&legacy_path, b"not valid JSON")
            .await
            .unwrap();

        migrate_legacy_proxy_config(&legacy_path, &target_path)
            .await
            .unwrap();
        assert!(!target_path.exists());
        assert!(legacy_path.exists());

        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn explicit_legacy_reset_removes_invalid_config() {
        let root = config_migration_test_dir();
        let legacy_path = root.join("legacy/proxy_config.json");
        tokio::fs::create_dir_all(legacy_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&legacy_path, b"not valid JSON")
            .await
            .unwrap();

        scrub_legacy_proxy_config(&legacy_path, InvalidLegacyConfigPolicy::Remove)
            .await
            .unwrap();

        assert!(!legacy_path.exists());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn explicit_legacy_reset_surfaces_valid_scrub_failure() {
        use std::os::unix::fs::PermissionsExt;

        let root = config_migration_test_dir();
        let legacy_dir = root.join("legacy");
        let legacy_path = legacy_dir.join("proxy_config.json");
        let target_path = root.join("app/proxy_config.json");
        tokio::fs::create_dir_all(&legacy_dir).await.unwrap();
        tokio::fs::create_dir_all(target_path.parent().unwrap())
            .await
            .unwrap();
        let config = ProxyConfig {
            api_key: "credential-that-must-not-survive".to_string(),
            enabled: true,
            auto_start: true,
            ..ProxyConfig::default()
        };
        tokio::fs::write(&legacy_path, serde_json::to_vec(&config).unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &target_path,
            serde_json::to_vec(&ProxyConfig::default()).unwrap(),
        )
        .await
        .unwrap();
        tokio::fs::set_permissions(&legacy_dir, std::fs::Permissions::from_mode(0o500))
            .await
            .unwrap();

        // An authoritative app-specific config keeps ordinary loads working.
        migrate_legacy_proxy_config(&legacy_path, &target_path)
            .await
            .unwrap();
        let result =
            scrub_legacy_proxy_config(&legacy_path, InvalidLegacyConfigPolicy::Remove).await;

        tokio::fs::set_permissions(&legacy_dir, std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        assert!(result.is_err());
        let unchanged: ProxyConfig =
            serde_json::from_slice(&tokio::fs::read(&legacy_path).await.unwrap()).unwrap();
        assert_eq!(unchanged.api_key, config.api_key);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn completed_migration_is_not_blocked_by_legacy_scrub_failure() {
        use std::os::unix::fs::PermissionsExt;

        let root = config_migration_test_dir();
        let legacy_dir = root.join("legacy");
        let legacy_path = legacy_dir.join("proxy_config.json");
        let target_path = root.join("app/proxy_config.json");
        tokio::fs::create_dir_all(&legacy_dir).await.unwrap();
        tokio::fs::create_dir_all(target_path.parent().unwrap())
            .await
            .unwrap();
        let config = ProxyConfig {
            api_key: "legacy-key".to_string(),
            enabled: true,
            auto_start: true,
            ..ProxyConfig::default()
        };
        tokio::fs::write(&legacy_path, serde_json::to_vec(&config).unwrap())
            .await
            .unwrap();
        tokio::fs::set_permissions(&legacy_dir, std::fs::Permissions::from_mode(0o500))
            .await
            .unwrap();

        let result = migrate_legacy_proxy_config(&legacy_path, &target_path).await;

        tokio::fs::set_permissions(&legacy_dir, std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        result.unwrap();
        let migrated: ProxyConfig =
            serde_json::from_slice(&tokio::fs::read(&target_path).await.unwrap()).unwrap();
        assert_eq!(migrated.api_key, config.api_key);
        let unchanged: ProxyConfig =
            serde_json::from_slice(&tokio::fs::read(&legacy_path).await.unwrap()).unwrap();
        assert_eq!(unchanged.api_key, config.api_key);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}

// Keep proxy state scoped to the Tauri application identifier on every platform.
// Production macOS/Linux installs copy the historical ~/.config/maple config on
// first use; workspace builds have unique identifiers and remain isolated.
async fn get_config_path(app_handle: &AppHandle) -> Result<PathBuf> {
    let app_dir = app_handle
        .path()
        .app_config_dir()
        .map_err(|e| anyhow!("Failed to resolve app config dir: {e}"))?;
    tokio::fs::create_dir_all(&app_dir).await?;
    let path = app_dir.join("proxy_config.json");

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if app_handle.config().identifier == MAPLE_APP_IDENTIFIER {
        migrate_legacy_proxy_config(&legacy_proxy_config_path()?, &path).await?;
    }

    Ok(path)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn legacy_proxy_config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("Failed to get home directory"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("maple")
        .join("proxy_config.json"))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn migrate_legacy_proxy_config(legacy_path: &Path, target_path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if !tokio::fs::try_exists(legacy_path).await? {
        return Ok(());
    }

    if tokio::fs::try_exists(target_path).await? {
        if let Err(error) =
            scrub_legacy_proxy_config(legacy_path, InvalidLegacyConfigPolicy::Preserve).await
        {
            // The app-specific config is already authoritative. Legacy cleanup
            // is retried on later loads and is mandatory during explicit reset.
            log::warn!(
                "Unable to scrub legacy proxy credentials from {}: {error}",
                legacy_path.display()
            );
        }
        return Ok(());
    }

    let contents = match tokio::fs::read(legacy_path).await {
        Ok(contents) => contents,
        Err(error) => {
            log::warn!(
                "Skipping unreadable legacy proxy config {}: {error}",
                legacy_path.display()
            );
            return Ok(());
        }
    };
    if let Err(error) = serde_json::from_slice::<ProxyConfig>(&contents) {
        log::warn!(
            "Skipping invalid legacy proxy config {}: {error}",
            legacy_path.display()
        );
        return Ok(());
    }

    let counter = LEGACY_CONFIG_MIGRATION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_name = format!(".proxy_config.migrate-{}-{counter}.tmp", std::process::id());
    let temp_path = target_path.with_file_name(temp_name);

    let migration = async {
        let mut temp = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp_path)
            .await?;
        temp.write_all(&contents).await?;
        temp.sync_all().await?;
        tokio::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o600)).await?;

        match tokio::fs::hard_link(&temp_path, target_path).await {
            Ok(()) => log::info!(
                "Migrated proxy configuration into {}",
                target_path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        Ok::<(), std::io::Error>(())
    }
    .await;

    let _ = tokio::fs::remove_file(&temp_path).await;
    migration?;

    // Keep a credential-free rollback fallback at the legacy path. The
    // app-specific copy is already authoritative, so cleanup failure is
    // best-effort here and remains a hard error during explicit reset.
    if let Err(error) =
        scrub_legacy_proxy_config(legacy_path, InvalidLegacyConfigPolicy::Preserve).await
    {
        log::warn!(
            "Unable to scrub legacy proxy credentials from {}: {error}",
            legacy_path.display()
        );
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Clone, Copy)]
enum InvalidLegacyConfigPolicy {
    Preserve,
    Remove,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn scrub_legacy_proxy_config(
    path: &Path,
    invalid_policy: InvalidLegacyConfigPolicy,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let contents = match tokio::fs::read(path).await {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut config: ProxyConfig = match serde_json::from_slice(&contents) {
        Ok(config) => config,
        Err(error) => {
            log::warn!(
                "Unable to parse legacy proxy config {} while removing credentials: {error}",
                path.display()
            );
            if matches!(invalid_policy, InvalidLegacyConfigPolicy::Remove) {
                tokio::fs::remove_file(path).await?;
            }
            return Ok(());
        }
    };

    let already_scrubbed = config.api_key.is_empty() && !config.enabled && !config.auto_start;
    let owner_only = tokio::fs::metadata(path).await?.permissions().mode() & 0o777 == 0o600;
    if already_scrubbed && owner_only {
        return Ok(());
    }

    config.api_key.clear();
    config.enabled = false;
    config.auto_start = false;

    let counter = LEGACY_CONFIG_MIGRATION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_name = format!(".proxy_config.scrub-{}-{counter}.tmp", std::process::id());
    let temp_path = path.with_file_name(temp_name);
    let rewrite = async {
        let mut temp = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp_path)
            .await?;
        temp.write_all(&serde_json::to_vec_pretty(&config)?).await?;
        temp.sync_all().await?;
        tokio::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o600)).await?;
        tokio::fs::rename(&temp_path, path).await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if rewrite.is_err() {
        let _ = tokio::fs::remove_file(&temp_path).await;
    }
    rewrite
}

async fn save_proxy_config(app_handle: &AppHandle, config: &ProxyConfig) -> Result<()> {
    let path = get_config_path(app_handle).await?;

    // On Windows, move the API key into Credential Manager and scrub it from
    // the JSON (the config dir is the roaming profile). Other platforms retain
    // the existing owner-only JSON behavior.
    #[cfg(target_os = "windows")]
    let json = {
        let scrubbed = match store_api_key(app_handle, &config.api_key) {
            Ok(true) => ProxyConfig {
                api_key: String::new(),
                ..config.clone()
            },
            Ok(false) => config.clone(),
            // Clearing the key failed: don't scrub the JSON and report success,
            // since the stale credential survives and would be resurrected on
            // the next load. Propagate so the failure is visible.
            Err(e) if config.api_key.is_empty() => return Err(e),
            // Storing failed for another reason: fall back to persisting the key
            // in plaintext JSON so it isn't lost.
            Err(e) => {
                log::warn!("{e}");
                config.clone()
            }
        };
        serde_json::to_string_pretty(&scrubbed)?
    };
    #[cfg(not(target_os = "windows"))]
    let json = serde_json::to_string_pretty(config)?;

    // Write the config file
    tokio::fs::write(&path, json).await?;

    // Set restrictive permissions on Unix systems (owner read/write only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        tokio::fs::set_permissions(&path, perms).await?;
    }

    Ok(())
}

pub async fn load_saved_proxy_config(app_handle: &AppHandle) -> Result<ProxyConfig> {
    let path = get_config_path(app_handle).await?;

    if !path.exists() {
        return Ok(ProxyConfig::default());
    }

    let json = tokio::fs::read_to_string(path).await?;
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut config: ProxyConfig = serde_json::from_str(&json)?;

    // On Windows, prefer the API key from Credential Manager; fall back to any
    // plaintext value still in the JSON if it's unavailable.
    #[cfg(target_os = "windows")]
    if let Some(key) = load_api_key(app_handle)? {
        if !key.is_empty() {
            config.api_key = key;
        }
    }

    Ok(config)
}

/// Initialize the proxy only after the renderer has registered event listeners
/// and installed the stable Transport V2 root for the saved backend origin.
#[tauri::command]
pub async fn init_proxy_on_startup(
    app_handle: AppHandle,
    proxy_state: State<'_, ProxyState>,
    transport_roots: State<'_, TransportRootState>,
) -> Result<(), String> {
    let _lifecycle_guard = proxy_state.lifecycle.lock().await;

    // Load saved config
    let config = load_saved_proxy_config(&app_handle)
        .await
        .map_err(|_| "Failed to load the saved proxy configuration".to_string())?;

    // Check if auto-start is enabled and we have an API key
    if config.auto_start && !config.api_key.is_empty() {
        log::info!("Auto-starting proxy from saved config");

        // Try to start the proxy
        match start_proxy_inner(
            app_handle.clone(),
            &proxy_state,
            &transport_roots,
            config.clone(),
        )
        .await
        {
            Ok(_) => {
                log::info!(
                    "Proxy auto-started successfully on {}:{}",
                    config.host,
                    config.port
                );
                // Optionally emit an event to notify the frontend
                let _ = app_handle.emit("proxy-autostarted", &config);
            }
            Err(e) => {
                log::error!("Failed to auto-start proxy: {e}");
                // Emit an event to notify the frontend of the failure
                let _ = app_handle.emit("proxy-autostart-failed", e);
            }
        }
    } else {
        log::info!("Proxy auto-start is disabled or no API key configured");
    }

    Ok(())
}
