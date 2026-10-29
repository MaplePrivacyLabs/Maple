#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=_common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"

print_source_provenance
export WRANGLER_SEND_METRICS=false
cd "${REPO_ROOT}"
python3 scripts/ci/test_agent_updates_workflows.py
python3 -I -m unittest discover -s services/download-tracking -p 'test_*.py'
python3 -I -m unittest discover -s services/agent-updates/publisher -p 'test_*.py'
cd services/agent-updates
bun install --frozen-lockfile --ignore-scripts
bun run check
