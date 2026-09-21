#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
FRONTEND_DIR="${REPO_ROOT}/apps/maple-research/frontend"
TAURI_DIR="${FRONTEND_DIR}/src-tauri"

source "${TAURI_DIR}/scripts/onnxruntime-pins.sh"
source "${TAURI_DIR}/scripts/onnxruntime-android-pins.sh"

export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"

verify_rust_lockfile() {
  (cd "${TAURI_DIR}" && cargo metadata --locked --format-version 1 >/dev/null)
}

install_frontend_deps() {
  disable_bun_env_files
  rm -rf "${FRONTEND_DIR}/node_modules"
  "${REPO_ROOT}/scripts/prepare-frontend-deps.sh"
  export MAPLE_FRONTEND_DEPS_PREPARED=1
}

disable_bun_env_files() {
  if [ -n "${MAPLE_BUN_NO_ENV_FILE:-}" ]; then
    export MAPLE_IGNORE_VITE_ENV_FILES=1
    return 0
  fi

  local real_bun wrapper_dir frontend_dir
  real_bun="$(command -v bun)"
  frontend_dir="${FRONTEND_DIR}"
  wrapper_dir="$(mktemp -d)"

  cat > "${wrapper_dir}/bun" <<EOF
#!/usr/bin/env bash
set -euo pipefail

real_bun="$(printf '%q' "${real_bun}")"
frontend_dir="$(printf '%q' "${frontend_dir}")"
restore_dir=""
moved_env_files=()

restore_frontend_env_files() {
  local name

  if [ -z "\${restore_dir}" ]; then
    return 0
  fi

  for name in "\${moved_env_files[@]}"; do
    if [ -e "\${restore_dir}/\${name}" ] || [ -L "\${restore_dir}/\${name}" ]; then
      mv -- "\${restore_dir}/\${name}" "\${frontend_dir}/\${name}"
    fi
  done

  rmdir "\${restore_dir}" 2>/dev/null || true
}

hide_frontend_env_files() {
  local cwd name

  cwd="\$(pwd -P)"
  case "\${cwd}" in
    "\${frontend_dir}" | "\${frontend_dir}"/*)
      ;;
    *)
      return 0
      ;;
  esac

  restore_dir="\$(mktemp -d "\${frontend_dir}/.maple-bun-env-hide.XXXXXX")"
  for name in \
    .env \
    .env.local \
    .env.development \
    .env.development.local \
    .env.production \
    .env.production.local \
    .env.test \
    .env.test.local; do
    if [ -e "\${frontend_dir}/\${name}" ] || [ -L "\${frontend_dir}/\${name}" ]; then
      mv -- "\${frontend_dir}/\${name}" "\${restore_dir}/\${name}"
      moved_env_files+=("\${name}")
    fi
  done
}

hide_frontend_env_files
trap restore_frontend_env_files EXIT HUP INT TERM

set +e
"\${real_bun}" --no-env-file "\$@"
status="\$?"
set -e
exit "\${status}"
EOF
  chmod +x "${wrapper_dir}/bun"

  export MAPLE_BUN_NO_ENV_FILE=1
  export MAPLE_IGNORE_VITE_ENV_FILES=1
  export PATH="${wrapper_dir}:${PATH}"
}

configure_sccache() {
  if command -v sccache >/dev/null 2>&1; then
    local os socket_root
    os="$(host_os)"

    export RUSTC_WRAPPER="${RUSTC_WRAPPER:-sccache}"
    export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-2G}"
    export CARGO_CACHE_RUSTC_INFO="${CARGO_CACHE_RUSTC_INFO:-0}"

    case "${os}" in
      darwin)
        socket_root="${TMPDIR:-/tmp}"
        export SCCACHE_SERVER_UDS="${SCCACHE_SERVER_UDS:-${socket_root%/}/maple-sccache-${os}.sock}"
        export SCCACHE_DIR="${SCCACHE_DIR:-${HOME}/Library/Caches/Mozilla.sccache}"
        ;;
      mingw* | msys* | cygwin*)
        unset SCCACHE_SERVER_UDS
        if [ -n "${LOCALAPPDATA:-}" ]; then
          export SCCACHE_DIR="${SCCACHE_DIR:-${LOCALAPPDATA}\\Mozilla\\sccache}"
        else
          export SCCACHE_DIR="${SCCACHE_DIR:-${HOME}/AppData/Local/Mozilla/sccache}"
        fi
        ;;
      *)
        socket_root="${TMPDIR:-/tmp}"
        export SCCACHE_SERVER_UDS="${SCCACHE_SERVER_UDS:-${socket_root%/}/maple-sccache-${os}.sock}"
        export SCCACHE_DIR="${SCCACHE_DIR:-${HOME}/.cache/sccache}"
        ;;
    esac
  fi
}

use_pr_environment() {
  local name
  while IFS='=' read -r name _; do
    case "${name}" in
      VITE_*)
        unset "${name}"
        ;;
    esac
  done < <(env)

  export VITE_OPEN_SECRET_API_URL="https://enclave.secretgpt.ai"
  export VITE_OPEN_SECRET_PCR_ENVIRONMENT="development"
  export VITE_OS_FLAGS_BASE_URL="https://flags-dev.opensecret.cloud"
  export VITE_MAPLE_BILLING_API_URL="https://billing-dev.opensecret.cloud"
  export VITE_CLIENT_ID="ba5a14b5-d915-47b1-b7b1-afda52bc5fc6"
}

use_release_environment() {
  local name
  while IFS='=' read -r name _; do
    case "${name}" in
      VITE_*)
        unset "${name}"
        ;;
    esac
  done < <(env)

  export VITE_OPEN_SECRET_API_URL="https://enclave.trymaple.ai"
  export VITE_OPEN_SECRET_PCR_ENVIRONMENT="production"
  export VITE_OS_FLAGS_BASE_URL="https://flags.opensecret.cloud"
  export VITE_MAPLE_BILLING_API_URL="https://billing.opensecret.cloud"
  export VITE_CLIENT_ID="ba5a14b5-d915-47b1-b7b1-afda52bc5fc6"
}

configure_reproducible_build_metadata() {
  if [ -z "${SOURCE_DATE_EPOCH:-}" ]; then
    SOURCE_DATE_EPOCH="315532800"
    export SOURCE_DATE_EPOCH
  fi

  export ZERO_AR_DATE="${ZERO_AR_DATE:-1}"
  configure_reproducible_rust_paths
  configure_reproducible_native_paths
  echo "SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}"
}

print_source_provenance() {
  if ! command -v git >/dev/null 2>&1; then
    return 0
  fi

  if ! git -C "${REPO_ROOT}" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    return 0
  fi

  printf 'git-commit  %s\n' "$(git -C "${REPO_ROOT}" rev-parse HEAD)"
  printf 'git-tree  %s\n' "$(git -C "${REPO_ROOT}" rev-parse 'HEAD^{tree}')"

  if ! git -C "${REPO_ROOT}" diff --quiet --ignore-submodules --; then
    echo "git-worktree-dirty  unstaged"
  fi

  if ! git -C "${REPO_ROOT}" diff --cached --quiet --ignore-submodules --; then
    echo "git-worktree-dirty  staged"
  fi
}

append_env_word_once() {
  local var_name="$1"
  local word="$2"
  local current

  current="${!var_name:-}"
  case " ${current} " in
    *" ${word} "*)
      ;;
    *)
      printf -v "${var_name}" '%s' "${current:+${current} }${word}"
      export "${var_name}"
      ;;
  esac
}

append_env_words_once() {
  local var_name="$1"
  local words="$2"
  local current

  if [ -z "${words}" ]; then
    return 0
  fi

  current="${!var_name:-}"
  case " ${current} " in
    *" ${words} "*)
      ;;
    *)
      printf -v "${var_name}" '%s' "${current:+${current} }${words}"
      export "${var_name}"
      ;;
  esac
}

append_rustflag_once() {
  local flag="$1"
  local var_name

  append_env_word_once RUSTFLAGS "${flag}"
  for var_name in \
    CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS \
    CARGO_TARGET_X86_64_APPLE_DARWIN_RUSTFLAGS; do
    if [ -n "${!var_name:-}" ]; then
      append_env_word_once "${var_name}" "${flag}"
    fi
  done
}

append_rust_remap_path_prefix() {
  local from="$1"
  local to="$2"

  if [ -z "${from}" ] || [ ! -d "${from}" ]; then
    return 0
  fi

  append_rustflag_once "--remap-path-prefix=${from}=${to}"
}

configure_reproducible_rust_paths() {
  local cargo_home rustup_home

  append_rust_remap_path_prefix "${REPO_ROOT}" "/maple"

  cargo_home="${CARGO_HOME:-${HOME:-}/.cargo}"
  append_rust_remap_path_prefix "${cargo_home}" "/cargo"

  rustup_home="${RUSTUP_HOME:-${HOME:-}/.rustup}"
  append_rust_remap_path_prefix "${rustup_home}" "/rustup"
}

append_native_remap_path_prefix() {
  local from="$1"
  local to="$2"
  local flag var_name

  if [ -z "${from}" ] || [ ! -d "${from}" ]; then
    return 0
  fi

  for flag in \
    "-ffile-prefix-map=${from}=${to}" \
    "-fmacro-prefix-map=${from}=${to}" \
    "-fdebug-prefix-map=${from}=${to}"; do
    for var_name in \
      CFLAGS CXXFLAGS OBJCFLAGS OBJCXXFLAGS \
      CMAKE_C_FLAGS CMAKE_CXX_FLAGS CMAKE_OBJC_FLAGS CMAKE_OBJCXX_FLAGS; do
      append_env_word_once "${var_name}" "${flag}"
    done
  done
}

configure_reproducible_native_paths() {
  local cargo_home rustup_home

  append_native_remap_path_prefix "${REPO_ROOT}" "/maple"

  cargo_home="${CARGO_HOME:-${HOME:-}/.cargo}"
  append_native_remap_path_prefix "${cargo_home}" "/cargo"

  rustup_home="${RUSTUP_HOME:-${HOME:-}/.rustup}"
  append_native_remap_path_prefix "${rustup_home}" "/rustup"
}

configure_tauri_updater_signing_key() {
  if [ -n "${TAURI_SIGNING_PRIVATE_KEY:-}" ]; then
    unset TAURI_SIGNING_PRIVATE_KEY_PATH
    return 0
  fi

  if [ -n "${TAURI_SIGNING_PRIVATE_KEY_PATH:-}" ]; then
    if [ ! -f "${TAURI_SIGNING_PRIVATE_KEY_PATH}" ]; then
      echo "TAURI_SIGNING_PRIVATE_KEY_PATH does not exist: ${TAURI_SIGNING_PRIVATE_KEY_PATH}" >&2
      return 1
    fi
    # tauri build requires the key content even though tauri signer sign accepts a path.
    export TAURI_SIGNING_PRIVATE_KEY="$(cat "${TAURI_SIGNING_PRIVATE_KEY_PATH}")"
    unset TAURI_SIGNING_PRIVATE_KEY_PATH
  fi
}

generate_fake_tauri_updater_keypair() {
  local key_dir private_key password

  key_dir="$(mktemp -d)"
  private_key="${key_dir}/tauri-updater.key"
  password="${MAPLE_TAURI_FAKE_UPDATER_PASSWORD:-maple-pr-fake-updater-key}"

  (
    cd "${FRONTEND_DIR}"
    env -u LD_LIBRARY_PATH bun tauri signer generate \
      --ci \
      --password "${password}" \
      --write-keys "${private_key}" \
      --force >/dev/null
  )

  unset TAURI_SIGNING_PRIVATE_KEY
  export TAURI_SIGNING_PRIVATE_KEY_PATH="${private_key}"
  export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${password}"
  export MAPLE_TAURI_UPDATER_PUBLIC_KEY_PATH="${private_key}.pub"

  echo "generated-fake-tauri-updater-keypair  ephemeral"
}

source_date_rfc3339() {
  local epoch="${SOURCE_DATE_EPOCH:?SOURCE_DATE_EPOCH is required}"

  if date -u -d "@${epoch}" +"%Y-%m-%dT%H:%M:%SZ" 2>/dev/null; then
    return 0
  fi

  date -u -r "${epoch}" +"%Y-%m-%dT%H:%M:%SZ"
}

is_valid_xcode_developer_dir() {
  local dir="${1:-}"
  [ -n "${dir}" ] \
    && [ -x "${dir}/usr/bin/simctl" ] \
    && [ -x "${dir}/Toolchains/XcodeDefault.xctoolchain/usr/bin/clang" ]
}

xcode_version_for_developer_dir() {
  local dir="${1:?developer dir is required}"
  DEVELOPER_DIR="${dir}" /usr/bin/xcodebuild -version 2>/dev/null | awk '/^Xcode / { print $2; exit }'
}

xcode_version_matches_expected() {
  local version="${1:-}"
  local expected="${2:-}"

  [ "${version}" = "${expected}" ] \
    || [ "${version}" = "${expected}.0" ] \
    || [ "${version}.0" = "${expected}" ]
}

resolve_xcode_developer_dir() {
  local expected="${MAPLE_NIX_XCODE_VERSION:-}"
  local candidate version
  local -a candidates=()

  if [ -n "${DEVELOPER_DIR:-}" ]; then
    candidates+=("${DEVELOPER_DIR}")
  fi

  if [ -n "${expected}" ]; then
    candidates+=(
      "/Applications/Xcode-${expected}.app/Contents/Developer"
      "/Applications/Xcode-${expected}.0.app/Contents/Developer"
      "/Applications/Xcode_${expected}.app/Contents/Developer"
      "/Applications/Xcode_${expected}.0.app/Contents/Developer"
    )
  fi

  candidate="$(xcode-select -p 2>/dev/null || true)"
  if [ -n "${candidate}" ]; then
    candidates+=("${candidate}")
  fi
  candidates+=("/Applications/Xcode.app/Contents/Developer")

  for candidate in "${candidates[@]}"; do
    if ! is_valid_xcode_developer_dir "${candidate}"; then
      continue
    fi

    if [ -n "${expected}" ]; then
      version="$(xcode_version_for_developer_dir "${candidate}")"
      if ! xcode_version_matches_expected "${version}" "${expected}"; then
        continue
      fi
    fi

    printf '%s\n' "${candidate}"
    return 0
  done

  if [ -n "${expected}" ]; then
    echo "Could not find Xcode ${expected}. Install it or select it with xcode-select." >&2
  else
    echo "Could not find a full Xcode installation." >&2
  fi
  return 1
}

use_xcode_toolchain() {
  if [ "$(host_os)" != "darwin" ]; then
    return 0
  fi

  local dev_dir toolchain_bin macos_sdkroot macos_link_flags xcode_info
  dev_dir="$(resolve_xcode_developer_dir)"
  toolchain_bin="${dev_dir}/Toolchains/XcodeDefault.xctoolchain/usr/bin"
  macos_sdkroot="$(DEVELOPER_DIR="${dev_dir}" /usr/bin/xcrun --sdk macosx --show-sdk-path)"
  macos_link_flags="-C link-arg=-isysroot -C link-arg=${macos_sdkroot}"
  xcode_info="$(DEVELOPER_DIR="${dev_dir}" /usr/bin/xcodebuild -version)"

  export DEVELOPER_DIR="${dev_dir}"
  export MAPLE_XCODE_VERSION="$(printf '%s\n' "${xcode_info}" | awk '/^Xcode / { print $2; exit }')"
  export MAPLE_XCODE_BUILD_VERSION="$(printf '%s\n' "${xcode_info}" | awk '/^Build version / { print $3; exit }')"
  export CC="/usr/bin/clang"
  export CXX="/usr/bin/clang++"
  export AR="/usr/bin/ar"
  export RANLIB="/usr/bin/ranlib"
  export PATH="${toolchain_bin}:${PATH}:${dev_dir}/usr/bin:/usr/bin"

  if command -v sw_vers >/dev/null 2>&1; then
    printf 'Using macOS %s build %s\n' "$(sw_vers -productVersion)" "$(sw_vers -buildVersion)"
  fi
  printf 'Using Xcode %s build %s from %s\n' "${MAPLE_XCODE_VERSION}" "${MAPLE_XCODE_BUILD_VERSION}" "${dev_dir}"

  if [ -n "${MAPLE_NIX_LIBICONV:-}" ] && [ -d "${MAPLE_NIX_LIBICONV}/lib" ]; then
    export LIBRARY_PATH="${MAPLE_NIX_LIBICONV}/lib${LIBRARY_PATH:+:${LIBRARY_PATH}}"
  fi

  export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER="${CC}"
  export CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER="${CC}"
  export CARGO_TARGET_AARCH64_APPLE_IOS_LINKER="${CC}"
  export CARGO_TARGET_AARCH64_APPLE_IOS_SIM_LINKER="${CC}"
  append_env_words_once CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS "${RUSTFLAGS:-}"
  append_env_words_once CARGO_TARGET_X86_64_APPLE_DARWIN_RUSTFLAGS "${RUSTFLAGS:-}"
  append_env_words_once CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS "${macos_link_flags}"
  append_env_words_once CARGO_TARGET_X86_64_APPLE_DARWIN_RUSTFLAGS "${macos_link_flags}"

  export CC_aarch64_apple_darwin="${CC}"
  export CXX_aarch64_apple_darwin="${CXX}"
  export AR_aarch64_apple_darwin="${AR}"
  export RANLIB_aarch64_apple_darwin="${RANLIB}"

  export CC_x86_64_apple_darwin="${CC}"
  export CXX_x86_64_apple_darwin="${CXX}"
  export AR_x86_64_apple_darwin="${AR}"
  export RANLIB_x86_64_apple_darwin="${RANLIB}"

  export CC_aarch64_apple_ios="${CC}"
  export CXX_aarch64_apple_ios="${CXX}"
  export AR_aarch64_apple_ios="${AR}"
  export RANLIB_aarch64_apple_ios="${RANLIB}"

  export CC_aarch64_apple_ios_sim="${CC}"
  export CXX_aarch64_apple_ios_sim="${CXX}"
  export AR_aarch64_apple_ios_sim="${AR}"
  export RANLIB_aarch64_apple_ios_sim="${RANLIB}"

  unset NIX_CC NIX_CC_FOR_BUILD NIX_BINTOOLS NIX_BINTOOLS_FOR_BUILD
  unset NIX_CFLAGS_COMPILE NIX_CFLAGS_COMPILE_FOR_BUILD NIX_CFLAGS_LINK NIX_CFLAGS_LINK_FOR_BUILD
  unset NIX_LDFLAGS NIX_LDFLAGS_FOR_BUILD NIX_LDFLAGS_BEFORE NIX_LDFLAGS_BEFORE_FOR_BUILD
  unset NIX_DONT_SET_RPATH NIX_DONT_SET_RPATH_FOR_BUILD NIX_ENFORCE_NO_NATIVE
  unset NIX_HARDENING_ENABLE NIX_IGNORE_LD_THROUGH_GCC NIX_NO_SELF_RPATH NIXPKGS_CMAKE_PREFIX_PATH
  unset LD CFLAGS CXXFLAGS CPPFLAGS LDFLAGS

  local name
  while IFS='=' read -r name _; do
    case "${name}" in
      NIX_CC_WRAPPER_* | NIX_BINTOOLS_WRAPPER_* | NIX_PKG_CONFIG_WRAPPER_*)
        unset "${name}"
        ;;
    esac
  done < <(env)

  configure_reproducible_native_paths

  export SDKROOT="${SDKROOT:-${macos_sdkroot}}"
  export CMAKE_OSX_SYSROOT="${CMAKE_OSX_SYSROOT:-${macos_sdkroot}}"
  for name in \
    CFLAGS CXXFLAGS OBJCFLAGS OBJCXXFLAGS \
    CFLAGS_aarch64_apple_darwin CXXFLAGS_aarch64_apple_darwin \
    CFLAGS_x86_64_apple_darwin CXXFLAGS_x86_64_apple_darwin; do
    append_env_words_once "${name}" "-isysroot ${macos_sdkroot}"
  done
}

require_ios_simulator_runtime_for_xcode() {
  if [ "$(host_os)" != "darwin" ]; then
    return 0
  fi

  local dev_dir sdk_settings version runtimes_json
  dev_dir="$(resolve_xcode_developer_dir)"
  sdk_settings="${dev_dir}/Platforms/iPhoneSimulator.platform/Developer/SDKs/iPhoneSimulator.sdk/SDKSettings.plist"

  if [ ! -f "${sdk_settings}" ]; then
    echo "iPhoneSimulator SDKSettings.plist is missing: ${sdk_settings}" >&2
    return 1
  fi

  version="$(/usr/libexec/PlistBuddy -c 'Print :Version' "${sdk_settings}")"
  runtimes_json="$(DEVELOPER_DIR="${dev_dir}" /usr/bin/xcrun simctl list runtimes --json)"

  if ! SIMCTL_RUNTIMES_JSON="${runtimes_json}" python3 - "${version}" >/dev/null <<'PY'
import json
import os
import sys

required = sys.argv[1]

def version_tuple(value):
    parts = []
    for part in value.split("."):
        try:
            parts.append(int(part))
        except ValueError:
            parts.append(0)
    while len(parts) < 3:
        parts.append(0)
    return tuple(parts)

required_tuple = version_tuple(required)
for runtime in json.loads(os.environ["SIMCTL_RUNTIMES_JSON"]).get("runtimes", []):
    if not str(runtime.get("name", "")).startswith("iOS"):
        continue
    if runtime.get("isAvailable") is False:
        continue
    if version_tuple(str(runtime.get("version", ""))) >= required_tuple:
        sys.exit(0)
sys.exit(1)
PY
  then
    echo "iOS Simulator runtime ${version} or newer is required for Xcode $(xcode_version_for_developer_dir "${dev_dir}")." >&2
    echo "Install it with: xcodebuild -downloadPlatform iOS" >&2
    DEVELOPER_DIR="${dev_dir}" /usr/bin/xcrun simctl list runtimes >&2 || true
    return 1
  fi
}

host_os() {
  uname -s | tr '[:upper:]' '[:lower:]'
}

require_windows_host() {
  case "$(host_os)" in
    mingw* | msys* | cygwin*)
      ;;
    *)
      echo "$1 must run on Windows under Git Bash/MSYS." >&2
      return 1
      ;;
  esac
}

to_windows_path() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
  else
    printf '%s\n' "$1"
  fi
}

linux_ort_arch() {
  case "$(uname -m)" in
    x86_64 | amd64)
      printf '%s\n' "x64"
      ;;
    aarch64 | arm64)
      printf '%s\n' "aarch64"
      ;;
    *)
      echo "Unsupported Linux ONNX Runtime architecture: $(uname -m)" >&2
      return 1
      ;;
  esac
}

prepare_linux_onnxruntime() {
  if [ "$(host_os)" != "linux" ]; then
    return 0
  fi

  local ort_env
  ort_env="$("${TAURI_DIR}/scripts/provide-linux-onnxruntime.sh")"
  printf '%s\n' "${ort_env}"

  local key value
  while IFS='=' read -r key value; do
    case "${key}" in
      ORT_LIB_LOCATION | ORT_SKIP_DOWNLOAD | ORT_DYLIB_PATH)
        export "${key}=${value}"
        ;;
    esac
  done <<< "${ort_env}"
}

prepare_windows_onnxruntime() {
  require_windows_host "prepare_windows_onnxruntime"

  local ort_env key value
  ort_env="$("${TAURI_DIR}/scripts/provide-windows-onnxruntime.sh")"
  printf '%s\n' "${ort_env}"

  while IFS='=' read -r key value; do
    case "${key}" in
      ORT_LIB_LOCATION | ORT_SKIP_DOWNLOAD | ORT_DYLIB_PATH)
        export "${key}=${value}"
        ;;
    esac
  done <<< "${ort_env}"
}

prepare_macos_onnxruntime() {
  if [ "$(host_os)" != "darwin" ]; then
    return 0
  fi

  local ort_env key value
  ort_env="$("${TAURI_DIR}/scripts/provide-macos-onnxruntime.sh")"
  printf '%s\n' "${ort_env}"

  while IFS='=' read -r key value; do
    case "${key}" in
      ORT_LIB_LOCATION | ORT_SKIP_DOWNLOAD | ORT_DYLIB_PATH)
        export "${key}=${value}"
        ;;
    esac
  done <<< "${ort_env}"
}

prepare_android_onnxruntime() {
  local ort_env

  ort_env="$("${TAURI_DIR}/scripts/provide-android-onnxruntime.sh")"
  printf '%s\n' "${ort_env}"
}

android_onnxruntime_jni_libs_dir() {
  printf '%s\n' "${TAURI_DIR}/gen/android/app/src/main/jniLibs"
}

android_llvm_readelf() {
  local found

  if command -v llvm-readelf >/dev/null 2>&1; then
    command -v llvm-readelf
    return 0
  fi

  if command -v readelf >/dev/null 2>&1; then
    command -v readelf
    return 0
  fi

  if [ -n "${NDK_HOME:-}" ]; then
    found="$(
      find "${NDK_HOME}/toolchains/llvm/prebuilt" -type f -path '*/bin/llvm-readelf' 2>/dev/null \
        | LC_ALL=C sort \
        | head -n 1 || true
    )"
    if [ -n "${found}" ]; then
      printf '%s\n' "${found}"
      return 0
    fi
  fi

  echo "llvm-readelf or readelf is required to verify Android native-library alignment." >&2
  return 1
}

