# Maple auth app guide

Read the root guide and `$review-maple-security` for authentication changes.
This application is independent of Research: use its own package.json,
bun.lock, source, public assets, and config. Do not import sibling app files,
parent node_modules, or local SDK source. Consume the exact published SDK pin.
An auth-only change must not alter Research web authentication or client entry
URLs.

Use the root `.#ci` Nix shell (Bun and Node match CI). See [README.md](README.md)
for development, component checks and the fixed Dev/Prod build profiles. Run
`scripts/ci/auth-ci.sh` for format, lint, type checking, and tests. The root hook
routes this app to `.githooks/pre-commit`. Run the auth build when source,
configuration or dependencies change, and root `nix flake check` for workflow
or shared CI changes. Do not overwrite ignored environment files.

The required `VITE_AUTH_ENVIRONMENT` selector fixes the backend, project and PCR
trust. Dev requires `native_app_variant=dev` and returns only through
`cloud.opensecret.maple.dev`; Prod retains the four-parameter request and
`cloud.opensecret.maple` return. Bind that identity to pending state and recheck
it before mint/open. Do not accept arbitrary URLs, schemes, or query-selected
backend configuration. Agent uses a separate `/agent/start` contract with a
canonical port and random return state, fixed `127.0.0.1` callback path, and
compiled environment label. Bind all target fields through callback and mint.
Require a second user click for loopback return and enforce the issuer's grant
expiry. Research's automatic return remains unchanged.

Preserve V2-only route parsing, same-origin OAuth callbacks, popup-only Apple,
SDK bootstrap ordering, pending target and account ownership checks, one mint
per confirmation, and the manual Open Maple link. Handoff completion clears
only its pending flow; it does not clear SDK credentials. Do not log provider
codes, state, tokens, handoff grants or credential-bearing URLs. Keep storage,
crypto and backend authority in the SDK and OpenSecret.

The build boundary must reject sibling app code, linked SDK source and the
legacy SDK. Preserve the unprivileged build and trusted independent publisher.
Once configured, relevant master builds and internal PR builds automatically
publish Dev and previews through trusted master tooling. Production publication
remains manually dispatched and independent of client releases; no workflow here
authorizes a production redirect change. Preview hosting does not configure OAuth
callbacks; use the stable Dev origin for provider rehearsal.
Report automated checks separately from real provider/native/browser testing.
