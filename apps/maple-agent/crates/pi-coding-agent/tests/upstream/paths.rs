//! Translated from Pi v1.0.4 `packages/coding-agent/test/paths.test.ts`.
//! Home and working-directory inputs are fixture-owned instead of process globals.
use pi_coding_agent::config::HostConfig;
use pi_coding_agent::utils::paths::{
    PathInputOptions, canonicalize_path, get_cwd_relative_path, is_local_path, normalize_path,
    normalize_windows_shell_path, resolve_path,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

struct Fixture {
    temp: tempfile::TempDir,
    config: HostConfig,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("pi-paths-")
            .tempdir()
            .unwrap();
        let config = HostConfig::new(
            "pi",
            ".pi",
            temp.path().join("home"),
            temp.path().join("pi-paths-cwd"),
        );
        Self { temp, config }
    }
    fn root(&self) -> &Path {
        self.temp.path()
    }
    fn cwd(&self) -> &Path {
        &self.config.process_cwd
    }
    fn options(&self) -> PathInputOptions {
        PathInputOptions {
            home_dir: Some(text(&self.config.home_dir)),
            ..Default::default()
        }
    }
}
fn text(path: &Path) -> String {
    path.to_str()
        .expect("fixture path is valid Unicode")
        .to_owned()
}
fn file_url(path: &Path) -> String {
    url::Url::from_file_path(path)
        .expect("fixture path is absolute")
        .into()
}
#[cfg(unix)]
fn symlink_file(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}
#[cfg(windows)]
fn symlink_file(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_file(target, link).unwrap();
}
#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}
#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_dir(target, link).unwrap();
}

