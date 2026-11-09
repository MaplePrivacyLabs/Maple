//! Environment variable helpers shared by every startup mode. Values are
//! trimmed, and an empty value counts as unset so a stray `NAME=` in a
//! launcher does not override a default with nothing.

// Only desktop update checks and the localization preview gate use `env_flag`.
#![cfg_attr(not(feature = "desktop"), allow(dead_code))]

/// The trimmed value of `name`, or `None` when unset or blank.
pub fn env_string(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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
}
