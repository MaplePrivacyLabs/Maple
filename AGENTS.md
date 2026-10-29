# Maple monorepo agent guide

Use current source and tests over historical plans. Read the owning component
guide and load the skill matching the requested work; keep unrelated workflows
out of the task.

## Find the owning component

| Path | Guide and workflow |
| --- | --- |
| `apps/maple-research/` | [Research guide](apps/maple-research/AGENTS.md); `$develop-maple` for existing React/Vite/Tauri web, desktop, and mobile |
| `apps/maple-auth/` | [Auth guide](apps/maple-auth/AGENTS.md); independent hosted V2 native sign-in, package and publisher; Research keeps built-in auth |
| `apps/maple-agent/` | [Agent guide](apps/maple-agent/AGENTS.md); `$develop-maple-agent` for the independent GPUI desktop prototype |
| `sdk/` | [SDK guide](sdk/README.md); `$develop-opensecret-sdk` for TypeScript/React and Rust clients |
| `proxy/` | [Proxy guide](proxy/README.md); `$develop-maple-proxy` for the separate OpenAI-compatible relay |
| `services/opensecret/` | [Backend guide](services/opensecret/AGENTS.md); `$develop-opensecret` and its API/provider/validation/security skills |
| `services/updates/` | [Updater guide](services/updates/README.md); preserve deployed identity and installed-client metadata compatibility |
| Shared CI, Nix, hooks, docs | `$change-maple-ci` to change, trigger or debug CI; [CI](docs/ci.md) and [repository workflows](docs/repository-workflows.md); root `scripts/`, `.github/`, `.agents/`, flake, Just and repository metadata |

## Choose the development environment

Choose Local, hosted Dev, or Prod deliberately before setup or validation; keep
API, client/project identity, auth route, billing/flags, and account in that
environment. Read [development environments](docs/development-environments.md)
for the contract and native-auth traps.

- **Local:** use an isolated linked stack for any backend change, heavy feature,
  or work needing frontend/backend interaction, backend logs, or billing linkage.
- **Dev:** small frontend work may use hosted Dev with a valid Dev account and
  supported login path. Preserve an explicitly selected environment.
- **Prod:** use only for intentional, authorized production validation or work.

A debug build, simulator, or successful secrets check does not select Local.
Routine local authentication uses an encrypted password/account fixture path;
native OAuth/provider checks are an intentional hosted integration scenario.

## Essential invariants

- Inspect checkout, branch, and changes first. Preserve unrelated work; do not
  switch branches or rewrite history over a dirty checkout. Read source/tests
  before placement and define contracts when a change crosses components.
- Before editing ignored env files, overlays, ports, databases, or processes,
  identify their owner. An external workspace may manage them; follow its
  lifecycle/configuration instructions and preserve generated state.
- Use the owning pinned Nix flake. Shared app recipes run from the monorepo
  root; backend and other component commands use their documented directory.
  Avoid substitute global toolchains. Use component `just clean-local`, since
  raw `cargo clean` can erase shared intermediates.
- Keep auth, authorization, encrypted persistence, provider secrets/routing,
  model policy, and usage truth in OpenSecret. UI, flags, and billing presentation
  are not authority. Inspect every affected SDK/client contract.
- Validate untrusted input at the privileged boundary. Preserve account
  isolation, cancellation, lifecycle ownership, and fail-closed permissions.
  Never log credentials, decrypted content, prompts/responses, or sensitive URLs.
  `VITE_*` is public build configuration and cannot hold secrets.
- Preserve shipped identity, API/protocol, signing/updater, and installed-client
  compatibility. Use existing generators; inspect deltas and never hide them
  with Git index flags. Keep dependency upgrades and rewrites scoped.

## Validation and publication authority

Use `$validate-maple` or `$validate-opensecret` for evidence matching the changed
boundary, and the matching security skill for trust-boundary work. Focused tests,
component gates, packages, exact-app runtime, and deployed behavior are distinct.
Report commands/results, effective environment, and unverified layers. A build
or hook pass does not prove runtime integration.

### Pre-commit hook

`./setup-hooks.sh` enables component-selected fast gates; they do not run
integration, cargo-deny, flake checks, or packaging
([hook details](docs/repository-workflows.md#pre-commit-hook)). Run
`just lane repo` (the root `nix flake check`) for flake, workflow, CI-script,
release-configuration or documentation changes.

Routine development never authorizes signing, releases, store uploads, deployment,
or live-service changes. A master push can start signed builds/TestFlight uploads;
a GitHub Release starts release publication. Use `$release-maple` only for
authorized release work. EIF/PCR comparison, signed approval, and deployment
are separate; preserve [manual PCR compatibility](services/opensecret/docs/pcr-compatibility.md)
and installed-client raw URLs. Never change approvals to clear ordinary PR checks.

## CI rules for agents

Load `$change-maple-ci` before changing, triggering or debugging CI; it holds
the full rules and their reasons. In short:

- `ci-ok` is the one required check. Without labels, every PR must reach it
  well under 10 minutes; slower builds run only by `ci:*` label, nightly, or
  on master.
- Add or change checks only as lanes in `scripts/ci/lanes.py`, each with a
  `test_plan.py` case; no new side workflows or bespoke guard scripts.
- Never put `[skip ci]`, `[ci skip]` or similar in a commit or PR title: master
  plans each push from the previous commit, so a skipped commit is never checked.
- Test behavior, or a contract another file consumes; never a script's or
  workflow's text, and never hand-copied lists of names.
- Lanes never save a cache on a pull request (the EIF jobs' branch-scoped
  exceptions are in the skill), and master runs are never cancelled.
- Never edit release or publisher workflows, signing environments, or OpenSecret
  approval gating to clear a check; a failing contract test means fix the change.
- Run `just lane <id>` for the lanes your change selects before pushing. Add a
  `ci:*` label when the change carries platform or packaging risk its paths do
  not show ([labels](docs/ci.md#labels)).

## Maintaining this guidance

Keep this public repository self-contained: implementation contracts, setup,
tests, and CI belong here; private fleet/credential administration and rollout
procedures belong with their owners. Keep routers short and conditional detail
in owned references. Update material workflow changes alongside the code;
re-check stale guidance against source and scope unrelated corrections separately.
