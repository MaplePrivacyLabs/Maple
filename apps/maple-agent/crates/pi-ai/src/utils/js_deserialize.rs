//! Deserialize typed contracts without narrowing JavaScript's value domain.
//!
//! An unpaired UTF-16 surrogate is carried through Serde's newtype visitor
//! channel. Real JSON arrays and objects remain arrays and objects, so no
//! user-controlled JSON shape can masquerade as the internal string carrier.

use serde::Deserializer;
use serde::de::{
    self, DeserializeOwned, DeserializeSeed, EnumAccess, MapAccess, SeqAccess, VariantAccess,
    Visitor,
};

use super::js_value::{JsObject, JsString, JsValue, JsonConversionError};

/// Decode a contract directly from a JavaScript value. Values such as infinity,
/// negative zero and unpaired UTF-16 surrogates never pass through JSON first.
pub fn from_js_value<T: DeserializeOwned>(value: JsValue) -> Result<T, JsonConversionError> {
    T::deserialize(JsValueDeserializer(value))
}

/// Strictly parse JSON before decoding its JavaScript values. This keeps
/// overflowed binary64 numbers and escaped surrogate code units intact and
/// never repairs malformed input. Call `json_parse::parse_json` separately when
/// the caller needs to distinguish syntax errors from contract decoding errors.
pub fn from_json<T: DeserializeOwned>(input: &str) -> Result<T, JsonConversionError> {
    let value = super::json_parse::parse_json(input)
        .map_err(|error| JsonConversionError::new(error.to_string()))?;
    from_js_value(value)
}

struct JsValueDeserializer(JsValue);

impl JsValueDeserializer {
    fn number(self) -> Result<f64, JsonConversionError> {
        match self.0 {
            JsValue::Number(number) => Ok(number),
            _ => Err(JsonConversionError::new("expected a JavaScript number")),
        }
    }
}

macro_rules! signed_integer {
    ($($method:ident: $integer:ty => $visit:ident),* $(,)?) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            let number = self.number()?;
            // The upper bound is exclusive: MAX may round up to the next
            // power of two when converted to f64. Never rely on a saturating
            // float-to-integer cast to check this boundary.
            let lower = <$integer>::MIN as f64;
            if !number.is_finite() || number.fract() != 0.0 || number < lower || number >= -lower {
                return Err(JsonConversionError::new(concat!("number is outside the exact ", stringify!($integer), " domain")));
            }
            visitor.$visit(number as $integer)
        }
    )*};
}

macro_rules! unsigned_integer {
    ($($method:ident: $integer:ty => $visit:ident),* $(,)?) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            let number = self.number()?;
            let upper = 2.0_f64.powi(<$integer>::BITS as i32);
            if !number.is_finite() || number.fract() != 0.0 || number < 0.0 || number >= upper {
                return Err(JsonConversionError::new(concat!("number is outside the exact ", stringify!($integer), " domain")));
            }
            visitor.$visit(number as $integer)
        }
    )*};
}

