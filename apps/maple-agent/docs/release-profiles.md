# Maple Work build profiles

`MAPLE_RELEASE_PROFILE=dev` or `prod` selects a packaged profile at compile
time. `release-profiles.json` is the shared public configuration used by both
Rust build scripts and packaging. Neither profile needs a developer shell or
runtime environment variables to select its API, billing API, public client
ID, or PCR trust environment.

| Profile | Application | Bundle ID | State namespace | API / billing trust |
| --- | --- | --- | --- | --- |
| Dev | Maple Work Dev | `cloud.opensecret.maple.agent.dev` | `maple-agent-dev` | Hosted Dev API and billing; development PCR |
| Prod | Maple Work | `cloud.opensecret.maple.agent` | `maple-agent-prod` | Hosted Prod API and billing; production PCR |

Display branding is independent of installation identity. macOS keeps the
`Maple Agent.app` and `Maple Agent Dev.app` filenames and updater archive roots;
Linux keeps `maple-agent.app` and `maple-agent-dev.app`. Executables, bundle IDs,
state namespaces, settings, update feeds, and artifact filenames are unchanged.
macOS uses `Resources/en.lproj/InfoPlist.strings` for the new display name while
the base `CFBundleDisplayName` matches the existing bundle filename for Finder.

The API endpoints and client ID match `scripts/ci/_common.sh`'s existing
Research Dev and Prod service selection. Billing browser pricing, checkout
success/cancellation, and customer portal return pages use the compiled
`web_url`: `https://app-dev.maple.ai` for Dev and `https://app.maple.ai` for
Prod. Dev remains protected by Cloudflare Access.

Packaged Dev selects `https://auth-dev.maple.ai` and Prod selects
`https://auth.maple.ai` as their compiled `auth_origin` for hosted native
sign-in with a loopback return.
The app waits for browser completion and allows cancellation; no callback URL
needs to be pasted. Unpackaged builds have `auth_origin: null` and retain their
existing provider callback paste flow. Hosted sign-in cannot be enabled or
redirected through an inherited environment variable. This Agent profile does
not change Research's Auth entry or the backend's default provider callbacks.

Packaged builds ignore inherited `MAPLE_API_URL`, `MAPLE_BILLING_API_URL`,
`MAPLE_CLIENT_ID` overrides. The updater feed and trusted keys are also immutable;
`MAPLE_UPDATE_REPO` is no longer supported. A build fails if a supplied
`VITE_OPEN_SECRET_PCR_ENVIRONMENT` disagrees with its profile; the runtime crate
bakes the required trust selection even when that variable is absent. Unknown
or empty `MAPLE_RELEASE_PROFILE` values also fail the build.

Configuration, credentials, logs, account settings, Goose state, and task
history use the profile namespace below the platform configuration and local
data roots. Absolute `XDG_CONFIG_HOME` and `XDG_DATA_HOME` may still change those
base roots, with the profile namespace always appended. Packaged startup never
adopts `maple-gpui`, unpackaged `maple-agent`, Research, or the other channel's
state. Installing both channels therefore keeps separate sign-ins and history.

With `MAPLE_RELEASE_PROFILE` unset, the binary reports `unpackaged`, retains the
existing `maple-agent` namespace, runtime endpoint/client overrides, explicit
compile-time PCR selection, and legacy `maple-gpui` adoption. Its debug bundle
identity is `cloud.opensecret.maple.agent.debug` and display name is Maple Work
Debug. Release packaging accepts only Dev or Prod metadata.

`maple-agent --build-info` prints one JSON object and exits before changing the
environment, initializing a GUI/backend, logging, or accessing state. Fields
are `profile`, `display_name`, `bundle_id`, `data_namespace`, `api_url`,
`billing_api_url`, `web_url`, `auth_origin`, `client_id`, `pcr_environment`, `version`, `git_revision`,
`source_sha`, `update_tag_prefix`, `prerelease`, `update_feed_url`, and
`update_public_keys`. `git_revision` is the short
revision with a possible `-dirty` suffix; `source_sha` is the full source commit.
Tarball/pure Nix builds without Git report `unknown` for unavailable revisions.

Packaged Prod builds use only the compiled Agent stable feed:
`https://updates.maple.ai/agent/stable/latest.json` on macOS, or
`https://updates.maple.ai/agent/stable/linux/latest.json` on Linux, and
`https://updates.maple.ai/agent/stable/windows/latest.json` on Windows. The checked-in
`update_feed_url`, `linux_update_feed_url` and `windows_update_feed_url` select
those targets; embedded build-info retains the existing `update_feed_url` field
with the effective feed.
Older installed packages retain their compiled `updates.trymaple.ai` feeds;
preserve that hostname and both legacy feed paths. Existing macOS/Linux signed
manifest paths and trusted keys are unchanged, so the two hostnames can serve
the same release.
The platform feeds describe the same Agent version, build and source when
publishing multiple platforms. Separate wire schemas preserve installed macOS
client compatibility;
this does not require separate release cadences. The profile pins the
reviewed primary and backup public keys, enabling packaged Prod discovery after
launch. Publication is a separate protected operation; invitations
require the signed update rehearsal for each supported platform. An empty-key profile reports
**Updates not configured** and
makes no update requests. Active trust requires two distinct public keys; the
backup supports planned rotation and does not revoke a compromised accepted
primary.

Windows Dev/Prod installers retain their profile and state identities. Packaged
Prod uses its separate Windows v3 signed feed, a `.tar.gz` update bundle, and an
NSIS `.exe` for manual installation. The macOS v1 and Linux v2 feeds cannot
authorize a Windows update. Windows packages use the existing Authenticode
signing identity alongside the Agent feed's pinned primary and backup keys.
Dev and unpackaged Windows builds remain excluded from production updates.
The final Windows package artifact has exactly five files: the signed manual
installer, its versioned updater `.tar.gz`, `build-info.json`, `manifest.json`
and `SHA256SUMS`. The manifest binds the positive decimal Actions build number,
source, profile, installer and updater names. The bundle's
`maple-agent-windows/` root contains exactly the signed application, the
public build and bundle descriptors, and the license. It omits the
uninstaller.

Windows installations keep a stable signed root launcher and uninstaller, plus
immutable generation payloads under `versions/<version>-<build>/`. A native
helper validates the generation before atomically replacing `current.json`;
the running generation stays intact until shutdown. Manual installation uses
the same native trust and destination checks before changing installed files.
Unsigned Prod previews must refuse installation, while Dev previews have no
production feed or keys. A future change to the stable bootstrap requires an
explicit manual-installer migration; the normal updater changes only the
generation payload.

Dev and unpackaged builds contain no updater feed or keys. No runtime override
can opt either into production updating. `MAPLE_DISABLE_UPDATE_CHECK=1` remains
a hard off for every profile. Release tag prefixes still identify packaging
channels; they are not the updater's source of authority.

Configured Prod checks after launch and approximately hourly, verifies signed
metadata and package contents, and downloads automatically by default. Users
can turn automatic downloads off and download explicitly. Installation waits
for **Restart to Update** and a warning that tasks and remote access stop.
Read-only/translocated apps and unsupported Linux install contexts offer manual
download links from signed metadata. See [updater development and activation](updates.md) for
verification, recovery and the separate production activation boundary.
