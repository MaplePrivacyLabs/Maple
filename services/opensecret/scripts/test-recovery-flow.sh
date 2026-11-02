#!/usr/bin/env bash
# Real encrypted HTTP recovery smoke. No email/provider credentials are used.
set -Eeuo pipefail
set +x
umask 077

component="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
if [[ "${1:-}" != --in-shell ]]; then
  cd "$component"
  exec env OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
    nix develop --no-update-lock-file '.?submodules=1' -c bash \
    "$component/scripts/test-recovery-flow.sh" --in-shell "$@"
fi
shift
cd "$component"
case "${1:-}" in
  --help)
    printf '%s\n' 'Usage: bash scripts/test-recovery-flow.sh [--existing]' \
      'Default: build this checkout, migrate a private temporary PostgreSQL cluster,' \
      'start a loopback backend, exercise recovery, and remove owned state.' \
      '--existing: require RECOVERY_SMOKE_URL, RECOVERY_SMOKE_DATABASE_URL and' \
      'ENCLAVE_SECRET_MOCK for a disposable local backend. Database name must start' \
      'opensecret_recovery_. Only newly registered smoke users are modified/deleted.' \
      'Optional RECOVERY_SMOKE_LOG enables credential/log-leak assertions.' \
      'Local mock attestation only; email delivery is replaced by a test-row MAC fixture.'
    exit 0 ;;
  --existing)
    : "${RECOVERY_SMOKE_URL:?required}" "${RECOVERY_SMOKE_DATABASE_URL:?required}" "${ENCLAVE_SECRET_MOCK:?required}"
    [[ $# == 1 ]]
    exec cargo test --locked --all-features local_flow_smoke::encrypted_account_flow -- --ignored --exact --nocapture ;;
  '') [[ $# == 0 ]] ;;
  *) printf 'Unknown argument; use --help\n' >&2; exit 2 ;;
esac

for cmd in initdb pg_ctl psql createdb diesel cargo python3 curl openssl; do
  command -v "$cmd" >/dev/null || { printf 'Missing dependency: %s\n' "$cmd" >&2; exit 1; }
done
# Compile before owning processes, so no test cluster sits idle during a build.
cargo build --locked
cargo test --locked --all-features local_flow_smoke::encrypted_account_flow --no-run
binary="$(cargo metadata --locked --no-deps --format-version=1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"] + "/debug/opensecret")')"
parent="${TMPDIR:-/tmp}"
work="$(mktemp -d "$parent/opensecret-recovery.XXXXXXXX")"
readonly work
server_pid=''
cleanup() {
  status=$?
  trap - EXIT INT TERM
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  if [[ -f "$work/pgdata/PG_VERSION" ]] && pg_ctl status -D "$work/pgdata" >/dev/null 2>&1; then
    pg_ctl stop -D "$work/pgdata" -m fast -w >/dev/null || exit 1
  fi
  if [[ $status != 0 ]]; then
    printf 'Recovery smoke failed; private logs retained in %s\n' "$work" >&2
  else
    [[ -d "$work" && ! -L "$work" && -O "$work" && -f "$work/.owned-recovery-smoke" ]] || exit 1
    rm -rf -- "$work"
    printf 'Recovery HTTP smoke passed; owned backend and temporary database removed.\n'
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
touch "$work/.owned-recovery-smoke"
mkdir "$work/sockets"
read -r pgport httpport < <(python3 - <<'PY'
import socket
with socket.socket() as pg, socket.socket() as http:
    pg.bind(('127.0.0.1', 0))
    http.bind(('127.0.0.1', 0))
    print(pg.getsockname()[1], http.getsockname()[1])
PY
)
initdb -D "$work/pgdata" --encoding=UTF8 --auth-local=trust --auth-host=scram-sha-256 >"$work/initdb.log"
pg_ctl start -D "$work/pgdata" -o "-h 127.0.0.1 -p $pgport -k $work/sockets" -l "$work/postgres.log" -w >/dev/null
[[ "$(psql -X -h "$work/sockets" -p "$pgport" -d postgres -Atqc 'SHOW data_directory')" == "$work/pgdata" ]]
psql -X -h "$work/sockets" -p "$pgport" -d postgres -v ON_ERROR_STOP=1 \
  -c "CREATE USER opensecret_user WITH PASSWORD 'password'" >/dev/null
database="opensecret_recovery_$(openssl rand -hex 6)"
createdb -h "$work/sockets" -p "$pgport" -O opensecret_user "$database"
export DATABASE_URL="postgres://opensecret_user:password@127.0.0.1:$pgport/$database"
export RECOVERY_SMOKE_DATABASE_URL="$DATABASE_URL"
diesel migration run --locked-schema >"$work/migrations.log"
diesel migration redo --locked-schema >>"$work/migrations.log"
export RECOVERY_SMOKE_URL="http://127.0.0.1:$httpport"
export ENCLAVE_SECRET_MOCK
ENCLAVE_SECRET_MOCK="$(openssl rand -hex 32)"
export RECOVERY_SMOKE_LOG="$work/backend.log"
# Start from a private directory with a minimal environment: do not discover a
# developer's .env, provider keys or externally managed process configuration.
(
  cd "$work"
  exec env -i PATH="$PATH" HOME="$work" APP_MODE=local DATABASE_URL="$DATABASE_URL" \
    ENCLAVE_SECRET_MOCK="$ENCLAVE_SECRET_MOCK" JWT_SECRET="$(openssl rand -hex 32)" \
    TINFOIL_API_KEY=local-smoke-placeholder OPENAI_API_BASE=http://127.0.0.1:9 \
    OPENSECRET_BIND_ADDR="127.0.0.1:$httpport" RUST_LOG=opensecret=debug \
    "$binary"
) >"$RECOVERY_SMOKE_LOG" 2>&1 &
server_pid=$!
ready=0
for ((attempt=0; attempt<90; attempt++)); do
  kill -0 "$server_pid" 2>/dev/null || { printf 'Backend exited during startup\n' >&2; exit 1; }
  if curl --noproxy '*' --silent --fail --max-time 1 "$RECOVERY_SMOKE_URL/health-check" >/dev/null; then ready=1; break; fi
  sleep 1
done
[[ $ready == 1 ]] || { printf 'Backend readiness timed out\n' >&2; exit 1; }
cargo test --locked --all-features local_flow_smoke::encrypted_account_flow -- --ignored --exact --nocapture
