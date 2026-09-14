# OpenSecret Transport V2

This document is the canonical wire contract for Transport V2. It describes
the protocol implemented by `src/transport_v2/` and the V2-specific
authentication paths. Older design notes are useful research, but are not
normative when they disagree with this document or the source.

Transport V2 is deliberately small:

- one attested X25519 request/response establishes an identity-neutral session;
- every logical request is one whole-request ChaCha20-Poly1305 envelope;
- every request carries its own credential, when one is required;
- a random request ID provides unordered, exact replay detection;
- responses are ordered authenticated records with explicit finality; and
- V1 remains a separate protocol with unchanged wire behavior.

It does **not** use a principal-bound session, EHBP, DPoP, Binary HTTP,
monotonic request counters, per-field signatures, or a custom memory scheduler.

## Security boundary

The TLS terminator, forwarding host, reverse proxy, and load balancer are not
trusted with logical request or response contents. After session
establishment, they can observe the V2 endpoint, session identifier, ciphertext
lengths, timing, and connection behavior, but not the logical method, target,
headers, credential, cache root, body, status, or response bytes.

The client trusts session keys only after it has:

1. authenticated the AWS Nitro attestation document, including its certificate
   chain, COSE signature, validity time, and freshness challenge;
2. applied its configured approved-PCR policy;
3. checked all three transcript bindings in the document; and
4. derived the same session identifier returned by the enclave.

The approved-PCR policy is a client release policy, not a value supplied by the
server or defined by this wire format. Production clients fail closed if it is
not satisfied. An exact loopback development configuration may use the mock
attestation path.

An encryption session proves possession of transport keys. It never supplies a
user, platform-user, project, or API-key identity. Application authentication
and authorization still run for every logical request.

## Byte conventions

Unless specified otherwise:

- integers are unsigned and big-endian;
- byte concatenation is written `||`;
- `0` in a derivation is one zero byte;
- Base64 means canonical padded RFC 4648 standard Base64;
- session and request IDs render as exactly 32 lowercase hexadecimal
  characters; and
- AEAD means ChaCha20-Poly1305 with its 16-byte authentication tag appended to
  the ciphertext.

Protocol labels are ASCII bytes exactly as printed below. They are not
NUL-terminated except for the explicit separator byte.

## 1. Establish an attested session

The client generates a fresh 32-byte random challenge and ephemeral X25519 key
pair, then sends:

```http
POST /v2/session
Content-Type: application/json
X-OpenSecret-Routing-Key: <canonical padded Base64 challenge>

{
  "version": 2,
  "challenge": "<canonical Base64 of 32 bytes>",
  "client_public_key": "<canonical Base64 of 32 bytes>"
}
```

`Content-Type` must be exactly `application/json` without parameters. The JSON
object rejects unknown or duplicate fields. The outer request has no query
string, authorization, proxy authorization, cookie, or content encoding. Its
actual body is limited to 4 KiB. `Content-Length` is optional; when present, it
is only an early size check, and the actual bytes read remain authoritative.

The enclave generates its own ephemeral X25519 key pair and returns plaintext
JSON:

```json
{
  "version": 2,
  "session_id": "<32 lowercase hex characters>",
  "attestation_document": "<Base64 Nitro attestation document>",
  "expires_in_seconds": 3600
}
```

The Nitro document must contain these exact bindings:

| Nitro field | Required value |
| --- | --- |
| `nonce` | the client's 32-byte challenge |
| `public_key` | the enclave's 32-byte ephemeral X25519 public key |
| `user_data` | `"opensecret/transport-v2/session/v1/client-public-key" || 0 || client_public_key` |

The client authenticates and approves the document **before** using the server
public key. It then computes X25519 with its ephemeral private key and rejects
the all-zero shared secret.

### Session key schedule

Let:

```text
transcript_hash = SHA-256(
    "opensecret/transport-v2/session/v1" || 0 ||
    challenge || client_public_key || server_public_key
)

PRK = HKDF-Extract-SHA256(
    salt = challenge,
    IKM  = X25519_shared_secret
)
```

