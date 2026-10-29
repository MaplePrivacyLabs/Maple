#!/usr/bin/env bash
# Manual, unsigned host-architecture Dev package. No upload or publication.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"
case "$(host_os)" in
  linux | darwin) ;;
  *)
    echo "Research Dev desktop packaging supports macOS and Linux." >&2
    exit 1
    ;;
esac
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
if [ "$(host_os)" = "linux" ]; then
  # Keep intermediate packages and executables out of the production build tree.
  evidence_dir="${TAURI_DIR}/target/research-dev"
  export CARGO_TARGET_DIR="${evidence_dir}/cargo"
  mkdir -p "${evidence_dir}"
  prepare_linux_onnxruntime
  config="${evidence_dir}/tauri.linux.conf.json"
  python3 "${SCRIPT_DIR}/research-dev-profile.py" linux-config --ort-library "${ORT_DYLIB_PATH}" > "${config}"
  cd "${FRONTEND_DIR}"
  # The DEB is only an intermediate payload. Distribute the normalized RPM below.
  remove_build_tree "${CARGO_TARGET_DIR}/release/bundle/deb"
  bun tauri build --bundles deb --no-sign --config "${config}"
  debs=()
  while IFS= read -r -d '' deb; do
    debs+=("${deb}")
  done < <(find "${CARGO_TARGET_DIR}/release/bundle/deb" -type f -name '*.deb' -print0)
  if [ "${#debs[@]}" -ne 1 ]; then
    echo "Research Dev packaging requires exactly one Debian payload source." >&2
    exit 1
  fi
  name="$(jq -r '.mainBinaryName' "${config}")"
  rpm="${evidence_dir}/${name}-$(jq -r '.version' "${config}")-1.$(rpm_arch).rpm"
  payload="$(mktemp -d)"
  trap 'rm -rf "${payload}"' EXIT
  extract_deb_payload "${debs[0]}" "${payload}/staged"
  sanitize_linux_package_executable "${payload}/staged/usr/bin/${name}"
  python3 "${SCRIPT_DIR}/tauri_bundle_type.py" --expect deb --set rpm "${payload}/staged/usr/bin/${name}"
  python3 "${SCRIPT_DIR}/research-dev-profile.py" stage-linux --bundle "${payload}/staged" > /dev/null
  build_rpm_package_from_payload "${rpm}" "${payload}/staged" "${config}"
  if [ "$(rpm -qp --queryformat '%{NAME}' "${rpm}")" != "${name}" ]; then
    echo "Research Dev RPM has the wrong package name." >&2
    exit 1
  fi
  mkdir "${payload}/installed"
  # cpio may stop at the archive trailer before rpm2cpio finishes writing its
  # padding. Avoid a timing-dependent SIGPIPE under pipefail.
  rpm2cpio "${rpm}" > "${payload}/installed.cpio"
  (cd "${payload}/installed" && cpio -id --quiet --no-absolute-filenames < "${payload}/installed.cpio")
  # Do not use production's RPM verifier: its dependency policy is independent
  # of the Dev profile. Verify this package's executable and requirements here.
  verify_linux_package_executable_metadata "${payload}/installed/usr/bin/${name}"
  python3 "${SCRIPT_DIR}/tauri_bundle_type.py" --expect rpm "${payload}/installed/usr/bin/${name}"
  python3 "${SCRIPT_DIR}/research-dev-profile.py" verify-linux --bundle "${payload}/installed" --rpm "${rpm}" > "${evidence_dir}/build-profile.json"
  verify_frontend_dist_unchanged
  printf 'Verified Research Dev RPM: %s\nProfile evidence: %s\n' "${rpm}" "${evidence_dir}/build-profile.json"
  exit 0
fi
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
