# Hosted native sign-in

The frontend builds two independent static sites. The existing `build` command
produces the Maple web app in `dist`. `build:auth` produces the hosted native
sign-in site in `dist-auth`, using `auth.html` and `src/auth-site/main.tsx`.
The auth entry does not load the web-app router, chat, billing, Agent Mode, or
the legacy V1 bridge.

## Routes and compatibility

- `/start` and the permanent `/desktop-auth` alias accept only `transport=v2`,
  a supported `provider`, and the native session and request IDs created by
  Maple. They do not accept an arbitrary return URL.
- `/auth/github/callback` and `/auth/google/callback` complete the pending
  browser flow and show the existing account confirmation before minting the
  native handoff grant. Callback errors keep the address intact and ask the
  user to restart sign-in in Maple. Maple Agent's paste flow uses the configured
  default callback on the web app, whose error page provides conditional
  paste guidance; it does not use the auth site.
- Apple uses its popup API with the existing Services ID
  `cloud.opensecret.maple.services`. It requires the auth domain and callback
  to be registered with Apple before live use. A static site cannot process
  Apple's form-post callback.
- `/complete` displays completion guidance. Other paths cannot start a login
  or fall through to the web app.

The SDK retains browser credentials on their current origin. Confirmation,
account ownership, pending-flow expiry, and native grant checks retain their
existing behavior. Completing or cancelling this flow does not sign the user
out of the web app or erase their browser credentials.

Browser OAuth initiation explicitly selects a callback on the initiating
origin. Before the callback-aware backend and this frontend are live together,
verify that each exact current-origin callback equals its provider's default
or an additional allowlist entry. Equivalent routes or trailing-slash redirects
do not satisfy exact membership. Apply the check to development and preview
origins used for rehearsal as well. The legacy V1 bridge remains part of the
web app and continues to use its existing default callback.

`VITE_AUTH_ORIGIN` selects the origin for native browser entry, using
`/desktop-auth` on that origin. It accepts an HTTPS origin, or exact loopback
HTTP in development. Its default and the current PR/release build profiles
remain `https://trymaple.ai`; building this change does not switch installed
clients to the auth subdomain.

## Build and local validation

From the repository root, use the pinned toolchain:

```sh
nix develop --no-update-lock-file .#ci -c bash scripts/ci/auth-web.sh
```

The default `pr` profile uses development services. Build/run commands ignore
local dotenv files; the pinned installer limitation is documented in
[Pages deployments](../../../docs/pages-deployments.md#independent-auth-site).
The script validates and archives the auth-only output. It does not publish
it or change OAuth settings. The CI shell pins Node because the TypeScript
and Vite command-line tools invoked by Bun use Node shebangs.

For a configured local development session, the frontend also exposes
`dev:auth` (loopback port 5174) and `preview:auth`. Preserve any externally
managed configuration and service ownership. Serve the built `dist-auth`
artifact when claiming artifact smoke evidence.

The frontend tests cover route validation, callback selection, popup and
handoff behavior. Real provider sign-in, native application opening, and live
response headers need separate runtime rehearsal before traffic is redirected.

## Independent publication

See [Pages deployments](../../../docs/pages-deployments.md) for the separate
auth artifact and publisher. Auth publication has its own manual trigger,
activation flag, environment, project, and production ref. An app release
does not publish the auth site. Provider registration, backend callback
allowlists, and traffic redirection are separate rollout steps.

The frontend pins published `@mapleai/sdk` 4.1.1 with a frozen registry lockfile.
Local SDK links remain supported during development; the production auth build
requires an exact published version and rejects local links.
