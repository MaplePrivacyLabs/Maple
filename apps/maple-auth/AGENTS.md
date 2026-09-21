# Maple auth app guide

Read the root guide and `$review-maple-security` for authentication changes.
This application is independent of Research: use its own package.json,
bun.lock, source, public assets, and config. Do not import sibling app files,
parent node_modules, or local SDK source. Consume the exact published SDK pin.
An auth-only change must not alter Research web authentication or client entry
URLs.

Use the root `.#ci` Nix shell (Bun and Node match CI). See [README.md](README.md)
for development, component checks and the two fixed build profiles. Run
`scripts/ci/auth-ci.sh` for format, lint, type checking, and tests. The root hook
routes this app to `.githooks/pre-commit`. Run the auth build when source,
configuration or dependencies change, and root `nix flake check` for workflow
or shared CI changes. Do not overwrite ignored environment files.

Preserve V2-only route parsing, same-origin OAuth callbacks, popup-only Apple,
SDK bootstrap ordering, pending target and account ownership checks, one mint
per confirmation, and the manual Open Maple link. Handoff completion clears
only its pending flow; it does not clear SDK credentials. Do not log provider
codes, state, tokens, handoff grants or credential-bearing URLs. Keep storage,
crypto and backend authority in the SDK and OpenSecret.

The build boundary must reject sibling app code, linked SDK source and the
legacy SDK. Preserve the unprivileged build and trusted independent publisher.
A merge or successful build does not authorize publication or redirects.
Report automated checks separately from real provider/native/browser testing.
