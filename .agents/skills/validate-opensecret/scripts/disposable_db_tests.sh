#!/usr/bin/env bash
set -Eeuo pipefail
umask 077
export LC_ALL=C

usage() {
  printf 'usage: %s [--redo-latest]\n' "${0##*/}" >&2
}

redo_latest=0
case "${1:-}" in
  "") ;;
  --redo-latest) redo_latest=1 ;;
  *)
    usage
    exit 2
    ;;
esac
if [ "$#" -gt 1 ]; then
  usage
  exit 2
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
maple_root="$(git -C "$script_dir" rev-parse --show-toplevel)"
repo_root="$maple_root/services/opensecret"
if [ ! -f "$repo_root/Cargo.toml" ] || [ ! -d "$repo_root/migrations" ]; then
  printf 'OpenSecret component is missing at %s\n' "$repo_root" >&2
  exit 1
fi
cd "$repo_root"

for required_command in initdb pg_ctl psql createdb diesel cargo python3 openssl; do
  if ! command -v "$required_command" >/dev/null 2>&1; then
    printf 'missing required command: %s\n' "$required_command" >&2
    exit 1
  fi
done

tmp_parent="${TMPDIR:-/tmp}"
tmp_parent="$(cd "$tmp_parent" && pwd -P)"
workdir="$(mktemp -d "$tmp_parent/opensecret-db-tests.XXXXXX")"
pgdata="$workdir/pgdata"
readonly workdir pgdata tmp_parent
pgsockets="$workdir/sockets"
run_token="$(openssl rand -hex 16)"
marker="$workdir/.opensecret-disposable-db"
printf '%s\n' "$run_token" >"$marker"
pgport="$(python3 - <<'PY'
import socket

with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)"
admin_user="$(id -un)"
test_database="opensecret_agent_test_$(openssl rand -hex 6)"
tests_passed=0

cleanup() {
  status=$?
  trap - EXIT INT TERM ERR

  case "$workdir" in
    "$tmp_parent"/opensecret-db-tests.*) ;;
    *)
      printf 'refusing to clean unexpected temporary directory: %s\n' \
        "$workdir" >&2
      exit 1
      ;;
  esac

  if [ -L "$workdir" ] || [ ! -d "$workdir" ] ||
    [ "$(dirname "$workdir")" != "$tmp_parent" ] ||
    [ "$pgdata" != "$workdir/pgdata" ] ||
    [ "$(cat "$marker" 2>/dev/null || true)" != "$run_token" ] ||
    [ "$(find "$workdir" -prune -user "$(id -u)" -print)" != "$workdir" ]; then
    printf 'refusing to clean unverified temporary directory: %s\n' \
      "$workdir" >&2
    exit 1
  fi

  if [ -f "$pgdata/PG_VERSION" ] && pg_ctl status -D "$pgdata" >/dev/null 2>&1; then
    if ! pg_ctl stop -D "$pgdata" -m fast >/dev/null; then
      printf 'PostgreSQL did not stop; preserving temporary state at %s\n' "$pgdata" >&2
      exit 1
    fi
  fi
  if [ -f "$pgdata/PG_VERSION" ] && pg_ctl status -D "$pgdata" >/dev/null 2>&1; then
    printf 'PostgreSQL still reports running; preserving temporary state at %s\n' \
      "$pgdata" >&2
    exit 1
  fi
  rm -rf -- "$workdir"

  if [ "$status" -eq 0 ] && [ "$tests_passed" -eq 1 ]; then
    printf 'Disposable-DB evidence: %s AEAD/database tests, %s OAuth database tests, and %s callback settings/transport tests passed; no tests skipped; temporary cluster removed.\n' \
      "$aead_count" "$oauth_count" "$callback_count"
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'status=$?; printf "disposable database validation failed at line %s (status %s)\n" "$LINENO" "$status" >&2; exit "$status"' ERR

initdb -D "$pgdata" --username="$admin_user" --encoding=UTF8 \
  --auth-local=trust --auth-host=scram-sha-256 >/dev/null
mkdir -p "$pgsockets"
pg_ctl start -D "$pgdata" \
  -o "-h 127.0.0.1 -p $pgport -k $pgsockets" \
  -l "$workdir/postgres.log" -w >/dev/null

expected_pgdata="$(cd "$pgdata" && pwd -P)"
reported_pgdata="$(psql -h "$pgsockets" -p "$pgport" -U "$admin_user" \
  -d postgres -Atqc 'SHOW data_directory')"
reported_pgdata="$(cd "$reported_pgdata" && pwd -P)"
test "$reported_pgdata" = "$expected_pgdata"
test "$(psql -h "$pgsockets" -p "$pgport" -U "$admin_user" \
  -d postgres -Atqc 'SHOW port')" = "$pgport"

psql -h "$pgsockets" -p "$pgport" -U "$admin_user" -d postgres \
  -v ON_ERROR_STOP=1 \
  -c "CREATE USER opensecret_user WITH PASSWORD 'password'" >/dev/null
