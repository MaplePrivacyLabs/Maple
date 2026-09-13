# OpenSecret agent guide

This file applies to `services/opensecret/` within the Maple monorepo, alongside
the [root guide](../../AGENTS.md). It contains durable project rules; task
procedures live in the root [`.agents/skills/`](../../.agents/skills/). Load the
matching skill before doing specialized work. Paths below are relative to this
component unless explicitly labeled as monorepo-root paths.

## Work safely

1. Inspect the checkout, branch, remotes, submodules, and worktree before
   editing. Preserve unrelated changes. For new work, prefer current
   `origin/master` unless the task names another base.
2. Inspect submodule status during review. Initialize dependencies with
   `git submodule update --init --recursive -- services/opensecret/nitro-toolkit services/opensecret/privatemode-public`
   from the monorepo root when building, testing, or working in their contents.
   Use the component's pinned Nix toolchain; do not install substitute
   system toolchains merely to bypass the repository environment.
3. Remember that `nix develop` has stateful PostgreSQL, `.env`, and Linux
   container hooks. Use `docs/dev-shell.md` for controls and give concurrent
   checkouts distinct state and ports.
4. Run `just diesel-migration-run-local` before the backend. Startup does not
   run Diesel schema migrations; `src/migrations.rs` is separate
   application-data migration logic.
5. Keep credentials and generated local state in ignored files or protected
   environment variables. Never commit `.env`, `.pgdata/`, `.local/`, provider
   captures, decrypted traffic, or secrets.

This component is one Rust package and binary, not a Cargo workspace. Run its
Cargo, Nix, and `just` commands from `services/opensecret/`, using its own
`flake.nix`, `flake.lock`, and `rust-toolchain.toml`.

## Ownership

- `src/main.rs`: configuration, shared state, middleware, and router assembly.
- `src/web/`: HTTP boundaries, authentication context, encryption middleware,
  errors, streaming, route orchestration, and Responses/conversation behavior.
- `src/models/`, `src/db.rs`, `migrations/`, and `src/models/schema.rs`:
  persistence and schema evolution.
- `src/encrypt.rs`, `src/seed_wrapping.rs`, `src/jwt.rs`, attestation/session
  code, and `src/security_invariants.rs`: cryptographic and identity boundaries.
- `src/model_config.rs`: public model catalog and capabilities.
- `src/provider_routing.rs`: provider selection and upstream model mapping.
- `src/proxy_config.rs` and `src/provider_client.rs`: provider endpoints,
  credentials, transport, attestation, streaming, and safe retry decisions.
- `src/kagi.rs` and `src/web/web_routes.rs`: Kagi web adapter and
  provider-neutral public web routes.

Keep server-controlled authentication, authorization, encryption, persistence,
provider credentials and routing, entitlement decisions, and usage accounting
in OpenSecret. Keep presentation, device integration, and local interaction in
clients such as Maple. Published OpenSecret SDKs own attestation and encrypted
transport; protected routes are not ordinary plaintext `fetch`, `curl`, or
`reqwest` APIs.

## Durable API and security rules

- Derive route authentication and middleware order from current router
  assembly. An encryption session establishes a protected transport, not user
  identity or authorization. Bodyless protected routes still require a live
  session.
- OpenAI-shaped routes describe decrypted payloads inside the OpenSecret
  protocol. Exercise protected routes through a pinned OpenSecret SDK or Maple.
- JWT and API-key contexts are different. Do not give an API-key path access to
  user-private storage without an explicit key-ownership and authorization
  design.
- Validate all client-controlled input before writes or provider side effects.
  Preserve method, status, content type, error shape, streaming order,
  cancellation, usage, encryption, and one terminal condition when changing a
  public contract.
- Return stable, sanitized errors. Do not expose provider bodies, credentials,
  decrypted content, SQL details, or cryptographic internals.
- Treat released SDKs and Maple as protocol consumers. Review both old-client /
  new-server and new-client / old-server behavior when they can update
  independently; gate or stage incompatible changes.

## Durable provider rules

- Keep canonical public model IDs separate from provider IDs, routing policy,
  feature flags, and credentials. Translate at the provider boundary and
  canonicalize client-visible responses.
- Route from authenticated identity and backend policy, never a caller-supplied
  provider or account identity.
- Review forwarded headers and provider-managed fields explicitly. Provider
  credentials, raw attestation material, and user cache namespaces must not
  cross into client responses or logs.
- Retry only when the failure is known to precede request acceptance. An
  ambiguous POST, response failure, or partial stream is not generally safe to
  replay.
- Treat provider responses, model output, web results, and extracted pages as
  untrusted. Bound data and loops, preserve URL provenance, and enforce the
  current SSRF policy.
- Keep usage tied to the actual provider and canonical public model while
  preserving the established user or API-key attribution.

## Persistence and migrations

- Add a new timestamped Diesel migration; do not rewrite deployed history.
  Review `up.sql`, `down.sql`, generated schema, model/query changes, scoping,
  and indexes together.
