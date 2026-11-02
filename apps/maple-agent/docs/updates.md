# Agent updates

Packaged Prod builds check a signed feed per platform; Dev and unpackaged builds
have no updater. `MAPLE_DISABLE_UPDATE_CHECK=1` disables checks.

| Platform | Feed | Manifest format |
| --- | --- | --- |
| macOS arm64 | `https://updates.maple.ai/agent/stable/latest.json` | `maple-agent-update-v1` |
| Linux x86_64 | `https://updates.maple.ai/agent/stable/linux/latest.json` | `maple-agent-update-v2` |
| Windows x64 | `https://updates.maple.ai/agent/stable/windows/latest.json` | `maple-agent-update-v3` |
| Windows ARM64 | `https://updates.maple.ai/agent/stable/windows-arm64/latest.json` | `maple-agent-update-v4` |

Older packages poll the same paths on `updates.trymaple.ai`, so both hosts must
keep routing `/agent/stable/*` and `/download/agent/stable/*` to the Agent Worker
([service](../../../services/agent-updates/README.md)).

## Releasing

1. Merge a PR that bumps `[workspace.package] version` in `apps/maple-agent/Cargo.toml`
   (and `Cargo.lock`).
2. Run the release:

   ```sh
   gh workflow run agent-desktop-build.yml --repo MaplePrivacyLabs/Maple --ref master \
     -f release=true -f notes="What changed"
   ```

That run builds and signs Prod for every platform, then its `publish` job
signs the four manifests and uploads the packages, the per-version manifests and
the four feed pointers to R2 (`services/agent-updates/publish.py`). The
`updates-production` environment holds the bucket and signing credentials. The
publish job refuses a version that is already live: release paths are cached as
immutable, so bump the version and release again instead.

To fix a bad release, publish a higher version. Clients never install an older
version, so moving a pointer back does not roll anyone back.

## User behavior

The desktop checks at startup and about hourly. Automatic download is on by
default and can be turned off in Settings. A downloaded update waits for
**Restart to Update**, which asks for confirmation because it stops active work.
A pending update marks the sidebar's Settings button with a badge (a dot when
the sidebar is collapsed) and adds a row to its menu. About, the application
menu and the tray also offer update actions. Paired computers can show and
request updates for each other through the
[experimental remote update controls](remote-access.md#experimental-remote-updates).

## Wire contract

Installed clients only accept what they were built to parse, so the publisher
must keep producing exactly this. The client source of truth is
`crates/maple-agent-updater/src/manifest.rs`.

- **Envelope:** `{"manifest": "<signed JSON text>", "signature": "<minisign>"}`,
  no other fields. The signature is prehashed minisign, exactly four lines, made
  by one of the two keys pinned in `release-profiles.json`, with trusted comment
  `product=maple-agent channel=stable version=<version>`.
- **Manifest:** exactly `format`, `product` (`maple-agent`), `channel`
  (`stable`), `version` (stable SemVer), `build` (the producing run's number,
  as a string), `source_sha`, `pub_date` (`YYYY-MM-DDTHH:MM:SSZ`), `notes`,
  `artifacts` and `downloads`. Each file record is exactly `path`
  (`/agent/stable/releases/<version>/<file>`), `size` and `sha256`. Clients
  reject unknown fields, so no field can be added.
- **Targets:** v1 has `artifacts.darwin-aarch64` (`.app.tar.gz`) and
  `downloads.macos` (`.dmg`) only; older macOS clients reject any other
  download key. v2 has `linux-x86_64` and `linux-bundle`, both the same
  `.tar.gz`. v3 has `windows-x86_64` (`.tar.gz`) and `windows` (`-setup.exe`).
  v4 has `windows-aarch64` and `windows-arm64`.
- **Payloads:** the manifest's `build` must equal the package's build number
  (macOS `CFBundleVersion`, Linux `bundle.json`, Windows `bundle-info.json`),
  which is why packages and manifests come from the same run. The macOS archive
  root stays `Maple Agent.app`; older clients reject `Maple Work.app`. The Linux
  bundle keeps the executable shim at `usr/lib/ld-linux-x86-64.so.2` that
  pre-host-glibc releases check before updating.

## Installation

macOS stages the new app beside the installed one and swaps the bundles.
Linux swaps the `maple-agent.app` directory. Windows installs each version to
`versions/<version>-<build>/` and switches `current.json`. The stable
`maple-agent.exe` launcher and uninstaller that NSIS installed are never
replaced by in-app updates, and old launchers check every new generation, so the
Windows generation layout, `.maple` build-info section and Authenticode signer
must stay compatible with them. The previous installation is kept until the new
version starts. When an installation can't be replaced (read-only location, disk
image, missing permissions), the app offers the signed manual download.
