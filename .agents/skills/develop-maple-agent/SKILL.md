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
account/session/tool policy in the runtime. The runtime consumes the existing
local `maple-sdk` and `maple-proxy` packages; registry publication is separate
work. Research's Tauri runtime remains independently owned.

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
`agent-dev` enter this component environment. Root `nix flake check
--no-update-lock-file` additionally validates workflow selection and security
contracts when CI, Nix, or routing changes.

`just test` and `just ci` prepare the pinned bundled CPython fixture and run its
worker and packaging suites. `just code-mode-smoke` exercises the actual worker;
direct Cargo worker tests require `just python-prepare` first. These commands
run from `apps/maple-agent/`. Runtime execution never downloads Python or falls
back to the system interpreter. Linux Nix packages retain their separately
declared CPython runtime closure; portable debug/archive layouts use the pinned
Python standalone distribution.

Agent has its own Cargo and Nix lockfiles. Shared Rust SDK/proxy runtime
changes must select Agent as well as the Research consumer; component-only
changes must not unnecessarily select Research packaging. Maintain the root
selectors, their tests, and `.github/workflows/agent-ci.yml` together.

Linux Nix packages use a pure source fileset rooted at the monorepo, including
the sibling SDK/proxy source and SDK assets. Validate it when adding a new local dependency
or build-time file. Never bypass a missing dependency hash by enabling an
unlocked or credential-bearing fetch.

## Launch the exact workspace

If OpenSecret Workspaces owns this checkout, use its generated
`bin/maple-agent` and `env/maple-agent.sh`. They select local/hosted services,
private XDG config/data, an isolated development bundle ID, and the shared
proxy reservation. Agent does not load dotenv files. Do not copy production
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
