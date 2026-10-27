#!/usr/bin/env bash
# ci.yml's os-sdk-e2e lane: both in-tree SDKs against the in-tree OpenSecret
# backend and a disposable PostgreSQL on 127.0.0.1:5432 (a ci.yml service; the
# backend's dev shell uses the same database URL). The EXIT trap stops the
# backend and, on failure, prints its log.
set -euo pipefail
cd "$(dirname "$0")/../.."

export APP_MODE=local
export DATABASE_URL=postgres://opensecret_user:password@127.0.0.1:5432/opensecret
export ENCLAVE_SECRET_MOCK=1111111111111111111111111111111111111111111111111111111111111111
export JWT_SECRET=2222222222222222222222222222222222222222222222222222222222222222
export OPENAI_API_BASE=http://127.0.0.1:9
export OPENSECRET_BIND_ADDR=127.0.0.1:3000
export OPENSECRET_DEV_CONTAINERS=0
export OPENSECRET_DEV_DATABASE_URL=postgres://opensecret_user:password@127.0.0.1:5432/opensecret
export OPENSECRET_DEV_ENV=0
export OPENSECRET_DEV_POSTGRES=0
export RUST_LOG=opensecret=info
export TINFOIL_API_KEY=sdk-integration-placeholder
export VITE_OPEN_SECRET_API_URL=http://127.0.0.1:3000
export VITE_OPEN_SECRET_PCR_ENVIRONMENT=development
export VITE_TEST_CLIENT_ID=ba5a14b5-d915-47b1-b7b1-afda52bc5fc6
export VITE_TEST_DEVELOPER_EMAIL=sdk-ci-developer@local.test
export VITE_TEST_DEVELOPER_INVITE_CODE=11111111-1111-4111-8111-111111111111
export VITE_TEST_DEVELOPER_NAME='SDK CI Developer'
export VITE_TEST_DEVELOPER_PASSWORD=Maple-sdk-ci-test-1234
export VITE_TEST_EMAIL=sdk-ci-user@local.test
export VITE_TEST_NAME='SDK CI User'
export VITE_TEST_PASSWORD=Maple-sdk-ci-test-1234
RUNNER_TEMP="${RUNNER_TEMP:-$(mktemp -d)}"

finish() {
  status=$?
  # Show OpenSecret log on failure
  if [[ $status -ne 0 ]]; then
    tail -n 300 "${RUNNER_TEMP}/opensecret.log" || true
  fi
  # Stop OpenSecret
  if [[ -f "${RUNNER_TEMP}/opensecret.pid" ]]; then
    kill "$(cat "${RUNNER_TEMP}/opensecret.pid")" 2>/dev/null || true
  fi
}
trap finish EXIT

# Migrate the disposable database and build OpenSecret
nix develop --no-update-lock-file './services/opensecret?submodules=1#ci' -c bash -lc '
  set -euo pipefail
  cd services/opensecret
  diesel migration run
  cargo build --locked
'
nix develop --no-update-lock-file './services/opensecret?submodules=1#ci' -c \
  psql "${DATABASE_URL}" -X -v ON_ERROR_STOP=1 \
  -f sdk/test/integration/bootstrap.sql

# Verify OAuth persistence and encrypted callback flows
nix develop --no-update-lock-file './services/opensecret?submodules=1#ci' -c bash -lc '
  set -euo pipefail
  cd services/opensecret
  bash ../../.agents/skills/validate-opensecret/scripts/disposable_db_tests.sh
'

# Start and verify OpenSecret
(
  nix develop --no-update-lock-file './services/opensecret?submodules=1#ci' -c \
    services/opensecret/target/debug/opensecret \
    > "${RUNNER_TEMP}/opensecret.log" 2>&1 &
  echo "$!" > "${RUNNER_TEMP}/opensecret.pid"

  for _ in $(seq 1 60); do
    if curl --fail --silent --show-error --max-time 2 \
      "${VITE_OPEN_SECRET_API_URL}/health-check" >/dev/null; then
      echo "OpenSecret is ready."
      exit 0
    fi
    if ! kill -0 "$(cat "${RUNNER_TEMP}/opensecret.pid")" 2>/dev/null; then
      echo "OpenSecret exited before becoming ready." >&2
      tail -n 200 "${RUNNER_TEMP}/opensecret.log" >&2
      exit 1
    fi
    sleep 2
  done

  echo "OpenSecret did not become ready." >&2
  tail -n 200 "${RUNNER_TEMP}/opensecret.log" >&2
  exit 1
)

# Run TypeScript SDK integration tests
nix develop --no-update-lock-file ./sdk -c bash -lc '
  set -euo pipefail
  cd sdk
  bun install --frozen-lockfile --ignore-scripts
  bun test src/lib/test/integration/api.test.ts --timeout 30000
  bun test src/lib/test/integration/ai.test.ts --timeout 30000
  bun test src/lib/test/integration/apiKeys.test.ts --timeout 30000
  bun test src/lib/test/integration/bip85.test.ts --timeout 30000
  bun test src/lib/test/integration/developer.test.ts --timeout 30000
  bun test src/lib/test/integration/oauthRedirect.test.ts --timeout 30000
  bun test src/lib/test/integration/signing.test.ts --timeout 30000
'

# Run Rust SDK integration tests
nix develop --no-update-lock-file ./sdk -c bash -lc '
  set -euo pipefail
  cd sdk/rust
  cargo test --locked --all-features --tests
  cargo test --locked --all-features --lib \
    client::tests::native_handoff_full_stack_round_trip \
    -- --ignored --exact --nocapture
'
