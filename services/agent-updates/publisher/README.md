# Agent release publisher

This tooling publishes the independent Agent feed. It does not change Research,
create Cloudflare resources, generate keys, create GitHub releases or execute
downloaded applications. Run it from a trusted checkout with the root pinned
`nix develop --no-update-lock-file .#ci` shell (Python, GitHub CLI and minisign).

The workflow separates read-only package verification, bucket reservation,
signing and publication. Reservation/publication use bucket-scoped S3 credentials;
the signing process receives a dedicated unencrypted minisign secret-key file.
Credentialed jobs reuse the existing `updates-production` environment and BWS
access credential. The pinned BWS action does not export to later steps; each
consuming step explicitly maps only the values it needs. This does not narrow
the underlying BWS grants or the environment's existing secret context. Pinned
tools are realized before retrieval. No new machine accounts or access tokens
are required; the exact item-ID inputs are listed in the [service setup](../README.md#publication-and-authority).
The public key must already be pinned both in the selected build's source profile
and in the publisher's trusted source profile. An empty key list blocks publication.

Normal publication defaults to macOS, Linux and Windows at the same version,
build and source. Omit a platform only explicitly for a historical producer
that did not build it. Windows uses the same production setup, protected
`updates-production` stages and exact-package verification as the other platforms;
there is no separate Windows activation variable or approval path.

```sh
publisher=services/agent-updates/publisher/publisher.py
python3 "$publisher" trust --run-id BUILD_RUN_ID --linux --windows --output trust.json
# All three platform packages come from this one verified producer run.
python3 "$publisher" prepare --run-id BUILD_RUN_ID --version 0.0.12 \
  --build BUILD_RUN_NUMBER --macos macos-artifacts --linux linux-artifacts \
  --windows windows-artifacts --notes-file notes.txt \
  --public-key public-key.pub --output bundle
python3 "$publisher" reserve --bundle bundle --public-key bundle/public-key.pub \
  --endpoint https://ACCOUNT.r2.cloudflarestorage.com --bucket AGENT_BUCKET
python3 "$publisher" sign --bundle bundle --public-key bundle/public-key.pub \
  --secret-key /private/temporary/signing-key
python3 "$publisher" publish --bundle bundle --public-key bundle/public-key.pub \
  --endpoint https://ACCOUNT.r2.cloudflarestorage.com --bucket AGENT_BUCKET \
  --base-url https://updates.maple.ai
```

The low-level CLI selects platform packages explicitly with `--linux` and
`--windows`, retaining historical bundle preparation. The normal workflow
supplies both. All included packages must come from that same selected producer
run, with the same source, version and build.

`trust` returns the source SHA, selected artifact names, exact run attempt,
publication date and build number as JSON. Only completed trusted master push or
dispatched runs of `agent-desktop-build.yml` qualify, never the nightly Dev run.
The selected Prod signing/package and downloaded
consumer validation jobs must have succeeded; an aggregate success or skipped job
is insufficient, while an unrelated Dev failure does not block a qualified Prod
package. The latest job inventory retains successful producer jobs when only
failed consumers were rerun. `prepare` repeats this check and verifies every final downloaded file's
GitHub attestation against the exact source SHA, master ref and signer workflow.
The self-hosted-runner denial applies to the job generating that attestation.
Final package attestations are generated on GitHub-hosted jobs; their input
binaries are compiled on the workflow's Blacksmith runners.
Package metadata, public Prod profile, inventory, notarization results and streaming
checksums are validated before copying the binary payloads into the bundle. The
macOS archive's bounded regular `Info.plist` member is read without extraction to
match the selected run's build number, version and application identity. Sparse
archives are rejected; extension headers and cumulative metadata are bounded.
Windows additionally requires the concrete successful `Build Windows binary (prod)`,
`Signed Windows (prod)` and `Verify downloaded Windows (prod)` jobs. Its exact
five-file final artifact contains the signed NSIS setup `.exe`, generation
`.tar.gz`, `build-info.json`, `manifest.json` and `SHA256SUMS`. Every file's
attestation is checked. The bounded Windows archive scan requires the fixed
ordinary-file/directory layout and exact descriptor, including source, build and
application identity; embedded build-info bytes must match the attested sidecar.
Links, extensions, auxiliary entries, malformed end markers and trailing content
are rejected without extracting or executing downloaded files. The generation
archive contains the signed application executable and build metadata; the
stable launcher and NSIS uninstaller remain outside each generation.

The manifest build number is the selected build run's `run_number`, matching the
packaging workflow's `CFBundleVersion` input and Linux `bundle.json` descriptor.
The Linux descriptor is read with bounded archive metadata, entry and expanded-size
limits, without extraction or execution; its exact Prod identity, version, build,
source, entrypoint and executable must match the selected release. Its application version must match
the selected packages and be a canonical stable SemVer. The default publication
date is the build run's creation date, making preparation deterministic across
retries. `--pub-date` can select another UTC date, but retries must preserve it,
the notes and every other reserved byte. No timestamps are inserted when signing.

Linux uses an actual `.tar.gz` bundle and a separate signed v2 feed. Preparation
always writes the mac-only v1 `manifest.json`, preserving the wire shape accepted
by installed Mac clients. With `--linux`, it additionally writes
`linux-manifest.json` with format `maple-agent-update-v2`, only the
`linux-x86_64` artifact and the identical `linux-bundle` download. The two
manifests share product, channel, version, build, source, date and notes. The
same primary key signs both exact documents as `envelope.json` and
`linux-envelope.json`. Linux publication also requires the canonical compiled
Linux feed in both the selected source profile and trusted publisher profile;
Mac-only producers do not need that field.

Windows uses its own strict `maple-agent-update-v3` document in
`windows-manifest.json`. Its sole `windows-x86_64` artifact is the generation
`.tar.gz`; its sole `windows` download is the signed setup `.exe`. These are
different payloads. With `--windows`, preparation writes that document and its
`windows-envelope.json` signed with the same primary key. The canonical compiled
Windows feed must be pinned in both selected-source and trusted-publisher
profiles. Version, build, source, date and notes must agree with all included
documents. macOS v1 and Linux v2 retain their existing wire shapes.

The canonical compiled feeds use `https://updates.maple.ai`; both source and
trusted-publisher profiles must pin those exact URLs. Packages built before the
hostname change keep checking `updates.trymaple.ai`, so retain delivery there.
The signed manifest uses hostless artifact paths and works unchanged on both
hosts. New publication preparation selects a package from the coordinated new
profile; it does not reinterpret an older package's compiled feed. Existing
prepared bundles retain their exact bytes for reservation/publication retries.

`reserve` creates `stable/releases/V/publication-manifest.json` using
`If-None-Match: *`. This private object reserves the exact unsigned bytes before
the signing stage. It also reserves `publication-metadata.json`, an exact hash and
size inventory of the private audit sidecars. Both digests are bound by the
reservation receipt. When Linux is included, it separately reserves
`publication-linux-manifest.json` and binds `linux_manifest_sha256` in that same
receipt. Linux payload hashes are inside the exact reserved Linux document; its
sidecars are inside the reserved metadata inventory. Windows equivalently reserves
`publication-windows-manifest.json` and binds `windows_manifest_sha256` in the
receipt. All selected bytes are
reserved before signing. It recovers each existing complete signed manifest into
the bundle, preserving every envelope on a retry. `sign` requires the reservation
receipt, signs the exact bytes with prehashed minisign and the exact trusted
comment `product=maple-agent channel=stable version=V`, then verifies the signature.

`publish` revalidates the envelope and local payload hashes, confirms the durable
reservation and snapshots the current feed's ETag. It creates immutable payloads,
and the bounded private audit sidecars, verifies each by reading its content back,
then creates the complete signed `stable/releases/V/manifest.json` and promotes
`stable/latest.json` using `If-Match` (or `If-None-Match: *` for the first release).
Existing immutable content is streamed and reused only if its bytes match,
before attempting a write against a possibly locked release object. Missing
objects still use create-only writes; a competing creation must match exactly.
A partially uploaded release resumes without replacing completed objects. A
concurrent pointer change stops promotion. Re-running an already published exact
release verifies the objects, repairs missing objects with create-only writes and
leaves the pointer unchanged; a different envelope at the same or lower version is
rejected. Failed attempts never delete objects or automatically restore a pointer.

For Linux, the immutable signed envelope is
`stable/releases/V/linux-manifest.json` and the separate conditional pointer is
`stable/linux/latest.json`. The public paths are
`/agent/stable/linux/latest.json` and
`/agent/stable/releases/V/linux-manifest.json`; `/download/agent/stable/linux`
redirects to the signed tar bundle. Both pointers are authenticated and snapshotted
before any promotion. Every selected payload, audit sidecar and complete envelope
is verified before either pointer advances. The two CAS writes are deliberately
independent: if the second fails, an exact retry verifies the already advanced
feed and safely promotes the remaining one. A newer or conflicting pointer blocks
the retry; nothing restores or rolls back a pointer automatically. Mac-only
publication never reads or changes the Linux pointer. The existing four stages
and three protected environment approvals remain unchanged.

Windows uses immutable `stable/releases/V/windows-manifest.json` and independent
conditional pointer `stable/windows/latest.json`. The public feed is
`/agent/stable/windows/latest.json`; `/download/agent/stable/windows` redirects to
its signed setup executable. Both Windows payloads are immutable and enumerated
in that signed document. All selected feed pointers are authenticated and
snapshotted before uploads, and each keeps its own opaque ETag for conditional
promotion. An interrupted multi-feed promotion can resume only from the same
reserved and signed bytes. A concurrent Windows pointer change blocks that
promotion and remains untouched. Publications without Windows never read or
change its pointer.

The bundle contains raw, provenance-checked build-info, package manifest,
SHA256SUMS and macOS notarization logs under `metadata/`, prefixed by platform,
plus the stable run/source/artifact identity and attestation policy in
`provenance.json`. The current diagnostic run attempt stays in CI's `trust.json`;
it is omitted from durable metadata so a successful verifier-only rerun can reuse
the exact release. A finite filename allowlist and a 4 MiB limit per file
bound this audit inventory. Reservation, signing and publication require exactly
the five macOS sidecars and provenance, plus the three Linux sidecars when the
separate Linux document is included and three Windows sidecars when its document
is included; a missing, empty, partial or extra inventory is rejected. The exact
bundle file inventories, including payloads, metadata, manifests and key, are:

| Included platforms | Prepared | Reserved | Signed |
| --- | ---: | ---: | ---: |
| macOS | 12 | 13 | 14 |
| macOS + Linux | 17 | 18 | 20 |
| macOS + Windows | 18 | 19 | 21 |
| macOS + Linux + Windows | 23 | 24 | 27 |

These
objects are stored immutably under
`stable/releases/V/metadata/` and verified before the complete manifest appears.
They remain private because the Worker serves only artifacts enumerated in the
signed wire manifest. The publication reservation and metadata inventory are also
private. Missing sidecars can be repaired on an identical retry; differing bytes
are a conflict. Original CI artifacts retain the independently verifiable package
attestations.
R2 credentials use `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`; the publisher
signs every S3 request with SigV4 and includes conditional headers in that signature.
All network requests use HTTPS, fixed timeouts and no implicit redirects. Errors
do not print subprocess output, signing-key paths, request headers or credentials.

Publication finishes by checking the live latest and immutable signed manifest,
signature, full artifact hashes, HEAD lengths, one-byte ranges, and installer
redirect destinations. All public requests use the native client's
`Maple-Agent-Updater/1` identity and `Accept-Encoding: identity`, including full
payload downloads. They also send `X-Maple-Download-Probe: 1` so publication
verification does not inflate private download analytics. Repeat that separately with `verify-live --bundle bundle
--public-key bundle/public-key.pub --base-url https://updates.maple.ai` after a
transport failure or operational change. If this final check fails after promotion,
inspect the live state and retry the identical bundle; publication may already
have completed. This check does not install or relaunch an application.
The same read-only verification command can target `https://updates.trymaple.ai`
to check compatibility delivery for older installed clients.

Run offline tests:

```sh
nix develop --no-update-lock-file .#ci -c python3 -m unittest discover \
  -s services/agent-updates/publisher -p 'test_*.py'
```

The fake-store tests cover interrupted writes, reservation/envelope reuse,
immutable conflicts, pointer races and already-published recovery. Their synthetic
signatures exercise envelope plumbing without making a cryptographic claim.
The pinned-shell minisign tests verify the shared public wire fixture and an
existing signed pointer using raw or commented public-key files and pinned-key
rotation, including an identical publication retry and missing-object repair. They generate
disposable offline keys only inside a private temporary directory, sign a reserved
manifest, and exercise verification, tampering and deterministic signing retries.
Mock-transport tests exercise fixed-time SigV4 vectors, signed conditional headers,
bounded reads, streamed uploads and hashes, HTTP failures and redirect refusal.
These offline checks do not establish the configured R2 bucket's behavior. Before
activation, its rehearsal must include locked-prefix identical reservation and
publication retries, a different-byte immutable conflict, missing-object repair,
and a competing pointer promotion that leaves the competing bytes unchanged.
No production credentials, resources or live release are used by these tests.
