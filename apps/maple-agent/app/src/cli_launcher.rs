//! The `maple-agent` command for terminals and ACP clients.
//!
//! An installed app can write a small launcher script that execs the app
//! where it is installed: `/usr/local/bin` on macOS, through an administrator
//! prompt when the folder is not writable, and `~/.local/bin` on Linux. A
//! script rather than a copy keeps the command current across updates, which
//! replace the app in place; rather than a symlink, it keeps `current_exe()`
//! inside the bundle, which macOS would otherwise report as the symlink's
//! path. Maple replaces or removes only a file that carries its own marker.
//! The Linux installer already writes its own wrapper at the same path, which
//! this treats as installed.

use std::path::{Path, PathBuf};

#[cfg(unix)]
const MARKER: &str = "# maple-agent-cli-launcher-v1";
/// The marker of the wrapper `scripts/linux-install.py` writes.
#[cfg(unix)]
const LINUX_INSTALLER_MARKER: &str = "# maple-agent-user-install-v1";
/// Larger files are not a launcher Maple wrote.
#[cfg(unix)]
const MAX_LAUNCHER_BYTES: u64 = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LauncherState {
    /// This build cannot install the command; the text says why.
    Unavailable(&'static str),
    Missing,
    /// Maple's launcher runs this app.
    Installed,
    /// The Linux installer's wrapper provides the command.
    InstalledByInstaller,
    /// Maple's launcher runs a different copy of the app.
    Stale,
    /// A file Maple did not write already uses the name.
    Foreign,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LauncherStatus {
    pub state: LauncherState,
    pub command: &'static str,
    /// The launcher's path, with the home folder shown as `~`.
    pub display_path: String,
    /// The launcher's folder, with the home folder shown as `~`.
    pub display_dir: String,
    /// Whether the launcher's folder is on the login shell's PATH; `None`
    /// when that is unknown or does not matter.
    pub on_path: Option<bool>,
}

impl LauncherStatus {
    fn unavailable(reason: &'static str) -> Self {
        Self {
            state: LauncherState::Unavailable(reason),
            command: command_name(),
            display_path: String::new(),
            display_dir: String::new(),
            on_path: None,
        }
    }
}

/// Where the launcher goes and what it runs.
#[derive(Clone, Debug)]
struct Plan {
    home: PathBuf,
    launcher: PathBuf,
    // Windows has no launcher to point at it yet.
    #[cfg_attr(not(unix), allow(dead_code))]
    target: PathBuf,
    command: &'static str,
}

impl Plan {
    fn status(&self, state: LauncherState) -> LauncherStatus {
        let dir = self.launcher.parent().unwrap_or(&self.home);
        LauncherStatus {
            state,
            command: self.command,
            display_path: display(&self.home, &self.launcher),
            display_dir: display(&self.home, dir),
            on_path: None,
        }
    }
}

/// The command name matches the Linux installer's: Dev installs side by side.
fn command_name() -> &'static str {
    if crate::profile::PROFILE == "dev" {
        "maple-agent-dev"
    } else {
        "maple-agent"
    }
}

/// Local builds can name the executable to launch, so the row can be
/// exercised without a package. Installed builds ignore it.
const TARGET_OVERRIDE: &str = "MAPLE_CLI_LAUNCHER_TARGET";

fn plan() -> Result<Plan, &'static str> {
    let target = if crate::profile::PACKAGED {
        installed_target()?
    } else {
        crate::env::env_string(TARGET_OVERRIDE)
            .map(PathBuf::from)
            .ok_or(
                "Available in installed builds of Maple Work. \
                 Development builds run through their own launcher.",
            )?
    };
    let home = dirs::home_dir().ok_or("Could not find your home folder.")?;
    let command = command_name();
    let launcher = launcher_dir(&home).join(command);
    Ok(Plan {
        home,
        launcher,
        target,
        command,
    })
}

/// `/usr/local/bin` is on macOS's default PATH; `~/.local/bin` is where the
/// Linux installer puts the command.
fn launcher_dir(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/usr/local/bin")
    } else {
        home.join(".local").join("bin")
    }
}

#[cfg(target_os = "macos")]
fn installed_target() -> Result<PathBuf, &'static str> {
    let executable = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|_| "Could not locate Maple Work.")?;
    macos_target(executable)
}

