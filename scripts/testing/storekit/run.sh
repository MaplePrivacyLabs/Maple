#!/usr/bin/env bash
set -euo pipefail

# Invoke through .#apple so native linker variables are prepared here.
# A deliberate local comparison can select Xcode 26.6 without changing pins:
# nix develop --no-update-lock-file .#apple -c env \
#   MAPLE_NIX_XCODE_VERSION=26.6 DEVELOPER_DIR=/Applications/Xcode_26.6.app/Contents/Developer \
#   bash scripts/testing/storekit/run.sh ...
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../ci" && pwd)/_common.sh"
use_xcode_toolchain
exec python3 -B "${REPO_ROOT}/scripts/testing/storekit/run.py" "$@"
