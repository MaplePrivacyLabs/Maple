#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "debug-app is only available on macOS" >&2
    exit 1
fi

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
requested_bundle_dir="${MAPLE_DEBUG_APP_PATH:-$repo_root/target/debug/Maple GPUI Dev.app}"
binary_source="$repo_root/target/debug/maple-gpui"
codesign_identity="${MAPLE_DEBUG_CODESIGN_IDENTITY:--}"
bundle_id="${MAPLE_DEBUG_BUNDLE_ID:-cloud.opensecret.maple.gpui.dev}"

if [[ ! "$bundle_id" =~ ^[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)+$ ]]; then
    echo "MAPLE_DEBUG_BUNDLE_ID must be a dotted bundle identifier" >&2
    exit 1
fi

python_source="$repo_root/target/debug/runtime/python"
worker_smoke="$repo_root/target/debug/code-mode-smoke"

python3 "$repo_root/scripts/prepare-python.py" --distribution pbs
if [[ ! -x "$worker_smoke" ]]; then
    echo "CodeMode debug smoke binary missing; run 'just code-mode-smoke' first" >&2
    exit 1
fi

if [[ ! -x "$binary_source" ]]; then
    echo "debug binary not found at $binary_source; run 'just build' first" >&2
    exit 1
fi

case "$requested_bundle_dir" in
    /*) ;;
    *) requested_bundle_dir="$PWD/$requested_bundle_dir" ;;
esac

bundle_name="$(basename "$requested_bundle_dir")"
bundle_parent="$(dirname "$requested_bundle_dir")"
if [[ "$bundle_name" != *.app || "$bundle_name" == ".app" ]]; then
    echo "MAPLE_DEBUG_APP_PATH must name a specific .app bundle" >&2
    exit 1
fi

mkdir -p "$bundle_parent"
bundle_parent="$(cd "$bundle_parent" && pwd -P)"
bundle_dir="$bundle_parent/$bundle_name"

swift_stdlib_tool="$(/usr/bin/xcrun --find swift-stdlib-tool 2>/dev/null || true)"
if [[ ! -x "$swift_stdlib_tool" ]]; then
    echo "swift-stdlib-tool was not found; install or select a full Xcode toolchain" >&2
    exit 1
fi

staging_root="$(mktemp -d "$bundle_parent/.maple-debug-app.XXXXXX")"
trap 'rm -rf -- "$staging_root"' EXIT

staged_bundle="$staging_root/$bundle_name"
contents_dir="$staged_bundle/Contents"
frameworks_dir="$contents_dir/Frameworks"
binary_destination="$contents_dir/MacOS/maple-gpui"

mkdir -p "$contents_dir/MacOS" "$frameworks_dir" "$contents_dir/Resources"
cp -R "$python_source" "$contents_dir/Resources/python"
cp "$repo_root/app/macos/Info.plist" "$contents_dir/Info.plist"
python3 "$repo_root/scripts/macos-debug-plist.py" "$contents_dir/Info.plist"
cp "$binary_source" "$binary_destination"
chmod 0755 "$binary_destination"

# Native dependencies may link Swift compatibility libraries using @rpath.
# The final application binary owns its bundle layout. Prefer the operating
# system's Swift runtime on current macOS releases, then fall back to the
# compatibility libraries in the app for older systems. Reversing these paths
# can load two Swift runtimes when Apple frameworks select the system copy.
load_commands="$(/usr/bin/otool -l "$binary_destination")"
system_rpath_line="$(/usr/bin/awk '$1 == "path" && $2 == "/usr/lib/swift" { print NR; exit }' <<<"$load_commands")"
bundle_rpath_line="$(/usr/bin/awk '$1 == "path" && $2 == "@executable_path/../Frameworks" { print NR; exit }' <<<"$load_commands")"
if [[ -z "$system_rpath_line" || -z "$bundle_rpath_line" ]]; then
    echo "debug binary is missing its Swift runtime paths; rebuild Maple before staging" >&2
    exit 1
fi
if ((system_rpath_line >= bundle_rpath_line)); then
    echo "debug binary must prefer /usr/lib/swift before its bundled Swift runtime" >&2
    exit 1
fi

# Let Xcode discover the complete Swift runtime closure. Keeping this scan
# dependency-driven avoids a hard-coded list that drifts when native crates
# add, remove, or update their Swift bridges. Omit --sign here so current Xcode
# versions do not leave backup files in the bundle; sign every copied dylib in
# an explicit inside-out pass below.
"$swift_stdlib_tool" \
    --copy \
    --scan-executable "$binary_destination" \
    --platform macosx \
    --destination "$frameworks_dir"

while IFS= read -r -d '' runtime_library; do
    /usr/bin/codesign --force --sign "$codesign_identity" --timestamp=none \
        "$runtime_library"
done < <(/usr/bin/find "$frameworks_dir" -type f -name '*.dylib' -print0)

# Sign every regular Mach-O payload, including stdlib extension modules and
# libpython, before sealing the outer bundle. Symlink aliases share that code.
while IFS= read -r -d '' python_code; do
    if /usr/bin/file -b "$python_code" | /usr/bin/grep -q 'Mach-O'; then
        /usr/bin/codesign --force --sign "$codesign_identity" --timestamp=none \
            "$python_code"
    fi
done < <(/usr/bin/find "$contents_dir/Resources/python" -type f -print0)

# Rust's linker gives arm64 executables an ad hoc signature, but copying that
# executable into an app bundle does not bind Info.plist or seal the bundle.
# TCC would then record a grant that the relaunched app cannot satisfy. Sign
# nested code first and the complete development bundle last. The default `-`
# identity uses no keychain certificate and remains separate from release
# signing; developers can opt into a stable Apple Development identity.
/usr/bin/codesign --force --sign "$codesign_identity" --timestamp=none \
  --identifier "$bundle_id" \
  "$staged_bundle"
/usr/bin/codesign --verify --deep --strict --verbose=2 "$staged_bundle"

# Exercise dyld against the signed bundle before replacing the last working
# build. This catches missing compatibility libraries and rejects the duplicate
# Swift-runtime warning that can otherwise precede mysterious casting crashes.
smoke_stderr="$staging_root/version.stderr"
if ! "$binary_destination" --version >/dev/null 2>"$smoke_stderr"; then
    echo "signed debug bundle failed its launch smoke test:" >&2
    /bin/cat "$smoke_stderr" >&2
    exit 1
fi
if /usr/bin/grep -Fq "is implemented in both" "$smoke_stderr"; then
    echo "signed debug bundle loaded duplicate Swift runtimes:" >&2
    /bin/cat "$smoke_stderr" >&2
    exit 1
fi

# Execute the actual signed nested interpreter and native stdlib extensions.
"$contents_dir/Resources/python/bin/python3.13" -I -B -u -c \
    'import sys, sysconfig, ssl, sqlite3, ctypes, zlib, bz2, lzma; assert sys.version_info[:3] == (3, 13, 15); assert not sysconfig.get_config_var("Py_GIL_DISABLED")'
"$worker_smoke" --manifest "$contents_dir/Resources/python/runtime.json"

# Build and validate away from the destination so a failed packaging step
# leaves the developer's last working bundle untouched. Replace only the
# explicit, validated .app path once the new bundle is complete.
if [[ -e "$bundle_dir" || -L "$bundle_dir" ]]; then
    rm -rf -- "$bundle_dir"
fi
mv "$staged_bundle" "$bundle_dir"

echo "$bundle_dir"
