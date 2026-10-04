#!/usr/bin/env bash
# Package a previously built binary. Never compile while signing credentials exist.
set +x
set -euo pipefail
umask 077

if [[ $# -lt 1 || $# -gt 2 || ( "$1" != dev && "$1" != prod ) || ( $# == 2 && "$2" != --unsigned ) ]]; then
    echo "usage: package-release.sh dev|prod [--unsigned]" >&2
    exit 2
fi
profile="$1"
unsigned=()
[[ $# == 1 ]] || unsigned=(--unsigned)
component="$(cd "$(dirname "$0")/.." && pwd)"
binary="$component/target/release/maple-agent"
metadata_tool="$component/scripts/release-info.py"

# Keep inherited credentials private to this shell until the macOS signing
# helper starts. Public metadata probes and package tools receive none of them.
for name in APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD APPLE_ID APPLE_ID_PASSWORD APPLE_PASSWORD APPLE_TEAM_ID; do
    export -n "$name" 2>/dev/null || true
done
unset TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PASSWORD
[[ -x "$binary" ]] || { echo "build the exact release profile before packaging" >&2; exit 1; }
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) platform=macos-aarch64 ;;
    Linux-x86_64) platform=linux-x86_64 ;;
    *) echo "release packaging supports macOS ARM64 and Linux x86_64" >&2; exit 1 ;;
esac

mkdir -p "$component/dist"
staging="$(mktemp -d "$component/dist/.release-${profile}.XXXXXX")"
trap 'rm -rf -- "$staging"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
"$binary" --build-info > "$staging/build-info.json"
source_sha="$(git -C "$component" rev-parse HEAD)"
python3 "$metadata_tool" validate "$profile" "$staging/build-info.json" --source-sha "$source_sha"
field() { python3 "$metadata_tool" field "$profile" "$staging/build-info.json" --field "$1"; }
export MAPLE_PACKAGE_CHANNEL="$profile"
export MAPLE_PACKAGE_APP_NAME="$(field display_name)"
export MAPLE_PACKAGE_BUNDLE_ID="$(field bundle_id)"
export MAPLE_PACKAGE_VERSION="$(field version)"
export MAPLE_PACKAGE_BUILD_NUMBER="${GITHUB_RUN_NUMBER:-1}"
export SOURCE_DATE_EPOCH="$(git -C "$component" show -s --format=%ct "$source_sha")"
name="maple-agent-${profile}-${MAPLE_PACKAGE_VERSION}-$(field git_revision)-${platform}"

if [[ "$platform" == macos-aarch64 ]]; then
    # Export secrets only to the native signing helper, after all binary/config
    # validation has completed. It unexports them before staging/native probes.
    APPLE_CERTIFICATE="${APPLE_CERTIFICATE:-}" \
    APPLE_CERTIFICATE_PASSWORD="${APPLE_CERTIFICATE_PASSWORD:-}" \
    APPLE_ID="${APPLE_ID:-}" APPLE_ID_PASSWORD="${APPLE_ID_PASSWORD:-}" \
    APPLE_TEAM_ID="${APPLE_TEAM_ID:-}" \
        "$component/scripts/macos-release-app.sh" "$binary" "$staging" "$name" "${unsigned[@]}"
else
    "$component/scripts/linux-release-appimage.sh" "$binary" "$staging/${name}.AppImage"
fi
python3 "$metadata_tool" manifest "$profile" "$staging/build-info.json" \
    --output "$staging" --platform "$platform" "${unsigned[@]}"
"$component/scripts/verify-release.sh" "$profile" "$staging" "${unsigned[@]}"

# Keep the previous package until the complete new package passes verification.
destination="$component/dist/$profile"
if [[ -e "$destination" ]]; then
    echo "artifact directory already exists: $destination; move it aside before packaging again" >&2
    exit 1
fi
mv "$staging" "$destination"
echo "$destination"
