//! Apply process-local crash-dump policy before any plaintext or key arrives.

fn debug_attach_allowed(debug_build: bool, requested: bool) -> bool {
    debug_build && requested
}

pub(crate) fn apply() -> Result<(), String> {
    if debug_attach_allowed(
        cfg!(debug_assertions),
        std::env::var_os("MAPLE_ALLOW_DEBUG_ATTACH").as_deref() == Some(std::ffi::OsStr::new("1")),
    ) {
        return Ok(());
    }
    #[cfg(unix)]
    disable_core_dumps()?;
    #[cfg(target_os = "linux")]
    {
        // SAFETY: PR_SET_DUMPABLE takes the integer zero, no pointer arguments.
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
            return Err("Could not protect Maple from debugger attachment.".into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::System::ErrorReporting::{
            WER_FAULT_REPORTING_DISABLE_SNAPSHOT_CRASH, WER_FAULT_REPORTING_DISABLE_SNAPSHOT_HANG,
            WER_FAULT_REPORTING_FLAG_NOHEAP, WerAddExcludedApplication, WerSetFlags,
        };
        let executable = std::env::current_exe()
            .map_err(|_| "Could not identify Maple for crash-dump exclusion.")?;
        let name: Vec<u16> = executable
            .file_name()
            .ok_or("Could not identify Maple for crash-dump exclusion.")?
            .encode_wide()
            .chain(Some(0))
            .collect();
        // SAFETY: the executable basename is a terminated UTF-16 buffer. FALSE
        // applies the documented exclusion only for the current user's account.
        if unsafe { WerAddExcludedApplication(name.as_ptr(), 0) } < 0 {
            return Err("Could not exclude Maple from Windows Error Reporting.".into());
        }
        // SAFETY: documented flags; no borrowed pointers.
        if unsafe {
            WerSetFlags(
                WER_FAULT_REPORTING_FLAG_NOHEAP
                    | WER_FAULT_REPORTING_DISABLE_SNAPSHOT_CRASH
                    | WER_FAULT_REPORTING_DISABLE_SNAPSHOT_HANG,
            )
        } < 0
        {
            return Err("Could not disable Maple crash snapshots.".into());
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn disable_core_dumps() -> Result<(), String> {
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: a valid, initialized rlimit pointer; process-local policy.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } != 0 {
        return Err("Could not disable Maple crash dumps.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_attach_opt_in_is_ignored_in_release_builds() {
        assert!(!debug_attach_allowed(false, true));
        assert!(!debug_attach_allowed(false, false));
        assert!(!debug_attach_allowed(true, false));
        assert!(debug_attach_allowed(true, true));
    }

    #[test]
    fn hardening_is_applied_in_an_isolated_child() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host::hardening::tests::hardening_child",
                "--ignored",
                "--nocapture",
            ])
            .env_remove("MAPLE_ALLOW_DEBUG_ATTACH")
            .status()
            .unwrap();
        assert!(result.success());
    }

    #[test]
    #[ignore = "invoked only in the isolated hardening test child"]
    fn hardening_child() {
        apply().unwrap();
        #[cfg(unix)]
        {
            let mut limit = libc::rlimit {
                rlim_cur: 1,
                rlim_max: 1,
            };
            // SAFETY: initialized writable rlimit in an isolated child.
            assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut limit) }, 0);
            assert_eq!(limit.rlim_cur, 0);
            assert_eq!(limit.rlim_max, 0);
        }
        #[cfg(target_os = "linux")]
        // SAFETY: PR_GET_DUMPABLE has no pointer arguments.
        assert_eq!(unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) }, 0);
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::{
                ErrorReporting::{WER_FAULT_REPORTING_FLAG_NOHEAP, WerGetFlags},
                Threading::GetCurrentProcess,
            };
            let mut flags = 0;
            // SAFETY: current process pseudo-handle and valid output pointer.
            assert_eq!(unsafe { WerGetFlags(GetCurrentProcess(), &mut flags) }, 0);
            assert_ne!(flags & WER_FAULT_REPORTING_FLAG_NOHEAP, 0);
        }
    }
}
