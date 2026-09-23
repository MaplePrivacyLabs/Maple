#!/usr/bin/env bash
set +x
set +a
set -euo pipefail

# Keep signing inputs out of dependency, frontend, and unsigned-build children.
# These shell variables are deliberately not exported, including when a caller
# has already exported a variable with one of these internal names.
ios_signing_issuer="${APPLE_API_ISSUER:-}"
ios_signing_key_id="${APPLE_API_KEY:-}"
ios_signing_private_key="${APPLE_API_PRIVATE_KEY:-}"
ios_signing_key_path="${APPLE_API_KEY_PATH:-}"
ios_signing_team="${APPLE_DEVELOPMENT_TEAM:-}"
export -n ios_signing_issuer ios_signing_key_id ios_signing_private_key ios_signing_key_path ios_signing_team
unset APPLE_API_ISSUER APPLE_API_KEY APPLE_API_PRIVATE_KEY APPLE_API_KEY_PATH APPLE_DEVELOPMENT_TEAM APPLE_TEAM_ID

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"
source "${SCRIPT_DIR}/ios-variant.sh"

# Fail on unknown variants before dependency installation or signing setup.
configure_ios_variant

if [ -z "${ios_signing_issuer}" ] || [ -z "${ios_signing_key_id}" ] || \
  { [ -z "${ios_signing_key_path}" ] && [ -z "${ios_signing_private_key}" ]; } || \
  [ -z "${ios_signing_team}" ]; then
  echo "iOS release signing variables are required: APPLE_API_ISSUER, APPLE_API_KEY, APPLE_API_KEY_PATH or APPLE_API_PRIVATE_KEY, APPLE_DEVELOPMENT_TEAM." >&2
  exit 1
fi

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
install_frontend_deps
configure_reproducible_build_metadata
build_frontend_dist

ios_project_state_dir=""
ios_signing_owned_dir=""
remove_ios_signing_key() {
  # A supplied APPLE_API_KEY_PATH belongs to the caller. Never modify it.
  if [ -n "${ios_signing_owned_dir}" ]; then
    rm -rf -- "${ios_signing_owned_dir}" || return $?
    ios_signing_owned_dir=""
  fi
}

cleanup_ios_release() {
  local status=$?
  trap - EXIT
  # Key cleanup must run even when project restoration fails.
  remove_ios_signing_key || status=1
  if [ -n "${ios_project_state_dir}" ] && [ -d "${ios_project_state_dir}" ]; then
    if python3 "${SCRIPT_DIR}/ios-build-profile.py" restore --state-dir "${ios_project_state_dir}"; then
      rm -rf -- "${ios_project_state_dir}" || status=1
    else
      # Preserve the snapshot so an operator can retry a failed restoration.
      status=1
    fi
  fi
  exit "${status}"
}

cd "${TAURI_DIR}"
trap cleanup_ios_release EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
ios_project_state_dir="$(mktemp -d "${TMPDIR:-/tmp}/maple-ios-profile.XXXXXX")"
ios_source_sha="$(git -C "${REPO_ROOT}" rev-parse HEAD)"
if [ "${MAPLE_IOS_VARIANT}" = "dev" ] && [ -z "${MAPLE_IOS_BUILD_NUMBER:-}" ]; then
  echo "MAPLE_IOS_BUILD_NUMBER is required for a Maple Dev TestFlight build." >&2
  exit 1
fi
ios_build_number="${MAPLE_IOS_BUILD_NUMBER:-$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "${TAURI_DIR}/tauri.conf.json")}"
python3 "${SCRIPT_DIR}/ios-build-profile.py" prepare \
  --state-dir "${ios_project_state_dir}" --variant "${MAPLE_IOS_VARIANT}" \
  --source-sha "${ios_source_sha}" --build-number "${ios_build_number}" \
  --frontend-hash "${MAPLE_FRONTEND_DIST_TREE_SHA256}"

