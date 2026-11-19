#!/usr/bin/env bash
set -euo pipefail

ORT_VERSION="${ORT_VERSION:-1.23.2}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/onnxruntime-pins.sh"
source "${SCRIPT_DIR}/onnxruntime-nuget.sh"

case "$(uname -m)" in
  x86_64 | amd64)
    ORT_ARCH="x64"
    ORT_NUGET_RUNTIME="linux-x64"
    ORT_DYLIB_SHA256_FUNC="onnxruntime_linux_x64_dylib_sha256_for_version"
    ;;
  aarch64 | arm64)
    ORT_ARCH="aarch64"
    ORT_NUGET_RUNTIME="linux-arm64"
    ORT_DYLIB_SHA256_FUNC="onnxruntime_linux_aarch64_dylib_sha256_for_version"
    ;;
  *)
    echo "Unsupported Linux ONNX Runtime architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

TAURI_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
ORT_ROOT="${TAURI_DIR}/onnxruntime-linux"
ORT_DIR="${ORT_ROOT}/onnxruntime-linux-${ORT_ARCH}-${ORT_VERSION}"
ORT_NUGET_ENTRY="runtimes/${ORT_NUGET_RUNTIME}/native/libonnxruntime.so"
ORT_DYLIB="${ORT_DIR}/lib/libonnxruntime.so.${ORT_VERSION}"
ORT_DYLIB_SHA256="$("${ORT_DYLIB_SHA256_FUNC}" "${ORT_VERSION}")"

sha256_file() {
  local path="$1"

  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${path}" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "${path}" | awk '{print $1}'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 -r "${path}" | awk '{print $1}'
  else
    echo "No SHA-256 tool found. Install sha256sum, shasum, or openssl." >&2
    return 1
  fi
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

if [ ! -f "${ORT_DYLIB}" ]; then
  rm -rf "${ORT_ROOT}"
  onnxruntime_nuget_extract_library "${ORT_VERSION}" "${ORT_NUGET_ENTRY}" "${ORT_DYLIB}"
fi

verify_sha256 "ONNX Runtime shared library" "${ORT_DYLIB}" "${ORT_DYLIB_SHA256}"

echo "ORT_LIB_LOCATION=${ORT_DIR}"
echo "ORT_SKIP_DOWNLOAD=true"
echo "ORT_DYLIB_PATH=${ORT_DYLIB}"