Derive three values with HKDF-Expand-SHA256:

```text
request_key  = Expand(PRK,
    "opensecret/transport-v2/request-key/v1" || 0 || transcript_hash, 32)

response_key = Expand(PRK,
    "opensecret/transport-v2/response-key/v1" || 0 || transcript_hash, 32)

session_id   = Expand(PRK,
    "opensecret/transport-v2/session-id/v1" || 0 || transcript_hash, 16)
```

The client compares lowercase-hex `session_id` with the response and rejects a
mismatch. It then erases the ephemeral private key, shared secret, transcript
scratch values, and any unneeded derived-key copies.

The server session has a fixed 3,600-second lifetime from creation. Activity
does not extend it. Clients should retire it slightly early to account for
establishment time and clock or network skew.

`X-OpenSecret-Routing-Key` is the exact canonical Base64 encoding of the
32-byte client challenge. It is public, random per session, and carries no
credential, identity, or authority. The enclave requires it to match the
challenge during session creation and the stored session on later requests.
The challenge is already authenticated by Nitro attestation and included in
the session key transcript, so no separate routing-key cryptography exists.
Its purpose is to let a stateful load balancer select the same enclave for the
initial handshake and every request that follows when using HTTP-header
affinity. The wire header remains required with a single origin or DNS-only
routing, even when that routing layer does not inspect HTTP headers.

## 2. Send a whole logical request

All application operations use one outer endpoint:

```http
POST /v2/request
Content-Type: application/octet-stream
X-Session-Id: <32 lowercase hex session ID>
X-OpenSecret-Routing-Key: <canonical padded Base64 session routing key>

<request record bytes>
```

`Content-Type` must be exactly `application/octet-stream` without parameters,
and there must be exactly one `X-Session-Id` and one
`X-OpenSecret-Routing-Key`. The outer URI has no query string.
Credentials, cookies, and content encoding are forbidden outside the
ciphertext. Transfer framing such as an absent `Content-Length` or HTTP
chunking is valid; the gateway always enforces the actual-body limit while
reading.

The client chooses a fresh, cryptographically random 16-byte `request_id` for
every logical request. Requests may arrive in any order.

### Request plaintext

The AEAD plaintext is:

```text
metadata_length_u32 || metadata_json || raw_body
```

`metadata_json` is UTF-8 JSON with exactly these fields:

```json
{
  "version": 2,
  "credential": null,
  "cache_namespace_root": null,
  "method": "GET",
  "target": "/v1/models?example=1",
  "headers": [
    { "name": "accept", "value": "application/json" }
  ],
  "body_present": false
}
```

Object field order is immaterial, but unknown and duplicate fields are
rejected. Canonical writers emit every field shown. The current decoder also
treats an omitted `credential` or `cache_namespace_root` as `null`; clients
must not rely on omitting any other field. Nested credential and header objects
also reject unknown or duplicate fields. `body_present: false` requires an
empty raw tail and means no logical body. `body_present: true` distinguishes a
present empty body from an absent body.

The method is an HTTP method token. The target is an origin-relative path with
an optional query, starts with one `/`, has no scheme or authority, and contains
neither a fragment nor a backslash. Logical header names are canonical
lowercase HTTP names. Repeated headers are allowed and preserve their order.

The following headers are controlled by the gateway and cannot appear as
logical headers:

```text
authorization  proxy-authorization  cookie  set-cookie  host
content-length transfer-encoding
connection     keep-alive           te      trailer      upgrade
forwarded      via                  x-forwarded-for
x-forwarded-host x-forwarded-proto  x-opensecret-routing-key
x-session-id
```

`content-encoding` is also rejected on the outer request. It is not in the
logical gateway-controlled list.

### Credentials and anonymous-to-authenticated use

`credential` is either `null` or an exact object:

```json
{ "kind": "bearer", "value": "<V2 access token>" }
```

The supported kinds are:

- `bearer`: a V2 user or platform access token, as required by the route;
- `api_key`: an inference-scoped OpenSecret API key; and
- `resumption`: a V2 refresh token, accepted by the user or platform refresh
  route instead of a refresh token in the logical body.

