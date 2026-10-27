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
target_args=()
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) target_args=(--target x86_64-pc-windows-msvc) ;;
esac
# An explicit Windows target keeps +crt-static off host proc macros and build
# scripts. Stage only this build's executable at the shared prebuilt path.
cargo build --release -p maple-agent-app --locked --timings "${target_args[@]}"
if [[ ${#target_args[@]} != 0 ]]; then
    mkdir -p target/release
    cp target/x86_64-pc-windows-msvc/release/maple-agent.exe target/release/maple-agent.exe
fi
binary="$component/target/release/maple-agent"
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*)
        # The unsigned prebuilt Windows bootstrap is not an installed app.
        # Validate its immutable PE metadata without entering runtime trust or
        # installation checks; signing and installed-app smoke happen later.
        python3 "$component/scripts/windows-build-info.py" "${binary}.exe" \
            --profile "$1" --source-sha "$(git rev-parse HEAD)" >/dev/null
        ;;
    *)
        "$binary" --build-info |
            python3 -c 'import json,sys; info=json.load(sys.stdin); expected=sys.argv[1]; assert info["profile"] == expected, "incorrect release profile"' "$1"
        ;;
esac
