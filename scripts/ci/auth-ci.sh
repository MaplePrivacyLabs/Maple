#!/usr/bin/env bash
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/auth-common.sh"
use_auth_environment pr
prepare_auth_tooling
print_auth_source_provenance
install_auth_deps
cd "$AUTH_APP_DIR"
bun --no-env-file run format:check
bun --no-env-file run lint
bun --no-env-file run typecheck
bun --no-env-file run test
