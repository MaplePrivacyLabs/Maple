//! JavaScript values retained across Pi's parser, tool validation and text paths.
//!
//! JSON observations intentionally stringify nonfinite numbers as `null`, but
//! they must remain distinct until then: TypeBox's conversion and validation
//! observe the original number and raw UTF-16 string contents.

pub use super::js_deserialize::{from_js_value, from_json};
pub use super::js_serde::to_js_value;
pub use super::js_string::JsString;
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::{
    fmt,
    ops::{Index, IndexMut},
};

#[derive(Clone, Debug, Default, PartialEq)]
pub enum JsValue {
    #[default]
    Null,
    Bool(bool),
    Number(f64),
    String(JsString),
    Array(Vec<JsValue>),
    Object(JsObject),
}

/// Ordered own properties. Keys also preserve lone UTF-16 surrogates.
/// Updating a property retains its position; removal followed by insertion
/// moves it to the end, matching a JavaScript object's insertion order.
#[derive(Clone, Debug, Default)]
pub struct JsObject(Vec<(JsString, JsValue)>);

impl JsObject {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn clear(&mut self) {
        self.0.clear();
    }
    pub fn insert(&mut self, key: impl Into<JsString>, value: JsValue) -> Option<JsValue> {
        let key = key.into();
        if let Some((_, prior)) = self.0.iter_mut().find(|(candidate, _)| candidate == &key) {
            return Some(std::mem::replace(prior, value));
        }
        self.0.push((key, value));
        None
    }
    pub fn get(&self, key: impl Into<JsString>) -> Option<&JsValue> {
        let key = key.into();
        self.0
            .iter()
            .find(|(candidate, _)| candidate == &key)
            .map(|(_, value)| value)
    }
    pub fn get_mut(&mut self, key: impl Into<JsString>) -> Option<&mut JsValue> {
        let key = key.into();
        self.0
            .iter_mut()
            .find(|(candidate, _)| candidate == &key)
            .map(|(_, value)| value)
    }
    pub fn contains_key(&self, key: impl Into<JsString>) -> bool {
        self.get(key).is_some()
    }
    pub fn remove(&mut self, key: impl Into<JsString>) -> Option<JsValue> {
        let key = key.into();
        let index = self.0.iter().position(|(candidate, _)| candidate == &key)?;
        Some(self.0.remove(index).1)
    }
    pub fn shift_remove(&mut self, key: impl Into<JsString>) -> Option<JsValue> {
        self.remove(key)
    }
    pub fn iter(
        &self,
    ) -> impl ExactSizeIterator<Item = (&JsString, &JsValue)> + DoubleEndedIterator {
        self.0.iter().map(|(key, value)| (key, value))
    }
    pub fn iter_mut(
        &mut self,
    ) -> impl ExactSizeIterator<Item = (&JsString, &mut JsValue)> + DoubleEndedIterator {
        self.0.iter_mut().map(|(key, value)| (&*key, value))
    }
    pub fn keys(&self) -> impl ExactSizeIterator<Item = &JsString> + DoubleEndedIterator {
        self.0.iter().map(|(key, _)| key)
    }
    pub fn values(&self) -> impl ExactSizeIterator<Item = &JsValue> + DoubleEndedIterator {
        self.0.iter().map(|(_, value)| value)
    }
    pub fn values_mut(
        &mut self,
    ) -> impl ExactSizeIterator<Item = &mut JsValue> + DoubleEndedIterator {
        self.0.iter_mut().map(|(_, value)| value)
    }
    pub fn into_values(self) -> impl ExactSizeIterator<Item = JsValue> {
        self.0.into_iter().map(|(_, value)| value)
    }
    pub fn retain(&mut self, mut keep: impl FnMut(&JsString, &mut JsValue) -> bool) {
        self.0.retain_mut(|(key, value)| keep(key, value));
    }
    pub fn entry(&mut self, key: impl Into<JsString>) -> JsObjectEntry<'_> {
        JsObjectEntry {
            object: self,
            key: key.into(),
        }
    }
    pub fn to_json(&self) -> Result<serde_json::Map<String, Value>, JsonConversionError> {
        self.iter()
            .map(|(key, value)| {
                let key = key.as_str().ok_or_else(|| {
                    JsonConversionError::new("object key contains an unpaired UTF-16 surrogate")
                })?;
                Ok((key.to_owned(), value.to_json()?))
            })
            .collect()
    }
}