/// Accept only an executable inside an app bundle that stays put: not on
/// the mounted disk image and not a translocated copy, both of which vanish.
#[cfg(any(target_os = "macos", all(test, unix)))]
fn macos_target(executable: PathBuf) -> Result<PathBuf, &'static str> {
    let text = executable.to_string_lossy();
    if !text.contains(".app/Contents/MacOS/")
        || text.starts_with("/Volumes/")
        || text.contains("/AppTranslocation/")
    {
        return Err("Move Maple Work to Applications and open it from there \
                    to install the command.");
    }
    Ok(executable)
}

#[cfg(target_os = "linux")]
fn installed_target() -> Result<PathBuf, &'static str> {
    // Inside Flatpak the bundle path does not exist on the host.
    if Path::new("/.flatpak-info").exists() {
        return Err("Not available in the Flatpak build.");
    }
    crate::self_launch::executable().map_err(|_| "Could not locate Maple Work.")
}

#[cfg(windows)]
fn installed_target() -> Result<PathBuf, &'static str> {
    Err(
        "Not available on Windows yet. Run maple-agent.exe from the Maple \
         Work install folder.",
    )
}

/// Check the launcher without changing anything. When it is installed, ask
/// the login shell whether its folder is on PATH.
pub async fn load_status() -> LauncherStatus {
    let plan = match plan() {
        Ok(plan) => plan,
        Err(reason) => return LauncherStatus::unavailable(reason),
    };
    let checked = plan.clone();
    let mut status =
        match tokio::task::spawn_blocking(move || checked.status(classify(&checked))).await {
            Ok(status) => status,
            Err(_) => return LauncherStatus::unavailable("Could not check the command."),
        };
    if matches!(
        status.state,
        LauncherState::Installed | LauncherState::InstalledByInstaller
    ) {
        let search_path = maple_agent::agent::login_search_path()
            .await
            .or_else(|| std::env::var("PATH").ok());
        status.on_path = search_path.map(|search_path| {
            plan.launcher
                .parent()
                .is_some_and(|dir| path_contains(&search_path, dir))
        });
    }
    status
}

/// Write the launcher, or point Maple's existing launcher at this app.
pub async fn install() -> Result<(), String> {
    let plan = plan().map_err(str::to_string)?;
    tokio::task::spawn_blocking(move || install_plan(&plan))
        .await
        .map_err(|_| "The install was cancelled".to_string())?
}

/// Delete Maple's launcher. Any other file is left alone.
pub async fn remove() -> Result<(), String> {
    let plan = plan().map_err(str::to_string)?;
    tokio::task::spawn_blocking(move || remove_plan(&plan))
        .await
        .map_err(|_| "The removal was cancelled".to_string())?
}

