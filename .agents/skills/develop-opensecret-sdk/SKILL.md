---
name: develop-opensecret-sdk
description: Develop and review the Maple TypeScript/React and Rust SDKs under Maple's sdk directory. Use for SDK API, authentication, attestation, encrypted transport, tests, in-tree OpenSecret integration, package contents, versions, or an explicitly authorized npm or crates.io publishing handoff; use develop-maple for application-only work.
---

# Develop the Maple SDK

Work from `MaplePrivacyLabs/Maple/sdk`. Read the repository-root `AGENTS.md`,
`sdk/README.md`, the affected implementation and tests, and the root
`.github/workflows/sdk-*.yml` files relevant to the change.

The SDK source is part of the Maple repository, but its TypeScript and Rust
package boundaries remain independently versioned and publishable:

- `src/` builds `@mapleai/sdk` for browser and React consumers.
- `rust/` builds the `maple-sdk` crate (imported as `maple_sdk`) for native consumers.
- `apps/maple-research/frontend/package.json` is authoritative for whether Maple's browser client
  consumes a published TypeScript version or the in-tree `file:../../../sdk` package.
- Research native clients, Maple Agent, and `proxy/` select their Rust SDK
  through their own manifests and lockfiles. Research iOS and Android use the
  Rust SDK for native OAuth; the embedded proxy and Goose remain desktop-only.

Follow the [consumer version policy](../../../docs/sdk-publishing.md#consumer-version-policy).
To ship an SDK change to clients, follow the
[rollout order](../../../docs/sdk-publishing.md#rolling-an-sdk-fix-out-to-clients):
SDK PR with version bump, publish, consumer pins, isolated app bump, release.
Prefer published pins without upgrading unrelated consumers. Local links are
allowed during active development, including on `master`; a registry-pinned
client build does not exercise an SDK source edit. Test an affected consumer
with the local SDK when that integration is part of the change.

Do not commit, push, open a PR, publish, or alter Maple's application dependency
wiring unless the user authorizes that action.

## Keep protocol ownership clear

OpenSecret owns authentication and authorization truth, public HTTP semantics,
provider policy, persistence, and usage accounting. The SDKs own client-side
attestation, encrypted sessions, typed contracts, authentication state, and
safe transport adaptation. Maple owns application presentation and local
device behavior.

For a public contract change, inspect `services/opensecret/` at the same
monorepo revision and both SDK implementations when they expose the affected
behavior. Preserve
old-client/new-server and new-client/old-server compatibility where clients can
update independently. Do not weaken HTTPS, PCR validation, attestation, key
exchange, randomness, retry safety, or sanitized errors to accommodate a
caller.

## Develop and validate

Use the SDK's pinned Nix shell. For TypeScript/React work:

```sh
nix develop --no-update-lock-file -c bun install --frozen-lockfile --ignore-scripts
nix develop --no-update-lock-file -c bun run format:check
nix develop --no-update-lock-file -c bun run build
```

For tests without service fixtures, use the credential-free selection in
`.github/workflows/sdk-typescript.yml`. An unfiltered `bun test` also collects
integration tests that require a configured backend and test identities; use
`sdk-integration.yml` for that complete local-stack setup.

For Rust work:

```sh
nix develop --no-update-lock-file -c bash -lc '
  set -euo pipefail
  cd rust
  cargo fmt --all -- --check
  cargo clippy --locked --all-targets --all-features -- -D warnings
  cargo test --locked --all-features --lib
  cargo doc --locked --no-deps --all-features
'
```

Run focused tests while iterating, then match the root path-scoped workflows:
`sdk-typescript.yml`, `sdk-rust.yml`, and `sdk-supply-chain.yml` as applicable.
The repository pre-commit hook runs `sdk/.githooks/pre-commit` in this shell
when SDK files are staged: the Rust format, Clippy, `--lib` tests, and docs,
and the TypeScript format, build, and top-level unit tests. It never runs the
integration suites or cargo-deny.
When the change reaches backend behavior, authentication, or the encrypted wire
contract, also match `sdk-integration.yml`. It starts disposable PostgreSQL and
the in-tree `services/opensecret/` backend from the same checkout, then tests
both SDK implementations without a hosted development server. Inspect backend
changes and run its component validation when the compatibility contract reaches
them; there is no separate integration revision to advance.

Provider-spending tests remain opt-in through `RUN_LIVE_AI=1` and require
explicit credential, egress, and cost authorization. Both SDKs use signed-PCR
histories under `MaplePrivacyLabs/Maple/master/services/opensecret/`, with the
existing verification key and separate development/production policies. Keep
the legacy `OpenSecretCloud/opensecret` histories available for older clients
through the backend's manual compatibility procedure. Source changes do not
publish SDKs or update installed clients.

Before handoff, inspect package boundaries as applicable:

```sh
bun run pack
cargo package --locked --manifest-path rust/Cargo.toml
```

These commands validate package contents; they do not publish them. For Rust
SDK changes, also run the root `scripts/ci/verify-local-rust-deps.sh` check and
the applicable desktop/proxy validation. Check its selected SDK source before
claiming Maple consumes the result.

## Publishing boundary

SDK publishing is separate from the Maple application release workflow.
Follow `docs/sdk-publishing.md`. Use the separate manual
`sdk-publish-npm.yml` and `sdk-publish-rust.yml` workflows on protected `master`;
each publishes the stable version already committed for that SDK. Neither
workflow creates GitHub Releases or tags, or changes the Maple application
release version.

`just publish-npm VERSION` and `just publish-cargo VERSION` dispatch validation
in GitHub Actions with `mode=trusted` and `dry_run=true`. They do not publish
locally. Initial publication also runs in Actions, using the guide's one-time
bootstrap procedure. Normal publication uses registry trusted publishing and
the protected `sdk-npm` or `sdk-crates` environment.

Setting `dry_run=false` authorizes an external production mutation; do that
only with explicit authority for the exact package, version, registry, and
source commit. Review the workflow's validated package and source commit before
approving its environment. Report the immutable registry result and the run URL.
Never use local `npm publish` or `cargo publish`, and never create a Maple
GitHub Release merely to publish an SDK.

## Report

State which SDK changed, the backend/API compatibility boundary, exact commands
and results, package inspection performed, the Maple dependency pins left
unchanged or deliberately updated, and every client, platform, live provider,
or publishing boundary not exercised.