repro_dir="${TAURI_DIR}/target/reproducibility"
if [ "${MAPLE_IOS_VARIANT}" = "dev" ]; then
  repro_dir="${repro_dir}/ios-dev"
  remove_build_tree "${TAURI_DIR}/target/ios-dev"
fi
mkdir -p "${repro_dir}"

verify_ios_onnxruntime_manifest
print_ios_onnxruntime_hashes
write_ios_onnxruntime_reproducibility_manifest "${repro_dir}/ios-onnxruntime-final.sha256"
./scripts/setup-ios-cargo-config.sh

export ORT_LIB_LOCATION="${TAURI_DIR}/onnxruntime-ios/onnxruntime.xcframework/ios-arm64"
export ORT_SKIP_DOWNLOAD="true"
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-16.0}"

ios_build_config="${ios_project_state_dir}/tauri-build-config.json"

verify_ios_profile() {
  python3 "${SCRIPT_DIR}/ios-build-profile.py" "$@" \
    --variant "${MAPLE_IOS_VARIANT}" --source-sha "${ios_source_sha}" \
    --build-number "${ios_build_number}"
}

remove_ios_release_outputs() {
  local artifact

  remove_build_tree "${TAURI_DIR}/gen/apple/build/arm64"
  remove_build_tree "${TAURI_DIR}/gen/apple/build/maple_iOS.xcarchive"
  remove_build_tree "${TAURI_DIR}/gen/apple/build/Payload"

  if [ -d "${TAURI_DIR}/gen/apple/build" ]; then
    while IFS= read -r -d '' artifact; do
      remove_build_tree "${artifact}"
    done < <(find "${TAURI_DIR}/gen/apple/build" -maxdepth 1 -type f -name '*.ipa' -print0 | LC_ALL=C sort -z)
  fi
}

build_ios_release() {
  cd "${FRONTEND_DIR}"
  bun tauri ios build --target aarch64 --ci --config "${ios_build_config}" "$@"
}

find_ios_release_app() {
  local app root

  for root in \
    "${TAURI_DIR}/gen/apple/build/maple_iOS.xcarchive/Products/Applications" \
    "${TAURI_DIR}/gen/apple/build/arm64" \
    "${TAURI_DIR}/gen/apple/build/Payload"; do
    if [ ! -d "${root}" ]; then
      continue
    fi

    app="$(find "${root}" -mindepth 1 -maxdepth 1 -type d -name '*.app' 2>/dev/null | LC_ALL=C sort | head -n 1)"
    if [ -n "${app}" ]; then
      printf '%s\n' "${app}"
      return 0
    fi
  done
}

write_ios_canonical_app_file_manifest() {
  local app="$1"
  local out="$2"
  local tmp

  tmp="$(mktemp -d)"
  cp -a "${app}" "${tmp}/app"
  remove_apple_signing_metadata "${tmp}/app"
  python3 "${REPO_ROOT}/scripts/ci/canonical-ios-app-hash.py" --manifest "${tmp}/app" > "${out}"
  rm -rf "${tmp}"
}

write_ios_ipa_payload_canonical_file_manifest() {
  local ipa="$1"
  local out="$2"
  local tmp app

  tmp="$(mktemp -d)"
  unzip -qq "${ipa}" -d "${tmp}"
  app="$(find "${tmp}/Payload" -mindepth 1 -maxdepth 1 -type d -name '*.app' 2>/dev/null | LC_ALL=C sort | head -n 1)"
  if [ -z "${app}" ]; then
    rm -rf "${tmp}"
    echo "Could not find a Payload/*.app bundle in ${ipa}" >&2
    return 1
  fi

  remove_apple_signing_metadata "${app}"
  write_ios_canonical_app_file_manifest "${app}" "${out}"
  rm -rf "${tmp}"
}

