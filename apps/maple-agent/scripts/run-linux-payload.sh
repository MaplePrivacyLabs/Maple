#!/usr/bin/env bash
# Run a Linux release binary on the Agent Nix shell's own glibc and runtime
# libraries. The binary targets an old glibc floor and normally runs on the
# host's libraries; in the build shell that would mix the host glibc with Nix
# libraries, so use the matching Nix loader instead.
# Usage: run-linux-payload.sh BINARY [ARGS...]
set -euo pipefail
: "${MAPLE_AGENT_LINUX_GLIBC:?run inside the Agent Nix shell}"
: "${MAPLE_AGENT_LINUX_RUNTIME_PATH:?run inside the Agent Nix shell}"
exec "$MAPLE_AGENT_LINUX_GLIBC/lib/ld-linux-x86-64.so.2" \
    --library-path "$MAPLE_AGENT_LINUX_GLIBC/lib:$MAPLE_AGENT_LINUX_RUNTIME_PATH" "$@"