The value is 1 to 16 KiB of visible non-space ASCII. A request that needs no
identity, including registration, login, and session establishment, uses
`null`. A successful login or registration returns V2 access and refresh tokens
inside the encrypted response; subsequent authenticated requests carry the
appropriate token inside their own envelope. The transport session itself does
not change state during this transition.

V2 user and platform access/refresh tokens have dedicated audiences that V1
validators do not accept. Third-party JWT issuance remains an authenticated
application operation; those tokens are outputs for external services, not V2
transport credentials.

### Provider cache root

`cache_namespace_root` is either `null` or canonical padded Base64 of exactly
32 client-random bytes. V2 chat-completion and Responses creation/inference
requests require a root; stored-response control and read operations do not. A
client keeps it stable across app restarts when it wants stable provider cache
hits, but never sends it outside the encrypted envelope.

After authenticating the request, the enclave derives:

```text
HMAC-SHA256(
    key = cache_namespace_root,
    data = "opensecret/provider-cache/tinfoil/user-cache-namespace/v1" || 0 ||
           verified_user_uuid_bytes
)
```

Only the hexadecimal derived value is supplied to Tinfoil as
`user_cache_secret`. Continuum uses the same construction with the distinct
label `opensecret/provider-cache/continuum/user-cache-namespace/v1` and receives
its hexadecimal derived value as `cache_salt`. Neither provider receives the
other provider's namespace or the raw root. The raw root is not persisted by
OpenSecret.

Caller versions of both provider-managed fields are removed before injecting
the selected provider's value. The shared completion path applies this policy
to chat, Responses model/tool turns, and server-selected image descriptions.
The same root and verified user retain the same provider namespace across
requests; changing either isolates the cache. Different devices with different
roots do not share cache entries, even for the same user.

The Continuum proxy runs without `--sharedPromptCache` or `--promptCacheSalt`.
Explicit request salts still enable prefix-cache reuse; a request without a
salt gets the proxy's fresh random salt instead of a shared fallback. Salting
partitions cached KV prefixes, not complete generated responses, and does not
guarantee cache residency or retention.

Legacy V1 clients do not supply a cache root. OpenSecret derives their Continuum
salts with the Continuum label and verified user UUID, keyed by a random
32-byte secret held only in the backend process. Those salts remain stable
within that process; restart or a different backend replica cold-starts the
namespace. No database migration or additional configured secret is required.
Legacy Tinfoil's `SHA-256(user UUID)` behavior is unchanged.

### Request record encryption

Treat the 32-byte directional `request_key` as an HKDF PRK and derive:

```text
request_subkey = HKDF-Expand-SHA256(
    request_key,
    "opensecret/transport-v2/request-subkey/v1" || 0 ||
    session_id || request_id,
    32
)

request_aad =
    "opensecret/transport-v2/request-record/v1" || 0 ||
    session_id || request_id
```

Encrypt the request plaintext with ChaCha20-Poly1305 using
`request_subkey`, a 12-byte all-zero nonce, and `request_aad`. The wire record
is:

```text
request_id || ciphertext_and_tag
```

The fixed nonce is safe only because every random request ID derives a distinct
subkey. A client must never use the same request ID with different plaintext;
canonical clients do not intentionally reuse an ID at all.

### Replay admission

After successful AEAD authentication and before application dispatch, the
enclave atomically inserts `request_id` into that session's exact `HashSet`.
There is no ordering assumption or highest-sequence window. Concurrent copies
of one record have exactly one winner.

A duplicate ID or exhausted per-session set is rejected before application
side effects. Process-wide replay pressure returns a generic, non-latching
capacity failure without permanently poisoning unrelated sessions. It does not
make a resend safe. Replay entries disappear with the in-memory session; after
an enclave restart the old session keys are also gone, so old ciphertext is
not usable in a new session.

## 3. Receive the logical response

After a request is authenticated and replay-admitted, its response writer is
derived directly from that same session and request ID. It is not selected by a
second caller-controlled lookup.

A successfully constructed admitted response uses:

```http
HTTP/1.1 200 OK
Content-Type: application/octet-stream
Cache-Control: no-store
X-Accel-Buffering: no

<framed encrypted records>
```

Each outer frame is:

```text
ciphertext_length_u32 || ciphertext_and_tag
```

HTTP transport chunks are not record boundaries. A client parser must buffer
across a fragmented length prefix or ciphertext and must also accept multiple
complete frames coalesced into one HTTP chunk. It must enforce the record-size
limit before allocating the declared ciphertext and reject EOF in a partial
prefix or frame.

The sequence number is implicit. It starts at zero and increments once for
every encrypted record; it is not carried separately on the wire.

Treat the 32-byte directional `response_key` as an HKDF PRK and derive one
subkey for the request:

```text
response_subkey = HKDF-Expand-SHA256(
    response_key,
    "opensecret/transport-v2/response-subkey/v1" || 0 ||
    session_id || request_id,
    32
)

response_nonce = 0x00000000 || sequence_u64

response_aad =
    "opensecret/transport-v2/response-record/v1" || 0 ||
    session_id || request_id || sequence_u64
```

Each plaintext response record begins with a one-byte tag:

| Tag | Name | Remaining plaintext |
| --- | --- | --- |
| `0x01` | Start | strict JSON `{"status": <200..599>, "headers": [...]}` |
| `0x02` | Chunk | raw logical response bytes, at most 64 KiB |
| `0x03` | End | empty |
| `0x04` | Error | strict JSON `{"code": "lowercase_machine_code"}` |

The required grammar is:

```text
Start, zero or more Chunk records, exactly one of End or Error, then EOF
```

Every response, including a unary JSON or binary response, uses this grammar.
SSE remains ordinary SSE bytes inside Chunk records; clients decrypt
incrementally and expose the reconstructed logical stream to the application.
An application body or stream-production failure after Start becomes an
authenticated transport Error record. An ordinary application failure—such as
an expired access credential—is instead a logical HTTP status, headers, and
body beginning in the authenticated Start record. In particular, clients
recognize `access_token_expired` only from the decrypted
`x-opensecret-error-contract` and `x-opensecret-error-code` logical headers;
matching outer headers cannot trigger access-token refresh. EOF without a
terminal record, a second Start, a record after the terminal, a sequence or
AEAD failure, an invalid frame length, a redirect, a non-200 outer status, or a
wrong outer content type is a transport failure. Clients fail closed except
for the narrowly marked session-recovery response described below; response
AEAD or framing failures never qualify for automatic recovery.

Before a request is authenticated and replay-admitted, the gateway can return a
generic plaintext outer `400`, `413`, `415`, or `503`. Once admitted, ordinary
application statuses and representable errors are carried in the encrypted
Start record. A replay rejection is deliberately an outer generic error: the
server does not encrypt a second response under a reused request subkey and
sequence. A generic outer `503` is only a capacity signal; it is not proof that
the request was safe to resend automatically.

Two pre-dispatch failures use outer HTTP `400` with the existing
`x-opensecret-error-contract: 1` header and one exact
`x-opensecret-error-code` value:

| Code | Server condition |
| --- | --- |
| `session_not_found` | The requested session is actually missing or expired |
| `request_decryption_failed` | AEAD authentication of the incoming request fails |

The gateway emits these codes only before application dispatch. A routing-key
mismatch for an existing session, record length/encoding errors, malformed
envelopes, replay rejection, capacity failures, and ordinary application errors
do not receive these recovery markers. These
outer headers are unauthenticated hints, not evidence the original request
never executed; see the retry rules below.

### Cryptographic interoperability fixture

`crypto::tests::deterministic_key_and_record_vector` fixes the following
synthetic KDF/record vector. The repeated-byte shared secret is supplied
directly for this fixture; it is not an X25519 private key.

