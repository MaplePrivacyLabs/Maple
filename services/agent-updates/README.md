# Maple Agent update delivery

Independent Agent Worker and R2 publisher. Research's `services/updates/`,
`maple-updates` Worker, domain mapping, publishing workflow and installed clients
are unchanged. This code creates no Cloudflare resources and starts no releases.
See [publisher tooling](publisher/README.md) for exact commands and retry behavior.

The checked-in binding names the owner-provisioned private `maple-agent-updates`
bucket. This is configuration, not evidence of Worker deployment or activated
delivery. There is no deploy script or automatic deployment workflow.
Private download metrics use the shared Queue and D1 ledger described in
[download tracking](../download-tracking/README.md). The existing Agent Worker
consumes events from both update Workers; it has no public metrics endpoint.
`workers_dev` and preview URLs are off; routine configuration must never contain
`route`, `routes` or a Custom Domain. Local checks use a separate fixture bucket.

## Local verification

From the monorepo root:

```sh
nix develop --no-update-lock-file .#ci -c ./scripts/ci/agent-updates.sh
```

This runs schema and Worker tests, real workerd/R2 fixture requests, publisher
failure/retry tests and a deploy dry run. Fixtures use fake signatures: they
prove delivery and parsing, while publisher tests and the client own pinned-key
authentication. None of these tests reads a production bucket or executes a
downloaded app. Source checks do not establish Cloudflare permissions or a
successful packaged update.

## Public contract

Only GET and HEAD are supported. Metadata and errors use `no-store` at both
browser and CDN layers. No origin fetch is made on failure.

| Path                                             | Behavior                                                                         |
| ------------------------------------------------ | -------------------------------------------------------------------------------- |
| `/agent/stable/latest.json`                      | Exact v1 signed envelope from `stable/latest.json`                               |
| `/agent/stable/linux/latest.json`                | Exact Linux v2 signed envelope from `stable/linux/latest.json`                   |
| `/agent/stable/windows/latest.json`              | Exact Windows v3 signed envelope from `stable/windows/latest.json`               |
| `/agent/stable/releases/V/manifest.json`         | Immutable v1 signed envelope                                                     |
| `/agent/stable/releases/V/linux-manifest.json`   | Immutable Linux v2 signed envelope                                               |
| `/agent/stable/releases/V/windows-manifest.json` | Immutable Windows v3 signed envelope                                             |
| `/agent/stable/releases/V/FILE`                  | Signed, enumerated artifact; GET/HEAD, single byte ranges, ETag, immutable cache |
| `/download/agent/stable/macos`                   | Uncached 302 to the current DMG                                                  |
| `/download/agent/stable/linux-appimage`          | Uncached 302 only when Linux is present                                          |
| `/download/agent/stable/linux`                   | Uncached 302 to the current v2 Linux tar bundle                                  |
| `/download/agent/stable/windows`                 | Uncached 302 to the current v3 Windows installer                                 |

Missing objects/targets return 404. Malformed metadata, mismatched stored sizes
and bucket failures return bounded 503 responses. The Worker checks schema and
minisign structure; it has no signing key. Clients must authenticate exact
manifest bytes with their pinned keys before trusting fields or hashes.

The envelope has exactly `manifest` (JSON text) and `signature` (minisign text).
The manifest has `format: maple-agent-update-v1`, `product: maple-agent`,
`channel: stable`, canonical stable SemVer `version`, positive decimal-string
`build` (up to 18 digits), lowercase `source_sha`, UTC second-resolution
`pub_date`, plain-text `notes`, `artifacts`, and `downloads`.

`artifacts.darwin-aarch64` is the `.app.tar.gz`; `downloads.macos` is the DMG.
Optional `artifacts.linux-x86_64` and `downloads.linux-appimage` must describe the
same `.AppImage`. Each artifact contains exactly `path`, positive `size` (at most
4 GiB) and lowercase SHA-256 `sha256`. Paths must start
`/agent/stable/releases/<version>/` and end in one ASCII filename using letters,
digits, `.`, `_`, `-`; traversal and encoded separators are rejected. Supporting
build-info and provenance objects in R2 are private, not public artifact routes.

The separate Linux feed uses `format: maple-agent-update-v2` with the same exact
top-level fields and limits. Its `artifacts` contains only `linux-x86_64`, a
`.tar.gz` bundle; its `downloads` contains only `linux-bundle`, describing that
same path, size and hash. It cannot contain macOS or legacy AppImage targets.
The original latest feed and immutable `manifest.json` require v1; Linux latest
and `linux-manifest.json` require v2. The historical v1 schema and AppImage alias
remain supported without reinterpreting stored signed bytes.

The separate Windows feed uses `format: maple-agent-update-v3` with the same
top-level fields and limits. Its `artifacts` contains only `windows-x86_64`, a
`.tar.gz` updater bundle; its `downloads` contains only `windows`, a distinct
`.exe` installer. Both files are bound to their own signed path, size and hash.
Windows latest and immutable `windows-manifest.json` require v3. Windows fields
are rejected by the historical v1/v2 schemas, including null fields, and a v3
document cannot contain macOS, Linux or AppImage entries.

Artifact lookup reads the requested version's fixed `manifest.json` first and
serves an exact enumerated match without consulting Linux metadata. A missing
or valid nonmatching v1 manifest permits an exact match from that version's
`linux-manifest.json`, then `windows-manifest.json` when neither earlier
manifest enumerates the path. An earlier exact match returns without consulting
later platform manifests. Every consulted manifest must have its required format
and requested version; corrupt metadata returns 503 without fallback. No latest
pointer authorizes a release artifact, and no unsigned auxiliary object is
exposed by any feed.

