//! Process-wide environment the desktop runtime must see before it starts.

/// Prepare environment variables that Maple's desktop dependencies read.
///
/// # Safety
///
/// This mutates the process environment, which is only sound while the process
/// is single-threaded. Call it as the first statement of `main`, before any
/// thread, async runtime, or library that reads the environment starts.
pub unsafe fn prepare_process_environment() {
    #[cfg(target_os = "linux")]
    unsafe {
        enable_wayland_computer_use()
    };
}

/// Opt into the Cua Driver SDK's native-Wayland backend on a Wayland session.
///
/// The SDK keeps that backend behind an environment variable and otherwise
/// routes window enumeration, screen capture, and input through X11. GNOME and
/// KDE still export `DISPLAY` for Xwayland on a native Wayland session, so the
/// X11 path looks viable, finds no native toplevels, and fails capture inside
/// `XGetImage` rather than reporting that it chose the wrong backend.
///
/// An explicit value from the user wins, including an explicit opt-out, so a
/// developer can still force the X11 path.
///
/// # Safety
///
/// See [`prepare_process_environment`]: the process must still be
/// single-threaded.
#[cfg(target_os = "linux")]
unsafe fn enable_wayland_computer_use() {
    const ENABLE_WAYLAND: &str = "CUA_DRIVER_RS_ENABLE_WAYLAND";

    if std::env::var_os(ENABLE_WAYLAND).is_some() {
        return;
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return;
    }
    // SAFETY: the caller guarantees the process is still single-threaded.
    unsafe { std::env::set_var(ENABLE_WAYLAND, "1") };
}
