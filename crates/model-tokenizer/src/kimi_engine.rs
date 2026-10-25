//! Kimi K3's pinned rank table and reserved-token policy.
//!
//! Regex and reserved-ID construction follow tokenization_kimi.py at
//! moonshotai/Kimi-K3@a590ce090cb049c93a33dfe8c208ec652aa20503.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use rustc_hash::FxHashMap;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use tiktoken_rs::CoreBPE;

use crate::{Error, SpecialTokenPolicy};

const ASSET_ID: &str = "kimi_k3";
const BASE_TOKENS: usize = 163_584;
const RESERVED_TOKENS: usize = 256;
const PATTERN: &str = concat!(
    r"[\p{Han}]+|",
    r"[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]*[\p{Ll}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]+(?i:'s|'t|'re|'ve|'m|'ll|'d)?|",
    r"[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]+[\p{Ll}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]*(?i:'s|'t|'re|'ve|'m|'ll|'d)?|",
    r"\p{N}{1,3}|",
    r" ?[^\s\p{L}\p{N}]+[\r\n]*|",
    r"\s*[\r\n]+|",
    r"\s+(?!\S)|",
    r"\s+"
);

#[derive(Deserialize)]
struct Config {
    added_tokens_decoder: BTreeMap<u32, AddedToken>,
}

#[derive(Deserialize)]
struct AddedToken {
    content: String,
}

fn invalid(reason: &'static str) -> Error {
    Error::InvalidAsset {
        asset_id: ASSET_ID,
        reason,
    }
}

pub(super) fn load(bytes: &[u8]) -> Result<CoreBPE, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("rank table is not UTF-8"))?;
    let mut ranks = FxHashMap::default();
    let mut rank_ids = HashSet::new();
    for line in text.lines() {
        let mut columns = line.split_whitespace();
        let encoded = columns.next().ok_or_else(|| invalid("empty rank line"))?;
        let rank = columns
            .next()
            .ok_or_else(|| invalid("missing rank"))?
            .parse::<u32>()
            .map_err(|_| invalid("invalid rank"))?;
        if columns.next().is_some() {
            return Err(invalid("extra rank columns"));
        }
        let token = STANDARD
            .decode(encoded)
            .map_err(|_| invalid("invalid base64 token"))?;
        if ranks.insert(token, rank).is_some() || !rank_ids.insert(rank) {
            return Err(invalid("duplicate rank or token"));
        }
    }
    if ranks.len() != BASE_TOKENS || !(0..BASE_TOKENS as u32).all(|id| rank_ids.contains(&id)) {
        return Err(invalid("unexpected rank count or range"));
    }
    let config: Config =
        serde_json::from_str(include_str!("../assets/kimi-k3-tokenizer-config.json"))
            .map_err(|_| invalid("invalid reserved-token configuration"))?;
    let mut specials = FxHashMap::default();
    for id in BASE_TOKENS..BASE_TOKENS + RESERVED_TOKENS {
        let token = config
            .added_tokens_decoder
            .get(&(id as u32))
            .map(|token| token.content.clone())
            .unwrap_or_else(|| format!("<|reserved_token_{id}|>"));
        if specials.insert(token, id as u32).is_some() {
            return Err(invalid("duplicate reserved-token text"));
        }
    }
    CoreBPE::new(ranks, specials, PATTERN).map_err(|_| invalid("invalid tokenizer regex"))
}

/// Match Kimi's wrapper as well as its BPE. Its chunk boundaries are part of
/// tokenization, including when a reserved-marker string crosses a boundary.
pub(super) fn encode(
    tokenizer: &CoreBPE,
    text: &str,
    policy: SpecialTokenPolicy,
) -> Result<Vec<u32>, Error> {
    let allowed = if policy == SpecialTokenPolicy::Ordinary {
        HashSet::new()
    } else {
        tokenizer.special_tokens()
    };
    let mut output = Vec::new();
    for chunk in chunks(text) {
        let (tokens, _) = tokenizer
            .encode(chunk, &allowed)
            .map_err(|_| Error::EncodingFailed { asset_id: ASSET_ID })?;
        output.extend(tokens);
    }
    Ok(output)
}

fn python_is_space(character: char) -> bool {
    // Python str.isspace includes the four ASCII information separators in
    // addition to Unicode White_Space (Rust char::is_whitespace).
    character.is_whitespace() || matches!(character, '\u{001c}'..='\u{001f}')
}

fn split_runs<'a>(text: &'a str, output: &mut Vec<&'a str>) {
    let mut run_length = 0usize;
    let mut run_is_space = text.chars().next().is_some_and(python_is_space);
    let mut slice_start = 0usize;
    for (byte, character) in text.char_indices() {
        let is_space = python_is_space(character);
        if run_is_space != is_space {
            run_length = 1;
            run_is_space = is_space;
        } else {
            run_length += 1;
            if run_length > 25_000 {
                output.push(&text[slice_start..byte]);
                slice_start = byte;
                run_length = 1;
            }
        }
    }
    output.push(&text[slice_start..]);
}

fn chunks(text: &str) -> Vec<&str> {
    let mut output = Vec::new();
    let mut outer_start = 0usize;
    for (index, (byte, _)) in text.char_indices().enumerate() {
        if index > 0 && index % 400_000 == 0 {
            split_runs(&text[outer_start..byte], &mut output);
            outer_start = byte;
        }
    }
    if outer_start < text.len() {
        split_runs(&text[outer_start..], &mut output);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunking_uses_python_character_and_whitespace_semantics() {
        let text = "界".repeat(25_001);
        let chunks = chunks(&text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), 25_000);
        assert_eq!(chunks[1], "界");
        assert!(python_is_space('\u{001c}'));
        assert!(!python_is_space('\u{200b}'));
    }
}
