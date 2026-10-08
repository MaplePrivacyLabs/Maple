//! Serialize typed Pi contracts directly into JavaScript values.
//!
//! Passing through `serde_json::Value` would collapse NaN/infinity to null and
//! cannot retain lone UTF-16 surrogates. This serializer keeps those values and
//! recognizes serde_json's RawValue token for escaped JavaScript strings.

use serde::Serialize;
use serde::ser::{
    SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple,
    SerializeTupleStruct, SerializeTupleVariant, Serializer,
};

use super::js_value::{JsObject, JsString, JsValue, JsonConversionError};

const RAW_VALUE_TOKEN: &str = "$serde_json::private::RawValue";

/// Capture typed fields without a JSON-text round trip. Rust integers that
/// cannot be represented exactly in binary64 are rejected instead of rounded;
/// callers choosing JavaScript rounding do so before this boundary.
pub fn to_js_value<T: Serialize + ?Sized>(value: &T) -> Result<JsValue, JsonConversionError> {
    value.serialize(JsSerializer)
}

struct JsSerializer;

fn signed_number(value: i128) -> Result<JsValue, JsonConversionError> {
    let number = value as f64;
    // A float-to-integer cast saturates, so MAX needs an explicit guard against
    // the rounded 2^127 value comparing equal after that saturation.
    if number as i128 != value || value == i128::MAX {
        return Err(JsonConversionError::new(
            "integer cannot be represented exactly as a JavaScript number",
        ));
    }
    Ok(JsValue::Number(number))
}

fn unsigned_number(value: u128) -> Result<JsValue, JsonConversionError> {
    let number = value as f64;
    if number as u128 != value || value == u128::MAX {
        return Err(JsonConversionError::new(
            "integer cannot be represented exactly as a JavaScript number",
        ));
    }
    Ok(JsValue::Number(number))
}

macro_rules! signed_serializer {
    ($($name:ident: $integer:ty),* $(,)?) => {$(
        fn $name(self, value: $integer) -> Result<Self::Ok, Self::Error> {
            signed_number(i128::from(value))
        }
    )*};
}

macro_rules! unsigned_serializer {
    ($($name:ident: $integer:ty),* $(,)?) => {$(
        fn $name(self, value: $integer) -> Result<Self::Ok, Self::Error> {
            unsigned_number(u128::from(value))
        }
    )*};
}

impl Serializer for JsSerializer {
    type Ok = JsValue;
    type Error = JsonConversionError;
    type SerializeSeq = Sequence;
    type SerializeTuple = Sequence;
    type SerializeTupleStruct = Sequence;
    type SerializeTupleVariant = Sequence;
    type SerializeMap = Map;
    type SerializeStruct = Struct;
    type SerializeStructVariant = Map;

    fn serialize_bool(self, value: bool) -> Result<Self::Ok, Self::Error> {
        Ok(JsValue::Bool(value))
    }

    signed_serializer!(serialize_i8: i8, serialize_i16: i16, serialize_i32: i32, serialize_i64: i64, serialize_i128: i128);
    unsigned_serializer!(serialize_u8: u8, serialize_u16: u16, serialize_u32: u32, serialize_u64: u64, serialize_u128: u128);

    fn serialize_f32(self, value: f32) -> Result<Self::Ok, Self::Error> {
        self.serialize_f64(f64::from(value))
    }

    fn serialize_f64(self, value: f64) -> Result<Self::Ok, Self::Error> {
        Ok(JsValue::Number(value))
    }

    fn serialize_char(self, value: char) -> Result<Self::Ok, Self::Error> {
        self.serialize_str(&value.to_string())
    }

    fn serialize_str(self, value: &str) -> Result<Self::Ok, Self::Error> {
        Ok(JsValue::String(value.into()))
    }

