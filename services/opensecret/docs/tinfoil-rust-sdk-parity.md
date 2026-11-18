# Tinfoil Rust SDK boundary

OpenSecret sends Tinfoil requests through the in-process, attested `tinfoil-rs`
client. The public OpenSecret API remains an encrypted client-to-OpenSecret
protocol; this document defines only the provider boundary.

## Compatibility boundary

Provider parity concerns decrypted HTTP semantics rather than randomized TLS
records or implementation-specific framing:

- method, path, query, and exact request body;
- safe end-to-end headers with provider authorization supplied by OpenSecret;
- upstream status, application content type, and response schema;
- ordered SSE payloads, numeric usage, and one terminal `[DONE]`.

Header casing/order, connection reuse, TLS bytes, live enclave metadata,
generated text, and chunk boundaries are not stable compatibility evidence.

## Automated evidence

`provider_client::tests::tinfoil_request_matches_contract_at_the_network_boundary`
uses a local listener to verify request method, path/query, header policy, and
body bytes.

`provider_client::tests::live_tinfoil_models_and_completions_match_the_legacy_api_contract`
is ignored by default because it requires an approved credential and network
egress. It exercises live attestation, model discovery, non-streaming response,
streaming SSE, terminal framing, and numeric usage. Run it only through the
Tier 3 procedure in
`.agents/skills/validate-opensecret/SKILL.md`.

`provider_client::tests::live_maple_guests_attest_and_complete_through_the_direct_transport`
is ignored for the same reason and needs the dedicated Maple inference
credential. It proves that each dedicated guest verifies against its compiled
release policy and serves models, one completion and one stream through the
production transport.

If raw live evidence is needed, keep it outside the repository, use a
non-secret credential label, and redact authorization and attestation metadata.
The live test proves that provider and moment only; it does not prove
OpenSecret auth, encrypted public transport, persistence, Maple, another
provider, EIF identity, or deployment.

## Trust and lifecycle

Preserve Tinfoil discovery, attestation, origin-bound TLS, bounded recovery,
connection reuse, and the no-downgrade rule. The SDK verifies V3 attestation
documents offline and enforces a freshness deadline on every verification;
OpenSecret renews a verification ahead of that deadline without a request in
hand, and replaces a client that still expires. The SDK pins a dedicated guest
to one repository; `src/maple_inference.rs` narrows each verification to the
approved tag, digest and launch measurement. Retry only failures known to occur
before request acceptance; do not replay ambiguous completion requests or
retain request bodies in shared recovery work.

When changing enclave technology or SDK revision, verify that the pinned SDK
supports the selected verifier and that deployment configuration selects the
same technology. Treat source, local provider tests, EIF/PCR evidence, and live
deployment evidence as separate claims.
