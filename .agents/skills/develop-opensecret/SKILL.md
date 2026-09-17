---
name: develop-opensecret
description: Set up and run the open-source OpenSecret Rust backend. Use when starting backend work, entering its pinned Nix/PostgreSQL environment, initializing submodules, running Diesel migrations, configuring the local provider stack, or deciding whether behavior belongs in OpenSecret or a client.
---

# Develop OpenSecret

## Start safely

Read the monorepo-root `AGENTS.md` and `services/opensecret/AGENTS.md`, inspect
the worktree, and preserve unrelated changes. For new work, prefer current
`origin/master` unless the task names another base. From the monorepo root,
initialize public submodules and enter the backend component before building:

```sh
git submodule update --init --recursive -- services/opensecret/nitro-toolkit services/opensecret/privatemode-public
cd services/opensecret
```

Subsequent paths and commands in this skill are relative to
`services/opensecret/` unless labeled otherwise. Use its pinned Nix environment
and run Cargo from that component; it is one Rust package. Keep deployment,
shared migration, PCR mutation, signing, and
remote-enclave operations outside routine development unless the user
authorizes the exact action and environment. Root `opensecret-ci.yml` and
`sdk-integration.yml` own backend validation; they do not build or publish EIFs
or deploy the TEE service. Follow `docs/pcr-compatibility.md` for the manual
signed-PCR publication contract.

## Enter the toolchain deliberately

```sh
OPENSECRET_DEV_CONTAINERS=0 nix develop --no-update-lock-file '.?submodules=1'
```

The repository pre-commit hook runs `cargo fmt --check`, Clippy, and
`cargo test` through `.githooks/pre-commit` in this shell with
`OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0`
when backend files are staged. It refuses to run when the submodules are not
checked out; the nix build, cargo-deny, EIF, and PCR checks stay in CI.

The shell may reuse a PostgreSQL listener, start `.pgdata`, and create `.env`
when absent. On Linux, container setup also changes user-level state unless
disabled. Read `docs/dev-shell.md` for controls and concurrent-checkout
isolation. Treat `.env`, `.pgdata/`, and `.local/` as private, gitignored state;
never point local migrations or tests at a shared, preview, or production
database.

## Prepare PostgreSQL

Run SQL migrations before starting the backend:

```sh
OPENSECRET_DEV_CONTAINERS=0 nix develop --no-update-lock-file '.?submodules=1' -c just diesel-migration-run-local
```

`src/migrations.rs` is application-data migration logic, not the Diesel runner.
For a schema change, create a new reversible migration and let Diesel regenerate
`src/models/schema.rs`:

```sh
OPENSECRET_DEV_CONTAINERS=0 nix develop --no-update-lock-file '.?submodules=1' -c \
  just diesel-migration-generate add_user_preferences
```

Replace the example name with the change being made. Use
`$validate-opensecret` for rollback, upgrade-shaped data, encrypted persistence,
and ignored database-test proof.

## Configure and run locally

Derive supported configuration from `.env.sample` and startup source. Tinfoil
is an in-process provider dependency; the macOS stack can also run the native
Continuum proxy. Follow `docs/local-macos-stack.md` for the service-owned
SecretSpec/BWS manifest, explicit credential checks, process topology, and Maple
wiring. Local run recipes resolve provider credentials from BWS at invocation;
reuse the existing Keychain login and keep values out of `.env`/secret files. Do not start a Tinfoil sidecar.

Billing and feature flags are optional external HTTP API boundaries. Configure
their URLs and backend-only credentials only when the task exercises their
public outcomes; do not pull their server implementations into this setup.

Plain `curl` is suitable for health probes, not protected-route proof. Use the
SDK in the monorepo-root `sdk/` directory or the corresponding Maple application
client for authenticated encrypted smoke tests. Preserve externally managed
environment files, service ports, and processes; use the owning workspace
manager's lifecycle commands when it provides this stack.

## Follow ownership

Use the source map and frontend/backend boundary in `AGENTS.md`. In particular,
keep server policy, provider credentials/routing, authorization, durable
storage, and cryptography in OpenSecret; keep presentation, device integration,
and local user interaction in clients.

Load the narrow sibling workflow when the task reaches it:

- `$change-opensecret-api` for routes, middleware, encrypted contracts,
  Responses, or client compatibility.
- `$change-opensecret-provider` for models, routing, transport, provider
  adapters, headers, retries, or usage.
- `$review-opensecret-security` for a trust-boundary change or security review.
- `$validate-opensecret` before claiming implementation complete.

Apply the union when a task crosses skills, but keep code at the narrowest
owning layer.
