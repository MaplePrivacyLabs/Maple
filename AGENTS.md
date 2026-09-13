# Maple monorepo agent guide

This file applies to the entire repository. Read the component guide and the
matching skill under `.agents/skills/` before changing that component. The
current source and tests take precedence over historical design documents.

## Repository map

- `apps/maple-research/`: the existing React/Vite/Tauri Maple application,
  including desktop Agent Mode. Read its [guide](apps/maple-research/AGENTS.md)
  for runtime placement, native security, and exact-app validation.
- `apps/maple-agent/`: GPUI desktop-v2 prototype, ACP and proxy CLI. Read its
  [guide](apps/maple-agent/AGENTS.md) and `$develop-maple-agent`. Its runtime and
  update discovery are separate from Research and its existing Agent Mode.
- `sdk/`: TypeScript/React and Rust Maple SDKs for the OpenSecret backend. Read
  `$develop-opensecret-sdk` and the SDK documentation.
- `proxy/`: the separate OpenAI-compatible relay, also consumed by the desktop
  app. Read `$develop-maple-proxy` and the proxy documentation.
- `services/updates/`: the updater Worker. Preserve its deployed identity,
  public endpoints, installed-client compatibility, and verified metadata flow.
- `services/opensecret/`: the OpenSecret Rust backend, including its pinned
  toolchain, local operator recipes, and signed PCR files. Read its
  [guide](services/opensecret/AGENTS.md) and `$develop-opensecret`.
- `docs/`, `scripts/`, `.github/`, `.agents/`, `flake.nix`, `flake.lock`,
  `justfile`, and `repo.meta.json`: shared documentation, tooling, CI, and
  repository identity. Keep active workflows and discoverable skills at root.

