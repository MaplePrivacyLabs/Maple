//! Process entry: picks the startup mode from the command line and runs
//! the desktop window, the ACP agent, or the OpenAI-compatible proxy.
//! Every mode shares the backend, settings, and logging below.

#[cfg(feature = "desktop")]
mod assets;
#[cfg(feature = "desktop")]
mod audio;
mod backend;
mod billing;
#[cfg(feature = "desktop")]
mod desktop;
mod env;
#[cfg(feature = "desktop")]
mod feedback;
mod framing;
mod host;
mod hosted_oauth;
#[cfg(feature = "desktop")]
mod keymap;
#[cfg(feature = "desktop")]
mod notify;
#[cfg(feature = "desktop")]
mod platform;
mod profile;
mod remote;
mod remote_updates;
mod self_launch;
mod settings;
#[cfg(feature = "desktop")]
mod shortcuts;
#[cfg(all(feature = "desktop", any(target_os = "linux", target_os = "windows")))]
mod tray;
#[cfg(feature = "desktop")]
mod ui;
#[cfg(feature = "desktop")]
mod update;
#[cfg(all(feature = "desktop", target_os = "windows"))]
mod windows_bootstrap;

use backend::AgentBackend;
use clap::{Args, Parser, Subcommand};

/// Shown when a mode was compiled out with `--no-default-features`.
#[cfg(not(all(feature = "desktop", feature = "acp", feature = "proxy")))]
fn disabled_mode(mode: &str, feature: &str) -> ! {
    eprintln!("maple-agent {mode} is not available: this build lacks the `{feature}` feature.");
    std::process::exit(2);
}

/// Command line for the binary. With no subcommand it opens the desktop
/// window; `acp` and `proxy` run headless services.
/// The `--version` string: package version plus the git revision baked in
/// by `build.rs`, so a running binary can be matched back to a checkout.
/// Clap wants a `&'static str` and both inputs are compile-time constants,
/// but `format!` is still runtime, hence the leak of one small string.
fn version_string() -> &'static str {
    let hash = option_env!("MAPLE_GIT_HASH").unwrap_or("unknown");
    Box::leak(format!("{} ({})", env!("CARGO_PKG_VERSION"), hash).into_boxed_str())
}

#[derive(Debug, Parser)]
#[command(name = "maple-agent", version = version_string(), about, disable_help_subcommand = true, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Print the immutable build profile as JSON and exit without opening state.
    #[arg(long, conflicts_with = "desktop_args")]
    build_info: bool,
    /// Internal read-only checker. Key material is accepted only on stdin.
    #[arg(
        long,
        hide = true,
        value_name = "STORE_V1_ROOT",
        conflicts_with = "build_info"
    )]
    internal_store_integrity_check: Option<std::path::PathBuf>,
    /// Internal Linux desktop-portal watcher; never opens application state.
    #[cfg(all(feature = "desktop", target_os = "linux"))]
    #[arg(long, hide = true, conflicts_with_all = ["build_info", "internal_store_integrity_check", "desktop_args"])]
    internal_theme_watch: bool,
    /// Credential-free native installer admission before any installed write.
    #[cfg(all(feature = "desktop", target_os = "windows"))]
    #[arg(long, hide = true, value_name = "INSTALL_ROOT", conflicts_with_all = ["build_info", "internal_store_integrity_check", "desktop_args", "internal_windows_installer_activate", "internal_windows_uninstall_cleanup", "internal_windows_restart"])]
    internal_windows_installer_preflight: Option<std::path::PathBuf>,
    #[cfg(all(feature = "desktop", target_os = "windows"))]
    #[arg(long, hide = true, num_args = 2, value_names = ["INSTALL_ROOT", "GENERATION"], conflicts_with_all = ["build_info", "internal_store_integrity_check", "desktop_args", "internal_windows_installer_preflight", "internal_windows_installer_activate", "internal_windows_uninstall_cleanup", "internal_windows_restart"])]
    internal_windows_installer_prepare: Vec<std::ffi::OsString>,
    #[cfg(all(feature = "desktop", target_os = "windows"))]
    #[arg(long, hide = true, value_name = "GENERATION", conflicts_with_all = ["build_info", "internal_store_integrity_check", "desktop_args", "internal_windows_uninstall_cleanup", "internal_windows_restart"])]
    internal_windows_installer_activate: Option<String>,
    #[cfg(all(feature = "desktop", target_os = "windows"))]
    #[arg(long, hide = true, conflicts_with_all = ["build_info", "internal_store_integrity_check", "desktop_args", "internal_windows_restart"])]
    internal_windows_uninstall_cleanup: bool,
    #[cfg(all(feature = "desktop", target_os = "windows"))]
    #[arg(long, hide = true, value_name = "PROCESS_IDENTITY", conflicts_with_all = ["build_info", "internal_store_integrity_check", "desktop_args"])]
    internal_windows_restart: Option<String>,
    #[command(subcommand)]
    mode: Option<Mode>,
    /// Arguments a desktop launcher may append (file paths, `%u` expansions).
    /// They are ignored so the window still opens.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    desktop_args: Vec<String>,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