impl<'de> Deserializer<'de> for JsValueDeserializer {
    type Error = JsonConversionError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.0 {
            JsValue::Null => visitor.visit_unit(),
            JsValue::Bool(value) => visitor.visit_bool(value),
            JsValue::Number(value) => {
                // Serde's untagged derives first buffer deserialize_any. Give
                // exact integral numbers an integer representation so integer
                // branches work after buffering. Negative zero must stay f64.
                if value.is_finite()
                    && value.fract() == 0.0
                    && !(value == 0.0 && value.is_sign_negative())
                {
                    if value >= i64::MIN as f64 && value < -(i64::MIN as f64) {
                        return visitor.visit_i64(value as i64);
                    }
                    if value >= 0.0 && value < 2.0_f64.powi(64) {
                        return visitor.visit_u64(value as u64);
                    }
                }
                visitor.visit_f64(value)
            }
            JsValue::String(value) => match value.into_string() {
                Ok(value) => visitor.visit_string(value),
                Err(value) => visitor.visit_newtype_struct(de::value::SeqDeserializer::<
                    _,
                    JsonConversionError,
                >::new(
                    value.as_utf16().into_iter()
                )),
            },
            JsValue::Array(values) => {
                let mut sequence = Sequence(values.into_iter());
                let value = visitor.visit_seq(&mut sequence)?;
                if !sequence.0.as_slice().is_empty() {
                    return Err(JsonConversionError::new("unconsumed array elements"));
                }
                Ok(value)
            }
            JsValue::Object(values) => {
                let mut map = Map {
                    entries: values.into_iter(),
                    pending: None,
                };
                let value = visitor.visit_map(&mut map)?;
                if map.pending.is_some() || !map.entries.as_slice().is_empty() {
                    return Err(JsonConversionError::new("unconsumed object properties"));
                }
                Ok(value)
            }
        }
    }

    signed_integer!(
        deserialize_i8: i8 => visit_i8,
        deserialize_i16: i16 => visit_i16,
        deserialize_i32: i32 => visit_i32,
        deserialize_i64: i64 => visit_i64,
        deserialize_i128: i128 => visit_i128,
    );
    unsigned_integer!(
        deserialize_u8: u8 => visit_u8,
        deserialize_u16: u16 => visit_u16,
        deserialize_u32: u32 => visit_u32,
        deserialize_u64: u64 => visit_u64,
        deserialize_u128: u128 => visit_u128,
    );

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_f32(self.number()? as f32)
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_f64(self.number()?)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if self.0.is_null() {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        let (tag, value) = match self.0 {
            JsValue::String(tag) => (tag, None),
            JsValue::Object(object) if object.len() == 1 => {
                let (tag, value) = object.into_iter().next().expect("one property");
                (tag, Some(value))
            }
            _ => {
                return Err(JsonConversionError::new(
                    "expected an enum string or a single-property object",
                ));
            }
        };
        visitor.visit_enum(Enum { tag, value })
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_unit()
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.0 {
            JsValue::String(value) => KeyDeserializer(value).deserialize_identifier(visitor),
            value => Self(value).deserialize_any(visitor),
        }
    }

    serde::forward_to_deserialize_any! {
        bool char str string bytes byte_buf unit unit_struct seq tuple tuple_struct map struct
    }
}

struct Sequence(std::vec::IntoIter<JsValue>);

impl<'de> SeqAccess<'de> for Sequence {
    type Error = JsonConversionError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        self.0
            .next()
            .map(|value| seed.deserialize(JsValueDeserializer(value)))
            .transpose()
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.0.len())
    }
}

struct Map {
    entries: <JsObject as IntoIterator>::IntoIter,
    pending: Option<JsValue>,
}

impl<'de> MapAccess<'de> for Map {
    type Error = JsonConversionError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        if self.pending.is_some() {
            return Err(JsonConversionError::new("object value was not consumed"));
        }
        let Some((key, value)) = self.entries.next() else {
            return Ok(None);
        };
        self.pending = Some(value);
        seed.deserialize(KeyDeserializer(key)).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        let value = self
            .pending
            .take()
            .ok_or_else(|| JsonConversionError::new("object value has no key"))?;
        seed.deserialize(JsValueDeserializer(value))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.entries.len())
    }
}

/// Object keys have their own adapter because Serde permits integer and boolean
/// map keys to be written as JSON property names. Strings retain their UTF-16
/// representation, including while an untagged or flattened derive buffers them.
struct KeyDeserializer(JsString);

macro_rules! key_number {
    ($($method:ident: $number:ty => $visit:ident),* $(,)?) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            let text = self.0.as_str().ok_or_else(|| JsonConversionError::new("numeric object key contains an unpaired surrogate"))?;
            if text.trim() != text {
                return Err(JsonConversionError::new("object key is not a valid number"));
            }
            let number = serde_json::from_str::<$number>(text).map_err(|_| JsonConversionError::new("object key is not a valid number"))?;
            visitor.$visit(number)
        }
    )*};
}

