---
name: develop-maple-agent
description: Develop the GPUI Maple Agent desktop-v2 prototype, transport-neutral runtime, ACP/proxy modes, component Nix builds, and Agent-specific update discovery under apps/maple-agent. Use develop-maple or change-maple-agent-mode for the shipped Research Tauri app instead.
---

# Develop Maple Agent

Read root `AGENTS.md`, `apps/maple-agent/AGENTS.md` (the component's
`CLAUDE.md`), its README, and the affected source and tests. The component
retains the internal `maple-gpui` Cargo package/executable; that name does not
identify an arbitrary running development instance.

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
runs `cargo fmt`, one workspace Clippy pass, and the workspace tests in this
shell when Agent files are staged; set `MAPLE_HOOK_FULL=1` for the complete
`just ci`. Root `nix flake check
--no-update-lock-file` additionally validates workflow selection and security
contracts when CI, Nix, or routing changes.

Agent has its own Cargo and Nix lockfiles. CI conservatively selects Agent for
shared Rust SDK/proxy runtime changes even when its SDK is registry-pinned;
passing that build does not validate unpublished SDK source. Component-only
changes should not unnecessarily select Research packaging. Maintain the root
selectors, their tests, and `.github/workflows/agent-ci.yml` together.

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
the process started by the current task. Never kill all `maple-gpui` processes.

Shared Cargo intermediates belong to other worktrees too. Preserve inherited
build settings; use only this component's `just clean-local` for authorized
cleanup. Do not run raw `cargo clean` against the shared cache.

## Task integrations

For composer integrations and external providers, read
`apps/maple-agent/docs/external-agents.md`. Keep provider metadata in the
runtime catalog and pass typed selection kinds through the UI bridge so
user-controlled MCP names cannot shadow provider IDs. Preserve inherited
defaults for tasks without overrides and CUA's existing backend metadata.
Exercise warm and cold session tool catalogs and ACP exclusion when changing
run-boundary admission.

## Security and publication

Apply `$review-maple-security`'s trust-boundary and evidence methodology to the
actual GPUI source; its Tauri-specific file list is for Research. Validate
account isolation, tool approval, MCP/ACP inputs, persistence, and process
ownership at the layer implementing the effect. Never treat a passing source
import or a native login screen as authenticated chat or containment proof.

Agent's update checker only links to stable `maple-agent-vX.Y.Z` releases.
Never use repository-wide `/releases/latest` for Agent, accept Research's bare
`vX.Y.Z` tags, or turn a failed/incomplete release scan into an update offer.
No Agent publisher is activated by the import. Future Agent release work
requires explicit authorization and `make_latest: false` to preserve Research's
latest pointer. Do not rename SDKs or publish registries incidentally.