Limits: 128 KiB envelope, 64 KiB manifest, 4 KiB minisign signature, 16 KiB notes.
Signature format is prehashed minisign Ed25519 with exact trusted comment
`product=maple-agent channel=stable version=<version>`. No expiry is claimed.

## Publication and authority

`agent-updates-publish.yml` is manual, trusted-master-only, and serialized with
operator pointer changes through `agent-updates-production`. It selects an exact
completed trusted-master `agent-desktop-build.yml` run, requires the successful
Prod producers and final download verifiers for every included platform (not
merely overall workflow success), verifies every final file's
GitHub attestation against master and the source commit, and validates build
profile, version, inventory and hashes before receiving signing/bucket secrets.
Unrelated Dev or omitted-Linux failures do not block a verified macOS-only
publication. Downloaded executable/package content is never executed in
credentialed jobs.

Publication uses create-only immutable objects, reuses identical bytes, resumes
missing objects and rejects differing bytes for a reserved version. The complete
signed manifest is created after payload verification. Only the latest pointer
is mutable, using an atomic compare-and-swap. A retry of the identical promoted
release verifies it as a no-op, including after a failed live verification.
Never delete a reserved release to make a conflicting retry pass. Publish a new
version or resolve the exact failure with the owner.

The owner must prepare and approve these inputs before activating delivery:

- A private existing Agent bucket, no `r2.dev`/bucket custom domain, retention
  and immutable-object overwrite/delete policy protecting release prefixes.
- Reuse the existing `desktop-signing` environment for macOS package signing
  and `updates-production` for Agent reservation, update signing and publication.
  Preserve their current protection policies and Research configuration. Update
  signing uses new Agent primary/offline backup keys; never Research's key.
- Bucket-scoped S3 Object Read & Write credentials, an approved account/bucket,
  public primary key and exact HTTPS origin. R2 credentials cannot deploy Workers.
- An existing Agent Worker and scoped Worker Editor deployment credential;
  confirm a pinned code deployment with the R2 binding preserves Routes. No
  Route or Custom Domain management is part of routine code deployment.
- Exactly `https://updates.trymaple.ai/agent/stable/*` and
  `https://updates.trymaple.ai/download/agent/stable/*`, separately owner-managed.
  Confirm zone/account/proxied DNS, HTTP redirects and edge allowlisting first.

Agent-specific workflow inputs are explicit. Reuse the
existing BWS access credential; no new machine accounts or access tokens are
required by this workflow:

| Scope                                           | Required input                                                                                                                                      |
| ----------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| Repository variables                            | `AGENT_UPDATES_ENABLED=true`, `AGENT_UPDATE_PUBLIC_KEY` (public minisign key)                                                                       |
| `updates-production` environment variables      | `AGENT_R2_ACCOUNT_ID`, `AGENT_R2_BUCKET`, `AGENT_UPDATE_SIGNING_KEY_ID`, `AGENT_R2_ACCESS_KEY_ID_SECRET_ID`, `AGENT_R2_SECRET_ACCESS_KEY_SECRET_ID` |
| `updates-production` environment secret         | `AGENT_UPDATES_BWS_ACCESS_TOKEN` (existing BWS access credential)                                                                                   |
| BWS items selected by the environment variables | Dedicated unencrypted Agent minisign key-file contents and Agent-bucket S3 access-key pair                                                          |

The pinned BWS action follows the existing PCR workflow: `set_env: false`, with
outputs passed only to the consuming step after the pinned tools are realized.
The sign step maps only the Agent update key; reservation/publication map only
the Agent bucket credentials. These mappings do not reduce the existing BWS
credential's grants or the GitHub environment's secret context. Shared access
therefore remains a trusted workflow boundary, not separate credential authority.

The public key must also be pinned in both the chosen build's Prod source profile
and the publisher's trusted master profile. An empty source key list fails closed;
setting workflow variables cannot replace those pins. Production verification uses
the fixed `https://updates.maple.ai` origin. No Worker deployment token belongs
in the Agent publication steps. Preserve the existing environment's policies;
the Agent workflow additionally enforces manual dispatch from trusted `master`.

New Agent packages compile the canonical `updates.maple.ai` feeds, and the
publisher requires those exact URLs in both the selected source and its trusted
profile. Keep the existing `updates.trymaple.ai` Agent routes active for installed
clients, alongside the separately owner-managed canonical hostname. Both hosts
must retain Agent delivery; Research remains on its existing hostname. Changing
these source pins does not deploy either hostname or change signed manifest
paths, keys, feed schemas or existing release bytes.

Before/after route activation compare Research's metadata bytes and all six
GET/HEAD download redirects, accounting for legitimate Research releases. In an
isolated rehearsal verify Research-style Worker redeploy/domain reattachment
preserves Agent routes; recheck after the next authorized Research publication.
Do not create a Research release for this check.

An unadvertised signed `0.0.1` to `0.0.2` rehearsal through real packages, feed,
keys and UI is required before invitations. Include Linux only after its own
packaged launch/update/relaunch gates pass. This service does not prove those
client gates or authorize a deploy/publication.
The separately authorized bucket rehearsal must also demonstrate identical
retries under its actual release-prefix lock policy, differing-byte refusal,
missing-object repair and conditional pointer-race rejection. Mock storage and
request-signature tests do not establish those live R2 properties.

Pointer rollback changes discovery, not installed apps. Install-time client
revalidation is required. Freeze/replay protection is limited in v1; a compromised
accepted key may require manual trusted reinstall. Keep the offline backup
separate and document key retirement/manual recovery before production signing.
