#!/usr/bin/env bash

# Extracts native libraries from Microsoft's official ONNX Runtime NuGet
# package: one download, then an entry and a destination per library. The
# package is checked against its pin before anything is read from it. The
# caller must still check each extracted library against that library's own
# pin.
#
# The sourcing script provides onnxruntime-pins.sh and verify_sha256.
onnxruntime_nuget_extract_libraries() (
  set -euo pipefail

  version="$1"
  shift

  url="$(onnxruntime_nuget_package_url_for_version "${version}")"
  expected="$(onnxruntime_nuget_package_sha256_for_version "${version}")"
  package="$(mktemp "${TMPDIR:-/tmp}/onnxruntime-nuget.XXXXXX")"
  staged=""
  trap 'rm -f "${package}" "${staged}"' EXIT

  curl -fL --retry 5 --retry-delay 2 --retry-all-errors \
    "${url}" \
    --output "${package}"
  verify_sha256 "ONNX Runtime NuGet package" "${package}" "${expected}" || exit 1

  while [ "$#" -ge 2 ]; do
    entry="$1"
    destination="$2"
    shift 2

    entry_count="$(unzip -Z1 "${package}" | grep -Fxc -- "${entry}" || true)"
    if [ "${entry_count}" != "1" ]; then
      echo "Expected exactly one ${entry} in the ONNX Runtime NuGet package; found ${entry_count}." >&2
      exit 1
    fi

    mkdir -p "$(dirname "${destination}")"
    staged="$(mktemp "${destination}.XXXXXX")"
    unzip -p "${package}" "${entry}" > "${staged}" || exit 1
    chmod 0755 "${staged}"
    mv "${staged}" "${destination}"
    staged=""
  done
)
