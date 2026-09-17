# OpenSecret backend import

OpenSecret lives in `services/opensecret/`, retaining its Rust package, Nix
toolchain, operator recipes, submodule revisions, and PCR filenames. This is a
source and development-workflow migration. Approval, signing, publication, and
deployment remain operator-controlled. Read-only EIF/PCR comparisons now also
run in CI under the [approval-check policy](../services/opensecret/docs/nitro-deploy.md#ci-approval-checks).

## Preserved source boundary

The source is `OpenSecretCloud/opensecret` master at
`22809e0b7e12f4c238cf9e116d65348c7c2826fb`, captured September 8, 2026.
It contains 905 reachable commits.

The unmodified import commit is
`5aa24fc9dd29e20c10c4f28a055fda5c07257ef7`. Its first parent is Maple master
`86757e9248e6bb46adc388bfac3c881f9d7069df`; its second parent is the source
commit above. The imported subtree exactly equals the original root tree,
`89bc0ccc1398aea4f4ece3f373a4a96d2cfd8019`. Monorepo adaptations follow this
separate commit.

The import merge is retained in Maple's ancestry. Its separate source parent
preserves upstream history rather than flattening it into a squash commit.

Only source master is imported. Existing OpenSecret pull requests and branches
remain in the old repository for separate review and replay under the new
prefix; this import does not merge or close them.

The root `.gitmodules` registers the unchanged backend gitlinks:

| Component path | Imported revision |
| --- | --- |
| `services/opensecret/nitro-toolkit` | `dcfea5f66c3f0aea232b649da2ce3661be54cc14` |
| `services/opensecret/privatemode-public` | `4b72dcbbd58940835b5ba32502c1e247721b9584` |

Initialize them with `git submodule update --init --recursive`. Backend Cargo,
Nix, and existing `just` recipes run from `services/opensecret/`. Its flake and
lockfile stay separate from the client build environments. Root backend
shortcuts are `just opensecret-check` and `just opensecret-pcr-check`.

## CI and local development

Root workflows replace the backend's inactive nested workflows. Backend CI
checks Rust, the ordinary Nix package/checks, and dependency policy.
The SDK integration lane starts the backend from this same checkout against
a disposable database; it no longer fetches an external backend revision.
This advances that lane from the former pinned commit
`d26eb6bd54d50cc8e6b2967f647a94c61da913da` to the imported source.

PR jobs have read-only repository contents permission and use hosted runners,
without signing/deployment credentials. Ordinary backend CI has no EIF
publisher or deployment step. Signed-PCR validation remains available through
the lightweight monorepo-root `pcr-compatibility` Nix check and the manual
publication procedure, not a standalone backend dev-shell job. A separate
ARM64 EIF workflow compares dev/prod measurements on PRs explicitly editing
approved PCR JSON, relevant backend/TEE or approval changes to master, and manual runs.
Ordinary backend PRs do not fail merely because approvals have not been
updated; master mismatches are an intentional deployment-approval signal.
Master and same-repository PR EIF checks receive OIDC for FlakeHub Cache and
also populate GitHub's branch-scoped Nix cache. Fork PRs and non-master manual
runs have no OIDC and use only GitHub caching; master runs warm the
default-branch cache for forks. Legacy cross-organization cache access does not
transfer automatically; follow the
[cache validation policy](../services/opensecret/docs/nitro-deploy.md#binary-caches-and-cold-run-validation).
Backend-only changes do not select Research or Agent application packaging.

An external workspace manager may own local backend selection, generated
configuration, and process lifecycle. Follow that environment's recorded paths
and lifecycle commands; do not move its generated configuration by hand.
The backend component is `services/opensecret/`.
See the [component guide](../services/opensecret/AGENTS.md)
and root `$develop-opensecret` / `$validate-opensecret` skills.

## Signed PCR compatibility and cutover

At the import boundary, the four files were byte-for-byte unchanged:
`pcrDev.json`, `pcrDevHistory.json`, `pcrProd.json`, and `pcrProdHistory.json`.
Each captured history contains 145 entries. All 290 captured PCR0 signatures verify
against the public key pinned by both SDKs. PCR1/PCR2 are present in the JSON
but are not covered by those signatures; this import preserves that format.

Keep `OpenSecretCloud/opensecret` at its current name, public, writable, and
unarchived. Installed clients still request its raw history URLs. Its current
default branch is a compatibility mirror; backend implementation lives here.
Historical source, issues, and branches do not create a second active backend.

Current TypeScript and Rust SDK defaults read the canonical histories under
`MaplePrivacyLabs/Maple/master/services/opensecret/`. The SDK package names are
`@mapleai/sdk` and `maple-sdk`, with independent protected
[publishing workflows](sdk-publishing.md). Publishing a package does not upgrade
its consumers or retire the URLs used by older clients.

For authorized backend releases, use the
[manual PCR compatibility procedure](../services/opensecret/docs/pcr-compatibility.md):
commit the reviewed signed files in Maple, prepare an exact-byte copy into the
legacy repository, and verify both published locations before deploying an EIF
that needs them. Preserve the signed-PCR format, pinned public key, and redirect
policy. Operator builds run from `services/opensecret/`; verify the selected
checkout and commit on each deployment host rather than assuming a merged
source change migrated that host.

The separate `OpenSecret EIF release` workflow builds and attests a candidate
and can sign new PCR entries behind its protected approval environment. It
pushes an approval branch for review; it creates no GitHub Release or tag and
does not deploy. Ordinary CI remains read-only with respect to approvals.
The copy helper does not commit or push. The signing-key transition and legacy
compatibility sunset remain separate decisions; legacy files do not expire
automatically.
