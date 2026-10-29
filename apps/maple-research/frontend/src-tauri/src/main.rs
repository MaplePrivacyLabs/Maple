// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[derive(Debug, PartialEq, Eq)]
enum StartupMode {
    Acp,
    Version,
    Desktop,
}

fn startup_mode(args: impl IntoIterator<Item = String>) -> StartupMode {
    match args.into_iter().next().as_deref() {
        Some("acp") => StartupMode::Acp,
        Some("--version" | "-V") => StartupMode::Version,
        _ => StartupMode::Desktop,
    }
}

fn version_text() -> &'static str {
    concat!("maple ", env!("CARGO_PKG_VERSION"))
}

/// External agent clients may still be configured to launch `maple acp`.
/// Answer them on stderr instead of opening or focusing the desktop app.
const ACP_MOVED_MESSAGE: &str =
    "Maple agent connections have moved to Maple Work: https://maple.ai/work/download";

fn main() {
    match startup_mode(std::env::args().skip(1)) {
        StartupMode::Acp => {
            eprintln!("{ACP_MOVED_MESSAGE}");
            std::process::exit(1);
        }
        StartupMode::Version => println!("{}", version_text()),
        StartupMode::Desktop => app_lib::run(),
    }
}

#[cfg(test)]
mod tests {
    use super::{startup_mode, version_text, StartupMode};

    fn mode(args: &[&str]) -> StartupMode {
        startup_mode(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn version_flags_use_the_fast_path() {
        assert_eq!(mode(&["--version"]), StartupMode::Version);
        assert_eq!(mode(&["-V"]), StartupMode::Version);
        assert_eq!(
            version_text(),
            format!("maple {}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn acp_subcommand_keeps_precedence_over_following_flags() {
        assert_eq!(mode(&["acp"]), StartupMode::Acp);
        assert_eq!(mode(&["acp", "--version"]), StartupMode::Acp);
    }

    #[test]
    fn other_arguments_keep_desktop_startup() {
        assert_eq!(mode(&[]), StartupMode::Desktop);
        assert_eq!(mode(&["--unknown"]), StartupMode::Desktop);
    }
}
