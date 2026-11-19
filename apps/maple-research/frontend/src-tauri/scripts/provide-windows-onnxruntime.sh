#!/usr/bin/env bash
set -euo pipefail

ORT_VERSION="${ORT_VERSION:-1.23.2}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/onnxruntime-pins.sh"
source "${SCRIPT_DIR}/onnxruntime-nuget.sh"

TAURI_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
ORT_ROOT="${TAURI_DIR}/onnxruntime-windows"
ORT_DIR="${ORT_ROOT}/onnxruntime-win-x64-${ORT_VERSION}"
ORT_NUGET_ENTRY="runtimes/win-x64/native/onnxruntime.dll"
ORT_DLL="${ORT_DIR}/lib/onnxruntime.dll"
ORT_DLL_SHA256="$(onnxruntime_windows_x64_dll_sha256_for_version "${ORT_VERSION}")"

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

# Internal bash operations (curl, unzip, sha256sum, file checks) work fine with
# MSYS2-style /d/a/... paths. But ORT_LIB_LOCATION and ORT_DYLIB_PATH are
# consumed by the native Windows Rust toolchain (ort crate build script) in a
# later step, which interprets a leading /d/... as drive-relative and fails to
# resolve. Convert paths to native Windows form at the GITHUB_ENV boundary.
# Falls through unchanged on platforms without cygpath so the script stays
# runnable for local sanity checks.
to_native_path() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
  else
    printf '%s' "$1"
  fi
}

if [ ! -f "${ORT_DLL}" ]; then
  rm -rf "${ORT_ROOT}"
  onnxruntime_nuget_extract_library "${ORT_VERSION}" "${ORT_NUGET_ENTRY}" "${ORT_DLL}"
fi

verify_sha256 "ONNX Runtime DLL" "${ORT_DLL}" "${ORT_DLL_SHA256}"

echo "ORT_LIB_LOCATION=$(to_native_path "${ORT_DIR}")"
echo "ORT_SKIP_DOWNLOAD=true"
echo "ORT_DYLIB_PATH=$(to_native_path "${ORT_DLL}")"
