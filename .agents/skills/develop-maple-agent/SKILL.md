---
name: develop-maple-agent
description: Develop the GPUI Maple Agent desktop-v2 prototype, transport-neutral runtime, ACP/proxy modes, component Nix builds, and Agent-specific update discovery under apps/maple-agent. Use develop-maple for the shipped Research Tauri app instead.
---

# Develop Maple Agent

Read root `AGENTS.md`, `apps/maple-agent/AGENTS.md` (the component's
`CLAUDE.md`), its README, and the affected source and tests. The component
builds the `maple-agent-app` Cargo package into the `maple-agent` executable;
that name does not identify an arbitrary running development instance.

`app/src/backend.rs` adapts the transport-neutral runtime under
`crates/maple-agent/` to GPUI. Keep window/UI concerns in `app`, and shared
account/session/tool policy in the runtime. The runtime consumes the in-tree
`maple-proxy` library and independently selects `maple-sdk` in the workspace
manifest and lockfile. Follow the [SDK consumer version policy](../../../docs/sdk-publishing.md#consumer-version-policy):
prefer a published pin and allow local links during active development. Keep
the runtime and embedded proxy on one SDK source/version. Research's Tauri
runtime remains independently owned.

## Build with the component environment

From the repository root:

```sh
cd apps/maple-agent
nix develop --no-update-lock-file
just ci
```

`just ci` checks formatting, lint across default/headless/single-mode features,
and warning-denied workspace builds/tests. Use `just release` for optimized
build and performance evidence. Root `just agent-check`, `agent-build`, and
`agent-dev` enter this component environment. The repository pre-commit hook
runs `just ci-lint` and `just ci-test` (the `agent-lint` and `agent-test` CI
lanes) in this shell when Agent files are staged; `MAPLE_HOOK_FULL=1` adds
`just ci-extra`, the rest of `just ci`. Root `just lane repo` additionally
validates workflow selection and security contracts when CI, Nix, or routing
changes.

Agent has its own Cargo and Nix lockfiles. CI selects Agent for proxy runtime
changes and for Rust SDK build inputs, both path dependencies it builds from
the tree, but not for TypeScript SDK changes. Agent lanes live in
`scripts/ci/lanes.py`; root `just lane <id>` runs one locally. Every Agent PR
runs Linux lint and tests plus macOS and Windows Clippy; updater and
dependency inputs add the native Windows updater tests and supply-chain
checks. Packaging rehearsals and the full suites run by label (`ci:package`,
`ci:windows`, `ci:full`), nightly, or on master. Load
`$change-maple-ci` for what each label adds and how to trigger it, and
[desktop builds](../../../apps/maple-agent/docs/desktop-builds.md) for the
signed master packages.

Linux Nix packages use a pure source fileset rooted at the monorepo, including
the sibling SDK/proxy source and SDK assets. Validate it when adding a new local dependency
or build-time file. Never bypass a missing dependency hash by enabling an
unlocked or credential-bearing fetch.

## Launch the exact workspace

If an external workspace manager owns this checkout, use its documented
launcher and generated environment. Preserve its local/hosted service choices,
isolated XDG config/data, development bundle ID, and shared proxy reservation.
Agent does not load dotenv files. Do not copy production
credentials into source or silently use the legacy GPUI state directory.

For macOS app-identity checks, source the managed environment and run
`just debug-app` inside the component Nix shell, then launch the exact printed
bundle path. Managed debug bundles record only public service configuration
and both XDG roots in `LSEnvironment`, so GUI launches preserve isolation;
Missing roots fail packaging. The default signing identity is ad hoc; this
proves local package startup, not official distribution signing or TCC grants. Track and stop only
the process started by the current task. Never kill all `maple-agent` processes.

Shared Cargo intermediates belong to other worktrees too. Preserve inherited
build settings; use only this component's `just clean-local` for authorized
cleanup. Do not run raw `cargo clean` against the shared cache.

## Task integrations

For composer integrations and external providers, read
`apps/maple-agent/docs/external-agents.md`. Keep provider metadata in the
runtime catalog and pass typed selection kinds through the UI bridge so
user-controlled MCP names cannot shadow provider IDs. External providers must
be enabled in Settings before appearing in the composer, and each task must
explicitly select them. A saved task choice cannot bypass Settings. Preserve
CUA's existing backend metadata.
Exercise warm and cold session tool catalogs and ACP exclusion when changing
run-boundary admission. Claude Code uses a native Rust transport adapted from
the pinned Goose provider in `external_agents/claude.rs`. Preserve the source
attribution when changing that adapted code. Keep process ownership in Maple's
contained host. The `claude_native_*` tests re-execute the
Rust test binary as a CLI fixture and need no Claude account or inference
request. Keep fixture launch and environment setup shared with the Codex tests.

## Security and publication

Apply `$review-maple-security`'s trust-boundary and evidence methodology to the
actual GPUI source; its Tauri-specific file list is for Research. Validate
account isolation, tool execution, MCP/ACP inputs, persistence, and process
ownership at the layer implementing the effect. Never treat a passing source
import or a native login screen as authenticated chat or containment proof.

Packaged Prod builds check a signed per-platform update feed; Dev and unpackaged
builds have no updater. A release is one dispatch of `agent-desktop-build.yml`
with `release=true` after a version bump: it builds, signs and publishes every
platform ([Agent updates](../../../apps/maple-agent/docs/updates.md)). Releasing
is a production action, so run it only when the owner asks. Installed clients
parse the feed strictly; keep the wire contract in that doc unchanged. See
[desktop builds](../../../apps/maple-agent/docs/desktop-builds.md) for packaging.
