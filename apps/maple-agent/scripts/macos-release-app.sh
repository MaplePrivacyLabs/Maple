#!/usr/bin/env bash
# Native distribution packaging; this helper never builds source or publishes.
set +x
set -euo pipefail
umask 077

# Absolute native tools preserve the Xcode/keychain boundary. Tests can source
# this file and replace this function without exposing a production test switch.
macos_native() {
    local tool="$1"
    shift
    case "$tool" in
        spctl) /usr/sbin/spctl "$@" ;;
        /*) "$tool" "$@" ;;
        *) "/usr/bin/$tool" "$@" ;;
    esac
}

macos_embed_dylibs() {
    local component="$1" binary="$2" app="$3"
    python3 "$component/scripts/macos-release-dylibs.py" "$binary" "$app"
}

macos_release_cleanup() {
    local status=$?
    trap - EXIT INT TERM HUP
    if [[ "${macos_release_restore_search_list:-false}" == true ]]; then
        if ! macos_native security list-keychains -d user -s \
            ${macos_release_search_list[@]+"${macos_release_search_list[@]}"} >/dev/null; then
            echo "failed to restore the original macOS keychain search list" >&2
            [[ "$status" != 0 ]] || status=1
        fi
    fi
    if [[ -n "${macos_release_keychain:-}" ]]; then
        macos_native security delete-keychain "$macos_release_keychain" >/dev/null 2>&1 || true
    fi
    if [[ -n "${macos_release_mount:-}" ]]; then
        macos_native hdiutil detach "$macos_release_mount" -quiet >/dev/null 2>&1 || true
    fi
    [[ -z "${macos_release_staging:-}" ]] || rm -rf -- "$macos_release_staging"
    exit "$status"
}

macos_release_main() {
    if [[ $# -lt 3 || $# -gt 4 || ( $# == 4 && "$4" != --unsigned ) ]]; then
        echo "usage: macos-release-app.sh BINARY ARTIFACT_DIRECTORY BASENAME [--unsigned]" >&2
        return 2
    fi
    local binary="$1" output_dir="$2" basename="$3" unsigned=false
    [[ $# != 4 ]] || unsigned=true
    [[ "$basename" =~ ^[A-Za-z0-9._-]+$ ]] || { echo "invalid package basename" >&2; return 1; }
    [[ "$(uname -s)-$(uname -m)" == Darwin-arm64 ]] || { echo "macOS release packaging requires Apple Silicon" >&2; return 1; }
    local component
    component="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    for name in APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD APPLE_ID APPLE_ID_PASSWORD APPLE_PASSWORD APPLE_TEAM_ID; do
        export -n "$name" 2>/dev/null || true
    done
    # Exercise Gatekeeper's native tool in unsigned previews too, before any
    # credential setup. Reading its status does not change assessment policy.
    macos_native spctl --status >/dev/null
    [[ -x "$binary" && -f "$output_dir/build-info.json" ]] || { echo "missing release binary/build-info" >&2; return 1; }
    local app_name="${MAPLE_PACKAGE_APP_NAME:?}" bundle_id="${MAPLE_PACKAGE_BUNDLE_ID:?}"
    local profile="${MAPLE_PACKAGE_CHANNEL:?}" build_number="${MAPLE_PACKAGE_BUILD_NUMBER:-1}"
    local arches
    arches="$(macos_native lipo -archs "$binary")"
    [[ "$arches" == arm64 ]] || { echo "release binary must contain exactly the arm64 slice" >&2; return 1; }

    macos_release_staging="$(mktemp -d "$output_dir/.macos-release.XXXXXX")"
    macos_release_keychain=""
    macos_release_search_list=()
    macos_release_restore_search_list=false
    macos_release_mount=""
    trap macos_release_cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    trap 'exit 129' HUP
    local app="$macos_release_staging/$app_name.app"
    local frameworks="$app/Contents/Frameworks" executable="$app/Contents/MacOS/maple-agent"
    mkdir -p "$frameworks" "$app/Contents/MacOS" "$app/Contents/Resources"
    cp "$binary" "$executable"
    chmod 0755 "$executable"
    local icon=maple-agent.icns
    [[ "$profile" != dev ]] || icon=maple-agent-dev.icns
    cp "$component/app/packaging/$icon" "$app/Contents/Resources/MapleAgent.icns"
    python3 "$component/scripts/release-info.py" plist "$profile" "$output_dir/build-info.json" \
        --build-number "$build_number" --output "$app/Contents/Info.plist"
    macos_embed_dylibs "$component" "$binary" "$app"

    local commands system_rpath bundle_rpath swift_tool
    commands="$(macos_native otool -l "$executable")"
    system_rpath="$(awk '$1 == "path" && $2 == "/usr/lib/swift" {print NR; exit}' <<<"$commands")"
    bundle_rpath="$(awk '$1 == "path" && $2 == "@executable_path/../Frameworks" {print NR; exit}' <<<"$commands")"
    if [[ -z "$system_rpath" || -z "$bundle_rpath" ]] || ((system_rpath >= bundle_rpath)); then
        echo "release binary must prefer the system Swift runtime before its bundle fallback" >&2
        return 1
    fi
    # A Nix dylib must never escape into the distribution package.
    if macos_native otool -L "$executable" | tail -n +2 | grep -Eq '/nix/store/|/home/runner/work/|/Users/runner/work/'; then
        echo "release binary contains a build-host runtime library path" >&2
        return 1
    fi
    swift_tool="$(macos_native xcrun --find swift-stdlib-tool)"
    macos_native "$swift_tool" --copy --scan-executable "$executable" --scan-folder "$frameworks" \
        --platform macosx --destination "$frameworks"
    # Public bundle contents must remain readable/executable after a CI runner's
    # private umask and ownership are encoded into the DMG and tar archive.
    chmod -R a+rX "$app"

    local identity="-" timestamp=--timestamp=none
    if [[ "$unsigned" == false ]]; then
        [[ -n "${APPLE_CERTIFICATE:-}" && -n "${APPLE_CERTIFICATE_PASSWORD:-}" \
           && -n "${APPLE_ID:-}" && -n "${APPLE_ID_PASSWORD:-}" && -n "${APPLE_TEAM_ID:-}" ]] || {
            echo "signed macOS packaging requires the desktop-signing credentials" >&2; return 1;
        }
        local certificate="$macos_release_staging/signing.p12" keychain_password cert_info
        macos_release_keychain="$macos_release_staging/signing.keychain-db"
        # codesign still consults the user's search list for identity/chain
        # resolution, even with --keychain. Private keychain creation does not
        # add it there. Preserve every existing quoted path and the default.
        local search_list search_path
        search_list="$(macos_native security list-keychains -d user)"
        printf '%s\n' "$search_list" | python3 -c '
import sys
for line in sys.stdin:
    value=line.strip()
    if not value: continue
    if len(value) < 3 or value[0] != "\"" or value[-1] != "\"" or not value[1:-1].startswith("/") or "\0" in value:
        raise SystemExit("invalid quoted macOS keychain search-list path")
    path=value[1:-1]
    sys.stdout.buffer.write(path.encode()+b"\0")' > "$macos_release_staging/search-list.paths"
        while IFS= read -r -d '' search_path; do
            macos_release_search_list+=("$search_path")
        done < "$macos_release_staging/search-list.paths"
        # Set this before any keychain operation: a failed mutation must also
        # restore the snapshot, including failures and signals during setup.
        macos_release_restore_search_list=true
        keychain_password="$(python3 -c 'import secrets; print(secrets.token_hex(24))')"
        printf '%s' "$APPLE_CERTIFICATE" | macos_native base64 -D > "$certificate"
        macos_native security create-keychain -p "$keychain_password" "$macos_release_keychain" >/dev/null
        macos_native security list-keychains -d user -s \
            ${macos_release_search_list[@]+"${macos_release_search_list[@]}"} \
            "$macos_release_keychain" >/dev/null
        macos_native security unlock-keychain -p "$keychain_password" "$macos_release_keychain" >/dev/null
        # The job owns this temporary keychain and deletes it on every exit.
        # Do not lock it during a long notarization wait; the password is discarded.
        macos_native security set-keychain-settings "$macos_release_keychain" >/dev/null
        macos_native security import "$certificate" -k "$macos_release_keychain" \
            -P "$APPLE_CERTIFICATE_PASSWORD" -T /usr/bin/codesign >/dev/null
        macos_native security set-key-partition-list -S apple-tool:,apple:,codesign: -s \
            -k "$keychain_password" "$macos_release_keychain" >/dev/null
        rm -f "$certificate"
        unset APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD keychain_password
        cert_info="$(macos_native security find-identity -v -p codesigning "$macos_release_keychain")"
        identity="$(printf '%s\n' "$cert_info" | python3 -c '
import re,sys
team=sys.argv[1]
matches=[m.group(1) for line in sys.stdin for m in [re.search(r"\b([0-9A-Fa-f]{40})\s+\"Developer ID Application:.*\("+re.escape(team)+r"\)\"",line)] if m]
if len(matches) != 1: raise SystemExit("expected exactly one Developer ID Application identity for the configured team")
print(matches[0])' "$APPLE_TEAM_ID")"
        timestamp=--timestamp
    fi
    local keychain_args=()
    [[ -z "$macos_release_keychain" ]] || keychain_args=(--keychain "$macos_release_keychain")
    local library
    while IFS= read -r -d '' library; do
        if macos_native otool -L "$library" | tail -n +2 | grep -Eq '/nix/store/|/home/runner/work/|/Users/runner/work/'; then
            echo "bundled library contains a build-host runtime path" >&2; return 1
        fi
        macos_native codesign --force --sign "$identity" "$timestamp" --options runtime \
            "${keychain_args[@]}" "$library"
    done < <(find "$frameworks" -type f -name '*.dylib' -print0)
    macos_native codesign --force --sign "$identity" "$timestamp" --options runtime \
        --identifier "$bundle_id" --entitlements "$component/app/packaging/macos-entitlements.plist" \
        "${keychain_args[@]}" "$app"
    macos_native codesign --verify --deep --strict --verbose=2 "$app"
    # Verify immutable embedded identity as data. Runtime/Swift probes belong
    # exclusively to the later credential-free downloaded-artifact job.
    python3 "$component/scripts/macos-build-info.py" "$executable" --profile "$profile" \
        --source-sha "$(python3 "$component/scripts/release-info.py" field "$profile" "$output_dir/build-info.json" --field source_sha)" \
        > "$macos_release_staging/static-build-info.json"
    cmp "$output_dir/build-info.json" "$macos_release_staging/static-build-info.json"

    if [[ "$unsigned" == false ]]; then
        # Notary credentials live only in this temporary keychain. The default
        # keychain is unchanged and cleanup restores its original search list.
        macos_native xcrun notarytool store-credentials maple-agent-release \
            --keychain "$macos_release_keychain" --apple-id "$APPLE_ID" \
            --team-id "$APPLE_TEAM_ID" --password "$APPLE_ID_PASSWORD" >/dev/null
        unset APPLE_ID APPLE_ID_PASSWORD APPLE_TEAM_ID
        macos_native ditto -c -k --keepParent "$app" "$macos_release_staging/notarize.zip"
        macos_native xcrun notarytool submit "$macos_release_staging/notarize.zip" \
            --keychain "$macos_release_keychain" --keychain-profile maple-agent-release \
            --wait --output-format json > "$macos_release_staging/notary.json"
        python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d.get("status") == "Accepted", "notarization was not accepted"' "$macos_release_staging/notary.json"
        local submission
        submission="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$macos_release_staging/notary.json")"
        macos_native xcrun notarytool log "$submission" --keychain "$macos_release_keychain" \
            --keychain-profile maple-agent-release "$output_dir/notarization-log.json"
        python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d.get("status") == "Accepted" and not d.get("issues"), "notarization log contains issues"' "$output_dir/notarization-log.json"
        macos_native xcrun stapler staple "$app"
        macos_native xcrun stapler validate "$app"
        macos_native spctl --assess --type execute --verbose=2 "$app"
    fi
    # Stapling can create a new ticket file after signing under the private
    # process umask. Make every final public app member readable again.
    chmod -R a+rX "$app"

    local image_root="$macos_release_staging/image"
    mkdir "$image_root"
    chmod 0755 "$image_root"
    macos_native ditto "$app" "$image_root/$app_name.app"
    ln -s /Applications "$image_root/Applications"
    # hdiutil's own -srcfolder size estimate can come up short and fail with
    # "No space left on device". Size the image from the payload with headroom,
    # as create-dmg and Tauri do, and report free space if creation still fails.
    local payload_mb image_mb
    payload_mb="$(python3 -c '
import os,sys
total=0
for root,dirs,files in os.walk(sys.argv[1]):
    for name in dirs+files:
        total+=os.lstat(os.path.join(root,name)).st_size
print(-(-total//(1024*1024)))' "$image_root")"
    image_mb=$((payload_mb + payload_mb / 5 + 64))
    if ! macos_native hdiutil create -volname "$app_name" -srcfolder "$image_root" \
        -fs HFS+ -size "${image_mb}m" -format UDZO -ov "$output_dir/$basename.dmg" >/dev/null; then
        echo "hdiutil could not create a ${image_mb} MB disk image for ${payload_mb} MB of content" >&2
        macos_native /bin/df -h "$output_dir" "${TMPDIR:-/tmp}" >&2 || true
        return 1
    fi
    macos_native codesign --force --sign "$identity" "$timestamp" "${keychain_args[@]}" "$output_dir/$basename.dmg"
    if [[ "$unsigned" == false ]]; then
        macos_native xcrun notarytool submit "$output_dir/$basename.dmg" \
            --keychain "$macos_release_keychain" --keychain-profile maple-agent-release \
            --wait --output-format json > "$macos_release_staging/dmg-notary.json"
        python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d.get("status") == "Accepted", "DMG notarization was not accepted"' "$macos_release_staging/dmg-notary.json"
        submission="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$macos_release_staging/dmg-notary.json")"
        macos_native xcrun notarytool log "$submission" --keychain "$macos_release_keychain" \
            --keychain-profile maple-agent-release "$output_dir/dmg-notarization-log.json"
        python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d.get("status") == "Accepted" and not d.get("issues"), "DMG notarization log contains issues"' "$output_dir/dmg-notarization-log.json"
        macos_native xcrun stapler staple "$output_dir/$basename.dmg"
        macos_native xcrun stapler validate "$output_dir/$basename.dmg"
        macos_native spctl --assess --type open --context context:primary-signature --verbose=2 "$output_dir/$basename.dmg"
    fi
    COPYFILE_DISABLE=1 macos_native tar -czf "$output_dir/$basename.app.tar.gz" -C "$macos_release_staging" "$app_name.app"
    chmod 0644 "$output_dir/$basename.dmg" "$output_dir/$basename.app.tar.gz"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    macos_release_main "$@"
fi
