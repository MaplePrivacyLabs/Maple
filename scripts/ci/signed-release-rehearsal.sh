#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${script_dir}/_common.sh"

usage() {
  cat >&2 <<'EOF'
usage: signed-release-rehearsal.sh <desktop|ios|android|all>

Runs signed release build scripts locally or in a protected trusted runner.
The scripts build unsigned baselines where applicable, build signed artifacts,
strip/normalize signed artifacts, and fail unless the canonical signed payloads
match the unsigned canonical hashes. Nothing is uploaded.

Environment loading:
  Export signing variables in the environment before running this script, or set
  MAPLE_SIGNING_ENV_FILE=/path/to/env to load one explicit env file.

Fake signing:
  Set MAPLE_RELEASE_FAKE_SIGNING=1 to use throwaway signing material where the
  platform supports it. This exercises Linux Tauri updater signing and Android
  APK/AAB signing without live keys. Apple release signing still requires real
  Apple credentials.

Windows:
  Windows Authenticode release signing is exercised in the protected Windows CI
  workflow through Azure Artifact Signing. This local rehearsal helper does not
  attempt a Windows release build or cloud signing flow.

No .env files are loaded implicitly.
EOF
}

fake_release_signing_enabled() {
  [ "${MAPLE_RELEASE_FAKE_SIGNING:-0}" = "1" ]
}

print_missing_env_var() {
  local name="$1"
  printf 'missing-env  %s\n' "${name}"
}

require_env_vars() {
  local missing=0 name

  for name in "$@"; do
    if [ -z "${!name:-}" ]; then
      print_missing_env_var "${name}"
      missing=1
    fi
  done

  if [ "${missing}" = "1" ]; then
    echo "Missing required signing environment variables for this rehearsal target." >&2
    return 1
  fi
}

require_any_env_var() {
  local name present=0

  for name in "$@"; do
    if [ -n "${!name:-}" ]; then
      present=1
    fi
  done

  if [ "${present}" = "0" ]; then
    printf 'missing-env  %s\n' "$*"
    echo "At least one of these signing environment variables is required." >&2
    return 1
  fi
}

load_env_file() {
  local file="$1"

  if [ ! -f "${file}" ]; then
    echo "Signing env file not found: ${file}" >&2
    return 1
  fi

  set -a
  # shellcheck disable=SC1090
  source "${file}"
  set +a
  printf 'loaded-signing-env  explicit-file\n'
}

load_signing_env() {
  if [ -n "${MAPLE_SIGNING_ENV_FILE:-}" ]; then
    load_env_file "${MAPLE_SIGNING_ENV_FILE}"
  fi
}

normalize_signing_env_aliases() {
  if [ -z "${APPLE_TEAM_ID:-}" ] && [ -n "${APPLE_DEVELOPMENT_TEAM:-}" ]; then
    export APPLE_TEAM_ID="${APPLE_DEVELOPMENT_TEAM}"
  fi

  if [ -z "${APPLE_PASSWORD:-}" ] && [ -n "${APPLE_ID_PASSWORD:-}" ]; then
    export APPLE_PASSWORD="${APPLE_ID_PASSWORD}"
  fi
}

without_ios_api_credentials() (
  # The ONNX setup and other platform builds do not need the iOS API key.
  # Keep APPLE_TEAM_ID: the desktop signer uses that separate input.
  unset APPLE_API_ISSUER APPLE_API_KEY APPLE_API_PRIVATE_KEY APPLE_API_KEY_PATH APPLE_DEVELOPMENT_TEAM
  "$@"
)

run_desktop_rehearsal() {
  local missing=0

  case "$(host_os)" in
    darwin)
      if fake_release_signing_enabled; then
        export MAPLE_TAURI_FAKE_UPDATER_SIGNING=1
      else
        require_any_env_var TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PATH || missing=1
      fi
      require_env_vars \
        APPLE_CERTIFICATE \
        APPLE_CERTIFICATE_PASSWORD \
        APPLE_ID \
        APPLE_PASSWORD \
        APPLE_TEAM_ID || missing=1
      ;;
    linux)
      if fake_release_signing_enabled; then
        export MAPLE_TAURI_FAKE_UPDATER_SIGNING=1
      else
        require_any_env_var TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PATH || missing=1
      fi
      ;;
  esac

  if [ "${missing}" = "1" ]; then
    return 1
  fi

  without_ios_api_credentials "${script_dir}/desktop-release.sh"
}

run_ios_rehearsal() {
  local missing=0

  if [ "$(host_os)" != "darwin" ]; then
    echo "iOS signed release rehearsal requires macOS." >&2
    return 1
  fi

  require_env_vars \
    APPLE_API_ISSUER \
    APPLE_API_KEY \
    APPLE_DEVELOPMENT_TEAM || missing=1
  if [ -z "${APPLE_API_PRIVATE_KEY:-}" ] && [ -z "${APPLE_API_KEY_PATH:-}" ]; then
    print_missing_env_var APPLE_API_PRIVATE_KEY
    print_missing_env_var APPLE_API_KEY_PATH
    echo "Either APPLE_API_PRIVATE_KEY or APPLE_API_KEY_PATH is required for iOS signing." >&2
    missing=1
  fi

  if [ "${missing}" = "1" ]; then
    return 1
  fi

  export MAPLE_ENFORCE_IOS_SIGNED_REPRODUCIBILITY="${MAPLE_ENFORCE_IOS_SIGNED_REPRODUCIBILITY:-1}"
  without_ios_api_credentials "${script_dir}/ios-onnxruntime.sh"
  "${script_dir}/ios-release.sh"
}

run_android_rehearsal() {
  local missing=0

  if fake_release_signing_enabled; then
    export MAPLE_ANDROID_FAKE_SIGNING=1
  else
    require_env_vars \
      ANDROID_KEYSTORE_BASE64 \
      ANDROID_KEY_ALIAS \
      ANDROID_KEY_PASSWORD || missing=1
  fi

  if [ "${missing}" = "1" ]; then
    return 1
  fi

  without_ios_api_credentials "${script_dir}/android-release.sh"
}

target="${1:-}"
case "${target}" in
  desktop | ios | android | all)
    ;;
  -h | --help | help)
    usage
    exit 0
    ;;
  *)
    usage
    exit 2
    ;;
esac

load_signing_env
normalize_signing_env_aliases

case "${target}" in
  desktop)
    run_desktop_rehearsal
    ;;
  ios)
    run_ios_rehearsal
    ;;
  android)
    run_android_rehearsal
    ;;
  all)
    run_desktop_rehearsal
    if [ "$(host_os)" = "darwin" ]; then
      run_ios_rehearsal
    elif [ "$(host_os)" = "linux" ] && [ "$(uname -m)" = "x86_64" ]; then
      run_android_rehearsal
    else
      echo "Skipping mobile signed rehearsal for host $(uname -s)/$(uname -m)." >&2
    fi
    ;;
esac
