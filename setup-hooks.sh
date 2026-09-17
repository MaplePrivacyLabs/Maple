#!/bin/sh
# Enable the Maple monorepo pre-commit hook for this checkout (and its worktrees).

GIT_ROOT=$(git rev-parse --show-toplevel 2>/dev/null)
if [ -z "$GIT_ROOT" ]; then
    echo "Error: Not in a git repository"
    exit 1
fi

git config core.hooksPath .githooks

echo "Git hooks configured (core.hooksPath = .githooks)."
echo "The pre-commit hook classifies staged paths and runs the affected component"
echo "checks inside that component's Nix flake (or from PATH without Nix):"
echo "  apps/maple-research   prettier, eslint, tsc, bun test; cargo fmt/clippy/test for src-tauri"
echo "  apps/maple-agent      cargo fmt, clippy, test (MAPLE_HOOK_FULL=1 runs 'just ci')"
echo "  sdk                   Rust fmt/clippy/test --lib/doc; TypeScript prettier, build, unit tests"
echo "  proxy                 cargo fmt, clippy, test, doc, machete"
echo "  services/opensecret   cargo fmt, clippy, test (no database or containers)"
echo "  services/updates      prettier, tsc, bun test"
echo "  repo                  actionlint on staged workflows, scripts/ci unit tests"
echo "Integration suites, cargo-deny, nix flake check, and packaging stay in CI."
echo "Skip once with 'git commit --no-verify' or MAPLE_HOOK_SKIP=1."