    fn serialize_bytes(self, value: &[u8]) -> Result<Self::Ok, Self::Error> {
        Ok(JsValue::Array(
            value
                .iter()
                .map(|byte| JsValue::Number(f64::from(*byte)))
                .collect(),
        ))
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        self.serialize_unit()
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<Self::Ok, Self::Error> {
        to_js_value(value)
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        Ok(JsValue::Null)
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        self.serialize_unit()
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        self.serialize_str(variant)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        to_js_value(value)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        Ok(tagged(variant, to_js_value(value)?))
    }

    fn serialize_seq(self, length: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Ok(Sequence::new(length.unwrap_or(0), None))
    }

    fn serialize_tuple(self, length: usize) -> Result<Self::SerializeTuple, Self::Error> {
        self.serialize_seq(Some(length))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        length: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        self.serialize_seq(Some(length))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        length: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Ok(Sequence::new(length, Some(variant)))
    }

    fn serialize_map(self, length: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Ok(Map::new(length.unwrap_or(0), None))
    }

    fn serialize_struct(
        self,
        name: &'static str,
        length: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        if name == RAW_VALUE_TOKEN {
            if length != 1 {
                return Err(JsonConversionError::new(
                    "RawValue must contain exactly one field",
                ));
            }
            Ok(Struct::Raw(None))
        } else {
            Ok(Struct::Object(Map::new(length, None)))
        }
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        length: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Ok(Map::new(length, Some(variant)))
    }
}

fn tagged(variant: &str, value: JsValue) -> JsValue {
    JsValue::Object(JsObject::from([(variant, value)]))
}

struct Sequence {
    values: Vec<JsValue>,
    variant: Option<&'static str>,
}

impl Sequence {
    fn new(length: usize, variant: Option<&'static str>) -> Self {
        Self {
            values: Vec::with_capacity(length),
            variant,
        }
    }

    fn finish(self) -> JsValue {
        let value = JsValue::Array(self.values);
        match self.variant {
            Some(variant) => tagged(variant, value),
            None => value,
        }
    }
}

macro_rules! sequence_serializer {
    ($trait:ident, $method:ident) => {
        impl $trait for Sequence {
            type Ok = JsValue;
            type Error = JsonConversionError;

            fn $method<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
                self.values.push(to_js_value(value)?);
                Ok(())
            }

            fn end(self) -> Result<Self::Ok, Self::Error> {
                Ok(self.finish())
            }
        }
    };
}

sequence_serializer!(SerializeSeq, serialize_element);
sequence_serializer!(SerializeTuple, serialize_element);
sequence_serializer!(SerializeTupleStruct, serialize_field);
sequence_serializer!(SerializeTupleVariant, serialize_field);

struct Map {
    values: JsObject,
    next_key: Option<JsString>,
    variant: Option<&'static str>,
}

impl Map {
    fn new(length: usize, variant: Option<&'static str>) -> Self {
        Self {
            values: JsObject::with_capacity(length),
            next_key: None,
            variant,
        }
    }

    fn finish(self) -> Result<JsValue, JsonConversionError> {
        if self.next_key.is_some() {
            return Err(JsonConversionError::new(
                "map key has no corresponding value",
            ));
        }
        let value = JsValue::Object(self.values);
        Ok(match self.variant {
            Some(variant) => tagged(variant, value),
            None => value,
        })
    }
}

impl SerializeMap for Map {
    type Ok = JsValue;
    type Error = JsonConversionError;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Self::Error> {
        if self.next_key.is_some() {
            return Err(JsonConversionError::new(
                "map key has no corresponding value",
            ));
        }
        let key = match to_js_value(key)? {
            JsValue::String(key) => key,
            JsValue::Bool(value) => JsString::from(if value { "true" } else { "false" }),
            JsValue::Number(value) if value.is_finite() => {
                JsString::from(ryu_js::Buffer::new().format(value))
            }
            _ => {
                return Err(JsonConversionError::new(
                    "map key must be a string or finite scalar",
                ));
            }
        };
        self.next_key = Some(key);
        Ok(())
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        let key = self
            .next_key
            .take()
            .ok_or_else(|| JsonConversionError::new("map value has no key"))?;
        self.values.insert(key, to_js_value(value)?);
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.finish()
    }
}

impl SerializeStructVariant for Map {
    type Ok = JsValue;
    type Error = JsonConversionError;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        self.values.insert(key, to_js_value(value)?);
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.finish()
    }
}

enum Struct {
    Object(Map),
    Raw(Option<String>),
}

