# Maple SDKs

This directory contains the TypeScript/React and Rust clients used by Maple and
internal OpenSecret applications. Both clients establish attested,
end-to-end encrypted sessions with an OpenSecret backend and expose the API
surface needed by those applications.

The developer/platform API remains part of the TypeScript SDK for internal
OpenSecret workflows. This repository does not maintain or deploy a separate
documentation website; keep behavior documentation close to the exported code
and tests.

## Repository layout

- `src/` — `@mapleai/sdk`, including the React providers, encrypted API
  client, attestation policy, model/conversation APIs, and internal developer
  platform client.
- `rust/` — the `maple-sdk` crate, imported as `maple_sdk` by native clients.
- `docs/PLATFORM.md` — internal developer/platform API notes.
- repository-root `.github/workflows/sdk-*.yml` — path-scoped TypeScript, Rust,
  and supply-chain validation for this directory.

Maple consumers prefer independently selected published SDK versions; local
TypeScript `file:` and Rust `path` dependencies remain available for active
development. The consumer's manifest and lockfile determine what it builds.
Research uses the Rust SDK on desktop and mobile for native authentication;
the embedded proxy remains desktop-only. See the
[consumer version policy](../docs/sdk-publishing.md#consumer-version-policy)
for switching sources and preparing client releases.

## Package identity migration

The package names are `@mapleai/sdk` and `maple-sdk`, first published at
TypeScript version 3.5.2 and Rust version 3.6.2. Their versions and publication
remain independent of Maple application releases.

The rename preserves the exported API, including `OpenSecretProvider`,
`useOpenSecret`, `OpenSecretDeveloper` and `OpenSecretClient`. OpenSecret remains
the backend name. Backend URLs, configuration variables, signed-PCR verification
and encrypted transport retain their existing contracts. Existing published
`@opensecret/react` and `opensecret` packages remain available to older consumers.

## Version 4 upgrade

Both SDKs use Transport V2 starting at `4.0.0`. Version 4 requires a backend
with Transport V2 support and does not fall back to V1. Applications
upgrading from V1 must establish a new session and ask existing users to sign in
again. Native authentication and hosted OAuth callbacks must use the matching
V2 flows; updating only one side of that handoff is insufficient.

The TypeScript SDK, Rust SDK, and Maple app are published independently.
Research selects `@mapleai/sdk` `4.0.0` from npm. Research's native clients,
Maple Agent, and the proxy resolve `maple-sdk` `4.0.0` from crates.io in their
lockfiles. Research and Agent still consume the in-tree proxy library.
Editing SDK source does not change these registry-pinned consumers; use local
links when validating an SDK change with an affected consumer. See the
[publishing guide](../docs/sdk-publishing.md).

## Security model

For non-local endpoints, both SDKs require HTTPS, verify AWS Nitro attestation,
and enforce an environment-scoped PCR0 trust policy before completing key
exchange. The SDKs bundle environment-specific PCR0 trust roots and the
verification key used to authenticate signed remote history entries.

Mock attestation is limited to exact loopback development endpoints (plus the
documented Android emulator alias in the Rust SDK). Do not weaken attestation,
PCR0 validation, or encrypted transport to accommodate a caller.

The SDKs use operating-system or Web Crypto randomness for keys, nonces, and
session material. Never substitute deterministic or convenience randomness in
production paths.

### Transport V2 session recovery

Managed SDK requests can resend the original logical operation once after a
fresh, verified attestation handshake, only on outer HTTP `400` with exactly
`x-opensecret-error-contract: 1` and `x-opensecret-error-code` equal to
`session_not_found` or `request_decryption_failed`. The backend marks only an
actually missing/expired session or failed AEAD authentication of the incoming
request, before application dispatch. The resend uses a new session and request
ID and applies to mutations and inference as well as reads.

These outer hints are unauthenticated. As with V1's best-effort recovery, an
intermediary can forge one after the original operation executed and cause a
duplicate under the new session. Per-session replay protection does not
guarantee cross-session at-most-once execution; use application-level
idempotency when needed.

Response decryption/framing failures, network failures, timeouts, partial
streams, redirects, generic `400`/`503`, and ordinary application errors do not
trigger automatic resend. Prepared native-handoff redemption and session-bound
OAuth callbacks require restarting their flows. There is no V1 fallback, and
credentials remain inside encrypted requests. Token refresh remains proactive
and coalesced; an authenticated expired-access response may refresh credentials
for future operations without resending the failed operation.

## TypeScript/React SDK

Install the selected SDK version:

```sh
bun add --exact @mapleai/sdk@4.0.0
```

Wrap the application with `OpenSecretProvider` and supply the backend URL and
client ID:

```tsx
import { OpenSecretProvider } from "@mapleai/sdk";
import type { ReactNode } from "react";

export function AppProviders({ children }: { children: ReactNode }) {
  return (
    <OpenSecretProvider
      apiUrl="https://api.example.com"
      clientId="00000000-0000-0000-0000-000000000000"
      pcrConfig={{ environment: "production" }}
    >
      {children}
    </OpenSecretProvider>
  );
}
```

Use `useOpenSecret` for authentication, encrypted application APIs,
conversations, inference, and account operations. Internal developer tooling
uses `OpenSecretDeveloper` and `useOpenSecretDeveloper`; preserve that surface
when changing the public exports.

### Development

Use the pinned Nix shell and Bun version. `bun.lock` is the supported dependency
lockfile; repository installation and updates use Bun. npm is used only to
publish the built tarball, so do not create an npm lockfile.

```sh
nix develop --no-update-lock-file
bun install --frozen-lockfile --ignore-scripts
bun run format:check
bun run build
```

For tests without a configured backend, use the credential-free selection in
the root `sdk-typescript.yml` workflow. An unfiltered `bun test` also collects
integration tests and requires the fixtures described below.

Integration tests read the variables documented in `.env.example`. Monorepo
[`sdk-integration.yml`](../.github/workflows/sdk-integration.yml) migrates
disposable PostgreSQL, starts the in-tree `services/opensecret/` backend from
the same checkout on loopback, and creates disposable SDK fixtures. It does
not depend on the hosted development service or stored test-account credentials.

Tests that spend model/provider capacity are opt-in with `RUN_LIVE_AI=1` and
are not part of the deterministic pull-request gate. Backend contract changes
and SDK changes are validated together against that checkout; released clients
and independently deployed backend versions still need compatibility review.
Both SDKs fetch their selected environment's signed PCR history from
`MaplePrivacyLabs/Maple/master/services/opensecret/`: `pcrProdHistory.json` for
production and `pcrDevHistory.json` for development. The existing verification
key, embedded roots, custom history URL overrides, and redirect rejection are
unchanged. Older published SDKs and installed clients still use the legacy
`OpenSecretCloud/opensecret` URLs, which remain a manual compatibility mirror.
Changing source defaults does not update those clients or publish an SDK. See
the [backend compatibility procedure](../services/opensecret/docs/pcr-compatibility.md).

Inspect the publishable npm artifact with:

```sh
bun run pack
```

Only `dist/` is included in the package.

Publishing runs in GitHub Actions. To dispatch validation of the committed
TypeScript version from `sdk/`:

```sh
just publish-npm 4.0.0
```

This defaults to a dry run. See the [SDK publishing guide](../docs/sdk-publishing.md)
for the protected publish action and the one-time registry setup.

## Rust SDK

Add the selected SDK version to a Rust application:

```toml
[dependencies]
maple-sdk = "=4.0.0"
```

Import the primary entry point with `use maple_sdk::OpenSecretClient`.
See `rust/README.md` for native
client examples and transport details.

Run the Rust validation from the `sdk/` directory:

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

Integration tests use the variables documented in `rust/.env.example` and are
separate from the default local validation path.

To dispatch validation of the committed Rust version from `sdk/`:

```sh
just publish-cargo 4.0.0
```

This defaults to a dry run. Both recipes only dispatch GitHub Actions; they do
not build or publish packages locally. Each SDK has its own workflow and
version, independent of Maple application releases. Follow the
[SDK publishing guide](../docs/sdk-publishing.md) to publish.

## Change discipline

- Keep the TypeScript and Rust attestation policies aligned intentionally;
  neither SDK's passing tests prove parity with the other.
- Treat API compatibility, authentication state, encrypted retry behavior, and
  PCR policy changes as security-sensitive.
- Update source comments and focused tests with behavior changes instead of
  regenerating a standalone documentation site.
- Validate the built npm package and Rust crate boundary before publishing a
  release.

## License

MIT
