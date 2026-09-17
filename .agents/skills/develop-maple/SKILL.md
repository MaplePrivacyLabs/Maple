---
name: develop-maple
description: Develop and debug ordinary non-Agent-Mode Maple features and fixes across its React/Vite frontend and Tauri/Rust application. Use for local setup, choosing the Maple-versus-OpenSecret boundary, implementing UI, state, API-client, native command, desktop, or mobile changes, running focused tests, and preparing a reviewable handoff. Route comprehensive validation, security review, Agent Mode work, and releases to Maple's specialized sibling skills.
---

# Develop Maple

Work from the `MaplePrivacyLabs/Maple` repository root. Treat `justfile`, `apps/maple-research/frontend/package.json`, `flake.nix`, `scripts/ci/`, and `.github/workflows/` as the command sources of truth. Check them again when they disagree with prose documentation.

## Route Specialized Work

- Use `$validate-maple` for full validation, packaged-app smoke tests, cross-platform builds, or release-artifact verification.
- Use `$review-maple-security` for security audits or changes involving authentication, authorization, attestation, OAuth/deep links, credentials, storage, CSP, local proxy exposure, filesystem/shell access, or trust boundaries.
- Use `$change-maple-agent-mode` for Goose, Agent Mode runtime/UI, tool permissions, MCP, ACP, subagents, or agent persistence.
- Use `$develop-opensecret-sdk` for changes to the TypeScript/React or Rust SDK
  source under `sdk/`, including its backend integration revision or package
  boundaries.
- Use `$develop-maple-proxy` for changes to the proxy crate, standalone binary,
  container, or public OpenAI-compatible behavior under `proxy/`.
- Use `$release-maple` for version bumps, tags, publishing, release CI, or distribution. Never use `just release` during ordinary development.

Keep the current task scoped when one of these skills is unnecessary. Do not claim specialized validation that was not run.

## Establish a Safe Baseline

1. Inspect `git status --short --branch`, the current branch, and recent history.
2. Preserve all existing changes. Never reset, overwrite, or reformat unrelated work.
3. For newly requested isolated work, base it on current `origin/master` unless the user specifies another base. Do not switch branches over a dirty checkout.
4. Read root `AGENTS.md`, `apps/maple-research/AGENTS.md`, relevant docs, nearby implementation, tests, and current dependency/API versions before designing the change.
5. State the intended behavior and the smallest affected boundary before editing.

Do not commit, push, open a PR, merge, tag, publish, or release unless the user explicitly requests that action.

## Use the Pinned Development Environment

Prefer Nix so local tools match CI:

```bash
nix develop --no-update-lock-file
./setup-hooks.sh
just install
```

The flake pins Bun, Rust, and platform tooling. Use Bun for frontend dependencies; do not create npm, Yarn, or pnpm lockfiles.