impl<'de> Deserializer<'de> for KeyDeserializer {
    type Error = JsonConversionError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        JsValueDeserializer(JsValue::String(self.0)).deserialize_any(visitor)
    }

    key_number!(
        deserialize_i8: i8 => visit_i8, deserialize_i16: i16 => visit_i16,
        deserialize_i32: i32 => visit_i32, deserialize_i64: i64 => visit_i64,
        deserialize_i128: i128 => visit_i128, deserialize_u8: u8 => visit_u8,
        deserialize_u16: u16 => visit_u16, deserialize_u32: u32 => visit_u32,
        deserialize_u64: u64 => visit_u64, deserialize_u128: u128 => visit_u128,
        deserialize_f32: f32 => visit_f32, deserialize_f64: f64 => visit_f64,
    );

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.0.as_str() {
            Some("true") => visitor.visit_bool(true),
            Some("false") => visitor.visit_bool(false),
            _ => Err(JsonConversionError::new("object key is not a boolean")),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        JsValueDeserializer(JsValue::String(self.0)).deserialize_enum(name, variants, visitor)
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.0.into_string() {
            Ok(value) => visitor.visit_string(value),
            // The identifier visitor generated for flattened structs lacks a
            // newtype handler but preserves unknown byte buffers in Content.
            // Bytes are another intrinsic Serde kind unavailable in JSON, so
            // this carrier also cannot collide with real arrays or objects.
            // The prefix makes it impossible to equal any valid UTF-8 field
            // name, even when the raw UTF-16LE bytes happen to be valid UTF-8.
            Err(value) => visitor.visit_byte_buf(
                [0xff, 0xff]
                    .into_iter()
                    .chain(value.units().flat_map(u16::to_le_bytes))
                    .collect(),
            ),
        }
    }

    serde::forward_to_deserialize_any! {
        char str string bytes byte_buf unit unit_struct seq tuple tuple_struct map struct ignored_any
    }
}

struct Enum {
    tag: JsString,
    value: Option<JsValue>,
}

impl<'de> EnumAccess<'de> for Enum {
    type Error = JsonConversionError;
    type Variant = Variant;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), Self::Error> {
        let tag = seed.deserialize(KeyDeserializer(self.tag))?;
        Ok((tag, Variant(self.value)))
    }
}

struct Variant(Option<JsValue>);

impl<'de> VariantAccess<'de> for Variant {
    type Error = JsonConversionError;