mod canonicalize_path_tests {
    use super::*;
    #[test]
    fn returns_the_real_path_for_a_regular_file() {
        let fixture = Fixture::new();
        let file = fixture.root().join("file.txt");
        fs::write(&file, "hello").unwrap();
        assert_eq!(
            canonicalize_path(&text(&file)),
            text(&fs::canonicalize(file).unwrap())
        );
    }
    #[test]
    fn resolves_symlinks_to_their_targets() {
        let fixture = Fixture::new();
        let target = fixture.root().join("target.txt");
        let link = fixture.root().join("link.txt");
        fs::write(&target, "hello").unwrap();
        symlink_file(&target, &link);
        assert_eq!(
            canonicalize_path(&text(&link)),
            text(&fs::canonicalize(target).unwrap())
        );
    }
    #[test]
    fn resolves_directory_symlinks() {
        let fixture = Fixture::new();
        let target = fixture.root().join("target-dir");
        let link = fixture.root().join("link-dir");
        fs::create_dir(&target).unwrap();
        symlink_dir(&target, &link);
        assert_eq!(
            canonicalize_path(&text(&link)),
            text(&fs::canonicalize(target).unwrap())
        );
    }
    #[test]
    fn falls_back_to_the_raw_path_when_the_target_does_not_exist() {
        let fixture = Fixture::new();
        let nonexistent = text(&fixture.root().join("no-such-file"));
        assert_eq!(canonicalize_path(&nonexistent), nonexistent);
    }
    #[test]
    fn falls_back_to_the_raw_path_for_a_dangling_symlink() {
        let fixture = Fixture::new();
        let target = fixture.root().join("target.txt");
        let link = fixture.root().join("link.txt");
        symlink_file(&target, &link);
        assert_eq!(canonicalize_path(&text(&link)), text(&link));
    }
}
mod get_cwd_relative_path_tests {
    use super::*;
    #[test]
    fn keeps_cwd_relative_names_that_start_with_dots() {
        let fixture = Fixture::new();
        let file = fixture.cwd().join("..config").join("AGENTS.md");
        assert_eq!(
            get_cwd_relative_path(&text(&file), &text(fixture.cwd())).unwrap(),
            Some(text(&PathBuf::from("..config").join("AGENTS.md")))
        );
    }
    #[test]
    fn rejects_parent_directory_traversals() {
        let fixture = Fixture::new();
        let file = fixture.cwd().join("..").join("AGENTS.md");
        assert_eq!(
            get_cwd_relative_path(&text(&file), &text(fixture.cwd())).unwrap(),
            None
        );
    }
}
mod resolve_path_tests {
    use super::*;
    #[test]
    fn expands_only_home_tilde_shortcuts() {
        let fixture = Fixture::new();
        let options = fixture.options();
        assert_eq!(
            normalize_path("~", &options).unwrap(),
            text(&fixture.config.home_dir)
        );
        assert_eq!(
            normalize_path("~/file.txt", &options).unwrap(),
            text(&fixture.config.home_dir.join("file.txt"))
        );
        assert_eq!(
            resolve_path("~draft.md", &text(fixture.cwd()), &options).unwrap(),
            text(&fixture.cwd().join("~draft.md"))
        );
        assert_eq!(normalize_path("~draft.md", &options).unwrap(), "~draft.md");
    }
    #[test]
    fn resolves_relative_paths_against_the_base_directory() {
        let fixture = Fixture::new();
        let expected = text(&fixture.cwd().join("subdir").join("file.txt"));
        assert_eq!(
            resolve_path("subdir/file.txt", &text(fixture.cwd()), &fixture.options()).unwrap(),
            expected
        );
        assert_eq!(
            resolve_path(
                "subdir/file.txt",
                &file_url(fixture.cwd()),
                &fixture.options()
            )
            .unwrap(),
            expected
        );
    }
    #[test]
    fn accepts_file_urls() {
        let fixture = Fixture::new();
        let file = fixture.root().join("file with spaces.txt");
        assert_eq!(
            resolve_path(
                &file_url(&file),
                &text(&fixture.root().join("base")),
                &fixture.options()
            )
            .unwrap(),
            text(&file)
        );
    }
    #[test]
    fn throws_for_invalid_file_urls() {
        let fixture = Fixture::new();
        assert!(
            resolve_path("file:///%E0%A4%A", &text(fixture.cwd()), &fixture.options()).is_err()
        );
    }
    #[test]
    fn preserves_posix_absolute_paths_with_literal_percent_sequences() {
        // Source platform applicability: paths.test.ts:109–112.
        // Declared in platform-coverage metadata; this return is not execution evidence.
        if cfg!(windows) {
            return;
        }
        let fixture = Fixture::new();
        for name in ["report%2026.md", "foo%2Fbar", "malformed%A.md"] {
            let file = fixture.root().join(name);
            assert_eq!(
                resolve_path(
                    &text(&file),
                    &text(&fixture.root().join("base")),
                    &fixture.options()
                )
                .unwrap(),
                text(&file)
            );
        }
    }
    #[test]
    fn does_not_treat_windows_file_url_pathname_strings_as_native_paths() {
        // Source platform applicability: paths.test.ts:120–123.
        // Declared in platform-coverage metadata; this return is not execution evidence.
        if !cfg!(windows) {
            return;
        }
        let fixture = Fixture::new();
        let file = fixture.root().join("dir").join("SKILL.md");
        let url = url::Url::from_file_path(&file).unwrap();
        let pathname = url.path();
        let bytes = pathname.as_bytes();
        assert!(
            bytes.len() >= 3
                && bytes[0] == b'/'
                && bytes[1].is_ascii_alphabetic()
                && bytes[2] == b':'
        );
        // Node resolve(pathname) uses the process's current drive, independently
        // of the explicit E: base passed to resolvePath. No process mutation.
        let expected = std::env::current_dir().unwrap().join(pathname);
        assert_eq!(
            resolve_path(pathname, r"E:\project", &fixture.options()).unwrap(),
            text(&expected)
        );
    }
}
mod normalize_windows_shell_path_tests {
    use super::*;
    #[test]
    fn converts_git_bash_msys_cygwin_and_wsl_drive_paths() {
        for (input, expected) in [
            ("/c/Users/example/project", r"C:\Users\example\project"),
            ("/cygdrive/d/work", r"D:\work"),
            ("/mnt/e/source", r"E:\source"),
            ("/c", "C:\\"),
        ] {
            assert_eq!(normalize_windows_shell_path(input), expected);
        }
    }
    #[test]
    fn leaves_other_path_forms_unchanged() {
        for path in [
            "C:/Users/example",
            r"C:\Users\example",
            "//server/share/file",
            r"/c/Users\example",
            "relative/file",
            "/tmp/file",
        ] {
            assert_eq!(normalize_windows_shell_path(path), path);
        }
    }
    #[test]
    fn is_applied_by_normal_path_handling_on_windows() {
        // Source platform applicability: paths.test.ts:154 it.runIf(win32).
        // Declared in platform-coverage metadata; this return is not execution evidence.
        if !cfg!(windows) {
            return;
        }
        let fixture = Fixture::new();
        assert_eq!(
            normalize_path("/c/Users/example", &fixture.options()).unwrap(),
            r"C:\Users\example"
        );
        assert_eq!(
            resolve_path("/mnt/c/Users/example", r"D:\work", &fixture.options()).unwrap(),
            r"C:\Users\example"
        );
    }
}
mod is_local_path_tests {
    use super::*;
    #[test]
    fn returns_true_for_bare_names() {
        assert!(is_local_path("my-package"));
    }
    #[test]
    fn returns_true_for_relative_paths() {
        assert!(is_local_path("./foo"));
    }
    #[test]
    fn returns_true_for_file_urls() {
        assert!(is_local_path("file:///tmp/foo"));
    }
    #[test]
    fn returns_false_for_npm_protocol() {
        assert!(!is_local_path("npm:package"));
    }
    #[test]
    fn returns_false_for_git_protocol() {
        assert!(!is_local_path("git://repo"));
    }
    #[test]
    fn returns_false_for_https_protocol() {
        assert!(!is_local_path("https://example.com"));
    }
}
