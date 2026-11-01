#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 1 || ( "$1" != dev && "$1" != prod ) ]]; then
    echo "usage: build-release.sh dev|prod" >&2
    exit 2
fi
component="$(cd "$(dirname "$0")/.." && pwd)"
cd "$component"

export MAPLE_RELEASE_PROFILE="$1"
if [[ "$1" == dev ]]; then
    export VITE_OPEN_SECRET_PCR_ENVIRONMENT=development
else
    export VITE_OPEN_SECRET_PCR_ENVIRONMENT=production
fi
if [[ "$(uname -s)" == Darwin ]]; then
    export MACOSX_DEPLOYMENT_TARGET=15.0
fi
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*)
        # An explicit Windows target keeps +crt-static off host proc macros
        # and build scripts. Stage only this build's executable at the shared
        # prebuilt path.
        windows_target="${MAPLE_WINDOWS_ARCH:-x86_64}-pc-windows-msvc"
        cargo build --release -p maple-agent-app --locked --timings --target "$windows_target"
        mkdir -p target/release
        cp "target/$windows_target/release/maple-agent.exe" target/release/maple-agent.exe
        ;;
    Linux)
        # Like Zed, link against an old glibc so the binary runs on the host's
        # own glibc, display libraries and GPU drivers.
        # zig does not read the Nix compiler wrapper's search paths, so name the
        # Nix library directories explicitly. Those libraries are link-time
        # stand-ins for the host's copies, which resolve their own glibc
        # symbols at runtime; --as-needed drops any that end up unused.
        link_flags=(-C link-arg=-Wl,--allow-shlib-undefined)
        IFS=: read -ra link_dirs <<< "${MAPLE_AGENT_LINUX_RUNTIME_PATH:?run inside the Agent Nix shell}"
        for directory in "${link_dirs[@]}"; do
            link_flags+=(-L "native=$directory")
        done
        RUSTFLAGS="${RUSTFLAGS:-} ${link_flags[*]}" \
            cargo zigbuild --release -p maple-agent-app --locked --timings --target x86_64-unknown-linux-gnu.2.31
        mkdir -p target/release
        cp target/x86_64-unknown-linux-gnu/release/maple-agent target/release/maple-agent
        ;;
    *)
        cargo build --release -p maple-agent-app --locked --timings
        ;;
esac