fn display(home: &Path, path: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn path_contains(search_path: &str, dir: &Path) -> bool {
    let canonical = dir.canonicalize().ok();
    std::env::split_paths(search_path).any(|entry| {
        entry == dir || (canonical.is_some() && entry.canonicalize().ok() == canonical)
    })
}

/// Single quotes keep every byte literal; a quote closes, escapes and reopens.
#[cfg(unix)]
fn shell_quote(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or("Maple Work's install path is not valid UTF-8")?;
    Ok(quote_text(text))
}

#[cfg(unix)]
fn quote_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

#[cfg(unix)]
fn launcher_script(target: &Path) -> Result<String, String> {
    Ok(format!(
        "#!/bin/sh\n{MARKER}\nexec {} \"$@\"\n",
        shell_quote(target)?
    ))
}

/// The launcher folder exists but this user cannot write it, or it does not
/// exist yet. Only macOS's system folder takes an administrator prompt.
#[cfg(target_os = "macos")]
fn needs_admin(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return true;
    };
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    unsafe { libc::access(path.as_ptr(), libc::W_OK) != 0 }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn needs_admin(_dir: &Path) -> bool {
    false
}

/// One `sh` command line that writes the launcher through a staged file and
/// a rename. Every line is a separate quoted `printf` argument, so the
/// command has no newline for AppleScript to carry. `create_only` refuses a
/// file that appeared after the unprivileged check.
#[cfg(unix)]
fn privileged_write_script(
    launcher: &Path,
    script: &str,
    create_only: bool,
) -> Result<String, String> {
    let dir = launcher.parent().ok_or("The command folder is invalid")?;
    let name = launcher
        .file_name()
        .ok_or("The command path is invalid")?
        .to_string_lossy();
    let staged = shell_quote(&dir.join(format!(".{name}.maple-tmp")))?;
    let target = shell_quote(launcher)?;
    let lines: Vec<String> = script.lines().map(quote_text).collect();
    let guard = if create_only {
        format!("[ ! -e {target} ] && [ ! -L {target} ] && ")
    } else {
        String::new()
    };
    Ok(format!(
        "umask 022 && /bin/mkdir -p {dir} && {guard}/bin/rm -f {staged} && \
         /usr/bin/printf '%s\\n' {lines} > {staged} && /bin/chmod 0755 {staged} && \
         /bin/mv -f {staged} {target}",
        dir = shell_quote(dir)?,
        lines = lines.join(" "),
    ))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn run_as_admin(_command: &str) -> Result<(), String> {
    Err("Administrator rights are not available here".to_string())
}

/// Run `command` as root after macOS's own administrator prompt.
#[cfg(target_os = "macos")]
fn run_as_admin(command: &str) -> Result<(), String> {
    let output = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(format!(
            "do shell script \"{}\" with administrator privileges",
            applescript_escape(command)
        ))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|_| "Could not ask for administrator rights".to_string())?;
    if output.status.success() {
        return Ok(());
    }
    // -128 is AppleScript's "User canceled."
    if String::from_utf8_lossy(&output.stderr).contains("(-128)") {
        Err("Cancelled; nothing was changed".to_string())
    } else {
        Err("Could not change /usr/local/bin with administrator rights".to_string())
    }
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn applescript_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(unix)]
fn classify(plan: &Plan) -> LauncherState {
    match launcher_script(&plan.target) {
        Ok(expected) => match classify_file(&plan.launcher, &expected) {
            Ok(state) => state,
            Err(_) => LauncherState::Unavailable("Could not check the command."),
        },
        Err(_) => LauncherState::Unavailable("Maple Work's install path is not valid UTF-8."),
    }
}

#[cfg(not(unix))]
fn classify(_plan: &Plan) -> LauncherState {
    LauncherState::Unavailable("Not available on this platform.")
}

#[cfg(unix)]
fn classify_file(launcher: &Path, expected: &str) -> std::io::Result<LauncherState> {
    let metadata = match std::fs::symlink_metadata(launcher) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LauncherState::Missing);
        }
        Err(error) => return Err(error),
    };
    // A symlink or folder is someone else's, whatever it points at.
    if !metadata.file_type().is_file() || metadata.len() > MAX_LAUNCHER_BYTES {
        return Ok(LauncherState::Foreign);
    }
    let Ok(contents) = String::from_utf8(std::fs::read(launcher)?) else {
        return Ok(LauncherState::Foreign);
    };
    if contents == expected {
        return Ok(LauncherState::Installed);
    }
    let mut lines = contents.lines();
    let (first, second) = (lines.next(), lines.next());
    Ok(match (first, second) {
        (Some("#!/bin/sh"), Some(MARKER)) => LauncherState::Stale,
        (Some("#!/bin/sh"), Some(LINUX_INSTALLER_MARKER)) => LauncherState::InstalledByInstaller,
        _ => LauncherState::Foreign,
    })
}

#[cfg(unix)]
fn install_plan(plan: &Plan) -> Result<(), String> {
    let script = launcher_script(&plan.target)?;
    let dir = plan
        .launcher
        .parent()
        .ok_or("The command folder is invalid")?;
    let shown = display(&plan.home, &plan.launcher);
    let failed = |error: std::io::Error| format!("Could not write {shown}: {error}");
    let state = classify_file(&plan.launcher, &script).map_err(failed)?;
    if matches!(state, LauncherState::Missing | LauncherState::Stale) && needs_admin(dir) {
        let create_only = state == LauncherState::Missing;
        return run_as_admin(&privileged_write_script(
            &plan.launcher,
            &script,
            create_only,
        )?);
    }
    match state {
        LauncherState::Installed | LauncherState::InstalledByInstaller => Ok(()),
        LauncherState::Missing => std::fs::create_dir_all(dir)
            .and_then(|()| write_new(&plan.launcher, &script))
            .map_err(failed),
        LauncherState::Stale => replace_own(&plan.launcher, &script).map_err(failed),
        LauncherState::Foreign | LauncherState::Unavailable(_) => Err(format!(
            "{shown} was not created by Maple Work; nothing was changed"
        )),
    }
}

#[cfg(not(unix))]
fn install_plan(_plan: &Plan) -> Result<(), String> {
    Err("Not available on this platform".to_string())
}