enum Mode {
    /// Manage the local shared host. `serve` is normally started automatically.
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
    /// Manage this computer's remote access and paired computers.
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Serve the Agent Client Protocol on stdio.
    ///
    /// Trailing arguments belong to the ACP client and are ignored.
    #[command(disable_version_flag = true, disable_help_flag = true)]
    Acp {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        client_args: Vec<String>,
    },
    /// Serve an OpenAI-compatible HTTP endpoint in front of Maple.
    Proxy(ProxyArgs),
    /// Sign in with email and password and save the session for `acp`.
    Login(LoginArgs),
    /// Unlock Maple local data; the password is prompted on the controlling TTY.
    Unlock,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
enum HostCommand {
    #[command(hide = true)]
    Serve,
    /// Stop this profile's host and all its running tasks.
    Stop,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
enum RemoteCommand {
    Status,
    Enable,
    Disable,
    Discover,
    /// Display a new five-minute, single-attempt code on this computer.
    BeginPairing,
    CancelPairing,
    /// Pair with an endpoint from Discover; the code is prompted privately.
    Pair {
        endpoint: String,
    },
    Revoke {
        endpoint: String,
    },
}

fn run_remote(command: RemoteCommand) -> Result<(), String> {
    let backend = AgentBackend::new(configured_api_url(), String::new())?;
    require_open_cli(&backend)?;
    let (method, params) = match command {
        RemoteCommand::Status => ("remote_status", serde_json::json!([])),
        RemoteCommand::Enable => ("remote_enable", serde_json::json!([true])),
        RemoteCommand::Disable => ("remote_enable", serde_json::json!([false])),
        RemoteCommand::Discover => ("remote_discover", serde_json::json!([])),
        RemoteCommand::BeginPairing => ("remote_begin_pairing", serde_json::json!([])),
        RemoteCommand::CancelPairing => ("remote_cancel_pairing", serde_json::json!([])),
        RemoteCommand::Pair { endpoint } => {
            let code = rpassword::prompt_password("Code shown on the other computer: ")
                .map_err(|_| "Could not read pairing code")?;
            (
                "remote_pair",
                serde_json::json!([endpoint, code.trim().to_ascii_uppercase()]),
            )
        }
        RemoteCommand::Revoke { endpoint } => ("remote_revoke", serde_json::json!([endpoint])),
    };
    let value: serde_json::Value = backend
        .runtime_handle()
        .block_on(backend.remote_setting(method, params))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(|_| "Could not display remote status")?
    );
    Ok(())
}

/// Settings for `maple-agent login`. The password is always prompted for
/// so it never lands in shell history or a process listing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Args)]
struct LoginArgs {
    /// Account email; prompted for when omitted.
    #[arg(long)]
    email: Option<String>,
}

/// Settings for `maple-agent proxy`, from flags with environment fallbacks.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
struct ProxyArgs {
    /// Bind address.
    #[arg(long, env = "MAPLE_PROXY_HOST", default_value = "127.0.0.1")]
    host: String,
    /// Bind port.
    #[arg(long, env = "MAPLE_PORT", default_value_t = 8080)]
    port: u16,
    /// Maple API key for requests without an Authorization header.
    ///
    /// Never combined with --cors: a browser-reachable proxy must not
    /// spend a saved credential.
    #[arg(long, env = "MAPLE_API_KEY")]
    api_key: Option<String>,
    /// Allow browser origins; every request must then carry its own key.
    #[arg(
        long,
        env = "MAPLE_ENABLE_CORS",
        value_parser = clap::builder::BoolishValueParser::new(),
    )]
    cors: bool,
}