write_ios_canonical_app_manifest_diff() {
  local unsigned_manifest="$1"
  local signed_manifest="$2"
  local out="$3"
  local unsigned_by_path signed_by_path

  unsigned_by_path="$(mktemp)"
  signed_by_path="$(mktemp)"
  sed -E 's/^([0-9a-f]{64})  (.*)$/\2  \1/' "${unsigned_manifest}" | LC_ALL=C sort > "${unsigned_by_path}"
  sed -E 's/^([0-9a-f]{64})  (.*)$/\2  \1/' "${signed_manifest}" | LC_ALL=C sort > "${signed_by_path}"
  comm -3 "${unsigned_by_path}" "${signed_by_path}" > "${out}"
  if [ ! -s "${out}" ]; then
    rm -f "${out}"
    printf 'verified-ios-canonical-file-manifest-no-diff  %s  %s\n' "$(basename "${unsigned_manifest}")" "$(basename "${signed_manifest}")"
  fi
  rm -f "${unsigned_by_path}" "${signed_by_path}"
}

remove_ios_release_outputs
build_ios_release --no-sign --archive-only

unsigned_app="$(find_ios_release_app)"
if [ -z "${unsigned_app}" ]; then
  echo "Could not find unsigned iOS app build product under ${TAURI_DIR}/gen/apple/build." >&2
  exit 1
fi
verify_ios_profile verify-app "${unsigned_app}"
unsigned_app_canonical_hash="$(print_canonical_ios_app_hash "${unsigned_app}" "$(repo_relative_path "${unsigned_app}")" | tee "${repro_dir}/ios-release-unsigned-app-canonical.sha256" | awk '{ print $2 }')"
cat "${repro_dir}/ios-release-unsigned-app-canonical.sha256"
write_ios_canonical_app_file_manifest "${unsigned_app}" "${repro_dir}/ios-release-unsigned-app-canonical-files.sha256"

remove_ios_release_outputs
if [ -z "${ios_signing_key_path}" ]; then
  ios_signing_owned_dir="$(umask 077; mktemp -d "${TMPDIR:-/tmp}/maple-ios-signing.XXXXXX")"
  ios_signing_key_path="${ios_signing_owned_dir}/AuthKey.p8"
  (umask 077; decode_base64_string_to_file "${ios_signing_private_key}" "${ios_signing_key_path}")
fi
unset ios_signing_private_key
# The signed Tauri archive/export still runs native build code with signing
# authority. This narrows inheritance; it is not an isolated signing service.
(
  export APPLE_API_ISSUER="${ios_signing_issuer}"
  export APPLE_API_KEY="${ios_signing_key_id}"
  export APPLE_API_KEY_PATH="${ios_signing_key_path}"
  export APPLE_DEVELOPMENT_TEAM="${ios_signing_team}"
  build_ios_release --export-method app-store-connect
)
remove_ios_signing_key
unset ios_signing_issuer ios_signing_key_id ios_signing_key_path ios_signing_team

signed_app="$(find_ios_release_app)"
if [ -z "${signed_app}" ]; then
  echo "Could not find signed iOS app build product under ${TAURI_DIR}/gen/apple/build." >&2
  exit 1
fi
verify_ios_profile verify-app "${signed_app}"
signed_app_canonical_hash="$(print_canonical_ios_app_hash "${signed_app}" "$(repo_relative_path "${signed_app}")" | tee "${repro_dir}/ios-release-archive-app-canonical.sha256" | awk '{ print $2 }')"
cat "${repro_dir}/ios-release-archive-app-canonical.sha256"
write_ios_canonical_app_file_manifest "${signed_app}" "${repro_dir}/ios-release-archive-app-canonical-files.sha256"
write_ios_canonical_app_manifest_diff \
  "${repro_dir}/ios-release-unsigned-app-canonical-files.sha256" \
  "${repro_dir}/ios-release-archive-app-canonical-files.sha256" \
  "${repro_dir}/ios-release-archive-app-vs-unsigned-canonical.diff.txt"