    fn unit_variant(self) -> Result<(), Self::Error> {
        match self.0 {
            None | Some(JsValue::Null) => Ok(()),
            _ => Err(JsonConversionError::new(
                "unit enum variant has a non-null value",
            )),
        }
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<T::Value, Self::Error> {
        let value = self
            .0
            .ok_or_else(|| JsonConversionError::new("enum variant value is absent"))?;
        seed.deserialize(JsValueDeserializer(value))
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        let value = self
            .0
            .ok_or_else(|| JsonConversionError::new("enum variant value is absent"))?;
        JsValueDeserializer(value).deserialize_tuple(len, visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        let value = self
            .0
            .ok_or_else(|| JsonConversionError::new("enum variant value is absent"))?;
        JsValueDeserializer(value).deserialize_struct("", fields, visitor)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::utils::js_serde::to_js_value;

    fn lone() -> JsString {
        JsString::from_utf16(vec![0xd800, 0x61, 0xdc00])
    }

    #[test]
    fn nested_typed_data_preserves_raw_strings_and_nonfinite_numbers() {
        #[derive(Debug, Deserialize, Serialize)]
        struct Nested {
            text: JsString,
            numbers: Vec<f64>,
            optional: Option<JsString>,
        }
        let original = Nested {
            text: lone(),
            numbers: vec![f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0],
            optional: Some(lone()),
        };
        let decoded: Nested = from_js_value(to_js_value(&original).unwrap()).unwrap();
        assert_eq!(decoded.text, original.text);
        assert_eq!(decoded.optional, original.optional);
        assert!(decoded.numbers[0].is_nan());
        assert_eq!(&decoded.numbers[1..3], &[f64::INFINITY, f64::NEG_INFINITY]);
        assert_eq!(decoded.numbers[3].to_bits(), (-0.0_f64).to_bits());
    }

    #[test]
    fn untagged_derive_retains_nested_strings_keys_and_numbers() {
        #[derive(Debug, Deserialize, PartialEq)]
        #[serde(untagged)]
        enum Choice {
            Text(JsString),
            Record {
                values: HashMap<JsString, Vec<JsValue>>,
            },
            Integer(u32),
        }
        assert_eq!(
            from_js_value::<Choice>(JsValue::String(lone())).unwrap(),
            Choice::Text(lone())
        );
        assert_eq!(
            from_js_value::<Choice>(JsValue::Number(42.0)).unwrap(),
            Choice::Integer(42)
        );
        let value = JsValue::Object(JsObject::from([(
            "values",
            JsValue::Object(JsObject::from([(
                lone(),
                JsValue::Array(vec![
                    JsValue::String(lone()),
                    JsValue::Number(f64::INFINITY),
                    JsValue::Number(-0.0),
                ]),
            )])),
        )]));
        let Choice::Record { values } = from_js_value::<Choice>(value).unwrap() else {
            panic!("wrong variant");
        };
        assert_eq!(values[&lone()][0], JsValue::String(lone()));
        assert_eq!(values[&lone()][1], JsValue::Number(f64::INFINITY));
        assert_eq!(
            values[&lone()][2].as_f64().unwrap().to_bits(),
            (-0.0_f64).to_bits()
        );
    }

    #[test]
    fn real_arrays_and_objects_cannot_impersonate_raw_strings() {
        let array = JsValue::Array(vec![JsValue::Number(55296.0)]);
        let object = JsValue::Object(JsObject::from([("$pi::JsString", array.clone())]));
        assert!(from_js_value::<JsString>(array.clone()).is_err());
        assert!(from_js_value::<JsString>(object.clone()).is_err());
        assert_eq!(from_js_value::<JsValue>(array.clone()).unwrap(), array);
        assert_eq!(from_js_value::<JsValue>(object.clone()).unwrap(), object);
        assert!(from_js_value::<String>(JsValue::String(lone())).is_err());
    }

    #[test]
    fn flattened_maps_retain_unpaired_keys_and_values() {
        #[derive(Debug, Deserialize)]
        struct Flat {
            known: u32,
            #[serde(flatten)]
            extra: HashMap<JsString, JsValue>,
        }
        let value = JsValue::Object(JsObject::from([
            (JsString::from("known"), JsValue::Number(2.0)),
            (lone(), JsValue::String(lone())),
        ]));
        let decoded: Flat = from_js_value(value).unwrap();
        assert_eq!(decoded.known, 2);
        assert_eq!(decoded.extra[&lone()], JsValue::String(lone()));
    }

    #[test]
    fn integer_conversions_check_fractional_nonfinite_and_saturation_boundaries() {
        for number in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.5, -0.5] {
            assert!(from_js_value::<i128>(JsValue::Number(number)).is_err());
            assert!(from_js_value::<u128>(JsValue::Number(number)).is_err());
        }
        assert_eq!(from_js_value::<i8>(JsValue::Number(-128.0)).unwrap(), -128);
        assert_eq!(from_js_value::<u8>(JsValue::Number(255.0)).unwrap(), 255);
        assert!(from_js_value::<i8>(JsValue::Number(128.0)).is_err());
        assert!(from_js_value::<u8>(JsValue::Number(256.0)).is_err());
        assert!(from_js_value::<u64>(JsValue::Number(-1.0)).is_err());
        assert_eq!(
            from_js_value::<i64>(JsValue::Number(i64::MIN as f64)).unwrap(),
            i64::MIN
        );
        assert!(from_js_value::<i64>(JsValue::Number(2.0_f64.powi(63))).is_err());
        assert!(from_js_value::<u64>(JsValue::Number(2.0_f64.powi(64))).is_err());
        assert_eq!(
            from_js_value::<i128>(JsValue::Number(i128::MIN as f64)).unwrap(),
            i128::MIN
        );
        assert!(from_js_value::<i128>(JsValue::Number(2.0_f64.powi(127))).is_err());
        assert!(from_js_value::<u128>(JsValue::Number(2.0_f64.powi(128))).is_err());
        assert!(from_js_value::<u32>(JsValue::String("12".into())).is_err());
    }

    #[test]
    fn externally_tagged_variants_roundtrip_raw_values() {
        #[derive(Debug, Deserialize, Serialize, PartialEq)]
        enum Event {
            Unit,
            Newtype(JsString),
            Tuple(u8, JsString),
            Record { value: JsString },
        }
        for original in [
            Event::Unit,
            Event::Newtype(lone()),
            Event::Tuple(4, lone()),
            Event::Record { value: lone() },
        ] {
            assert_eq!(
                from_js_value::<Event>(to_js_value(&original).unwrap()).unwrap(),
                original
            );
        }
        #[derive(Debug, Deserialize, Serialize, PartialEq)]
        #[serde(tag = "kind", content = "data")]
        enum Adjacent {
            Record { text: JsString, value: f64 },
        }
        let original = Adjacent::Record {
            text: lone(),
            value: f64::INFINITY,
        };
        assert_eq!(
            from_js_value::<Adjacent>(to_js_value(&original).unwrap()).unwrap(),
            original
        );
    }

    #[test]
    fn options_newtypes_ignored_fields_and_tuples_use_standard_serde_shapes() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Wrapped(JsString);
        assert_eq!(
            from_js_value::<Wrapped>(JsValue::String(lone())).unwrap(),
            Wrapped(lone())
        );
        assert_eq!(
            from_js_value::<Option<JsString>>(JsValue::Null).unwrap(),
            None
        );
        assert_eq!(
            from_js_value::<Option<JsString>>(JsValue::String(lone())).unwrap(),
            Some(lone())
        );
        assert_eq!(
            from_js_value::<(u8, bool)>(JsValue::Array(vec![
                JsValue::Number(3.0),
                JsValue::Bool(true)
            ]))
            .unwrap(),
            (3, true)
        );
        assert!(
            from_js_value::<(u8,)>(JsValue::Array(vec![JsValue::Number(3.0), JsValue::Null]))
                .is_err()
        );
        #[derive(Debug, Deserialize)]
        struct Known {
            value: u8,
        }
        let value = JsValue::Object(JsObject::from([
            ("value", JsValue::Number(9.0)),
            ("ignored", JsValue::String(lone())),
        ]));
        assert_eq!(from_js_value::<Known>(value).unwrap().value, 9);
    }

    #[test]
    fn object_keys_support_numeric_boolean_and_newtype_maps() {
        let numeric = JsValue::Object(JsObject::from([("255", JsValue::Bool(true))]));
        assert!(from_js_value::<HashMap<u8, bool>>(numeric.clone()).unwrap()[&255]);
        assert!(from_js_value::<HashMap<i8, bool>>(numeric).is_err());
        for key in ["+1", "01", " 1", "1 ", "1.0", "NaN", "Infinity"] {
            let value = JsValue::Object(JsObject::from([(key, JsValue::Null)]));
            assert!(from_js_value::<HashMap<u64, ()>>(value).is_err());
        }
        let boolean = JsValue::Object(JsObject::from([("false", JsValue::Null)]));
        assert_eq!(
            from_js_value::<HashMap<bool, ()>>(boolean)
                .unwrap()
                .get(&false),
            Some(&())
        );
        #[derive(Debug, Deserialize, Eq, Hash, PartialEq)]
        struct Key(JsString);
        let map = JsValue::Object(JsObject::from([(lone(), JsValue::Number(1.0))]));
        assert_eq!(
            from_js_value::<HashMap<Key, u8>>(map).unwrap()[&Key(lone())],
            1
        );
    }

    #[test]
    fn json_entrypoint_preserves_javascript_domain_and_rejects_repairable_input() {
        #[derive(Deserialize)]
        struct Record {
            text: JsString,
            number: f64,
        }
        let value: Record = from_json(r#"{"text":"\ud800","number":1e400}"#).unwrap();
        assert_eq!(value.text.as_utf16(), [0xd800]);
        assert_eq!(value.number, f64::INFINITY);
        for invalid in [
            r#"{"text":"\q","number":1}"#,
            "{\"text\":\"a\nb\",\"number\":1}",
            r#"{"text":"ok","number":1,}"#,
        ] {
            assert!(from_json::<Record>(invalid).is_err());
        }
    }

    #[test]
    fn flattened_surrogate_key_cannot_match_a_unicode_field_name() {
        #[derive(Debug, Deserialize)]
        struct Flat {
            #[serde(rename = "A؀\0", default)]
            known: Option<bool>,
            #[serde(flatten)]
            extra: HashMap<JsString, JsValue>,
        }
        // Without a non-UTF-8 prefix these units form bytes 41 D8 80 00,
        // exactly the UTF-8 encoding of the unrelated renamed field above.
        let raw = JsString::from_utf16(vec![0xd841, 0x0080]);
        let value = JsValue::Object(JsObject::from([
            (raw.clone(), JsValue::Bool(true)),
            (JsString::from("A؀\0"), JsValue::Bool(false)),
        ]));
        let decoded: Flat = from_js_value(value).unwrap();
        assert_eq!(decoded.known, Some(false));
        assert_eq!(decoded.extra[&raw], JsValue::Bool(true));
        assert_eq!(decoded.extra.len(), 1);
    }
}
