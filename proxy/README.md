# 🍁 Maple Proxy

A lightweight proxy for Maple/OpenSecret's OpenAI-compatible inference
endpoints, with the security and privacy benefits of Trusted Execution
Environment (TEE) processing.

## 🚀 Features

- **OpenAI-Compatible Surface** - Models, chat completions, and embeddings endpoints
- **Secure TEE Processing** - All requests processed in secure enclaves
- **Lossless Chat Parameters** - Provider-specific request fields pass through unchanged
- **Streaming and Non-Streaming** - Supports both chat completion response modes
- **Flexible Authentication** - Environment variables or per-request API keys
- **Familiar Clients** - Point compatible OpenAI clients at the proxy base URL
- **Lightweight** - Minimal overhead, maximum performance
- **Optional CORS** - Browser mode requires per-request keys and deployment review

## 📦 Installation

### As a Binary

Maple's current release workflow builds native proxy archives for Linux x86_64,
Linux ARM64, Apple Silicon macOS, and Windows x86_64 and attaches them to the
ordinary Maple app Release. Maple v3.3.9 completed the first post-integration
publication and verification of all four archives plus their checksum manifest.

After that release, verify the assets are present and use the stable download
URLs, for example:

```bash
curl -LO https://github.com/MaplePrivacyLabs/Maple/releases/latest/download/maple-proxy-linux-x86_64.tar.gz
curl -LO https://github.com/MaplePrivacyLabs/Maple/releases/latest/download/maple-proxy-release-final.sha256
sha256sum --check --ignore-missing maple-proxy-release-final.sha256
```

There is no separate proxy GitHub Release or proxy release tag. To build from
source now:

```bash
git clone https://github.com/MaplePrivacyLabs/Maple.git
cd Maple/proxy
cargo build --locked --release
```

### As a Library

Add to your `Cargo.toml`:

```toml
[dependencies]
maple-proxy = "0.3.2"
```

Crates.io publishing remains separate from Maple application releases; the
example above uses the latest published crate version.