Follow the [SDK consumer version policy](../../../docs/sdk-publishing.md#consumer-version-policy).
Prefer published pins; use local SDK links when developing across that boundary,
and update only the consumers intended to adopt a new SDK version.

Configure a local API without committing secrets:

```bash
test -e apps/maple-research/frontend/.env.local || cp apps/maple-research/frontend/.env.example apps/maple-research/frontend/.env.local
```

Set `VITE_OPEN_SECRET_API_URL` to the OpenSecret API under test. Set the billing or feature-flag API URLs only when exercising those integrations. Keep `.env.local` untracked, never put secrets in `VITE_*` values, and do not overwrite an existing developer configuration.

Use `nix develop --no-update-lock-file -c <command>` when an interactive shell is inconvenient. Use `nix develop --no-update-lock-file -c just clean-local` to clean this checkout; do not run raw `cargo clean` from the Nix shell because Cargo intermediates may be shared.

## Choose the Correct Boundary

Keep in Maple:

- React presentation, navigation, accessible interaction, view-local state, and client-side orchestration.
- Tauri commands, OS integration, application lifecycle, packaging, and explicitly local-only features.
- Client adaptation to an already-defined OpenSecret API contract.

Keep in OpenSecret:

- Authentication and authorization enforcement.
- Shared durable state and behavior that must agree across clients.
- Model/provider policy, confidential-compute enforcement, and public API semantics.
- Validation that must remain authoritative when a client is modified or bypassed.

Do not emulate missing server enforcement in Maple. If the work changes an API
contract, coordinate the open-source backend with the SDK source under `sdk/`,
preserve compatible failure behavior where practical, and test Maple against
the exact backend revision. Configure external billing and flags services
through their public API URLs; do not depend on their source trees.

## Follow the Existing Architecture

- Put React, routes, contexts, and browser-facing services under `apps/maple-research/frontend/src/`.
- Put privileged/native behavior under `apps/maple-research/frontend/src-tauri/src/` and expose the narrowest typed Tauri command or event needed by the UI.
- Use `@mapleai/sdk` and the existing OpenSecret client paths for authentication, encryption, and API calls. Read `apps/maple-research/frontend/package.json` to determine whether the application consumes a published version or the in-tree `file:../../../sdk` package; do not infer that boundary from prose. Do not duplicate protocol or cryptographic logic in components.
- Follow nearby state ownership, cancellation, error, and cleanup patterns. Preserve account isolation and handle logout, navigation, retries, and stale async completion explicitly.
- Treat all network, file, deep-link, shell, tool, and Tauri-command inputs as untrusted. Validate again at the enforcing boundary.
- Keep feature flags as rollout/UI controls, never authorization controls.
- Preserve accessibility semantics, keyboard behavior, focus handling, loading states, and actionable errors.
- Do not edit `apps/maple-research/frontend/src/routeTree.gen.ts` manually; let TanStack Router regenerate it.
- Treat generated mobile projects and platform patches as intentional source. Change them only when the platform behavior requires it and verify the relevant target.
- Update `apps/maple-research/frontend/bun.lock` or `apps/maple-research/frontend/src-tauri/Cargo.lock` with dependency
  changes. Explain new dependencies and avoid broad upgrades during an
  unrelated fix.
- Never log plaintext prompts, decrypted content, tokens, session keys, credentials, or sensitive file contents.

## Run the Development Loop

Choose the lightest runtime that exercises the changed boundary:

```bash
just dev          # browser UI at port 5173
just desktop-dev  # Tauri desktop app with ONNX Runtime provisioning
```

For iOS work, prepare the pinned runtime and select the target explicitly:

```bash
just ios-build-onnxruntime
just ios-dev-sim "iPhone 16 Pro"
# or: just ios-dev-device "Device Name"
```

Run direct `bun` and `bun tauri` commands from `apps/maple-research/frontend/`, not the repository root. Prefer `just desktop-dev` over raw `bun tauri dev` because the recipe provisions the platform ONNX Runtime.

During implementation:

1. Add or update the smallest behavior-focused test near the changed logic.
2. Reproduce failures deterministically; do not weaken assertions to make them pass.
3. Keep UI and Rust changes independently understandable when possible.
4. Inspect runtime logs for the exact app/build under test. Do not attach to another installed Maple build by display name alone.

## Run Focused Checks

For frontend changes:

```bash
cd apps/maple-research/frontend
bun --no-env-file test path/to/test.ts
bun run format:check
bun run lint
bun run typecheck
bun --no-env-file test
bun run build
```

For Rust/Tauri changes:

```bash
cd apps/maple-research/frontend/src-tauri
cargo fmt --check
./scripts/run-with-desktop-onnxruntime.sh cargo clippy --locked -- -D warnings
./scripts/run-with-desktop-onnxruntime.sh cargo test --locked --all-targets
```

There is no `just test` recipe. The pre-commit hook (`./setup-hooks.sh`) runs the Research format, ESLint, type-check, and unit-test lanes plus src-tauri format, Clippy, and tests when their files are staged, but it does not replace packaging checks or runtime smoke testing.

Before handoff, run `git diff --check`, inspect the complete diff and status, and invoke `$validate-maple` when the risk or requested proof exceeds these focused checks.

## Hand Off Precisely

Report:

- The behavior changed and why it belongs in Maple.
- The files and trust boundaries affected.
- Exact commands run and their results.
- Runtime/platform smoke evidence, clearly separated from automated tests.
- Any platform, API, signing, external-service, or credential limitation not exercised.
- Whether the tree remains uncommitted and unpushed.

Do not describe a web build as desktop proof, a unit test as end-to-end proof, or an unsigned/local artifact as a release artifact.
