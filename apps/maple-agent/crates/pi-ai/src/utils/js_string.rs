//! JavaScript strings, including unpaired UTF-16 surrogates.
//!
//! Valid Unicode uses a compact Rust `String`. An invalid UTF-16 sequence keeps
//! its exact code units instead of replacing lone surrogates with U+FFFD. All
//! constructors and mutations keep that representation canonical.

use std::fmt;
use std::iter::{Copied, FusedIterator};
use std::slice::Iter;
use std::str::EncodeUtf16;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum Representation {
    Utf8(String),
    Utf16(Vec<u16>),
}

/// An exact UTF-16 string without an implicit lossy conversion to Rust text.
///
/// Equality and hashing are canonical: constructing valid text through UTF-16
/// produces the same value as constructing it through a Rust string.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct JsString(Representation);

impl Ord for JsString {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.units().cmp(other.units())
    }
}

impl PartialOrd for JsString {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl JsString {
    pub fn from_utf16(units: impl Into<Vec<u16>>) -> Self {
        let units = units.into();
        match String::from_utf16(&units) {
            Ok(text) => Self(Representation::Utf8(text)),
            Err(_) => Self(Representation::Utf16(units)),
        }
    }

    /// Return Rust text only when every surrogate is paired.
    pub fn as_str(&self) -> Option<&str> {
        match &self.0 {
            Representation::Utf8(text) => Some(text),
            Representation::Utf16(_) => None,
        }
    }

    pub fn as_utf16(&self) -> Vec<u16> {
        self.units().collect()
    }

    pub fn utf16_len(&self) -> usize {
        match &self.0 {
            Representation::Utf8(text) => text.encode_utf16().count(),
            Representation::Utf16(units) => units.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match &self.0 {
            Representation::Utf8(text) => text.is_empty(),
            Representation::Utf16(units) => units.is_empty(),
        }
    }

    /// Slice by UTF-16 code units, preserving a surrogate split by either
    /// boundary. Bounds are clamped; an end before the start yields an empty
    /// string. Callers normalize JavaScript's negative indexes before this API.
    pub fn slice(&self, start: usize, end: usize) -> Self {
        let length = self.utf16_len();
        let start = start.min(length);
        let end = end.min(length);
        if start >= end {
            return Self::default();
        }
        Self::from_utf16(
            self.units()
                .skip(start)
                .take(end - start)
                .collect::<Vec<_>>(),
        )
    }

    pub fn push_str(&mut self, text: &str) {
        if let Representation::Utf8(current) = &mut self.0 {
            current.push_str(text);
            return;
        }
        let mut units = self.as_utf16();
        units.extend(text.encode_utf16());
        *self = Self::from_utf16(units);
    }

    /// Concatenate exact code units. Two lone surrogates that form a pair at
    /// the join become ordinary Rust text when the result is wholly valid.
    pub fn push(&mut self, text: &Self) {
        if let (Representation::Utf8(current), Representation::Utf8(text)) = (&mut self.0, &text.0)
        {
            current.push_str(text);
            return;
        }
        let mut units = self.as_utf16();
        units.extend(text.units());
        *self = Self::from_utf16(units);
    }

    pub fn join<'a>(strings: impl IntoIterator<Item = &'a Self>, separator: &str) -> Self {
        let mut strings = strings.into_iter();
        let Some(first) = strings.next() else {
            return Self::default();
        };
        let mut result = first.clone();
        for string in strings {
            result.push_str(separator);
            result.push(string);
        }
        result
    }

    /// Iterate over exact code units without allocating.
    pub fn units(&self) -> JsStringUnits<'_> {
        JsStringUnits(match &self.0 {
            Representation::Utf8(text) => Units::Utf8(text.encode_utf16()),
            Representation::Utf16(units) => Units::Utf16(units.iter().copied()),
        })
    }

    /// Recover Rust text without replacing invalid code units. On failure the
    /// original value is returned intact.
    pub fn into_string(self) -> Result<String, Self> {
        match self.0 {
            Representation::Utf8(text) => Ok(text),
            Representation::Utf16(units) => Err(Self(Representation::Utf16(units))),
        }
    }

    /// Explicit display-only conversion. Each unpaired surrogate is replaced
    /// with U+FFFD; callers must keep the original value for model data.
    pub fn to_string_lossy(&self) -> String {
        match &self.0 {
            Representation::Utf8(text) => text.clone(),
            Representation::Utf16(units) => String::from_utf16_lossy(units),
        }
    }

