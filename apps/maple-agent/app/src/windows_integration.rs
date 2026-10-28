//! Best-effort desktop cosmetics; never part of update or app admission.

pub(super) fn start() {
    if !crate::profile::PACKAGED {
        return;
    }
    let integration = crate::backend::profile_data_root().join("windows-integration");
    let icon: &'static [u8] = if crate::profile::PROFILE == "dev" {
        include_bytes!("../packaging/maple-agent-dev.ico")
    } else {
        include_bytes!("../packaging/maple-agent.ico")
    };
    // COM and shell I/O run off the UI thread. There is no revision marker or
    // availability gate: each normal GUI startup can repair existing cosmetics.
    let _ = std::thread::Builder::new()
        .name("maple-shell-icons".into())
        .spawn(move || {
            let result = std::env::current_exe()
                .map_err(maple_agent_updater::installer::InstallError::from)
                .and_then(|executable| {
                    maple_agent_updater::installer::refresh_windows_shell_integration(
                        &executable,
                        crate::profile::PROFILE,
                        crate::profile::DISPLAY_NAME,
                        &integration,
                        icon,
                    )
                });
            if result.is_err() {
                // Paths, registry values and native error text are not logged.
                log::warn!("Windows shell artwork could not be refreshed; Maple remains available");
            }
        });
}
