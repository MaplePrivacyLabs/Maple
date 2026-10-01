//! Environment variable helpers shared by every startup mode. Values are
//! trimmed, and an empty value counts as unset so a stray `NAME=` in a
//! launcher does not override a default with nothing.

// A headless build (no `desktop` feature) leaves some helpers without a
// caller: `env_flag` belongs to the update check, `hostname` to the host
// and client roles the build may lack.
#![cfg_attr(not(feature = "desktop"), allow(dead_code))]

/// The package version of this binary.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The git revision `build.rs` baked in (`abc1234`, or `abc1234-dirty`),
/// or `None` when the build ran outside a git checkout. Sent to peers so
/// two builds of one version can be told apart.
pub fn build_hash() -> Option<&'static str> {
    option_env!("MAPLE_GIT_HASH").filter(|hash| !hash.is_empty() && *hash != "unknown")
}

/// The `--version` string: package version plus the git revision, so a
/// running binary can be matched back to a checkout. `unknown` stands in
/// for a missing revision.
pub fn version_string() -> String {
    maple_remote::wire::version_label(APP_VERSION, Some(build_hash().unwrap_or("unknown")))
}

/// The trimmed value of `name`, or `None` when unset or blank.
pub fn env_string(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// This machine's name, for hosts and devices to show each other.
/// `HOSTNAME` in the environment overrides what the system reports.
pub fn hostname() -> String {
    env_string("HOSTNAME")
        .or_else(system_hostname)
        .unwrap_or_else(|| "maple".to_string())
}

/// The name the operating system reports, or `None` when it has none.
#[cfg(unix)]
fn system_hostname() -> Option<String> {
    // `gethostname` truncates to the buffer without a terminator when the
    // name is longer; 256 exceeds every platform's HOST_NAME_MAX.
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is valid for writes of its full length, and the
    // call writes at most that many bytes.
    let status =
        unsafe { libc::gethostname(buffer.as_mut_ptr() as *mut libc::c_char, buffer.len()) };
    if status != 0 {
        return None;
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    let name = String::from_utf8_lossy(&buffer[..end]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

#[cfg(not(unix))]
fn system_hostname() -> Option<String> {
    env_string("COMPUTERNAME")
}

/// Whether `name` is set to `1`, `true`, or `yes` (case-insensitive).
pub fn env_flag(name: &str) -> bool {
    env_string(name).is_some_and(|value| {
        value.eq_ignore_ascii_case("1")
            || value.eq_ignore_ascii_case("true")
            || value.eq_ignore_ascii_case("yes")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env vars are process-global; tests share one lock so they do not
    /// race on the same names.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_var<T>(name: &str, value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the tests in this module are the only readers of these
        // names and they run under `LOCK`.
        unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        let out = f();
        unsafe { std::env::remove_var(name) };
        out
    }

    #[test]
    fn version_string_names_the_package_version_and_the_revision() {
        let version = version_string();
        match build_hash() {
            Some(hash) => {
                assert_ne!(hash, "unknown");
                assert_eq!(version, format!("{APP_VERSION} ({hash})"));
            }
            None => assert_eq!(version, format!("{APP_VERSION} (unknown)")),
        }
    }

    #[test]
    fn string_trims_and_drops_blank() {
        let read = || env_string("MAPLE_TEST_STRING");
        assert_eq!(
            with_var("MAPLE_TEST_STRING", Some("  x  "), read),
            Some("x".to_string())
        );
        assert_eq!(with_var("MAPLE_TEST_STRING", Some("   "), read), None);
        assert_eq!(with_var("MAPLE_TEST_STRING", None, read), None);
    }

    #[test]
    fn flag_accepts_truthy_words_only() {
        let read = || env_flag("MAPLE_TEST_FLAG");
        for value in ["1", "true", "YES", " yes "] {
            assert!(with_var("MAPLE_TEST_FLAG", Some(value), read), "{value:?}");
        }
        for value in ["0", "false", "", "on"] {
            assert!(!with_var("MAPLE_TEST_FLAG", Some(value), read), "{value:?}");
        }
        assert!(!with_var("MAPLE_TEST_FLAG", None, read));
    }

    #[test]
    fn hostname_prefers_the_override_and_never_comes_back_empty() {
        assert_eq!(with_var("HOSTNAME", Some(" box "), hostname), "box");
        let system = with_var("HOSTNAME", None, hostname);
        assert!(!system.is_empty());
        assert_eq!(system, system.trim());
        assert!(!system.contains('\0'));
    }
}