```text
challenge         = byte 0x11 repeated 32 times
client_public_key = byte 0x22 repeated 32 times
server_public_key = byte 0x33 repeated 32 times
shared_secret     = byte 0x44 repeated 32 times

session_id  = f7258fb103137c612baab47ced4a5a02
request_key = 00f898a5f2dcd40a703f42221f2a2b842b7e97ed5a555caa362c4153a5e1c491
response_key = e4fb003c5c829f5385531eebfdbd0ee3d8430a0bd71322e9f3e41ace915c3190

request_id = byte 0x55 repeated 16 times
plaintext  = "vector plaintext"
record     = 55555555555555555555555555555555671f5c411205cb00f769e6b2705052b795e91f44516fc6165e16a152e686b209

response request_id = byte 0x66 repeated 16 times
sequence            = 0
plaintext           = "vector response"
ciphertext          = 25a2d5ed89864bd7b5e13c83eb49b1f314a70abf8bd7e871b706bb6768c9e1
```

## Limits and memory behavior

These are byte and retained-state caps, not reservations:

| Resource | Limit |
| --- | ---: |
| `/v2/session` request body | 4 KiB actual bytes |
| Request metadata JSON | 128 KiB |
| Logical request body | 50 MiB |
| Encoded request plaintext | 52,559,876 bytes |
| Raw encrypted request record | 52,559,908 bytes |
| Credential | 16 KiB |
| Method | 32 bytes |
| Relative target | 16 KiB |
| Logical request headers | 64 entries |
| Response metadata JSON | 64 KiB |
| Response Chunk | 64 KiB |
| Encrypted response record | 65,553 bytes |
| Logical response headers | 32 entries |
| Error code | 64 bytes |
| Session lifetime | 3,600 seconds, absolute |
| Live sessions per enclave process | 2,097,152 |
| Replay IDs per session | 1,048,576 |
| Replay IDs per enclave process | 16,777,216 |
| Pending OAuth states | 4,096 per provider for 10 minutes |

These body, session, and replay caps are active defaults, not optional
deployment settings. The gateway allocates for bytes and state that actually
arrive. It does not pre-reserve a maximum request, translate byte ceilings into
concurrent-request permits, or hold a provider/storage permit for a stream. The
session and replay limits allocate no entries up front.

The request carrier is intentionally buffered once before decryption. During
admission, a maximum request can briefly coexist as about 50.1 MiB of
ciphertext and 50.1 MiB of plaintext (about 100.25 MiB total before ordinary
HTTP and allocator overhead). Smaller requests use proportionally less. The
decoded logical body slices the decrypted allocation rather than copying the
50 MiB tail again.

There is deliberately no transport-level aggregate reservation for in-flight
bytes or concurrent requests. Concurrent maximum-size requests multiply that
transient footprint, so ingress admission, memory observability, and horizontal
capacity must keep such traffic within the enclave's real RAM. Likewise, the
session and replay counts are hard safety ceilings rather than steady-state
capacity promises; hash-table overhead makes their RAM cost greater than the
raw identifier bytes.

Responses are processed as bounded records. A long stream has no protocol-wide
aggregate-byte or per-session response-record reservation. Once the response
subkey is derived, the stream writer retains only that key plus its session ID,
request ID, and sequence; it does not pin the complete session or replay set.

Expired sessions are purged at least once per minute and on lookup. Live
sessions are never evicted to make room for unauthenticated handshakes. A full
session store or global replay set fails closed with `503`; dropping a session
releases all of its retained replay IDs. Expiry prevents new admissions, but an
already-admitted response or stream may finish after its session expires.

## Redirect-based OAuth authorization-code binding

V2 OAuth initiation and callback both travel through the encrypted request
channel. The enclave stores each one-time OAuth state with the exact initiating
V2 session and a provider-verifiable proof:

- GitHub and Google authorization requests use a fresh PKCE S256 challenge;
  the enclave retains the verifier and supplies it exactly once during the
  token exchange.
- Apple authorization requests use a fresh raw nonce retained only in enclave
  memory. The authorization URL carries lowercase hex
  `SHA-256(raw_nonce_ascii)`, and the signed token-endpoint ID token must contain
  the same nonce value when verified from the retained raw nonce.