    /// Project each UTF-16 code unit to one Rust character for ASCII-literal
    /// regex patterns. Surrogate units each become U+E000, so a supplementary
    /// character occupies two positions and a lone surrogate occupies one.
    ///
    /// This is only for patterns made from ASCII literals and JavaScript's
    /// dot, line-terminator, or whitespace rules. It does not preserve arbitrary
    /// Unicode classes or non-ASCII literal matching. The source is unchanged.
    pub fn ascii_pattern_text(&self) -> String {
        self.units()
            .map(|unit| char::from_u32(u32::from(unit)).unwrap_or('\u{e000}'))
            .collect()
    }
}

impl Default for JsString {
    fn default() -> Self {
        Self(Representation::Utf8(String::new()))
    }
}

impl From<&str> for JsString {
    fn from(text: &str) -> Self {
        Self(Representation::Utf8(text.to_owned()))
    }
}

impl From<String> for JsString {
    fn from(text: String) -> Self {
        Self(Representation::Utf8(text))
    }
}

impl From<&String> for JsString {
    fn from(text: &String) -> Self {
        Self::from(text.as_str())
    }
}

impl From<&JsString> for JsString {
    fn from(text: &JsString) -> Self {
        text.clone()
    }
}

impl AsRef<JsString> for JsString {
    fn as_ref(&self) -> &JsString {
        self
    }
}

impl PartialEq<str> for JsString {
    fn eq(&self, text: &str) -> bool {
        self.as_str().is_some_and(|value| value == text)
    }
}

impl PartialEq<&str> for JsString {
    fn eq(&self, text: &&str) -> bool {
        self == *text
    }
}

impl PartialEq<String> for JsString {
    fn eq(&self, text: &String) -> bool {
        self == text.as_str()
    }
}

impl PartialEq<JsString> for str {
    fn eq(&self, text: &JsString) -> bool {
        text == self
    }
}

impl PartialEq<JsString> for &str {
    fn eq(&self, text: &JsString) -> bool {
        text == *self
    }
}

impl PartialEq<JsString> for String {
    fn eq(&self, text: &JsString) -> bool {
        text == self.as_str()
    }
}

impl Serialize for JsString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.0 {
            Representation::Utf8(text) => serializer.serialize_str(text),
            Representation::Utf16(_) => {
                // RawValue preserves the JSON escape instead of asking serde's
                // Rust-string interface to hold an unpaired surrogate. The
                // JsValue serializer recognizes this same token and parses it
                // through the UTF-16-preserving JavaScript parser.
                let raw = serde_json::value::RawValue::from_string(super::js_json::quote(self))
                    .map_err(serde::ser::Error::custom)?;
                raw.serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for JsString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StringVisitor;

        impl<'de> serde::de::Visitor<'de> for StringVisitor {
            type Value = JsString;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JavaScript string")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(JsString::from(value))
            }

            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(JsString::from(value))
            }

            fn visit_bytes<E: serde::de::Error>(self, value: &[u8]) -> Result<Self::Value, E> {
                // The bridge uses this intrinsic byte form for map identifiers
                // buffered by serde's flatten visitor, which does not preserve
                // a newtype event. JSON arrays and objects cannot emit bytes.
                let value = value.strip_prefix(&[0xff, 0xff]).ok_or_else(|| {
                    E::custom("UTF-16 identifier bytes are missing their intrinsic prefix")
                })?;
                if !value.len().is_multiple_of(2) {
                    return Err(E::custom(
                        "UTF-16 identifier bytes must have an even length",
                    ));
                }
                Ok(JsString::from_utf16(
                    value
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| u16::from_le_bytes(*pair))
                        .collect::<Vec<_>>(),
                ))
            }

            fn visit_byte_buf<E: serde::de::Error>(self, value: Vec<u8>) -> Result<Self::Value, E> {
                self.visit_bytes(&value)
            }

            fn visit_newtype_struct<D: Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<Self::Value, D::Error> {
                // The JsValue bridge emits an intrinsic newtype containing
                // exact UTF-16 units. This cannot collide with a JSON object
                // or array: ordinary JSON has no newtype event, and this outer
                // visitor deliberately does not accept sequences.
                Vec::<u16>::deserialize(deserializer).map(JsString::from_utf16)
            }
        }

        deserializer.deserialize_any(StringVisitor)
    }
}

#[derive(Clone, Debug)]
enum Units<'a> {
    Utf8(EncodeUtf16<'a>),
    Utf16(Copied<Iter<'a, u16>>),
}

/// A borrowing iterator returned by [`JsString::units`].
#[derive(Clone, Debug)]
pub struct JsStringUnits<'a>(Units<'a>);