if [ "${signed_app_canonical_hash}" != "${unsigned_app_canonical_hash}" ]; then
  echo "diagnostic-ios-archive-app-canonical-mismatch  archived iOS app does not strip back to the unsigned app tree." >&2
  echo "unsigned=${unsigned_app_canonical_hash}" >&2
  echo "archive_canonical=${signed_app_canonical_hash}" >&2
  echo "Canonical iOS archive app file manifest diff diagnostic:" >&2
  if [ -s "${repro_dir}/ios-release-archive-app-vs-unsigned-canonical.diff.txt" ]; then
    sed -n '1,80p' "${repro_dir}/ios-release-archive-app-vs-unsigned-canonical.diff.txt" >&2
  else
    echo "No canonical iOS archive app file manifest diff was produced." >&2
  fi
else
  printf 'verified-ios-archive-app  %s  %s\n' "${signed_app_canonical_hash}" "$(repo_relative_path "${signed_app}")"
fi

ios_artifacts=()
while IFS= read -r -d '' file; do
  ios_artifacts+=("${file}")
done < <(find "${TAURI_DIR}/gen/apple/build" -type f -name '*.ipa' -print0 | LC_ALL=C sort -z)

if [ "${#ios_artifacts[@]}" -ne 1 ]; then
  echo "Expected exactly one iOS IPA artifact, found ${#ios_artifacts[@]}." >&2
  exit 1
fi

if [ "${MAPLE_IOS_VARIANT}" = "dev" ]; then
  mkdir -p "${TAURI_DIR}/target/ios-dev"
  mv "${ios_artifacts[0]}" "${TAURI_DIR}/target/ios-dev/Maple-Dev.ipa"
  ios_artifacts=("${TAURI_DIR}/target/ios-dev/Maple-Dev.ipa")
  ios_profile_report="${TAURI_DIR}/target/ios-dev/ios-build-profile.json"
else
  ios_profile_report="${repro_dir}/ios-release-build-profile.json"
fi
verify_ios_profile verify-ipa "${ios_artifacts[0]}" --report "${ios_profile_report}"
write_sha256_manifest "${repro_dir}/ios-release-final.sha256" "${ios_artifacts[@]}"
print_file_hashes "${ios_artifacts[@]}"

: > "${repro_dir}/ios-release-canonical-payload.sha256"
for artifact in "${ios_artifacts[@]}"; do
  payload_file_manifest="${repro_dir}/ios-release-ipa-payload-canonical-files.sha256"
  payload_diff="${repro_dir}/ios-release-ipa-vs-unsigned-canonical.diff.txt"

  ipa_canonical_hash="$(print_canonical_ipa_payload_hash "${artifact}" "$(repo_relative_path "${artifact}")" | tee -a "${repro_dir}/ios-release-canonical-payload.sha256" | awk '{ print $2 }')"
  write_ios_ipa_payload_canonical_file_manifest "${artifact}" "${payload_file_manifest}"
  write_ios_canonical_app_manifest_diff \
    "${repro_dir}/ios-release-unsigned-app-canonical-files.sha256" \
    "${payload_file_manifest}" \
    "${payload_diff}"

  if [ "${ipa_canonical_hash}" != "${unsigned_app_canonical_hash}" ]; then
    echo "warning-ios-exported-payload-canonical-mismatch  exported iOS IPA payload does not strip back to the unsigned app build product." >&2
    echo "unsigned=${unsigned_app_canonical_hash}" >&2
    echo "ipa_payload=${ipa_canonical_hash}" >&2
    echo "Canonical iOS IPA payload file manifest diff diagnostic:" >&2
    if [ -s "${payload_diff}" ]; then
      sed -n '1,80p' "${payload_diff}" >&2
    else
      echo "No canonical iOS IPA payload file manifest diff was produced." >&2
    fi
    if [ "${MAPLE_ENFORCE_IOS_SIGNED_REPRODUCIBILITY:-0}" = "1" ]; then
      exit 1
    fi
    continue
  fi

  printf 'verified-ios-exported-payload  %s  %s\n' "${ipa_canonical_hash}" "$(repo_relative_path "${artifact}")"
done

verify_frontend_dist_unchanged