verify_android_elf_load_alignment() {
  local library="$1"
  local label="$2"
  local readelf align load_count=0

  readelf="$(android_llvm_readelf)"
  while IFS= read -r align; do
    load_count=$((load_count + 1))
    if [[ ! "${align}" =~ ^0x[0-9a-fA-F]+$ ]] || (( align < 0x4000 )); then
      echo "Android native library ${label} has a LOAD segment aligned below 16 KiB: ${align}." >&2
      return 1
    fi
  done < <("${readelf}" -lW "${library}" | awk '$1 == "LOAD" { print $NF }')

  if [ "${load_count}" -eq 0 ]; then
    echo "Android native library ${label} has no ELF LOAD segments." >&2
    return 1
  fi

  printf 'verified-android-elf-load-alignment  16384  %s\n' "${label}"
}

prepare_android_cargo_config() {
  local cargo_dir="${TAURI_DIR}/.cargo"
  local config_file="${cargo_dir}/config.toml"
  local page_size_flags="-C link-arg=-Wl,-z,max-page-size=16384"

  # configure_reproducible_build_metadata sets RUSTFLAGS for path remapping.
  # Cargo gives RUSTFLAGS precedence over target.<triple>.rustflags, so carry
  # the Android linker setting there as well while preserving the remap flags.
  # Tauri's Android Cargo invocations use --target, keeping these flags scoped
  # to the Android target rather than host build scripts and proc macros.
  append_env_words_once RUSTFLAGS "${page_size_flags}"

  # Tauri's Gradle task starts nested Cargo builds. Keep the target linker
  # settings file-backed so they survive that process boundary.
  if [ -f "${config_file}" ] \
    && ! grep -Fq "Generated by: scripts/ci/_common.sh (Android)" "${config_file}" \
    && ! grep -Fq "Generated by: scripts/setup-ios-cargo-config.sh" "${config_file}"; then
    echo "Refusing to overwrite unrecognized Cargo config: ${config_file}" >&2
    return 1
  fi

  mkdir -p "${cargo_dir}"
  cat > "${config_file}" <<'EOF'
# Auto-generated cargo config for Android 16 KiB page-size support
# Generated by: scripts/ci/_common.sh (Android)

[target.aarch64-linux-android]
rustflags = ["-C", "link-arg=-Wl,-z,max-page-size=16384"]

[target.armv7-linux-androideabi]
rustflags = ["-C", "link-arg=-Wl,-z,max-page-size=16384"]

[target.i686-linux-android]
rustflags = ["-C", "link-arg=-Wl,-z,max-page-size=16384"]

[target.x86_64-linux-android]
rustflags = ["-C", "link-arg=-Wl,-z,max-page-size=16384"]
EOF
}

remove_generated_android_cargo_config() {
  local config_file="${TAURI_DIR}/.cargo/config.toml"

  if [ ! -f "${config_file}" ]; then
    return 0
  fi

  if grep -Fq "Generated by: scripts/ci/_common.sh (Android)" "${config_file}"; then
    rm -f "${config_file}"
    rmdir "${TAURI_DIR}/.cargo" 2>/dev/null || true
  fi
}

verify_android_artifact_native_libraries() (
  set -euo pipefail

  local artifact="$1"
  local entry_pattern entry entry_count library native_library_count=0
  local tmp

  case "${artifact}" in
    *.apk)
      entry_pattern='^lib/[^/]+/[^/]+[.]so$'
      ;;
    *.aab)
      entry_pattern='^[^/]+/lib/[^/]+/[^/]+[.]so$'
      ;;
    *)
      echo "Unsupported Android artifact for native-library verification: ${artifact}" >&2
      return 1
      ;;
  esac

  tmp="$(mktemp -d)"
  trap 'rm -rf "${tmp:-}"' EXIT

  while IFS= read -r entry; do
    native_library_count=$((native_library_count + 1))
    entry_count="$(unzip -Z1 "${artifact}" | grep -Fxc -- "${entry}" || true)"
    if [ "${entry_count}" != "1" ]; then
      echo "Expected exactly one ${entry} in ${artifact}; found ${entry_count}." >&2
      return 1
    fi

    library="${tmp}/${native_library_count}.so"
    unzip -p "${artifact}" "${entry}" > "${library}"
    verify_android_elf_load_alignment "${library}" "$(basename "${artifact}")/${entry}"
  done < <(
    unzip -Z1 "${artifact}" \
      | grep -E "${entry_pattern}" \
      | LC_ALL=C sort
  )

  if [ "${native_library_count}" -eq 0 ]; then
    echo "Android artifact contains no native libraries: ${artifact}" >&2
    return 1
  fi

  printf 'verified-android-native-libraries  %s  %s\n' \
    "${native_library_count}" "$(basename "${artifact}")"
)

verify_android_onnxruntime_staged_libraries() {
  local version jni_dir abi library expected actual

  version="${ORT_ANDROID_VERSION:-$(onnxruntime_android_version)}"
  jni_dir="$(android_onnxruntime_jni_libs_dir)"

  while IFS= read -r abi; do
    library="${jni_dir}/${abi}/libonnxruntime.so"
    expected="$(onnxruntime_android_lib_sha256_for_version "${version}" "${abi}")"
    if [ ! -f "${library}" ]; then
      echo "Missing staged ONNX Runtime Android library: ${library}" >&2
      return 1
    fi

    actual="$(sha256_file "${library}" | awk '{ print $1 }')"
    if [ "${actual}" != "${expected}" ]; then
      echo "Staged ONNX Runtime Android library SHA-256 mismatch for ${abi}." >&2
      echo "expected=${expected}" >&2
      echo "actual=${actual}" >&2
      return 1
    fi

    printf 'verified-android-onnxruntime-staged  %s  %s\n' "${actual}" "${abi}"
    verify_android_elf_load_alignment "${library}" "jniLibs/${abi}/libonnxruntime.so"
  done < <(onnxruntime_android_abis)
}

android_zipalign() {
  local found

  if command -v zipalign >/dev/null 2>&1; then
    command -v zipalign
    return 0
  fi

  if [ -n "${ANDROID_HOME:-}" ]; then
    found="$(
      find -L "${ANDROID_HOME}/build-tools" -mindepth 2 -maxdepth 4 -type f -name zipalign 2>/dev/null \
        | LC_ALL=C sort -V \
        | tail -n 1 || true
    )"
    if [ -n "${found}" ]; then
      printf '%s\n' "${found}"
      return 0
    fi
  fi

  echo "zipalign is required to verify Android APK native-library alignment." >&2
  return 1
}

android_aapt2() {
  local found

  if command -v aapt2 >/dev/null 2>&1; then
    command -v aapt2
    return 0
  fi

  if [ -n "${ANDROID_HOME:-}" ]; then
    found="$(
      find -L "${ANDROID_HOME}/build-tools" -mindepth 2 -maxdepth 4 -type f -name aapt2 2>/dev/null \
        | LC_ALL=C sort -V \
        | tail -n 1 || true
    )"
    if [ -n "${found}" ]; then
      printf '%s\n' "${found}"
      return 0
    fi
  fi

  echo "aapt2 is required to verify the Android target SDK." >&2
  return 1
}

configured_android_target_sdk() {
  local gradle_properties="${TAURI_DIR}/gen/android/gradle.properties"
  local target_sdk

  target_sdk="$(sed -n 's/^maple[.]targetSdk=//p' "${gradle_properties}")"
  if ! [[ "${target_sdk}" =~ ^[0-9]+$ ]]; then
    echo "Could not read maple.targetSdk from ${gradle_properties}." >&2
    return 1
  fi

  printf '%s\n' "${target_sdk}"
}

verify_android_apk_target_sdk() {
  local artifact="$1"
  local aapt2 expected actual

  aapt2="$(android_aapt2)"
  expected="$(configured_android_target_sdk)"
  actual="$("${aapt2}" dump badging "${artifact}" | sed -n "s/^targetSdkVersion:'\\([^']*\\)'.*/\\1/p")"

  if [ "${actual}" != "${expected}" ]; then
    echo "Android APK target SDK mismatch for ${artifact}." >&2
    echo "expected=${expected}" >&2
    echo "actual=${actual:-missing}" >&2
    return 1
  fi

  printf 'verified-android-target-sdk  %s  %s\n' "${actual}" "$(basename "${artifact}")"
}

verify_android_onnxruntime_artifact() (
  set -euo pipefail

  local artifact="$1"
  local version prefix abi entry entry_count expected actual library all_entries_count zipalign
  local tmp

  version="${ORT_ANDROID_VERSION:-$(onnxruntime_android_version)}"
  case "${artifact}" in
    *.apk)
      prefix="lib"
      ;;
    *.aab)
      prefix="base/lib"
      ;;
    *)
      echo "Unsupported Android artifact for ONNX Runtime verification: ${artifact}" >&2
      return 1
      ;;
  esac

  tmp="$(mktemp -d)"
  trap 'rm -rf "${tmp}"' EXIT

  all_entries_count="$(unzip -Z1 "${artifact}" | grep -Ec '(^|/)libonnxruntime\.so$' || true)"
  if [ "${all_entries_count}" != "4" ]; then
    echo "Expected four libonnxruntime.so entries in ${artifact}; found ${all_entries_count}." >&2
    return 1
  fi

  while IFS= read -r abi; do
    entry="${prefix}/${abi}/libonnxruntime.so"
    entry_count="$(unzip -Z1 "${artifact}" | grep -Fxc -- "${entry}" || true)"
    if [ "${entry_count}" != "1" ]; then
      echo "Expected exactly one ${entry} in ${artifact}; found ${entry_count}." >&2
      return 1
    fi

    library="${tmp}/${abi}-libonnxruntime.so"
    unzip -p "${artifact}" "${entry}" > "${library}"
    expected="$(onnxruntime_android_lib_sha256_for_version "${version}" "${abi}")"
    actual="$(sha256_file "${library}" | awk '{ print $1 }')"
    if [ "${actual}" != "${expected}" ]; then
      echo "Packaged ONNX Runtime Android library SHA-256 mismatch for ${entry}." >&2
      echo "expected=${expected}" >&2
      echo "actual=${actual}" >&2
      return 1
    fi

    printf 'verified-android-onnxruntime-package  %s  %s  %s\n' \
      "${actual}" "$(basename "${artifact}")" "${entry}"
    verify_android_elf_load_alignment "${library}" "$(basename "${artifact}")/${entry}"
  done < <(onnxruntime_android_abis)

  if [[ "${artifact}" == *.apk ]]; then
    zipalign="$(android_zipalign)"
    "${zipalign}" -c -P 16 4 "${artifact}" >/dev/null
    printf 'verified-android-apk-zip-alignment  16384  %s\n' "$(basename "${artifact}")"
  fi
)

stage_windows_runtime_dlls() {
  require_windows_host "stage_windows_runtime_dlls"

  local vc_redist_version wix_cli_version

  vc_redist_version="$(windows_vc_redist_x64_version)"
  export MAPLE_WINDOWS_VC_REDIST_VERSION="${vc_redist_version}"
  export MAPLE_WINDOWS_VC_REDIST_URL
  export MAPLE_WINDOWS_VC_REDIST_SHA256
  MAPLE_WINDOWS_VC_REDIST_URL="$(windows_vc_redist_x64_url_for_version "${vc_redist_version}")"
  MAPLE_WINDOWS_VC_REDIST_SHA256="$(windows_vc_redist_x64_archive_sha256_for_version "${vc_redist_version}")"

  wix_cli_version="$(windows_wix_cli_version)"
  export MAPLE_WINDOWS_WIX_CLI_VERSION="${wix_cli_version}"
  export MAPLE_WINDOWS_WIX_CLI_URL
  export MAPLE_WINDOWS_WIX_CLI_SHA256
  MAPLE_WINDOWS_WIX_CLI_URL="$(windows_wix_cli_url_for_version "${wix_cli_version}")"
  MAPLE_WINDOWS_WIX_CLI_SHA256="$(windows_wix_cli_archive_sha256_for_version "${wix_cli_version}")"

  pwsh -NoLogo -NoProfile -ExecutionPolicy Bypass \
    -File "$(to_windows_path "${TAURI_DIR}/scripts/stage-windows-runtime-dlls.ps1")" \
    -OrtDllPath "$(to_windows_path "${ORT_DYLIB_PATH:?ORT_DYLIB_PATH is required}")" \
    -Destination "$(to_windows_path "${TAURI_DIR}/resources/windows")"
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1"
  else
    shasum -a 256 "$1"
  fi
}

