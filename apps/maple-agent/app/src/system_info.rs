//! A short description of this computer's operating system for the log and
//! support reports. It names the OS release and CPU architecture only: no
//! host name, user name or hardware serial.

/// For example `macOS 26.1 (25B78), aarch64`.
pub fn os_description() -> String {
    format!("{}, {}", os_release(), std::env::consts::ARCH)
}

#[cfg(target_os = "macos")]
fn os_release() -> String {
    match (
        sysctl_string(c"kern.osproductversion"),
        sysctl_string(c"kern.osversion"),
    ) {
        (Some(version), Some(build)) => format!("macOS {version} ({build})"),
        (Some(version), None) => format!("macOS {version}"),
        _ => "macOS".into(),
    }
}

#[cfg(target_os = "macos")]
fn sysctl_string(name: &std::ffi::CStr) -> Option<String> {
    let mut buffer = [0u8; 64];
    let mut length = buffer.len();
    // SAFETY: a fixed sysctl name and an initialized buffer whose size is
    // passed in `length`; the kernel writes at most that many bytes.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    let value = buffer.get(..length)?;
    let value = value.split(|byte| *byte == 0).next()?;
    let value = std::str::from_utf8(value).ok()?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(target_os = "linux")]
fn os_release() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| pretty_name(&text))
        .unwrap_or_else(|| "Linux".into())
}

#[cfg(any(target_os = "linux", test))]
fn pretty_name(os_release: &str) -> Option<String> {
    let value = os_release
        .lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))?
        .trim()
        .trim_matches('"')
        .trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn os_release() -> String {
    match std::env::consts::OS {
        "windows" => "Windows".into(),
        other => other.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_names_the_architecture() {
        let description = os_description();
        assert!(
            description.ends_with(std::env::consts::ARCH),
            "{description}"
        );
        assert!(!description.starts_with(','), "{description}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_description_has_a_version_and_build() {
        let description = os_description();
        assert!(description.starts_with("macOS "), "{description}");
        assert!(description.contains('('), "{description}");
    }

    #[test]
    fn pretty_name_reads_quoted_and_bare_values() {
        assert_eq!(
            pretty_name("NAME=Ubuntu\nPRETTY_NAME=\"Ubuntu 24.04.3 LTS\"\n").as_deref(),
            Some("Ubuntu 24.04.3 LTS")
        );
        assert_eq!(
            pretty_name("PRETTY_NAME=Arch Linux\n").as_deref(),
            Some("Arch Linux")
        );
        assert_eq!(pretty_name("PRETTY_NAME=\"\"\n"), None);
        assert_eq!(pretty_name("NAME=Fedora\n"), None);
    }
}
