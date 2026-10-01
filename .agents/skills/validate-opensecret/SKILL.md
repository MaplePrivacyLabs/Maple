---
name: validate-opensecret
description: Select and run backend Rust, disposable database, encrypted client, provider, Nix, and EIF/PCR evidence matching an OpenSecret change. Use before backend handoff or to assess API, provider, persistence, security, build, or deployment validation claims.
---

# Validate OpenSecret

Read root/component `AGENTS.md`, diff/source/tests, and the owning workflow.
Commands use `services/opensecret/` and its pinned Nix shell unless they
explicitly enter the monorepo root. This is one package, not a Cargo workspace.

Any backend code change and client/backend/log/billing integration requires
an isolated linked Local stack. Preserve externally generated environments,
ports, database state, accounts, and process ownership. Read the shared
[environment/login contract](../../../docs/development-environments.md).

## Select the relevant evidence

| Change | Checks |
| --- | --- |
| Documentation | Verify changed paths, commands, variables, links, and claims |
| Rust behavior/dependency | Focused tests, then exact Rust CI (Tier 1) |
| Auth/encryption/persistence/migration | Tier 1 plus disposable migrated DB/security suites (Tier 2) when state is involved |
| HTTP/middleware/SSE/Responses/client contract | Tier 1 plus encrypted client smoke (Tier 4), including affected SDK/app paths |
| Provider/model/routing/headers/usage/attestation | Tier 1 and focused tests; live provider/client proof only when that claim needs it |
| Nix/entrypoint/kernel/packaging | Affected Rust gates plus current-host flake/build evidence (Tier 5) |
| Authorized publication/deployment | Linux/ARM64 release artifact and reviewed EIF/PCR evidence (Tier 5), followed by separately authorized operations |

Load [validation procedures](references/checks.md) only for the applicable
tiers; higher-level proof supplements relevant lower-level checks. The default
Rust CI has no DB service and does not run ignored tests. Never blanket-run
`cargo test --locked -- --ignored`: it mixes disposable DB mutation and
credentialed live-provider tests. Use the guarded
[disposable DB helper](scripts/disposable_db_tests.sh) for the selected suites;
read its procedure before invoking it.

For migrations, prove empty-database upgrade and the latest down/up when
appropriate. Data conversions also need representative pre-change rows,
restart/retry/rollback evidence, and the owning key. A synthetic database pass
does not prove user-key data conversion or live OAuth providers.

## Preserve boundaries

- Pure checks disable stateful shell hooks; migrations/test DBs target only
  identified disposable local state. Keep secrets and user content out of
  commands/logs/fixtures/tracked files.
- Health is liveness only. Protected API proof uses the SDK/encrypted client
  with route-appropriate auth, not plaintext HTTP. Local baseline accounts
  use supported encrypted password/fixture paths.
- Inspect each consumer manifest/lock for the actual SDK source/version. The
  root in-tree SDK integration tests both SDKs against this backend; it does
  not prove an application, published SDK, or live provider.
- Live provider probes require explicit scope for credentials, network/cost,
  and named provider; default tests do not establish live availability.
- Ordinary PRs do not require new PCR approvals. Distinguish EIF build failure
  from measurement mismatch. Never copy/sign approvals to clear a check.
  Read [PCR compatibility](../../../services/opensecret/docs/pcr-compatibility.md)
  and [cache boundaries](../../../services/opensecret/docs/nitro-deploy.md#binary-caches-and-cold-run-validation)
  when those inputs change; local cache hits cannot prove fresh hosted caches.
- Signing, PCR/history changes, KMS/IAM, shared/remote migrations, artifact
  transfer, enclave lifecycle, secrets writes, staging, and deployment require
  explicit authority. CI artifact/signing evidence is not deployment proof.

## Handoff

Report commit/dirty state, host, exact commands/results and pass/ignored/skip
counts, disposable DB lifecycle, selected API/account/integrations, runtime
scenarios, and unverified boundaries. Label unit/static, disposable DB,
encrypted full stack, provider, Linux/Nitro/PCR, and deployed evidence
separately. Failed/skipped/interrupted/partial checks remain exactly that.