sha256_stream() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum
  else
    shasum -a 256
  fi
}

decode_base64_string_to_file() {
  local value="$1"
  local out="$2"

  if printf '%s' "${value}" | base64 --decode > "${out}" 2>/dev/null; then
    return 0
  fi

  printf '%s' "${value}" | base64 -D > "${out}"
}

decode_base64_file_to_file() {
  local in_file="$1"
  local out_file="$2"

  if base64 --decode "${in_file}" > "${out_file}" 2>/dev/null; then
    return 0
  fi

  base64 -D -i "${in_file}" > "${out_file}"
}

encode_base64_file_to_string() {
  base64 < "$1" | tr -d '\n'
}

repo_relative_path() {
  local path="$1"
  printf '%s\n' "${path#"${REPO_ROOT}/"}"
}

print_file_hashes() {
  local file digest

  for file in "$@"; do
    if [ ! -f "${file}" ]; then
      continue
    fi

    digest="$(sha256_file "${file}" | awk '{ print $1 }')"
    printf 'sha256-file  %s  %s\n' "${digest}" "$(repo_relative_path "${file}")"
  done
}

tree_hash_digest() {
  local dir="$1"

  (
    cd "${dir}"
    find . -type f -print0 \
      | LC_ALL=C sort -z \
      | while IFS= read -r -d '' file; do
          file="${file#./}"
          printf '%s  %s\n' "$(sha256_file "${file}" | awk '{ print $1 }')" "${file}"
        done \
      | sha256_stream \
      | awk '{ print $1 }'
  )
}

print_canonical_ios_app_hash() {
  local bundle="$1"
  local label="${2:-$(repo_relative_path "${bundle}")}"
  local digest

  if [ ! -d "${bundle}" ]; then
    return 0
  fi

  digest="$(canonical_apple_bundle_hash_from_path_digest "${bundle}")"
  printf 'sha256-ios-app-canonical  %s  %s\n' "${digest}" "${label}"
}

scrub_host_metadata_files() {
  local root="$1"

  if [ ! -d "${root}" ]; then
    return 0
  fi

  find "${root}" \
    \( -name '.DS_Store' -o -name '._*' -o -name 'Thumbs.db' -o -name 'Desktop.ini' \) \
    -type f -print0 \
    | while IFS= read -r -d '' file; do
        rm -f "${file}"
      done
}

strip_apple_debug_symbols() {
  local root="$1"
  local file

  if [ ! -d "${root}" ] || ! command -v strip >/dev/null 2>&1; then
    return 0
  fi

  while IFS= read -r -d '' file; do
    if file "${file}" | grep -q 'Mach-O'; then
      strip -S "${file}"
    fi
  done < <(find "${root}" -type f -print0 | LC_ALL=C sort -z)
}

adhoc_resign_apple_bundle() {
  local bundle="$1"

  if [ ! -d "${bundle}" ] || ! command -v codesign >/dev/null 2>&1; then
    return 0
  fi

  codesign --force --sign - --timestamp=none --generate-entitlement-der "${bundle}"
}

remove_build_tree() {
  local root="$1"

  if [ ! -e "${root}" ]; then
    return 0
  fi

  chmod -R u+w "${root}" 2>/dev/null || true
  rm -rf "${root}"
}

linux_runtime_library_path() {
  local paths_file="${MAPLE_NIX_LINUX_CLOSURE_INFO:-}/store-paths"
  local lib_dirs=()
  local store_path lib_dir

  if [ -f "${paths_file}" ]; then
    while IFS= read -r store_path; do
      for lib_dir in "${store_path}/lib" "${store_path}/lib64"; do
        if [ -d "${lib_dir}" ]; then
          lib_dirs+=("${lib_dir}")
        fi
      done
    done < "${paths_file}"
  fi

  if [ -n "${MAPLE_NIX_GCC_LIB:-}" ] && [ -d "${MAPLE_NIX_GCC_LIB}/lib" ]; then
    lib_dirs+=("${MAPLE_NIX_GCC_LIB}/lib")
  fi

  if [ "${#lib_dirs[@]}" -eq 0 ]; then
    return 0
  fi

  printf '%s\n' "${lib_dirs[@]}" | awk '!seen[$0]++' | paste -sd ':' -
}

prepend_linux_runtime_library_path() {
  local library_path
  library_path="$(linux_runtime_library_path)"

  if [ -n "${library_path}" ]; then
    if [ "${MAPLE_LINUX_RUNTIME_LIBRARY_PATH_ACTIVE:-0}" = "1" ]; then
      return 0
    fi

    if [ "${LD_LIBRARY_PATH+x}" = "x" ]; then
      export MAPLE_LINUX_RUNTIME_ORIGINAL_LD_LIBRARY_PATH="${LD_LIBRARY_PATH}"
      export MAPLE_LINUX_RUNTIME_ORIGINAL_LD_LIBRARY_PATH_SET=1
    else
      unset MAPLE_LINUX_RUNTIME_ORIGINAL_LD_LIBRARY_PATH
      export MAPLE_LINUX_RUNTIME_ORIGINAL_LD_LIBRARY_PATH_SET=0
    fi
    export MAPLE_LINUX_RUNTIME_LIBRARY_PATH_ACTIVE=1
    export LD_LIBRARY_PATH="${library_path}${LD_LIBRARY_PATH:+:${LD_LIBRARY_PATH}}"
  fi
}

restore_linux_runtime_library_path() {
  if [ "${MAPLE_LINUX_RUNTIME_LIBRARY_PATH_ACTIVE:-0}" != "1" ]; then
    return 0
  fi

  if [ "${MAPLE_LINUX_RUNTIME_ORIGINAL_LD_LIBRARY_PATH_SET:-0}" = "1" ]; then
    export LD_LIBRARY_PATH="${MAPLE_LINUX_RUNTIME_ORIGINAL_LD_LIBRARY_PATH}"
  else
    unset LD_LIBRARY_PATH
  fi

  unset MAPLE_LINUX_RUNTIME_LIBRARY_PATH_ACTIVE
  unset MAPLE_LINUX_RUNTIME_ORIGINAL_LD_LIBRARY_PATH
  unset MAPLE_LINUX_RUNTIME_ORIGINAL_LD_LIBRARY_PATH_SET
}

linuxdeploy_tools_arch() {
  case "$(uname -m)" in
    x86_64 | amd64)
      printf '%s\n' "x86_64"
      ;;
    aarch64 | arm64)
      printf '%s\n' "aarch64"
      ;;
    *)
      echo "Unsupported Linux AppImage tool architecture: $(uname -m)" >&2
      return 1
      ;;
  esac
}

write_chmod_patchelf_wrapper() {
  local wrapper="$1"
  local real_patchelf="$2"
  local bash_path

  bash_path="/bin/bash"
  cat > "${wrapper}" <<EOF
#!${bash_path}
for arg in "\$@"; do
  if [ -f "\${arg}" ]; then
    chmod u+w "\${arg}" 2>/dev/null || true
  fi
done
exec "${real_patchelf}" "\$@"
EOF
  chmod +x "${wrapper}"
}

wrap_linuxdeploy_appdir_patchelf() {
  local appdir_bin="$1"
  local patchelf_path real_patchelf

  patchelf_path="${appdir_bin}/patchelf"
  real_patchelf="${appdir_bin}/patchelf.real"
  if [ ! -x "${patchelf_path}" ]; then
    return 0
  fi

  mv "${patchelf_path}" "${real_patchelf}"
  write_chmod_patchelf_wrapper "${patchelf_path}" "${real_patchelf}"
}

prepare_linuxdeploy_support_bin() {
  local support_bin="$1"
  local bash_path path_dir entry name target real_pkg_config real_patchelf support_path
  local -a path_dirs=()

  rm -rf "${support_bin}"
  mkdir -p "${support_bin}"

  support_path="${MAPLE_NIX_LINUXDEPLOY_SUPPORT_PATH:-}"
  if [ -z "${support_path}" ]; then
    echo "MAPLE_NIX_LINUXDEPLOY_SUPPORT_PATH is required for reproducible linuxdeploy support tooling." >&2
    return 1
  fi

  # This path is exported by flake.nix and contains only pinned Nix package bins.
  IFS=':' read -r -a path_dirs <<< "${support_path}"
  for path_dir in "${path_dirs[@]}"; do
    if [ -z "${path_dir}" ] || [ ! -d "${path_dir}" ] || [ "${path_dir}" = "${support_bin}" ]; then
      continue
    fi

    while IFS= read -r -d '' entry; do
      name="$(basename "${entry}")"
      case "${name}" in
        linuxdeploy-plugin-*)
          continue
          ;;
      esac

      if [ -e "${support_bin}/${name}" ] || [ -L "${support_bin}/${name}" ]; then
        continue
      fi

      target="$(readlink -f "${entry}" 2>/dev/null || true)"
      if [ -z "${target}" ] || [ ! -x "${target}" ]; then
        continue
      fi

      ln -s "${target}" "${support_bin}/${name}" 2>/dev/null || true
    done < <(find "${path_dir}" -maxdepth 1 \( -type f -o -type l \) -perm /111 -print0 2>/dev/null | LC_ALL=C sort -z)
  done

  bash_path="/bin/bash"
  real_pkg_config="$(resolve_bwrap_visible_command pkg-config pkgconf || true)"
  if [ -n "${real_pkg_config}" ]; then
    rm -f "${support_bin}/pkgconf" "${support_bin}/pkg-config"
    cat > "${support_bin}/pkgconf" <<EOF
#!${bash_path}
set -euo pipefail
if [ "\${1:-}" = "--variable=schemasdir" ] && [ "\${2:-}" = "gio-2.0" ] && [ -n "\${MAPLE_NIX_GLIB_SCHEMAS:-}" ]; then
  printf '%s\n' "/usr/share/glib-2.0/schemas"
  exit 0
fi
if [ "\${1:-}" = "--variable=exec_prefix" ] && [ "\${2:-}" = "gtk+-3.0" ] && [ -n "\${MAPLE_NIX_GTK_LIB:-}" ]; then
  printf '%s\n' "/usr"
  exit 0
fi
if [ "\${1:-}" = "--variable=libdir" ] && [ "\${2:-}" = "gtk+-3.0" ] && [ -n "\${MAPLE_NIX_GTK_LIB:-}" ]; then
  printf '%s\n' "/usr/lib"
  exit 0
fi
case "\${2:-}" in
  gobject-2.0 | gio-2.0 | librsvg-2.0 | pango | pangocairo | pangoft2)
    if [ "\${1:-}" = "--variable=libdir" ] && [ -n "\${MAPLE_NIX_LINUX_CLOSURE_INFO:-}" ]; then
      printf '%s\n' "/usr/lib"
      exit 0
    fi
    ;;
esac
if [ "\${2:-}" = "gdk-pixbuf-2.0" ] && [ -n "\${MAPLE_NIX_GDK_PIXBUF_BINARYDIR:-}" ]; then
  case "\${1:-}" in
    --variable=libdir)
      printf '%s\n' "/usr/lib"
      exit 0
      ;;
    --variable=gdk_pixbuf_binarydir)
      printf '%s\n' "/usr/lib/gdk-pixbuf-2.0/2.10.0"
      exit 0
      ;;
    --variable=gdk_pixbuf_cache_file)
      printf '%s\n' "/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"
      exit 0
      ;;
    --variable=gdk_pixbuf_moduledir)
      printf '%s\n' "/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders"
      exit 0
      ;;
  esac
fi
exec "${real_pkg_config}" "\$@"
EOF
    chmod +x "${support_bin}/pkgconf"
    ln -s "${support_bin}/pkgconf" "${support_bin}/pkg-config"
  fi

  real_patchelf="$(resolve_bwrap_visible_command patchelf || true)"
  if [ -n "${real_patchelf}" ]; then
    rm -f "${support_bin}/patchelf"
    write_chmod_patchelf_wrapper "${support_bin}/patchelf" "${real_patchelf}"
  fi
}

write_linuxdeploy_input_plugin_wrapper() {
  local wrapper="$1"
  local real_plugin_relative="$2"

  cat > "${wrapper}" <<EOF
#!/bin/bash
set -euo pipefail

for arg in "\$@"; do
  case "\${arg}" in
    --plugin-type)
      printf '%s\n' input
      exit 0
      ;;
    --plugin-api-version)
      printf '%s\n' 0
      exit 0
      ;;
  esac
done

script_dir="\$(CDPATH= cd -- "\$(dirname -- "\$0")" && pwd)"
real_plugin="\${script_dir}/${real_plugin_relative}"

if [ ! -x "\${real_plugin}" ]; then
  echo "Missing extracted linuxdeploy input plugin at \${real_plugin}" >&2
  exit 1
fi

exec "\${real_plugin}" "\$@"
EOF
  chmod +x "${wrapper}"
}

