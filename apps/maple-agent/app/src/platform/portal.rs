//! System appearance through a credential-free instance of this executable.
//!
//! The GUI remains non-dumpable from startup. Only this helper stays visible
//! through /proc so xdg-desktop-portal can identify its caller. The helper never
//! opens Maple state; its only output is a bounded stream of theme names.

use std::{io, io::Write as _, process::Stdio, time::Duration};

use futures_lite::StreamExt as _;
use tokio::io::AsyncReadExt as _;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_FRAME: usize = 8;
const DESKTOP_ENVIRONMENT: &[&str] = &["DBUS_SESSION_BUS_ADDRESS", "XDG_RUNTIME_DIR"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scheme {
    Dark,
    Light,
    Default,
}

impl Scheme {
    fn frame(self) -> &'static [u8] {
        match self {
            Self::Dark => b"dark\n",
            Self::Light => b"light\n",
            Self::Default => b"default\n",
        }
    }

    fn parse(frame: &[u8]) -> io::Result<Self> {
        match frame {
            b"dark\n" => Ok(Self::Dark),
            b"light\n" => Ok(Self::Light),
            b"default\n" => Ok(Self::Default),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid theme frame",
            )),
        }
    }

    fn appearance(self) -> gpui::WindowAppearance {
        match self {
            Self::Dark => gpui::WindowAppearance::Dark,
            Self::Light | Self::Default => gpui::WindowAppearance::Light,
        }
    }
}

impl From<ashpd::desktop::settings::ColorScheme> for Scheme {
    fn from(value: ashpd::desktop::settings::ColorScheme) -> Self {
        use ashpd::desktop::settings::ColorScheme;
        match value {
            ColorScheme::PreferDark => Self::Dark,
            ColorScheme::PreferLight => Self::Light,
            ColorScheme::NoPreference => Self::Default,
        }
    }
}

/// This is a terminal startup branch: no logging, store, backend, or GUI.
pub(crate) fn run_helper() -> io::Result<()> {
    crate::host::hardening::disable_core_dumps().map_err(io::Error::other)?;
    // Parent lifetime is represented by its private stdin pipe, not a PID
    // lookup. EOF also covers GUI crashes and prevents orphaned subscribers.
    std::thread::Builder::new()
        .name("theme-parent".into())
        .spawn(|| {
            use std::io::Read as _;
            let _ = io::stdin().read(&mut [0_u8]);
            std::process::exit(0);
        })?;
    futures_lite::future::block_on(async {
        // The GUI owns the startup deadline and kills an unresponsive helper.
        let settings = ashpd::desktop::settings::Settings::new()
            .await
            .map_err(|_| io::Error::other("Desktop portal unavailable"))?;
        // Subscribe before reading so a concurrent desktop switch cannot
        // fall into a gap between the initial read and the subscription.
        let mut changes = settings
            .receive_color_scheme_changed()
            .await
            .map_err(|_| io::Error::other("Desktop portal subscription unavailable"))?;
        let initial = settings
            .color_scheme()
            .await
            .map_err(|_| io::Error::other("Desktop portal appearance unavailable"))?;
        let mut output = io::stdout().lock();
        let mut previous = Scheme::from(initial);
        output
            .write_all(previous.frame())
            .and_then(|()| output.flush())?;
        while let Some(value) = changes.next().await {
            let scheme = Scheme::from(value);
            if scheme != previous {
                output
                    .write_all(scheme.frame())
                    .and_then(|()| output.flush())?;
                previous = scheme;
            }
        }
        Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "Desktop portal stream closed",
        ))
    })
}

fn helper_command() -> io::Result<std::process::Command> {
    // Resolve the bundled loader/payload before clearing package environment.
    let mut command = crate::self_launch::child_command().map_err(io::Error::other)?;
    configure_helper(&mut command, |name| std::env::var_os(name));
    Ok(command)
}

