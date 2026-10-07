# OpenSecret implementation contracts

Read the relevant section for API, provider, persistence, or security work.
Source paths are relative to `services/opensecret/`. Follow the
[component guide](../AGENTS.md) and the matching root skill for commands.

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
- Reasoning effort is one per-model table (`model_config::ModelReasoning`) that
  feeds the catalog `reasoning` object, request validation and tests. Clients
  choose a level only through OpenAI's `reasoning_effort` (Chat Completions) and
  `reasoning.effort` (Responses); template switches are not a client contract.
  Validate after alias resolution against the model that runs: reject an
  unsupported value on an explicit model with OpenAI's `unsupported_value`
  error, move it to the nearest accepted effort on an `auto:` alias, and never
  forward a thinking-off control to a model whose reasoning is mandatory.
  Re-verify the table live whenever a provider changes its engine build.

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
