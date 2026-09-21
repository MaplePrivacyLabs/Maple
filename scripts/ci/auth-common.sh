#!/usr/bin/env bash
# Standalone auth tooling. This must not source Research/Tauri build helpers.
set -euo pipefail

AUTH_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AUTH_REPO_ROOT="$(cd "${AUTH_SCRIPT_DIR}/../.." && pwd)"
AUTH_APP_DIR="${AUTH_REPO_ROOT}/apps/maple-auth"

use_auth_environment() {
  local profile="$1" name
  case "$profile" in
    pr | release) ;;
    *) printf 'Unsupported auth build profile; expected pr or release.\n' >&2; return 1 ;;
  esac
  while IFS='=' read -r name _; do
    case "$name" in VITE_*) unset "$name" ;; esac
  done < <(env)
  export VITE_CLIENT_ID="ba5a14b5-d915-47b1-b7b1-afda52bc5fc6"
  if [ "$profile" = release ]; then
    export VITE_OPEN_SECRET_API_URL="https://enclave.trymaple.ai"
    export VITE_OPEN_SECRET_PCR_ENVIRONMENT="production"
  else
    export VITE_OPEN_SECRET_API_URL="https://enclave.secretgpt.ai"
    export VITE_OPEN_SECRET_PCR_ENVIRONMENT="development"
  fi
}

prepare_auth_tooling() {
  # Build/run subprocesses ignore dotenv files, and auth Vite uses envDir:false.
  # Bun 1.3.5 install can still read dotenv despite --no-env-file; scripts are
  # disabled and that child cannot alter this shell's fixed profile. Never move
  # ignored or externally managed dotenv files to work around that Bun behavior.
  local real_bun
  real_bun="$(command -v bun)"
  AUTH_WRAPPER_DIR="$(mktemp -d)"
  trap 'rm -rf -- "$AUTH_WRAPPER_DIR"' EXIT
  printf '#!/usr/bin/env bash\nexec %q --no-env-file "$@"\n' "$real_bun" >"$AUTH_WRAPPER_DIR/bun"
  chmod +x "$AUTH_WRAPPER_DIR/bun"
  export PATH="$AUTH_WRAPPER_DIR:$PATH"
  export MAPLE_IGNORE_VITE_ENV_FILES=1
  unset NODE_OPTIONS BUN_OPTIONS BUN_PRELOAD
}

install_auth_deps() {
  # Both profiles use the independent published dependency graph. No source SDK
  # preparation, Research node_modules, native toolchain or app dotenv is read.
  python3 -I "${AUTH_SCRIPT_DIR}/pages_auth_build.py" sdk-pin --frontend "$AUTH_APP_DIR"
  (cd "$AUTH_APP_DIR" && bun --no-env-file install --frozen-lockfile --ignore-scripts)
  python3 -I "${AUTH_SCRIPT_DIR}/pages_auth_build.py" sdk-pin --frontend "$AUTH_APP_DIR" --installed
}

print_auth_source_provenance() {
  if ! command -v git >/dev/null 2>&1 ||
    ! git -C "$AUTH_REPO_ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    return 0
  fi
  printf 'git-commit  %s\n' "$(git -C "$AUTH_REPO_ROOT" rev-parse HEAD)"
  printf 'git-tree  %s\n' "$(git -C "$AUTH_REPO_ROOT" rev-parse 'HEAD^{tree}')"
  if ! git -C "$AUTH_REPO_ROOT" diff --quiet --ignore-submodules --; then
    echo 'git-worktree-dirty  unstaged'
  fi
  if ! git -C "$AUTH_REPO_ROOT" diff --cached --quiet --ignore-submodules --; then
    echo 'git-worktree-dirty  staged'
  fi
}
