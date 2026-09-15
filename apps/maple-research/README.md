# Maple Research

Maple Research is the existing Maple client, built with React, Vite, and Tauri.
It runs as a web application and as native desktop/mobile applications, and uses
[OpenSecret](../../services/opensecret/README.md) for confidential
authentication, inference, conversations, and related APIs.

Research chat and desktop Agent Mode are different client paths. Research chat
uses the TypeScript Maple SDK with the Responses and Conversations APIs;
Agent Mode embeds Goose and uses the Rust Maple SDK through Tauri. The
local OpenAI-compatible proxy is a separate user-facing service.

The Maple SDK source and its upstream Git history live under
[`sdk/`](../../sdk/README.md). Research selects published TypeScript and Rust
versions independently in its manifests and lockfiles, with local links
supported during development. See the
[SDK consumer version policy](../../docs/sdk-publishing.md#consumer-version-policy).
The proxy source and its upstream history live under
[`proxy/`](../../proxy/README.md); desktop Maple consumes that in-tree crate too.

The source directory name does not change the shipped Maple application identity
or remove desktop Agent Mode. Commands and code paths below are relative to the
**monorepo root**, unless a command explicitly changes directory. Shared Nix,
Just, CI, and release entry points remain there.

## Quick start

The supported development environment is the Nix flake. It pins Bun, Rust,
platform tools, and native dependencies used by the repository.

```bash
nix develop --no-update-lock-file
./setup-hooks.sh
just install
test -e apps/maple-research/frontend/.env.local || cp apps/maple-research/frontend/.env.example apps/maple-research/frontend/.env.local
```

This creates `apps/maple-research/frontend/.env.local` only when it is absent. Never overwrite an
existing file: it may be externally managed or contain checkout-specific
backend and application configuration. Inspect its source before changing its
values.

The common recipes below assume the active Nix shell. For an independent call,
prefix a recipe with `nix develop --no-update-lock-file -c`.

Select the intended OpenSecret API in the ignored `apps/maple-research/frontend/.env.local`:

```dotenv
VITE_OPEN_SECRET_API_URL=http://127.0.0.1:3000
```

The public Maple client ID is already present in `.env.example`. All `VITE_*`
values are shipped to the client and must never contain secrets.

Start the runtime you intend to exercise:

```bash
just dev          # Browser/web development only
just desktop-dev  # Tauri desktop, including Agent Mode and native features
```

`just desktop-dev` is preferred over a raw `bun tauri dev`: it provisions the
pinned ONNX Runtime and applies a local Tauri configuration overlay when one is
present.

## API configuration

`apps/maple-research/frontend/.env.example` documents Maple's public configuration surface:

- `VITE_OPEN_SECRET_API_URL` selects the required OpenSecret backend.
- `VITE_OPEN_SECRET_PCR_ENVIRONMENT` selects the matching PCR0 trust roots;
  it defaults to `production`, so hosted development enclaves must set
  `development` explicitly.
- `VITE_CLIENT_ID` overrides Maple's public project ID when developing against
  another OpenSecret project.
- `VITE_OS_FLAGS_BASE_URL` selects an optional feature-flags API.
- `VITE_MAPLE_BILLING_API_URL` selects an optional billing API.
- `VITE_FORCE_FEATURE_FLAGS` is a local preview override, not authorization.

Flags and billing are independent clients; configure their dev or production
API URLs for the environment being tested. A working OpenSecret chat does not
prove billing- or flag-gated behavior. Provider credentials and administrative
API keys belong on their servers, never in Maple.

To use a local backend, follow
[OpenSecret's setup guide](../../services/opensecret/README.md#local-quick-start)
from `services/opensecret/`, including its SQL migration step. Keep the default
frontend origin unless you also intend to change and validate OAuth/verification
callback handling.

## Common commands

Run commands from the repository root:

```bash
just                    # List recipes
just install            # Install frontend dependencies
just dev                # Web dev server
just desktop-dev        # Desktop dev application
just desktop-build-debug-overlay # Unsigned debug package with local overlay
just build              # Local web build
just format             # Format frontend source
just lint               # Lint frontend source
just rust-check         # Check the Tauri Rust crate
just rust-lint          # Rust formatting check and strict Clippy
just clean-local        # Clean only this checkout's Cargo artifacts
```

Bun commands run from `apps/maple-research/frontend/`; Cargo commands run from
`apps/maple-research/frontend/src-tauri/`. Raw `cargo clean` can remove a shared Nix Cargo build
directory, so use `just clean-local`.

## Validation

Use the checked-in CI entry points rather than reconstructing them:

```bash
# Frozen install, formatting, ESLint, typecheck, and Bun tests
nix develop --no-update-lock-file .#ci -c ./scripts/ci/frontend.sh

# Locked all-target Rust tests
nix develop --no-update-lock-file .#ci -c ./scripts/ci/rust.sh

# PR-configured web artifact
MAPLE_WEB_ENVIRONMENT=pr nix develop --no-update-lock-file .#ci -c ./scripts/ci/web.sh

# Nix/toolchain/workflow metadata; not application tests
nix flake check --no-update-lock-file
```

There is no general checked-in browser, packaged-app, or React-to-Tauri-command
integration harness. Unit tests and package builds do not prove GUI, native,
backend, billing, flags, or IPC integration. A privileged IPC change therefore
requires a manual smoke test through the exact desktop application and native
effect. For change-specific test selection and a repeatable evidence format,
use `.agents/skills/validate-maple/`.

PR build scripts deliberately ignore local `.env*` files and compile fixed PR
endpoints. They prove PR packaging, not integration with the backend configured
in `apps/maple-research/frontend/.env.local`. To smoke an exact desktop development application
against that configured backend, use `just desktop-dev`, record the active
Tauri overlay, endpoint configuration, executable and application identifier,
then exercise the user entry point through Tauri IPC to the native result.

## Platform development

Desktop recipes provision ONNX Runtime automatically:

```bash
just desktop-build                 # Standard application identity
just desktop-build-debug           # Standard application identity
just desktop-build-debug-overlay   # Requires .local/tauri-workspace.json
```

Only the overlay recipe applies `.local/tauri-workspace.json` while packaging.
Use it when a checkout-specific bundle identity is part of the smoke test.

Linux desktop builds require the system libraries supplied by the Nix shell.
For an already-built binary in a headless display environment, WebKit may need:

```bash
WEBKIT_DISABLE_COMPOSITING_MODE=1 \
WEBKIT_DISABLE_DMABUF_RENDERER=1 \
DISPLAY=:0 ./apps/maple-research/frontend/src-tauri/target/debug/maple
```

Apple development uses the pinned Apple shell and repository recipes:

```bash
just ios-build-onnxruntime
just ios-dev
just ios-dev-sim 'iPhone 16 Pro'
just ios-dev-device 'Your iPhone'
```

`just ios-fix-arch` mutates the generated Xcode project. Inspect and either
commit or deliberately restore generated changes; do not hide them from Git.

Android development is supported from x86_64 Linux; the `.#android` shell is
not exposed on macOS:

```bash
nix develop --no-update-lock-file .#android -c just android-build
```

The CI scripts under `scripts/ci/` are the authority for PR and release-shaped
platform builds. Some are platform-specific, remove build outputs or
`node_modules`, and deliberately ignore local `.env*` files in favor of fixed
PR/release endpoints; read a script before running it locally.

## Architecture and feature documentation

- [`docs/agent-mode-mcp.md`](docs/agent-mode-mcp.md) explains MCP configuration
  and includes a deterministic feature smoke test.
- [`docs/agent-mode-acp.md`](docs/agent-mode-acp.md) documents the ACP edge and
  its trust model.
- [`docs/pdf-ocr.md`](docs/pdf-ocr.md) covers the local PDF/OCR pipeline.
- [`AGENTS.md`](AGENTS.md) defines Research placement and runtime standards;
  the [root guide](../../AGENTS.md) defines shared repository policy.
- [`.agents/skills/`](../../.agents/skills/) contains task-specific development,
  validation, security, Agent Mode, and release procedures.

`apps/maple-research/frontend/src/routeTree.gen.ts` and native platform projects contain generated
content. Use their generators and review the resulting diff rather than editing
generated output opportunistically.

## Releases

Release preparation and publication are production actions. A push to
`master` that changes classified Maple app inputs starts production-shaped
signed workflows and can upload an iOS build to TestFlight; creating a GitHub
Release always starts the complete release pipeline, attaches the four native
`maple-proxy` archives to that same release, and starts downstream publication.
Do not use either as routine validation.

Use `.agents/skills/release-maple/` for version parity, tag safety, workflow
monitoring, artifact verification, and explicit store handoff. Do not use the
legacy `just release` recipe to create an unreviewed local tag.

When the OpenSecret enclave changes, update and review the corresponding
`PCR0_DEV_VALUES` or `PCR0_VALUES` in
`apps/maple-research/frontend/src/config/openSecretClientConfig.ts` as part of the
attestation compatibility change.

Version changes update:

- `apps/maple-research/frontend/package.json`
- `apps/maple-research/frontend/src-tauri/tauri.conf.json`
- `apps/maple-research/frontend/src-tauri/Cargo.toml`
- `apps/maple-research/frontend/src-tauri/gen/apple/project.yml`
- `apps/maple-research/frontend/src-tauri/gen/apple/maple_iOS/Info.plist`
- `apps/maple-research/frontend/src-tauri/Cargo.lock`

## Contributing

Keep changes focused, follow the nearest established patterns, add regression
coverage at the owning layer, and report exactly which checks and runtime paths
you exercised. Security-sensitive changes should distinguish source-confirmed
facts from deployment assumptions and live-environment observations.

Before opening a pull request, run the applicable full gates above and inspect
`git diff --check`. See `AGENTS.md` for the complete change and review standard.
