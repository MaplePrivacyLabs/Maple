#!/usr/bin/env bash
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"

print_source_provenance
verify_rust_lockfile

install_frontend_deps
configure_sccache
case "${MAPLE_DESKTOP_WEB_ENVIRONMENT:-release}" in
  pr)
    use_pr_environment
    ;;
  release)
    use_release_environment
    ;;
  *)
    echo "Unsupported MAPLE_DESKTOP_WEB_ENVIRONMENT=${MAPLE_DESKTOP_WEB_ENVIRONMENT}. Expected pr or release." >&2
    exit 1
    ;;
esac
configure_reproducible_build_metadata
if [ "${MAPLE_TAURI_FAKE_UPDATER_SIGNING:-0}" = "1" ]; then
  generate_fake_tauri_updater_keypair
fi
configure_tauri_updater_signing_key
remove_generated_ios_cargo_config
build_frontend_dist

repro_dir="${TAURI_DIR}/target/reproducibility"
mkdir -p "${repro_dir}"

cd "${FRONTEND_DIR}"

case "$(host_os)" in
  linux)
    prepare_linux_onnxruntime
    export APPIMAGE_EXTRACT_AND_RUN="${APPIMAGE_EXTRACT_AND_RUN:-1}"
    export NO_STRIP="${NO_STRIP:-true}"
    prepend_linux_runtime_library_path
    prepare_tauri_linuxdeploy_tools_cache
    verify_linuxdeploy_plugin_metadata
    run_with_nix_usr_bin "${TAURI_DIR}/target/.tauri/linuxdeploy-$(linuxdeploy_tools_arch).AppImage" --appimage-extract-and-run --list-plugins

    remove_build_tree "${TAURI_DIR}/target/release/bundle/appimage"
    remove_build_tree "${TAURI_DIR}/target/release/bundle/deb"
    remove_build_tree "${TAURI_DIR}/target/release/bundle/rpm"

    release_config="$(linux_tauri_release_config)"
    run_with_nix_usr_bin pkg-config --modversion glib-2.0
    (cd "${TAURI_DIR}" && cargo build --locked --bins --features tauri/custom-protocol --release)
    sanitize_linux_target_release_executable
    run_with_nix_usr_bin bun tauri build --verbose --config "${release_config}"
    restore_linux_runtime_library_path
    normalize_linux_desktop_packages

    normalized_linux_packages=()
    while IFS= read -r -d '' file; do
      normalized_linux_packages+=("${file}")
    done < <(find "${TAURI_DIR}/target/release/bundle" -type f \( -name '*.deb' -o -name '*.rpm' \) -print0 | LC_ALL=C sort -z)
    sign_tauri_updater_artifacts "${normalized_linux_packages[@]}"

    desktop_artifacts=()
    while IFS= read -r -d '' file; do
      desktop_artifacts+=("${file}")
    done < <(find "${TAURI_DIR}/target/release/bundle" -type f \( \
      -name 'Maple_*.AppImage' -o \
      -name 'Maple_*.AppImage.sig' -o \
      -name '*.deb' -o \
      -name '*.deb.sig' -o \
      -name '*.rpm' -o \
      -name '*.rpm.sig' \
    \) -print0 | LC_ALL=C sort -z)

    verify_linux_desktop_package_metadata "${desktop_artifacts[@]}"
    verify_tauri_updater_signature_files "${desktop_artifacts[@]}"
    write_sha256_manifest "${repro_dir}/desktop-release-linux-final.sha256" "${desktop_artifacts[@]}"
    print_file_hashes "${desktop_artifacts[@]}"
    print_file_hashes "${TAURI_DIR}/target/release/maple"
    verify_frontend_dist_unchanged
    ;;
  darwin)
    use_xcode_toolchain
    import_apple_developer_certificate
    prepare_macos_onnxruntime

    export MACOSX_DEPLOYMENT_TARGET="13.4"
    export CMAKE_OSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET}"
    export SDKROOT="${SDKROOT:-$(xcrun --sdk macosx --show-sdk-path)}"
    export LIBRARY_PATH="${SDKROOT}/usr/lib${LIBRARY_PATH:+:${LIBRARY_PATH}}"
    export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }-Clink-arg=-isysroot -Clink-arg=${SDKROOT}"

    unsigned_config='{"build":{"beforeBuildCommand":null},"bundle":{"createUpdaterArtifacts":false,"macOS":{"minimumSystemVersion":"13.4"}}}'
    signed_config="$(jq -cn --arg updaterPubkey "$(tauri_updater_public_key_config_value)" '{
      build: {
        beforeBuildCommand: null
      },
      plugins: {
        updater: {
          pubkey: $updaterPubkey
        }
      },
      bundle: {
        createUpdaterArtifacts: true,
        macOS: {
          minimumSystemVersion: "13.4"
        }
      }
    }')"

    bun tauri build --target universal-apple-darwin --no-sign --config "${unsigned_config}"
    unsigned_app="${TAURI_DIR}/target/universal-apple-darwin/release/bundle/macos/Maple.app"
    unsigned_hash="$(print_canonical_apple_bundle_hash "${unsigned_app}" "apps/maple-research/frontend/src-tauri/target/universal-apple-darwin/release/bundle/macos/Maple.app" | tee "${repro_dir}/desktop-release-macos-unsigned.sha256" | awk '{ print $2 }')"
    cat "${repro_dir}/desktop-release-macos-unsigned.sha256"

    remove_build_tree "${TAURI_DIR}/target/universal-apple-darwin/release/bundle/dmg"
    remove_build_tree "${TAURI_DIR}/target/universal-apple-darwin/release/bundle/macos"

    prepare_macos_password_provisioning_profile
    signed_path="${PATH}"
    if [ -n "${MAPLE_MACOS_PROVISIONING_PROFILE:-}" ]; then
      signed_path="${REPO_ROOT}/scripts/ci/macos-codesign-wrapper:${PATH}"
    fi
    PATH="${signed_path}" bun tauri build --target universal-apple-darwin --config "${signed_config}"

    signed_app="${TAURI_DIR}/target/universal-apple-darwin/release/bundle/macos/Maple.app"
    if [ -n "${MAPLE_MACOS_PROVISIONING_PROFILE:-}" ] && [ ! -f "${signed_app}/Contents/embedded.provisionprofile" ]; then
      echo "Signed Maple.app is missing Contents/embedded.provisionprofile." >&2
      exit 1
    fi
    signed_canonical_hash="$(print_canonical_apple_bundle_hash "${signed_app}" "apps/maple-research/frontend/src-tauri/target/universal-apple-darwin/release/bundle/macos/Maple.app" | tee "${repro_dir}/desktop-release-macos-signed-canonical.sha256" | awk '{ print $2 }')"
    cat "${repro_dir}/desktop-release-macos-signed-canonical.sha256"

    if [ "${signed_canonical_hash}" != "${unsigned_hash}" ]; then
      echo "Signed macOS app does not strip back to the unsigned app tree." >&2
      echo "unsigned=${unsigned_hash}" >&2
      echo "signed_canonical=${signed_canonical_hash}" >&2
      exit 1
    fi
    printf 'verified-macos-signed-app  %s  %s\n' "${signed_canonical_hash}" "$(repo_relative_path "${signed_app}")"

    desktop_artifacts=()
    while IFS= read -r -d '' file; do
      desktop_artifacts+=("${file}")
    done < <(find "${TAURI_DIR}/target/universal-apple-darwin/release/bundle" -type f \( \
      -name '*.dmg' -o \
      -name '*.app.tar.gz' -o \
      -name '*.app.tar.gz.sig' \
    \) -print0 | LC_ALL=C sort -z)

    : > "${repro_dir}/desktop-release-macos-container-canonical.sha256"
    for artifact in "${desktop_artifacts[@]}"; do
      case "${artifact}" in
        *.app.tar.gz)
          artifact_canonical_hash="$(print_canonical_app_tar_payload_hash "${artifact}" "$(repo_relative_path "${artifact}")" | tee -a "${repro_dir}/desktop-release-macos-container-canonical.sha256" | awk '{ print $2 }')"
          ;;
        *.dmg)
          artifact_canonical_hash="$(print_canonical_dmg_app_hash "${artifact}" "$(repo_relative_path "${artifact}")" | tee -a "${repro_dir}/desktop-release-macos-container-canonical.sha256" | awk '{ print $2 }')"
          ;;
        *)
          continue
          ;;
      esac

      if [ "${artifact_canonical_hash}" != "${unsigned_hash}" ]; then
        echo "macOS release container does not strip back to the unsigned app tree." >&2
        echo "unsigned=${unsigned_hash}" >&2
        echo "container_canonical=${artifact_canonical_hash}" >&2
        echo "artifact=$(repo_relative_path "${artifact}")" >&2
        exit 1
      fi

      printf 'verified-macos-container-payload  %s  %s\n' "${artifact_canonical_hash}" "$(repo_relative_path "${artifact}")"
    done
    test -s "${repro_dir}/desktop-release-macos-container-canonical.sha256"

    verify_tauri_updater_signature_files "${desktop_artifacts[@]}"
    write_sha256_manifest "${repro_dir}/desktop-release-macos-final.sha256" "${desktop_artifacts[@]}"
    print_file_hashes "${desktop_artifacts[@]}"
    verify_frontend_dist_unchanged
    ;;
  *)
    echo "Unsupported desktop release build host: $(uname -s)" >&2
    exit 1
    ;;
esac
