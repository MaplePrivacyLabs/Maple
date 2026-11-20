#!/usr/bin/env bash
set -euo pipefail

ORT_VERSION="${ORT_VERSION:-1.23.2}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/onnxruntime-pins.sh"
source "${SCRIPT_DIR}/onnxruntime-nuget.sh"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "macOS ONNX Runtime provisioning must run on macOS." >&2
  exit 1
fi

TAURI_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
ORT_ROOT="${TAURI_DIR}/onnxruntime-macos"
ORT_DIR="${ORT_ROOT}/onnxruntime-osx-universal2-${ORT_VERSION}"
ORT_DYLIB="${ORT_DIR}/lib/libonnxruntime.${ORT_VERSION}.dylib"
ORT_X64_SHA256="$(onnxruntime_macos_x64_dylib_sha256_for_version "${ORT_VERSION}")"
ORT_ARM64_SHA256="$(onnxruntime_macos_arm64_dylib_sha256_for_version "${ORT_VERSION}")"

sha256_file() {
  shasum -a 256 "$1" | awk '{print $1}'
}

verify_sha256() {
  local label="$1"
  local path="$2"
  local expected="$3"
  local actual

  actual="$(sha256_file "${path}")"
  if [ "${actual}" != "${expected}" ]; then
    echo "${label} SHA-256 mismatch for ${path}" >&2
    echo "expected: ${expected}" >&2
    echo "actual:   ${actual}" >&2
    return 1
  fi
}

# The universal library is checked one architecture at a time, because the
# pins are for the two files Microsoft publishes, not for lipo's layout.
verify_universal_dylib() (
  slice="$(mktemp "${ORT_ROOT}/.slice.XXXXXX")"
  trap 'rm -f "${slice}"' EXIT

  lipo -thin x86_64 "${ORT_DYLIB}" -output "${slice}" || exit 1
  verify_sha256 "ONNX Runtime x86_64 dylib" "${slice}" "${ORT_X64_SHA256}" || exit 1
  lipo -thin arm64 "${ORT_DYLIB}" -output "${slice}" || exit 1
  verify_sha256 "ONNX Runtime arm64 dylib" "${slice}" "${ORT_ARM64_SHA256}" || exit 1
)

# A library from before this provider used NuGet has other signatures and
# fails the check once; it is fetched again rather than reported.
if [ ! -f "${ORT_DYLIB}" ] || ! verify_universal_dylib 2>/dev/null; then
  rm -rf "${ORT_ROOT}"
  mkdir -p "${ORT_DIR}/lib"
  x64_dylib="${ORT_ROOT}/libonnxruntime.x86_64.dylib"
  arm64_dylib="${ORT_ROOT}/libonnxruntime.arm64.dylib"

  onnxruntime_nuget_extract_libraries "${ORT_VERSION}" \
    "runtimes/osx-x64/native/libonnxruntime.dylib" "${x64_dylib}" \
    "runtimes/osx-arm64/native/libonnxruntime.dylib" "${arm64_dylib}"

  verify_sha256 "ONNX Runtime x86_64 dylib" "${x64_dylib}" "${ORT_X64_SHA256}"
  verify_sha256 "ONNX Runtime arm64 dylib" "${arm64_dylib}" "${ORT_ARM64_SHA256}"
  lipo -create "${x64_dylib}" "${arm64_dylib}" -output "${ORT_DYLIB}"
  rm -f "${x64_dylib}" "${arm64_dylib}"
fi

verify_universal_dylib

echo "ORT_LIB_LOCATION=${ORT_DIR}"
echo "ORT_SKIP_DOWNLOAD=true"
echo "ORT_DYLIB_PATH=${ORT_DYLIB}"
