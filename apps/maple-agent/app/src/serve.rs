//! `maple-agent serve`: publish this machine's agent runtime to paired
//! clients over the LAN or a Tailscale network.
//!
//! The host signs in on its own (`maple-agent login`) and holds its own
//! credentials; clients bring nothing but their device key. A one-time code
//! from `serve pair` admits a device into the saved account; `serve devices`
//! lists and revokes that account's devices. One server per data root,
//! enforced with a lock file.

use clap::{Args, Subcommand};

use crate::remote::DEFAULT_LISTEN;

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ServeArgs {
    #[command(subcommand)]
    pub command: Option<ServeCommand>,
    /// Address to listen on. Pairing is the gate, so every interface is
    /// the default; give one address (a Tailscale IP) to narrow it.
    #[arg(long, env = "MAPLE_SERVE_LISTEN", default_value = DEFAULT_LISTEN)]
    pub listen: String,
    /// Name clients show for this host. Defaults to the machine's hostname.
    #[arg(long, env = "MAPLE_SERVE_NAME")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum ServeCommand {
    /// Publish a one-time pairing code for a new device. The running host
    /// accepts it for five minutes.
    Pair,
    /// Devices paired with this host.
    Devices {
        #[command(subcommand)]
        command: DevicesCommand,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum DevicesCommand {
    List,
    /// Forget a device by public key or by name. Its live connections end
    /// within seconds.
    Revoke {
        device: String,
    },
}

#[cfg(feature = "serve")]
pub use enabled::run;

#[cfg(feature = "serve")]
mod enabled {
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt as _;
    use std::sync::Arc;

    use super::{DevicesCommand, ServeArgs, ServeCommand};
    use crate::backend::{AgentBackend, RestoreOutcome};
    use crate::remote::host::{self, Hosting};

    pub fn run(args: ServeArgs) -> Result<(), String> {
        let ServeArgs {
            command,
            listen,
            name,
        } = args;
        match command {
            None => run_server(&listen, name),
            Some(ServeCommand::Pair) => publish_code(&saved_account()?),
            Some(ServeCommand::Devices {
                command: DevicesCommand::List,
            }) => list_devices(&saved_account()?),
            Some(ServeCommand::Devices {
                command: DevicesCommand::Revoke { device },
            }) => revoke_device(&saved_account()?, &device),
        }
    }

    /// Tell systemd how the service is doing, when it asked
    /// (`NOTIFY_SOCKET` set). Silent everywhere else. Only the main process
    /// may report, so this is the command's alone.
    fn sd_notify(state: &str) {
        #[cfg(unix)]
        {
            let Some(socket) = std::env::var_os("NOTIFY_SOCKET") else {
                return;
            };
            let mut path = socket.into_encoded_bytes();
            // An abstract socket is written with a leading `@`; the
            // address needs a NUL there.
            if path.first() == Some(&b'@') {
                path[0] = 0;
            }
            let Ok(socket) = std::os::unix::net::UnixDatagram::unbound() else {
                return;
            };
            let sent = if path.first() == Some(&0) {
                #[cfg(target_os = "linux")]
                {
                    use std::os::linux::net::SocketAddrExt as _;
                    std::os::unix::net::SocketAddr::from_abstract_name(&path[1..])
                        .and_then(|address| socket.send_to_addr(state.as_bytes(), &address))
                }
                #[cfg(not(target_os = "linux"))]
                {
                    Err(std::io::Error::other("abstract sockets are Linux only"))
                }
            } else {
                socket.send_to(
                    state.as_bytes(),
                    std::path::PathBuf::from(std::ffi::OsString::from_vec(path)),
                )
            };
            if let Err(error) = sent {
                log::debug!("sd_notify failed: {error}");
            }
        }
        #[cfg(not(unix))]
        let _ = state;
    }

    const NO_SIGN_IN: &str =
        "No saved Maple sign-in on this machine. Run `maple-agent login` first.";

    /// The account the device commands act on: the one a host here would
    /// serve. Read from the saved sign-in without contacting the server.
    fn saved_account() -> Result<String, String> {
        AgentBackend::new(crate::configured_api_url())?
            .saved_user_id()
            .ok_or_else(|| NO_SIGN_IN.to_string())
    }

    fn run_server(listen: &str, name: Option<String>) -> Result<(), String> {
        let backend = Arc::new(AgentBackend::new(crate::configured_api_url())?);
        let saved = backend
            .saved_user_id()
            .ok_or_else(|| NO_SIGN_IN.to_string())?;
        let user_id = match backend.restore_outcome_now() {
            RestoreOutcome::Valid(user_id) => user_id,
            RestoreOutcome::Rejected => {
                return Err(
                    "The saved Maple sign-in was rejected. Run `maple-agent login` again."
                        .to_string(),
                );
            }
            RestoreOutcome::Unavailable => {
                // A host that boots before the network (a systemd unit at
                // login, a laptop off Wi-Fi) still serves: the sign-in is
                // kept, requests report the server state, and the sign-in
                // is retried behind them until it goes through.
                log::warn!("the Maple server could not be reached; serving with the saved sign-in");
                eprintln!(
                    "The Maple server could not be reached. Serving anyway; requests fail \
                     until the sign-in goes through, which is retried in the background."
                );
                retry_sign_in(&backend);
                saved
            }
        };
        crate::adopt_legacy_session_defaults(&backend, &user_id);
        let local_host = backend.local_host(&user_id);
        let name = name.unwrap_or_else(crate::env::hostname);
        let runtime = backend.runtime_handle();
        let hosting = runtime.block_on(Hosting::start(local_host, &user_id, listen, name))?;
        eprintln!(
            "Serving as host \"{}\" ({}) on {}.",
            hosting.name, hosting.host_id, hosting.listen
        );
        eprintln!("Pair a device with `maple-agent serve pair`. Stop with Ctrl-C.");
        // Under systemd (`Type=notify`) the unit is up once the port is
        // bound, not when the process forked.
        sd_notify(&format!(
            "READY=1\nSTATUS=Serving as {} on {}",
            hosting.name, hosting.listen
        ));
        runtime.block_on(async {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate() => {}
            }
        });
        sd_notify("STOPPING=1");
        // Wait for the port and the lock to go: a restart right after
        // (systemd's `Restart=`) must be able to bind.
        runtime.block_on(hosting.stop());
        Ok(())
    }

    /// Validate the saved sign-in again, with growing pauses, until the
    /// server answers. Requests that need the session wait for each
    /// attempt (`AgentBackend::restore_in_background`) and fail between
    /// them. A rejection ends the retries; the host stays up so the
    /// operator sees the error on the next request and in the log.
    fn retry_sign_in(backend: &Arc<AgentBackend>) {
        let runtime = backend.runtime_handle();
        let backend = Arc::clone(backend);
        runtime.spawn(async move {
            let mut pause = std::time::Duration::from_secs(5);
            loop {
                tokio::time::sleep(pause).await;
                match backend.restore_in_background().await {
                    Ok(RestoreOutcome::Valid(_)) => {
                        log::info!("the saved sign-in went through");
                        return;
                    }
                    Ok(RestoreOutcome::Rejected) => {
                        log::error!("the saved sign-in was rejected; run `maple-agent login`");
                        return;
                    }
                    Ok(RestoreOutcome::Unavailable) | Err(_) => {
                        pause = (pause * 2).min(std::time::Duration::from_secs(5 * 60));
                    }
                }
            }
        });
    }

    /// Resolves on SIGTERM, which `systemctl stop` sends. Never resolves
    /// where there is no such signal.
    async fn terminate() {
        #[cfg(unix)]
        {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(error) => {
                    log::warn!("cannot listen for SIGTERM: {error}");
                    std::future::pending::<()>().await;
                }
            }
        }
        #[cfg(not(unix))]
        std::future::pending::<()>().await;
    }

    fn publish_code(user_id: &str) -> Result<(), String> {
        let (code, _pending) = host::publish_pairing_code(user_id)?;
        // The code goes to stdout so it can be piped; guidance to stderr.
        println!("{}", code.display());
        eprintln!(
            "Pairing code published. It admits one device and expires in {} minutes.",
            maple_remote::pairing::CODE_TTL.as_secs() / 60
        );
        match host::read_state(&crate::remote::remote_dir()?) {
            Some(state) => eprintln!(
                "In the Maple app, add host \"{}\" at {} and enter the code.",
                state.name, state.listen
            ),
            None => eprintln!(
                "No host is running here. Start `maple-agent serve` or turn on remote \
                 connections in the app before the code expires."
            ),
        }
        Ok(())
    }

    fn list_devices(user_id: &str) -> Result<(), String> {
        let devices = host::list_devices(user_id)?;
        if devices.is_empty() {
            eprintln!("No paired devices. Publish a code with `maple-agent serve pair`.");
            return Ok(());
        }
        for device in devices {
            println!(
                "{}\t{}\t{}",
                device.public_key,
                device.name,
                device.user_id.as_deref().unwrap_or("-")
            );
        }
        Ok(())
    }

    fn revoke_device(user_id: &str, device: &str) -> Result<(), String> {
        let removed = host::revoke_device(user_id, device)?;
        eprintln!(
            "Revoked {} ({}). A live connection from it ends within seconds.",
            removed.name, removed.public_key
        );
        Ok(())
    }
}
