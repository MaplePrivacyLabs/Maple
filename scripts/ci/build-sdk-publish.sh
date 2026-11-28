#!/usr/bin/env bash
# Build and test one SDK or proxy crate artifact. This script never authenticates
# or publishes.
set -euo pipefail

if [[ $# != 2 || ( "$1" != npm && "$1" != rust && "$1" != proxy ) ]]; then
  echo "Usage: $0 npm|rust|proxy OUTPUT_DIR" >&2
  exit 2
fi

sdk_kind="$1"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
mkdir -p "$2"
output_dir="$(cd "$2" && pwd -P)"
if [[ -n "$(ls -A "${output_dir}")" ]]; then
  echo "Output directory must be empty." >&2
  exit 2
fi

build_dir="$(mktemp -d "${TMPDIR:-/tmp}/maple-sdk-package.XXXXXXXX")"
trap 'rm -rf "${build_dir}"' EXIT

# Stage the npm build from tracked working files, so local checks include edits
# but never load ignored environment, dependencies, or package-manager config.
# Do not move or rewrite managed workspace files.
python3 -I - "${repo_root}" "${build_dir}" <<'PY'
from pathlib import Path
import shutil
import subprocess
import sys

root, stage = map(Path, sys.argv[1:])
# The SDK's tests compare its embedded trust inputs with these reviewed copies.
trust_anchors = ["scripts/ci/opensecret-release/dev-signing-key.pem",
                 "scripts/ci/opensecret-release/trusted_root.json"]
tracked = subprocess.check_output(["git", "-C", str(root), "ls-files", "-z", "sdk", *trust_anchors])
for raw in tracked.split(b"\0"):
    if not raw:
        continue
    relative = Path(raw.decode())
    if any(part.startswith(".env") for part in relative.parts) or relative.name == ".npmrc":
        continue
    source, destination = root / relative, stage / relative
    if source.is_symlink() or not source.is_file():
        raise SystemExit(f"Expected tracked regular SDK file: {relative}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination)
(stage / "sdk" / ".npmrc").write_text(
    "registry=https://registry.npmjs.org/\nignore-scripts=true\npackage-lock=false\n"
)
for name in ("npm-userconfig", "npm-globalconfig"):
    (stage / name).write_text("")
PY

# Publishing credentials and opt-in hosted test configuration have no role in
# this job. Nix still receives its normal toolchain/cache configuration.
unset NPM_TOKEN NODE_AUTH_TOKEN CARGO_REGISTRY_TOKEN CARGO_REGISTRIES_CRATES_IO_TOKEN
unset RUN_LIVE_AI ACTIONS_ID_TOKEN_REQUEST_TOKEN ACTIONS_ID_TOKEN_REQUEST_URL
for sdk_variable in "${!VITE_@}"; do unset "${sdk_variable}"; done
export NPM_CONFIG_USERCONFIG="${build_dir}/npm-userconfig"
export NPM_CONFIG_GLOBALCONFIG="${build_dir}/npm-globalconfig"

cd "${repo_root}"
# Each crate builds in its own pinned shell.
package_flake=./sdk
if [[ "${sdk_kind}" == proxy ]]; then
  package_flake=./proxy
fi
nix develop --no-update-lock-file "${package_flake}" -c bash -euo pipefail -c '
  sdk_kind="$1"
  build_dir="$2"
  repo_root="$3"
  output_dir="$4"
  cd "${build_dir}/sdk"
  case "${sdk_kind}" in
    npm)
      bun --no-env-file install --frozen-lockfile --ignore-scripts
      # GHSA-fc5q-39xc-vjpf: picomatch ReDoS with no fixed release, reached only
      # through build and lint tooling. Drop the flag here and in nightly.yml
      # once a fixed picomatch is published.
      bun --no-env-file audit --audit-level=high --ignore=GHSA-fc5q-39xc-vjpf
      bun --no-env-file run format:check
      bun --no-env-file run build
      VITE_OPEN_SECRET_API_URL=http://127.0.0.1:3000 \
        VITE_OPEN_SECRET_PCR_ENVIRONMENT=development \
        bun --no-env-file test \
          src/lib/test/*.test.ts \
          src/lib/test/integration/attestation.test.ts \
          src/lib/test/integration/developerHook.test.ts \
          src/lib/test/integration/pcr.test.ts \
          src/lib/test/integration/platformPushSettings.test.ts \
          src/lib/test/integration/web.test.ts \
          --timeout 30000
      bun --no-env-file pm pack --ignore-scripts --filename "${build_dir}/package.tgz"
      python3 -I "${repo_root}/scripts/ci/check-sdk-package-consumers.py" \
        "${build_dir}/sdk" "${build_dir}/package.tgz"
      python3 -I "${repo_root}/scripts/ci/check-sdk-packed-release-evidence.py" \
        "${build_dir}/sdk" "${build_dir}/package.tgz"
      cp "${build_dir}/package.tgz" "${output_dir}/package.tgz"
      ;;
    rust|proxy)
      # Preserve Cargo-generated source SHA/path metadata. Cargo refuses dirty
      # crate source; documentation/scripts elsewhere can be edited locally.
      if [[ "${sdk_kind}" == rust ]]; then
        crate_dir=sdk/rust
        crate_name=maple-sdk
        # Library tests do not load dotenv or hosted integration credentials.
        test_scope=(--lib)
      else
        crate_dir=proxy
        crate_name=maple-proxy
        # The proxy lane runs every test; none needs credentials.
        test_scope=()
      fi
      cd "${repo_root}/${crate_dir}"
      # Rustup cargo-subcommand proxies must use the owning Nix toolchain too.
      export RUSTUP_TOOLCHAIN="$(dirname "$(dirname "$(command -v rustc)")")"
      # This is the crate cache, never an Agent/shared desktop target directory.
      export CARGO_TARGET_DIR="${repo_root}/${crate_dir}/target"
      export RUSTFLAGS="-D warnings"
      cargo fmt --all -- --check
      cargo clippy --locked --all-targets --all-features -- -D warnings
      cargo test --locked --all-features "${test_scope[@]}"
      RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
      # Cargo also extracts and compiles this exact package before succeeding.
      # The proxy crate resolves maple-sdk from crates.io here, so a proxy that
      # needs a newer SDK fails until that SDK version is published.
      cargo package --locked --all-features
      package_version="$(cargo metadata --locked --no-deps --format-version 1 |
        jq -er --arg name "${crate_name}" '\''.packages | map(select(.name == $name)) |
          if length == 1 then .[0].version else error("Expected one crate") end'\'')"
      cp "${CARGO_TARGET_DIR}/package/${crate_name}-${package_version}.crate" \
        "${output_dir}/package.crate"
      ;;
  esac
' build-sdk-publish "${sdk_kind}" "${build_dir}" "${repo_root}" "${output_dir}"

echo "Validated ${sdk_kind} package written to ${output_dir}."