fn configure_helper(
    command: &mut std::process::Command,
    mut environment: impl FnMut(&str) -> Option<std::ffi::OsString>,
) {
    command
        .env_clear()
        .current_dir("/")
        .arg("--internal-theme-watch");
    for name in DESKTOP_ENVIRONMENT {
        if let Some(value) = environment(name) {
            command.env(name, value);
        }
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // The helper stays dumpable, so same-user processes can open its
    // /proc/<pid>/fd entries. Native GUI libraries may leak descriptors without
    // CLOEXEC; close everything past stdio before exec. Kernels without
    // close_range fail the spawn and the GUI keeps its native appearance.
    // SAFETY: the closure makes one async-signal-safe syscall and allocates nothing.
    unsafe {
        use std::os::unix::process::CommandExt as _;
        command.pre_exec(|| {
            if libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 0_u32) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Start once per desktop process. No portal calls occur on the UI thread.
pub(crate) fn start(backend: &crate::backend::AgentBackend, cx: &mut gpui::App) {
    let (sender, mut receiver) = tokio::sync::watch::channel(None);
    let mut task = Some(backend.spawn(async move {
        // Package resolution can read filesystem metadata. Keep that off the
        // UI thread along with subprocess startup and pipe reads.
        let result = async {
            let command = helper_command()?;
            monitor(command, sender).await
        }
        .await;
        if result.is_err() {
            log::warn!("Desktop theme watcher stopped; retaining the last system appearance");
        }
    }));
    cx.on_app_quit(move |_| {
        let task = task.take();
        async move {
            if let Some(task) = task {
                task.abort();
                let _ = task.await;
            }
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        while receiver.changed().await.is_ok() {
            let scheme = *receiver.borrow_and_update();
            if let Some(scheme) = scheme {
                cx.update(|cx| {
                    crate::ui::theme::set_system_appearance(scheme.appearance());
                    if crate::ui::theme::preference() == crate::ui::theme::Preference::System {
                        cx.refresh_windows();
                    }
                });
            }
        }
    })
    .detach();
}

async fn monitor(
    command: std::process::Command,
    sender: tokio::sync::watch::Sender<Option<Scheme>>,
) -> io::Result<()> {
    let mut child = tokio::process::Command::from(command)
        .kill_on_drop(true)
        .spawn()?;
    let lifetime = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("Missing lifetime pipe"))?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("Missing theme pipe"))?;
    let result = async {
        let initial = tokio::select! {
            _ = sender.closed() => return Ok(()),
            value = tokio::time::timeout(STARTUP_TIMEOUT, read_frame(&mut output)) => {
                value.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Theme watcher timed out"))??
            }
        };
        sender.send_replace(Some(initial));
        loop {
            tokio::select! {
                _ = sender.closed() => return Ok(()),
                value = read_frame(&mut output) => {
                    let scheme = value?;
                    sender.send_if_modified(|current| {
                        if *current == Some(scheme) { return false; }
                        *current = Some(scheme);
                        true
                    });
                }
            }
        }
    }.await;
    drop(lifetime);
    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}

async fn read_frame(input: &mut (impl tokio::io::AsyncRead + Unpin)) -> io::Result<Scheme> {
    let mut frame = [0; MAX_FRAME];
    for index in 0..MAX_FRAME {
        frame[index] = input.read_u8().await?;
        if frame[index] == b'\n' {
            return Scheme::parse(&frame[..=index]);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "Oversized theme frame",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_are_bounded_and_strict() {
        for (mut frame, expected) in [
            (b"dark\n".as_slice(), Scheme::Dark),
            (b"light\n", Scheme::Light),
            (b"default\n", Scheme::Default),
        ] {
            assert_eq!(read_frame(&mut frame).await.unwrap(), expected);
        }
        for mut frame in [
            b"DARK\n".as_slice(),
            b"dark\r\n",
            b"\n",
            b"dark",
            b"anything-too-long\n",
        ] {
            assert!(read_frame(&mut frame).await.is_err());
        }
    }

    #[tokio::test]
    async fn reads_one_frame_without_consuming_the_next() {
        let mut input = b"dark\nlight\ndefault\n".as_slice();
        assert_eq!(read_frame(&mut input).await.unwrap(), Scheme::Dark);
        assert_eq!(read_frame(&mut input).await.unwrap(), Scheme::Light);
        assert_eq!(read_frame(&mut input).await.unwrap(), Scheme::Default);
    }

    fn idle_child() -> std::process::Command {
        let mut command = std::process::Command::new("sleep");
        command
            .arg("60")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    #[tokio::test]
    async fn closed_ui_receiver_stops_and_reaps_an_idle_child() {
        let (sender, receiver) = tokio::sync::watch::channel(None);
        drop(receiver);
        tokio::time::timeout(Duration::from_secs(1), monitor(idle_child(), sender))
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn helper_startup_has_a_deadline() {
        let (sender, _receiver) = tokio::sync::watch::channel(None);
        let error = tokio::time::timeout(Duration::from_secs(5), monitor(idle_child(), sender))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn helper_receives_only_desktop_bus_environment() {
        let mut command = std::process::Command::new("maple-agent");
        command.env("MAPLE_THEME_TEST_SECRET", "must-not-reach-helper");
        configure_helper(&mut command, |key| Some(format!("fixture-{key}").into()));
        let environment: Vec<_> = command
            .get_envs()
            .map(|(key, value)| (key.to_str().unwrap(), value.unwrap().to_str().unwrap()))
            .collect();
        assert_eq!(
            environment,
            vec![
                (
                    "DBUS_SESSION_BUS_ADDRESS",
                    "fixture-DBUS_SESSION_BUS_ADDRESS"
                ),
                ("XDG_RUNTIME_DIR", "fixture-XDG_RUNTIME_DIR")
            ]
        );
        assert_eq!(command.get_current_dir(), Some(std::path::Path::new("/")));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["--internal-theme-watch"]
        );
    }

    #[test]
    fn helper_does_not_inherit_leaked_descriptors() {
        let file = std::fs::File::open("/dev/null").unwrap();
        // dup() leaves the copy without CLOEXEC, like a careless native library.
        let leaked = unsafe { libc::dup(std::os::fd::AsRawFd::as_raw_fd(&file)) };
        assert!(leaked > 2);
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg(format!(
            "if [ -e /proc/self/fd/{leaked} ]; then echo leaked; else echo closed; fi"
        ));
        configure_helper(&mut command, |_| None);
        let output = command.output().unwrap();
        unsafe { libc::close(leaked) };
        assert!(output.status.success());
        assert_eq!(output.stdout, b"closed\n");
    }
}
