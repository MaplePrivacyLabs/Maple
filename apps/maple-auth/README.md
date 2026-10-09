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
  `transport=v2`, `native_session_id`, and `native_request_id` on Prod. Dev
  additionally requires `native_app_variant=dev`. Prod rejects that parameter;
  Dev rejects the Prod request form. Neither accepts a caller-selected backend,
  scheme, return URL, or project.
- `/agent/start` accepts the four V2 fields plus `return_port` and `return_state`
  for the fixed loopback contract below. It never accepts `native_app_variant`.
- `/auth/github/callback` and `/auth/google/callback` use the same-origin SDK
  pending state. OAuth initiation explicitly selects this origin's callback.
- Apple uses its popup flow. `/auth/apple/callback` only explains how to restart
  sign-in; it does not exchange a redirect callback or expose a copyable code.
- `/complete` presents completion guidance; other routes fail closed.

`VITE_AUTH_ENVIRONMENT` is a required build selector, not a request parameter:

| Selector      | Backend / PCR trust                          | Native return                                       |
| ------------- | -------------------------------------------- | --------------------------------------------------- |
| `production`  | `https://enclave.trymaple.ai` / production   | `cloud.opensecret.maple://auth?handoff_grant=…`     |
| `development` | `https://enclave.secretgpt.ai` / development | `cloud.opensecret.maple.dev://auth?handoff_grant=…` |

Both profiles use the fixed public Maple project ID and published SDK pin.
The selector chooses backend, PCR environment, and native identity together;
independent API/project/PCR overrides are not supported. Vite rejects a missing
or unknown selector. Dev and Prod use separate hosted origins (`auth-dev.maple.ai`
and `auth.maple.ai`) and independently published artifacts. This foundation
adds the Dev hosted contract; native client adoption is separate work.

The SDK initializes retained credentials before a hosted flow starts. Native
handoff requires account confirmation and a single grant for the stored native
session and request. The fixed native identity is stored with the target and
revalidated on callback and mint. Target and account ownership are checked
again after asynchronous work. Cancellation, timeout, or a replacement flow
prevents a late grant from opening the app. The manual Open Maple link remains available after
a successful mint. SDK credentials remain on this origin; finishing a handoff
does not sign out another tab or the user.

## Agent return contract

Agent opens `/agent/start?transport=v2&provider=...&native_session_id=...&native_request_id=...&return_port=...&return_state=...`.
Each field occurs exactly once. Session/request IDs and `return_state` are
32 lowercase hex characters; the native client generates a fresh random
16-byte return state and binds its IPv4 loopback listener before opening Auth.
`return_port` is canonical decimal from 1 through 65535. No hostname, path,
URL, app variant, or environment may be supplied in the request.

Auth keeps the compiled environment, Agent identity, port and state in the
pending target alongside the provider/session/request pair. These fields must
still match after provider callbacks and minting. Account confirmation mints
once and shows **Return to Maple Agent Dev** (Dev) or **Return to Maple Agent**
(Prod). Agent return requires that separate button click; Auth never navigates
to loopback automatically. The top-level destination is exactly:

```text
http://127.0.0.1:<port>/auth/callback?handoff_grant=<grant>&return_state=<state>
```

The URL stays in component memory and expires at the earlier of the SDK's
`expires_at` (Unix seconds) and the pending attempt's deadline. Manual return
rechecks account ownership, credential revision, expiry, and replacement flows.
The native listener validates its state, redeems the grant through the reserved
native session/request, and publishes the authenticated account. Credentials
remain stored on Auth. Research keeps its automatic return and manual fallback.
Auth supports the Agent route in both profiles; native Agent Prod adoption is
separately disabled until its production configuration is approved.

## Develop and validate

From the repository root, enter the pinned toolchain and install this app only:

```sh
nix develop .#ci --no-update-lock-file
cd apps/maple-auth
bun install --frozen-lockfile
VITE_AUTH_ENVIRONMENT=development bun --no-env-file run dev
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
MAPLE_AUTH_ENVIRONMENT=dev nix develop .#ci --no-update-lock-file -c bash scripts/ci/auth-web.sh
MAPLE_AUTH_ENVIRONMENT=release nix develop .#ci --no-update-lock-file -c bash scripts/ci/auth-web.sh
```

`pr` and `dev` both compile the Dev environment; `release` compiles Prod.
Once Dev publication is configured, relevant `master` pushes automatically
test, build and publish to `auth-dev.maple.ai`. Internal PRs, including stacked
PRs, get their own Dev-configured Pages previews and a PR link. Forks get CI
without hosted previews. Production `auth.maple.ai` keeps separate, manually
dispatched build and publish workflows, independent of client releases.
See [Pages deployment architecture](../../docs/pages-deployments.md#independent-auth-site).

Preview URLs do not acquire OAuth provider or backend callback registrations.
Use stable `auth-dev.maple.ai` for real provider and native-return rehearsal;
preview publication itself proves neither callback eligibility nor OAuth success.

The package also exposes `format:check`, `lint`, `typecheck`, `test`, and `build`.
Tests cover both profiles: route and target admission, pending handoff ownership
and expiry, real SDK bootstrap, retained sessions, provider UI, cancellation,
manual open, and build isolation. Prod request and return bytes stay compatible
with installed clients; Dev grants open only the fixed Research Dev scheme.
A build rejects modules outside this application (including linked SDK source
or sibling app imports) and the legacy SDK. Output goes to
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