The callback must use the initiating V2 session and matching state. A mismatch
in state or session does not consume the legitimate state; a match is
atomically consumed before provider exchange. A provider or network failure
after that point requires a new OAuth attempt. Clients must therefore preserve
the exact attested session across the browser redirect. The Apple front-channel
ID token is not the proof used here; verification uses the signed ID token from
the token endpoint. This redirect proof does not replace `/auth/apple/native`:
that route verifies the submitted signed identity token and preserves its
existing optional client-provided nonce behavior. V1 keeps its existing OAuth
URL and state behavior.

## Refresh, retry, and downgrade rules

V2 refresh is a separate encrypted logical request to `/refresh` or
`/platform/refresh`. It has no logical body and carries the refresh token as a
`resumption` credential. A successful response replaces the client's stored V2
access and refresh tokens.

Refresh currently issues a new signed pair but does not consume or revoke the
previous stateless refresh token. Do not describe this as one-time refresh-token
rotation; revocation remains a separate application concern.

Official clients use the access token's decoded expiry only as a scheduling
hint. They refresh before sending a new operation when expiry is within 30
seconds and coalesce concurrent refresh work for the same authentication
revision. Authentication remains a server decision. If preflight refresh fails
transiently, credentials are preserved and the original operation is not sent.

Managed SDK requests allow one automatic resend of the original logical
operation after a fresh, verified attestation handshake, and only when the
first attempt returns outer HTTP `400`, exactly
`x-opensecret-error-contract: 1`, and exactly one of the two recovery codes
above. The resend uses the new session and a fresh request ID; a second failure
is returned to the caller. This applies to ordinary managed requests including
mutations and inference, not just reads.

This deliberately matches V1's best-effort session recovery behavior. An
untrusted intermediary can let the original request execute, replace its
response with a forged recovery hint, and cause the client to submit the
operation again under a new session. The per-session replay set cannot prevent
that second execution: cross-session at-most-once execution is not guaranteed.
Operations needing that guarantee require application-level idempotency.

There is no automatic resend for client-side response AEAD or framing failures,
network failures or timeouts, partial streams, redirects, generic outer `400`
or `503`, or ordinary application errors. An authenticated
`access_token_expired` response may trigger refresh for future operations, but
the original operation is still returned as failed. A lost response remains an
ambiguous outcome. Prepared native-handoff redemption and session-bound OAuth
callbacks cannot move to a fresh session; their flows must restart instead.

V2 clients do not fall back to V1 when session establishment or a request
fails. The narrow recovery hints never authenticate response content or permit
plaintext credentials; logical credentials remain inside the encrypted resend.
V1 and V2 use separate session stores, record formats, and internal token
audiences. Existing
raw API keys remain the same inference-only application capability across both
transports; the transport does not rotate them.

## Native application handoff

Native handoff transfers authentication without placing access or refresh
tokens in a deep link:

1. The native client establishes a V2 session, chooses the random request ID
   it will use for redemption, and retains both without sending the request yet.
2. An already-authenticated V2 browser session requests
   `/auth/native-handoff/grant` with that target session ID and request ID.
3. OpenSecret returns an ES256K-signed compact JWT grant, valid for five
   minutes (with 30 seconds of validation leeway), containing the user's
   authenticated binding and the exact target session and request IDs. Its
   claims are readable, so a client treats it as a short-lived capability
   rather than encrypted data.
4. The native client sends `/auth/native-handoff/redeem` under the prearranged
   session and request ID, with the grant in the logical body and no credential
   or cache root.
5. OpenSecret verifies the grant, its times and audience, the exact transport
   bindings, current user/project ownership, and the active seed wrap, then
   returns fresh V2 access and refresh tokens encrypted to the native session.

A copied grant cannot be redeemed from a different session or request. The
session replay set prevents duplicate redemption within the live session.
Prepared redemption does not use managed session-recovery retries: losing its
bound session requires a new handoff and grant.

## V1 compatibility

Transport V2 is an additive network boundary. Its decrypted logical methods,
targets, headers, bodies, statuses, and streams are dispatched through the same
application routers used by V1. A logical target such as `/v1/chat/completions`
inside a V2 envelope is not a V1 transport request.

