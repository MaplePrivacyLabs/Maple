---
name: develop-opensecret
description: Set up and implement the OpenSecret backend using its pinned component shell, isolated local state, SQL migrations, provider stack, and owned API/provider contracts.
---

# Develop OpenSecret

Read root/component `AGENTS.md`, affected source/tests, and choose the Local
stack required for backend work. Preserve externally owned ignored environments,
ports, database/account state, and processes; use the owner's lifecycle when
provided. Commands below use the backend component.

## Enter the owning environment

From the monorepo root, initialize the public dependency before builds/tests:

```sh
git submodule update --init --recursive -- services/opensecret/privatemode-public
cd services/opensecret
OPENSECRET_DEV_CONTAINERS=0 nix develop --no-update-lock-file '.?submodules=1'
```

This is one Rust package. `nitro-toolkit/` is tracked backend source, not a
submodule. The shell can reuse/start PostgreSQL, create missing `.env`, and
change Linux container state. Read [shell controls](../../../services/opensecret/docs/dev-shell.md)
before pure checks or concurrent startup. Keep private local state ignored;
never aim local migrations/tests at shared or remote databases.

For pure checks disable all hooks with `OPENSECRET_DEV_POSTGRES=0`,
`OPENSECRET_DEV_ENV=0`, and `OPENSECRET_DEV_CONTAINERS=0` before `nix develop`.
The pre-commit hook does this for backend formatting/Clippy/tests; database,
client, Nix/build, EIF and PCR evidence are separate.

## Prepare and run the local stack

For standalone state, use [local macOS setup](../../../services/opensecret/docs/local-macos-stack.md)
and `.env.sample`/startup source. When state is externally owned, use its
commands and generated configuration instead of recreating standalone defaults.
Run `just diesel-migration-run-local` against the identified local DB before
backend startup. `src/migrations.rs` is application-data logic, not Diesel.
For schema work create a new reversible migration with the owning recipe and
regenerate `src/models/schema.rs`; do not rewrite deployed history.

Provider credentials are resolved by explicit service-owned SecretSpec check/
run recipes; reuse the supported keyring login and never copy values into
`.env`/secret files. Tinfoil is in-process, with no sidecar; the native
Continuum proxy is a separate process. Generated JWT/database/account fixtures
belong to their workspace, independently of provider credentials.

Follow [client environments and login](../../../docs/development-environments.md)
for effective API/project values and encrypted local password/account fixtures.
Health probes are preliminary; protected API smoke uses an SDK/encrypted app
client. Billing/flags are external HTTP boundaries; link and configure their
local APIs when the task needs those interactions, keeping server credentials
on the backend.

## Implement and validate

Keep authorization, cryptography, persistence, model/provider policy and usage
in OpenSecret; clients own UI and device effects. Read only the relevant
[implementation contracts](../../../services/opensecret/docs/development-contracts.md)
and load `$change-opensecret-api`, `$change-opensecret-provider`, or
`$review-opensecret-security` for the boundary being changed.

Use `$validate-opensecret` before handoff for focused then complete component
gates, disposable DB tests, and changed encrypted-client behavior. Report
commands/counts, selected services/account, and unverified boundaries.
Routine development does not authorize shared migrations, signing/PCR changes,
remote enclave lifecycle, deployment, or publication. Preserve the
[manual PCR contract](../../../services/opensecret/docs/pcr-compatibility.md).
