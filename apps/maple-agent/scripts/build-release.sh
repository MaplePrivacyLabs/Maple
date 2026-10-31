#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 1 || ( "$1" != dev && "$1" != prod ) ]]; then
    echo "usage: build-release.sh dev|prod" >&2
    exit 2
fi
component="$(cd "$(dirname "$0")/.." && pwd)"
cd "$component"

# Signed packaging is a later workflow step. Neither Cargo nor dependency build
# scripts receive certificates, notarization passwords, or updater keys.
unset APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD APPLE_ID APPLE_ID_PASSWORD
unset APPLE_PASSWORD APPLE_TEAM_ID TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PASSWORD
unset AZURE_CLIENT_ID AZURE_TENANT_ID AZURE_SUBSCRIPTION_ID
unset MAPLE_WINDOWS_ARTIFACT_SIGNING_ENDPOINT MAPLE_WINDOWS_ARTIFACT_SIGNING_ACCOUNT_NAME
unset MAPLE_WINDOWS_ARTIFACT_SIGNING_CERTIFICATE_PROFILE_NAME
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
        windows_arch="${MAPLE_WINDOWS_ARCH:-x86_64}"
        case "$windows_arch" in
            x86_64|aarch64) ;;
            *) echo "unsupported native Windows architecture" >&2; exit 2 ;;
        esac
        windows_target="${windows_arch}-pc-windows-msvc"
        cargo build --release -p maple-agent-app --locked --timings --target "$windows_target"
        mkdir -p target/release
        cp "target/$windows_target/release/maple-agent.exe" target/release/maple-agent.exe
        ;;
    Linux)
        [[ "$(uname -m)" == x86_64 ]] || { echo "Linux release bundles are x86_64 only" >&2; exit 1; }
        # Like Zed, link against an old glibc so the binary runs on the host's
        # own glibc, display libraries and GPU drivers. Keep this floor equal
        # to GLIBC_FLOOR in linux-release-appimage.py, whose audit enforces it.
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
binary="$component/target/release/maple-agent"
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*)
        # The unsigned prebuilt Windows bootstrap is not an installed app.
        # Validate its immutable PE metadata without entering runtime trust or
        # installation checks; signing and installed-app smoke happen later.
        python3 "$component/scripts/windows-build-info.py" "${binary}.exe" \
            --profile "$1" --source-sha "$(git rev-parse HEAD)" --arch "$windows_arch" >/dev/null
        ;;
    Linux)
        "$component/scripts/run-linux-payload.sh" "$binary" --build-info |
            python3 -c 'import json,sys; info=json.load(sys.stdin); expected=sys.argv[1]; assert info["profile"] == expected, "incorrect release profile"' "$1"
        ;;
    *)
        "$binary" --build-info |
            python3 -c 'import json,sys; info=json.load(sys.stdin); expected=sys.argv[1]; assert info["profile"] == expected, "incorrect release profile"' "$1"
        ;;
esac
