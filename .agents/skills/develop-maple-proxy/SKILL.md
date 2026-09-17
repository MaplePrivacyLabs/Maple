---
name: develop-maple-proxy
description: Develop and review the maple-proxy Rust crate, binary, container, and OpenAI-compatible HTTP behavior under Maple's proxy directory. Use for proxy APIs, configuration, authentication, CORS, attested OpenSecret transport, tests, container builds, package versions, or an explicitly authorized crates.io or GHCR publishing handoff; use develop-maple for the Tauri lifecycle wrapper alone.
---

# Develop Maple Proxy

Work from the `MaplePrivacyLabs/Maple` repository root; the component source is
under `proxy/`. Read the repository-root `AGENTS.md`, `proxy/README.md`,
affected source and tests, and the root
`.github/workflows/proxy-*.yml` files relevant to the change.

The source is part of Maple but keeps distinct public package and runtime
boundaries:

- `proxy/` builds the `maple-proxy` crate and binary.
- `proxy/Cargo.toml` uses a compatible `maple-sdk` registry requirement;
  `proxy/Cargo.lock` selects the standalone binary's version. Local SDK links
  are also supported during active development.
- Research desktop and Maple Agent consume the in-tree proxy library and
  choose their SDK versions independently in their own Cargo manifests/locks.
  iOS and Android do not compile Research's proxy.
- `apps/maple-research/frontend/src-tauri/src/proxy.rs` owns Maple's account-scoped listener,
  configuration, key storage, and lifecycle around the library. Do not move
  that application behavior into the reusable crate incidentally.

Follow the [SDK consumer version policy](../../../docs/sdk-publishing.md#consumer-version-policy).
Keep the embedded proxy and its host on the same SDK source/version; avoid
exact-pinning the reusable library in a way that forces all hosts to upgrade.

Do not commit, push, open a PR, publish, tag, release, or change live
infrastructure unless the user authorizes that action.

## Keep validation and routing aligned

Run the credential-free proxy checks through its pinned shell:

```sh
nix develop --no-update-lock-file ./proxy -c bash -lc '
  set -euo pipefail
  cd proxy
  cargo fmt --all -- --check
  cargo clippy --locked --all-targets --all-features -- -D warnings
  cargo test --locked --all-features
  RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
  cargo machete
'
```

The repository pre-commit hook runs exactly these commands through
`proxy/.githooks/pre-commit` in the proxy shell when proxy files are staged.

For Rust SDK or dependency-wiring changes, also prove Research resolves one
SDK from its selected source and the in-tree proxy:

```sh
nix develop --no-update-lock-file .#ci -c \
  ./scripts/ci/verify-local-rust-deps.sh
```

Root workflows own proxy Rust, daily supply-chain, non-publishing container,
and native-release rehearsal checks. `proxy/src/**`, `proxy/Cargo.toml`, and
unknown proxy build inputs are desktop application inputs; tests, examples,
docs, the standalone lockfile, and container-only files do not by themselves
route expensive Maple app builds. `sdk/rust` runtime changes conservatively
select proxy checks and desktop Maple even with a published SDK pin; verify
the selected source when testing an SDK edit. When a new input changes either
graph, update
`scripts/ci/change_detection.py`, its table-driven tests, and workflow paths in
the same change.

## Exercise the right runtime

Run the standalone proxy on a checkout-specific loopback port with an explicit
backend and PCR0 environment. Keep API keys in ignored local configuration or
the invoking process; never print or commit them. Exercise `/health`,
`/v1/models`, streaming and non-streaming chat, embeddings, invalid
authentication, timeout, and cancellation only as relevant to the change.

Container builds require the Maple repository root as context because the
Dockerfile copies both `proxy/` and `sdk/rust`:

```sh
docker build -f proxy/Dockerfile -t maple-proxy:dev .
```

Use the configured container runtime from `proxy/justfile` when Docker is not
the intended local engine. A successful image build is not proof that GHCR was
published or that the service works against a live enclave.

For authentication, CORS, bind exposure, saved-key behavior, backend URL or
PCR0 selection, request forwarding, logging, or timeout changes, load
`$review-maple-security`. Preserve the core loopback and CORS-off defaults;
treat container CORS-on configuration as a separate exposed mode. A configured
default API key is for private/originless clients; browser-facing CORS mode must
require each request's bearer key rather than spending the saved default. Never
assume protections in the Tauri wrapper also exist in the standalone router.
Never log any portion of an API key. Treat raw OpenAI request and response
bodies as untrusted and potentially sensitive.

## Preserve publishing boundaries

Maple's GitHub Release workflow builds, checksums, attests, uploads, and
re-verifies four native proxy archives. Maple v3.3.9 proved this integrated
publication path for macOS arm64, Linux arm64, Linux x86_64, and Windows
x86_64. Never create a proxy GitHub tag or Release; a proxy-only binary fix
ships through a normal Maple patch release.

Crates.io publishing remains separately versioned and manual. On an authorized
publish, inspect the exact package first:

```sh
cargo package --locked --manifest-path proxy/Cargo.toml
```

If the proxy references a new `maple-sdk` version, publish that SDK crate
first. Do not publish either crate from Maple's application Release workflow.
Root proxy container CI builds without pushing. After a successful stable Maple
Release, `.github/workflows/proxy-publish.yml` independently compares that
release's proxy version with the previous stable Maple Release. Unchanged
versions skip, including the unbackfilled 0.3.3 baseline. A strictly newer,
previously unpublished version automatically publishes Linux AMD64/ARM64 to
`ghcr.io/mapleprivacylabs/maple-proxy` with exact, minor, major, and `latest`
tags. The workflow is serialized, rejects rollback and stale releases, verifies
the public manifest, and supports manual retry from `master`. The transferred Maple repository uses its `GITHUB_TOKEN` to publish in
`MaplePrivacyLabs`; the new package must be public and grant Maple Actions write
access. Existing old-namespace images remain available but receive no updates. Treat any namespace, trigger, version policy, or package-access change
as a separate production-authority decision.

## Report

State the proxy behavior and public contract changed, SDK/application boundary,
exact checks and runtime evidence, container or package inspection performed,
version/publisher state left unchanged or deliberately updated, and every
platform, live backend, registry, or release boundary not exercised.