#[cfg(unix)]
fn remove_plan(plan: &Plan) -> Result<(), String> {
    let script = launcher_script(&plan.target)?;
    let shown = display(&plan.home, &plan.launcher);
    let failed = |error: std::io::Error| format!("Could not remove {shown}: {error}");
    match classify_file(&plan.launcher, &script).map_err(failed)? {
        LauncherState::Missing => Ok(()),
        LauncherState::Installed | LauncherState::Stale => {
            match plan.launcher.parent().filter(|dir| needs_admin(dir)) {
                Some(_) => run_as_admin(&format!("/bin/rm -f {}", shell_quote(&plan.launcher)?)),
                None => std::fs::remove_file(&plan.launcher).map_err(failed),
            }
        }
        _ => Err(format!(
            "{shown} was not created by Maple Work; nothing was changed"
        )),
    }
}

#[cfg(not(unix))]
fn remove_plan(_plan: &Plan) -> Result<(), String> {
    Err("Not available on this platform".to_string())
}

/// Create the launcher, failing if anything appeared at the path meanwhile.
#[cfg(unix)]
fn write_new(path: &Path, script: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(path)?;
    let written = file
        .write_all(script.as_bytes())
        .and_then(|()| file.set_permissions(std::fs::Permissions::from_mode(0o755)))
        .and_then(|()| file.sync_all());
    if written.is_err() {
        let _ = std::fs::remove_file(path);
    }
    written
}

