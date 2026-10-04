#!/usr/bin/env bash
# Validate the downloaded executable before a later signing step receives secrets.
set +x
set -euo pipefail
umask 077

if [[ $# != 1 || ( "$1" != dev && "$1" != prod ) ]]; then
    echo "usage: verify-prebuilt-release.sh dev|prod" >&2
    exit 2
fi
component="$(cd "$(dirname "$0")/.." && pwd)"
binary="$component/target/release/maple-agent"

unset APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD APPLE_ID APPLE_ID_PASSWORD
unset APPLE_PASSWORD APPLE_TEAM_ID TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PASSWORD
[[ -f "$binary" && ! -L "$binary" ]] || { echo "download the exact prebuilt release binary before verification" >&2; exit 1; }
# Artifact downloads do not preserve Unix execution permissions.
chmod 0755 "$binary"
metadata="$(mktemp)"
trap 'rm -f -- "$metadata"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
"$binary" --build-info > "$metadata"
source_sha="$(git -C "$component" rev-parse HEAD)"
python3 "$component/scripts/release-info.py" validate "$1" "$metadata" --source-sha "$source_sha"
echo "Verified prebuilt $1 Agent binary for $source_sha"
