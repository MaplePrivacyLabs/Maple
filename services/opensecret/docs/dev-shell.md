# Development shell

Run these commands from `services/opensecret/` in the Maple monorepo. The
component retains its own `flake.nix`, `flake.lock`, and Rust toolchain.

`nix develop` provides the pinned toolchain and manages optional local state.
Before starting a cluster, its hook checks `localhost:$PGPORT`; if a server
responds, that listener is reused even when it does not belong to this
checkout. Otherwise the hook initializes or starts `$PGDATA`. It creates `.env`
from `.env.sample` only when `.env` is absent.

## Shell-hook controls

| Variable | Effect |
| --- | --- |
| `OPENSECRET_DEV_POSTGRES=0` | Do not inspect, initialize, or start PostgreSQL. |
| `OPENSECRET_DEV_ENV=0` | Do not create `.env`. |
| `OPENSECRET_DEV_CONTAINERS=0` | Do not configure Linux user-level container state. |
| `PGDATA` / `PGSOCKETS` / `PGPORT` | Select local PostgreSQL state, sockets, and listener. |
| `OPENSECRET_DEV_DATABASE_URL` | Set the database URL written into a newly generated `.env`. |
| `OPENSECRET_BIND_ADDR` | Select the backend listener instead of `127.0.0.1:3000`. |

For a pure check, disable every stateful hook and avoid changing the lockfile:

```sh
OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
  nix develop --no-write-lock-file '.?submodules=1' -c cargo fmt --all -- --check
```

For concurrent live checkouts, choose distinct `PGDATA`, `PGSOCKETS`, `PGPORT`,
`DATABASE_URL`, and backend bind addresses. Verify the database identity before
running migrations or destructive tests; never assume that a responding port
belongs to the current checkout.

## Recovery credential smoke

From this component, run:

```sh
bash scripts/test-recovery-flow.sh
```

Nix must be installed, but no manual shell setup is needed. The script enters
the pinned component Nix shell with the normal stateful hooks disabled, builds
this checkout, creates and migrates a private PostgreSQL cluster, and starts
the backend on an available loopback port. It runs the encrypted-HTTP client in
`src/recovery_smoke.rs`, preserving existing `.env`, PostgreSQL, and application
processes. Successful runs remove owned temporary state. Failed runs stop owned
processes and report the location of retained private logs and database files.

Coverage includes account creation, enrollment, preserving recovery, code reuse,
rotation, disablement, re-enrollment, destructive reset, legacy compatibility,
token lifecycle, invalid proof/code/payload scenarios, encrypted response
framing, and a scan of owned backend logs for generated secrets. It also injects
invalid stored recovery hash/envelope sizes and checks that recovery lookup and
completion return sanitized server errors without consuming the reset proof or
changing credentials. Restoring the fixture lets the same proof complete.

For an already running **disposable local backend**, set these environment
variables and run `bash scripts/test-recovery-flow.sh --existing`:

| Variable | Requirement |
| --- | --- |
| `RECOVERY_SMOKE_URL` | Backend URL using `http://127.0.0.1` and its port. |
| `RECOVERY_SMOKE_DATABASE_URL` | Fully migrated PostgreSQL database on `127.0.0.1`, named `opensecret_recovery_*`. |
| `ENCLAVE_SECRET_MOCK` | Matching local backend mock enclave secret. |
| `RECOVERY_SMOKE_LOG` | Optional backend log path to enable secret-leak assertions. |

Existing-backend mode modifies and deletes only its newly registered test users;
it does not start or stop that backend/database. Use `--help` for usage.

### Evidence limits

The real `/password-reset/request` route creates each reset row. Without email
credentials, the client replaces only its new test user's reset-code MAC with
a random known email-code fixture. The backend-created secret hash, proof
verification, completion, encryption, and persistence paths are exercised;
email delivery is not tested.

The client uses the backend's transport-v2 crypto/envelope primitives with local
mock attestation and transcript checks. It does not verify Nitro certificates
or exercise SDK/GUI integration. Production attestation/PCRs, external provider
delivery, deployed rate limits, and production log hygiene require separate
evidence. The migration down/up check runs before recovery enrollment; it does
not establish populated rollback safety, since the down migration rejects
existing recovery rows.

## Logging

`APP_MODE=local` writes line-buffered tracing to stdout so redirected `cargo run`
logs appear immediately. When `RUST_LOG` is unset the default is
`opensecret=debug` plus `axum_login`, `tower_sessions`, `sqlx=warn`, and
`tower_http`. Override `RUST_LOG` to quiet or expand that set. Do not log
secrets, tokens, decrypted bodies, or raw provider payloads.