This checkout prepares proxy `0.4.0` with a compatible registry requirement for
`maple-sdk` starting at `4.0.0`. The published proxy crate in the example above
predates this Transport V2 update. The standalone binary's lockfile selects its
SDK version; an embedding application selects its own. Keep the host and proxy
on one SDK source/version because their public APIs exchange SDK types. Local
SDK links remain supported during development. See the
[SDK consumer version policy](../docs/sdk-publishing.md#consumer-version-policy).
Publishing the SDK does not publish the proxy crate or release the Maple app.

## ⚙️ Configuration

Set environment variables or use command-line arguments:

```bash
# Environment Variables
export MAPLE_HOST=127.0.0.1                    # Server host (default: 127.0.0.1)
export MAPLE_PORT=8080                         # Server port (default: 8080)
export MAPLE_BACKEND_URL=http://localhost:3000         # Maple backend URL (prod: https://enclave.trymaple.ai)
export MAPLE_PCR0_ENVIRONMENT=production       # PCR0 trust roots: production (default) or development
export MAPLE_API_KEY=your-maple-api-key        # Optional for trusted, non-browser clients only
export MAPLE_CACHE_NAMESPACE_ROOT="$(openssl rand -base64 32)" # Generate once; persist it
export MAPLE_DEBUG=true                        # Enable debug logging
export MAPLE_ENABLE_CORS=false                 # Default; see browser warning below
export MAPLE_REQUEST_TIMEOUT_SECS=300          # Backend request timeout
export MAPLE_STREAM_IDLE_TIMEOUT_SECS=300      # Streaming idle timeout between chunks
```

Both timeout defaults are five minutes. `MAPLE_REQUEST_TIMEOUT_SECS` covers
sending a request through completion of a non-streaming response body. For SSE,
it covers response setup; once the stream starts, `MAPLE_STREAM_IDLE_TIMEOUT_SECS`
limits the wait for each next chunk without imposing a total stream duration.
A timeout before response headers returns HTTP 504. After headers have been
forwarded, a timeout terminates the response body with an error. Timed-out
requests are not automatically retried.

The underlying Rust SDK permits one session-recovery resend, including for
inference, after a fresh verified handshake only on outer HTTP `400` with
exactly `x-opensecret-error-contract: 1` and `x-opensecret-error-code` equal to
`session_not_found` or `request_decryption_failed`. The backend marks only a
missing/expired session or incoming-request AEAD authentication failure before
dispatch. These hints are unauthenticated: an intermediary can forge one after
execution and cause a duplicate in the new session. This V1-equivalent
best-effort behavior does not guarantee cross-session at-most-once execution.
Response decryption/framing failures, network failures, timeouts, partial
streams, generic `400`/`503`, redirects, and application errors do not trigger
resends. There is no V1 fallback or plaintext credential resend. See the
[Rust SDK retry limits](../sdk/rust/README.md#session-recovery-and-retry-limits).

`MAPLE_CACHE_NAMESPACE_ROOT` is a client-side secret used to keep Tinfoil
provider-cache entries stable across proxy restarts. Generate it once with
`openssl rand -base64 32`, store the canonical padded base64 output in your
secret manager, and never log or commit it. If it is omitted, the proxy safely
generates a process-lifetime root; requests still work, but provider-cache hits
from an earlier proxy process are intentionally unavailable.

Or use CLI arguments:
```bash
cargo run --locked -- --host 0.0.0.0 --port 8080 --backend-url https://enclave.trymaple.ai

# Development enclaves must be selected explicitly
cargo run --locked -- --backend-url https://enclave.secretgpt.ai --pcr0-environment development
```

## 🛠️ Usage

### Using as a Binary

#### Start the Server

```bash
cargo run --locked
```

You should see:
```
🚀 Maple Proxy Server started successfully!
📋 Available endpoints:
   GET  /health              - Health check
   GET  /v1/models           - List available models
   POST /v1/chat/completions - Create chat completions (streaming & non-streaming)
   POST /v1/embeddings       - Create embeddings
```

### API Endpoints

#### List Models
```bash
curl http://localhost:8080/v1/models \
  -H "Authorization: Bearer YOUR_MAPLE_API_KEY"
```

#### Chat Completions
```bash
curl -N http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer YOUR_MAPLE_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "llama3-3-70b",
    "messages": [
      {"role": "user", "content": "Write a haiku about technology"}
    ],
    "stream": true
  }'
```

Set `stream` to `true` for Server-Sent Events or `false` for one JSON response.
Additional provider-specific JSON fields are forwarded without being parsed or
rewritten by the proxy or Rust SDK.

#### Embeddings
```bash
curl http://localhost:8080/v1/embeddings \
  -H "Authorization: Bearer YOUR_MAPLE_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "nomic-embed-text",
    "input": "Generate an embedding for this text"
  }'
```

### Using as a Library

You can also embed Maple Proxy in your own Rust application:

```rust
use maple_proxy::{Config, Pcr0Environment, create_app};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    // Create config programmatically
    let config = Config::new(
        "127.0.0.1".to_string(),
        8081,  // Custom port
        "https://enclave.trymaple.ai".to_string(),
    )
    .with_pcr0_environment(Pcr0Environment::Production)
    .with_api_key("your-api-key-here".to_string())
    .with_debug(true)
    .with_cors(true);

    // Create the app
    let app = create_app(config.clone());

    // Start the server
    let addr = config.socket_addr()?;
    let listener = TcpListener::bind(addr).await?;
    
    println!("Maple proxy server running on http://{}", addr);
    
    axum::serve(listener, app).await?;

    Ok(())
}
```

Run the example:
```bash
cargo run --locked --example library_usage
```

## 💻 Client Examples

### Python (OpenAI Library)

```python
import openai

client = openai.OpenAI(
    api_key="YOUR_MAPLE_API_KEY",
    base_url="http://localhost:8080/v1"
)

# Streaming chat completion
stream = client.chat.completions.create(
    model="llama3-3-70b",
    messages=[{"role": "user", "content": "Hello, world!"}],
    stream=True
)

for chunk in stream:
    if chunk.choices[0].delta.content is not None:
        print(chunk.choices[0].delta.content, end="")
```

### JavaScript/Node.js

```javascript
import OpenAI from 'openai';

const openai = new OpenAI({
  apiKey: 'YOUR_MAPLE_API_KEY',
  baseURL: 'http://localhost:8080/v1',
});

const stream = await openai.chat.completions.create({
  model: 'llama3-3-70b',
  messages: [{ role: 'user', content: 'Hello!' }],
  stream: true,
});

for await (const chunk of stream) {
  process.stdout.write(chunk.choices[0]?.delta?.content || '');
}
```

### cURL

```bash
# Health check
curl http://localhost:8080/health

# List models
curl http://localhost:8080/v1/models \
  -H "Authorization: Bearer YOUR_MAPLE_API_KEY"

# Streaming chat completion
curl -N http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer YOUR_MAPLE_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "llama3-3-70b",
    "messages": [{"role": "user", "content": "Tell me a joke"}],
    "stream": true
  }'

# Embeddings
curl http://localhost:8080/v1/embeddings \
  -H "Authorization: Bearer YOUR_MAPLE_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "nomic-embed-text",
    "input": "Generate an embedding for this text"
  }'
```

## 🔐 Authentication

Maple Proxy supports two authentication methods:

### 1. Environment Variable (Default)
Set `MAPLE_API_KEY` - all requests will use this key by default:
```bash
export MAPLE_API_KEY=your-maple-api-key
cargo run --locked
```

### 2. Per-Request Authorization Header
Override the default key or provide one if not set:
```bash
curl -H "Authorization: Bearer different-api-key" ...
```

The proxy maintains one shared attested V2 session. Each request's API key is
placed inside that request's encrypted envelope; it is not installed as mutable
client state and is not written to proxy logs.

## 🌐 CORS Support

The standalone proxy does not inherit Maple's Tauri wrapper protections. If a
browser must call it, do not configure `MAPLE_API_KEY`; require every request
to carry its own bearer key and review the network exposure separately before
enabling CORS:

```bash
unset MAPLE_API_KEY
export MAPLE_ENABLE_CORS=true
cargo run --locked
```

When CORS is enabled, the proxy ignores any configured default API key and
rejects requests that do not provide their own bearer credential.

## 🐳 Docker Deployment

### Pre-built Image

New container publications use `ghcr.io/mapleprivacylabs/maple-proxy` after the
Maple repository transfer. The release-following publisher builds the eligible
proxy version from a successful stable Maple Release and verifies its exact tag,
platforms, provenance, and aliases. The first publication in this namespace must
complete and the package must be public before the commands below will work.
GitHub creates new packages privately. Once the first image exists, set the new
package to public in GitHub package settings, retain Maple Actions write access,
and rerun only the container publisher if anonymous verification was blocked.
The retry verifies an existing exact version rather than overwriting it.

The old `ghcr.io/opensecretcloud/maple-proxy` package remains available at its
existing versions but receives no new publications. Existing deployments must
change their image URL to receive future updates; registry image names do not
follow the repository transfer redirect. Version `0.3.3` remains the explicit
unbackfilled baseline. The next prepared proxy version is `0.4.0`.

After the first new-namespace publication is verified:

```bash
# Pull the latest image
docker pull ghcr.io/mapleprivacylabs/maple-proxy:latest

# Run with your API key
docker run -p 8080:8080 \
  -e MAPLE_BACKEND_URL=https://enclave.trymaple.ai \
  -e MAPLE_REQUEST_TIMEOUT_SECS=300 \
  -e MAPLE_STREAM_IDLE_TIMEOUT_SECS=300 \
  ghcr.io/mapleprivacylabs/maple-proxy:latest
```

### Build from Source

```bash
# Build the image locally
just docker-build

# Run the container
just docker-run
```

### Production Docker Setup

1. **Option A: Use the published image from GHCR**
```bash
# In your docker-compose.yml, use:
image: ghcr.io/mapleprivacylabs/maple-proxy:latest
```

2. **Option B: Build your own image**
```bash
docker build -f Dockerfile -t maple-proxy:latest ..
```

3. **Run with docker-compose:**
```bash
# Copy the example environment file
cp .env.example .env

# Edit .env with your configuration
vim .env

# Start the service
docker-compose up -d
```

### 🔒 Security Note for Public Deployments

When deploying Maple Proxy on a public network:

- **DO NOT** set `MAPLE_API_KEY` in the container environment
- Instead, require clients to pass their API key with each request:

```python
# Client-side authentication for public proxy
client = OpenAI(
    base_url="https://your-proxy.example.com/v1",
    api_key="user-specific-maple-api-key"  # Each user provides their own key
)
```

This keeps API keys out of the shared container configuration and allows each
client to supply its own credential. Review proxy logs and browser policy
before treating a public deployment as safe.

### Docker Commands

```bash
# Build image
just docker-build

# Run interactively
just docker-run

# Run in background
just docker-run-detached

# View logs
just docker-logs

# Stop container
just docker-stop

# Use docker-compose
just compose-up
just compose-logs
just compose-down
```

### Container Configuration

The Docker image:
- Uses multi-stage builds for minimal size (~130MB)
- Runs as non-root user for security
- Includes health checks
- Supports both x86_64 and ARM architectures

### Environment Variables for Docker

```yaml
# docker-compose.yml environment section
environment:
  - MAPLE_BACKEND_URL=https://enclave.trymaple.ai  # Production backend
  - MAPLE_ENABLE_CORS=true                         # Enable for web apps
  - MAPLE_REQUEST_TIMEOUT_SECS=300                 # Backend request timeout
  - MAPLE_STREAM_IDLE_TIMEOUT_SECS=300             # Streaming idle timeout
  - RUST_LOG=info                                  # Logging level
  # - MAPLE_API_KEY=xxx                            # Only for private deployments!
  # - MAPLE_CACHE_NAMESPACE_ROOT=xxx               # Stable secret for cache continuity
```

## 🔧 Development

### Docker Images & CI/CD

The source lives under [`proxy/`](https://github.com/MaplePrivacyLabs/Maple/tree/master/proxy)
in the Maple repository. Root, path-scoped workflows run locked Rust checks,
supply-chain policy, and non-publishing AMD64/ARM64 container builds for proxy
changes. After every successful stable Maple Release, a separate serialized
publisher compares the checked-in proxy version with the previous stable Maple
Release and rejects changed container inputs without a version bump. Version
`0.3.3` remains the explicit unbackfilled migration baseline. A new or missing
later version publishes native AMD64/ARM64 images with per-platform provenance,
verifies their exact build digests and release labels, then reconciles the minor,
major, and `latest` aliases. Existing exact versions are verified without being
overwritten, and manual dispatch safely retries verification or alias repair.
The transferred Maple repository publishes with its `GITHUB_TOKEN` in the
`MaplePrivacyLabs` organization. No old-organization package token is needed.
The package must grant Maple Actions write access and be public for anonymous
verification; local recipes intentionally cannot publish to it.

`proxy/Cargo.lock`, `sdk/rust/Cargo.lock`, and
`apps/maple-research/frontend/src-tauri/Cargo.lock` remain separate lockfiles. Runtime dependency
changes must keep the selected SDK sources and affected locks coherent; one
consumer's SDK upgrade does not require upgrading the others. Docker builds
must use the Maple repository root as context so both `proxy/` and `sdk/rust/`
are available. GitHub Release archives, crates.io, and GHCR are three separate
publication paths; completing one does not update the others.

**Local Development (Justfile)**
```bash
# For local testing and debugging
just docker-build        # Build locally
just docker-run          # Test locally
```

### Build from Source
```bash
cargo build --locked
```

### Run with Debug Logging
```bash
export MAPLE_DEBUG=true
cargo run --locked
```

### Run Tests
```bash
cargo test --locked
```

## 📊 Supported Models

Maple Proxy supports all models available in the Maple/OpenSecret platform, including:
- `llama3-3-70b` - Llama 3.3 70B parameter model
- `nomic-embed-text` - Embedding model for `/v1/embeddings`
- And many others - check `/v1/models` endpoint for current list

## 🔍 Troubleshooting

### Common Issues

**"No API key provided"**
- Set `MAPLE_API_KEY` environment variable or provide `Authorization: Bearer <key>` header

**"Failed to establish secure connection"**
- Check your `MAPLE_BACKEND_URL` is correct
- Ensure your API key is valid
- Check network connectivity

**Connection refused**
- Make sure the server is running on the specified host/port
- Check firewall settings

### Debug Mode

Enable debug logging for detailed information:
```bash
export MAPLE_DEBUG=true
cargo run --locked
```

## 🏗️ Architecture

```
┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐
│   OpenAI Client │───▶│   Maple Proxy   │───▶│  Maple Backend  │
│   (Python/JS)   │    │   (localhost)   │    │      (TEE)      │
└─────────────────┘    └─────────────────┘    └─────────────────┘
```

1. **Client** makes standard OpenAI API calls to localhost
2. **Maple Proxy** maintains one attested encrypted session to the TEE
3. **Requests**, including their individual API keys, are encrypted and
   securely forwarded
4. **Responses** are streamed back to the client in OpenAI format

## 📝 License

MIT License - see LICENSE file for details.

## 🤝 Contributing

Contributions welcome! Please feel free to submit a Pull Request.
