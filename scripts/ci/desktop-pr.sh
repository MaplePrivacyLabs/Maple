#!/usr/bin/env bash
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"

print_source_provenance
verify_rust_lockfile

install_frontend_deps
configure_sccache
use_pr_environment
configure_reproducible_build_metadata
remove_generated_ios_cargo_config
build_frontend_dist

cd "${FRONTEND_DIR}"

case "$(host_os)" in
  linux)
    prepare_linux_onnxruntime
    export APPIMAGE_EXTRACT_AND_RUN="${APPIMAGE_EXTRACT_AND_RUN:-1}"
    export NO_STRIP="${NO_STRIP:-true}"
    prepend_linux_runtime_library_path
    prepare_tauri_linuxdeploy_tools_cache
    verify_linuxdeploy_plugin_metadata
    run_with_nix_usr_bin "$(tauri_linuxdeploy_wrapper)" --appimage-extract-and-run --list-plugins
    run_with_nix_usr_bin pkg-config --modversion glib-2.0
    remove_build_tree "${TAURI_DIR}/target/release/bundle/appimage"
    remove_build_tree "${TAURI_DIR}/target/release/bundle/deb"
    remove_build_tree "${TAURI_DIR}/target/release/bundle/rpm"
    (cd "${TAURI_DIR}" && cargo build --locked --bins --features tauri/custom-protocol --release)
    sanitize_linux_target_release_executable
    run_with_nix_usr_bin bun tauri build --verbose --no-sign --config "$(linux_tauri_pr_config)"
    verify_tauri_used_pinned_linuxdeploy
    restore_linux_runtime_library_path
    normalize_linux_desktop_packages

    desktop_artifacts=()
    while IFS= read -r -d '' file; do
      desktop_artifacts+=("${file}")
    done < <(find "${TAURI_DIR}/target/release/bundle" -type f \( -name 'Maple_*.AppImage' -o -name '*.deb' -o -name '*.rpm' \) -print0 | LC_ALL=C sort -z)
    verify_linux_desktop_package_metadata "${desktop_artifacts[@]}"

    repro_dir="${TAURI_DIR}/target/reproducibility"
    write_sha256_manifest "${repro_dir}/desktop-pr-linux-final.sha256" "${desktop_artifacts[@]}" "${TAURI_DIR}/target/release/maple"
    print_file_hashes "${desktop_artifacts[@]}"
    print_file_hashes "${TAURI_DIR}/target/release/maple"

    if [ "${MAPLE_TAURI_FAKE_UPDATER_SIGNING:-0}" = "1" ]; then
      generate_fake_tauri_updater_keypair
      fake_public_key="${repro_dir}/desktop-pr-linux-fake-updater.pub"
      cp "${MAPLE_TAURI_UPDATER_PUBLIC_KEY_PATH}" "${fake_public_key}"
      write_sha256_manifest "${repro_dir}/desktop-pr-linux-fake-updater-public-key.sha256" "${fake_public_key}"

      sign_tauri_updater_artifacts "${desktop_artifacts[@]}"

      fake_signature_artifacts=()
      while IFS= read -r -d '' file; do
        fake_signature_artifacts+=("${file}")
      done < <(find "${TAURI_DIR}/target/release/bundle" -type f \( -name '*.AppImage.sig' -o -name '*.deb.sig' -o -name '*.rpm.sig' \) -print0 | LC_ALL=C sort -z)

      if [ "${#fake_signature_artifacts[@]}" -eq 0 ]; then
        echo "No fake Linux updater signature artifacts were created." >&2
        exit 1
      fi

      verify_tauri_updater_signature_files "${fake_signature_artifacts[@]}"
      write_sha256_manifest "${repro_dir}/desktop-pr-linux-fake-signing-final.sha256" "${desktop_artifacts[@]}" "${fake_signature_artifacts[@]}"
      printf 'verified-linux-fake-updater-signatures  %s\n' "${#fake_signature_artifacts[@]}"
    fi

    verify_frontend_dist_unchanged
    ;;
  darwin)
    use_xcode_toolchain
    "$(python3_runner)" "${REPO_ROOT}/scripts/ci/test-canonical-macho.py"
    prepare_macos_onnxruntime
    export MACOSX_DEPLOYMENT_TARGET="13.4"
    export CMAKE_OSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET}"
    export SDKROOT="${SDKROOT:-$(xcrun --sdk macosx --show-sdk-path)}"
    export LIBRARY_PATH="${SDKROOT}/usr/lib${LIBRARY_PATH:+:${LIBRARY_PATH}}"
    export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }-Clink-arg=-isysroot -Clink-arg=${SDKROOT}"
    # Same DMG step as the release build (scripts/ci/desktop-release.sh), unsigned.
    bun tauri build --target universal-apple-darwin --bundles app --no-sign --config '{"build":{"beforeBuildCommand":null},"bundle":{"createUpdaterArtifacts":false,"macOS":{"minimumSystemVersion":"13.4"}}}'
    product_name="$(jq -er '.productName' "${TAURI_DIR}/tauri.conf.json")"
    app_version="$(jq -er '.version' "${TAURI_DIR}/tauri.conf.json")"
    APPLE_SIGNING_IDENTITY="" build_macos_dmg "${TAURI_DIR}/target/universal-apple-darwin/release/bundle/macos/${product_name}.app" "${TAURI_DIR}/target/universal-apple-darwin/release/bundle/dmg/${product_name}_${app_version}_universal.dmg" "${product_name}"

    desktop_artifacts=()
    while IFS= read -r -d '' file; do
      desktop_artifacts+=("${file}")
    done < <(find "${TAURI_DIR}/target/universal-apple-darwin/release/bundle" -type f -name '*.dmg' -print0 | LC_ALL=C sort -z)

    repro_dir="${TAURI_DIR}/target/reproducibility"
    mkdir -p "${repro_dir}"
    app_dir="${TAURI_DIR}/target/universal-apple-darwin/release/bundle/macos/Maple.app"
    app_archive="${repro_dir}/maple-macos-app.tar.gz"

    print_file_hashes "${desktop_artifacts[@]}"
    scrub_host_metadata_files "${app_dir}"
    print_tree_hash "${app_dir}"
    print_canonical_apple_bundle_hash "${app_dir}" "$(repo_relative_path "${app_dir}")" | tee "${repro_dir}/desktop-pr-macos-app-canonical.sha256"
    rm -f "${app_archive}"
    archive_tree_as_root_tar_gz "$(dirname "${app_dir}")" "${app_archive}"
    write_sha256_manifest "${repro_dir}/desktop-pr-macos-final.sha256" "${desktop_artifacts[@]}" "${app_archive}"
    print_file_hashes "${app_archive}"
    verify_frontend_dist_unchanged
    ;;
  *)
    echo "Unsupported desktop PR build host: $(uname -s)" >&2
    exit 1
    ;;
esac
