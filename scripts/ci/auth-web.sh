#!/usr/bin/env bash
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"

case "${MAPLE_AUTH_ENVIRONMENT:-pr}" in
  pr) use_pr_environment ;;
  release)
    # Reject the staged local SDK link before any installation or build work.
    python3 -I "${SCRIPT_DIR}/pages_auth_build.py" sdk-pin --frontend "${FRONTEND_DIR}"
    use_release_environment
    ;;
  *) printf 'Unsupported auth build profile; expected pr or release.\n' >&2; exit 1 ;;
esac

# Build/run subprocesses ignore dotenv files, and auth Vite uses envDir:false.
# Bun 1.3.5 install ignores --no-env-file and may still read local dotenv files;
# install scripts are disabled and its environment cannot change this parent
# shell's fixed build profile. Never move managed dotenv files to work around it.
real_bun="$(command -v bun)"
wrapper_dir="$(mktemp -d)"
trap 'rm -rf -- "$wrapper_dir"' EXIT
printf '#!/usr/bin/env bash\nexec %q --no-env-file "$@"\n' "$real_bun" >"$wrapper_dir/bun"
chmod +x "$wrapper_dir/bun"
export PATH="$wrapper_dir:$PATH"
export MAPLE_BUN_NO_ENV_FILE=1 MAPLE_IGNORE_VITE_ENV_FILES=1
unset NODE_OPTIONS BUN_OPTIONS BUN_PRELOAD

print_source_provenance
install_frontend_deps
if [ "${MAPLE_AUTH_ENVIRONMENT:-pr}" = release ]; then
  python3 -I "${SCRIPT_DIR}/pages_auth_build.py" sdk-pin --frontend "${FRONTEND_DIR}" --installed
fi
configure_reproducible_build_metadata
cd "${FRONTEND_DIR}"
bun --no-env-file run build:auth
test -f dist-auth/index.html
scrub_host_metadata_files "${FRONTEND_DIR}/dist-auth"

repro_dir="${TAURI_DIR}/target/reproducibility"
mkdir -p "$repro_dir"
auth_archive="$repro_dir/maple-auth-dist.tar.gz"
archive_tree_as_root_tar_gz "${FRONTEND_DIR}/dist-auth" "$auth_archive"
python3 -I "${SCRIPT_DIR}/pages_auth_build.py" artifact --archive "$auth_archive"
write_sha256_manifest "$repro_dir/auth-final.sha256" "$auth_archive"
print_file_hashes "$auth_archive"
