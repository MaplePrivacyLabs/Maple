#!/usr/bin/env bash
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"

print_source_provenance
verify_rust_lockfile

if [ "$(host_os)" != "darwin" ]; then
  echo "iOS builds must run on macOS." >&2
  exit 1
fi

command -v xcodebuild >/dev/null 2>&1 || {
  echo "xcodebuild is required for iOS builds." >&2
  exit 1
}

use_xcode_toolchain
# Nix's macOS libiconv cannot be linked into an iOS target. The Xcode SDK
# supplies the target-appropriate system libraries.
unset LIBRARY_PATH
if ! require_ios_simulator_runtime_for_xcode; then
  if [ "${MAPLE_IOS_DOWNLOAD_SIMULATOR_RUNTIME:-0}" = "1" ]; then
    xcodebuild -downloadPlatform iOS
    require_ios_simulator_runtime_for_xcode
  else
    exit 1
  fi
fi
install_frontend_deps
use_pr_environment
configure_reproducible_build_metadata
build_frontend_dist

ios_project_state_dir=""
ios_info_plist_present=0
ios_entitlements_present=0
restore_ios_build_state() {
  remove_generated_ios_cargo_config

  if [ -n "${ios_project_state_dir}" ] && [ -d "${ios_project_state_dir}" ]; then
    if [ -f "${ios_project_state_dir}/Info.plist" ]; then
      cp "${ios_project_state_dir}/Info.plist" "${TAURI_DIR}/gen/apple/maple_iOS/Info.plist"
    elif [ "${ios_info_plist_present}" = "0" ]; then
      rm -f "${TAURI_DIR}/gen/apple/maple_iOS/Info.plist"
    fi
    if [ -f "${ios_project_state_dir}/maple_iOS.entitlements" ]; then
      cp "${ios_project_state_dir}/maple_iOS.entitlements" "${TAURI_DIR}/gen/apple/maple_iOS/maple_iOS.entitlements"
    elif [ "${ios_entitlements_present}" = "0" ]; then
      rm -f "${TAURI_DIR}/gen/apple/maple_iOS/maple_iOS.entitlements"
    fi
    rm -rf "${ios_project_state_dir}"
  fi
}

cd "${TAURI_DIR}"
ios_project_state_dir="$(mktemp -d)"
trap restore_ios_build_state EXIT
if [ -f "${TAURI_DIR}/gen/apple/maple_iOS/Info.plist" ]; then
  ios_info_plist_present=1
  cp "${TAURI_DIR}/gen/apple/maple_iOS/Info.plist" "${ios_project_state_dir}/Info.plist"
fi
if [ -f "${TAURI_DIR}/gen/apple/maple_iOS/maple_iOS.entitlements" ]; then
  ios_entitlements_present=1
  cp "${TAURI_DIR}/gen/apple/maple_iOS/maple_iOS.entitlements" "${ios_project_state_dir}/maple_iOS.entitlements"
fi

verify_ios_onnxruntime_manifest
print_ios_onnxruntime_hashes
./scripts/setup-ios-cargo-config.sh

export ORT_LIB_LOCATION="${TAURI_DIR}/onnxruntime-ios/onnxruntime.xcframework/ios-arm64-simulator"
export ORT_SKIP_DOWNLOAD="true"
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-16.0}"
rm -rf "${TAURI_DIR}/gen/apple/build/arm64-sim" "${TAURI_DIR}/gen/apple/build/maple_iOS.xcarchive"

cd "${FRONTEND_DIR}"
bun tauri ios build --debug --target aarch64-sim --ci --config '{"build":{"beforeBuildCommand":null}}'
require_ios_scene_manifest "${TAURI_DIR}/gen/apple/build/arm64-sim/Maple.app"
strip_apple_debug_symbols "${TAURI_DIR}/gen/apple/build/arm64-sim/Maple.app"
adhoc_resign_apple_bundle "${TAURI_DIR}/gen/apple/build/arm64-sim/Maple.app"
scrub_host_metadata_files "${TAURI_DIR}/gen/apple/build/arm64-sim/Maple.app"
repro_dir="${TAURI_DIR}/target/reproducibility"
mkdir -p "${repro_dir}"
{
  print_tree_hash "${TAURI_DIR}/gen/apple/build/arm64-sim/Maple.app" "apps/maple-research/frontend/src-tauri/gen/apple/build/arm64-sim/Maple.app"
  print_canonical_ios_app_hash "${TAURI_DIR}/gen/apple/build/arm64-sim/Maple.app" "apps/maple-research/frontend/src-tauri/gen/apple/build/arm64-sim/Maple.app"
} | tee "${repro_dir}/ios-pr-final.sha256"
verify_frontend_dist_unchanged
