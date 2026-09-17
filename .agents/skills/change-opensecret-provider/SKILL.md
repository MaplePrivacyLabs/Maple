---
name: change-opensecret-provider
description: Change or review OpenSecret inference and web-provider integrations. Use for model catalog entries or aliases, provider routing, Tinfoil or standard-provider transport, credentials, attestation, request or response canonicalization, retries, headers, cache namespaces, usage accounting, audio or embeddings, web search, or extraction.
---

# Change an OpenSecret provider

Keep providers behind the backend boundary. Clients select public models and
capabilities; OpenSecret owns provider selection, credentials, transport,
provider-model translation, request policy, response normalization, and usage
attribution.

Read the monorepo-root `AGENTS.md` and `services/opensecret/AGENTS.md`. Source
and documentation paths below are relative to `services/opensecret/`; run
backend commands there through its pinned Nix shell.

## Trace the live provider path

Derive the current graph from source instead of copying provider or model
inventories:

- `src/model_config.rs`: canonical public IDs, aliases, capabilities, limits,
  visibility, and access.
- `src/provider_registry.rs` and `src/inference_planning.rs`: V2 route topology,
  deterministic planning, and eligible same-model providers.
- `src/provider_routing.rs` and `src/os_flags.rs`: V1/V2 dispatch, V1 provider
  preferences, and integration with V2 health-aware selection.
- `src/inference.rs` and `src/inference/health.rs`: inference intent and IDs,
  typed outcomes, capacity/circuit state, snapshots, and probe leases.
- `src/proxy_config.rs`: provider endpoints and credentials.
- `src/provider_client.rs`: standard and attested transport, headers,
  streaming, and retry decisions.
- `src/web/openai.rs` and `src/web/responses/`: route-specific request
  rewriting, shared preparation/execution, pinning, canonical response
  projection, tools, and usage. `responses/image_describer.rs` owns the image
  model candidates; its callers use the shared executor.
- `src/web/web_routes.rs` and `src/kagi.rs`: public web contracts and the
  Kagi search/extract adapter.

Trace the changed public model or capability through every affected layer and
its tests. Historical parity notes can explain intent, but current source and
pinned consumers define the contract.

## Preserve the public contract

- Configure public model behavior centrally. Do not add an ID only in a route
  response or selector.
- Keep public IDs provider-neutral and pinned. Resolve aliases before routing,
  translate immediately before send, and canonicalize provider IDs in both
  streaming and non-streaming responses.
- Route from authenticated identity and backend policy. Never accept a
  caller-supplied provider, upstream model ID, routing flag, or another user's
  cache namespace.
- Keep model capability, access, routing, endpoint/credential resolution,
  transport, and route orchestration in their owning layers.
- Treat current-turn reasoning and replay of prior reasoning as separate model
  capabilities. Prove provider-specific behavior before encoding it as policy.

When billing or feature flags affect the path, treat them only as configured
external HTTP APIs. Keep their credentials backend-only and test the changed
call site's unavailable, timeout, denial, fallback, and success semantics.

## Preserve routing context

Router V1 evaluates provider preferences; Router V2 does not consume those V1
preferences. Keep image and title helpers on the request's appropriate routing
context. A helper candidate identifies a public model; resolve its provider
through shared routing rather than pinning an independently guessed provider.

Trace alias resolution separately from provider selection. Current V2 failover
stays within the resolved public model; an Auto alias alone does not implement
cross-model failover. Do not infer inactive behavior from historical `Shadow`
type names: active V2 selection consumes health snapshots.

Preserve first-send claim handling and later-turn pinning. A claim lost before
the first send may select another same-model provider; later Responses tool
turns remain pinned and may fail locally. Neither permits replaying an upstream
attempt with an ambiguous outcome. Rebuild provider-specific request fields
when a permitted pre-send selection changes the route.

Validate the affected combinations of V1/V2, explicit/Auto model, Responses/
Chat Completions, and main/title/image execution. Keep live rollout percentages,
account allocations, and operator procedures out of this public skill.

## Preserve transport and retry safety

Tinfoil and ordinary OpenAI-compatible providers are distinct trust boundaries.
Preserve Tinfoil discovery, attestation, origin-bound TLS, bounded recovery,
and the no-downgrade rule. A new standard provider needs an explicit endpoint,
authentication, confidentiality, and error policy; an OpenAI-shaped API alone
does not establish those properties.

Classify custom provider bases and credential forwarding from parsed current
configuration, not string intuition. Cover exact approved hosts and adversarial
hostnames when changing URL or credential behavior.

Retry only when the transport proves request bytes were not accepted. Do not
replay an ambiguous completion POST after a timeout, response error, provider
status, or partial stream without an explicit idempotency and accounting
design. A refresh task must not retain a request body or inherit one caller's
cancellation accidentally.

## Own outbound data

- Replace inbound authorization with the configured provider credential and
  keep secrets out of URLs, public responses, client configuration, evidence,
  and logs.
- Review forwarded headers and `Connection`-named headers explicitly. The
  backend owns host, framing, content type, credentials, and provider-managed
  request fields.
- Derive cache namespaces from authenticated backend identity. Strip or
  replace caller-controlled provider cache fields according to the selected
  provider policy.
- Log only bounded, allowlisted metadata. Sanitize upstream errors before
  returning them publicly.

Add boundary tests for every changed header, credential, URL, cache, or request
rewrite rule.

## Preserve streams and usage

Parse complete frames, support the established line endings, preserve ordered
JSON chunks, canonicalize model IDs, propagate cancellation, and emit exactly
one terminal condition. Distinguish finish evidence from final usage frames.

Normalize usage once and retain the actual provider, canonical public model,
and established JWT/API-key attribution. Do not publish successful usage for a
partial or unterminated response unless the owning contract explicitly defines
that outcome. Review Responses multi-turn aggregation so a tool loop does not
duplicate usage.

Audio, transcription, and embeddings have independent payload, provider,
limit, and retry policies in `src/web/openai.rs` and
`src/web/audio_utils.rs`; do not generalize chat behavior to them.

## Keep web content untrusted

Keep search and extraction separate. Normalize and authorize public HTTPS URLs
under the current provenance and SSRF policy, and bound all provider results
before placing them in model context. Do not grant URL authority from assistant
text, snippets, diagnostics, or extracted page content. When continuation can
resume tool history, reconstruct authority only from visible persisted inputs
and trusted structured provider output.

## Validate the changed layers

Run focused model, routing, transport, route, usage, or web-safety tests while
iterating, then load `$validate-opensecret`. Live provider probes require
explicit credential, egress, and cost authorization; keep raw evidence outside
the repository and redact it.

Use `docs/tinfoil-rust-sdk-parity.md` for the named Tinfoil live boundary. A
provider-direct probe, encrypted OpenSecret SDK smoke, and Maple smoke prove
different layers; run and report each layer that the change claims.