impl ProxyArgs {
    /// Rejects `--cors` together with a default key. This is checked here
    /// and not with clap's `conflicts_with`: an env var counts as present
    /// to clap even when it holds `0` or `false`.
    fn validate(&self) -> Result<(), String> {
        if self.cors && self.api_key.is_some() {
            return Err(
                "--cors (MAPLE_ENABLE_CORS) cannot be combined with a default API key \
                 (--api-key or MAPLE_API_KEY): a browser-reachable proxy must not spend \
                 a saved credential. Drop --cors or the key."
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// Process start time, for the startup milestone logs. Set once at the
/// top of `main`; `startup_elapsed` reads it from anywhere.
#[cfg(feature = "desktop")]
static PROCESS_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Milliseconds since `main` began.
#[cfg(feature = "desktop")]
pub(crate) fn startup_elapsed() -> u128 {
    PROCESS_START
        .get()
        .map(|start| start.elapsed().as_millis())
        .unwrap_or(0)
}

fn main() {
    #[cfg(all(feature = "desktop", target_os = "windows"))]
    if profile::PACKAGED {
        // Delegate before Clap so the immutable bootstrap does not freeze the
        // selected generation's CLI grammar or report its own old version.
        match windows_bootstrap::delegate_before_parse(profile::PROFILE) {
            Ok(Some(code)) => std::process::exit(code),
            Ok(None) => {}
            Err(message) => {
                eprintln!("{message}");
                std::process::exit(1);
            }
        }
    }
    let cli = Cli::parse();
    #[cfg(all(feature = "desktop", target_os = "windows"))]
    {
        // The stable launcher and installer modes never open application data,
        // credentials or a host. Their paths and code identities are checked
        // by the updater's native installer before mutation or delegation.
        let internal = if let Some(root) = &cli.internal_windows_installer_preflight {
            Some(
                std::env::current_exe()
                    .map_err(|_| "Could not locate the Windows installer checker".to_owned())
                    .and_then(|exe| {
                        maple_agent_updater::installer::preflight_windows_installation(
                            &exe,
                            root,
                            profile::PROFILE,
                        )
                        .map_err(|error| error.to_string())
                    }),
            )
        } else if !cli.internal_windows_installer_prepare.is_empty() {
            Some(
                std::env::current_exe()
                    .map_err(|_| "Could not locate the Windows installer checker".to_owned())
                    .and_then(|exe| {
                        let root =
                            std::path::PathBuf::from(&cli.internal_windows_installer_prepare[0]);
                        let generation = cli.internal_windows_installer_prepare[1]
                            .to_str()
                            .ok_or_else(|| "Invalid Windows generation".to_owned())?;
                        maple_agent_updater::installer::prepare_windows_installation(
                            &exe,
                            &root,
                            profile::PROFILE,
                            generation,
                        )
                        .map_err(|error| error.to_string())
                    }),
            )
        } else if let Some(generation) = &cli.internal_windows_installer_activate {
            Some(
                std::env::current_exe()
                    .map_err(|_| "Could not locate the Windows bootstrap".to_owned())
                    .and_then(|exe| {
                        maple_agent_updater::installer::activate_windows_installation(
                            &exe,
                            profile::PROFILE,
                            generation,
                        )
                        .map_err(|error| error.to_string())
                    }),
            )
        } else if cli.internal_windows_uninstall_cleanup {
            Some(
                std::env::current_exe()
                    .map_err(|_| "Could not locate the Windows bootstrap".to_owned())
                    .and_then(|exe| {
                        maple_agent_updater::installer::cleanup_windows_installation(
                            &exe,
                            profile::PROFILE,
                        )
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                    }),
            )
        } else {
            cli.internal_windows_restart
                .as_deref()
                .map(|identity| windows_bootstrap::run_restart(identity, profile::PROFILE))
        };
        if let Some(result) = internal {
            if let Err(error) = result {
                eprintln!("{error}");
                std::process::exit(1);
            }
            return;
        }
    }
    if cli.build_info {
        println!(
            "{}",
            serde_json::to_string(&profile::build_info()).expect("build metadata")
        );
        return;
    }
    #[cfg(all(feature = "desktop", target_os = "linux"))]
    if cli.internal_theme_watch {
        // The helper is a separate, credential-free process. It must remain
        // identifiable to the portal, but never reaches normal app startup.
        if platform::portal::run_helper().is_err() {
            eprintln!("Desktop theme watcher unavailable.");
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = host::hardening::apply() {
        eprintln!("{error}");
        std::process::exit(1);
    }
    if let Some(root) = cli.internal_store_integrity_check {
        let result = maple_agent::store::integrity::run_checker_stdio(&root);
        if let Err(error) = result {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    // SAFETY: argument parsing and hardening start no threads. No other
    // thread exists yet, so environment mutation cannot race a reader.
    unsafe { maple_agent::prepare_process_environment() };
    #[cfg(feature = "desktop")]
    let _ = PROCESS_START.set(std::time::Instant::now());
    match cli.mode {
        Some(Mode::Host { command }) => {
            init_logging(LogOutput::FileOnly);
            let result = match &command {
                HostCommand::Serve => host::run(configured_api_url()),
                HostCommand::Stop => host::stop(&configured_api_url()),
            };
            if let Err(error) = result {
                exit_host_error(&command, &error);
            }
        }
        Some(Mode::Remote { command }) => {
            init_logging(LogOutput::FileOnly);
            if let Err(error) = run_remote(command) {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        Some(Mode::Acp { .. }) => {
            // stdout is the ACP channel. Logs go to the log file only, so
            // the client's stderr capture stays quiet. An inherited
            // RUST_LOG belongs to the client, which may set it for its own
            // log, so it is ignored here or it would silence Maple's log.
            #[cfg(not(feature = "acp"))]
            disabled_mode("acp", "acp");
            #[cfg(feature = "acp")]
            {
                init_logging(LogOutput::FileOnly);
                if let Err(error) = run_acp() {
                    log::error!("{error}");
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            }
        }
        Some(Mode::Proxy(args)) => {
            if let Err(message) = args.validate() {
                eprintln!("{message}");
                std::process::exit(2);
            }
            #[cfg(not(feature = "proxy"))]
            {
                let _ = args;
                disabled_mode("proxy", "proxy");
            }
            #[cfg(feature = "proxy")]
            {
                init_logging(LogOutput::FileAndStderr);
                if let Err(error) = run_proxy(args) {
                    log::error!("{error}");
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            }
        }
        Some(Mode::Unlock) => {
            init_logging(LogOutput::FileOnly);
            if let Err(error) = run_unlock() {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        Some(Mode::Login(args)) => {
            init_logging(LogOutput::FileOnly);
            if let Err(error) = run_login(args) {
                log::error!("{error}");
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        None => {
            #[cfg(feature = "desktop")]
            desktop::run();
            #[cfg(not(feature = "desktop"))]
            disabled_mode("(desktop)", "desktop");
        }
    }
}

/// A detached host has no stderr, so leave a diagnostic before exiting. Only
/// fixed summaries reach the log: errors can contain service URLs or paths.
fn exit_host_error(command: &HostCommand, error: &str) -> ! {
    let operation = match command {
        HostCommand::Serve => "serve",
        HostCommand::Stop => "stop",
    };
    // env_logger's Pipe target writes and flushes each record synchronously,
    // and TeeWriter also flushes ERROR records. process::exit skips destructors.
    log::error!(
        "Maple host {operation} failed: {}",
        host_error_summary(error)
    );
    eprintln!("{error}");
    std::process::exit(1);
}

fn host_error_summary(error: &str) -> &'static str {
    if error.starts_with("failed to build the billing HTTP client:")
        || error == "builder error"
        || error.contains("no root certificates found")
        || error.contains("No CA certificates were loaded from the system")
    {
        "HTTP client initialization failed; check OS CA certificates and TLS trust configuration."
    } else if error.starts_with("Cannot open host lock:")
        || error.starts_with("Cannot acquire Maple host ownership:")
    {
        "Host profile lock is unavailable; check profile storage permissions and existing host processes."
    } else {
        "Host operation failed; check profile configuration and runtime dependencies."
    }
}

/// `maple-agent proxy`: an OpenAI-compatible endpoint in front of Maple's
/// enclave, for tools that speak the OpenAI API. Runs until killed.
#[cfg(feature = "proxy")]
fn run_proxy(args: ProxyArgs) -> Result<(), String> {
    use axum::http::{Method, StatusCode, header::ORIGIN};
    use axum::response::IntoResponse;
    use tower_http::cors::{AllowHeaders, Any, CorsLayer};

    let backend_url = maple_agent::maple_api::validate_api_url(&configured_api_url())?;
    let pcr0 = maple_agent::open_secret_config::configured_pcr0_environment()?;
    let mut config = maple_proxy::Config::new(args.host.clone(), args.port, backend_url)
        .with_pcr0_environment(pcr0)
        .with_debug(false)
        // The browser boundary is owned below so the Authorization header
        // can be allowed explicitly and browser requests rejected when CORS
        // is off; maple-proxy's permissive layer stays out.
        .with_cors(false);
    if let Some(api_key) = args.api_key.clone() {
        config = config.with_api_key(api_key);
    }
    let addr = config
        .socket_addr()
        .map_err(|error| format!("Invalid proxy address: {error}"))?;
    let app = maple_proxy::create_app(config);
    let app = if args.cors {
        app.layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
                // Authorization is not covered by `*` under Fetch; mirror
                // the preflight list so bearer keys and SDK headers pass.
                .allow_headers(AllowHeaders::mirror_request()),
        )
    } else {
        // Disabling CORS alone only hides responses. A no-cors browser POST
        // can still reach loopback and spend the saved key, so fail closed
        // on browser-only headers before the body is read.
        app.layer(axum::middleware::from_fn(
            |request: axum::extract::Request, next: axum::middleware::Next| async move {
                if request.headers().contains_key(ORIGIN)
                    || request.headers().contains_key("sec-fetch-site")
                {
                    return StatusCode::FORBIDDEN.into_response();
                }
                next.run(request).await
            },
        ))
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("Failed to start the proxy runtime: {error}"))?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|error| format!("Failed to bind {addr}: {error}"))?;
        log::info!(
            "Maple proxy listening on http://{addr} (cors={}, default key={})",
            args.cors,
            args.api_key.is_some()
        );
        eprintln!("Maple proxy listening on http://{addr}");
        eprintln!("  GET  /v1/models    POST /v1/chat/completions    POST /v1/embeddings");
        if args.api_key.is_none() {
            eprintln!("  Requests must send: Authorization: Bearer <Maple API key>");
        }
        axum::serve(listener, app)
            .await
            .map_err(|error| format!("Proxy server error: {error}"))
    })
}

fn configured_api_url() -> String {
    profile::runtime_value("MAPLE_API_URL", profile::API_URL)
}

/// `maple-agent acp`: a standalone ACP agent over stdio. It reuses the
/// sign-in saved by the desktop app and hosts its own agent runtime, so
/// the desktop app does not need to run.
#[cfg(feature = "acp")]
fn run_acp() -> Result<(), String> {
    host::acp(&configured_api_url())
}

/// `maple-agent login`: sign in from a terminal. The saved session is the
/// same one the desktop app writes, so `maple-agent acp` can run on a
/// machine that never opened the window.
fn run_login(args: LoginArgs) -> Result<(), String> {
    let backend = AgentBackend::new(configured_api_url(), String::new())?;
    require_open_cli(&backend)?;
    if let Some(user_id) = backend.saved_user_id() {
        eprintln!("A Maple sign-in is already saved; signing in again replaces it.");
        log::info!("login replaces the saved session for account {user_id}");
    }
    let email = match args.email {
        Some(email) => email,
        None => prompt_line("Email: ")?,
    };
    let password = rpassword::prompt_password("Password: ")
        .map_err(|error| format!("Could not read the password: {error}"))?;
    let session = backend
        .runtime_handle()
        .block_on(backend.login(email, password))?;
    let account = backend
        .runtime_handle()
        .block_on(backend.account(&session.user_id))
        .ok();
    match account.and_then(|account| account.email) {
        Some(email) => eprintln!("Signed in as {email}."),
        None => eprintln!("Signed in."),
    }
    eprintln!("`maple-agent acp` and the desktop app will use this sign-in.");
    Ok(())
}

fn require_open_cli(backend: &AgentBackend) -> Result<(), String> {
    let status: serde_json::Value = backend
        .runtime_handle()
        .block_on(backend.local_control("store_status", ()))?;
    let state: maple_agent::store::StoreState =
        serde_json::from_value(status.get("state").cloned().ok_or("Invalid store status")?)
            .map_err(|_| "Invalid store status")?;
    match state {
        maple_agent::store::StoreState::Open => Ok(()),
        maple_agent::store::StoreState::Locked => {
            Err("Maple is locked. Open Maple or run `maple-agent unlock`, then try again.".into())
        }
        maple_agent::store::StoreState::NeedsSetup => {
            Err("Maple needs local store setup. Open Maple to continue.".into())
        }
        maple_agent::store::StoreState::Refused { reason } => Err(reason.message().into()),
        _ => Err("Maple local data is not ready. Open Maple and try again.".into()),
    }
}

fn run_unlock() -> Result<(), String> {
    #[derive(serde::Serialize)]
    struct Unlock<'a> {
        password: &'a str,
    }
    let backend = std::sync::Arc::new(AgentBackend::new(configured_api_url(), String::new())?);
    let password = zeroize::Zeroizing::new(
        rpassword::prompt_password("Maple unlock password: ")
            .map_err(|_| "Could not read the unlock password")?,
    );
    let result: serde_json::Value = backend.runtime_handle().block_on(backend.local_control(
        "unlock",
        Unlock {
            password: &password,
        },
    ))?;
    if result
        .get("used_previous_password")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        eprintln!("That was your previous unlock password. Your current password is still active.");
    } else {
        eprintln!("Maple is unlocked.");
    }
    backend
        .runtime_handle()
        .block_on(backend.detach())
        .map_err(|_| "Could not detach from Maple")?;
    Ok(())
}

/// One trimmed line from the terminal, with `prompt` shown first.
fn prompt_line(prompt: &str) -> Result<String, String> {
    use std::io::{BufRead, Write};
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|error| format!("Could not read from the terminal: {error}"))?;
    let line = line.trim().to_string();
    if line.is_empty() {
        return Err("No input; nothing was changed.".to_string());
    }
    Ok(line)
}

/// Log to stderr and to the new `store-v1/logs/maple-agent.log` so a freeze or
/// crash leaves evidence on disk. `RUST_LOG` still controls the level;
/// the default is `info`. Panics are logged as well.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LogOutput {
    /// Log file plus stderr, with `RUST_LOG` honored. The desktop default.
    FileAndStderr,
    /// Log file only, with the default filter. For stdio protocol modes
    /// and the login prompt, whose terminal must stay clean.
    FileOnly,
}

fn init_logging(output: LogOutput) {
    let paths = backend::store_paths();
    let log_dir = paths.logs();
    let file = paths
        .prepare_control_root()
        .and_then(|paths| paths.open_log())
        .ok();
    const DEFAULT_FILTER: &str =
        "info,iroh=warn,iroh_relay=warn,noq=warn,noq_proto=warn,netwatch=warn,tracing::span=warn";
    let mut builder = match output {
        LogOutput::FileAndStderr => env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or(DEFAULT_FILTER),
        ),
        LogOutput::FileOnly => {
            let mut builder = env_logger::Builder::new();
            builder.parse_filters(DEFAULT_FILTER);
            builder
        }
    };
    let stderr = output == LogOutput::FileAndStderr;
    if let Some(file) = file {
        builder.target(env_logger::Target::Pipe(Box::new(TeeWriter {
            file: std::io::BufWriter::with_capacity(16 * 1024, file),
            stderr: stderr.then(std::io::stderr),
        })));
    }
    builder.init();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("panic: {info}");
        default_hook(info);
    }));
    log::info!(
        "maple-agent {} starting; log file: {}",
        env!("CARGO_PKG_VERSION"),
        log_dir.join("maple-agent.log").display()
    );
}

/// Buffered file sink plus stderr. Warnings and errors flush the file at
/// once so a crash still leaves them on disk; info lines are batched.
struct TeeWriter {
    file: std::io::BufWriter<std::fs::File>,
    stderr: Option<std::io::Stderr>,
}

impl std::io::Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.file.write_all(buf);
        let urgent = buf.windows(4).any(|w| w == b"WARN" || w == b"ERRO");
        if urgent || buf.windows(5).any(|w| w == b"panic") {
            let _ = self.file.flush();
        }
        if let Some(stderr) = &mut self.stderr {
            stderr.write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = self.file.flush();
        match &mut self.stderr {
            Some(stderr) => stderr.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, HostCommand, LoginArgs, Mode};
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<Option<Mode>, clap::Error> {
        Cli::try_parse_from(std::iter::once("maple-agent").chain(args.iter().copied()))
            .map(|cli| cli.mode)
    }

    #[test]
    fn host_error_summaries_include_only_fixed_guidance() {
        let private = "https://fake-user:fake-password@example.invalid/private?token=fake-token account=fake-account";
        for error in [
            format!("failed to build the billing HTTP client: builder error: {private}"),
            "builder error".to_string(),
            format!("no root certificates found: {private}"),
            format!("No CA certificates were loaded from the system: {private}"),
        ] {
            assert_eq!(
                super::host_error_summary(&error),
                "HTTP client initialization failed; check OS CA certificates and TLS trust configuration."
            );
        }
        for error in [
            format!("Cannot open host lock: /private/fake-account: {private}"),
            format!("Cannot acquire Maple host ownership: {private}"),
        ] {
            assert_eq!(
                super::host_error_summary(&error),
                "Host profile lock is unavailable; check profile storage permissions and existing host processes."
            );
        }
        assert_eq!(
            super::host_error_summary(private),
            "Host operation failed; check profile configuration and runtime dependencies."
        );
    }

    #[test]
    fn host_error_exit_leaves_sanitized_log_after_process_exit() {
        const CHILD_OPERATION: &str = "MAPLE_TEST_HOST_ERROR_LOG_CHILD";
        if let Ok(operation) = std::env::var(CHILD_OPERATION) {
            super::init_logging(super::LogOutput::FileOnly);
            let command = match operation.as_str() {
                "serve" => HostCommand::Serve,
                "stop" => HostCommand::Stop,
                _ => panic!("invalid child operation"),
            };
            super::exit_host_error(
                &command,
                "failed to build the billing HTTP client: builder error: https://fake-user:fake-password@example.invalid/private?token=fake-token account=fake-account",
            );
        }

        for operation in ["serve", "stop"] {
            let root = tempfile::tempdir().unwrap();
            let data = root.path().join("data");
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::host_error_exit_leaves_sanitized_log_after_process_exit",
                ])
                .env(CHILD_OPERATION, operation)
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_DATA_HOME", &data)
                .env("RUST_LOG", "off")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(1));
            let log = std::fs::read_to_string(
                data.join(crate::profile::DATA_NAMESPACE)
                    .join("store-v1/logs/maple-agent.log"),
            )
            .unwrap();
            assert!(log.contains("ERROR"));
            assert!(log.contains(&format!("Maple host {operation} failed:")));
            assert!(log.contains("check OS CA certificates and TLS trust configuration"));
            for private in [
                "https://",
                "fake-user",
                "fake-password",
                "example.invalid",
                "?token=",
                "fake-token",
                "fake-account",
            ] {
                assert!(!log.contains(private), "private input reached the log");
            }
        }
    }