/// Replace Maple's own launcher atomically so the command never disappears.
#[cfg(unix)]
fn replace_own(path: &Path, script: &str) -> std::io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("launcher has no file name"))?;
    let staged = path.with_file_name(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    write_new(&staged, script)?;
    let renamed = std::fs::rename(&staged, path);
    if renamed.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    renamed
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn plan_in(home: &Path, target: &str) -> Plan {
        Plan {
            home: home.to_path_buf(),
            launcher: home.join(".local/bin/maple-agent"),
            target: PathBuf::from(target),
            command: "maple-agent",
        }
    }

    const TARGET: &str = "/Applications/Maple Work.app/Contents/MacOS/maple-agent";

    #[test]
    fn install_writes_an_executable_marked_launcher() {
        let home = tempfile::tempdir().unwrap();
        let plan = plan_in(home.path(), TARGET);
        assert_eq!(classify(&plan), LauncherState::Missing);

        install_plan(&plan).unwrap();

        let script = std::fs::read_to_string(&plan.launcher).unwrap();
        assert_eq!(
            script,
            "#!/bin/sh\n# maple-agent-cli-launcher-v1\n\
             exec '/Applications/Maple Work.app/Contents/MacOS/maple-agent' \"$@\"\n"
        );
        let mode = std::fs::metadata(&plan.launcher)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(classify(&plan), LauncherState::Installed);
        // Installing again is a no-op.
        install_plan(&plan).unwrap();
    }

    #[test]
    fn the_launcher_passes_arguments_and_stdio_through() {
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("it's the app");
        std::fs::write(&target, "#!/bin/sh\nprintf '%s|' \"$@\"\ncat\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let plan = plan_in(home.path(), target.to_str().unwrap());
        install_plan(&plan).unwrap();

        // Another test thread forking while these files were open for
        // writing can briefly make them busy to exec.
        let mut attempts = 0;
        let mut child = loop {
            match std::process::Command::new(&plan.launcher)
                .args(["acp", "two words"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
            {
                Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && attempts < 50 => {
                    attempts += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                spawned => break spawned.unwrap(),
            }
        };
        use std::io::Write as _;
        child.stdin.take().unwrap().write_all(b"frame").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"acp|two words|frame");
    }

    #[test]
    fn a_launcher_for_another_copy_is_updated_in_place() {
        let home = tempfile::tempdir().unwrap();
        install_plan(&plan_in(home.path(), "/Volumes/Old/Maple Work.app/x")).unwrap();
        let plan = plan_in(home.path(), TARGET);
        assert_eq!(classify(&plan), LauncherState::Stale);

        install_plan(&plan).unwrap();

        assert_eq!(classify(&plan), LauncherState::Installed);
        let leftovers: Vec<_> = std::fs::read_dir(plan.launcher.parent().unwrap())
            .unwrap()
            .collect();
        assert_eq!(leftovers.len(), 1, "no staged file is left behind");
    }

    #[test]
    fn files_maple_did_not_write_are_never_replaced_or_removed() {
        let home = tempfile::tempdir().unwrap();
        let plan = plan_in(home.path(), TARGET);
        std::fs::create_dir_all(plan.launcher.parent().unwrap()).unwrap();
        std::fs::write(&plan.launcher, "#!/bin/sh\nexec something-else\n").unwrap();

        assert_eq!(classify(&plan), LauncherState::Foreign);
        assert!(install_plan(&plan).is_err());
        assert!(remove_plan(&plan).is_err());
        assert_eq!(
            std::fs::read_to_string(&plan.launcher).unwrap(),
            "#!/bin/sh\nexec something-else\n"
        );
    }

    #[test]
    fn a_symlink_is_foreign_even_when_it_points_at_a_launcher() {
        let home = tempfile::tempdir().unwrap();
        let plan = plan_in(home.path(), TARGET);
        let real = home.path().join("real");
        std::fs::write(&real, launcher_script(&plan.target).unwrap()).unwrap();
        std::fs::create_dir_all(plan.launcher.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &plan.launcher).unwrap();

        assert_eq!(classify(&plan), LauncherState::Foreign);
        assert!(install_plan(&plan).is_err());
    }

    #[test]
    fn the_linux_installer_wrapper_counts_as_installed_and_is_kept() {
        let home = tempfile::tempdir().unwrap();
        let plan = plan_in(home.path(), TARGET);
        std::fs::create_dir_all(plan.launcher.parent().unwrap()).unwrap();
        let wrapper = "#!/bin/sh\n# maple-agent-user-install-v1\nexec '/x/AppRun' \"$@\"\n";
        std::fs::write(&plan.launcher, wrapper).unwrap();

        assert_eq!(classify(&plan), LauncherState::InstalledByInstaller);
        install_plan(&plan).unwrap();
        assert!(remove_plan(&plan).is_err());
        assert_eq!(std::fs::read_to_string(&plan.launcher).unwrap(), wrapper);
    }

    #[test]
    fn remove_deletes_only_maples_launcher() {
        let home = tempfile::tempdir().unwrap();
        let plan = plan_in(home.path(), TARGET);
        remove_plan(&plan).unwrap();
        install_plan(&plan).unwrap();

        remove_plan(&plan).unwrap();

        assert_eq!(classify(&plan), LauncherState::Missing);
    }

    #[test]
    fn macos_accepts_only_a_bundle_that_stays_put() {
        assert!(macos_target(PathBuf::from(TARGET)).is_ok());
        assert!(
            macos_target(PathBuf::from(
                "/Users/a/Applications/Maple Work.app/Contents/MacOS/maple-agent"
            ))
            .is_ok()
        );
        for rejected in [
            "/Volumes/Maple Work/Maple Work.app/Contents/MacOS/maple-agent",
            "/private/var/folders/x/T/AppTranslocation/1/d/Maple Work.app/Contents/MacOS/maple-agent",
            "/Users/a/src/target/debug/maple-agent",
        ] {
            assert!(macos_target(PathBuf::from(rejected)).is_err(), "{rejected}");
        }
    }

    fn run_sh(command: &str) -> bool {
        std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn the_privileged_command_writes_the_same_launcher() {
        let root = tempfile::tempdir().unwrap();
        let launcher = root.path().join("usr local/bin/maple-agent");
        let target = Path::new("/Applications/Maple's Work.app/Contents/MacOS/maple-agent");
        let script = launcher_script(target).unwrap();

        let create = privileged_write_script(&launcher, &script, true).unwrap();
        assert!(
            !create.contains('\n'),
            "AppleScript gets one line: {create}"
        );
        assert!(run_sh(&create));
        assert_eq!(std::fs::read_to_string(&launcher).unwrap(), script);
        let mode = std::fs::metadata(&launcher).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);

        // A file that appeared after the check is not replaced by a create.
        std::fs::write(&launcher, "someone else's").unwrap();
        assert!(!run_sh(&create));
        assert_eq!(
            std::fs::read_to_string(&launcher).unwrap(),
            "someone else's"
        );

        // An update replaces Maple's own launcher and leaves nothing staged.
        let update = privileged_write_script(&launcher, &script, false).unwrap();
        assert!(run_sh(&update));
        assert_eq!(std::fs::read_to_string(&launcher).unwrap(), script);
        assert_eq!(
            std::fs::read_dir(launcher.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn applescript_strings_keep_quotes_and_backslashes() {
        assert_eq!(
            applescript_escape(r#"printf '%s\n' 'a "b"' '\''"#),
            r#"printf '%s\\n' 'a \"b\"' '\\''"#
        );
    }

    #[test]
    fn path_lookup_matches_the_launcher_folder() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let joined = std::env::join_paths([Path::new("/usr/bin"), &bin])
            .unwrap()
            .into_string()
            .unwrap();
        assert!(path_contains(&joined, &bin));
        assert!(!path_contains("/usr/bin:/bin", &bin));
        assert_eq!(display(home.path(), &bin), "~/.local/bin");
    }
}
