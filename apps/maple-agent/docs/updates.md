# Agent updates

Packaged Prod builds use the Agent-only signed update service at
`https://updates.maple.ai/agent/stable/latest.json` on macOS and
`https://updates.maple.ai/agent/stable/linux/latest.json` on Linux, and
`https://updates.maple.ai/agent/stable/windows/latest.json` on Windows x86_64. Dev and unpackaged
builds do not check that service. The Prod profile now pins the reviewed primary
and backup public keys, so packaged Prod builds check the compiled feed after
launch. Builds and production publication remain separate operations;
pinning keys does not deploy or publish anything. Invitations require the
platform-specific signed installed-update rehearsal below. Environment
variables cannot supply a feed or signing key. `MAPLE_DISABLE_UPDATE_CHECK=1`
can only disable checks. A profile with no keys still shows **Updates not
configured** and makes no updater requests.

New packages compile the `updates.maple.ai` feeds. Existing packages still poll
their compiled `updates.trymaple.ai` feeds, which must remain available alongside
the canonical hostname. Both hosts serve the same signed manifests and hostless
artifact paths; this hostname change does not change their bytes, wire formats,
signing keys or installation identity. Migrating the compiled feed takes effect
only after installing a new package; there is no runtime feed override.

This feature does not change Research's updater, update Worker, publisher,
release selection, signing configuration, or download paths. Agent publication
does not create GitHub Releases or tags and cannot change the repository's
latest Release.

## User behavior

The desktop checks at startup and about hourly, with jitter. Automatic download
is enabled by default and can be disabled in Settings. An authenticated update
is downloaded in the background, but replacement requires **Restart to Update**
and confirmation that active work will stop. The update banner appears below the
title bar across screens, including existing tasks, remote tasks, and Settings;
it always describes the update for this computer. **Later** hides the banner while
keeping the prepared update available in Settings. About, the native application
menu, and the tray provide update access.

The banner keeps the message and actions on one compact row. Long status or error
messages wrap without hiding the actions.