    #[test]
    fn build_info_is_a_standalone_exit_mode() {
        let cli = Cli::try_parse_from(["maple-agent", "--build-info"]).unwrap();
        assert!(cli.build_info);
        assert!(cli.mode.is_none());
        assert!(cli.desktop_args.is_empty());
        assert!(parse(&["--build-info", "login"]).is_err());
        assert!(parse(&["--build-info", "some/file.txt"]).is_err());
    }

    #[test]
    fn login_takes_an_optional_email_and_never_a_password() {
        assert_eq!(
            parse(&["login"]).unwrap(),
            Some(Mode::Login(LoginArgs { email: None }))
        );
        assert_eq!(
            parse(&["login", "--email", "a@b.c"]).unwrap(),
            Some(Mode::Login(LoginArgs {
                email: Some("a@b.c".into())
            }))
        );
        assert!(parse(&["login", "--password", "x"]).is_err());
    }

    #[test]
    fn version_flags_print_the_package_version() {
        let expected = format!(
            "maple-agent {} ({})",
            env!("CARGO_PKG_VERSION"),
            option_env!("MAPLE_GIT_HASH").unwrap_or("unknown")
        );
        for flag in ["--version", "-V"] {
            let error = parse(&[flag]).expect_err("version exits early");
            assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
            assert_eq!(error.to_string().trim(), expected);
        }
    }