- Enforce ownership in database queries. Filtering an unscoped result in a
  handler is not authorization.
- Identify the owning key before changing encrypted data. User-private content
  uses credential-derived user keys; server-owned secrets use their designated
  enclave/system key domain.
- Version ciphertext formats. User-key data normally needs dual-read/new-write
  plus lazy authenticated rewrite after the user's key is available. SQL or
  startup code cannot safely re-encrypt opaque user data without that key.
- Run migration and database-backed security tests only against an identified,
  disposable, fully migrated database.

## Privacy and evidence

- Do not log secrets, tokens, session material, raw headers, OAuth payloads,
  prompts, reasoning, decrypted bodies, response deltas, provider bodies, or
  other sensitive user content. Safe metadata must be bounded and allowlisted;
  `trace` is not a private channel.
- Preserve capacity, expiry, one-use/lease, cleanup, cancellation, and failure
  behavior at unauthenticated, cryptographic, streaming, and external-service
  boundaries.
- Treat billing and feature flags only as configurable external HTTP APIs.
  Their server credentials remain backend-only, and each changed call site
  must define its own unavailable, timeout, denial, and success behavior.
- Separate source-confirmed, test-confirmed, build-confirmed, live-confirmed,
  inferred, and unverified claims. Source and local tests do not prove deployed
  PCRs, KMS/IAM policy, artifact identity, network placement, or log retention.

Keep revision-specific findings in the review, not in evergreen repository
guidance.

## Development and validation

Use `$develop-opensecret` for the local stack and code-placement workflow. Use
`$validate-opensecret` to choose focused tests, exact Rust CI parity,
disposable-database validation, authorized provider probes, encrypted client
smoke tests, Nix checks, and read-only EIF/PCR evidence.

Match evidence to the changed boundary. Report exact commands, counts, ignored
or skipped tests, configured external services, and every unverified layer.

## Operator authority

EIF/PCR parity is a release and deployment gate, not an ordinary development
or pull-request gate. The monorepo-root `opensecret-ci.yml` validates Rust,
Nix checks and the default backend binary, and dependency policy;
`sdk-integration.yml` exercises both in-tree SDKs against this backend.
The separate root `opensecret-eif.yml` builds dev/prod EIFs and compares their
measurements only when a PR explicitly edits one of the four approved PCR JSON
files, on relevant backend/TEE or approval changes to master, or on a manual
run. An ordinary backend PR does not require updated PCR approvals. A master
mismatch deliberately reports that the revision does not match current
approvals. GitHub Actions never signs approvals, creates EIF releases, or deploys
the service. Nix cache writes are not approval or deployment: master and
same-repository PR EIF checks have FlakeHub OIDC permission, while fork PRs and
other manual refs use the branch-scoped GitHub cache without OIDC. Follow the
[cache validation policy](docs/nitro-deploy.md#binary-caches-and-cold-run-validation).
Do not update PCR references as part of ordinary pull-request work. Treat an
EIF build failure separately from PCR mismatch.

Before publishing or deploying an authorized dev or prod EIF, build it on the
supported Linux/ARM64 release builder, intentionally review its measurements,
and complete the authorized reference, history, signing, and comparison work.
Follow [manual PCR compatibility](docs/pcr-compatibility.md) to validate and
copy the same signed files to `OpenSecretCloud/opensecret` for installed clients.
Keep its existing raw URLs and the current SDK verification key intact.

Require explicit authorization for changing PCR references or histories, KMS
or IAM policy, shared or remote migrations, copying artifacts to hosts,
starting or terminating enclaves, restarting remote services, writing remote
secrets, staging, deployment, release, or signing operations. Inspect a recipe
before running it; its name alone does not establish whether it mutates state.

## Skills

- `$develop-opensecret`: local setup, migrations, process topology, and code
  placement.
- `$change-opensecret-api`: encrypted API contracts, Responses persistence,
  streaming, and client compatibility.
- `$change-opensecret-provider`: models, routing, transport, provider adapters,
  retries, and usage.
- `$validate-opensecret`: proportional test and smoke evidence.
- `$review-opensecret-security`: trust-boundary review and evidence claims.

## Maintaining this guidance

Treat this guide and the repository skills as living operational documentation,
not infallible rules. Re-check prescriptive language against current source,
tooling, and architecture. If guidance appears stale, materially wrong,
unnecessarily absolute, or repeatedly creates development friction, surface the
mismatch and confirm the intended correction with the user before changing it.
Avoid churn for stylistic preferences or isolated nits; do narrow words such as
"always" or "never" when they claim more than the invariant requires.

Update the relevant guide or skill in the same branch when a material change
adds, removes, or rearchitects a workflow, ownership boundary, validation path,
or recurring development procedure. If unrelated drift is discovered, keep the
current task scoped and propose a standalone change with evidence and reasoning.
Add a new skill only for a genuinely reusable workflow; otherwise consolidate
existing guidance. Validate every command and path the update prescribes.
