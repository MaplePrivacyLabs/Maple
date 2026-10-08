//! Pi v1.0.4 `api/openai-prompt-cache.ts`.
use crate::types::JsString;
pub const OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH: usize = 64;
/// JavaScript Array.from iterates Unicode code points, retaining unpaired units.
pub fn clamp_open_ai_prompt_cache_key(key: Option<&JsString>) -> Option<JsString> {
    key.map(|key| {
        let units = key.as_utf16();
        let (mut offset, mut count) = (0, 0);
        while offset < units.len() && count < OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH {
            let paired = (0xd800..=0xdbff).contains(&units[offset])
                && units
                    .get(offset + 1)
                    .is_some_and(|u| (0xdc00..=0xdfff).contains(u));
            offset += if paired { 2 } else { 1 };
            count += 1;
        }
        key.slice(0, offset)
    })
}
