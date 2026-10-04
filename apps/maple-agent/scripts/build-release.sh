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
export MAPLE_RELEASE_PROFILE="$1"
if [[ "$1" == dev ]]; then
    export VITE_OPEN_SECRET_PCR_ENVIRONMENT=development
else
    export VITE_OPEN_SECRET_PCR_ENVIRONMENT=production
fi
if [[ "$(uname -s)" == Darwin ]]; then
    export MACOSX_DEPLOYMENT_TARGET=15.0
fi
cargo build --release -p maple-agent-app --locked
"$component/target/release/maple-agent" --build-info |
    python3 -c 'import json,sys; info=json.load(sys.stdin); expected=sys.argv[1]; assert info["profile"] == expected, "incorrect release profile"' "$1"