prepare_tauri_linuxdeploy_tools_cache() {
  if [ "$(host_os)" != "linux" ]; then
    return 0
  fi

  if [ -z "${MAPLE_NIX_TAURI_LINUXDEPLOY_TOOLS:-}" ]; then
    echo "MAPLE_NIX_TAURI_LINUXDEPLOY_TOOLS is required for reproducible AppImage bundling." >&2
    return 1
  fi

  local arch linuxdeploy_arch tools cache internal linuxdeploy_wrapper appimage_wrapper linuxdeploy_appdir_bin plugin_bin runtime_file appimage_real_appimage appimage_appdir gtk_real_plugin gstreamer_real_plugin
  arch="$(linuxdeploy_tools_arch)"
  linuxdeploy_arch="${arch}"
  tools="${MAPLE_NIX_TAURI_LINUXDEPLOY_TOOLS}"
  cache="${TAURI_DIR}/target/.tauri"
  internal="${cache}/maple-linuxdeploy-tools"
  linuxdeploy_wrapper="${cache}/linuxdeploy-${linuxdeploy_arch}.AppImage"
  appimage_wrapper="${cache}/linuxdeploy-plugin-appimage.AppImage"
  linuxdeploy_appdir_bin="${cache}/linuxdeploy-${linuxdeploy_arch}.AppDir/usr/bin"
  plugin_bin="${internal}/plugins"
  runtime_file="${internal}/appimage-runtime-${arch}"
  appimage_real_appimage="${internal}/linuxdeploy-plugin-appimage.real.AppImage"
  appimage_appdir="${internal}/linuxdeploy-plugin-appimage.AppDir"
  gtk_real_plugin="${internal}/linuxdeploy-plugin-gtk.real.sh"
  gstreamer_real_plugin="${internal}/linuxdeploy-plugin-gstreamer.real.sh"

  mkdir -p "${cache}" "${internal}"
  prepare_linuxdeploy_support_bin "${internal}/bin"
  rm -rf "${plugin_bin}"
  mkdir -p "${plugin_bin}"
  rm -f "${cache}/linuxdeploy-plugin-appimage.real.AppImage"
  rm -rf "${cache}/linuxdeploy-plugin-appimage.AppDir"
  install -m 0755 "${tools}/AppRun-${arch}" "${cache}/AppRun-${arch}"
  install -m 0755 "${tools}/linuxdeploy-${linuxdeploy_arch}.wrapper" "${linuxdeploy_wrapper}"
  install -m 0755 "${tools}/linuxdeploy-${linuxdeploy_arch}.AppImage" "${cache}/linuxdeploy-${linuxdeploy_arch}.real.AppImage"
  install -m 0755 "${tools}/linuxdeploy-plugin-appimage.real.AppImage" "${appimage_real_appimage}"
  install -m 0755 "${tools}/appimage-runtime-${arch}" "${runtime_file}"
  install -m 0755 "${tools}/linuxdeploy-plugin-gtk.sh" "${gtk_real_plugin}"
  install -m 0755 "${tools}/linuxdeploy-plugin-gstreamer.sh" "${gstreamer_real_plugin}"

  extract_appimage_tool "${cache}/linuxdeploy-${linuxdeploy_arch}.real.AppImage" "${cache}/linuxdeploy-${linuxdeploy_arch}.AppDir"
  extract_appimage_tool "${appimage_real_appimage}" "${appimage_appdir}"
  wrap_linuxdeploy_appdir_patchelf "${linuxdeploy_appdir_bin}"

  rm -f "${linuxdeploy_appdir_bin}"/linuxdeploy-plugin-*
  write_linuxdeploy_input_plugin_wrapper "${cache}/linuxdeploy-plugin-gtk.sh" "maple-linuxdeploy-tools/linuxdeploy-plugin-gtk.real.sh"
  write_linuxdeploy_input_plugin_wrapper "${cache}/linuxdeploy-plugin-gstreamer.sh" "maple-linuxdeploy-tools/linuxdeploy-plugin-gstreamer.real.sh"
  write_linuxdeploy_input_plugin_wrapper "${plugin_bin}/linuxdeploy-plugin-gtk" "../linuxdeploy-plugin-gtk.real.sh"
  write_linuxdeploy_input_plugin_wrapper "${plugin_bin}/linuxdeploy-plugin-gstreamer" "../linuxdeploy-plugin-gstreamer.real.sh"

  cat > "${appimage_wrapper}" <<EOF
#!/bin/bash
set -euo pipefail

stage_appdir_for_appimage() {
  local mode="\${MAPLE_STAGE_APPDIR_FOR_APPIMAGE:-auto}"
  local root="\$1"

  case "\${mode}" in
    1 | true | yes)
      return 0
      ;;
    0 | false | no)
      return 1
      ;;
    auto)
      case "\$(uname -s):\${root}" in
        Linux:/Users/*)
          return 0
          ;;
        *)
          return 1
          ;;
      esac
      ;;
    *)
      echo "Unsupported MAPLE_STAGE_APPDIR_FOR_APPIMAGE=\${mode}; expected auto, 1, or 0." >&2
      return 1
      ;;
  esac
}

sanitize_appdir_executable() {
  local appdir="\$1"
  local exe="\${appdir}/usr/bin/maple"
  local description interpreter rpath

  if [ ! -f "\${exe}" ]; then
    return 0
  fi

  description="\$(LC_ALL=C file "\${exe}")"
  case "\${description}" in
    *"x86-64"*)
      interpreter="/lib64/ld-linux-x86-64.so.2"
      ;;
    *"aarch64"*)
      interpreter="/lib/ld-linux-aarch64.so.1"
      ;;
    *)
      echo "Unsupported AppImage ELF architecture: \${description}" >&2
      return 1
      ;;
  esac

  patchelf --set-interpreter "\${interpreter}" --remove-rpath "\${exe}"
  rpath="\$(patchelf --print-rpath "\${exe}" 2>/dev/null || true)"
  printf 'verified-linux-appimage-appdir-elf  interpreter=%s  rpath=%s  %s\n' \
    "\${interpreter}" \
    "\${rpath:-<empty>}" \
    "\${exe}"
}

run_appimage_plugin() {
  local tmp_dir="" tmp_appdir="" output_parent="" status file previous arg
  local -a plugin_args=()

  if [ -z "\${appdir}" ] || ! stage_appdir_for_appimage "\${appdir}"; then
    exec "\${real_plugin}" "\$@"
  fi

  output_parent="\$(CDPATH= cd -- "\$(dirname -- "\${appdir}")" && pwd)"
  tmp_dir="\$(mktemp -d)"
  trap 'rm -rf "\${tmp_dir}"' EXIT
  tmp_appdir="\${tmp_dir}/\$(basename -- "\${appdir}")"
  cp -a --no-preserve=xattr "\${appdir}" "\${tmp_appdir}"

  previous=""
  for arg in "\$@"; do
    if [ "\${previous}" = "--appdir" ]; then
      plugin_args+=("\${tmp_appdir}")
      previous=""
      continue
    fi

    case "\${arg}" in
      --appdir=*)
        plugin_args+=("--appdir=\${tmp_appdir}")
        ;;
      --appdir)
        plugin_args+=("--appdir")
        previous="--appdir"
        ;;
      *)
        plugin_args+=("\${arg}")
        ;;
    esac
  done

  set +e
  "\${real_plugin}" "\${plugin_args[@]}"
  status="\$?"
  set -e

  if [ "\${status}" -eq 0 ]; then
    while IFS= read -r -d '' file; do
      mv -f "\${file}" "\${output_parent}/\$(basename -- "\${file}")"
    done < <(find "\${tmp_dir}" -maxdepth 1 -type f -name '*.AppImage' -print0)
  fi

  rm -rf "\${tmp_dir}"
  trap - EXIT
  exit "\${status}"
}

for arg in "\$@"; do
  case "\${arg}" in
    --plugin-type)
      printf '%s\n' output
      exit 0
      ;;
    --plugin-api-version)
      printf '%s\n' 0
      exit 0
      ;;
  esac
done

appdir=""
previous=""
for arg in "\$@"; do
  if [ "\${previous}" = "--appdir" ]; then
    appdir="\${arg}"
    previous=""
    continue
  fi

  case "\${arg}" in
    --appdir=*)
      appdir="\${arg#--appdir=}"
      ;;
    --appdir)
      previous="--appdir"
      ;;
  esac
done

if [ -n "\${appdir}" ]; then
  source "${SCRIPT_DIR}/_common.sh"
  prepare_linux_appdir_for_appimage "\${appdir}"
fi

script_dir="\$(CDPATH= cd -- "\$(dirname -- "\$0")" && pwd)"
real_plugin="\${script_dir}/maple-linuxdeploy-tools/linuxdeploy-plugin-appimage.AppDir/AppRun"
export LDAI_RUNTIME_FILE="\${script_dir}/maple-linuxdeploy-tools/appimage-runtime-${arch}"

if [ ! -x "\${real_plugin}" ]; then
  echo "Missing extracted linuxdeploy AppImage plugin at \${real_plugin}" >&2
  exit 1
fi

run_appimage_plugin "\$@"
EOF
  chmod +x "${appimage_wrapper}"

  rm -f "${plugin_bin}/linuxdeploy-plugin-appimage"
  cat > "${plugin_bin}/linuxdeploy-plugin-appimage" <<EOF
#!/bin/bash
set -euo pipefail

stage_appdir_for_appimage() {
  local mode="\${MAPLE_STAGE_APPDIR_FOR_APPIMAGE:-auto}"
  local root="\$1"

  case "\${mode}" in
    1 | true | yes)
      return 0
      ;;
    0 | false | no)
      return 1
      ;;
    auto)
      case "\$(uname -s):\${root}" in
        Linux:/Users/*)
          return 0
          ;;
        *)
          return 1
          ;;
      esac
      ;;
    *)
      echo "Unsupported MAPLE_STAGE_APPDIR_FOR_APPIMAGE=\${mode}; expected auto, 1, or 0." >&2
      return 1
      ;;
  esac
}

sanitize_appdir_executable() {
  local appdir="\$1"
  local exe="\${appdir}/usr/bin/maple"
  local description interpreter rpath

  if [ ! -f "\${exe}" ]; then
    return 0
  fi

  description="\$(LC_ALL=C file "\${exe}")"
  case "\${description}" in
    *"x86-64"*)
      interpreter="/lib64/ld-linux-x86-64.so.2"
      ;;
    *"aarch64"*)
      interpreter="/lib/ld-linux-aarch64.so.1"
      ;;
    *)
      echo "Unsupported AppImage ELF architecture: \${description}" >&2
      return 1
      ;;
  esac

  patchelf --set-interpreter "\${interpreter}" --remove-rpath "\${exe}"
  rpath="\$(patchelf --print-rpath "\${exe}" 2>/dev/null || true)"
  printf 'verified-linux-appimage-appdir-elf  interpreter=%s  rpath=%s  %s\n' \
    "\${interpreter}" \
    "\${rpath:-<empty>}" \
    "\${exe}"
}

run_appimage_plugin() {
  local tmp_dir="" tmp_appdir="" output_parent="" status file previous arg
  local -a plugin_args=()

  if [ -z "\${appdir}" ] || ! stage_appdir_for_appimage "\${appdir}"; then
    exec "\${real_plugin}" "\$@"
  fi

  output_parent="\$(CDPATH= cd -- "\$(dirname -- "\${appdir}")" && pwd)"
  tmp_dir="\$(mktemp -d)"
  trap 'rm -rf "\${tmp_dir}"' EXIT
  tmp_appdir="\${tmp_dir}/\$(basename -- "\${appdir}")"
  cp -a --no-preserve=xattr "\${appdir}" "\${tmp_appdir}"

  previous=""
  for arg in "\$@"; do
    if [ "\${previous}" = "--appdir" ]; then
      plugin_args+=("\${tmp_appdir}")
      previous=""
      continue
    fi

    case "\${arg}" in
      --appdir=*)
        plugin_args+=("--appdir=\${tmp_appdir}")
        ;;
      --appdir)
        plugin_args+=("--appdir")
        previous="--appdir"
        ;;
      *)
        plugin_args+=("\${arg}")
        ;;
    esac
  done

  set +e
  "\${real_plugin}" "\${plugin_args[@]}"
  status="\$?"
  set -e

  if [ "\${status}" -eq 0 ]; then
    while IFS= read -r -d '' file; do
      mv -f "\${file}" "\${output_parent}/\$(basename -- "\${file}")"
    done < <(find "\${tmp_dir}" -maxdepth 1 -type f -name '*.AppImage' -print0)
  fi

  rm -rf "\${tmp_dir}"
  trap - EXIT
  exit "\${status}"
}

for arg in "\$@"; do
  case "\${arg}" in
    --plugin-type)
      printf '%s\n' output
      exit 0
      ;;
    --plugin-api-version)
      printf '%s\n' 0
      exit 0
      ;;
  esac
done

appdir=""
previous=""
for arg in "\$@"; do
  if [ "\${previous}" = "--appdir" ]; then
    appdir="\${arg}"
    previous=""
    continue
  fi

  case "\${arg}" in
    --appdir=*)
      appdir="\${arg#--appdir=}"
      ;;
    --appdir)
      previous="--appdir"
      ;;
  esac
done

if [ -n "\${appdir}" ]; then
  source "${SCRIPT_DIR}/_common.sh"
  prepare_linux_appdir_for_appimage "\${appdir}"
fi

script_dir="\$(CDPATH= cd -- "\$(dirname -- "\$0")" && pwd)"
real_plugin="\${script_dir}/../linuxdeploy-plugin-appimage.AppDir/AppRun"
export LDAI_RUNTIME_FILE="\${script_dir}/../appimage-runtime-${arch}"

if [ ! -x "\${real_plugin}" ]; then
  echo "Missing extracted linuxdeploy AppImage plugin at \${real_plugin}" >&2
  exit 1
fi

run_appimage_plugin "\$@"
EOF
  chmod +x "${plugin_bin}/linuxdeploy-plugin-appimage"

  print_file_hashes \
    "${cache}/AppRun-${arch}" \
    "${cache}/linuxdeploy-${linuxdeploy_arch}.real.AppImage" \
    "${cache}/linuxdeploy-${linuxdeploy_arch}.AppImage" \
    "${cache}/linuxdeploy-${linuxdeploy_arch}.AppDir/AppRun" \
    "${plugin_bin}/linuxdeploy-plugin-appimage" \
    "${plugin_bin}/linuxdeploy-plugin-gtk" \
    "${plugin_bin}/linuxdeploy-plugin-gstreamer" \
    "${appimage_real_appimage}" \
    "${cache}/linuxdeploy-plugin-appimage.AppImage" \
    "${appimage_appdir}/AppRun" \
    "${runtime_file}" \
    "${cache}/linuxdeploy-plugin-gtk.sh" \
    "${cache}/linuxdeploy-plugin-gstreamer.sh"
}

verify_linuxdeploy_plugin_metadata() {
  if [ "$(host_os)" != "linux" ]; then
    return 0
  fi

  local plugin api_version plugin_type
  for plugin in "${TAURI_DIR}/target/.tauri/maple-linuxdeploy-tools/plugins"/linuxdeploy-plugin-*; do
    if [ ! -e "${plugin}" ]; then
      continue
    fi
    if [ ! -x "${plugin}" ]; then
      echo "linuxdeploy plugin is not executable: $(repo_relative_path "${plugin}")" >&2
      return 1
    fi

    api_version="$(run_with_nix_usr_bin "${plugin}" --plugin-api-version)"
    plugin_type="$(run_with_nix_usr_bin "${plugin}" --plugin-type)"

    if [ "${api_version}" != "0" ]; then
      echo "linuxdeploy plugin reported unsupported API ${api_version}: $(repo_relative_path "${plugin}")" >&2
      return 1
    fi

    case "${plugin_type}" in
      input | output)
        ;;
      *)
        echo "linuxdeploy plugin reported unsupported type ${plugin_type}: $(repo_relative_path "${plugin}")" >&2
        return 1
        ;;
    esac

    printf 'verified-linuxdeploy-plugin  type=%s  api=%s  %s\n' "${plugin_type}" "${api_version}" "$(repo_relative_path "${plugin}")"
  done
}

extract_appimage_tool() {
  local appimage="$1"
  local out="$2"
  local offset tmp

  command -v unsquashfs >/dev/null 2>&1 || {
    echo "unsquashfs is required to extract AppImage tools without executing their runtime." >&2
    return 1
  }

  if ! offset="$(appimage_squashfs_offset "${appimage}")"; then
    echo "Could not locate embedded SquashFS payload in AppImage: ${appimage}" >&2
    return 1
  fi

  tmp="$(mktemp -d)"
  rm -rf "${out}"

  if ! unsquashfs -d "${tmp}/squashfs-root" -o "${offset}" "${appimage}" >/dev/null; then
    rm -rf "${tmp}"
    return 1
  fi

  if [ ! -x "${tmp}/squashfs-root/AppRun" ]; then
    echo "Extracted AppImage is missing AppRun: ${appimage}" >&2
    rm -rf "${tmp}"
    return 1
  fi

  mv "${tmp}/squashfs-root" "${out}"
  chmod -R u+w "${out}" 2>/dev/null || true
  rm -rf "${tmp}"
}

appimage_squashfs_offset() {
  local appimage="$1"
  local offset _

  while IFS=: read -r offset _; do
    if unsquashfs -s -o "${offset}" "${appimage}" >/dev/null 2>&1; then
      printf '%s\n' "${offset}"
      return 0
    fi
  done < <(LC_ALL=C grep -abo 'hsqs' "${appimage}" || true)

  return 1
}

print_tree_hash() {
  local dir="$1"
  local label="${2:-$(repo_relative_path "${dir}")}"
  local digest

  if [ ! -d "${dir}" ]; then
    return 0
  fi

  digest="$(tree_hash_digest "${dir}")"

  printf 'sha256-tree  %s  %s\n' "${digest}" "${label}"
}

write_sha256_manifest() {
  local out="$1"
  shift

  mkdir -p "$(dirname "${out}")"
  : > "${out}"

  local file digest
  for file in "$@"; do
    if [ -f "${file}" ]; then
      digest="$(sha256_file "${file}" | awk '{ print $1 }')"
      printf '%s  %s\n' "${digest}" "$(repo_relative_path "${file}")" >> "${out}"
    fi
  done

  test -s "${out}"
  cat "${out}"
}

print_zip_payload_hash() {
  local archive="$1"
  local label="${2:-$(repo_relative_path "${archive}")}"
  local digest

  digest="$(zip_payload_hash_digest "${archive}")"

  printf 'sha256-zip-payload  %s  %s\n' "${digest}" "${label}"
}

zip_payload_hash_digest() {
  local archive="$1"

  unzip -Z1 "${archive}" \
    | LC_ALL=C sort \
    | while IFS= read -r entry; do
        case "${entry}" in
          */)
            continue
            ;;
          META-INF/MANIFEST.MF | META-INF/*.SF | META-INF/*.RSA | META-INF/*.DSA | META-INF/*.EC)
            continue
            ;;
        esac

        printf '%s  %s\n' "$(zip_payload_entry_hash_digest "${archive}" "${entry}")" "${entry}"
      done \
    | sha256_stream \
    | awk '{ print $1 }'
}

zip_payload_entry_hash_digest() {
  local archive="$1"
  local entry="$2"

  case "${entry}" in
    assets/tauri.conf.json | */assets/tauri.conf.json)
      unzip -p "${archive}" "${entry}" | jq -cS . | sha256_stream | awk '{ print $1 }'
      ;;
    *)
      unzip -p "${archive}" "${entry}" | sha256_stream | awk '{ print $1 }'
      ;;
  esac
}

normalize_zip_json_entry() {
  local archive="$1"
  local entry="$2"
  local tmp

  if ! unzip -Z1 "${archive}" | grep -Fx -- "${entry}" >/dev/null; then
    return 0
  fi

  command -v zip >/dev/null 2>&1 || {
    echo "zip is required to normalize ${entry} in ${archive}. Run through the flake CI shell." >&2
    return 1
  }

  tmp="$(mktemp -d)"
  mkdir -p "${tmp}/$(dirname "${entry}")"
  unzip -p "${archive}" "${entry}" | jq -cS . > "${tmp}/${entry}"
  touch_tree_to_source_date_epoch "${tmp}"

  (
    cd "${tmp}"
    zip -X -q "${archive}" "${entry}"
  )

  rm -rf "${tmp}"
}

normalize_android_zip_metadata() {
  local archive="$1"

  normalize_zip_json_entry "${archive}" "assets/tauri.conf.json"
  normalize_zip_json_entry "${archive}" "base/assets/tauri.conf.json"
}

remove_apple_signing_metadata() {
  local root="$1"

  scrub_host_metadata_files "${root}"

  while IFS= read -r -d '' dir; do
    rm -rf "${dir}"
  done < <(find "${root}" -type d -name '_CodeSignature' -print0)

  find "${root}" \
    \( -name 'CodeResources' \
      -o -name 'embedded.mobileprovision' \
      -o -name 'archived-expanded-entitlements.xcent' \) \
    -type f -delete

  if command -v codesign >/dev/null 2>&1; then
    while IFS= read -r -d '' bundle; do
      codesign --remove-signature "${bundle}" >/dev/null 2>&1 || true
    done < <(find "${root}" -type d \( -name '*.app' -o -name '*.appex' -o -name '*.framework' -o -name '*.xpc' \) -print0 | LC_ALL=C sort -z -r)

    while IFS= read -r -d '' file; do
      if file "${file}" | grep -q 'Mach-O'; then
        codesign --remove-signature "${file}" >/dev/null 2>&1 || true
      fi
    done < <(find "${root}" -type f -print0 | LC_ALL=C sort -z)
  fi

  while IFS= read -r -d '' dir; do
    rm -rf "${dir}"
  done < <(find "${root}" -type d -name '_CodeSignature' -print0)

  scrub_host_metadata_files "${root}"
}

python3_runner() {
  local candidate

  for candidate in python3 python py; do
    if command -v "${candidate}" >/dev/null 2>&1; then
      printf '%s\n' "${candidate}"
      return 0
    fi
  done

  echo "Python 3 is required but python3, python, and py were not found." >&2
  return 1
}

print_canonical_apple_bundle_hash() {
  local bundle="$1"
  local label="${2:-$(repo_relative_path "${bundle}")}"
  local digest

  if [ ! -d "${bundle}" ]; then
    return 0
  fi

  digest="$(canonical_apple_bundle_hash_from_path_digest "${bundle}")"

  printf 'sha256-apple-unsigned-tree  %s  %s\n' "${digest}" "${label}"
}

canonical_apple_bundle_hash_from_path_digest() {
  local bundle="$1"
  local tmp digest

  tmp="$(mktemp -d)"
  cp -a "${bundle}" "${tmp}/bundle"
  remove_apple_signing_metadata "${tmp}/bundle"
  digest="$(canonical_apple_bundle_hash_digest "${tmp}/bundle")"
  rm -rf "${tmp}"

  printf '%s\n' "${digest}"
}

canonical_apple_bundle_hash_digest() {
  local bundle="$1"
  "$(python3_runner)" "${REPO_ROOT}/scripts/ci/canonical-ios-app-hash.py" "${bundle}"
}

find_canonical_app_bundle_under_dir() {
  local root="$1"
  find "${root}" -type d -name '*.app' -print 2>/dev/null | LC_ALL=C sort | head -n 1
}

print_canonical_app_tar_payload_hash() {
  local archive="$1"
  local label="${2:-$(repo_relative_path "${archive}")}"
  local tmp app digest

  if [ ! -f "${archive}" ]; then
    return 0
  fi

  tmp="$(mktemp -d)"
  tar -xzf "${archive}" -C "${tmp}"

  app="$(find_canonical_app_bundle_under_dir "${tmp}")"
  if [ -z "${app}" ]; then
    echo "Could not find a *.app bundle in ${archive}" >&2
    rm -rf "${tmp}"
    return 1
  fi

  digest="$(canonical_apple_bundle_hash_from_path_digest "${app}")"
  rm -rf "${tmp}"

  printf 'sha256-macos-app-tar-canonical  %s  %s::*.app\n' "${digest}" "${label}"
}

