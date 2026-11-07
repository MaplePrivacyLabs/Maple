//! Lossless, bounded payloads for materialized timeline rows.
//!
//! The entry remains authoritative. References belong to individual fields,
//! rather than to the row's latest update: a result can replace a tool's output
//! while its large input still belongs to the original assistant entry.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::AgentTimelineItem;
use crate::store::{StoreError, StoreResult};

pub(super) const MAX_FIELD_BYTES: usize = 1024 * 1024;
const STORED_VERSION: u8 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TruncatedField {
    /// RFC 6901 pointer into the serialized timeline item.
    path: String,
    bytes: usize,
    /// Exact serialized JSON string bytes omitted by the prefix reduction.
    json_extra_bytes: usize,
    entry_seq: i64,
    item_index: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredItem {
    version: u8,
    item: AgentTimelineItem,
    /// Only the text property is omitted; other block properties survive.
    duplicate_output_text: bool,
    truncated_fields: Vec<TruncatedField>,
}

impl StoredItem {
    /// `item_index` is the stable operation ordinal within this source entry.
    pub(super) fn new(
        item: &AgentTimelineItem,
        entry_seq: i64,
        item_index: i64,
    ) -> StoreResult<Self> {
        if entry_seq <= 0 || item_index < 0 {
            return Err(StoreError::InvalidInput);
        }
        let mut value = serde_json::to_value(item).map_err(|_| StoreError::InvalidData)?;
        let duplicate_output_text = remove_duplicate_output_text(&mut value);
        let mut truncated_fields = Vec::new();
        // Ids, roles, statuses and headings participate in canonical merge
        // decisions and are bounded by the projector. Only display payloads
        // can hold arbitrary user/tool data.
        for root in ["text", "input", "output"] {
            if let Some(payload) = value.get_mut(root) {
                truncate_strings(
                    payload,
                    &format!("/{root}"),
                    entry_seq,
                    item_index,
                    &mut truncated_fields,
                )?;
            }
        }
        Ok(Self {
            version: STORED_VERSION,
            item: serde_json::from_value(value).map_err(|_| StoreError::InvalidData)?,
            duplicate_output_text,
            truncated_fields,
        })
    }

    /// The bounded representation, for canonical merges and row metadata.
    /// Tool results resolve their legacy lookup key against the transaction's
    /// latest visible call. Field references retain their original provenance.
    pub(super) fn set_row_key(&mut self, key: String) {
        self.item.id = key;
    }

    pub(super) fn item(&self) -> &AgentTimelineItem {
        &self.item
    }

    /// The display shape with oversized fields left as bounded prefixes.
    /// Duplicate text is an internal storage reduction, so page consumers
    /// must see both canonical output properties even for a bounded row.
    pub(super) fn bounded_item(&self) -> StoreResult<AgentTimelineItem> {
        self.validate()?;
        if !self.duplicate_output_text {
            return Ok(self.item.clone());
        }
        let mut value = serde_json::to_value(&self.item).map_err(|_| StoreError::InvalidData)?;
        restore_duplicate_output_text(&mut value)?;
        serde_json::from_value(value).map_err(|_| StoreError::InvalidData)
    }

    #[cfg(test)]
    fn has_truncated_fields(&self) -> bool {
        !self.truncated_fields.is_empty()
    }

    /// Full canonical row size without fetching its authoritative entries.
    /// JSON escapes can make a missing byte require up to six wire bytes, so
    /// raw UTF-8 lengths alone cannot enforce page budgets accurately.
    pub(super) fn original_json_bytes(&self) -> StoreResult<usize> {
        let bounded = self.bounded_item()?;
        let mut bytes = json_bytes(&bounded)?;
        for field in &self.truncated_fields {
            let copies = if self.duplicate_output_text && field.path == "/output/text" {
                2
            } else {
                1
            };
            bytes = field
                .json_extra_bytes
                .checked_mul(copies)
                .and_then(|extra| bytes.checked_add(extra))
                .ok_or(StoreError::InvalidData)?;
        }
        Ok(bytes)
    }

    pub(super) fn encode(&self) -> StoreResult<Vec<u8>> {
        serde_json::to_vec(self).map_err(|_| StoreError::InvalidData)
    }

    pub(super) fn decode(encoded: &[u8]) -> StoreResult<Self> {
        let stored: Self = serde_json::from_slice(encoded).map_err(|_| StoreError::InvalidData)?;
        stored.validate()?;
        Ok(stored)
    }

    /// Consume one checked stored representation for a page. Original size and
    /// display size share its one canonical reconstruction and serialization.
    pub(super) fn decode_bounded(encoded: &[u8]) -> StoreResult<(AgentTimelineItem, usize, usize)> {
        let stored = Self::decode(encoded)?;
        let mut extra_bytes = 0usize;
        for field in &stored.truncated_fields {
            let copies = if stored.duplicate_output_text && field.path == "/output/text" {
                2
            } else {
                1
            };
            extra_bytes = field
                .json_extra_bytes
                .checked_mul(copies)
                .and_then(|extra| extra_bytes.checked_add(extra))
                .ok_or(StoreError::InvalidData)?;
        }
        let mut item = stored.item;
        if stored.duplicate_output_text {
            restore_output_text(item.output.as_mut().ok_or(StoreError::InvalidData)?)?;
        }
        let bounded_bytes = json_bytes(&item)?;
        let original_bytes = bounded_bytes
            .checked_add(extra_bytes)
            .ok_or(StoreError::InvalidData)?;
        Ok((item, original_bytes, bounded_bytes))
    }

    /// Merge without fetching the large authoritative entry payloads.
    ///
    /// Persisted entry projection emits replacements. Small streaming appends
    /// remain supported, but an append needing multiple entry fragments cannot
    /// be represented by one field reference and is refused before mutation.
    pub(super) fn merge(&mut self, incoming: Self) -> StoreResult<()> {
        self.validate()?;
        incoming.validate()?;
        if self.item.id != incoming.item.id {
            return Err(StoreError::InvalidInput);
        }
        let append = incoming.item.merge == "append"
            && matches!(incoming.item.item_type.as_str(), "message" | "thinking")
            && incoming.item.text.is_some();
        if append
            && (self.has_root_reference("text")
                || incoming.has_root_reference("text")
                || self
                    .item
                    .text
                    .as_ref()
                    .map_or(0, String::len)
                    .saturating_add(incoming.item.text.as_ref().map_or(0, String::len))
                    > MAX_FIELD_BYTES)
        {
            return Err(StoreError::InvalidInput);
        }

        let replaces_text = incoming.item.text.is_some();
        let replaces_input = incoming.item.input.is_some();
        let replaces_output = incoming.item.output.is_some();
        self.truncated_fields.retain(|field| {
            !((replaces_text && under_root(&field.path, "text"))
                || (replaces_input && under_root(&field.path, "input"))
                || (replaces_output && under_root(&field.path, "output")))
        });
        self.truncated_fields.extend(incoming.truncated_fields);
        if replaces_output {
            self.duplicate_output_text = incoming.duplicate_output_text;
        }
        let mut items = vec![self.item.clone()];
        crate::agent::timeline::merge_into(&mut items, incoming.item);
        self.item = items.pop().ok_or(StoreError::InvalidData)?;
        Ok(())
    }

    /// Resolve a source entry's operation using the same projection used on
    /// append/rebuild. The source item is complete, including duplicated text.
    pub(super) fn expand<F>(&self, mut source: F) -> StoreResult<AgentTimelineItem>
    where
        F: FnMut(i64, i64) -> StoreResult<AgentTimelineItem>,
    {
        self.validate()?;
        let mut value = serde_json::to_value(&self.item).map_err(|_| StoreError::InvalidData)?;
        let mut sources = HashMap::<(i64, i64), Value>::new();
        for field in &self.truncated_fields {
            let source_key = (field.entry_seq, field.item_index);
            if let std::collections::hash_map::Entry::Vacant(entry) = sources.entry(source_key) {
                entry.insert(
                    serde_json::to_value(source(field.entry_seq, field.item_index)?)
                        .map_err(|_| StoreError::InvalidData)?,
                );
            }
            let full = sources
                .get(&source_key)
                .and_then(|source| source.pointer(&field.path))
                .and_then(Value::as_str)
                .ok_or(StoreError::InvalidData)?;
            let destination = value
                .pointer_mut(&field.path)
                .ok_or(StoreError::InvalidData)?;
            let prefix = destination.as_str().ok_or(StoreError::InvalidData)?;
            if full.len() != field.bytes
                || !full.starts_with(prefix)
                || json_bytes(full)?.checked_sub(json_bytes(prefix)?)
                    != Some(field.json_extra_bytes)
            {
                return Err(StoreError::InvalidData);
            }
            *destination = Value::String(full.to_owned());
        }
        if self.duplicate_output_text {
            restore_duplicate_output_text(&mut value)?;
        }
        serde_json::from_value(value).map_err(|_| StoreError::InvalidData)
    }

    fn has_root_reference(&self, root: &str) -> bool {
        self.truncated_fields
            .iter()
            .any(|field| under_root(&field.path, root))
    }

    fn validate(&self) -> StoreResult<()> {
        if self.version != STORED_VERSION {
            return Err(StoreError::InvalidData);
        }
        // These are the only metadata-dependent checks below. A normal item
        // has already passed its typed deserializer and needs no Value copy.
        if self.truncated_fields.is_empty() && !self.duplicate_output_text {
            // Preserve the Value serializer's numeric-range check for the one
            // typed u128 field without cloning every string and JSON subtree.
            return serde_json::to_value(self.item.created_ms)
                .map(|_| ())
                .map_err(|_| StoreError::InvalidData);
        }
        let value = serde_json::to_value(&self.item).map_err(|_| StoreError::InvalidData)?;
        let mut paths = HashSet::new();
        for field in &self.truncated_fields {
            if field.bytes <= MAX_FIELD_BYTES
                || field.entry_seq <= 0
                || field.item_index < 0
                || !["text", "input", "output"]
                    .iter()
                    .any(|root| under_root(&field.path, root))
                || !paths.insert(field.path.as_str())
            {
                return Err(StoreError::InvalidData);
            }
            let prefix = value
                .pointer(&field.path)
                .and_then(Value::as_str)
                .ok_or(StoreError::InvalidData)?;
            // UTF-8 can need up to four bytes per scalar. The longest valid
            // prefix therefore ends at most three bytes before the cap.
            if !(MAX_FIELD_BYTES - 3..=MAX_FIELD_BYTES).contains(&prefix.len()) {
                return Err(StoreError::InvalidData);
            }
            if field.json_extra_bytes < field.bytes - prefix.len() {
                return Err(StoreError::InvalidData);
            }
        }
        if self.duplicate_output_text {
            let output = value.get("output").ok_or(StoreError::InvalidData)?;
            let content = output
                .get("content")
                .and_then(Value::as_array)
                .ok_or(StoreError::InvalidData)?;
            if content.len() != 1
                || content[0].get("type").and_then(Value::as_str) != Some("text")
                || content[0].get("text").is_some()
                || output.get("text").and_then(Value::as_str).is_none()
            {
                return Err(StoreError::InvalidData);
            }
        }
        Ok(())
    }
}

fn under_root(path: &str, root: &str) -> bool {
    path.strip_prefix('/').is_some_and(|path| {
        path == root
            || path
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn truncate_strings(
    value: &mut Value,
    path: &str,
    entry_seq: i64,
    item_index: i64,
    fields: &mut Vec<TruncatedField>,
) -> StoreResult<()> {
    match value {
        Value::String(text) if text.len() > MAX_FIELD_BYTES => {
            let bytes = text.len();
            let full_json_bytes = json_bytes(text)?;
            let mut end = MAX_FIELD_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            let json_extra_bytes = full_json_bytes
                .checked_sub(json_bytes(text)?)
                .ok_or(StoreError::InvalidData)?;
            fields.push(TruncatedField {
                path: path.to_string(),
                bytes,
                json_extra_bytes,
                entry_seq,
                item_index,
            });
        }
        Value::Array(values) => {
            for (index, value) in values.iter_mut().enumerate() {
                truncate_strings(
                    value,
                    &format!("{path}/{index}"),
                    entry_seq,
                    item_index,
                    fields,
                )?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                let key = key.replace('~', "~0").replace('/', "~1");
                truncate_strings(
                    value,
                    &format!("{path}/{key}"),
                    entry_seq,
                    item_index,
                    fields,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Count using the same serializer as responses, without allocating an
/// escaped copy of a potentially very large string or row.
fn json_bytes<T: Serialize + ?Sized>(value: &T) -> StoreResult<usize> {
    #[derive(Default)]
    struct Counter(usize);

    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or_else(|| io::Error::other("JSON size overflow"))?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter::default();
    serde_json::to_writer(&mut counter, value).map_err(|_| StoreError::InvalidData)?;
    Ok(counter.0)
}

fn remove_duplicate_output_text(value: &mut Value) -> bool {
    let Some(output) = value.get_mut("output") else {
        return false;
    };
    let Some(text) = output.get("text").and_then(Value::as_str) else {
        return false;
    };
    let Some(content) = output.get("content").and_then(Value::as_array) else {
        return false;
    };
    if content.len() != 1
        || content[0].get("type").and_then(Value::as_str) != Some("text")
        || content[0].get("text").and_then(Value::as_str) != Some(text)
    {
        return false;
    }
    output["content"][0]
        .as_object_mut()
        .expect("a block with string properties is an object")
        .remove("text");
    true
}

fn restore_duplicate_output_text(value: &mut Value) -> StoreResult<()> {
    restore_output_text(value.get_mut("output").ok_or(StoreError::InvalidData)?)
}

fn restore_output_text(output: &mut Value) -> StoreResult<()> {
    let text = output
        .get("text")
        .and_then(Value::as_str)
        .ok_or(StoreError::InvalidData)?
        .to_owned();
    let block = output
        .pointer_mut("/content/0")
        .and_then(Value::as_object_mut)
        .ok_or(StoreError::InvalidData)?;
    block.insert("text".to_string(), Value::String(text));
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn item() -> AgentTimelineItem {
        AgentTimelineItem {
            label: None,
            persisted: None,
            live: None,
            truncated: None,
            revision: 0,
            id: "a1-call".to_string(),
            item_type: "tool".to_string(),
            role: Some("assistant".to_string()),
            title: Some("Write".to_string()),
            text: None,
            status: Some("running".to_string()),
            input: None,
            output: None,
            created_ms: 1,
            merge: "replace".to_string(),
        }
    }

    fn assert_page_decode(stored: &StoredItem, original: &AgentTimelineItem) {
        let (codec, encoded) = crate::store::codec::encode(&stored.encode().unwrap()).unwrap();
        let (bounded, original_bytes, bounded_bytes) =
            super::super::decode_bounded_row(codec, &encoded).unwrap();
        assert_eq!(bounded, stored.bounded_item().unwrap());
        assert_eq!(original_bytes, serde_json::to_vec(original).unwrap().len());
        assert_eq!(bounded_bytes, serde_json::to_vec(&bounded).unwrap().len());
    }

    #[test]
    fn unicode_multiple_fields_and_escaped_pointers_round_trip() {
        let mut original = item();
        original.text = Some(format!("{}🦀rest", "x".repeat(MAX_FIELD_BYTES - 1)));
        original.input = Some(json!({"a/b~c": ["é".repeat(MAX_FIELD_BYTES)]}));
        let stored = StoredItem::new(&original, 7, 2).unwrap();
        assert_eq!(
            stored.item().text.as_ref().unwrap().len(),
            MAX_FIELD_BYTES - 1
        );
        assert_eq!(stored.truncated_fields.len(), 2);
        assert!(
            stored
                .truncated_fields
                .iter()
                .any(|field| field.path == "/input/a~1b~0c/0")
        );
        let decoded = StoredItem::decode(&stored.encode().unwrap()).unwrap();
        assert_page_decode(&stored, &original);
        assert_eq!(
            decoded.original_json_bytes().unwrap(),
            serde_json::to_vec(&original).unwrap().len()
        );
        let mut calls = 0;
        let expanded = decoded
            .expand(|seq, index| {
                assert_eq!((seq, index), (7, 2));
                calls += 1;
                Ok(original.clone())
            })
            .unwrap();
        assert_eq!(expanded, original);
        assert_eq!(calls, 1, "a source operation is loaded once per expansion");
    }

    #[test]
    fn duplicate_single_text_block_is_rehydrated_after_large_text() {
        let mut original = item();
        let text = "result".repeat(MAX_FIELD_BYTES);
        original.output = Some(json!({
            "text": text,
            "content": [{"type": "text", "text": text, "extra": true}]
        }));
        let stored = StoredItem::new(&original, 4, 0).unwrap();
        assert_page_decode(&stored, &original);
        assert!(stored.duplicate_output_text);
        assert_eq!(stored.truncated_fields.len(), 1);
        assert!(
            stored.item().output.as_ref().unwrap()["content"][0]
                .get("text")
                .is_none()
        );
        assert!(stored.has_truncated_fields());
        let bounded = stored.bounded_item().unwrap();
        let bounded_output = bounded.output.as_ref().unwrap();
        assert_eq!(bounded_output["text"], bounded_output["content"][0]["text"]);
        assert_eq!(
            bounded_output["text"].as_str().unwrap().len(),
            MAX_FIELD_BYTES
        );
        assert_eq!(
            stored.original_json_bytes().unwrap(),
            serde_json::to_vec(&original).unwrap().len()
        );
        assert_eq!(
            stored.expand(|_, _| Ok(original.clone())).unwrap(),
            original
        );
    }

    #[test]
    fn distinct_or_multiple_blocks_are_preserved() {
        for output in [
            json!({"text": "a", "content": [{"type": "text", "text": "b"}]}),
            json!({"text": "a", "content": [{"type": "image", "text": "a"}]}),
            json!({"text": "a", "content": [
                {"type": "text", "text": "a"}, {"type": "text", "text": ""}
            ]}),
        ] {
            let mut original = item();
            original.output = Some(output);
            let stored = StoredItem::new(&original, 1, 0).unwrap();
            assert_page_decode(&stored, &original);
            assert!(!stored.duplicate_output_text);
            assert!(!stored.has_truncated_fields());
            assert_eq!(stored.bounded_item().unwrap(), original);
            assert_eq!(
                stored.expand(|_, _| panic!("no entry needed")).unwrap(),
                original
            );
        }
    }

    #[test]
    fn result_merge_preserves_input_entry_provenance() {
        let mut call = item();
        call.input = Some(json!({"content": "input".repeat(MAX_FIELD_BYTES)}));
        let mut result = item();
        result.title = None;
        result.created_ms = 2;
        result.status = Some("completed".to_string());
        let text = "output".repeat(MAX_FIELD_BYTES);
        result.output = Some(json!({"text": text, "content": [{"type": "text", "text": text}]}));
        let mut stored = StoredItem::new(&call, 3, 5).unwrap();
        stored
            .merge(StoredItem::new(&result, 9, 0).unwrap())
            .unwrap();
        let mut expected = vec![call.clone()];
        crate::agent::timeline::merge_into(&mut expected, result.clone());
        assert_eq!(
            stored.original_json_bytes().unwrap(),
            serde_json::to_vec(&expected[0]).unwrap().len()
        );
        let expanded = stored
            .expand(|seq, index| match (seq, index) {
                (3, 5) => Ok(call.clone()),
                (9, 0) => Ok(result.clone()),
                _ => panic!("wrong source"),
            })
            .unwrap();
        assert_eq!(expanded, expected.pop().unwrap());
    }

    #[test]
    fn original_json_size_counts_escaped_and_unicode_tail_bytes_exactly() {
        let mut original = item();
        original.text = Some(format!(
            "{}🦀\"\\\n\r\t\u{0001}",
            "x".repeat(MAX_FIELD_BYTES - 1)
        ));
        original.input = Some(json!({
            "payload": format!("{}\"\u{0000}é", "y".repeat(MAX_FIELD_BYTES))
        }));
        let stored = StoredItem::new(&original, 1, 0).unwrap();
        assert_page_decode(&stored, &original);
        assert!(stored.truncated_fields.iter().all(|field| {
            let stored_json = serde_json::to_value(stored.item()).unwrap();
            let prefix = stored_json.pointer(&field.path).unwrap().as_str().unwrap();
            field.json_extra_bytes > field.bytes - prefix.len()
        }));
        let decoded = StoredItem::decode(&stored.encode().unwrap()).unwrap();
        assert_eq!(
            decoded.original_json_bytes().unwrap(),
            serde_json::to_vec(&original).unwrap().len()
        );
        assert_eq!(
            decoded.expand(|_, _| Ok(original.clone())).unwrap(),
            original
        );
    }

    #[test]
    fn replacement_drops_obsolete_references_and_duplicate_marker() {
        let mut old = item();
        old.input = Some(json!({"content": "x".repeat(MAX_FIELD_BYTES + 1)}));
        old.output = Some(json!({"text": "same", "content": [{"type": "text", "text": "same"}]}));
        let mut replacement = item();
        replacement.input = Some(json!({"content": "new"}));
        replacement.output = Some(json!({"text": "new", "content": []}));
        let mut stored = StoredItem::new(&old, 1, 0).unwrap();
        stored
            .merge(StoredItem::new(&replacement, 2, 0).unwrap())
            .unwrap();
        assert!(stored.truncated_fields.is_empty());
        assert!(!stored.duplicate_output_text);
        assert_eq!(
            stored.expand(|_, _| panic!("no entry needed")).unwrap(),
            replacement
        );
    }

    #[test]
    fn corrupt_reference_and_wrong_source_fail_without_content_in_error() {
        let mut original = item();
        original.text = Some("secret".repeat(MAX_FIELD_BYTES));
        let stored = StoredItem::new(&original, 1, 0).unwrap();
        let mut wrong = original.clone();
        wrong.text = Some("different".to_string());
        assert_eq!(
            stored.expand(|_, _| Ok(wrong.clone())),
            Err(StoreError::InvalidData)
        );
        let mut value = serde_json::to_value(&stored).unwrap();
        value["truncated_fields"][0]["path"] = json!("/missing");
        assert!(matches!(
            StoredItem::decode(&serde_json::to_vec(&value).unwrap()),
            Err(StoreError::InvalidData)
        ));
        assert!(matches!(
            StoredItem::decode_bounded(&serde_json::to_vec(&value).unwrap()),
            Err(StoreError::InvalidData)
        ));
    }

    #[test]
    fn page_decode_keeps_version_duplicate_shape_and_numeric_range_validation() {
        let stored = StoredItem::new(&item(), 1, 0).unwrap();
        let mut invalid = serde_json::to_value(&stored).unwrap();
        invalid["version"] = json!(255);
        assert!(StoredItem::decode_bounded(&serde_json::to_vec(&invalid).unwrap()).is_err());
        invalid["version"] = json!(STORED_VERSION);
        invalid["duplicate_output_text"] = json!(true);
        invalid["item"]["output"] =
            json!({"text":"kept","content":[{"type":"text","text":"unexpected"}]});
        assert!(StoredItem::decode_bounded(&serde_json::to_vec(&invalid).unwrap()).is_err());
        let mut too_large = stored;
        too_large.item.created_ms = u128::MAX;
        let encoded = serde_json::to_vec(&too_large).unwrap();
        assert!(StoredItem::decode(&encoded).is_err());
        assert!(StoredItem::decode_bounded(&encoded).is_err());
    }

    #[test]
    fn oversized_append_is_refused_without_mutating_existing_row() {
        let mut original = item();
        original.item_type = "message".to_string();
        original.text = Some("a".repeat(MAX_FIELD_BYTES));
        let mut incoming = original.clone();
        incoming.merge = "append".to_string();
        incoming.text = Some("b".to_string());
        let mut stored = StoredItem::new(&original, 1, 0).unwrap();
        let before = stored.encode().unwrap();
        assert_eq!(
            stored.merge(StoredItem::new(&incoming, 2, 0).unwrap()),
            Err(StoreError::InvalidInput)
        );
        assert_eq!(stored.encode().unwrap(), before);
    }
}