pub struct JsObjectEntry<'a> {
    object: &'a mut JsObject,
    key: JsString,
}
impl<'a> JsObjectEntry<'a> {
    pub fn or_insert(self, value: JsValue) -> &'a mut JsValue {
        self.or_insert_with(|| value)
    }
    pub fn or_insert_with(self, make: impl FnOnce() -> JsValue) -> &'a mut JsValue {
        let index = self
            .object
            .0
            .iter()
            .position(|(candidate, _)| candidate == &self.key)
            .unwrap_or_else(|| {
                self.object.0.push((self.key, make()));
                self.object.0.len() - 1
            });
        &mut self.object.0[index].1
    }
}

impl<K: Into<JsString>> FromIterator<(K, JsValue)> for JsObject {
    fn from_iter<T: IntoIterator<Item = (K, JsValue)>>(iter: T) -> Self {
        let mut object = Self::new();
        for (key, value) in iter {
            object.insert(key, value);
        }
        object
    }
}
impl<K: Into<JsString>> Extend<(K, JsValue)> for JsObject {
    fn extend<T: IntoIterator<Item = (K, JsValue)>>(&mut self, iter: T) {
        for (key, value) in iter {
            self.insert(key, value);
        }
    }
}
impl<K: Into<JsString>, const N: usize> From<[(K, JsValue); N]> for JsObject {
    fn from(value: [(K, JsValue); N]) -> Self {
        value.into_iter().collect()
    }
}
impl IntoIterator for JsObject {
    type Item = (JsString, JsValue);
    type IntoIter = std::vec::IntoIter<Self::Item>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}
pub struct JsObjectIter<'a>(std::slice::Iter<'a, (JsString, JsValue)>);
impl<'a> Iterator for JsObjectIter<'a> {
    type Item = (&'a JsString, &'a JsValue);
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(key, value)| (key, value))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}
impl ExactSizeIterator for JsObjectIter<'_> {}
impl<'a> IntoIterator for &'a JsObject {
    type Item = (&'a JsString, &'a JsValue);
    type IntoIter = JsObjectIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        JsObjectIter(self.0.iter())
    }
}
pub struct JsObjectIterMut<'a>(std::slice::IterMut<'a, (JsString, JsValue)>);
impl<'a> Iterator for JsObjectIterMut<'a> {
    type Item = (&'a JsString, &'a mut JsValue);
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(key, value)| (&*key, value))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}
impl ExactSizeIterator for JsObjectIterMut<'_> {}
impl<'a> IntoIterator for &'a mut JsObject {
    type Item = (&'a JsString, &'a mut JsValue);
    type IntoIter = JsObjectIterMut<'a>;
    fn into_iter(self) -> Self::IntoIter {
        JsObjectIterMut(self.0.iter_mut())
    }
}

macro_rules! object_index {
    ($key:ty) => {
        impl Index<$key> for JsObject {
            type Output = JsValue;
            fn index(&self, key: $key) -> &Self::Output {
                self.get(key).expect("object property is absent")
            }
        }
        impl IndexMut<$key> for JsObject {
            fn index_mut(&mut self, key: $key) -> &mut Self::Output {
                self.get_mut(key).expect("object property is absent")
            }
        }
    };
}
object_index!(&str);
object_index!(&String);
object_index!(&JsString);

