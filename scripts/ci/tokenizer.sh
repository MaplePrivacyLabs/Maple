#!/usr/bin/env bash
# Run inside the backend's pinned Nix shell with stateful hooks disabled.
# A path dependency's own integration/golden tests are not run by its consumers.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
tokenizer_manifest="$repo_root/crates/model-tokenizer/Cargo.toml"

cargo fmt --all --manifest-path "$tokenizer_manifest" -- --check
cargo clippy --locked --all-targets --all-features --manifest-path "$tokenizer_manifest" -- -D warnings
cargo test --locked --all-features --manifest-path "$tokenizer_manifest"
# Exercise both serde_json map representations so a consumer's feature union
# cannot silently change the shared crate's prompt serialization.
cargo test --locked --all-features --features serde_json/preserve_order --manifest-path "$tokenizer_manifest"
