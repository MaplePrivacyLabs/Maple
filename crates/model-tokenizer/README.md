# model-tokenizer

A neutral Rust library for pinned, offline text tokenization and serving-profile
prompt accounting. It has no application, router, storage or billing dependency.

`TokenizerId::from_model` accepts the eight explicit public catalog model IDs.
Unknown names and unresolved Auto selectors return an error. Raw counts match
the pinned text pipeline; they do not include chat formatting or media expansion.

```rust
use model_tokenizer::{count_text, TokenizerId};

let model = TokenizerId::from_model("deepseek-v4-1-flash")?;
let tokens = count_text(model, "A small text example.")?;
# Ok::<(), model_tokenizer::Error>(())
```

For an assembled chat request, select the actual deployment profile and call
`count_chat_request`. A profile identifies reviewed rendering behavior, rather
than asserting that a mutable external provider currently runs that behavior.
The result includes its revision-bearing profile ID and accuracy scope. Media,
unqualified provider templates and unsupported request overrides fail explicitly.

```rust
use model_tokenizer::{count_chat_request, RenderContext, RendererId};
use serde_json::json;

let request = json!({"messages": [{"role": "user", "content": "Hello."}]});
let count = count_chat_request(
    RendererId::DeepSeekV41Flash1017,
    &request,
    &RenderContext::default(),
)?;
# Ok::<(), model_tokenizer::RenderError>(())
```

The library does not reserve output tokens or remove conversation history.
Callers own that policy, then recount the final request. Cross-model fallback
requires preparing the same canonical history for the new profile. Run long
CPU-bound counts away from async/UI executor threads; loaded engines are cached
and shared by verified raw-asset identity.

Kimi's segment boundaries and ordinary/control token policies are significant.
Use the renderer output directly; concatenating segments before tokenization can
change counts or turn literal user text into structural tokens.

The GPT-OSS native Harmony profiles also retain every ordinary/control encoding
boundary from the pinned native renderer. They include vLLM's default system
message, tool namespace schemas, reasoning-history normalization and the
assistant completion header. The separate HF Jinja reference profiles remain
available for comparison; they are not substituted for native Harmony.

## Reproducible build inputs

The repository Nix shells provide `MAPLE_TOKENIZER_ASSETS_DIR` at build time.
`build.rs` verifies every asset's size and SHA-256 and embeds its bytes. There is
no runtime download, mutable Hub lookup, missing-asset fallback or Python daemon.
The standalone fetcher is `scripts/fetch-tokenizer-assets.py` at repository root;
it uses the same reviewed manifest by default. Model weights are not needed.

Run the crate tests from the backend's pinned shell with its stateful hooks off:

```sh
cd services/opensecret
OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
  nix develop --no-write-lock-file '.?submodules=1' -c \
  cargo test --locked --manifest-path ../../crates/model-tokenizer/Cargo.toml
```

Synthetic checked-in goldens compare native Rust IDs and rendered bytes with
pinned Python references. Tests run offline and require no inference credentials.
`examples/count_cpu.rs` is a bounded local CPU observation, including long text
without whitespace; it sends no requests. Local fixtures establish parity for
their identified source profiles, not deployed third-party accounting.

Tokenizer assets and templates retain their upstream licenses. Source revisions,
hashes and license references are recorded in `assets/manifest.json`, the Kimi
configuration provenance file and `profiles/manifest.json`; see `NOTICE`.
