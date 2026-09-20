#!/usr/bin/env bash
set -euo pipefail

# Run from the Maple root through: nix develop --no-update-lock-file .#apple -c <this script>
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../ci" && pwd)/_common.sh"
use_xcode_toolchain
unset LIBRARY_PATH
require_ios_simulator_runtime_for_xcode
verify_ios_onnxruntime_manifest
verify_rust_lockfile

# Tauri regenerates these files even for simulator builds. Restore the caller's
# exact starting bytes, including any uncommitted edits, on every exit.
storekit_build_state="$(mktemp -d)"
for file in Info.plist maple_iOS.entitlements; do
  if [[ -f "${TAURI_DIR}/gen/apple/maple_iOS/${file}" ]]; then
    cp "${TAURI_DIR}/gen/apple/maple_iOS/${file}" "${storekit_build_state}/${file}"
  fi
done
restore_storekit_build_state() {
  for file in Info.plist maple_iOS.entitlements; do
    if [[ -f "${storekit_build_state}/${file}" ]]; then
      cp "${storekit_build_state}/${file}" "${TAURI_DIR}/gen/apple/maple_iOS/${file}"
    fi
  done
  rm -rf "${storekit_build_state}"
}
trap restore_storekit_build_state EXIT

(cd "${TAURI_DIR}" && ./scripts/setup-ios-cargo-config.sh)
export ORT_LIB_LOCATION="${TAURI_DIR}/onnxruntime-ios/onnxruntime.xcframework/ios-arm64-simulator"
export ORT_SKIP_DOWNLOAD=true IPHONEOS_DEPLOYMENT_TARGET=16.0
export VITE_STOREKIT_EXPERIMENT=1
cd "${FRONTEND_DIR}"
bun run build
rm -rf "${TAURI_DIR}/gen/apple/build/arm64-sim" "${TAURI_DIR}/gen/apple/build/maple_iOS.xcarchive"
bun tauri ios build --debug --target aarch64-sim --ci --config '{"build":{"beforeBuildCommand":null}}'
python3 "${REPO_ROOT}/scripts/testing/storekit/prepare-simulator-products.py"
python3 "${REPO_ROOT}/scripts/testing/storekit/prepare-xcode.py"
