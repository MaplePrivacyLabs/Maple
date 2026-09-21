# Maple hosted authentication

This standalone React/Vite application handles V2 native sign-in at the hosted
authentication origin. It owns its dependencies, lockfile, source, assets, tests,
build, and independent Pages publication. Its build does not import Research,
its configuration, or the in-tree SDK. The app consumes the published
`@mapleai/sdk` version pinned in its own manifest and lockfile.

Research keeps its existing browser authentication and legacy native bridge.
This application does not replace web login or change installed clients' entry
URLs. Initial migration traffic reaches it through a separately enabled V2-only
redirect. See the repository [Pages guide](../../docs/pages-deployments.md) for
build and publication controls.

## Routes and account state

- `/start` and the permanent `/desktop-auth` alias accept exactly `provider`,
  `transport=v2`, `native_session_id`, and `native_request_id`.
- `/auth/github/callback` and `/auth/google/callback` use the same-origin SDK
  pending state. OAuth initiation explicitly selects this origin's callback.
- Apple uses its popup flow. `/auth/apple/callback` only explains how to restart
  sign-in; it does not exchange a redirect callback or expose a copyable code.
- `/complete` presents completion guidance; other routes fail closed.

The SDK initializes retained credentials before a hosted flow starts. Native
handoff requires account confirmation and a single grant for the stored native
session and request. Target and account ownership are checked again after
asynchronous work. Cancellation, timeout, or a replacement flow prevents a late
grant from opening the app. The manual Open Maple link remains available after
a successful mint. SDK credentials remain on this origin; finishing a handoff
does not sign out another tab or the user.

## Develop and validate

From the repository root, enter the pinned toolchain and install this app only:

```sh
nix develop .#ci --no-update-lock-file
cd apps/maple-auth
bun install --frozen-lockfile
VITE_OPEN_SECRET_API_URL=https://enclave.secretgpt.ai \
  VITE_OPEN_SECRET_PCR_ENVIRONMENT=development bun --no-env-file run dev
```

The server listens on `127.0.0.1:5174`. Actual provider sign-in also requires
approved loopback callback entries and provider configuration. `VITE_*` values
are public build configuration; never put secrets in them. A developer may use
this app's ignored `.env.local`; managed CI builds ignore dotenv files without
modifying them.

Run the independent validation or build profiles from the repository root:

```sh
nix develop .#ci --no-update-lock-file -c bash scripts/ci/auth-ci.sh
MAPLE_AUTH_ENVIRONMENT=pr nix develop .#ci --no-update-lock-file -c bash scripts/ci/auth-web.sh
MAPLE_AUTH_ENVIRONMENT=release nix develop .#ci --no-update-lock-file -c bash scripts/ci/auth-web.sh
```

The package also exposes `format:check`, `lint`, `typecheck`, `test`, and `build`.
Tests cover route admission, pending handoff ownership and expiry, real SDK
bootstrap, retained sessions, provider UI, cancellation and manual open, and
build isolation. A build rejects modules outside this application (including
linked SDK source or sibling app imports) and the legacy SDK. Output goes to
`dist/`; the reproducible Pages archive and checksum go to
`target/reproducibility/`.

Builds and unit tests do not prove real provider, browser, native-client, or
production behavior. Publication, DNS/provider settings, redirect activation,
and rollback rehearsals are separate operations.

## Code ownership

The initial handoff confirmation, storage helpers, button styling, and public
OpenSecret configuration were copied from Research at
`2500c86589564e98b50c49d24ee05af72d0c51ae` to preserve their tested behavior
without changing Research. The auth copy omits client URL construction and
legacy transport routing. These files are now owned here. There is no live
source dependency between the applications: fixes to these copies, including
approved PCR fallback changes, need an explicit review for each consumer.
Assess lifecycle and security fixes for both copies; their behavior can diverge
where the applications have different requirements, without adding import
coupling or requiring byte-for-byte parity.
Encryption, OAuth callback fencing, credential storage, and handoff API calls
remain in the published SDK rather than duplicated protocol implementations.