impl JsValue {
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
    pub fn is_boolean(&self) -> bool {
        matches!(self, Self::Bool(_))
    }
    pub fn is_number(&self) -> bool {
        matches!(self, Self::Number(_))
    }
    pub fn is_string(&self) -> bool {
        matches!(self, Self::String(_))
    }
    pub fn is_array(&self) -> bool {
        matches!(self, Self::Array(_))
    }
    pub fn is_object(&self) -> bool {
        matches!(self, Self::Object(_))
    }
    pub fn as_bool(&self) -> Option<bool> {
        if let Self::Bool(value) = self {
            Some(*value)
        } else {
            None
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        if let Self::Number(value) = self {
            Some(*value)
        } else {
            None
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        self.as_f64()
            .filter(|value| {
                value.is_finite()
                    && value.fract() == 0.0
                    && *value >= i64::MIN as f64
                    && *value < -(i64::MIN as f64)
            })
            .map(|value| value as i64)
    }
    pub fn as_u64(&self) -> Option<u64> {
        self.as_f64()
            .filter(|value| {
                value.is_finite()
                    && value.fract() == 0.0
                    && *value >= 0.0
                    && *value < (u64::MAX as f64)
            })
            .map(|value| value as u64)
    }
    pub fn as_js_str(&self) -> Option<&JsString> {
        if let Self::String(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        self.as_js_str().and_then(JsString::as_str)
    }
    pub fn as_array(&self) -> Option<&Vec<Self>> {
        if let Self::Array(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Self>> {
        if let Self::Array(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn as_object(&self) -> Option<&JsObject> {
        if let Self::Object(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn as_object_mut(&mut self) -> Option<&mut JsObject> {
        if let Self::Object(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn get(&self, key: impl Into<JsString>) -> Option<&Self> {
        self.as_object()?.get(key)
    }
    pub fn get_mut(&mut self, key: impl Into<JsString>) -> Option<&mut Self> {
        self.as_object_mut()?.get_mut(key)
    }
    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }

    /// Convert only when ordinary JSON values can retain all observable data.
    /// In particular, this does not silently turn NaN or infinity into null or
    /// substitute replacement characters for unpaired UTF-16 surrogates.
    pub fn to_json(&self) -> Result<Value, JsonConversionError> {
        match self {
            Self::Null => Ok(Value::Null),
            Self::Bool(value) => Ok(Value::Bool(*value)),
            Self::Number(value) => serde_json::Number::from_f64(*value)
                .map(Value::Number)
                .ok_or_else(|| {
                    JsonConversionError::new(
                        "nonfinite JavaScript number cannot be represented by serde_json::Value",
                    )
                }),
            Self::String(value) => value
                .as_str()
                .map(|value| Value::String(value.to_owned()))
                .ok_or_else(|| {
                    JsonConversionError::new("string contains an unpaired UTF-16 surrogate")
                }),
            Self::Array(values) => values
                .iter()
                .map(Self::to_json)
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            Self::Object(values) => values.to_json().map(Value::Object),
        }
    }

    /// Explicitly apply JavaScript's binary64 number representation to a JSON
    /// value. For a lossless boundary, use `TryFrom<Value>` instead.
    pub fn from_json_with_js_numbers(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Bool(value),
            Value::Number(value) => {
                Self::Number(value.as_f64().expect("serde JSON numbers fit binary64"))
            }
            Value::String(value) => Self::String(value.into()),
            Value::Array(values) => Self::Array(
                values
                    .into_iter()
                    .map(Self::from_json_with_js_numbers)
                    .collect(),
            ),
            Value::Object(values) => Self::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, Self::from_json_with_js_numbers(value)))
                    .collect(),
            ),
        }
    }
}

impl TryFrom<Value> for JsValue {
    type Error = JsonConversionError;
    fn try_from(value: Value) -> Result<Self, Self::Error> {
        match value {
            Value::Null => Ok(Self::Null),
            Value::Bool(value) => Ok(Self::Bool(value)),
            Value::Number(value) => {
                let number = value.as_f64().ok_or_else(|| {
                    JsonConversionError::new(
                        "JSON number cannot be represented as a JavaScript number",
                    )
                })?;
                let loses_integer = value
                    .as_i64()
                    .is_some_and(|integer| i128::from(integer) != number as i128)
                    || value
                        .as_u64()
                        .is_some_and(|integer| u128::from(integer) != number as u128);
                if loses_integer {
                    return Err(JsonConversionError::new(
                        "JSON integer cannot be represented exactly as a JavaScript number",
                    ));
                }
                Ok(Self::Number(number))
            }
            Value::String(value) => Ok(Self::String(value.into())),
            Value::Array(values) => values
                .into_iter()
                .map(Self::try_from)
                .collect::<Result<Vec<_>, _>>()
                .map(Self::Array),
            Value::Object(values) => values
                .into_iter()
                .map(|(key, value)| Ok((key, Self::try_from(value)?)))
                .collect::<Result<JsObject, _>>()
                .map(Self::Object),
        }
    }
}
impl TryFrom<serde_json::Map<String, Value>> for JsObject {
    type Error = JsonConversionError;
    fn try_from(values: serde_json::Map<String, Value>) -> Result<Self, Self::Error> {
        values
            .into_iter()
            .map(|(key, value)| Ok((key, JsValue::try_from(value)?)))
            .collect()
    }
}
impl From<JsObject> for JsValue {
    fn from(value: JsObject) -> Self {
        Self::Object(value)
    }
}
impl From<Vec<JsValue>> for JsValue {
    fn from(value: Vec<JsValue>) -> Self {
        Self::Array(value)
    }
}
impl From<JsString> for JsValue {
    fn from(value: JsString) -> Self {
        Self::String(value)
    }
}
impl From<String> for JsValue {
    fn from(value: String) -> Self {
        Self::String(value.into())
    }
}
impl From<&str> for JsValue {
    fn from(value: &str) -> Self {
        Self::String(value.into())
    }
}
impl From<bool> for JsValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}
impl From<f64> for JsValue {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}
impl From<i32> for JsValue {
    fn from(value: i32) -> Self {
        Self::Number(f64::from(value))
    }
}
impl From<u32> for JsValue {
    fn from(value: u32) -> Self {
        Self::Number(f64::from(value))
    }
}

macro_rules! value_index {
    ($key:ty) => {
        impl Index<$key> for JsValue {
            type Output = JsValue;
            fn index(&self, key: $key) -> &Self::Output {
                self.get(key).unwrap_or(&JsValue::Null)
            }
        }
        impl IndexMut<$key> for JsValue {
            fn index_mut(&mut self, key: $key) -> &mut Self::Output {
                if self.is_null() {
                    *self = Self::Object(JsObject::new());
                }
                self.as_object_mut()
                    .expect("cannot index non-object JavaScript value by property")
                    .entry(key)
                    .or_insert(JsValue::Null)
            }
        }
    };
}
value_index!(&str);
value_index!(&String);
value_index!(&JsString);
impl Index<usize> for JsValue {
    type Output = Self;
    fn index(&self, index: usize) -> &Self {
        self.as_array()
            .and_then(|values| values.get(index))
            .unwrap_or(&JsValue::Null)
    }
}
impl IndexMut<usize> for JsValue {
    fn index_mut(&mut self, index: usize) -> &mut Self {
        &mut self
            .as_array_mut()
            .expect("cannot index non-array JavaScript value")[index]
    }
}

impl Serialize for JsValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Number(value) => serializer.serialize_f64(*value),
            Self::String(value) => value.serialize(serializer),
            Self::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(value)?;
                }
                sequence.end()
            }
            Self::Object(values) => values.serialize(serializer),
        }
    }
}
impl Serialize for JsObject {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.len()))?;
        for (key, value) in super::js_json::js_object_entries(self) {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}