The legacy attestation/session endpoints, outer bearer behavior, encrypted-body
shape, response shape, OAuth behavior, and `SHA-256(user UUID)` Tinfoil cache
namespace remain unchanged for released V1 clients. A new server therefore
continues to serve old clients. New V2 clients speak only V2. Existing V1 user
or platform JWT sessions require fresh V2 authentication rather than token
migration; login, registration, OAuth, and native handoff are valid V2 entry
paths, while existing raw inference-only API keys remain usable. V1 cache
entries do not share the new V2 namespace, and V1 traffic does not gain V2's
credential confidentiality, whole-request binding, or replay guarantees.
This transport change adds no database migration, schema backfill, runtime
environment variable, deployed secret, or additional service.

## Rollout gates

Before enabling V2 clients in production:

1. Deploy the V2-capable backend first and prove released V1 clients still use
   their unchanged paths.
2. Build and verify the production EIF, publish the reviewed PCR evidence, and
   confirm each client enforces the intended PCR policy before key derivation.
3. Keep each V2 session's normal traffic on the enclave process that created
   it. A single origin or stable [DNS-only
   persistence](https://developers.cloudflare.com/load-balancing/additional-options/dns-persistence/)
   can meet this requirement; HTTP-header affinity does not apply in DNS-only
   mode. Verify the actual topology and persistence rather than inferring it
   from a hostname. For a proxied HTTP deployment whose multiple origins rely
   on cookie affinity, preserve the existing cookie policy for V1 and add a
   [custom load-balancing
   rule](https://developers.cloudflare.com/load-balancing/additional-options/load-balancing-rules/)
   matching `starts_with(http.request.uri.path, "/v2/")` that overrides only
   those requests to [HTTP-header session
   affinity](https://developers.cloudflare.com/load-balancing/understand-basics/session-affinity/)
   on `x-opensecret-routing-key`, requires that header, and uses the maximum
   3,600-second idle TTL. Canonical clients retire the absolute 3,600-second
   enclave session 30 seconds early, before an otherwise-idle affinity entry
   can expire. Global replacement of cookie affinity would break released V1
   clients, while cookie affinity alone is insufficient for V2 because V2
   deliberately omits credentials and rejects outer cookies. Header affinity
   also cannot use Cloudflare's sticky zero-downtime failover; an origin loss
   therefore requires a fresh attestation and session. Only the marked outer
   `400` recovery cases permit one automatic resend; network failure alone
   does not. Session-bound OAuth callbacks and prepared native handoffs must
   restart because their state cannot move to the replacement session.
4. Configure ingress to pass `application/octet-stream` bodies byte-for-byte,
   permit at least 52,559,908 bytes if the full logical body limit is supported,
   accept ordinary HTTP transfer framing, and stream responses without
   buffering. Idle timeouts must accommodate provider streaming behavior.
5. Keep HTTPS mandatory except for the exact loopback development carve-out;
   do not follow redirects for either V2 endpoint.
6. Exercise user, platform, API-key, refresh, bodyless mutation, binary, SSE,
   OAuth-provider, native-handoff, replay, disconnect, and enclave-restart paths
   through the pinned SDKs. Provider-backed and real-Nitro checks are distinct
   from local mock/unit tests. Before Apple redirect OAuth is enabled, a real
   provider smoke must confirm that the signed token-endpoint ID token carries
   the requested nonce.
7. Restrict direct origin ingress to Cloudflare, or enforce equivalent
   connection, upload-time, and request-rate controls at every origin-local
   boundary. Rate-limit public session creation and other unauthenticated
   admission where those controls cannot be bypassed. The process-wide state
   counts are final safety caps, not the primary abuse-control policy.
8. Load-test concurrent large requests against the production enclave memory
   profile and set operational admission and headroom accordingly. The
   protocol intentionally has no custom aggregate memory reservation layer.
9. Retain a coordinated rollback plan. Rolling the backend back below V2 after
   publishing V2-only clients strands those clients; they intentionally do not
   downgrade or transmit credentials over V1.
