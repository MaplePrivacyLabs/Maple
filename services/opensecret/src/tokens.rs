//! Thin wrapper around tiktoken for fast, cached token counting.

use once_cell::sync::Lazy;
use tiktoken_rs::cl100k_base;

use crate::model_config::model_context_window;

/// Cached encoder – created once per process.
/// No mutex needed: CoreBPE is immutable and thread-safe.
static ENCODER: Lazy<tiktoken_rs::CoreBPE> =
    Lazy::new(|| cl100k_base().expect("init cl100k encoder"));

/// Longest run without whitespace that is encoded in one go. BPE merging is quadratic in
/// the length of a run: a 200k-character "word" takes about 16 s unsplit and under 100 ms in
/// pieces this size. Splitting only inside such runs leaves every ordinary text's count
/// unchanged and overcounts a pathological run by at most a token per piece.
const MAX_RUN_BYTES: usize = 1024;

/// Count tokens for a piece of UTF‑8 text.
pub fn count_tokens(text: &str) -> usize {
    split_long_runs(text)
        .map(|piece| ENCODER.encode_with_special_tokens(piece).len())
        .sum()
}

/// Yields `text` whole unless a whitespace-free run exceeds `MAX_RUN_BYTES`, in which case
/// the run is cut into pieces of about that size, on char boundaries.
fn split_long_runs(text: &str) -> impl Iterator<Item = &str> {
    let mut pieces = Vec::new();
    let mut piece_start = 0;
    let mut run_start = 0;
    for (index, ch) in text.char_indices() {
        if ch.is_whitespace() {
            run_start = index + ch.len_utf8();
        } else if index - run_start >= MAX_RUN_BYTES {
            pieces.push(&text[piece_start..index]);
            piece_start = index;
            run_start = index;
        }
    }
    pieces.push(&text[piece_start..]);
    pieces.into_iter()
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
    fn long_runs_are_counted_in_bounded_pieces() {
        // Unsplit, this one "word" takes seconds (quadratic merging); in pieces it is fast and
        // the count stays within a token per piece of the exact one.
        let run = "x".repeat(100_000);
        let count = count_tokens(&run);
        assert!((12_000..=13_000).contains(&count), "{count}");
        // Ordinary text is encoded whole, so its count is exactly the encoder's.
        let prose = "The quick brown fox jumps over the lazy dog. ".repeat(200);
        assert_eq!(
            count_tokens(&prose),
            ENCODER.encode_with_special_tokens(&prose).len()
        );
        assert_eq!(split_long_runs(&prose).count(), 1);
        assert_eq!(split_long_runs("").count(), 1);
        // Pieces end on char boundaries and cover the text.
        let wide = "é".repeat(2_000);
        let pieces: Vec<&str> = split_long_runs(&wide).collect();
        assert_eq!(pieces.iter().map(|p| p.len()).sum::<usize>(), wide.len());
        assert!(pieces.iter().all(|p| p.len() <= MAX_RUN_BYTES + 4));
        assert!(pieces.len() >= 3);
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
        assert_eq!(model_max_ctx("gemma4-31b"), 262_144);
        assert_eq!(model_max_ctx("glm-5-3"), 262_144);
        assert_eq!(model_max_ctx("glm-5-3-flash"), 1_048_576);
        assert_eq!(model_max_ctx("kimi-k3"), 262_144);
        assert_eq!(model_max_ctx("deepseek-v4-1-flash"), 1_048_576);
        assert_eq!(model_max_ctx("auto:quick"), 1_048_576);
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
