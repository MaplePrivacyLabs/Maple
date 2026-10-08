#!/usr/bin/env bash
# Manual, unsigned macOS host-architecture package. No upload or publication.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"
if [ "$(host_os)" != "darwin" ]; then
  echo "Research Dev desktop packaging currently supports macOS." >&2
  exit 1
fi
print_source_provenance
verify_rust_lockfile
install_frontend_deps
configure_sccache
use_pr_environment
# This profile is independent of the existing iOS Dev repository variable.
profile_exports="$(python3 "${SCRIPT_DIR}/research-dev-profile.py" environment)"
eval "${profile_exports}"
unset CARGO_TARGET_DIR MAPLE_IOS_VARIANT TAURI_CONFIG APPLE_SIGNING_IDENTITY TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PASSWORD
configure_reproducible_build_metadata
remove_generated_ios_cargo_config
build_frontend_dist
use_xcode_toolchain
prepare_macos_onnxruntime
export MACOSX_DEPLOYMENT_TARGET="13.4"
export CMAKE_OSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET}"
# Match Research's macOS PR/release linker setup: prefer Apple's SDK libraries
# over the Nix libiconv directory added by use_xcode_toolchain.
export SDKROOT="$(xcrun --sdk macosx --show-sdk-path)"
export LIBRARY_PATH="${SDKROOT}/usr/lib${LIBRARY_PATH:+:${LIBRARY_PATH}}"
export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }-Clink-arg=-isysroot -Clink-arg=${SDKROOT}"
cd "${FRONTEND_DIR}"
bun tauri build --debug --bundles app --no-sign --config src-tauri/tauri.desktop-dev.conf.json
verify_frontend_dist_unchanged
app_dir="${TAURI_DIR}/target/debug/bundle/macos/Maple Research Dev.app"
evidence_dir="${TAURI_DIR}/target/research-dev"
mkdir -p "${evidence_dir}"
python3 "${SCRIPT_DIR}/research-dev-profile.py" verify --bundle "${app_dir}" > "${evidence_dir}/build-profile.json"
COPYFILE_DISABLE=1 tar -czf "${evidence_dir}/maple-research-dev-macos.tar.gz" -C "$(dirname "${app_dir}")" "$(basename "${app_dir}")"
printf 'Verified Research Dev package: %s\nProfile evidence: %s\n' "${app_dir}" "${evidence_dir}/build-profile.json"