impl PartialEq for JsObject {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self
                .iter()
                .all(|(key, value)| other.get(key) == Some(value))
    }
}

struct JsValueVisitor;
impl<'de> serde::de::Visitor<'de> for JsValueVisitor {
    type Value = JsValue;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JavaScript value")
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(JsValue::Null)
    }
    fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(JsValue::Null)
    }
    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
        Ok(JsValue::Bool(value))
    }
    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
        let number = value as f64;
        if number as i128 != i128::from(value) {
            return Err(E::custom(
                "integer cannot be represented exactly as a JavaScript number",
            ));
        }
        Ok(JsValue::Number(number))
    }
    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
        let number = value as f64;
        if number as u128 != u128::from(value) {
            return Err(E::custom(
                "integer cannot be represented exactly as a JavaScript number",
            ));
        }
        Ok(JsValue::Number(number))
    }
    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
        Ok(JsValue::Number(value))
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(JsValue::String(value.into()))
    }
    fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
        Ok(JsValue::String(value.into()))
    }
    fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<Self::Value, E> {
        let Some(bytes) = bytes.strip_prefix(&[0xff, 0xff]) else {
            return Err(E::custom("invalid UTF-16 property marker"));
        };
        if !bytes.len().is_multiple_of(2) {
            return Err(E::custom("UTF-16 property marker has an odd byte length"));
        }
        Ok(JsValue::String(JsString::from_utf16(
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|bytes| u16::from_le_bytes(*bytes))
                .collect::<Vec<_>>(),
        )))
    }
    fn visit_byte_buf<E: serde::de::Error>(self, bytes: Vec<u8>) -> Result<Self::Value, E> {
        self.visit_bytes(&bytes)
    }
    fn visit_newtype_struct<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        Vec::<u16>::deserialize(deserializer)
            .map(|units| JsValue::String(JsString::from_utf16(units)))
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut sequence: A,
    ) -> Result<Self::Value, A::Error> {
        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
        while let Some(value) = sequence.next_element()? {
            values.push(value);
        }
        Ok(JsValue::Array(values))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut object = JsObject::with_capacity(map.size_hint().unwrap_or(0));
        while let Some((key, value)) = map.next_entry::<JsString, JsValue>()? {
            object.insert(key, value);
        }
        Ok(JsValue::Object(object))
    }
}
impl<'de> Deserialize<'de> for JsValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsValueVisitor)
    }
}
impl<'de> Deserialize<'de> for JsObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match deserializer.deserialize_map(JsValueVisitor)? {
            JsValue::Object(value) => Ok(value),
            _ => unreachable!("map visitor returns an object"),
        }
    }
}
impl PartialEq<Value> for JsValue {
    fn eq(&self, other: &Value) -> bool {
        Self::try_from(other.clone()).is_ok_and(|other| self == &other)
    }
}
impl PartialEq<JsValue> for Value {
    fn eq(&self, other: &JsValue) -> bool {
        other == self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonConversionError {
    pub message: String,
}
impl JsonConversionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}
impl fmt::Display for JsonConversionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for JsonConversionError {}
impl serde::de::Error for JsonConversionError {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self::new(message.to_string())
    }
}
impl serde::ser::Error for JsonConversionError {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self::new(message.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lossless_json_conversion_rejects_nonfinite_numbers_and_lone_surrogates() {
        for value in [
            JsValue::Number(f64::INFINITY),
            JsValue::Number(f64::NAN),
            JsString::from_utf16(vec![0xd800]).into(),
        ] {
            assert!(value.to_json().is_err());
        }
        let mut object = JsObject::new();
        object.insert(JsString::from_utf16(vec![0xdc00]), JsValue::Null);
        assert!(object.to_json().is_err());
    }

    #[test]
    fn json_integer_rounding_requires_explicit_conversion() {
        let value = json!(9007199254740993_u64);
        assert!(JsValue::try_from(value.clone()).is_err());
        assert_eq!(
            JsValue::from_json_with_js_numbers(value),
            JsValue::Number(9007199254740992.0)
        );
        assert_eq!(
            JsValue::try_from(json!(1_u64 << 60)).unwrap(),
            JsValue::Number(2.0_f64.powi(60))
        );
        assert!(JsValue::try_from(json!(u64::MAX)).is_err());
    }

    #[test]
    fn object_updates_preserve_order_and_distinguish_surrogate_keys() {
        let mut object = JsObject::new();
        object.insert("a", JsValue::Null);
        object.insert("b", JsValue::Bool(true));
        object.insert("a", JsValue::Bool(false));
        assert_eq!(
            object
                .keys()
                .map(|key| key.as_str().unwrap())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        object.remove("a");
        object.insert("a", JsValue::Null);
        assert_eq!(
            object
                .keys()
                .map(|key| key.as_str().unwrap())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
        let lone = JsString::from_utf16(vec![0xd800]);
        object.insert(&lone, JsValue::Number(1.0));
        assert_eq!(object.get(&lone), Some(&JsValue::Number(1.0)));
        assert!(object.get("�").is_none());
    }
}