print_canonical_dmg_app_hash() {
  local dmg="$1"
  local label="${2:-$(repo_relative_path "${dmg}")}"
  local tmp mount app digest

  if [ ! -f "${dmg}" ]; then
    return 0
  fi

  if [ "$(host_os)" != "darwin" ]; then
    echo "DMG canonicalization requires macOS: ${dmg}" >&2
    return 1
  fi

  tmp="$(mktemp -d)"
  mount="${tmp}/mount"
  mkdir -p "${mount}"

  if ! hdiutil attach -nobrowse -readonly -mountpoint "${mount}" "${dmg}" >/dev/null; then
    rm -rf "${tmp}"
    return 1
  fi

  app="$(find_canonical_app_bundle_under_dir "${mount}")"
  if [ -z "${app}" ]; then
    hdiutil detach "${mount}" -quiet || hdiutil detach "${mount}" -force -quiet || true
    rm -rf "${tmp}"
    echo "Could not find a *.app bundle in ${dmg}" >&2
    return 1
  fi

  digest="$(canonical_apple_bundle_hash_from_path_digest "${app}")"
  hdiutil detach "${mount}" -quiet || hdiutil detach "${mount}" -force -quiet || true
  rm -rf "${tmp}"

  printf 'sha256-macos-dmg-canonical  %s  %s::*.app\n' "${digest}" "${label}"
}

print_canonical_ipa_payload_hash() {
  local ipa="$1"
  local label="${2:-$(repo_relative_path "${ipa}")}"
  local tmp app digest

  if [ ! -f "${ipa}" ]; then
    return 0
  fi

  tmp="$(mktemp -d)"
  unzip -qq "${ipa}" -d "${tmp}"

  app="$(find "${tmp}/Payload" -mindepth 1 -maxdepth 1 -type d -name '*.app' 2>/dev/null | LC_ALL=C sort | head -n 1)"
  if [ -z "${app}" ]; then
    echo "Could not find a Payload/*.app bundle in ${ipa}" >&2
    rm -rf "${tmp}"
    return 1
  fi

  remove_apple_signing_metadata "${app}"
  digest="$(canonical_apple_bundle_hash_digest "${app}")"
  rm -rf "${tmp}"

  printf 'sha256-ios-unsigned-app-tree  %s  %s::Payload/*.app\n' "${digest}" "${label}"
}

print_canonical_windows_nsis_payload_hash() {
  local installer="$1"
  local label="${2:-$(repo_relative_path "${installer}")}"

  "$(python3_runner)" "${REPO_ROOT}/scripts/ci/canonical-windows-nsis-payload-hash.py" \
    "${installer}" \
    "${label}"
}

write_windows_signed_canonical_manifest() {
  local manifest="$1"
  local signed_installer="$2"
  local unsigned_installer="$3"
  local signed_label unsigned_label output

  signed_label="$(repo_relative_path "${signed_installer}")"
  unsigned_label="$(repo_relative_path "${unsigned_installer}")"

  output="$(
    "$(python3_runner)" "${REPO_ROOT}/scripts/ci/canonical-windows-nsis-payload-hash.py" \
      --compare \
      "${signed_installer}" \
      "${unsigned_installer}" \
      "${signed_label}" \
      "${unsigned_label}"
  )"
  printf '%s\n' "${output}"
  printf '%s\n' "${output}" | awk '$1 == "sha256-windows-nsis-payload-canonical" { print }' > "${manifest}"

  if [ ! -s "${manifest}" ]; then
    echo "Windows signed canonical manifest was not written: ${manifest}" >&2
    return 1
  fi
}

tauri_updater_public_key_file() {
  local out="$1"
  local encoded decoded

  if [ -n "${MAPLE_TAURI_UPDATER_PUBLIC_KEY_PATH:-}" ]; then
    if [ ! -f "${MAPLE_TAURI_UPDATER_PUBLIC_KEY_PATH}" ]; then
      echo "MAPLE_TAURI_UPDATER_PUBLIC_KEY_PATH does not exist: ${MAPLE_TAURI_UPDATER_PUBLIC_KEY_PATH}" >&2
      return 1
    fi

    cp "${MAPLE_TAURI_UPDATER_PUBLIC_KEY_PATH}" "${out}"
  elif [ -n "${MAPLE_TAURI_UPDATER_PUBLIC_KEY:-}" ]; then
    printf '%s' "${MAPLE_TAURI_UPDATER_PUBLIC_KEY}" > "${out}"
  else
    encoded="$(jq -er '.plugins.updater.pubkey' "${TAURI_DIR}/tauri.conf.json")"
    decode_base64_string_to_file "${encoded}" "${out}"
  fi

  if grep -q 'minisign public key' "${out}"; then
    return 0
  fi

  decoded="$(mktemp)"
  if decode_base64_file_to_file "${out}" "${decoded}" && grep -q 'minisign public key' "${decoded}"; then
    mv "${decoded}" "${out}"
    return 0
  fi
  rm -f "${decoded}"

  echo "Configured Tauri updater public key is not a minisign public key." >&2
  return 1
}

tauri_updater_public_key_config_value() {
  local tmp encoded

  if [ -n "${MAPLE_TAURI_UPDATER_PUBLIC_KEY_PATH:-}" ] || [ -n "${MAPLE_TAURI_UPDATER_PUBLIC_KEY:-}" ]; then
    tmp="$(mktemp)"
    tauri_updater_public_key_file "${tmp}" || {
      rm -f "${tmp}"
      return 1
    }
    encoded="$(encode_base64_file_to_string "${tmp}")"
    rm -f "${tmp}"
    printf '%s\n' "${encoded}"
    return 0
  fi

  jq -er '.plugins.updater.pubkey' "${TAURI_DIR}/tauri.conf.json"
}

verify_tauri_updater_signature() {
  local artifact="$1"
  local signature="$2"
  local label="${3:-$(repo_relative_path "${artifact}")}"
  local tmp pubkey decoded_signature

  if [ ! -f "${artifact}" ]; then
    echo "Missing updater artifact for signature verification: ${artifact}" >&2
    return 1
  fi

  if [ ! -f "${signature}" ]; then
    echo "Missing updater signature for ${artifact}: ${signature}" >&2
    return 1
  fi

  command -v minisign >/dev/null 2>&1 || {
    echo "minisign is required to verify Tauri updater signatures. Run through the flake CI shell." >&2
    return 1
  }

  tmp="$(mktemp -d)"
  pubkey="${tmp}/tauri-updater.pub"
  decoded_signature="${tmp}/artifact.minisig"

  tauri_updater_public_key_file "${pubkey}"
  decode_base64_file_to_file "${signature}" "${decoded_signature}"

  if ! env -u LD_LIBRARY_PATH minisign -Vm "${artifact}" -p "${pubkey}" -x "${decoded_signature}" -q; then
    rm -rf "${tmp}"
    return 1
  fi
  rm -rf "${tmp}"

  printf 'verified-tauri-updater-signature  %s  %s\n' "${label}" "$(repo_relative_path "${signature}")"
}

sign_tauri_updater_artifact() {
  local artifact="$1"

  if [ ! -f "${artifact}" ]; then
    echo "Missing updater artifact to sign: ${artifact}" >&2
    return 1
  fi

  configure_tauri_updater_signing_key

  if [ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ] && [ -z "${TAURI_SIGNING_PRIVATE_KEY_PATH:-}" ]; then
    echo "TAURI_SIGNING_PRIVATE_KEY or TAURI_SIGNING_PRIVATE_KEY_PATH is required to sign ${artifact}." >&2
    return 1
  fi

  (
    cd "${FRONTEND_DIR}"
    env -u LD_LIBRARY_PATH bun tauri signer sign "${artifact}" >/dev/null
  )
}

sign_tauri_updater_artifacts() {
  local artifact

  for artifact in "$@"; do
    sign_tauri_updater_artifact "${artifact}"
  done
}

verify_tauri_updater_signature_files() {
  local signature artifact

  for signature in "$@"; do
    case "${signature}" in
      *.sig)
        ;;
      *)
        continue
        ;;
    esac

    if [ ! -f "${signature}" ]; then
      continue
    fi

    artifact="${signature%.sig}"
    verify_tauri_updater_signature "${artifact}" "${signature}" "$(repo_relative_path "${artifact}")"
  done
}

