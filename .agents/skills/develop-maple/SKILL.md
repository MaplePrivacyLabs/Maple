---
name: develop-maple
description: Implement ordinary Research client features and fixes in React/Vite/Tauri, including its web, desktop, and mobile paths. Use for client setup, placement, focused development, and handoff; use the Agent Mode, SDK, proxy, validation, or release skills for those workflows.
---

# Develop Maple Research

Work from the monorepo root. Read root and Research `AGENTS.md`, affected
source/tests, `justfile`, package manifests, flake and relevant CI. Research
includes the existing desktop Agent Mode; the independent hosted Auth and
GPUI Agent apps follow their own component guides.

## Choose environment and runtime

Use the [environment contract](../../../docs/development-environments.md):
Local for backend changes, heavy features, or specific frontend/backend/log/
billing interaction; hosted Dev is suitable for intentional small frontend
work with a valid Dev-account login. Preserve selected API/project, account,
auth route, and dependency endpoints.

Identify external ownership before configuring ignored env files, overlays,
ports, or processes. Consume generated state and use its owner's lifecycle.
Standalone setup is conditional on those resources being unowned:

```sh
nix develop --no-update-lock-file
./setup-hooks.sh
just install
test -e apps/maple-research/frontend/.env.local || cp apps/maple-research/frontend/.env.example apps/maple-research/frontend/.env.local
```

Choose the smallest runtime exercising the change: `just dev` for web;
`just desktop-dev` for native/Agent Mode. These consume existing configuration;
for hosted Dev browser work, use the
[explicit Dev profile loop](../../../docs/development-environments.md#research-hosted-dev-browser-loop).
For Research iOS, follow
[the ONNX/runtime guide](../../../apps/maple-research/docs/ios-onnxruntime-local-development.md)
and select the simulator/device explicitly. These are Tauri commands, not a
setup workflow for a new native Swift client. Debug/simulator does not imply
Local. Local baseline login uses encrypted password/account fixtures; native
OAuth is an intentional hosted/provider check with separate browser origin.

## Implement at the owning boundary

Keep UI/account-scoped state in `apps/maple-research/frontend/src/` and native
effects in `frontend/src-tauri/src/`. OpenSecret owns authorization, persistence,
cryptography, provider/model policy and usage. Use selected SDK client contracts
and the [consumer version policy](../../../docs/sdk-publishing.md#consumer-version-policy);
do not duplicate encryption or emulate missing server enforcement in UI.

Read [implementation contracts](../../../apps/maple-research/docs/development-contracts.md)
only for relevant placement, privileged IPC, account/cancellation, or runtime
details. Preserve generated files through their generators and inspect deltas.
Use Bun, component Cargo commands and owning pinned toolchains; do not add
substitute lockfiles/toolchains or unrelated upgrades.

Load the matching workflow when the task crosses it:

- `$change-maple-agent-mode`: embedded Goose/ACP/MCP, tools/permissions, subagents.
- `$develop-opensecret-sdk` or `$develop-maple-proxy`: their owned source/contracts.
- `$review-maple-security`: a trust-boundary change or security review.
- `$validate-maple`: validation beyond the focused loop and before handoff.
- `$release-maple`: explicitly authorized publication only.

## Focused loop and handoff

Reproduce the changed behavior, add focused behavior tests where they provide
useful proof, and run affected complete component gates before handoff using
`$validate-maple`. Frontend Bun commands use `apps/maple-research/frontend/`;
Rust commands use its `src-tauri/`. There is no root `just test`. The hook runs
selected frontend checks plus Research Rust formatting/tests; strict Clippy
is a separate optional diagnostic, not current Research CI/hook.

Inspect `git diff --check`, complete diff and status. Report changed behavior,
contracts/files, exact checks, effective environment and exact runtime evidence,
and untested boundaries. Preserve unrelated work and existing authorization.
Commit/push/PR/publication only within user-authorized scope; routine validation
does not authorize releases, master pushes, deployment, or store uploads.
