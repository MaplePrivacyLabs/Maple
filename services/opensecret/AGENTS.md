# OpenSecret agent guide

This guide applies to `services/opensecret/`; read the
[root guide](../../AGENTS.md). Paths/commands use this component unless labeled
monorepo-root. This is one Rust package/binary with its own pinned Nix flake,
lockfile, and toolchain.

## Choose the workflow

- `$develop-opensecret`: isolated local setup, stateful shell, migrations,
  provider topology, and externally owned environment/processes.
- `$change-opensecret-api`: routes/middleware, encrypted HTTP, Responses,
  streaming, and client compatibility.
- `$change-opensecret-provider`: models/routing, adapters/transport, retries,
  forwarded headers, and usage.
- `$validate-opensecret`: Rust CI, disposable database, encrypted-client smoke,
  provider and artifact evidence.
- `$review-opensecret-security`: trust-boundary work or security review.

Read the relevant [implementation contracts](docs/development-contracts.md)
for source ownership, API/provider boundaries, persistence, and privacy.

## Local development essentials

- Use an isolated linked Local stack for backend changes and relevant client,
  log, or billing integration. Preserve externally generated env files, ports,
  database state, and processes; use their owner's lifecycle instructions.
  Client configuration/login follows the selected
  [environment contract](../../docs/development-environments.md).
- Initialize `services/opensecret/privatemode-public` from the monorepo root
  before builds/tests; `nitro-toolkit/` is ordinary tracked component source.
- `nix develop` has stateful PostgreSQL, `.env`, and Linux container hooks.
  Read [shell controls](docs/dev-shell.md) before pure checks or concurrent
  startup. The backend hook runs formatting/Clippy/tests with stateful hooks
  disabled; it does not prove database, client, EIF, or PCR behavior.
- Run `just diesel-migration-run-local` before startup against the identified
  local database. Startup does not run Diesel schema migrations;
  `src/migrations.rs` is separate application-data logic. Never target shared
  or remote state with local migrations/tests.
- Provider keys are service-owned in `secretspec.toml`, resolved only by explicit
  check/run recipes. Follow [local macOS stack](docs/local-macos-stack.md).
  On Linux x86_64, build and run the proxy with the
  [x86_64 recipe](docs/local-linux-x86_64-proxy.md). That recipe does not
  replace the checked-in aarch64 binary or the macOS `.local/bin` flow.
  Do not retrieve secrets in shell hooks or copy them into generated `.env`.
  Tinfoil is in-process; Continuum may use its native proxy. Generated local
  auth/database state is separate from provider credentials.

## Essential API, provider, and persistence invariants

- OpenSecret owns authentication/authorization, encrypted persistence, provider
  secrets/routing, model policy, entitlements, and usage truth. Clients own
  presentation and device effects; billing/flags are external HTTP dependencies.
- An encryption session is protected transport, not user authorization. Derive
  auth/middleware from router assembly. Protected routes, including bodyless
  ones, use an SDK/encrypted client; plain `curl` proves health only. JWT and
  API-key contexts have separate ownership/permissions.
- Validate inputs before database/provider effects. Preserve methods/status,
  sanitized errors, encryption, stream ordering/terminal conditions, cancellation,
  usage attribution, and independently updated client compatibility.
- Keep public model IDs distinct from provider IDs and policy. Route from
  authenticated identity; retry only failures known to precede acceptance.
  Ambiguous POSTs and partial streams are not generally safe to replay.
- Enforce database ownership in queries. Add timestamped reversible Diesel
  migrations rather than rewriting deployed history. Identify the owning key
  and version ciphertext; user-key data needs authenticated dual-read/new-write
  and lazy rewrite, not opaque SQL/startup re-encryption.
- Never log credentials, raw auth/headers, decrypted data, prompts/responses,
  provider bodies, or sensitive payloads. Keep safe metadata bounded/allowlisted.
  Preserve capacity/expiry, one-use/lease, cleanup, cancellation, SSRF and
  fail-closed behavior at untrusted and external boundaries.

## Validation and operator authority

Keep source, unit, disposable DB, encrypted client, provider, artifact, and
deployed evidence distinct. Report exact commands/counts, endpoint/account
scope, ignored/skipped tests, and unverified layers through `$validate-opensecret`.

Ordinary PRs do not require new PCR approvals. Root backend/SDK CI validates
code and in-tree compatibility; EIF checks and protected signing have separate
triggers/authority. Never change or sign approvals to clear a check. Preserve
[CI and cache boundaries](../../docs/repository-workflows.md) and
[manual signed-PCR compatibility](docs/pcr-compatibility.md), installed-client
raw URLs, and the current verification key. CI does not deploy the service.

PCR/history/signing, KMS/IAM, shared/remote migrations, artifact transfer, enclave
lifecycle, service restart, secrets writes, staging, and deployment require
explicit authority for the action/environment. Use the existing
[Nitro runbook](docs/nitro-deploy.md) and [signing contract](secretspec/README.md)
only for authorized operator work. Inspect recipe effects before running them.

Keep findings in the task; update material contract/workflow changes in the
owning reference and retain the root public/private documentation boundary.
