#!/usr/bin/env bash
# Verify the exact downloaded files; never sign, install updates, or publish.
set +x
set -euo pipefail
umask 077
if [[ $# -lt 2 || ( "$1" != dev && "$1" != prod ) ]]; then
    echo "usage: verify-release.sh dev|prod ARTIFACT_DIRECTORY [--unsigned] [--static] [--platform PLATFORM]" >&2
    exit 2
fi
profile="$1"
directory="$(cd "$2" && pwd)"
unsigned=()
static=false
platform_options=()
shift 2
while [[ $# -gt 0 ]]; do
    case "$1" in
        --unsigned) [[ ${#unsigned[@]} == 0 ]] || exit 2; unsigned=(--unsigned); shift ;;
        --static) [[ "$static" == false ]] || exit 2; static=true; shift ;;
        --platform)
            [[ ${#platform_options[@]} == 0 && $# -ge 2 && ( "$2" == macos-aarch64 || "$2" == linux-x86_64 ) ]] || exit 2
            platform_options=(--platform "$2")
            shift 2
            ;;
        *) echo "unknown verification option: $1" >&2; exit 2 ;;
    esac
done
component="$(cd "$(dirname "$0")/.." && pwd)"
unset APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD APPLE_ID APPLE_ID_PASSWORD APPLE_PASSWORD APPLE_TEAM_ID
unset TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PASSWORD
expected_source="$(git -C "$component" rev-parse HEAD)"
manifest="$(python3 "$component/scripts/release-info.py" verify "$profile" "$directory" --source-sha "$expected_source" "${unsigned[@]}" "${platform_options[@]}")"
platform="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["platform"])' "$manifest")"
package="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["package"])' "$manifest")"
temporary="$(mktemp -d "${TMPDIR:-/tmp}/maple-agent-verify.XXXXXX")"
mount=""
cleanup() {
    local status=$?
    trap - EXIT INT TERM HUP
    if [[ -n "$mount" ]]; then
        # The shared host exits shortly after the GUI probe's last client.
        # Give it time to release the mounted executable before removing files.
        for attempt in {1..10}; do
            if /usr/bin/hdiutil detach "$mount" -quiet >/dev/null 2>&1; then
                mount=""
                break
            fi
            sleep 1
        done
        if [[ -n "$mount" ]]; then
            echo "Could not detach verification image; leaving $temporary intact" >&2
            exit 1
        fi
    fi
    rm -rf -- "$temporary"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

if [[ "$platform" == macos-aarch64 ]]; then
    [[ "$(uname -s)-$(uname -m)" == Darwin-arm64 ]] || { echo "macOS artifacts require native Apple Silicon verification" >&2; exit 1; }
    if [[ ${#unsigned[@]} == 0 ]]; then
        /usr/bin/codesign --verify --strict --verbose=2 "$directory/$package"
        /usr/bin/xcrun stapler validate "$directory/$package"
        /usr/sbin/spctl --assess --type open --context context:primary-signature --verbose=2 "$directory/$package"
    fi
    mount="$temporary/mount"
    mkdir "$mount"
    /usr/bin/hdiutil attach -readonly -nobrowse -noautoopen -mountpoint "$mount" "$directory/$package" >/dev/null
    app_name="$(python3 "$component/scripts/release-info.py" field "$profile" "$directory/build-info.json" --field display_name)"
    bundle_name="$(python3 "$component/scripts/release-info.py" bundle-name "$profile" "$directory/build-info.json")"
    bundle_id="$(python3 "$component/scripts/release-info.py" field "$profile" "$directory/build-info.json" --field bundle_id)"
    verify_macos_app() {
        local app="$1" signature executable
        [[ -d "$app" ]] || { echo "downloaded package does not contain the expected app" >&2; return 1; }
        /usr/bin/codesign --verify --deep --strict --verbose=2 "$app"
        signature="$(/usr/bin/codesign --display --verbose=4 "$app" 2>&1)"
        if [[ ${#unsigned[@]} == 0 ]]; then
            grep -Fq 'Authority=Developer ID Application:' <<< "$signature"
            grep -Eq 'flags=.*\(runtime\)' <<< "$signature"
            /usr/bin/xcrun stapler validate "$app"
            /usr/sbin/spctl --assess --type execute --verbose=2 "$app"
        else
            grep -Fq 'Signature=adhoc' <<< "$signature"
            # An ad-hoc signature has no Team ID. Hardened runtime would make
            # dyld reject the bundled non-system libraries despite valid seals.
            if ! grep -Eq '^CodeDirectory .* flags=0x0*2\(adhoc\)([[:space:]]|$)' <<< "$signature"; then
                echo "unsigned macOS app must use only ad-hoc signing flags, without hardened runtime or library validation" >&2
                return 1
            fi
        fi
        /usr/bin/codesign --display --entitlements :- "$app" > "$temporary/entitlements.plist" 2>/dev/null
        python3 - "$temporary/entitlements.plist" "$component/app/packaging/macos-entitlements.plist" <<'PY'
import plistlib,sys
actual=plistlib.loads(open(sys.argv[1], "rb").read())
expected=plistlib.loads(open(sys.argv[2], "rb").read())
assert actual == expected, "downloaded app entitlements differ from the reviewed policy"
PY
        python3 - "$app/Contents/Info.plist" "$bundle_id" "$app_name" "$directory/build-info.json" "$bundle_name" <<'PY'
import json,plistlib,re,sys
from pathlib import Path
with open(sys.argv[1], "rb") as source:
    plist=plistlib.load(source)
info=json.load(open(sys.argv[4]))
assert plist["CFBundleIdentifier"] == sys.argv[2]
assert plist["CFBundleDisplayName"] == sys.argv[5]
assert plist["CFBundleName"] == sys.argv[3]
localized=Path(sys.argv[1]).parent / "Resources/en.lproj/InfoPlist.strings"
assert plistlib.loads(localized.read_bytes()) == {
    "CFBundleDisplayName": sys.argv[3], "CFBundleName": sys.argv[3],
}
assert plist["CFBundleExecutable"] == "maple-agent"
assert plist["CFBundleShortVersionString"] == info["version"].split("-",1)[0]
assert re.fullmatch(r"[1-9][0-9]*", plist["CFBundleVersion"])
assert plist["LSMinimumSystemVersion"] == "15.0"
assert plist.get("NSMicrophoneUsageDescription")
assert "LSEnvironment" not in plist
PY
        executable="$app/Contents/MacOS/maple-agent"
        [[ "$(/usr/bin/lipo -archs "$executable")" == arm64 ]]
        python3 "$component/scripts/macos-build-info.py" "$executable" --profile "$profile" \
            --source-sha "$expected_source" > "$temporary/build-info.json"
        cmp "$directory/build-info.json" "$temporary/build-info.json"
        if [[ "$static" == false ]]; then
            "$executable" --build-info > "$temporary/runtime-build-info.json" 2> "$temporary/runtime.stderr"
            cmp "$directory/build-info.json" "$temporary/runtime-build-info.json"
            "$executable" --version 2>> "$temporary/runtime.stderr"
            if grep -Fq 'is implemented in both' "$temporary/runtime.stderr"; then
                echo "release app loaded duplicate Swift runtimes" >&2; return 1
            fi
        fi
    }
    dmg_app="$mount/$bundle_name.app"
    verify_macos_app "$dmg_app"
    archives=("$directory"/*.app.tar.gz)
    [[ ${#archives[@]} == 1 && -f "${archives[0]}" ]] || { echo "expected exactly one app archive" >&2; exit 1; }
    python3 "$component/scripts/release-info.py" extract-app "$profile" "${archives[0]}" --output "$temporary/archive"
    archive_app="$temporary/archive/$bundle_name.app"
    verify_macos_app "$archive_app"
    python3 "$component/scripts/release-info.py" compare-apps "$profile" "$dmg_app" --output "$archive_app"
    if [[ "$static" == false ]]; then
        python3 "$component/scripts/smoke-macos-release.py" "$dmg_app"
    fi
else
    [[ "$static" == false ]] || { echo "static verification is only supported for macOS packages" >&2; exit 2; }
    [[ "$(uname -s)-$(uname -m)" == Linux-x86_64 ]] || { echo "Linux artifacts require x86_64 verification" >&2; exit 1; }
    # Parse and audit stored bundle paths/modes before executing the payload.
    # Extraction remains private and does not install into the user's app tree.
    python3 "$component/scripts/linux-release-bundle.py" extract "$directory/$package" \
        "$temporary/extracted" --profile "$profile"
    bundle_root=maple-agent.app
    [[ "$profile" == prod ]] || bundle_root=maple-agent-dev.app
    app="$temporary/extracted/$bundle_root"
    python3 "$component/scripts/linux-release-bundle.py" audit "$app"
    # The payload uses the host's libraries; verify-linux-bundle-docker.sh runs
    # AppRun on stock distributions. Here, check identity on the Nix runtime.
    "$component/scripts/run-linux-payload.sh" "$app/usr/bin/maple-agent" --build-info > "$temporary/build-info.json"
    cmp "$directory/build-info.json" "$temporary/build-info.json"
    "$component/scripts/run-linux-payload.sh" "$app/usr/bin/maple-agent" --version
fi
echo "verified ${profile} ${platform} release artifacts"
