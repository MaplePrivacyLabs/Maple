# Maple

Maple is a monorepo for the Maple applications, Maple SDKs, local proxy,
OpenSecret backend, and updater service. The existing web, desktop, and mobile client lives under
[`apps/maple-research/`](apps/maple-research/README.md), including Research chat
and desktop Agent Mode. Its directory name does not change the shipped Maple
application identity.

## Components

| Path | Responsibility |
| --- | --- |
| [`apps/maple-research/`](apps/maple-research/README.md) | React/Vite frontend and Tauri desktop/mobile application, app documentation, and distribution configuration |
| [`apps/maple-agent/`](apps/maple-agent/README.md) | GPUI desktop-v2 prototype, ACP agent, and CLI proxy; independent from the shipped Research app |
| [`sdk/`](sdk/README.md) | Independently published Maple TypeScript/React and Rust SDKs |
| [`proxy/`](proxy/README.md) | Standalone OpenAI-compatible proxy, also consumed by desktop Maple |
| [`services/updates/`](services/updates/README.md) | Desktop updater Worker and verified release-metadata publishing |
| [`services/opensecret/`](services/opensecret/README.md) | Confidential authentication, inference, conversations, and related backend APIs; local Nitro tooling and signed PCR files |
| [`docs/`](docs/) | Shared deployment documentation |
| [`scripts/`](scripts/) and [`.github/workflows/`](.github/workflows/) | Shared validation and release entry points |

The backend import and legacy PCR publication contract are recorded in
[the backend migration note](docs/opensecret-import.md). The original
[OpenSecret repository](https://github.com/OpenSecretCloud/opensecret) remains
available for installed clients' signed PCR histories. The GPUI import history
and upstream follow-up branches are recorded in
[the Agent migration note](docs/maple-agent-import.md).

## Development

Use the pinned Nix environment and run shared commands from the repository root:

```bash
nix develop --no-update-lock-file
./setup-hooks.sh
just install
just                    # List recipes
just dev                # Research web development
just desktop-dev        # Research Tauri application, including Agent Mode
just agent-dev          # GPUI Agent app, using its own pinned component shell
```

`./setup-hooks.sh` enables the pre-commit hook, which runs the format, lint,
and unit-test checks for the components whose files are staged inside their
pinned Nix shells (see the [root agent guide](AGENTS.md#pre-commit-hook)).

Follow the [Research setup guide](apps/maple-research/README.md#quick-start) to
configure `apps/maple-research/frontend/.env.local` before starting the app.
Preserve existing configuration and externally managed workspace resources.
All `VITE_*` values are public client configuration and must never contain secrets.
Root `justfile`, `flake.nix`, `.agents/`, and CI scripts remain shared entry points;
use each component's guide for its direct Bun or Cargo commands.
Consumers select their SDK versions independently. Published pins are the
default, with local links available for development; follow the
[SDK consumer version policy](docs/sdk-publishing.md#consumer-version-policy).
OpenSecret retains its own pinned Nix shell and Rust package: enter
`services/opensecret/` before running its commands, and follow its
[setup guide](services/opensecret/README.md#local-quick-start) for stateful
PostgreSQL and environment hooks.

## Validation and releases

Use the [root agent guide](AGENTS.md) for shared policy and the
[Research validation guide](apps/maple-research/README.md#validation) or component
README for the checks affected by a change. Report unit tests, builds, runtime
smoke tests, and deployed behavior separately.

Production-shaped master builds and GitHub Releases are production actions. A
master push that changes app inputs can upload an iOS build to TestFlight; a
GitHub Release starts cross-platform packaging and downstream publication. Use
[the release procedure](.agents/skills/release-maple/SKILL.md) only for authorized
release work. The [Pages guide](docs/pages-deployments.md) documents preview and
production deployment profiles and controls.
SDK publication uses separate protected manual workflows and does not create
a Maple application release; see the [SDK publishing guide](docs/sdk-publishing.md).
OpenSecret's root CI workflows validate code and in-tree SDK compatibility;
they do not publish EIFs or deploy the TEE service. Backend deployment and
[manual signed-PCR publication](services/opensecret/docs/pcr-compatibility.md)
remain explicit operator actions.

Keep changes focused, preserve compatibility at public and installed-client
boundaries, and report the exact checks and runtime paths exercised before
opening a pull request.