For SDK consumption, follow the [consumer version policy](docs/sdk-publishing.md#consumer-version-policy):
prefer independently pinned published versions, allow local links during active
development, and review the actual SDK source before a client release. SDK
publication and upgrading a consumer are separate decisions.

The backend import does not change TEE deployment or introduce EIF publication
through GitHub Actions. Preserve the manual signed-PCR compatibility procedure
in [the backend guide](services/opensecret/docs/pcr-compatibility.md) and the
existing `OpenSecretCloud/opensecret` raw URLs used by installed clients.

## Start safely

1. Confirm the checkout, branch, and worktree state. Preserve unrelated user
   changes and do not switch branches or rewrite history in a dirty checkout.
2. Read relevant source and tests before proposing placement. Prefer the
   narrowest existing component; write down cross-component API contracts
   before changing them.
3. Before creating configuration or starting or stopping services, determine
   whether an external development environment owns ignored environment files,
   the root `.local/tauri-workspace.json` overlay, ports, or processes. Preserve
   those resources and follow that environment's lifecycle instructions.
4. Use the versions and platform dependencies pinned by the owning Nix flake.
   Run shared app recipes from the repository root; enter component directories
   only where their command instructions require it. Do not add substitute
   global toolchain bootstraps to docs or CI.
5. Use `just clean-local` for the Research checkout. Raw `cargo clean` may erase
   a shared Nix Cargo build directory used by other checkouts. Follow the
   owning component's cleanup contract for other components.

## Shared security and compatibility

- Keep authentication, authorization, encrypted persistence, provider
  credentials/routing, model policy, and inference-usage truth in OpenSecret.
  Client UI, feature flags, and billing presentation are not authority.
- Never log access/refresh tokens, API keys, plaintext prompts/responses,
  decrypted payloads, or credential-bearing headers/environments. Sanitize
  errors before exposing them across a trust boundary.
- Treat network, renderer, model, file, deep-link, and tool input as untrusted.
  Validate at the boundary that owns the privileged effect. Preserve account
  isolation, cancellation, lifecycle ownership, and fail-closed permissions.
- Keep secrets out of `VITE_*`; these are public build-time values. Preserve
  local ignored configuration and do not overwrite it with defaults.
- Preserve shipped application identity, signing/updater contracts, public
  APIs, and protocol compatibility unless the user authorizes the change.
  TypeScript and Rust SDK transports, retry behavior, and API coverage differ;
  verify every affected client path when a backend contract changes.
- Follow nearest code, error, accessibility, and test patterns. Avoid unrelated
  dependency upgrades, rewrites, and generated-file edits. Use the appropriate
  generators and inspect their deltas; never hide them with Git index flags.

Load `$review-maple-security` for auth, proxy, Agent tooling, native
capabilities, filesystem/process access, deep links, or persistence work. Keep
findings in the task review or another authorized destination; keep durable
standards and methodology in guides and skills.

## Validation and publication authority

Start focused, then run complete gates for the layers changed. The app's
[validation guide](apps/maple-research/AGENTS.md#validation-is-proportional-evidence)
and `$validate-maple` describe the change-to-evidence matrix. Component checks,
unit tests, web/native packages, exact-app runtime smoke, and live deployment
are separate evidence. Do not claim one proves another.

Run `nix flake check --no-update-lock-file` for flake, workflow, CI-script, or
release-configuration changes, plus the affected component checks. The
pre-commit hook is useful but does not establish full CI parity.

`scripts/ci/change_detection.py` routes expensive app packaging. It conservatively
selects Research frontend builds for TypeScript SDK runtime inputs and desktop
builds for Rust SDK and proxy runtime inputs, including when a consumer uses a
published SDK pin. Tests, docs, container-only inputs, and standalone
component lockfiles retain their independent lanes. Update the classifier and
its table-driven tests when the dependency graph or component layout changes.
The backend has its own root `opensecret-ci.yml` workflow and change selector;
`sdk-integration.yml` tests both SDKs against `services/opensecret/` from the
same checkout. Backend changes do not imply Research or Agent packaging.
The separate `opensecret-eif.yml` compares dev/prod EIF measurements only on PRs
editing approved PCR JSON, relevant master changes, and manual runs. Preserve
ordinary backend PRs without fresh approvals and meaningful master mismatches;
these checks never change approvals, sign, release, or authorize deployment.
Master and same-repository PR EIF checks receive OIDC for FlakeHub caching.
Fork PRs and non-master manual runs use GitHub's branch-scoped cache without
OIDC. Preserve that head-repository boundary and verify cache changes on fresh
hosted runners, not just a warm local Nix store. See the [cache policy](services/opensecret/docs/nitro-deploy.md#binary-caches-and-cold-run-validation).

For Pages, read [the deployment guide](docs/pages-deployments.md). Preserve
unprivileged preview builds and separate development/production profiles.
Credential-bearing publication executes trusted master. Publishing flags,
protected environments, and native Cloudflare build controls are operator
configuration, not consequences of merging source.

Routine development never authorizes releases, signing, store submission,
deployment, or live-service changes. Never push to `master` as a validation
step: app inputs start production-shaped signed builds and can upload iOS
artifacts to TestFlight. Creating a GitHub Release starts release builds and
downstream publication. Use `$release-maple` only for explicitly requested
release work, and report the tag and commit before publishing.

## Skills

- `$develop-maple`: Research setup and ordinary web/desktop/mobile development.
- `$develop-maple-agent`: GPUI Agent app, runtime, component CI and isolated launch.
- `$develop-opensecret-sdk`: SDK implementation, backend compatibility,
  package-boundary validation, and publishing handoff.
- `$develop-opensecret`: backend setup, local stack, migrations, and ownership.
- `$change-opensecret-api`: backend HTTP and encrypted client contracts.
- `$change-opensecret-provider`: provider routing, transport, and usage.
- `$validate-opensecret`: backend Rust, database, client, and artifact evidence.
- `$review-opensecret-security`: backend trust boundaries and evidence claims.
- `$develop-maple-proxy`: proxy behavior, native/container builds, app dependency
  boundaries, and publishing handoff.
- `$validate-maple`: CI parity, exact-app/full-stack smoke, and evidence reporting.
- `$change-maple-agent-mode`: the shipped Tauri app's Agent Mode, ACP, Goose,
  permissions, tools, MCP, trust, cancellation, and lifecycle work.
- `$review-maple-security`: security design, implementation, and review.
- `$release-maple`: version preparation, preflight, and authorized publication.

## Maintaining this guidance

Treat guides and skills as living operational documentation, not infallible
rules. Re-check prescriptive language against current source, tooling, and
architecture. If guidance appears stale, materially wrong, unnecessarily
absolute, or repeatedly creates friction, surface the mismatch and confirm the
intended correction with the user before changing it. Avoid stylistic churn;
narrow claims that exceed the actual invariant.

Update relevant guides and skills in the same branch when an authorized change
alters workflows, ownership, validation, or recurring development procedures.
Keep unrelated drift scoped to a separately proposed change. Prefer improving
an existing skill over adding a redundant one. Ground guidance in current
source or executed workflow experience and validate prescribed commands/paths.
