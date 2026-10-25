//! Offline tokenization with reviewed, model-specific assets.
//!
//! Text counts are the exact pinned text pipeline, **not** a count of an entire
//! chat request or of image/audio expansion. A serving-profile renderer must
//! supply roles, tool schemas, thinking policy and generation markers first.
//! No model-name guessing, network retrieval or generic cl100k fallback occurs.

use std::sync::OnceLock;

use tokenizers::Tokenizer;

mod deepseek;
mod harmony;
mod kimi;
mod kimi_engine;
pub mod render;

pub use render::{
    count_chat_request, render_chat, PromptAccuracy, PromptCount, PromptSegment, RenderContext,
    RenderedPrompt, RendererId,
};

/// A reviewed catalog model. Auto selectors deliberately have no tokenizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenizerId {
    DeepSeekV41Flash,
    Glm53,
    Glm53Flash,
    KimiK3,
    GptOss120b,
    GptOssSafeguard120b,
    Gemma431b,
    Llama3370b,
}

impl TokenizerId {
    pub const ALL: [Self; 8] = [
        Self::DeepSeekV41Flash,
        Self::Glm53,
        Self::Glm53Flash,
        Self::KimiK3,
        Self::GptOss120b,
        Self::GptOssSafeguard120b,
        Self::Gemma431b,
        Self::Llama3370b,
    ];

    /// Resolve only exact public catalog IDs; aliases and Auto are unsupported.
    pub fn from_model(model: &str) -> Result<Self, Error> {
        Self::ALL
            .into_iter()
            .find(|id| id.model() == model)
            .ok_or(Error::UnsupportedModel)
    }

