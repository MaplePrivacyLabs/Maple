# Maple Rust SDK

This source lives under Maple's `sdk/rust/` directory. Research desktop,
Maple Agent, and the proxy select published versions independently; local links
remain supported for development. See the
[consumer version policy](../../docs/sdk-publishing.md#consumer-version-policy).

Maple Rust SDK for the OpenSecret backend: secure AI APIs, encrypted sessions, and Nitro attestation.

## Features

- 🔐 **Nitro Attestation**: Verify server identity through AWS Nitro Enclaves
- 🔑 **End-to-End Encryption**: All API calls encrypted with session keys
- 👤 **Authentication**: Support for both email-based and guest users
- 🔄 **Token Management**: Proactive, coalesced refresh
- ↔️ **Lossless Inference Transport**: Provider-specific request and response fields pass through as raw bytes
- 🛡️ **Secure by Default**: No plaintext data transmission

## Installation

`maple-sdk` is the published replacement for the `opensecret` crate. Add the
published version selected for your application and backend to `Cargo.toml`.
The `4.0.0` pin below is an example, not a current-version declaration. This
SDK's source version is declared in [`Cargo.toml`](Cargo.toml); a consumer's
manifest and lockfile determine what it builds.

```toml
[dependencies]
maple-sdk = "=4.0.0"
bytes = "1"
futures = "0.3"
http = "1"
```

Public Rust types such as `OpenSecretClient` keep their existing names from the
package rename. Version 4 uses Transport V2, requires a V2-capable backend, and
does not fall back to V1. Existing V1 users must sign in again to establish a new
session. Coordinate native and hosted OAuth flows with the consuming app; see
the [version 4 upgrade notes](../README.md#version-4-upgrade).

## Quick Start

The caller supplies the backend URL and the UUID of a project registered in
that backend. For a disposable local backend, pass its actual HTTP loopback
URL, including its assigned port, and use local email/password or guest
fixtures. The SDK does not infer these values from your application's
environment or choose a hosted authentication website. See the
[environment configuration table](../README.md#typescriptreact-sdk) for the
separate local, hosted development, and production settings.

```rust
use maple_sdk::{OpenSecretClient, Result};
use uuid::Uuid;

async fn register_local_user(
    api_url: &str,
    client_id: Uuid,
    email: String,
    password: String,
) -> Result<()> {
    let client = OpenSecretClient::new(api_url)?;

    // Establish secure session
    client.perform_attestation_handshake().await?;

    // Register and login
    let response = client.register(
        email,
        password,
        client_id,
        Some("John Doe".to_string())
    ).await?;

    println!("Logged in as: {}", response.id);
    Ok(())
}
```

Production clients verify both the AWS Nitro attestation and the enclave's
PCR0 deployment identity. `OpenSecretClient::new` uses pinned official PCR0
values and OpenSecret's signed production history. Development trust must be
selected explicitly and checks only the development roots and signed history:

```rust
use maple_sdk::Pcr0Environment;

let development_client = OpenSecretClient::new_with_pcr0_environment(
    "https://enclave.secretgpt.ai",
    Pcr0Environment::Development,
)?;
```

The API-key equivalent is
`OpenSecretClient::new_with_api_key_and_pcr0_environment`. A signed PCR0 added
to the selected GitHub history is accepted without a client update. Neither
official policy falls back to the other environment.

Custom deployments can add a static allowlist without replacing the selected
official trust policy:

```rust
use maple_sdk::Pcr0TrustPolicy;

let policy = Pcr0TrustPolicy::official().with_additional_pcr0s([
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
])?;
let client = OpenSecretClient::new_with_pcr0_trust_policy(
    "https://api.opensecret.cloud",
    policy,
)?;
```

Use `Pcr0TrustPolicy::from_static_allowlist(...)` to disable remote history and
trust only an explicit custom set. Remote entries are size/time bounded and
must verify against the SDK's hardcoded OpenSecret P-384 signing key. Exact
`http://` localhost and loopback development endpoints use mock attestation;
Android also supports the exact emulator alias `10.0.2.2`. Other endpoints
must use HTTPS. An HTTP address on the Mac's LAN is not this local exception;
do not weaken attestation checks to accommodate physical-device networking.

## Inference APIs

`send_inference_request` is the lossless inference API. The caller owns the
HTTP method, route, query, headers, body bytes, and response parsing; the SDK
owns attestation, authentication, encryption, and response decryption. It does
not parse or rewrite inference parameters such as `stream`. Its only automatic
resend is the bounded session recovery described below:

```rust
use bytes::Bytes;
use futures::StreamExt;
use http::Request;

let request = Request::post("/v1/chat/completions")
    .header("x-provider-option", "value")
    .body(Bytes::from_static(br#"{
        "model": "provider-model",
        "messages": [{"role": "user", "content": "Hello"}],
        "provider_option": {"enabled": true}
    }"#))
    .expect("valid inference request");

let response = client.send_inference_request(request).await?;
let status = response.status();
let mut body = response.into_body();
while let Some(chunk) = body.next().await {
    let decrypted_bytes = chunk?;
    // Parse or forward the bytes in the calling application.
}
```

The transport accepts only the SDK's explicitly allowed inference routes;
it cannot be used to bypass JWT-only account or conversation APIs. The existing
typed model, embedding, and chat-completion helpers remain available as
compatibility wrappers over the same transport.

The SDK manages transport credentials and framing. Caller-provided `Host`,
`Authorization`, cookies, `x-session-id`, `Forwarded`, `Via`,
`X-Forwarded-*`, `Content-Length`, `Content-Encoding`, `Accept-Encoding`,
`Content-MD5`, `Digest`, hop-by-hop, and `Connection`-listed headers are not
forwarded. Logical `Content-Type` and other end-to-end headers are preserved,
including multipart boundaries.

API keys can also be supplied per request with
`send_inference_request_with_api_key`. The key is encrypted inside that one
logical request and is not installed as mutable client state.

### Session recovery and retry limits

Managed requests, including inference and mutations, permit one resend after a
fresh, verified attestation handshake only on outer HTTP `400` with exactly
`x-opensecret-error-contract: 1` and `x-opensecret-error-code` equal to
`session_not_found` or `request_decryption_failed`. The server marks only an
actually missing/expired session or incoming-request AEAD authentication
failure before dispatch. The resend uses a new session and request ID.

This is V1-equivalent best-effort recovery: an intermediary can forge these
unauthenticated hints after the original operation executed. The new session's
replay set cannot prevent duplicate execution across sessions; operations that
need at-most-once execution require application-level idempotency.

Client-side response decryption/framing failures, network failures, timeouts,
partial streams, redirects, generic `400`/`503`, and application errors are not
automatically retried. Prepared native handoffs and session-bound OAuth
callbacks require a new flow instead of recovery under a new session. There is
no V1 fallback or plaintext credential resend. Proactive token refresh is
separate; an expired-access response can refresh future credentials without
resending the failed operation.

## Provider cache continuity

Transport V2 uses a random 32-byte client secret to namespace provider cache
entries. The default is random for each `OpenSecretClient`. Applications that
need cache hits across restarts should generate a root once, store it as a
secret, and restore it when constructing the client:

```rust
use maple_sdk::TransportV2CacheNamespaceRoot;

let root = TransportV2CacheNamespaceRoot::generate()?;
let persisted = root.to_base64();

let restored = TransportV2CacheNamespaceRoot::from_base64(&persisted)?;
let client = OpenSecretClient::new(api_url)?
    .with_cache_namespace_root(restored);
```

Do not derive this root from a user identifier or credential, and do not log
it. The type redacts `Debug` output and zeroizes its bytes on drop.

## Authentication

### User Registration

Register with email:
```rust
let response = client.register(
    email,
    password,
    client_id,
    Some(name)  // Optional
).await?;
```

Register as guest (no email):
```rust
let response = client.register_guest(
    password,
    client_id
).await?;
```

### Login

Login with email:
```rust
let response = client.login(
    email,
    password,
    client_id
).await?;
```

Login with user ID (guests only):
```rust
let response = client.login_with_id(
    user_id,
    password,
    client_id
).await?;
```

### OAuth callbacks

Transport V2 binds an OAuth attempt to the attested session that initiated it.
Call `initiate_github_auth`, `initiate_google_auth`, or `initiate_apple_auth`
and the matching `handle_*_callback` method on the same live
`OpenSecretClient`. Reconstructing the client between those calls establishes
a different session and intentionally cannot redeem the original attempt.
Native applications should use OpenSecret's native handoff flow rather than
trying to export transport keys.

### Token Management

Tokens are automatically stored after login/registration. You can:

```rust
// Get one coherent access/refresh pair snapshot
let tokens = client.get_tokens()?;

// Individual reads remain available when a coherent pair is not required
let access_token = client.get_access_token()?;
let refresh_token = client.get_refresh_token()?;

// Refresh tokens
client.refresh_token().await?;

// Logout (clears credentials; the identity-neutral crypto session is reusable)
client.logout().await?;
```

## Session Management

Every API call uses an encrypted Transport V2 session:

1. **Attestation Handshake**: Establishes trust and exchanges encryption keys
2. **Encrypted Communication**: The entire logical request, including its
   credential, travels inside one authenticated envelope
3. **Token Authentication**: Protected endpoints require a valid credential in
   each encrypted request

```rust
// Optional eager setup; API calls also establish a session lazily.
client.perform_attestation_handshake().await?;

// Check session status
if let Some(session_id) = client.get_session_id()? {
    println!("Active session: {}", session_id);
}
```

The crypto session is not bound to one user or API key. Token changes and
logout therefore do not rotate it. Before a JWT-authenticated call, the SDK
uses the token's untrusted `exp` claim only as a timing hint and coalesces a
refresh when it is within 30 seconds of expiry. A refresh failure prevents that
new call from being sent. Transient, outer-transport, or AEAD failures preserve
stored credentials; only an authenticated 401 or 403 from the refresh request
clears them. Token refresh does not resend an operation that was already sent,
and ambiguous network or response-authentication failures do not trigger a
resend. The sole exception is the one fresh-session resend on an exact marked
outer `400` described in [Session recovery and retry limits](#session-recovery-and-retry-limits);
reattestation alone is not permission to replay a request.

## Error Handling

The SDK uses a custom `Error` type with detailed error variants:

```rust
use maple_sdk::Error;

match client.login(email, password, client_id).await {
    Ok(response) => println!("Success!"),
    Err(Error::Authentication(msg)) => println!("Auth failed: {}", msg),
    Err(Error::Api { status, message }) => println!("API error {}: {}", status, message),
    Err(e) => println!("Other error: {}", e),
}
```

## Testing

Run the credential-free library tests from `sdk/` with the pinned Nix shell:

```sh
nix develop --no-update-lock-file -c cargo test --locked \
  --manifest-path rust/Cargo.toml --all-features --lib
```

These tests need no existing backend or account credentials. The full format,
Clippy, library-test, and rustdoc gates are in the
[SDK development instructions](../README.md#rust-sdk) and
[`sdk-rust.yml`](../../.github/workflows/sdk-rust.yml).

Integration tests are a separate selection. Follow
[`sdk-integration.yml`](../../.github/workflows/sdk-integration.yml) for its
disposable database, same-checkout OpenSecret backend, and fixture setup. It
uses loopback and does not require hosted development accounts. Configure the
actual test endpoint and port, project UUID, and test identities from that
setup rather than assuming a backend is running on `localhost:3000`.

The test helpers read variables documented in [`.env.example`](.env.example).
Several also load `../.env.local` when run from `sdk/rust/`, with `.env`
fallbacks; the client library itself does not load dotenv configuration. Keep
test credentials in the ignored test environment, preserve configuration owned
by an external development environment, and use disposable identities. These
suites can create, change, and delete backend state. Production PCR trust is
the default when `VITE_OPEN_SECRET_PCR_ENVIRONMENT` is omitted; set
`development` explicitly for an intentionally selected hosted development
enclave.

After setting up the disposable fixtures, run from `sdk/rust/` inside the SDK
Nix shell:

```sh
cargo test --locked --all-features --tests
# The full-stack native-handoff case is intentionally ignored by --lib.
cargo test --locked --all-features --lib \
  client::tests::native_handoff_full_stack_round_trip -- --ignored --exact
```

An unfiltered `cargo test` also collects integration tests; it is not the
credential-free library gate. Provider-spending cases require explicit opt-in
with `RUN_LIVE_AI=1` and are separate from deterministic pull-request checks.

## Examples

[`examples/auth_example.rs`](examples/auth_example.rs) is a standalone local
authentication example. It hardcodes `http://localhost:3000`, requires
`VITE_TEST_CLIENT_ID`, and currently prints token prefixes. Use the Quick Start
above for an explicitly configured endpoint and project; keep token material
out of logs. This example is not an application launch profile or a smoke test
for an endpoint selected through environment variables.

## Security Considerations

1. **Always verify attestation** in production environments
2. **Store tokens securely** - the SDK keeps them in memory only
3. **Use HTTPS** for all production API calls
4. **Rotate tokens regularly** using the refresh mechanism
5. **Clear credentials** after use with `logout()`; the identity-neutral
   transport session may remain available for later anonymous use

## License

MIT
