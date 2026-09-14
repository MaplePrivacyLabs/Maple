//! Thin wrapper around tiktoken for fast, cached token counting.

use once_cell::sync::Lazy;
use tiktoken_rs::cl100k_base;

use crate::model_config::model_context_window;

/// Cached encoder – created once per process.
/// No mutex needed: CoreBPE is immutable and thread-safe.
static ENCODER: Lazy<tiktoken_rs::CoreBPE> =
    Lazy::new(|| cl100k_base().expect("init cl100k encoder"));

/// Count tokens for a piece of UTF‑8 text.
pub fn count_tokens(text: &str) -> usize {
    ENCODER.encode_with_special_tokens(text).len()
}

pub fn model_max_ctx(model: &str) -> usize {
    model_context_window(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_count_tokens_basic() {
        // Basic English text
        assert_eq!(count_tokens("Hello, world!"), 4); // "Hello", ",", " world", "!"

        // Empty string
        assert_eq!(count_tokens(""), 0);

        // Single token
        assert_eq!(count_tokens("Hello"), 1);
    }

    #[test]
    fn test_count_tokens_consistency() {
        // Same text should always produce same count
        let text = "The quick brown fox jumps over the lazy dog";
        let count1 = count_tokens(text);
        let count2 = count_tokens(text);
        assert_eq!(count1, count2);
    }

    #[test]
    fn test_model_max_ctx_known_models() {
        assert_eq!(model_max_ctx("llama3-3-70b"), 131_072);
        assert_eq!(model_max_ctx("gpt-oss-120b"), 131_072);
        assert_eq!(model_max_ctx("gpt-oss-safeguard-120b"), 131_072);
        assert_eq!(model_max_ctx("kimi-k2-6"), 262_144);
        assert_eq!(model_max_ctx("gemma4-31b"), 262_144);
        assert_eq!(model_max_ctx("glm-5-3"), 262_144);
        assert_eq!(model_max_ctx("glm-5-3-flash"), 1_048_576);
        assert_eq!(model_max_ctx("kimi-k3"), 262_144);
        assert_eq!(model_max_ctx("deepseek-v4-1-flash"), 1_048_576);
        assert_eq!(model_max_ctx("auto:quick"), 131_072);
        assert_eq!(model_max_ctx("auto:powerful"), 262_144);
    }

    #[test]
    fn test_model_max_ctx_unknown_models() {
        // Unknown models should default to 64k
        assert_eq!(model_max_ctx("gpt-4"), 64_000);
        assert_eq!(model_max_ctx("claude-3"), 64_000);
        assert_eq!(model_max_ctx("unknown-model-xyz"), 64_000);
        assert_eq!(model_max_ctx("voxtral-small-24b"), 64_000);
        assert_eq!(model_max_ctx(""), 64_000);
    }

    #[test]
    fn test_model_max_ctx_prefix_matching() {
        assert_eq!(model_max_ctx("llama3-3-70b-instruct"), 131_072);
        assert_eq!(model_max_ctx("unknown-r1-70b-instruct"), 64_000);
    }
}
