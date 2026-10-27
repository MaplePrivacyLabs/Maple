The Windows v3 fixture uses the public deterministic test-only key `[7; 32]`
and the ASCII payload `signed payload fixture bytes`, like the v1/v2 fixtures.
Neither the updater nor installer fixture is executable or proves Authenticode
trust. Never pin this public key in an application profile.

The Windows-only manifest uses `format: maple-agent-update-v3`, exactly one
`artifacts.windows-x86_64` `.tar.gz` bundle and one `downloads.windows` `.exe`
installer. These artifacts have separate independently validated paths, sizes
and SHA-256 hashes. All common manifest fields, bounds and signature comments
retain the existing contract.

The separate discovery path is `/agent/stable/windows/latest.json`; immutable
signed metadata lives at `/agent/stable/releases/0.0.2/windows-manifest.json`.
Windows inventory is forbidden in macOS v1 and Linux v2 metadata, including
explicit-null download fields. Windows v3 similarly rejects all legacy fields.