resolve_bwrap_visible_tool() {
  local candidate resolved

  for candidate in "$@"; do
    if [ -z "${candidate}" ]; then
      continue
    fi

    resolved="$(readlink -f "${candidate}" 2>/dev/null || printf '%s\n' "${candidate}")"
    case "${resolved}" in
      /usr/* | /bin/*)
        continue
        ;;
    esac

    printf '%s\n' "${resolved}"
    return 0
  done

  return 1
}

resolve_bwrap_visible_command() {
  local candidate name

  for name in "$@"; do
    while IFS= read -r candidate; do
      if resolve_bwrap_visible_tool "${candidate}"; then
        return 0
      fi
    done < <(type -a -P "${name}" 2>/dev/null || true)
  done

  return 1
}

install_bwrap_command_link() {
  local dest_dir="$1"
  local name="$2"
  local source_path="$3"
  local resolved

  if [ -z "${source_path}" ]; then
    return 0
  fi

  resolved="$(readlink -f "${source_path}" 2>/dev/null || printf '%s\n' "${source_path}")"
  if [ ! -x "${resolved}" ]; then
    return 0
  fi

  rm -f "${dest_dir}/${name}"
  case "${resolved}" in
    /bin/* | /usr/*)
      cp -L "${resolved}" "${dest_dir}/${name}"
      chmod +x "${dest_dir}/${name}"
      ;;
    *)
      ln -s "${resolved}" "${dest_dir}/${name}"
      ;;
  esac
}

run_with_nix_usr_bin() {
  if [ "$(host_os)" != "linux" ]; then
    "$@"
    return $?
  fi

  if [ -z "${MAPLE_NIX_LINUX_CLOSURE_INFO:-}" ] && [ -x /usr/bin/xdg-mime ] && [ -x /bin/bash ]; then
    "$@"
    return $?
  fi

  command -v bwrap >/dev/null 2>&1 || {
    echo "bubblewrap is required to provide Nix xdg-utils at /usr/bin for AppImage bundling." >&2
    return 1
  }

  local bash_path bin_dir tool_bin usr_root tool tool_path
  bash_path="/bin/bash"
  bin_dir="$(mktemp -d)"
  tool_bin="$(mktemp -d)"
  usr_root="$(mktemp -d)"
  mkdir -p "${usr_root}/bin" "${usr_root}/lib" "${usr_root}/share/glib-2.0"

  for tool in bash sh; do
    tool_path="$(command -v "${tool}" 2>/dev/null || true)"
    install_bwrap_command_link "${bin_dir}" "${tool}" "${tool_path}"
    install_bwrap_command_link "${usr_root}/bin" "${tool}" "${tool_path}"
  done

  for tool in env xdg-mime xdg-open update-desktop-database; do
    tool_path="$(command -v "${tool}" 2>/dev/null || true)"
    install_bwrap_command_link "${usr_root}/bin" "${tool}" "${tool_path}"
  done

  local paths_file="${MAPLE_NIX_LINUX_CLOSURE_INFO:-}/store-paths"
  local store_path lib_dir lib_file
  if [ -f "${paths_file}" ]; then
    while IFS= read -r store_path; do
      for lib_dir in "${store_path}/lib" "${store_path}/lib64"; do
        if [ ! -d "${lib_dir}" ]; then
          continue
        fi

        while IFS= read -r -d '' lib_file; do
          case "${lib_file}" in
            *-gdb.py | *.debug)
              continue
              ;;
          esac
          ln -sf "$(readlink -f "${lib_file}")" "${usr_root}/lib/$(basename "${lib_file}")"
        done < <(find "${lib_dir}" -maxdepth 1 \( -type f -o -type l \) -name '*.so*' -print0 | LC_ALL=C sort -z)
      done
    done < "${paths_file}"
  fi

  if [ -n "${MAPLE_NIX_GTK_LIB:-}" ] && [ -d "${MAPLE_NIX_GTK_LIB}/gtk-3.0" ]; then
    cp -a "${MAPLE_NIX_GTK_LIB}/gtk-3.0" "${usr_root}/lib/gtk-3.0"
    chmod -R u+w "${usr_root}/lib/gtk-3.0" 2>/dev/null || true
  fi

  if [ -n "${MAPLE_NIX_GLIB_SCHEMAS:-}" ] && [ -d "${MAPLE_NIX_GLIB_SCHEMAS}" ]; then
    mkdir -p "${usr_root}/share/glib-2.0/schemas"
    cp -a "${MAPLE_NIX_GLIB_SCHEMAS}/." "${usr_root}/share/glib-2.0/schemas/"
    chmod -R u+w "${usr_root}/share/glib-2.0/schemas" 2>/dev/null || true
  fi

  if [ -n "${MAPLE_NIX_GDK_PIXBUF_BINARYDIR:-}" ] && [ -d "${MAPLE_NIX_GDK_PIXBUF_BINARYDIR}" ]; then
    mkdir -p "${usr_root}/lib/gdk-pixbuf-2.0/2.10.0"
    cp -a "${MAPLE_NIX_GDK_PIXBUF_BINARYDIR}/." "${usr_root}/lib/gdk-pixbuf-2.0/2.10.0/"
    chmod -R u+w "${usr_root}/lib/gdk-pixbuf-2.0/2.10.0" 2>/dev/null || true
  fi

  local real_pkg_config
  real_pkg_config="$(resolve_bwrap_visible_command pkg-config pkgconf || true)"
  if [ -n "${real_pkg_config}" ]; then
    cat > "${tool_bin}/pkgconf" <<EOF
#!${bash_path}
set -euo pipefail
if [ "\${1:-}" = "--variable=schemasdir" ] && [ "\${2:-}" = "gio-2.0" ] && [ -n "\${MAPLE_NIX_GLIB_SCHEMAS:-}" ]; then
  printf '%s\n' "/usr/share/glib-2.0/schemas"
  exit 0
fi
if [ "\${1:-}" = "--variable=exec_prefix" ] && [ "\${2:-}" = "gtk+-3.0" ] && [ -n "\${MAPLE_NIX_GTK_LIB:-}" ]; then
  printf '%s\n' "/usr"
  exit 0
fi
if [ "\${1:-}" = "--variable=libdir" ] && [ "\${2:-}" = "gtk+-3.0" ] && [ -n "\${MAPLE_NIX_GTK_LIB:-}" ]; then
  printf '%s\n' "/usr/lib"
  exit 0
fi
case "\${2:-}" in
  gobject-2.0 | gio-2.0 | librsvg-2.0 | pango | pangocairo | pangoft2)
    if [ "\${1:-}" = "--variable=libdir" ] && [ -n "\${MAPLE_NIX_LINUX_CLOSURE_INFO:-}" ]; then
      printf '%s\n' "/usr/lib"
      exit 0
    fi
    ;;
esac
if [ "\${2:-}" = "gdk-pixbuf-2.0" ] && [ -n "\${MAPLE_NIX_GDK_PIXBUF_BINARYDIR:-}" ]; then
  case "\${1:-}" in
    --variable=libdir)
      printf '%s\n' "/usr/lib"
      exit 0
      ;;
    --variable=gdk_pixbuf_binarydir)
      printf '%s\n' "/usr/lib/gdk-pixbuf-2.0/2.10.0"
      exit 0
      ;;
    --variable=gdk_pixbuf_cache_file)
      printf '%s\n' "/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"
      exit 0
      ;;
    --variable=gdk_pixbuf_moduledir)
      printf '%s\n' "/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders"
      exit 0
      ;;
  esac
fi
exec "${real_pkg_config}" "\$@"
EOF
    chmod +x "${tool_bin}/pkgconf"
    ln -s "${tool_bin}/pkgconf" "${tool_bin}/pkg-config"
  fi

  local real_patchelf
  real_patchelf="$(resolve_bwrap_visible_command patchelf || true)"
  if [ -n "${real_patchelf}" ]; then
    cat > "${tool_bin}/patchelf" <<EOF
#!${bash_path}
for arg in "\$@"; do
  if [ -f "\${arg}" ]; then
    chmod u+w "\${arg}" 2>/dev/null || true
  fi
done
exec "${real_patchelf}" "\$@"
EOF
    chmod +x "${tool_bin}/patchelf"
  fi

  local status
  set +e
  GDK_PIXBUF_MODULEDIR="${GDK_PIXBUF_MODULEDIR:-/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders}" \
    PATH="${tool_bin}:${PATH}" \
    bwrap --dev-bind / / --bind "${bin_dir}" /bin --bind "${usr_root}" /usr -- "$@"
  status=$?
  set -e
  rm -rf "${bin_dir}"
  rm -rf "${tool_bin}"
  rm -rf "${usr_root}"
  return "${status}"
}

build_frontend_dist() {
  disable_bun_env_files
  cd "${FRONTEND_DIR}"
  rm -rf dist
  bun run build
  scrub_host_metadata_files "${FRONTEND_DIR}/dist"
  MAPLE_FRONTEND_DIST_TREE_SHA256="$(tree_hash_digest "${FRONTEND_DIR}/dist")"
  export MAPLE_FRONTEND_DIST_TREE_SHA256
  printf 'sha256-tree  %s  %s\n' "${MAPLE_FRONTEND_DIST_TREE_SHA256}" "apps/maple-research/frontend/dist"
}

verify_frontend_dist_unchanged() {
  local digest

  if [ ! -d "${FRONTEND_DIR}/dist" ]; then
    echo "Missing frontend dist at ${FRONTEND_DIR}/dist" >&2
    return 1
  fi

  digest="$(tree_hash_digest "${FRONTEND_DIR}/dist")"
  if [ -n "${MAPLE_FRONTEND_DIST_TREE_SHA256:-}" ] && [ "${digest}" != "${MAPLE_FRONTEND_DIST_TREE_SHA256}" ]; then
    echo "apps/maple-research/frontend/dist changed during native packaging." >&2
    echo "before=${MAPLE_FRONTEND_DIST_TREE_SHA256}" >&2
    echo "after=${digest}" >&2
    return 1
  fi

  printf 'sha256-tree  %s  %s\n' "${digest}" "apps/maple-research/frontend/dist"
}

ios_onnxruntime_xcframework_dir() {
  printf '%s\n' "${TAURI_DIR}/onnxruntime-ios/onnxruntime.xcframework"
}

ios_onnxruntime_manifest_file() {
  printf '%s\n' "${TAURI_DIR}/onnxruntime-ios/onnxruntime.xcframework.sha256"
}

ios_onnxruntime_version() {
  printf '%s\n' "${IOS_ONNXRUNTIME_VERSION:-1.23.2}"
}

require_ios_onnxruntime_files() {
  local xcframework
  xcframework="$(ios_onnxruntime_xcframework_dir)"

  local -a required_files=(
    "${xcframework}/Info.plist"
    "${xcframework}/Headers/onnxruntime_c_api.h"
    "${xcframework}/ios-arm64/libonnxruntime.a"
    "${xcframework}/ios-arm64-simulator/libonnxruntime.a"
  )

  local file
  for file in "${required_files[@]}"; do
    if [ ! -f "${file}" ]; then
      echo "Missing iOS ONNX Runtime artifact file: ${file}" >&2
      return 1
    fi
  done
}

write_ios_onnxruntime_manifest() {
  require_ios_onnxruntime_files

  local xcframework manifest
  xcframework="$(ios_onnxruntime_xcframework_dir)"
  manifest="$(ios_onnxruntime_manifest_file)"

  mkdir -p "$(dirname "${manifest}")"
  (
    cd "${xcframework}"
    find . -type f -print0 \
      | LC_ALL=C sort -z \
      | while IFS= read -r -d '' file; do
          file="${file#./}"
          printf '%s  %s\n' "$(sha256_file "${file}" | awk '{ print $1 }')" "${file}"
        done
  ) > "${manifest}"

  print_file_hashes "${manifest}"
}

verify_ios_onnxruntime_manifest() {
  require_ios_onnxruntime_files

  local xcframework manifest
  xcframework="$(ios_onnxruntime_xcframework_dir)"
  manifest="$(ios_onnxruntime_manifest_file)"

  if [ ! -f "${manifest}" ]; then
    echo "Missing iOS ONNX Runtime manifest: ${manifest}" >&2
    echo "Run scripts/ci/ios-onnxruntime.sh before the iOS app build." >&2
    return 1
  fi

  (
    cd "${xcframework}"
    if command -v sha256sum >/dev/null 2>&1; then
      sha256sum --check "${manifest}"
    else
      shasum -a 256 --check "${manifest}"
    fi
  )

  verify_ios_onnxruntime_pinned_hashes
}

verify_ios_onnxruntime_pinned_hashes() {
  local version xcode_build xcframework manifest file expected actual status

  version="$(ios_onnxruntime_version)"
  xcode_build="${MAPLE_XCODE_BUILD_VERSION:-17F42}"
  xcframework="$(ios_onnxruntime_xcframework_dir)"
  manifest="$(ios_onnxruntime_manifest_file)"
  status=0

  file="${xcframework}/ios-arm64/libonnxruntime.a"
  expected="$(onnxruntime_ios_device_lib_sha256_for_version "${version}" "${xcode_build}")"
  actual="$(sha256_file "${file}" | awk '{ print $1 }')"
  if [ "${actual}" != "${expected}" ]; then
    echo "iOS ONNX Runtime device library hash mismatch for ${version} with Xcode build ${xcode_build}." >&2
    echo "expected=${expected}" >&2
    echo "actual=${actual}" >&2
    status=1
  fi

  file="${xcframework}/ios-arm64-simulator/libonnxruntime.a"
  expected="$(onnxruntime_ios_simulator_lib_sha256_for_version "${version}" "${xcode_build}")"
  actual="$(sha256_file "${file}" | awk '{ print $1 }')"
  if [ "${actual}" != "${expected}" ]; then
    echo "iOS ONNX Runtime simulator library hash mismatch for ${version} with Xcode build ${xcode_build}." >&2
    echo "expected=${expected}" >&2
    echo "actual=${actual}" >&2
    status=1
  fi

  expected="$(onnxruntime_ios_xcframework_sha256_for_version "${version}" "${xcode_build}")"
  actual="$(sha256_file "${manifest}" | awk '{ print $1 }')"
  if [ "${actual}" != "${expected}" ]; then
    echo "iOS ONNX Runtime manifest hash mismatch for ${version} with Xcode build ${xcode_build}." >&2
    echo "expected=${expected}" >&2
    echo "actual=${actual}" >&2
    status=1
  fi

  actual="$(tree_hash_digest "${xcframework}")"
  if [ "${actual}" != "${expected}" ]; then
    echo "iOS ONNX Runtime xcframework tree hash mismatch for ${version} with Xcode build ${xcode_build}." >&2
    echo "expected=${expected}" >&2
    echo "actual=${actual}" >&2
    status=1
  fi

  return "${status}"
}

print_ios_onnxruntime_hashes() {
  local xcframework
  xcframework="$(ios_onnxruntime_xcframework_dir)"

  print_file_hashes \
    "${xcframework}/ios-arm64/libonnxruntime.a" \
    "${xcframework}/ios-arm64-simulator/libonnxruntime.a" \
    "$(ios_onnxruntime_manifest_file)"
  print_tree_hash "${xcframework}" "apps/maple-research/frontend/src-tauri/onnxruntime-ios/onnxruntime.xcframework"
}

write_ios_onnxruntime_reproducibility_manifest() {
  local out="$1"
  local xcframework
  xcframework="$(ios_onnxruntime_xcframework_dir)"

  write_sha256_manifest \
    "${out}" \
    "${xcframework}/ios-arm64/libonnxruntime.a" \
    "${xcframework}/ios-arm64-simulator/libonnxruntime.a" \
    "$(ios_onnxruntime_manifest_file)"
}

remove_generated_ios_cargo_config() {
  local config_file="${TAURI_DIR}/.cargo/config.toml"

  if [ ! -f "${config_file}" ]; then
    return 0
  fi

  if grep -q "Generated by: scripts/setup-ios-cargo-config.sh" "${config_file}"; then
    rm -f "${config_file}"
    rmdir "${TAURI_DIR}/.cargo" 2>/dev/null || true
  fi
}

archive_tree_as_root_tar_gz() {
  local root="$1"
  local out="$2"
  local tar_cmd="${MAPLE_NIX_GNUTAR:-tar}"
  local gzip_cmd="${MAPLE_NIX_GZIP:-gzip}"

  if [ ! -x "${tar_cmd}" ] && command -v gtar >/dev/null 2>&1; then
    tar_cmd="gtar"
  fi
  if [ ! -x "${tar_cmd}" ]; then
    tar_cmd="tar"
  fi
  if [ ! -x "${gzip_cmd}" ]; then
    gzip_cmd="gzip"
  fi

  (
    cd "${root}"
    find . -mindepth 1 -print0 \
      | LC_ALL=C sort -z \
      | "${tar_cmd}" --null --no-recursion \
          --mtime="@${SOURCE_DATE_EPOCH}" \
          --owner=0 \
          --group=0 \
          --numeric-owner \
          -cf - \
          -T -
  ) | "${gzip_cmd}" -n > "${out}"
}

linux_system_elf_interpreter_for_file() {
  local elf="$1"
  local description

  description="$(LC_ALL=C file "${elf}")"
  case "${description}" in
    *"x86-64"*)
      printf '%s\n' "/lib64/ld-linux-x86-64.so.2"
      ;;
    *"aarch64"*)
      printf '%s\n' "/lib/ld-linux-aarch64.so.1"
      ;;
    *)
      echo "Unsupported Linux ELF architecture for distributable package: ${description}" >&2
      return 1
      ;;
  esac
}

linux_appimage_ignored_needed_library() {
  case "$1" in
    ld-linux-*.so.* | \
      libc.so.* | \
      libdl.so.* | \
      libm.so.* | \
      libmvec.so.* | \
      libnss_*.so.* | \
      libpthread.so.* | \
      libresolv.so.* | \
      librt.so.* | \
      libutil.so.*)
      return 0
      ;;
  esac

  return 1
}

linux_appdir_lib_dir() {
  printf '%s/usr/lib\n' "$1"
}

linux_runtime_closure_store_paths_file() {
  local closure_info="${MAPLE_NIX_LINUX_CLOSURE_INFO:-}"

  if [ -z "${closure_info}" ] || [ ! -f "${closure_info}/store-paths" ]; then
    echo "MAPLE_NIX_LINUX_CLOSURE_INFO/store-paths is required for AppImage runtime closure repair." >&2
    return 1
  fi

  printf '%s/store-paths\n' "${closure_info}"
}

linux_runtime_closure_library_for_soname() {
  local soname="$1"
  local store_paths root candidate lib_root

  store_paths="$(linux_runtime_closure_store_paths_file)"
  while IFS= read -r root; do
    for candidate in "${root}/lib/${soname}" "${root}/lib64/${soname}"; do
      if [ -e "${candidate}" ]; then
        printf '%s\n' "${candidate}"
        return 0
      fi
    done

    for lib_root in "${root}/lib" "${root}/lib64"; do
      [ -d "${lib_root}" ] || continue
      while IFS= read -r -d '' candidate; do
        printf '%s\n' "${candidate}"
        return 0
      done < <(find "${lib_root}" -mindepth 2 -maxdepth 3 \( -type f -o -type l \) -name "${soname}" -print0 | LC_ALL=C sort -z)
    done
  done < "${store_paths}"

  return 1
}

linux_appdir_path_resolves_inside_appdir() {
  local appdir="$1"
  local path="$2"
  local appdir_real path_real

  appdir_real="$(readlink -f "${appdir}")" || return 1
  path_real="$(readlink -f "${path}" 2>/dev/null)" || return 1

  case "${path_real}" in
    "${appdir_real}"/*)
      return 0
      ;;
  esac

  return 1
}

linux_appdir_library_is_self_contained() {
  local appdir="$1"
  local library="$2"

  [ -f "${library}" ] || return 1
  [ ! -L "${library}" ] || linux_appdir_path_resolves_inside_appdir "${appdir}" "${library}"
}

copy_linux_runtime_library_to_appdir() {
  local appdir="$1"
  local source="$2"
  local soname="$3"
  local libdir dest

  libdir="$(linux_appdir_lib_dir "${appdir}")"
  dest="${libdir}/${soname}"
  mkdir -p "${libdir}"

  if [ -L "${dest}" ] && ! linux_appdir_path_resolves_inside_appdir "${appdir}" "${dest}"; then
    rm -f "${dest}"
  fi

  if [ ! -e "${dest}" ]; then
    cp -L --preserve=mode,timestamps "${source}" "${dest}"
    chmod u+w "${dest}" 2>/dev/null || true
    if patchelf --print-needed "${dest}" >/dev/null 2>&1; then
      patchelf --set-rpath '$ORIGIN' "${dest}" 2>/dev/null || true
    fi
    touch -h -d "@${SOURCE_DATE_EPOCH}" "${dest}"
    printf 'staged-linux-appimage-runtime-lib  %s  %s\n' "${soname}" "${source}"
  fi
}

linux_runtime_closure_webkitgtk_store_path() {
  local store_paths root

  store_paths="$(linux_runtime_closure_store_paths_file)"
  while IFS= read -r root; do
    if [ -e "${root}/lib/libwebkit2gtk-4.1.so.0" ] &&
      [ -x "${root}/libexec/webkit2gtk-4.1/WebKitNetworkProcess" ]; then
      printf '%s\n' "${root}"
      return 0
    fi
  done < "${store_paths}"

  return 1
}

linux_runtime_closure_executable_named() {
  local name="$1"
  local store_paths root candidate

  store_paths="$(linux_runtime_closure_store_paths_file)"
  while IFS= read -r root; do
    for candidate in "${root}/bin/${name}" "${root}/libexec/${name}"; do
      if [ -x "${candidate}" ]; then
        printf '%s\n' "${candidate}"
        return 0
      fi
    done
  done < "${store_paths}"

  return 1
}

copy_linux_appdir_tree() {
  local source="$1"
  local dest="$2"

  mkdir -p "${dest}"
  cp -aL "${source}/." "${dest}/"
  chmod -R u+w "${dest}" 2>/dev/null || true
  touch_tree_to_source_date_epoch "${dest}"
}

sanitize_linux_appdir_executables_under() {
  local root="$1"
  local exe

  if [ ! -d "${root}" ]; then
    return 0
  fi

  while IFS= read -r -d '' exe; do
    if patchelf --print-interpreter "${exe}" >/dev/null 2>&1; then
      chmod u+w "${exe}" 2>/dev/null || true
      sanitize_linux_package_executable "${exe}"
      touch -h -d "@${SOURCE_DATE_EPOCH}" "${exe}"
    fi
  done < <(find "${root}" -type f -perm /111 -print0)
}

set_linux_appdir_rpath_under() {
  local root="$1"
  local rpath="$2"
  local elf

  if [ ! -d "${root}" ]; then
    return 0
  fi

  while IFS= read -r -d '' elf; do
    if patchelf --print-needed "${elf}" >/dev/null 2>&1; then
      chmod u+w "${elf}" 2>/dev/null || true
      patchelf --set-rpath "${rpath}" "${elf}" 2>/dev/null || true
      touch -h -d "@${SOURCE_DATE_EPOCH}" "${elf}"
    fi
  done < <(find "${root}" -type f -print0)
}

linux_appdir_portable_library_rpath() {
  printf '%s\n' '$ORIGIN:$ORIGIN/..:$ORIGIN/../..:$ORIGIN/../../..:$ORIGIN/../../../..'
}

sanitize_linux_appdir_library_rpaths() {
  local appdir="$1"
  local rpath elf

  rpath="$(linux_appdir_portable_library_rpath)"
  if [ ! -d "${appdir}/usr/lib" ]; then
    return 0
  fi

  while IFS= read -r -d '' elf; do
    if patchelf --print-rpath "${elf}" >/dev/null 2>&1; then
      chmod u+w "${elf}" 2>/dev/null || true
      patchelf --set-rpath "${rpath}" "${elf}"
      touch -h -d "@${SOURCE_DATE_EPOCH}" "${elf}"
    fi
  done < <(find "${appdir}/usr/lib" -type f -print0)
}

remove_linux_appdir_nix_xdg_wrappers() {
  local appdir="$1"
  local tool path target

  for tool in xdg-open xdg-mime; do
    path="${appdir}/usr/bin/${tool}"
    if [ -L "${path}" ]; then
      target="$(readlink -f "${path}" 2>/dev/null || true)"
      case "${target}" in
        /nix/store/*)
          rm -f "${path}"
          printf 'removed-linux-appimage-nix-xdg-wrapper  %s\n' "${path}"
          continue
          ;;
      esac
    fi

    if [ -f "${path}" ] && grep -a -q '/nix/store' "${path}"; then
      rm -f "${path}"
      printf 'removed-linux-appimage-nix-xdg-wrapper  %s\n' "${path}"
    fi
  done
}

install_linux_appimage_string_patcher() {
  local appdir="$1"
  local runtime_dir source patcher compiler

  runtime_dir="${appdir}/usr/libexec/maple-webkit-runtime"
  patcher="${runtime_dir}/maple-replace-strings"
  source="$(mktemp)"
  compiler="${MAPLE_NIX_CC:-${CC:-cc}}"

  mkdir -p "${runtime_dir}"

  cat > "${source}" <<'EOF'
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

static int replace_all(unsigned char *data, size_t size, const char *needle, const char *replacement) {
  size_t needle_len = strlen(needle);
  size_t replacement_len = strlen(replacement);
  size_t index;
  int replacements = 0;

  if (needle_len == 0) {
    fprintf(stderr, "empty replacement needle\n");
    return 1;
  }

  if (replacement_len > needle_len) {
    fprintf(stderr, "replacement is longer than needle\nneedle=%s\nreplacement=%s\n", needle, replacement);
    return 1;
  }

  for (index = 0; index + needle_len <= size; index++) {
    if (memcmp(data + index, needle, needle_len) == 0) {
      memcpy(data + index, replacement, replacement_len);
      memset(data + index + replacement_len, 0, needle_len - replacement_len);
      replacements++;
      index += needle_len - 1;
    }
  }

  if (replacements == 0) {
    fprintf(stderr, "replacement needle not found: %s\n", needle);
    return 1;
  }

  return 0;
}

int main(int argc, char **argv) {
  int fd;
  struct stat st;
  unsigned char *data;
  int arg;
  int status = 0;

  if (argc < 4 || ((argc - 2) % 2) != 0) {
    fprintf(stderr, "usage: %s FILE NEEDLE REPLACEMENT [NEEDLE REPLACEMENT ...]\n", argv[0]);
    return 2;
  }

  fd = open(argv[1], O_RDWR);
  if (fd < 0) {
    fprintf(stderr, "open %s: %s\n", argv[1], strerror(errno));
    return 1;
  }

  if (fstat(fd, &st) != 0) {
    fprintf(stderr, "stat %s: %s\n", argv[1], strerror(errno));
    close(fd);
    return 1;
  }

  if (st.st_size <= 0) {
    fprintf(stderr, "file is empty: %s\n", argv[1]);
    close(fd);
    return 1;
  }

  data = mmap(NULL, (size_t)st.st_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (data == MAP_FAILED) {
    fprintf(stderr, "mmap %s: %s\n", argv[1], strerror(errno));
    close(fd);
    return 1;
  }

  for (arg = 2; arg < argc; arg += 2) {
    if (replace_all(data, (size_t)st.st_size, argv[arg], argv[arg + 1]) != 0) {
      status = 1;
    }
  }

  if (msync(data, (size_t)st.st_size, MS_SYNC) != 0) {
    fprintf(stderr, "msync %s: %s\n", argv[1], strerror(errno));
    status = 1;
  }

  munmap(data, (size_t)st.st_size);
  close(fd);
  return status;
}
EOF

  "${compiler}" -x c -O2 -Wall -Wextra "${source}" -o "${patcher}"
  rm -f "${source}"
  chmod 0755 "${patcher}"
  sanitize_linux_package_executable "${patcher}"
}

write_linux_appimage_webkit_hook() {
  local appdir="$1"
  local webkit_store="$2"
  local bwrap_source="$3"
  local xdg_dbus_proxy_source="$4"
  local hook libwebkit_hash

  hook="${appdir}/apprun-hooks/maple-webkitgtk.sh"
  libwebkit_hash="$(sha256_file "${appdir}/usr/lib/libwebkit2gtk-4.1.so.0" | awk '{ print $1 }')"
  mkdir -p "$(dirname "${hook}")"

  cat > "${hook}" <<EOF
#! /usr/bin/env bash

maple_webkit_orig_libexec="${webkit_store}/libexec/webkit2gtk-4.1"
maple_webkit_orig_injected_bundle="${webkit_store}/lib/webkit2gtk-4.1/injected-bundle/"
maple_webkit_orig_locale="${webkit_store}/share/locale"
maple_webkit_orig_share="${webkit_store}/share"
maple_webkit_orig_lib="${webkit_store}/lib"
maple_webkit_orig_bwrap="${bwrap_source}"
maple_webkit_orig_xdg_dbus_proxy="${xdg_dbus_proxy_source}"
maple_webkit_runtime_id="${libwebkit_hash}"

maple_webkit_path_fits() {
  local root="\$1"
  local libexec_path="\${root}/libexec/webkit2gtk-4.1"
  local injected_bundle_path="\${root}/lib/webkit2gtk-4.1/injected-bundle/"
  local locale_path="\${root}/share/locale"
  local share_path="\${root}/share"
  local lib_path="\${root}/lib"
  local bwrap_path="\${root}/bin/bwrap"
  local xdg_dbus_proxy_path="\${root}/bin/xdg-dbus-proxy"

  [ \${#libexec_path} -le \${#maple_webkit_orig_libexec} ] &&
    [ \${#injected_bundle_path} -le \${#maple_webkit_orig_injected_bundle} ] &&
    [ \${#locale_path} -le \${#maple_webkit_orig_locale} ] &&
    [ \${#share_path} -le \${#maple_webkit_orig_share} ] &&
    [ \${#lib_path} -le \${#maple_webkit_orig_lib} ] &&
    [ \${#bwrap_path} -le \${#maple_webkit_orig_bwrap} ] &&
    [ \${#xdg_dbus_proxy_path} -le \${#maple_webkit_orig_xdg_dbus_proxy} ]
}

maple_webkit_select_runtime_root() {
  local root

  for root in \\
    "\${XDG_RUNTIME_DIR:+\${XDG_RUNTIME_DIR%/}/maple-webkitgtk-4.1}" \\
    "/tmp/maple-webkitgtk-4.1-\$(id -u)"; do
    [ -n "\${root}" ] || continue
    if maple_webkit_path_fits "\${root}"; then
      printf '%s\n' "\${root}"
      return 0
    fi
  done

  echo "Maple AppImage cannot choose a short enough WebKit runtime path." >&2
  return 1
}

maple_webkit_runtime_dir_is_safe() {
  local root="\$1"
  local owner

  if [ -L "\${root}" ]; then
    echo "Refusing to use symlinked WebKit runtime directory: \${root}" >&2
    return 1
  fi

  if [ -d "\${root}" ]; then
    owner="\$(stat -c '%u' "\${root}" 2>/dev/null || stat -f '%u' "\${root}" 2>/dev/null || printf unknown)"
    if [ "\${owner}" != "\$(id -u)" ]; then
      echo "Refusing to use WebKit runtime directory owned by another user: \${root}" >&2
      return 1
    fi
  fi
}

maple_webkit_prepare_runtime() {
  local appdir root patched_lib marker tmp_lib leaf

  appdir="\${APPDIR:-\${this_dir}}"
  root="\$(maple_webkit_select_runtime_root)"

  if [ -e "\${root}" ] || [ -L "\${root}" ]; then
    maple_webkit_runtime_dir_is_safe "\${root}" || return 1
  else
    mkdir "\${root}" || return 1
  fi
  maple_webkit_runtime_dir_is_safe "\${root}" || return 1
  chmod 700 "\${root}"
  for child in bin lib libexec; do
    if [ -L "\${root}/\${child}" ]; then
      echo "Refusing to use symlinked WebKit runtime subdirectory: \${root}/\${child}" >&2
      return 1
    fi
  done
  mkdir -p "\${root}/bin" "\${root}/lib" "\${root}/libexec"

  for leaf in \\
    "\${root}/libexec/webkit2gtk-4.1" \\
    "\${root}/lib/webkit2gtk-4.1" \\
    "\${root}/share" \\
    "\${root}/bin/bwrap" \\
    "\${root}/bin/xdg-dbus-proxy"; do
    if [ -e "\${leaf}" ] || [ -L "\${leaf}" ]; then
      rm -rf -- "\${leaf}"
    fi
  done

  ln -sfn "\${appdir}/usr/libexec/webkit2gtk-4.1" "\${root}/libexec/webkit2gtk-4.1"
  ln -sfn "\${appdir}/usr/lib/webkit2gtk-4.1" "\${root}/lib/webkit2gtk-4.1"
  ln -sfn "\${appdir}/usr/share" "\${root}/share"
  ln -sfn "\${appdir}/usr/libexec/maple-webkit-runtime/bin/bwrap" "\${root}/bin/bwrap"
  ln -sfn "\${appdir}/usr/libexec/maple-webkit-runtime/bin/xdg-dbus-proxy" "\${root}/bin/xdg-dbus-proxy"

  patched_lib="\${root}/lib/libwebkit2gtk-4.1.so.0"
  marker="\${patched_lib}.maple-id"
  if [ ! -f "\${patched_lib}" ] || [ "\$(cat "\${marker}" 2>/dev/null || true)" != "\${maple_webkit_runtime_id}" ]; then
    tmp_lib="\${patched_lib}.tmp.\$\$"
    cp -f "\${appdir}/usr/lib/libwebkit2gtk-4.1.so.0" "\${tmp_lib}"
    chmod u+w "\${tmp_lib}"
    "\${appdir}/usr/libexec/maple-webkit-runtime/maple-replace-strings" "\${tmp_lib}" \\
      "\${maple_webkit_orig_libexec}" "\${root}/libexec/webkit2gtk-4.1" \\
      "\${maple_webkit_orig_injected_bundle}" "\${root}/lib/webkit2gtk-4.1/injected-bundle/" \\
      "\${maple_webkit_orig_locale}" "\${root}/share/locale" \\
      "\${maple_webkit_orig_share}" "\${root}/share" \\
      "\${maple_webkit_orig_lib}" "\${root}/lib" \\
      "\${maple_webkit_orig_bwrap}" "\${root}/bin/bwrap" \\
      "\${maple_webkit_orig_xdg_dbus_proxy}" "\${root}/bin/xdg-dbus-proxy"
    mv -f "\${tmp_lib}" "\${patched_lib}"
    printf '%s\n' "\${maple_webkit_runtime_id}" > "\${marker}"
  fi

  export MAPLE_WEBKIT_LIBRARY_PATH="\${root}/lib"
}

maple_webkit_prepare_runtime
EOF
  chmod 0755 "${hook}"
  touch -h -d "@${SOURCE_DATE_EPOCH}" "${hook}"
}

write_linux_appimage_maple_apprun() {
  local appdir="$1"
  local app_run="${appdir}/AppRun"

  cat > "${app_run}" <<'EOF'
#! /usr/bin/env bash

set -e

this_dir="$(readlink -f "$(dirname "$0")")"
export APPDIR="${APPDIR:-${this_dir}}"

source "$this_dir"/apprun-hooks/"linuxdeploy-plugin-gtk.sh"
source "$this_dir"/apprun-hooks/"linuxdeploy-plugin-gstreamer.sh"
source "$this_dir"/apprun-hooks/"maple-webkitgtk.sh"

export PATH="$this_dir/usr/bin:$this_dir/usr/sbin:$this_dir/usr/games:$this_dir/bin:$this_dir/sbin:$PATH"
export LD_LIBRARY_PATH="${MAPLE_WEBKIT_LIBRARY_PATH:+${MAPLE_WEBKIT_LIBRARY_PATH}:}$this_dir/usr/lib:$this_dir/usr/lib/i386-linux-gnu:$this_dir/usr/lib/x86_64-linux-gnu:$this_dir/usr/lib32:$this_dir/usr/lib64:$this_dir/lib:$this_dir/lib/i386-linux-gnu:$this_dir/lib/x86_64-linux-gnu:$this_dir/lib32:$this_dir/lib64${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export XDG_DATA_DIRS="$this_dir/usr/share:${XDG_DATA_DIRS:-/usr/local/share/:/usr/share/}"
export GSETTINGS_SCHEMA_DIR="$this_dir/usr/share/glib-2.0/schemas${GSETTINGS_SCHEMA_DIR:+:$GSETTINGS_SCHEMA_DIR}"
export PYTHONPATH="$this_dir/usr/share/pyshared${PYTHONPATH:+:$PYTHONPATH}"
export PERLLIB="$this_dir/usr/share/perl5:$this_dir/usr/lib/perl5${PERLLIB:+:$PERLLIB}"
export QT_PLUGIN_PATH="$this_dir/usr/lib/qt4/plugins:$this_dir/usr/lib/i386-linux-gnu/qt4/plugins:$this_dir/usr/lib/x86_64-linux-gnu/qt4/plugins:$this_dir/usr/lib32/qt4/plugins:$this_dir/usr/lib64/qt4/plugins:$this_dir/usr/lib/qt5/plugins:$this_dir/usr/lib/i386-linux-gnu/qt5/plugins:$this_dir/usr/lib/x86_64-linux-gnu/qt5/plugins:$this_dir/usr/lib32/qt5/plugins:$this_dir/usr/lib64/qt5/plugins${QT_PLUGIN_PATH:+:$QT_PLUGIN_PATH}"
export GST_PLUGIN_SYSTEM_PATH="$this_dir/usr/lib/gstreamer${GST_PLUGIN_SYSTEM_PATH:+:$GST_PLUGIN_SYSTEM_PATH}"
export GST_PLUGIN_SYSTEM_PATH_1_0="$this_dir/usr/lib/gstreamer-1.0${GST_PLUGIN_SYSTEM_PATH_1_0:+:$GST_PLUGIN_SYSTEM_PATH_1_0}"
export PYTHONDONTWRITEBYTECODE=1

exec "$this_dir/usr/bin/maple" "$@"
EOF
  chmod 0755 "${app_run}"
  touch -h -d "@${SOURCE_DATE_EPOCH}" "${app_run}"
}

stage_linux_appdir_webkit_runtime() {
  local appdir="$1"
  local webkit_store bwrap_source xdg_dbus_proxy_source runtime_dir exe

  if [ ! -e "${appdir}/usr/lib/libwebkit2gtk-4.1.so.0" ]; then
    return 0
  fi

  webkit_store="$(linux_runtime_closure_webkitgtk_store_path)"
  bwrap_source="$(linux_runtime_closure_executable_named bwrap)"
  xdg_dbus_proxy_source="$(linux_runtime_closure_executable_named xdg-dbus-proxy)"
  runtime_dir="${appdir}/usr/libexec/maple-webkit-runtime"

  copy_linux_appdir_tree \
    "${webkit_store}/libexec/webkit2gtk-4.1" \
    "${appdir}/usr/libexec/webkit2gtk-4.1"
  copy_linux_appdir_tree \
    "${webkit_store}/lib/webkit2gtk-4.1" \
    "${appdir}/usr/lib/webkit2gtk-4.1"
  copy_linux_appdir_tree \
    "${webkit_store}/share" \
    "${appdir}/usr/share"

  mkdir -p "${runtime_dir}/bin"
  cp -L --preserve=mode,timestamps "${bwrap_source}" "${runtime_dir}/bin/bwrap"
  cp -L --preserve=mode,timestamps "${xdg_dbus_proxy_source}" "${runtime_dir}/bin/xdg-dbus-proxy"

  sanitize_linux_appdir_executables_under "${appdir}/usr/libexec/webkit2gtk-4.1"
  sanitize_linux_appdir_executables_under "${runtime_dir}/bin"
  set_linux_appdir_rpath_under "${appdir}/usr/lib/webkit2gtk-4.1" '$ORIGIN/../..'
  install_linux_appimage_string_patcher "${appdir}"

  for exe in "${runtime_dir}/bin/bwrap" "${runtime_dir}/bin/xdg-dbus-proxy"; do
    touch -h -d "@${SOURCE_DATE_EPOCH}" "${exe}"
  done

  write_linux_appimage_webkit_hook \
    "${appdir}" \
    "${webkit_store}" \
    "${bwrap_source}" \
    "${xdg_dbus_proxy_source}"
  write_linux_appimage_maple_apprun "${appdir}"

  printf 'staged-linux-appimage-webkit-runtime  %s\n' "${webkit_store}"
}

repair_linux_appdir_runtime_closure() {
  local appdir="$1"
  local libdir pass changed elf needed soname library_source

  libdir="$(linux_appdir_lib_dir "${appdir}")"
  mkdir -p "${libdir}"

  for pass in $(seq 1 20); do
    changed=0

    while IFS= read -r -d '' elf; do
      while IFS= read -r needed; do
        [ -n "${needed}" ] || continue

        case "${needed}" in
          */*)
            soname="$(basename "${needed}")"
            if ! linux_appimage_ignored_needed_library "${soname}"; then
              if library_source="$(linux_runtime_closure_library_for_soname "${soname}")"; then
                copy_linux_runtime_library_to_appdir "${appdir}" "${library_source}" "${soname}"
              else
                library_source=""
              fi
            fi

            if [ "${needed}" != "${soname}" ]; then
              patchelf --replace-needed "${needed}" "${soname}" "${elf}"
              touch -h -d "@${SOURCE_DATE_EPOCH}" "${elf}"
              printf 'rewrote-linux-appimage-needed  %s  %s  %s\n' "${needed}" "${soname}" "${elf}"
              changed=1
            fi
            continue
            ;;
          *)
            soname="${needed}"
            ;;
        esac

        if linux_appimage_ignored_needed_library "${soname}"; then
          continue
        fi

        if linux_appdir_library_is_self_contained "${appdir}" "${libdir}/${soname}"; then
          continue
        fi

        if library_source="$(linux_runtime_closure_library_for_soname "${soname}")"; then
          copy_linux_runtime_library_to_appdir "${appdir}" "${library_source}" "${soname}"
          changed=1
        fi
      done < <(patchelf --print-needed "${elf}" 2>/dev/null || true)
    done < <(find "${appdir}" -type f -print0)

    if [ "${changed}" -eq 0 ]; then
      return 0
    fi
  done

  echo "AppImage runtime closure repair did not converge." >&2
  return 1
}

