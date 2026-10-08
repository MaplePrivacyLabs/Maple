//! Port of `packages/ai/src/utils/sanitize-unicode.ts`.

/// Remove unpaired surrogate code units while preserving every valid pair.
pub fn sanitize_surrogates(text: impl Into<super::js_value::JsString>) -> String {
    sanitize_utf16(&text.into().as_utf16())
}

/// UTF-16 boundary counterpart for callers receiving raw JavaScript code units.
/// Valid pairs become their scalar value; unpaired high and low surrogates are removed.
pub fn sanitize_utf16(text: &[u16]) -> String {
    char::decode_utf16(text.iter().copied())
        .filter_map(Result::ok)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_utf16_retains_pairs_and_drops_only_unpaired_surrogates() {
        assert_eq!(
            sanitize_utf16(&[0x61, 0xd83d, 0xde48, 0xd83d, 0x62, 0xde48]),
            "a🙈b"
        );
        assert_eq!(sanitize_surrogates("Hello 🙈 World"), "Hello 🙈 World");
        assert_eq!(
            sanitize_surrogates(super::super::js_value::JsString::from_utf16(vec![
                0xd800, 0x61, 0xdc00
            ])),
            "a"
        );
    }
}