    #[test]
    fn acp_subcommand_keeps_precedence_over_following_flags() {
        assert!(matches!(parse(&["acp"]), Ok(Some(Mode::Acp { .. }))));
        assert!(matches!(
            parse(&["acp", "--version"]),
            Ok(Some(Mode::Acp { .. }))
        ));
    }

    #[test]
    fn proxy_flags_parse_with_defaults() {
        let Ok(Some(Mode::Proxy(args))) = parse(&["proxy"]) else {
            panic!("proxy without flags must parse");
        };
        assert_eq!(args.host, "127.0.0.1");
        assert_eq!(args.port, 8080);
        assert!(!args.cors);

        let Ok(Some(Mode::Proxy(args))) =
            parse(&["proxy", "--host", "0.0.0.0", "--port", "9999", "--cors"])
        else {
            panic!("proxy flags must parse");
        };
        assert_eq!(
            (args.host.as_str(), args.port, args.cors),
            ("0.0.0.0", 9999, true)
        );

        assert!(parse(&["proxy", "--port", "x"]).is_err());
        assert!(parse(&["proxy", "--bogus"]).is_err());
        let Ok(Some(Mode::Proxy(args))) = parse(&["proxy", "--cors", "--api-key", "k"]) else {
            panic!("clap accepts the pair; validate rejects it");
        };
        assert!(args.validate().is_err());
        let Ok(Some(Mode::Proxy(args))) = parse(&["proxy", "--api-key", "k"]) else {
            panic!("key alone must parse");
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn other_arguments_keep_desktop_startup() {
        assert!(matches!(parse(&[]), Ok(None)));
        assert!(matches!(parse(&["--unknown"]), Ok(None)));
        assert!(matches!(parse(&["some/file.txt"]), Ok(None)));
    }
}