    pub const fn model(self) -> &'static str {
        match self {
            Self::DeepSeekV41Flash => "deepseek-v4-1-flash",
            Self::Glm53 => "glm-5-3",
            Self::Glm53Flash => "glm-5-3-flash",
            Self::KimiK3 => "kimi-k3",
            Self::GptOss120b => "gpt-oss-120b",
            Self::GptOssSafeguard120b => "gpt-oss-safeguard-120b",
            Self::Gemma431b => "gemma4-31b",
            Self::Llama3370b => "llama3-3-70b",
        }
    }

    const fn asset_id(self) -> &'static str {
        match self {
            Self::DeepSeekV41Flash => "deepseek_v41_flash",
            Self::Glm53 | Self::Glm53Flash => "glm53",
            Self::KimiK3 => "kimi_k3",
            Self::GptOss120b | Self::GptOssSafeguard120b => "gpt_oss120b",
            Self::Gemma431b => "gemma4_31b",
            Self::Llama3370b => "llama33_70b",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenizerEngine {
    HuggingFace,
    TiktokenRanks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accuracy {
    /// Exact pinned text pipeline; excludes chat rendering and media expansion.
    ExactTextPipeline,
}

#[derive(Debug, Clone, Copy)]
pub struct TokenizerSource {
    pub model: &'static str,
    pub repository: &'static str,
    pub revision: &'static str,
    pub url: &'static str,
    pub license: &'static str,
    pub license_url: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct TokenizerProvenance {
    pub model: &'static str,
    pub asset_id: &'static str,
    pub asset_sha256: &'static str,
    pub engine: TokenizerEngine,
    pub accuracy: Accuracy,
    pub source: TokenizerSource,
}

/// Recognition of structural token strings is distinct from adding BOS/EOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialTokenPolicy {
    /// Match the model's raw encode pipeline without adding a BOS/EOS wrapper.
    RawPipeline,
    /// Encode literal user/tool text without recognizing reserved specials.
    Ordinary,
    /// Permit the model's reserved structural tokens in renderer-owned text.
    AllowAll,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    #[error("unsupported or unresolved tokenizer model")]
    UnsupportedModel,
    #[error("invalid pinned tokenizer asset {asset_id}: {reason}")]
    InvalidAsset {
        asset_id: &'static str,
        reason: &'static str,
    },
    #[error("text tokenization failed for pinned asset {asset_id}")]
    EncodingFailed { asset_id: &'static str },
}

/// Sanitized rendering errors never contain message or tool argument content.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RenderError {
    #[error("unsupported prompt feature: {0}")]
    Unsupported(&'static str),
    #[error("invalid prompt request: {0}")]
    InvalidRequest(&'static str),
    #[error("prompt tokenizer failed: {0}")]
    Tokenizer(&'static str),
}

struct EmbeddedAsset {
    id: &'static str,
    sha256: &'static str,
    engine: TokenizerEngine,
    bytes: &'static [u8],
    sources: &'static [TokenizerSource],
}

include!(concat!(env!("OUT_DIR"), "/embedded_assets.rs"));

enum Engine {
    Hf(Box<HfEngine>),
    Kimi(tiktoken_rs::CoreBPE),
}

struct HfEngine {
    raw: Tokenizer,
    ordinary: OnceLock<Tokenizer>,
}

static ENGINES: [OnceLock<Result<Engine, Error>>; 6] = [const { OnceLock::new() }; 6];

fn embedded(id: TokenizerId) -> (usize, &'static EmbeddedAsset) {
    let found = EMBEDDED_ASSETS
        .iter()
        .enumerate()
        .find(|(_, asset)| asset.id == id.asset_id())
        .expect("reviewed model has a checked build-time manifest entry");
    debug_assert_eq!(found.1.sha256, provenance(id).asset_sha256);
    debug_assert!(found
        .1
        .sources
        .iter()
        .any(|source| source.model == id.model()));
    found
}

fn engine(id: TokenizerId) -> Result<&'static Engine, Error> {
    let (index, asset) = embedded(id);
    let result = ENGINES[index].get_or_init(|| match asset.engine {
        TokenizerEngine::HuggingFace => {
            let mut tokenizer =
                Tokenizer::from_bytes(asset.bytes).map_err(|_| Error::InvalidAsset {
                    asset_id: asset.id,
                    reason: "HF JSON could not be loaded",
                })?;
            // Admission must observe the full length, never silent truncation.
            tokenizer
                .with_truncation(None)
                .map_err(|_| Error::InvalidAsset {
                    asset_id: asset.id,
                    reason: "HF truncation could not be disabled",
                })?;
            tokenizer.with_padding(None);
            Ok(Engine::Hf(Box::new(HfEngine {
                raw: tokenizer,
                ordinary: OnceLock::new(),
            })))
        }
        TokenizerEngine::TiktokenRanks => kimi_engine::load(asset.bytes).map(Engine::Kimi),
    });
    result.as_ref().map_err(Clone::clone)
}

pub fn provenance(id: TokenizerId) -> &'static TokenizerProvenance {
    MODEL_PROVENANCE
        .iter()
        .find(|item| item.model == id.model())
        .expect("reviewed model has checked build-time provenance")
}

/// Encode with the pinned raw text pipeline and no automatic BOS/EOS wrapper.
/// This may recognize literal special-token strings; it is not an ordinary-text
/// guarantee, and is not a chat-prompt count.
pub fn encode_text(id: TokenizerId, text: &str) -> Result<Vec<u32>, Error> {
    encode_segment(id, text, SpecialTokenPolicy::RawPipeline)
}

pub fn count_text(id: TokenizerId, text: &str) -> Result<usize, Error> {
    count_segment(id, text, SpecialTokenPolicy::RawPipeline)
}

pub fn count_segment(
    id: TokenizerId,
    text: &str,
    policy: SpecialTokenPolicy,
) -> Result<usize, Error> {
    match engine(id)? {
        Engine::Hf(hf) => hf_for_policy(&hf.raw, &hf.ordinary, policy)
            .encode_fast(text, false)
            .map(|encoding| encoding.get_ids().len())
            .map_err(|_| Error::EncodingFailed {
                asset_id: id.asset_id(),
            }),
        Engine::Kimi(tokenizer) => Ok(kimi_engine::encode(tokenizer, text, policy)?.len()),
    }
}

fn hf_for_policy<'a>(
    raw: &'a Tokenizer,
    ordinary: &'a OnceLock<Tokenizer>,
    policy: SpecialTokenPolicy,
) -> &'a Tokenizer {
    if policy == SpecialTokenPolicy::Ordinary {
        ordinary.get_or_init(|| {
            let mut tokenizer = raw.clone();
            // HF calls this setting "encode_special_tokens": true means
            // ignore special-token matches and split them as literal text.
            tokenizer.set_encode_special_tokens(true);
            tokenizer
        })
    } else {
        raw
    }
}

pub fn encode_segment(
    id: TokenizerId,
    text: &str,
    policy: SpecialTokenPolicy,
) -> Result<Vec<u32>, Error> {
    match engine(id)? {
        Engine::Hf(hf) => hf_for_policy(&hf.raw, &hf.ordinary, policy)
            .encode_fast(text, false)
            .map(|encoding| encoding.get_ids().to_vec())
            .map_err(|_| Error::EncodingFailed {
                asset_id: id.asset_id(),
            }),
        Engine::Kimi(tokenizer) => kimi_engine::encode(tokenizer, text, policy),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_explicit_and_complete() {
        for id in TokenizerId::ALL {
            assert_eq!(TokenizerId::from_model(id.model()).unwrap(), id);
            let provenance = provenance(id);
            assert_eq!(provenance.model, id.model());
            assert_eq!(provenance.source.revision.len(), 40);
            assert_eq!(provenance.asset_sha256.len(), 64);
        }
        for model in [
            "",
            "auto:quick",
            "auto:powerful",
            "unknown",
            "deepseek-v4-1-flash-1017",
        ] {
            assert!(matches!(
                TokenizerId::from_model(model),
                Err(Error::UnsupportedModel)
            ));
        }
    }

    #[test]
    fn equal_raw_assets_have_equal_counts_but_keep_model_provenance() {
        for (left, right) in [
            (TokenizerId::Glm53, TokenizerId::Glm53Flash),
            (TokenizerId::GptOss120b, TokenizerId::GptOssSafeguard120b),
        ] {
            assert_eq!(
                provenance(left).asset_sha256,
                provenance(right).asset_sha256
            );
            assert_ne!(
                provenance(left).source.model,
                provenance(right).source.model
            );
            assert_eq!(
                encode_text(left, "fn main() { println!(\"你好 🌲\"); }\n").unwrap(),
                encode_text(right, "fn main() { println!(\"你好 🌲\"); }\n").unwrap()
            );
        }
    }

    #[test]
    fn all_models_load_and_reuse_cached_engines() {
        for id in TokenizerId::ALL {
            assert_eq!(count_text(id, "").unwrap(), 0);
            let first = engine(id).unwrap() as *const Engine;
            assert_eq!(first, engine(id).unwrap() as *const Engine);
            assert!(!encode_text(id, "Hello, 世界!\n").unwrap().is_empty());
        }
    }

    #[test]
    fn kimi_structural_markers_are_not_user_text() {
        let id = TokenizerId::KimiK3;
        let structural = encode_segment(id, "<|open|>", SpecialTokenPolicy::AllowAll).unwrap();
        let ordinary = encode_segment(id, "<|open|>", SpecialTokenPolicy::Ordinary).unwrap();
        assert_eq!(structural.len(), 1);
        assert_ne!(ordinary, structural);
        assert!(ordinary.len() > 1);
    }
}