createdb -h "$pgsockets" -p "$pgport" -U "$admin_user" \
  --maintenance-db=postgres --owner=opensecret_user "$test_database"

export DATABASE_URL="postgres://opensecret_user:password@127.0.0.1:${pgport}/${test_database}"
export AEAD_TAMPER_TEST_DATABASE_URL="$DATABASE_URL"

assert_database_identity() {
  local db_identity
  local expected_identity
  db_identity="$(psql "$AEAD_TAMPER_TEST_DATABASE_URL" -Atqc \
    "SELECT current_database() || '|' || current_user || '|' ||
            pg_get_userbyid(datdba) || '|' || host(inet_server_addr()) || '|' ||
            current_setting('port')
       FROM pg_database
      WHERE datname = current_database()")"
  expected_identity="$test_database|opensecret_user|opensecret_user|127.0.0.1|$pgport"
  if [ "$db_identity" != "$expected_identity" ]; then
    printf 'unexpected database identity: expected %s, received %s\n' \
      "$expected_identity" "$db_identity" >&2
    return 1
  fi
}

assert_migration_count() {
  local applied_migrations
  applied_migrations="$(psql "$AEAD_TAMPER_TEST_DATABASE_URL" -Atqc \
    'SELECT count(*) FROM __diesel_schema_migrations')"
  test "$applied_migrations" -eq "$expected_migrations"
}

assert_database_identity
test "$(psql "$AEAD_TAMPER_TEST_DATABASE_URL" -Atqc \
  "SELECT count(*) FROM information_schema.tables
    WHERE table_schema = 'public'")" -eq 0

diesel migration run --locked-schema \
  --database-url "$AEAD_TAMPER_TEST_DATABASE_URL"
expected_migrations="$(find migrations -type f -name up.sql | wc -l | tr -d ' ')"
test "$expected_migrations" -gt 0
assert_migration_count

if [ "$redo_latest" -eq 1 ]; then
  diesel migration redo --locked-schema \
    --database-url "$AEAD_TAMPER_TEST_DATABASE_URL"
  assert_migration_count
fi

cargo test --locked --all-features aead_db_tamper_tests \
  -- --ignored --list | tee "$workdir/aead-tests.list"
aead_count="$(awk '/^aead_db_tamper_tests::.*: test$/ { count++ }
  END { print count + 0 }' "$workdir/aead-tests.list")"
test "$aead_count" -gt 0

cargo test --locked --all-features web::oauth_routes::tests::db_ \
  -- --ignored --list | tee "$workdir/oauth-tests.list"
oauth_count="$(awk '/^web::oauth_routes::tests::db_.*: test$/ { count++ }
  END { print count + 0 }' "$workdir/oauth-tests.list")"
test "$oauth_count" -gt 0

cargo test --locked --all-features aead_db_tamper_tests \
  -- --ignored --test-threads=1 --nocapture 2>&1 | tee "$workdir/aead-tests.log"
if grep -qi 'skipping:' "$workdir/aead-tests.log"; then
  printf 'AEAD/database test output contained a skip marker\n' >&2
  exit 1
fi
grep -Eq "test result: ok\\. ${aead_count} passed; 0 failed; 0 ignored;" \
  "$workdir/aead-tests.log"

cargo test --locked --all-features web::oauth_routes::tests::db_ \
  -- --ignored --test-threads=1 --nocapture 2>&1 | tee "$workdir/oauth-tests.log"
if grep -qi 'skipping:' "$workdir/oauth-tests.log"; then
  printf 'OAuth database test output contained a skip marker\n' >&2
  exit 1
fi
grep -Eq "test result: ok\\. ${oauth_count} passed; 0 failed; 0 ignored;" \
  "$workdir/oauth-tests.log"

callback_count=0
for callback_filter in \
  db::tests::db_oauth_settings_ \
  transport_v2::gateway::tests::db_oauth_callback_selection_v1_v2 \
  transport_v2::gateway::tests::db_oauth_callback_completion_; do
  cargo test --locked --all-features "$callback_filter" \
    -- --ignored --list >"$workdir/callback-tests.list"
  selected_count="$(awk -v prefix="$callback_filter" \
    'index($0, prefix) == 1 && /: test$/ { count++ }
     END { print count + 0 }' "$workdir/callback-tests.list")"
  test "$selected_count" -gt 0
  if [ "$callback_filter" = "transport_v2::gateway::tests::db_oauth_callback_completion_" ]; then
    # Both Google and GitHub must complete through both encrypted transports.
    test "$selected_count" -eq 4
  fi

  cargo test --locked --all-features "$callback_filter" \
    -- --ignored --test-threads=1 --nocapture 2>&1 | tee "$workdir/callback-tests.log"
  if grep -qi 'skipping:' "$workdir/callback-tests.log"; then
    printf 'OAuth callback test output contained a skip marker\n' >&2
    exit 1
  fi
  grep -Eq "test result: ok\\. ${selected_count} passed; 0 failed; 0 ignored;" \
    "$workdir/callback-tests.log"
  callback_count=$((callback_count + selected_count))
done

assert_database_identity
assert_migration_count
tests_passed=1
