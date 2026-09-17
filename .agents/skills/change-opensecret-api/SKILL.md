---
name: change-opensecret-api
description: Change or review OpenSecret HTTP contracts and their SDK or Maple consumers. Use for routes, authentication context, attested encrypted requests or responses, OpenAI-shaped payloads, Responses or conversation persistence, errors, SSE streaming, cancellation, or cross-client compatibility.
---

# Change the OpenSecret API

Treat OpenSecret, released SDKs, and affected Maple paths as one versioned
protocol. Preserve existing public behavior unless the task deliberately
changes it.

Read the monorepo-root `AGENTS.md` and `services/opensecret/AGENTS.md`. Backend
source paths below are relative to `services/opensecret/`; run backend commands
there through its pinned Nix shell. Consumer paths in the coordination section
are relative to the monorepo root.

Normative rules in this skill govern new or changed code; they do not certify
untouched paths. Re-confirm current behavior from source and tests. If an
unrelated path conflicts with a rule, keep that observation task-local and do
not broaden the change without user approval.

## Trace the live contract

Derive the route from current router assembly in `src/main.rs` and its module in
`src/web/`; do not maintain a copied endpoint table. Trace one request through:

1. method, path, middleware order, and established auth context;
2. decryption and typed or extensible request validation;
3. authorization, storage, provider, and usage side effects;
4. status, headers, body or SSE projection, and error mapping;
5. focused tests, released SDK support, and pinned Maple consumers.

Use router assembly as the authority for outer authentication and the route
module as the authority for inner session/decryption middleware. Use
`src/web/openai.rs` for OpenAI-shaped inference routes and
`src/web/responses/` for Responses, conversations, tools, persistence, and
events.

For Transport V2, also trace `src/transport_v2/gateway.rs` and
`src/transport_v2/session.rs` from the mounted routes in `src/main.rs`.
V1 and V2 transport coexist; establish which path the actual SDK invokes.
Transport V2 and Router V2 are separate mechanisms, so a transport version
does not establish the selected inference router.

## Preserve transport and identity boundaries

- OpenAI-shaped describes the decrypted payload, not a plaintext wire API.
  Protected routes require the OpenSecret attestation/session protocol and a
  route-appropriate auth context.
- A session protects transport; it does not establish user identity, project
  membership, or storage-key ownership. Bodyless protected routes still
  validate and touch the session.
- JWT and API-key contexts are distinct. Preserve the auth method through
  authorization, persistence eligibility, quota, and usage attribution.
- Hold the session lease for the complete response body or stream. Successful
  protected responses and ordinary stream events remain encrypted according to
  the established client protocol.
- Validate provider-free input before writes. Pinned clients may recover from
  selected session or auth failures by retrying, so a side-effecting change
  needs explicit idempotency or proof that validation precedes the effect.

Inspect `src/web/attestation_routes.rs`,
`src/web/encryption_middleware.rs`, session state in `src/main.rs`,
`src/web/openai_auth.rs`, and `src/jwt.rs` when the change reaches those
boundaries. Do not duplicate their policy inside a handler.

Derive errors from the response type actually returned by the route. Handlers
returning `ApiError` use its mapping in `src/main.rs`; route-local error types
may intentionally differ. For a new or intentionally revised contract, use
stable HTTP semantics and sanitized public bodies. Once a changed stream has
started, use its typed encrypted error event rather than introducing an
unauthenticated plaintext data frame.

## Preserve stateful Responses behavior

Read `src/web/responses/handlers.rs`, `constants.rs`, `events.rs`, and
`context_builder.rs` together with the response/conversation models and
schema. Derive supported fields and event names from those files rather than
from upstream API documentation.

Preserve these ordering rules unless the contract change explicitly replaces
them:

- authenticate, authorize ownership, validate payload/model limits, and build
  context before durable writes;
- check ownership before decrypting or mutating user content;
- keep user content in its established user-key encryption domain;
- persist assistant, reasoning, tool, and output items in emitted order;
- keep event names, decrypted `type`, sequence policy, terminal status,
  cancellation, and durable item ordering consistent.

Do not equate a dropped client stream with explicit cancellation. Define and
test the disconnect points affected by the change, and claim background
continuation only after source and tests establish independent task ownership
at those points.

## Coordinate pinned consumers

Use the same Maple checkout: SDK source lives under `sdk/`, Research consumers
under `apps/maple-research/frontend/`, and the GPUI prototype under
`apps/maple-agent/`. Search from the monorepo root without hiding errors and
follow each component's guide and applicable skills. If an affected consumer
cannot be exercised, report compatibility as unverified rather than falling
back to the retired standalone SDK repository or claiming there are no consumers.

Maple's browser Research path uses Responses/Conversations through the
TypeScript client, while native Agent Mode uses chat completions through the
Rust client. A semantic change intended for both is two protocol integrations,
not one shared wire-field edit. Trace request construction, provider handoff,
persistence, and usage in each affected path.

Use the SDK source and application dependency resolutions recorded by the
selected Maple revision. The frontend's `package.json` and `bun.lock`, and each
Rust consumer's Cargo manifest and lockfile, select a published SDK or local
source. Follow the [SDK consumer version policy](../../../docs/sdk-publishing.md#consumer-version-policy).
Validate SDK source changes separately from consumers pinned to a published
version; update only the consumers intended to adopt the changed contract.
Update SDK types, custom-fetch adaptation, native
transport allowlists, call sites, mocks, and fixtures only where the contract
reaches them. Test old-client/new-server and new-client/old-server behavior.
Prefer server-first rollout for compatible additions; use an explicit
capability/version gate when either direction cannot interoperate.

## Validate the changed boundary

Run focused owning-module tests while iterating, then load
`$validate-opensecret` for the complete gate. Exercise protected contracts
through a pinned encrypted client, not plaintext `curl`, and cover each auth
mode the change affects. Include the applicable Maple browser or native path
for a client-facing change.

Label evidence precisely: unit contract, encrypted SDK call, provider-backed
stream, Maple browser, Maple native, or deployed environment. One layer does
not prove the others.