impl Iterator for JsStringUnits<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            Units::Utf8(units) => units.next(),
            Units::Utf16(units) => units.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            Units::Utf8(units) => units.size_hint(),
            Units::Utf16(units) => units.size_hint(),
        }
    }
}

impl FusedIterator for JsStringUnits<'_> {}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    use super::JsString;

    #[test]
    fn ordering_is_lexicographic_utf16_including_lone_surrogates() {
        let mut strings = [
            JsString::from("\u{ffff}"),
            JsString::from("🙈"),
            JsString::from_utf16(vec![0xd800]),
            JsString::from("a"),
        ];
        strings.sort();
        assert_eq!(
            strings.iter().map(JsString::as_utf16).collect::<Vec<_>>(),
            vec![vec![0x61], vec![0xd800], vec![0xd83d, 0xde48], vec![0xffff],]
        );
        assert_eq!(
            JsString::from("🙈").cmp(&JsString::from_utf16(vec![0xd83d, 0xde48])),
            std::cmp::Ordering::Equal
        );
    }

    fn hash(value: &JsString) -> u64 {
        let mut state = DefaultHasher::new();
        value.hash(&mut state);
        state.finish()
    }

    #[test]
    fn valid_utf16_is_canonical_with_rust_text() {
        let text = "a\0é😀z";
        let from_text = JsString::from(text);
        let from_units = JsString::from_utf16(text.encode_utf16().collect::<Vec<_>>());
        assert_eq!(from_units, from_text);
        assert_eq!(hash(&from_units), hash(&from_text));
        assert_eq!(from_units.as_str(), Some(text));
        assert_eq!(from_units.utf16_len(), 6);
        assert_eq!(from_units.into_string().unwrap(), text);
        assert_eq!(JsString::from_utf16(Vec::new()), JsString::default());
    }

    #[test]
    fn unpaired_surrogates_remain_exact_and_never_equal_replacement_text() {
        for units in [vec![0xd800], vec![0xdc00], vec![0xd800, 0x61, 0xdc00]] {
            let text = JsString::from_utf16(units.clone());
            assert_eq!(text.as_str(), None);
            assert_eq!(text.as_utf16(), units);
            assert_eq!(text.units().collect::<Vec<_>>(), units);
            assert_eq!(text.utf16_len(), units.len());
            assert!(!text.is_empty());
            assert_eq!(text.clone().into_string().unwrap_err(), text);
            assert_ne!(text, String::from_utf16_lossy(&units));
        }
    }

    #[test]
    fn slicing_uses_code_units_and_preserves_split_pairs() {
        let text = JsString::from("A😀B");
        assert_eq!(text.utf16_len(), 4);
        assert_eq!(text.slice(1, 2).as_utf16(), [0xd83d]);
        assert_eq!(text.slice(2, 3).as_utf16(), [0xde00]);
        assert_eq!(text.slice(1, 3), "😀");
        assert_eq!(text.slice(0, 2).as_utf16(), [0x41, 0xd83d]);
        assert_eq!(text.slice(3, usize::MAX), "B");
        assert!(text.slice(usize::MAX, usize::MAX).is_empty());
        assert!(text.slice(4, 0).is_empty());
        assert!(text.slice(2, 2).is_empty());
    }

    #[test]
    fn joining_surrogate_halves_restores_canonical_unicode() {
        let mut text = JsString::from_utf16(vec![0xd83d]);
        text.push(&JsString::from_utf16(vec![0xde00]));
        assert_eq!(text.as_str(), Some("😀"));
        assert_eq!(text, JsString::from("😀"));
        assert_eq!(hash(&text), hash(&JsString::from("😀")));
        text.push_str("!");
        assert_eq!(text.into_string().unwrap(), "😀!");
    }

    #[test]
    fn concatenation_preserves_unpaired_units_around_rust_text() {
        let mut text = JsString::from("A");
        text.push(&JsString::from_utf16(vec![0xd800]));
        text.push_str("é");
        text.push(&JsString::from_utf16(vec![0xdc00]));
        assert_eq!(text.as_utf16(), [0x41, 0xd800, 0xe9, 0xdc00]);
        assert_eq!(text.as_str(), None);
        assert_eq!(text.slice(2, 3), "é");
    }

    #[test]
    fn join_keeps_surrogates_and_places_separators_between_values() {
        let values = [
            JsString::from_utf16(vec![0xd83d]),
            JsString::from_utf16(vec![0xde00]),
        ];
        assert_eq!(JsString::join(&values, ""), "😀");
        assert_eq!(
            JsString::join(&values, ",").as_utf16(),
            [0xd83d, 0x2c, 0xde00]
        );
        assert!(JsString::join(std::iter::empty(), ",").is_empty());
        assert_eq!(JsString::join([&values[0]], ","), values[0]);
    }

    #[test]
    fn rust_string_conversions_and_comparisons_are_exact() {
        let owned = String::from("hello 😀");
        let text = JsString::from(&owned);
        assert_eq!(JsString::from(owned.clone()), text);
        assert_eq!(JsString::from(&text), text);
        assert_eq!(text, owned);
        assert_eq!(owned, text);
        assert_eq!(text, owned.as_str());
        assert_eq!(owned.as_str(), text);
        assert_ne!(text, "hello");
    }

    #[test]
    fn code_unit_iterator_can_be_cloned_and_is_fused() {
        for text in [
            JsString::from("A😀"),
            JsString::from_utf16(vec![0x41, 0xd800]),
        ] {
            let mut units = text.units();
            assert_eq!(units.next(), Some(0x41));
            let remaining = units.clone().collect::<Vec<_>>();
            assert_eq!(units.by_ref().collect::<Vec<_>>(), remaining);
            assert_eq!(units.next(), None);
            assert_eq!(units.next(), None);
        }
    }

    #[test]
    fn serde_handles_unicode_and_serializes_lone_surrogates_exactly() {
        let text = JsString::from("hello 😀\0");
        let encoded = serde_json::to_string(&text).unwrap();
        assert_eq!(serde_json::from_str::<JsString>(&encoded).unwrap(), text);
        let invalid = JsString::from_utf16(vec![0xd800]);
        assert_eq!(serde_json::to_string(&invalid).unwrap(), r#""\ud800""#);
        assert!(serde_json::from_str::<JsString>(r#""\ud800""#).is_err());
        assert!(serde_json::from_str::<JsString>("[55296]").is_err());
        assert!(serde_json::from_str::<JsString>(r#"{"$pi::JsString":[55296]}"#).is_err());
    }

    #[test]
    fn intrinsic_identifier_bytes_are_utf16_little_endian() {
        use serde::Deserialize;
        use serde::de::value::{BorrowedBytesDeserializer, Error};

        let input = BorrowedBytesDeserializer::<Error>::new(&[0xff, 0xff, 0x00, 0xd8, 0x61, 0x00]);
        assert_eq!(
            JsString::deserialize(input).unwrap().as_utf16(),
            [0xd800, 0x61]
        );
        let odd = BorrowedBytesDeserializer::<Error>::new(&[0xff, 0xff, 0x00]);
        assert!(JsString::deserialize(odd).is_err());
        let missing_prefix = BorrowedBytesDeserializer::<Error>::new(&[0x00, 0xd8]);
        assert!(JsString::deserialize(missing_prefix).is_err());
    }

    #[test]
    fn regex_projection_preserves_utf16_positions_without_changing_source() {
        let lone = JsString::from_utf16(vec![0x61, 0xd800, 0x62]);
        let pair = JsString::from("a😀b");
        assert_eq!(lone.ascii_pattern_text(), "a\u{e000}b");
        assert_eq!(pair.ascii_pattern_text(), "a\u{e000}\u{e000}b");
        assert_eq!(lone.as_utf16(), [0x61, 0xd800, 0x62]);
        assert_eq!(pair.as_str(), Some("a😀b"));
        let pattern = regex::Regex::new("rate.?limit").unwrap();
        let mut lone_limit = JsString::from("rate");
        lone_limit.push(&JsString::from_utf16(vec![0xd800]));
        lone_limit.push_str("limit");
        assert!(pattern.is_match(&lone_limit.ascii_pattern_text()));
        assert!(!pattern.is_match(&JsString::from("rate😀limit").ascii_pattern_text()));
        assert_eq!(
            JsString::from("é\n\r\u{2028}\u{2029}").ascii_pattern_text(),
            "é\n\r\u{2028}\u{2029}"
        );
    }

    #[test]
    fn lossy_display_is_explicit_and_keeps_the_original_units() {
        let text = JsString::from_utf16(vec![0xd800, 0x61, 0xdc00]);
        assert_eq!(text.to_string_lossy(), "�a�");
        assert_eq!(text.as_utf16(), [0xd800, 0x61, 0xdc00]);
        assert_eq!(JsString::from("😀").to_string_lossy(), "😀");
    }
}