Paired controllers can inspect a computer's version and prepared update through
the optional [experimental remote update controls](remote-access.md#experimental-remote-updates).
Opening that view reads the remote's reported state. **Refresh update status**
requests a new update check on a compatible remote desktop; automatic downloads
still follow that computer's saved preference. It never installs an update or
checks the controller's updater. Failed checks retain clearly labelled last
reported information. Older peers without this capability require a local check.
An acknowledged request is consumed by the execution computer's running desktop
and uses this same installer. Password-protected hosts are excluded even while
unlocked; an independent host-side check runs at the final shutdown boundary.
The controller's updater is never installed or restarted by this operation.
Daemon-only hosts require a local update. Remote management requires compatible
remote protocol majors, just like ordinary task access. The attachment change
requires wire 7 on both computers; it does not add a cross-version management
bridge. Update the execution computer first while both still use wire 6, then
update the controller through its local updater. During the expected mismatch,
retain the original install operation rather than resubmitting it; verify the
running identity and its receipt after both computers reconnect on wire 7.
Updating the controller first requires updating the execution computer directly.
A remote mismatch does not disable the controller's local update controls.
Definite install refusals are shown as declined requests and require a new check
before another confirmation. A potentially sent request with an unknown outcome
keeps its original operation identity; refreshing never resends installation.

### Remote upgrade qualification

Changes to pairing, remote update controls or the installer require a composed
upgrade test in addition to package verification. Package consumers establish
provenance, signatures and package startup; they do not establish a paired
remote upgrade.

The normal Agent test suite includes
`cpace_confirm_restart_and_mutual_pairing_feed_the_real_install_grant_policy`.
It exchanges real CPace and ConfirmGrant frames between two loopback endpoints,
reopens both durable stores, and checks the production install-grant policy.
It covers lost pairing acknowledgement, independent pairing directions and
revocation. Do not replace its pairing-produced records with pre-authorized
fixtures. Enrollment, account sessions and signed installation are separate
boundaries.

Before claiming native remote-upgrade coverage, record these observations from
isolated installations and a disposable account:

1. Pair through the normal application path, restart both applications, and
   request the execution computer's exact verified update from its controller.
   Observe admission, the durable receipt, shutdown, replacement, relaunch and
   the new running version/source. The controller must remain unchanged.
2. Change the signed feed after observing Current. Explicit refresh must produce
   a new HTTP check on the execution computer and report that result. A cached
   heartbeat, failed check, disconnected desktop or unsupported old peer cannot
   be reported as a successful fresh check.
3. Exercise a definite pre-admission refusal and a lost acknowledgement after
   sending. The latter must retain its operation ID without replaying the
   installation. Include re-pairing or disconnect while its reply is pending.
4. For a wire-major transition, exercise the supported remote-first order with
   the previous published application. Record the expected incompatibility if
   the controller is updated first and verify the direct local recovery path.

Preserve failures and identify the exact source, binary hashes, profiles,
operating systems and signing identities used. A Local fixture signed by a test
key can establish that fixture's lifecycle; it does not establish production
Developer ID, notarization, production account routing or public publication.

The installer holds the per-application lock while staging a verified bundle
beside the installed application and while that update is ready. It checks the
fresh signed offer again and holds the profile's existing startup and host ownership locks from host stop
through replacement. It checks the payload and staged tree again before macOS's
atomic bundle exchange, Linux directory exchange, or Windows generation-pointer commit. A previous bundle remains in the installation until
the new installed version starts successfully and validates its receipt. This
is crash recovery retention, not automatic rollback or permission to install an
older signed version.

After replacement commits, the old process retires its backend and enters
**Restart required** before asking the platform to relaunch. If the restart
helper cannot start, **Restart Maple** retries only that committed application
path; Quit remains available so the user can reopen the installed update.
Checks, downloads, a second installation and new backend work cannot resume in
the old process. A relaunch failure never triggers an automatic downgrade.

Read-only locations, disk images, translocated bundles and accounts lacking
replacement permission offer a manual download before active tasks stop. The
installation lock uses one shared inode for the canonical application, readable
by other Mac accounts; Linux also binds staging and cleanup to its canonical
installation. Separate profile locks do not replace that exclusion.
Permissions are rechecked before host shutdown and replacement can still fail
safely if the filesystem changes afterward.

Linux x86_64 uses a persistent user-local directory bundle distributed as a
`.tar.gz`, with launcher and desktop integration. The bundle carries its loader
and client libraries; users need no FUSE mount or Nix runtime. Host graphics
drivers, CA certificates and desktop services remain required. Read-only,
inconsistent or unsupported installations offer the signed manual download.
Unsupported architectures do not install another architecture's payload.
See [desktop builds](desktop-builds.md) for installation commands.

The bundled loader makes `current_exe()` refer to the loader. Detached hosts and
reopen use the validated persistent `AppRun`; the bounded SQL integrity child
executes the validated loader/payload directly to preserve parent ownership and
private stdin. After replacement, restart targets the committed bundle's `AppRun`.

### Windows installation and recovery

Windows starts with a per-user NSIS installer, not a legacy-layout migration.
The registered installation contains a stable signed `maple-agent.exe` bootstrap
and `uninstall.exe`, `current.json`, and complete generations under
`versions/<version>-<build>/`. Shortcuts and CLI invocations use the bootstrap;
it authenticates the selected generation and forwards native arguments. CLI
help, version, build information and ACP belong to that selected generation.
The stable bootstrap and uninstaller are not replaced by in-app updates;
changing their contract requires a separately qualified manual installer.

An update verifies the signed v3 offer, archive hash, exact bounded generation
inventory, embedded profile/version/source identity, and native Authenticode
trust and timestamp for `Mutiny Wallet Inc. dba OpenSecret`. Prod never accepts
unsigned preview code. The installer writes a complete new generation before
atomically replacing `current.json` with a same-directory write-through rename.
The running executable is not overwritten. The previous generation remains
until the new application's successful store checkpoint authenticates its
receipt and bytes. A missing or malformed pointer and newly launched inactive
generations fail closed; there is no implicit launch of the older root binary.

Manual installation uses a durable transaction intent tied to the actual
installer process and creation time. A concurrent installer is refused; an
interrupted installer can resume without deleting unknown files. An incomplete
manual transaction does not prevent launch of the authenticated current
generation. Native checks reject reparse points, hardlinks, unsupported paths
and directories writable by another unprivileged account. The updater does not
repair unsafe ownership or permissions.

Storage ownership must belong to the current user, SYSTEM or the built-in
Administrators group, the same principals permitted to write its private DACL.
This admits Windows' elevated default owner without admitting another
unprivileged owner or writer. Native-created private objects explicitly select
the user owner. Unsigned Dev previews require both an absent embedded PE
certificate and no registered local SHA-256/SHA-1 catalog membership; a
catalog-signed system file cannot be treated as an unsigned Dev executable.

Restart uses native process arguments and waits for the retiring process's
exact PID and creation time. It does not construct a PowerShell command from
the installation path. If relaunch fails, reopen the registered launcher;
the committed pointer and retained generation remain available. Windows uses
the same desktop and host exclusion, remote update admission and manual
controller policy described above.

Normal launch verifies the installed code with local Windows trust data and no
certificate-network retrieval. New payload admission retains chain/revocation
verification. This keeps an already installed application independent of live
certificate-service availability; it does not obtain fresh revocation knowledge
at every launch. Local distrust, invalid signatures and changed code still fail.

## Maple Work filename bridge

Display branding is Maple Work; current macOS packages still contain
`Maple Agent.app`. The installer accepts either `Maple Agent.app` or
`Maple Work.app` as the installed Prod bundle and exactly one of those roots
inside a macOS update archive. Dev names and arbitrary application names are
not admitted. Bundle ID, signing identity, signed metadata, payload hashes,
version checks and the existing extraction limits remain mandatory.

The archive root is normalized to the legacy staging name inside the private
staging directory. Replacement and relaunch preserve the existing installation
path, and saved preparations/receipts keep their existing format. The two names
in one parent directory share the legacy install lock, preventing concurrent
preparation or startup cleanup from crossing names. If both apps exist, updating
one leaves the other intact; this bridge does not merge or remove installations.

This is compatibility preparation, not a filename migration. Do not change the
root published on the existing macOS feed yet: older clients require
`Maple Agent.app` and can skip the bridge release because they fetch one latest
offer. A later rename needs a separately reviewed delivery plan that retains a
legacy-root upgrade path for those clients (or an explicit manual-upgrade
policy), coordinates package generation/verification and tests signed installed
updates. Automatically moving an existing app to a new path additionally needs
collision, interruption, relaunch and recovery handling. This change neither
renames existing installations nor changes feed/schema, release paths or data
locations. Linux retains its existing `maple-agent.app` installation identity;
its visible branding and updater guidance use Maple Work.

## Preview the banner in a development build

Unpackaged debug builds can show a presentation-only update banner:

```sh
cd apps/maple-agent
nix develop --no-update-lock-file -c just build
nix develop --no-update-lock-file -c env MAPLE_PREVIEW_UPDATE_BANNER=1 ./target/debug/maple-agent
```

Both building and launching must run inside the component Nix environment.
On Linux, GPUI loads Wayland and Vulkan libraries at runtime; the unwrapped
`target/debug/maple-agent` binary needs the shell's library paths. Launching it
outside that shell can panic with `NoWaylandLib`. Alternatively, enter an
interactive `nix develop --no-update-lock-file` shell first and run both commands
there.

Use your workspace's launcher instead of the binary command if it manages your
environment and isolated state; pass the flag to that launcher. Keep your normal
API/auth configuration and state roots. The flag does not select an environment.

The banner is labelled **Preview only**. Navigate between a new task, an existing
conversation, and Settings to check visibility. **Restart to Update** only shows
an explanation; it never downloads, installs, or restarts. **Later** hides the
preview until the next launch with the flag. Settings and remote update status
continue to show the real updater state, not the fixture. Packaged Dev/Prod and
release builds ignore the flag. This checks presentation, not signed updating.

## Wire and trust contract

The response is an envelope with two fields: `manifest` is the exact JSON string
that was signed, and `signature` is a four-line, prehashed minisign signature.
The authenticated trusted comment is
`product=maple-agent channel=stable version=<version>`. Clients verify a compiled
Agent public key before interpreting the manifest. Research keys are not used.

The `maple-agent-update-v1` manifest identifies `maple-agent`, `stable`, a
canonical stable SemVer, a positive decimal build number, the source commit,
UTC publication date, bounded plain-text notes, updater artifacts, and manual
downloads. macOS arm64 uses `.app.tar.gz` for installation and `.dmg` for manual
download. Legacy v1 parsing retains its optional `.AppImage` contract, but new
macOS publications remain mac-only for compatibility with installed clients.
Linux uses a separate `maple-agent-update-v2` envelope with only
`artifacts["linux-x86_64"]` and `downloads["linux-bundle"]`, both describing the
same `.tar.gz`. Its immutable envelope is `releases/<version>/linux-manifest.json`;
macOS retains `releases/<version>/manifest.json`. Clients bind their target to the
matching schema and compiled feed before advancing state. Windows uses
`maple-agent-update-v3` with exactly `artifacts["windows-x86_64"]` for its
generation `.tar.gz` and `downloads["windows"]` for the separate signed NSIS
`.exe`. Its immutable envelope is `releases/<version>/windows-manifest.json`.
Older v1/v2 manifests retain their existing strict fields and bytes; they do
not admit Windows fields. Each artifact has a
signed path, byte size, and SHA-256.

Paths stay under `/agent/stable/releases/<version>/<filename>`. Metadata is
bounded to 128 KiB, manifest text to 64 KiB, signature text to 4 KiB, release notes
to 16 KiB, and each artifact to 4 GiB. Download redirects admit only the two
compiled HTTPS origins `updates.trymaple.ai` and `updates.maple.ai`, on port 443;
requests have timeouts and no application cookies,
credentials, referrer, or inherited proxy. HTTP content decoding is disabled so
signed sizes and resume offsets refer to the same bytes. A partial response must
match the requested range, and the complete file is hashed before preparation.

Updater state is private to the compiled profile. It records the highest verified
version and rejects older offers. A moved application keeps that high-water mark
and clears its old preparation binding. Restored preparations are authenticated
and rehashed; saved paths are not authority. Before installation a withdrawn or
changed offer clears preparation. A newer offer immediately starts the normal
check/download path. A malicious service can withhold a release; signed metadata
has no expiry, and this implementation does not claim freeze-attack prevention.

Discovery and anti-rollback state live in the profile's `updates/` directory,
outside the resettable `store-v1/` SQLite data. The startup checkpoint waits for
the current Open store and its saved UI settings to load; locked, damaged or
loading stores retain the previous application. Automatic downloads start with
the saved preference, and store reattachment does not create another polling loop.
Restart flushes pending frontend settings before the host's coordinated durable
shutdown. Installation holds both startup admission and the store's authoritative
owner lock through replacement and the old process's exit. A committed restart
or terminal Quit never sends another old-build request to that stopped host.

macOS installation also requires the expected bundle ID
`cloud.opensecret.maple.agent`, Developer ID team `X773Y823TN`, signed version and
build, hardened code-signing identity, and a bundle tree matching the downloaded
archive. Extraction bounds entries, expanded bytes and path lengths; it rejects
path traversal, hard links, special files, duplicate paths and escaping or cyclic
symlinks while allowing internal framework links. No downloaded executable runs
before replacement.

Linux installation additionally requires canonical `bundle.json` identity,
matching running version/source and the expected persistent layout. Preparation
uses the same bounded archive/tree scanner. Under the install lock, replacement
rechecks the actual installed version and atomically exchanges directories with
`RENAME_EXCHANGE`; unsupported filesystems fail before commit. A stale process
cannot replace a newer installed bundle. Stage names are installation-specific,
and cleanup authenticates the receipt and new installed tree after successful
store bootstrap before deleting a retained previous bundle.

## Owner setup before activation

Implementation PRs do not create infrastructure, production signing keys,
GitHub environments, routes, credentials, releases, or production publications.
Before enabling the feed, prepare and review these concrete operations:

1. The two reviewed Agent public keys are pinned in `release-profiles.json`;
   the primary and backup are distinct from test and Research keys. Build the
   initial `0.0.1` package from this pinned source. Keep the backup private key
   offline and unavailable to CI. Key rotation remains a separately reviewed
   release operation.
2. Reuse the existing `desktop-signing` environment and its five Apple
   signing/notarization secrets for the trusted master Agent packaging job. No
   new app-signing environment, Apple credentials or BWS migration is required;
   keep the existing environment's credentials and policies unchanged.
3. Provision the Agent R2 bucket and Worker. Bind only the Agent bucket. Create
   the two HTTPS routes for `/agent/stable/*` and
   `/download/agent/stable/*` as an owner operation. Keep routes out of routine
   Worker deployment configuration so deploy tokens do not need route-write
   authority. Configure HTTP-to-HTTPS handling in the zone.
4. Reuse the existing `updates-production` approval environment and BWS access
   credential for Agent reservation, update signing and publication. Configure
   the Agent signing-key and bucket-item IDs documented by the service without
   changing the existing environment's policies or Research credentials. The
   workflow maps BWS outputs only into the consuming step; shared BWS grants and
   environment access remain a trusted workflow boundary. Publication must verify that the
   selected artifact's embedded pins and trusted master both contain the chosen
   public key; an environment variable is not a substitute for compiled trust.
5. Deploy with the narrowly scoped existing-Worker token and confirm the R2
   binding can be retained. Verify Agent activation leaves Research working,
   and a later normal Research publication leaves Agent routes working.

The service and publisher's own README documents exact commands and recovery.
Artifact builds remain separate from publication. Only final verified Prod
packages from the trusted desktop workflow may be published; selector jobs and
unsigned PR packages are not release evidence. Immutable version objects are
create-only and pointer promotion is conditional, so retries cannot overwrite
an existing version or race a later release.

## Validation and release gates

Run Agent checks through its pinned environment:

```sh
cd apps/maple-agent
nix develop --no-update-lock-file -c just ci
```

Workflow and configuration changes also require the root pinned
`nix flake check --no-update-lock-file`. Focused core and installer tests cover
signature, schema, anti-rollback, interrupted downloads, saved-state tampering,
archive/path attacks, staging integrity, replacement receipts and lock exclusion.
Prod macOS packaging also runs the real installer archive scanner against each
produced unsigned PR or signed master `.app.tar.gz`, binding the scanned bytes to
the package checksum before final upload/attestation. BSDtar host metadata is
explicitly omitted during creation; signed file contents and existing native
signature/notarization checks remain intact.
They do not prove Developer ID distribution, deployed service behavior, or a
successful real package update.

The focused **Agent Windows Updater Tests** workflow runs the actual updater
and bootstrap component tests on Windows with no signing or publication
credentials. It requires the named native filesystem, trust rejection,
transaction and recovery cases and rejects ignored or empty suites. These
fixtures do not establish official Maple signatures, installed A-to-B updates,
visible GUI or paired remote-upgrade behavior. Record those qualifications
separately from a green component job or an installer consumer.

Windows publication is opt-in and additionally requires the repository variable
`AGENT_WINDOWS_UPDATES_ENABLED=true`. Leave it disabled until signed installed
upgrades, interruption recovery, native relaunch, GUI and paired remote checks
are qualified. Before public Windows downloads, apply the reviewed download
tracking `0002` migration as documented by that service; its existing database
was initialized outside Wrangler's migration history.

Before invites, publish two unadvertised signed increasing versions and run the
actual installed application's check, download, confirmation, host stop, swap
and relaunch through the production service. Verify active host clients cannot
start during installation, the new version works, and a failed or interrupted
attempt preserves a runnable installation. Keep version, build, source SHA,
workflow run, manifest, artifact hashes, OS, install location and observed result
with the rehearsal evidence. The invitation date slips if these gates fail.

For Linux, first verify tar-bundle installation, GUI, host, ACP and reopen on the
available Ubuntu environment, including spaces in the path, a clean runtime
environment and no FUSE or Nix runtime. The first release targets `0.0.4`; an
owner Omarchy test provides additional desktop coverage. Fedora and other distro
compatibility remain unverified until exercised. Then verify a real signed
N-to-N+1 directory replacement/relaunch through the production UI with host
exclusion, persistence and failure recovery. The first installation alone does
not pass the preview's updater gate. Do not infer package proof from a launcher
fixture or Rust unit test; assess audio, portals, CUA and GPU support separately.

Recovery must preserve the signed high-water mark. Fix a bad offer by publishing
a new, higher version; do not rewrite an immutable version or lower the pointer
and expect clients to downgrade. Keep the previous installation until the new
startup checkpoint succeeds. If a disk image, permissions, signature, host stop,
or network check prevents installation, use the verified manual download path
or retry without weakening validation.
