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

## Logging

`APP_MODE=local` writes line-buffered tracing to stdout so redirected `cargo run`
logs appear immediately. When `RUST_LOG` is unset the default is
`opensecret=debug` plus `axum_login`, `tower_sessions`, `sqlx=warn`, and
`tower_http`. Override `RUST_LOG` to quiet or expand that set. Do not log
secrets, tokens, decrypted bodies, or raw provider payloads.

Non-success inference responses add `upstream_diagnostic` to the correlated
`Inference attempt failed` warning. The diagnostic reader retains at most 8 KiB
and waits at most 100 ms in total. It emits only allowlisted error codes/types
and fixed summaries for recognized errors, never a raw message excerpt. Unknown
text is suppressed; empty, truncated, invalid, interrupted, or timed-out bodies
have explicit diagnostic states. HTTP status and safe retry hints remain the
routing contract regardless of diagnostic availability. This applies to both
standard and attested inference transports; it does not add retries.

At INFO level, `Inference routing decision` records V2 alternate Auto choices
and sticky/fallback provider choices after the send-time route claim. Join its
request, execution and attempt IDs with response-start and terminal records;
the decision alone proves neither success nor client receipt. It includes the
selector mode, surface, workload, actual model/provider, reason/source, policy
versions and skipped Auto candidates with their typed reasons and retry hints,
without an account identifier. These are selection-time observations; a retained
sticky choice can have no skipped candidates and does not establish the preferred
model's health. Ordinary primary/weighted routing
remains at DEBUG, so INFO-only logs are not a complete traffic denominator.
