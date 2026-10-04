# Maple Agent build profiles

`MAPLE_RELEASE_PROFILE=dev` or `prod` selects a packaged profile at compile
time. `release-profiles.json` is the shared public configuration used by both
Rust build scripts and packaging. Neither profile needs a developer shell or
runtime environment variables to select its API, billing API, public client
ID, or PCR trust environment.

| Profile | Application | Bundle ID | State namespace | API / billing trust |
| --- | --- | --- | --- | --- |
| Dev | Maple Agent Dev | `cloud.opensecret.maple.agent.dev` | `maple-agent-dev` | Hosted Dev API and billing; development PCR |
| Prod | Maple Agent | `cloud.opensecret.maple.agent` | `maple-agent-prod` | Hosted Prod API and billing; production PCR |

The public endpoints and client ID match `scripts/ci/_common.sh`'s existing
Research Dev and Prod service selection. Billing browser pricing and checkout
return pages retain the existing `https://trymaple.ai` destinations.

Packaged builds ignore inherited `MAPLE_API_URL`, `MAPLE_BILLING_API_URL`,
`MAPLE_CLIENT_ID`, and `MAPLE_UPDATE_REPO` overrides. A build fails if a supplied
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
identity is `cloud.opensecret.maple.agent.debug` and display name is Maple Agent
Debug. Release packaging accepts only Dev or Prod metadata.

`maple-agent --build-info` prints one JSON object and exits before changing the
environment, initializing a GUI/backend, logging, or accessing state. Fields
are `profile`, `display_name`, `bundle_id`, `data_namespace`, `api_url`,
`billing_api_url`, `client_id`, `pcr_environment`, `version`, `git_revision`,
`source_sha`, `update_tag_prefix`, and `prerelease`. `git_revision` is the short
revision with a possible `-dirty` suffix; `source_sha` is the full source commit.
Tarball/pure Nix builds without Git report `unknown` for unavailable revisions.

Update discovery remains a manual release-page banner. Prod accepts only
non-draft GitHub stable releases tagged `maple-agent-vMAJOR.MINOR.PATCH`. Dev
accepts only non-draft GitHub prereleases tagged
`maple-agent-dev-vMAJOR.MINOR.PATCH`. Both reject SemVer prerelease/build
suffixes, Research tags, and the other Agent channel. Packaged builds pin
`MaplePrivacyLabs/Maple`; unpackaged builds retain validated repository
overrides and the Prod tag namespace. `MAPLE_DISABLE_UPDATE_CHECK=1` remains
available for every profile. No update is downloaded or installed.