verify_linux_appdir_runtime_closure() {
  local appdir="$1"
  local libdir status elf needed soname library

  libdir="$(linux_appdir_lib_dir "${appdir}")"
  status=0

  while IFS= read -r -d '' elf; do
    while IFS= read -r needed; do
      [ -n "${needed}" ] || continue

      case "${needed}" in
        */*)
          echo "AppImage ELF contains an absolute DT_NEEDED path." >&2
          echo "needed=${needed}" >&2
          echo "file=${elf}" >&2
          status=1
          soname="$(basename "${needed}")"
          ;;
        *)
          soname="${needed}"
          ;;
      esac

      if linux_appimage_ignored_needed_library "${soname}"; then
        continue
      fi

      library="${libdir}/${soname}"
      if [ ! -f "${library}" ]; then
        echo "AppImage is missing bundled runtime library ${soname} required by ${elf}." >&2
        status=1
      elif [ -L "${library}" ] && ! linux_appdir_path_resolves_inside_appdir "${appdir}" "${library}"; then
        echo "AppImage runtime library ${soname} resolves outside the AppDir." >&2
        echo "file=${library}" >&2
        echo "target=$(readlink -f "${library}" 2>/dev/null || printf '<unresolved>')" >&2
        status=1
      fi
    done < <(patchelf --print-needed "${elf}" 2>/dev/null || true)
  done < <(find "${appdir}" -type f -print0)

  if [ "${status}" -eq 0 ]; then
    printf 'verified-linux-appimage-runtime-closure  %s\n' "${appdir}"
  fi

  return "${status}"
}

verify_linux_appdir_portable_runtime_paths() {
  local appdir="$1"
  local status=0
  local path rpath target

  while IFS= read -r -d '' path; do
    rpath="$(patchelf --print-rpath "${path}" 2>/dev/null || true)"
    case "${rpath}" in
      *"/nix/store/"* | *"/home/runner/work/"* | *"/Users/runner/work/"* | *"/Users/tony/"*)
        echo "AppImage ELF contains build-host paths in RPATH/RUNPATH." >&2
        echo "rpath=${rpath}" >&2
        echo "file=${path}" >&2
        status=1
        ;;
    esac
  done < <(find "${appdir}" -type f -print0)

  for path in "${appdir}/usr/bin/xdg-open" "${appdir}/usr/bin/xdg-mime"; do
    if [ -L "${path}" ]; then
      target="$(readlink -f "${path}" 2>/dev/null || true)"
      case "${target}" in
        /nix/store/*)
          echo "AppImage bundles a Nix-resolved xdg-utils symlink." >&2
          echo "file=${path}" >&2
          echo "target=${target}" >&2
          status=1
          continue
          ;;
      esac
    fi

    if [ -f "${path}" ] && grep -a -q '/nix/store' "${path}"; then
      echo "AppImage bundles a Nix-resolved xdg-utils wrapper." >&2
      echo "file=${path}" >&2
      status=1
    fi
  done

  if [ "${status}" -eq 0 ]; then
    printf 'verified-linux-appimage-portable-runtime-paths  %s\n' "${appdir}"
  fi

  return "${status}"
}

verify_linux_appdir_webkit_runtime() {
  local appdir="$1"
  local status=0
  local path
  local required_paths=(
    "apprun-hooks/maple-webkitgtk.sh"
    "usr/lib/libwebkit2gtk-4.1.so.0"
    "usr/libexec/webkit2gtk-4.1/WebKitNetworkProcess"
    "usr/libexec/webkit2gtk-4.1/WebKitWebProcess"
    "usr/libexec/webkit2gtk-4.1/WebKitGPUProcess"
    "usr/lib/webkit2gtk-4.1/injected-bundle/libwebkit2gtkinjectedbundle.so"
    "usr/libexec/maple-webkit-runtime/bin/bwrap"
    "usr/libexec/maple-webkit-runtime/bin/xdg-dbus-proxy"
    "usr/libexec/maple-webkit-runtime/maple-replace-strings"
  )

  if [ ! -e "${appdir}/usr/lib/libwebkit2gtk-4.1.so.0" ]; then
    return 0
  fi

  for path in "${required_paths[@]}"; do
    if [ ! -e "${appdir}/${path}" ]; then
      echo "AppImage is missing WebKit runtime payload: ${path}" >&2
      status=1
    fi
  done

  if [ -f "${appdir}/AppRun" ]; then
    if ! grep -Fq 'apprun-hooks/"maple-webkitgtk.sh"' "${appdir}/AppRun"; then
      echo "AppImage AppRun does not source the Maple WebKit runtime hook." >&2
      status=1
    fi
    if ! grep -Fq 'exec "$this_dir/usr/bin/maple" "$@"' "${appdir}/AppRun"; then
      echo "AppImage AppRun does not launch Maple with the verified runtime environment." >&2
      status=1
    fi
  else
    echo "AppImage is missing AppRun." >&2
    status=1
  fi

  for path in \
    "${appdir}/usr/libexec/webkit2gtk-4.1/WebKitNetworkProcess" \
    "${appdir}/usr/libexec/webkit2gtk-4.1/WebKitWebProcess" \
    "${appdir}/usr/libexec/webkit2gtk-4.1/WebKitGPUProcess" \
    "${appdir}/usr/libexec/maple-webkit-runtime/bin/bwrap" \
    "${appdir}/usr/libexec/maple-webkit-runtime/bin/xdg-dbus-proxy" \
    "${appdir}/usr/libexec/maple-webkit-runtime/maple-replace-strings"; do
    if [ -f "${path}" ]; then
      verify_linux_package_executable_metadata "${path}" || status=1
    fi
  done

  if [ "${status}" -eq 0 ]; then
    printf 'verified-linux-appimage-webkit-runtime  %s\n' "${appdir}"
  fi

  return "${status}"
}

prepare_linux_appdir_for_appimage() {
  local appdir="$1"
  local exe="${appdir}/usr/bin/maple"

  rm -f "${appdir}/.DirIcon"
  if [ -f "${exe}" ]; then
    sanitize_linux_package_executable "${exe}"
  fi

  stage_linux_appdir_webkit_runtime "${appdir}"
  repair_linux_appdir_runtime_closure "${appdir}"
  sanitize_linux_appdir_library_rpaths "${appdir}"
  remove_linux_appdir_nix_xdg_wrappers "${appdir}"
  verify_linux_appdir_runtime_closure "${appdir}"
  verify_linux_appdir_portable_runtime_paths "${appdir}"
  verify_linux_appdir_webkit_runtime "${appdir}"
  touch_tree_to_source_date_epoch "${appdir}"
}

verify_linux_package_executable_metadata() {
  local exe="$1"
  local expected_interpreter actual_interpreter rpath

  if [ ! -f "${exe}" ]; then
    echo "Missing Linux package executable: ${exe}" >&2
    return 1
  fi

  expected_interpreter="$(linux_system_elf_interpreter_for_file "${exe}")"
  actual_interpreter="$(patchelf --print-interpreter "${exe}")"
  if [ "${actual_interpreter}" != "${expected_interpreter}" ]; then
    echo "Linux package executable uses a non-distributable ELF interpreter." >&2
    echo "expected=${expected_interpreter}" >&2
    echo "actual=${actual_interpreter}" >&2
    echo "file=${exe}" >&2
    return 1
  fi

  rpath="$(patchelf --print-rpath "${exe}" 2>/dev/null || true)"
  case "${rpath}" in
    *"/nix/store/"* | *"/home/runner/work/"* | *"/Users/runner/work/"*)
      echo "Linux package executable contains build-host paths in RPATH/RUNPATH." >&2
      echo "rpath=${rpath}" >&2
      echo "file=${exe}" >&2
      return 1
      ;;
  esac

  printf 'verified-linux-package-elf  interpreter=%s  rpath=%s  %s\n' \
    "${actual_interpreter}" \
    "${rpath:-<empty>}" \
    "${exe}"
}

sanitize_linux_package_executable() {
  local exe="$1"
  local interpreter

  interpreter="$(linux_system_elf_interpreter_for_file "${exe}")"
  chmod u+w "${exe}" 2>/dev/null || true
  patchelf --set-interpreter "${interpreter}" --remove-rpath "${exe}"
  verify_linux_package_executable_metadata "${exe}"
}

sanitize_linux_deb_payload() {
  local payload="$1"

  sanitize_linux_package_executable "${payload}/usr/bin/maple"
}

sanitize_linux_target_release_executable() {
  sanitize_linux_package_executable "${TAURI_DIR}/target/release/maple"
}

verify_linux_deb_package_executable_metadata() {
  local deb="$1"
  local tmp status

  deb="$(cd "$(dirname "${deb}")" && pwd -P)/$(basename "${deb}")"
  tmp="$(mktemp -d)"
  status=0
  (
    cd "${tmp}"
    ar x "${deb}"
    mkdir data
    tar -xzf data.tar.gz -C data
    verify_linux_package_executable_metadata "${tmp}/data/usr/bin/maple"
  ) || status=$?
  rm -rf "${tmp}"
  return "${status}"
}

verify_linux_appimage_executable_metadata() {
  local appimage="$1"
  local tmp status

  tmp="$(mktemp -d)"
  status=0
  (
    extract_appimage_tool "${appimage}" "${tmp}/AppDir"
    verify_linux_package_executable_metadata "${tmp}/AppDir/usr/bin/maple"
    verify_linux_appdir_runtime_closure "${tmp}/AppDir"
    verify_linux_appdir_portable_runtime_paths "${tmp}/AppDir"
    verify_linux_appdir_webkit_runtime "${tmp}/AppDir"
  ) || status=$?
  rm -rf "${tmp}"
  return "${status}"
}

verify_linux_desktop_package_metadata() {
  local artifact

  for artifact in "$@"; do
    case "${artifact}" in
      *Maple_*.AppImage)
        verify_linux_appimage_executable_metadata "${artifact}"
        ;;
      *.deb)
        verify_linux_deb_package_executable_metadata "${artifact}"
        ;;
    esac
  done
}

normalize_deb_package() {
  local deb="$1"
  local tmp
  tmp="$(mktemp -d)"

  (
    cd "${tmp}"
    ar x "${deb}"
    mkdir control data
    tar -xzf control.tar.gz -C control
    tar -xzf data.tar.gz -C data
    sanitize_linux_deb_payload "${tmp}/data"
    archive_tree_as_root_tar_gz "${tmp}/control" "${tmp}/control.tar.gz"
    archive_tree_as_root_tar_gz "${tmp}/data" "${tmp}/data.tar.gz"
    printf '2.0\n' > debian-binary
    ar crD normalized.deb debian-binary control.tar.gz data.tar.gz
  )

  mv "${tmp}/normalized.deb" "${deb}"
  rm -rf "${tmp}"
}

extract_deb_payload() {
  local deb="$1"
  local payload="$2"
  local tmp
  tmp="$(mktemp -d)"

  (
    cd "${tmp}"
    ar x "${deb}"
    mkdir -p "${payload}"
    tar -xzf data.tar.gz -C "${payload}"
  )

  rm -rf "${tmp}"
}

touch_tree_to_source_date_epoch() {
  local root="$1"
  find "${root}" -exec touch -h -d "@${SOURCE_DATE_EPOCH}" {} +
}

rpm_arch() {
  case "$(uname -m)" in
    x86_64 | amd64)
      printf '%s\n' "x86_64"
      ;;
    aarch64 | arm64)
      printf '%s\n' "aarch64"
      ;;
    *)
      echo "Unsupported Linux RPM architecture: $(uname -m)" >&2
      return 1
      ;;
  esac
}

rebuild_rpm_package_from_payload() {
  local rpm="$1"
  local payload="$2"
  local topdir spec built_rpm
  local version release arch rpm_build_shell

  command -v rpmbuild >/dev/null 2>&1 || {
    echo "rpmbuild is required to normalize RPM packages. Run through the flake CI shell." >&2
    return 1
  }
  rpm_build_shell="$(command -v bash)"

  version="$(jq -r '.version' "${TAURI_DIR}/tauri.conf.json")"
  release="$(jq -r '.bundle.linux.rpm.release // "1"' "${TAURI_DIR}/tauri.conf.json")"
  arch="$(rpm_arch)"
  topdir="$(mktemp -d)"
  spec="${topdir}/SPECS/maple.spec"

  touch_tree_to_source_date_epoch "${payload}"
  mkdir -p "${topdir}/BUILD" "${topdir}/BUILDROOT" "${topdir}/RPMS" "${topdir}/SOURCES" "${topdir}/SPECS" "${topdir}/SRPMS" "${topdir}/tmp"

  cat > "${spec}" <<EOF
Name: maple
Epoch: 0
Version: ${version}
Release: ${release}
Summary: Maple AI
License: MIT
BuildArch: ${arch}
AutoReqProv: no

%description
Maple AI

%prep

%build

%install
rm -rf %{buildroot}
mkdir -p %{buildroot}
cp -a ${payload}/. %{buildroot}/

%files
%defattr(-,root,root,-)
EOF

  (
    cd "${payload}"
    find . \( -type f -o -type l \) -print \
      | LC_ALL=C sort \
      | sed 's#^\./#/#'
  ) >> "${spec}"

  env -u LD_LIBRARY_PATH SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH}" rpmbuild -bb "${spec}" \
    --define "_topdir ${topdir}" \
    --define "_tmppath ${topdir}/tmp" \
    --define "_buildshell ${rpm_build_shell}" \
    --define "_dbpath ${topdir}/rpmdb" \
    --define "_buildhost (none)" \
    --define "use_source_date_epoch_as_buildtime 1" \
    --define "_build_name_fmt %{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}.rpm" \
    --define "_binary_payload w9.gzdio"

  built_rpm="$(find "${topdir}/RPMS" -type f -name '*.rpm' | LC_ALL=C sort | head -n 1)"
  if [ -z "${built_rpm}" ]; then
    echo "rpmbuild did not produce an RPM." >&2
    rm -rf "${topdir}"
    return 1
  fi

  mv "${built_rpm}" "${rpm}"
  rm -rf "${topdir}"
}

normalize_linux_desktop_packages() {
  if [ "$(host_os)" != "linux" ]; then
    return 0
  fi

  local deb rpm payload
  local -a debs=()
  local -a rpms=()

  while IFS= read -r -d '' deb; do
    debs+=("${deb}")
  done < <(find "${TAURI_DIR}/target/release/bundle/deb" -type f -name '*.deb' -print0 2>/dev/null | LC_ALL=C sort -z)

  while IFS= read -r -d '' rpm; do
    rpms+=("${rpm}")
  done < <(find "${TAURI_DIR}/target/release/bundle/rpm" -type f -name '*.rpm' -print0 2>/dev/null | LC_ALL=C sort -z)

  for deb in "${debs[@]}"; do
    normalize_deb_package "${deb}"
  done

  if [ "${#rpms[@]}" -gt 0 ]; then
    if [ "${#debs[@]}" -eq 0 ]; then
      echo "Cannot normalize RPM packages without a Debian payload source." >&2
      return 1
    fi

    payload="$(mktemp -d)"
    extract_deb_payload "${debs[0]}" "${payload}"
    for rpm in "${rpms[@]}"; do
      rebuild_rpm_package_from_payload "${rpm}" "${payload}"
    done
    rm -rf "${payload}"
  fi
}

linux_tauri_pr_config() {
  local ort_version="${ORT_VERSION:-1.23.2}"
  local ort_arch
  ort_arch="$(linux_ort_arch)"
  local ort_rel="./onnxruntime-linux/onnxruntime-linux-${ort_arch}-${ort_version}/lib/libonnxruntime.so.${ort_version}"

  jq -cn --arg ort "${ort_rel}" '{
    build: {
      beforeBuildCommand: null
    },
    bundle: {
      createUpdaterArtifacts: false,
      useLocalToolsDir: true,
      targets: ["appimage", "deb", "rpm"],
      linux: {
        appimage: {
          bundleMediaFramework: true,
          files: {
            "/usr/lib/maple/libonnxruntime.so": $ort
          }
        },
        deb: {
          files: {
            "/usr/lib/maple/libonnxruntime.so": $ort
          }
        },
        rpm: {
          files: {
            "/usr/lib/maple/libonnxruntime.so": $ort
          }
        }
      }
    }
  }'
}

linux_tauri_release_config() {
  local ort_version="${ORT_VERSION:-1.23.2}"
  local ort_arch updater_pubkey
  ort_arch="$(linux_ort_arch)"
  local ort_rel="./onnxruntime-linux/onnxruntime-linux-${ort_arch}-${ort_version}/lib/libonnxruntime.so.${ort_version}"
  updater_pubkey="$(tauri_updater_public_key_config_value)"

	  jq -cn --arg ort "${ort_rel}" --arg updaterPubkey "${updater_pubkey}" '{
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
	      useLocalToolsDir: true,
	      targets: ["appimage", "deb", "rpm"],
	      linux: {
	        appimage: {
          bundleMediaFramework: true,
          files: {
            "/usr/lib/maple/libonnxruntime.so": $ort
          }
        },
        deb: {
          files: {
            "/usr/lib/maple/libonnxruntime.so": $ort
          }
        },
        rpm: {
          files: {
            "/usr/lib/maple/libonnxruntime.so": $ort
          }
        }
      }
    }
  }'
}

windows_tauri_release_build_config() {
  jq -cn '{
    build: {
      beforeBuildCommand: null
    },
    bundle: {
      createUpdaterArtifacts: false,
      targets: ["nsis"]
    }
  }'
}

windows_tauri_release_bundle_config() {
  local sign_script

  sign_script="$(to_windows_path "${REPO_ROOT}/scripts/ci/windows-artifact-sign.ps1")"

  jq -cn --arg signScript "${sign_script}" '{
    build: {
      beforeBuildCommand: null
    },
    bundle: {
      createUpdaterArtifacts: false,
      targets: ["nsis"],
      windows: {
        signCommand: {
          cmd: "pwsh",
          args: [
            "-NoLogo",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            $signScript,
            "%1"
          ]
        }
      }
    }
  }'
}

windows_release_app_exe() {
  printf '%s\n' "${TAURI_DIR}/target/release/maple.exe"
}

windows_release_product_name() {
  jq -er '.productName' "${TAURI_DIR}/tauri.conf.json"
}

windows_release_version() {
  jq -er '.version' "${TAURI_DIR}/tauri.conf.json"
}

windows_release_setup_exe_basename_for_version() {
  local version="$1"
  printf '%s_%s_x64-setup.exe\n' "$(windows_release_product_name)" "${version}"
}

windows_release_setup_exe_basename() {
  windows_release_setup_exe_basename_for_version "$(windows_release_version)"
}

windows_release_setup_exe_path() {
  printf '%s\n' "${TAURI_DIR}/target/release/bundle/nsis/$(windows_release_setup_exe_basename)"
}

windows_release_setup_sig_path() {
  printf '%s.sig\n' "$(windows_release_setup_exe_path)"
}

windows_release_unsigned_setup_exe_path() {
  printf '%s\n' "${TAURI_DIR}/target/reproducibility/windows-unsigned/$(windows_release_setup_exe_basename)"
}

windows_release_setup_exes_found() {
  local nsis_dir="${TAURI_DIR}/target/release/bundle/nsis"
  if [ ! -d "${nsis_dir}" ]; then
    return 0
  fi
  find "${nsis_dir}" -maxdepth 1 -type f -name '*_x64-setup.exe' -print | LC_ALL=C sort
}

windows_release_setup_exe_required() {
  local setup
  setup="$(windows_release_setup_exe_path)"
  if [ ! -f "${setup}" ]; then
    echo "Expected Windows NSIS setup executable was not found: ${setup}" >&2
    echo "Found Windows NSIS setup candidates:" >&2
    windows_release_setup_exes_found | sed 's/^/  /' >&2
    return 1
  fi
  printf '%s\n' "${setup}"
}

verify_windows_authenticode_signatures() {
  require_windows_host "verify_windows_authenticode_signatures"

  if [ "$#" -eq 0 ]; then
    echo "verify_windows_authenticode_signatures requires at least one file." >&2
    return 1
  fi

  local files_manifest
  local file
  local expected_cn
  expected_cn="${MAPLE_WINDOWS_AUTHENTICODE_EXPECTED_CN:-Mutiny Wallet Inc. dba OpenSecret}"
  files_manifest="$(mktemp)"
  for file in "$@"; do
    if [ ! -f "${file}" ]; then
      echo "Missing Windows signed artifact: ${file}" >&2
      rm -f "${files_manifest}"
      return 1
    fi
    printf '%s\n' "$(to_windows_path "${file}")" >> "${files_manifest}"
  done

  # Keep the expected identity field and file list in the environment. Passing
  # them as positional args through Git Bash into pwsh can split values that
  # contain spaces. Get-AuthenticodeSignature validates the certificate chain;
  # this check only asserts that the trusted signer identity is ours.
  # shellcheck disable=SC2016
  if ! MAPLE_WINDOWS_AUTHENTICODE_FILES="$(to_windows_path "${files_manifest}")" \
    MAPLE_WINDOWS_AUTHENTICODE_EXPECTED_CN="${expected_cn}" \
    pwsh -NoLogo -NoProfile -ExecutionPolicy Bypass -Command '
    $ErrorActionPreference = "Stop"
    $expectedCn = $env:MAPLE_WINDOWS_AUTHENTICODE_EXPECTED_CN
    if ([string]::IsNullOrWhiteSpace($expectedCn)) {
      throw "MAPLE_WINDOWS_AUTHENTICODE_EXPECTED_CN is required to verify the Windows signer identity."
    }
    $filesManifest = $env:MAPLE_WINDOWS_AUTHENTICODE_FILES
    if ([string]::IsNullOrWhiteSpace($filesManifest)) {
      throw "MAPLE_WINDOWS_AUTHENTICODE_FILES is required to verify Windows signed artifacts."
    }
    $files = @(Get-Content -LiteralPath $filesManifest | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    if ($files.Count -eq 0) {
      throw "No Windows signed artifacts were provided to Authenticode verification."
    }
    foreach ($file in $files) {
      $signature = Get-AuthenticodeSignature -LiteralPath $file
      if ($signature.Status -ne "Valid") {
        throw "Authenticode signature is not valid for $file. Status=$($signature.Status) Message=$($signature.StatusMessage)"
      }
      if ($null -eq $signature.SignerCertificate) {
        throw "Authenticode signature is valid but no signer certificate was returned for $file."
      }
      $subject = $signature.SignerCertificate.Subject
      $issuer = $signature.SignerCertificate.Issuer
      $expectedCnPattern = "(^|,\s*)CN=$([regex]::Escape($expectedCn))(\s*,|$)"
      if (-not [regex]::IsMatch($subject, $expectedCnPattern)) {
        throw "Authenticode signer subject CN mismatch for $file. Actual=$subject Issuer=$issuer"
      }
      Write-Output ("verified-windows-authenticode  {0}  subject={1}  issuer={2}  thumbprint={3}" -f $file, $subject, $issuer, $signature.SignerCertificate.Thumbprint)
    }
  '; then
    rm -f "${files_manifest}"
    return 1
  fi

  rm -f "${files_manifest}"
}

import_apple_developer_certificate() {
  if [ "$(host_os)" != "darwin" ] || [ -z "${APPLE_CERTIFICATE:-}" ]; then
    return 0
  fi

  if [ -z "${APPLE_CERTIFICATE_PASSWORD:-}" ]; then
    echo "APPLE_CERTIFICATE_PASSWORD is required when APPLE_CERTIFICATE is set." >&2
    return 1
  fi

  local temp_dir cert_file keychain keychain_password cert_info cert_id
  temp_dir="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
  cert_file="${temp_dir%/}/maple-apple-certificate.p12"
  keychain="${temp_dir%/}/maple-build.keychain-db"
  keychain_password="${KEYCHAIN_PASSWORD:-$(openssl rand -hex 24)}"

  decode_base64_string_to_file "${APPLE_CERTIFICATE}" "${cert_file}"

  security create-keychain -p "${keychain_password}" "${keychain}"
  security default-keychain -s "${keychain}"
  security unlock-keychain -p "${keychain_password}" "${keychain}"
  security set-keychain-settings -t 3600 -u "${keychain}"
  security import "${cert_file}" -k "${keychain}" -P "${APPLE_CERTIFICATE_PASSWORD}" -T /usr/bin/codesign -T /usr/bin/productbuild
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "${keychain_password}" "${keychain}"

  cert_info="$(security find-identity -v -p codesigning "${keychain}" | grep "Developer ID Application" | head -n 1 || true)"
  if [ -z "${cert_info}" ]; then
    echo "No Developer ID Application identity found in imported Apple certificate." >&2
    security find-identity -v -p codesigning "${keychain}" >&2 || true
    return 1
  fi

  cert_id="$(printf '%s\n' "${cert_info}" | awk -F'"' '{ print $2 }')"
  export APPLE_SIGNING_IDENTITY="${APPLE_SIGNING_IDENTITY:-${cert_id}}"
  echo "Imported Apple Developer ID certificate."
}
