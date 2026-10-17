# Temporary updater ZIP dependency patch

This is the published `tauri-plugin-updater` 2.10.1 crate, downloaded from
[crates.io](https://static.crates.io/crates/tauri-plugin-updater/tauri-plugin-updater-2.10.1.crate).
The original archive SHA-256 is
`806d9dac662c2e4594ff03c647a552f2c9bd544e7d0f683ec58f872f952ce4af`,
verified against the crates.io registry checksum. Its recorded source commit is
`d6a3898001a4bcc659e045f9501498751b77dbe6` in
`tauri-apps/plugins-workspace`, under `plugins/updater`.

The only upstream file changes are the Windows `zip` requirement from `4` to
`9` in `Cargo.toml` and `Cargo.toml.orig`. This backports
[Tauri PR #3698](https://github.com/tauri-apps/plugins-workspace/pull/3698)
(commit `ba98bfadc4c868c9a7e1a226c83af528e1f11b89`) to clear
RUSTSEC-2026-0367, RUSTSEC-2026-0368 and RUSTSEC-2026-0369. The application
lockfile selects the exact patched ZIP release. The packaged `Cargo.lock` is
omitted because Maple's parent lockfile is authoritative.

Plugin Rust source, permissions, default features, signatures and update-format
handling are unchanged. The `zip` feature remains enabled, preserving legacy
Windows ZIP bundles and compressed Linux AppImage updates. On Windows,
`Error::Extract` now contains ZIP 9's `ZipError`, as in the upstream fix.

Remove this directory and its `[patch.crates-io]` entry after adopting and
validating a published updater version that requires ZIP 9 or newer. Upstream
2.14.1 is pending in
[Tauri PR #3712](https://github.com/tauri-apps/plugins-workspace/pull/3712)
as of 2026-10-17; upgrading must also account for its newer Tauri requirement.