impl SerializeStruct for Struct {
    type Ok = JsValue;
    type Error = JsonConversionError;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        match self {
            Self::Object(map) => {
                map.values.insert(key, to_js_value(value)?);
                Ok(())
            }
            Self::Raw(raw) => {
                if key != RAW_VALUE_TOKEN || raw.is_some() {
                    return Err(JsonConversionError::new("invalid RawValue field"));
                }
                let JsValue::String(value) = to_js_value(value)? else {
                    return Err(JsonConversionError::new("RawValue source must be a string"));
                };
                *raw = Some(value.into_string().map_err(|_| {
                    JsonConversionError::new("RawValue source must be valid UTF-8 JSON text")
                })?);
                Ok(())
            }
        }
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        match self {
            Self::Object(map) => map.finish(),
            Self::Raw(raw) => {
                let raw =
                    raw.ok_or_else(|| JsonConversionError::new("RawValue field is absent"))?;
                // Strict parsing also rejects a custom Serialize implementation
                // impersonating RawValue with incomplete JSON; no repair occurs
                // at this serialization boundary.
                super::json_parse::parse_json(&raw)
                    .map_err(|error| JsonConversionError::new(error.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn typed_fields_retain_nonfinite_numbers_and_omitted_values() {
        #[derive(Serialize)]
        struct Record {
            nan: f64,
            infinity: f64,
            negative_zero: f64,
            #[serde(skip_serializing_if = "Option::is_none")]
            absent: Option<bool>,
            null: Option<bool>,
        }
        let value = to_js_value(&Record {
            nan: f64::NAN,
            infinity: f64::INFINITY,
            negative_zero: -0.0,
            absent: None,
            null: None,
        })
        .unwrap();
        assert!(value["nan"].as_f64().unwrap().is_nan());
        assert_eq!(value["infinity"].as_f64(), Some(f64::INFINITY));
        assert!(value["negative_zero"].as_f64().unwrap().is_sign_negative());
        assert!(value.get("absent").is_none());
        assert_eq!(value.get("null"), Some(&JsValue::Null));
    }

    #[test]
    fn raw_json_and_typed_strings_retain_unpaired_surrogates() {
        let source = r#"{"\ud800":["\udc00","\ud83d\ude00"]}"#;
        let raw = serde_json::value::RawValue::from_string(source.to_owned()).unwrap();
        let value = to_js_value(&raw).unwrap();
        let key = JsString::from_utf16(vec![0xd800]);
        let items = value.get(&key).unwrap().as_array().unwrap();
        assert_eq!(items[0].as_js_str().unwrap().as_utf16(), [0xdc00]);
        assert_eq!(items[1].as_str(), Some("😀"));
        assert_eq!(to_js_value(&key).unwrap(), JsValue::String(key));
    }

    #[test]
    fn javascript_objects_preserve_invalid_keys_and_nonfinite_fields() {
        let key = JsString::from_utf16(vec![0xd800]);
        let mut object = JsObject::new();
        object.insert(&key, JsValue::Number(f64::NEG_INFINITY));
        object.insert("nil", JsValue::Null);
        let value = to_js_value(&object).unwrap();
        assert_eq!(value.get(&key).unwrap().as_f64(), Some(f64::NEG_INFINITY));
        assert_eq!(value.get("nil"), Some(&JsValue::Null));
        assert!(value.get("�").is_none());
    }

    #[test]
    fn derived_enum_and_sequence_shapes_match_serde_json() {
        #[derive(Serialize)]
        enum Event {
            Unit,
            Newtype(u32),
            Tuple(bool, String),
            Struct { value: i32 },
        }
        for event in [
            Event::Unit,
            Event::Newtype(7),
            Event::Tuple(true, "x".into()),
            Event::Struct { value: -2 },
        ] {
            assert_eq!(
                to_js_value(&event).unwrap(),
                serde_json::to_value(event).unwrap()
            );
        }
        #[derive(Serialize)]
        struct Pair(u32, String);
        #[derive(Serialize)]
        struct Wrapper(Pair);
        assert_eq!(
            to_js_value(&Wrapper(Pair(3, "v".into()))).unwrap(),
            json!([3, "v"])
        );
        assert_eq!(
            to_js_value(&(true, vec![1, 2], Some('é'))).unwrap(),
            json!([true, [1, 2], "é"])
        );
    }

    #[test]
    fn integer_precision_loss_is_rejected_at_the_boundary() {
        assert!(to_js_value(&9_007_199_254_740_993_u64).is_err());
        assert!(to_js_value(&u64::MAX).is_err());
        assert!(to_js_value(&i64::MAX).is_err());
        assert!(to_js_value(&u128::MAX).is_err());
        assert!(to_js_value(&i128::MAX).is_err());
        assert_eq!(
            to_js_value(&(1_u128 << 127)).unwrap(),
            JsValue::Number(2.0_f64.powi(127))
        );
        assert_eq!(
            to_js_value(&i128::MIN).unwrap(),
            JsValue::Number(-2.0_f64.powi(127))
        );
        assert_eq!(
            to_js_value(&9_007_199_254_740_992_u64).unwrap(),
            JsValue::Number(9_007_199_254_740_992.0)
        );
    }

    #[test]
    fn malformed_raw_value_tokens_are_rejected_instead_of_repaired() {
        struct InvalidRaw;
        impl Serialize for InvalidRaw {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut raw = serializer.serialize_struct(RAW_VALUE_TOKEN, 1)?;
                raw.serialize_field(RAW_VALUE_TOKEN, "[1,")?;
                raw.end()
            }
        }
        assert!(to_js_value(&InvalidRaw).is_err());
    }
}
