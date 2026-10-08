//! Port of `packages/ai/src/utils/headers.ts`.

use indexmap::IndexMap;

/// Pi permits `null` values to remove a provider header inherited from a prior source.
pub type ProviderHeaders = IndexMap<String, Option<String>>;
pub type HeaderRecord = IndexMap<String, String>;

/// Convert entries from an already normalized HTTP Headers collection to a record.
/// Header normalization belongs to the host's HTTP implementation.
pub fn headers_to_record<I, K, V>(headers: I) -> HeaderRecord
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    headers
        .into_iter()
        .map(|(name, value)| (name.into(), value.into()))
        .collect()
}

pub fn provider_headers_to_record<'a>(
    sources: impl IntoIterator<Item = Option<&'a ProviderHeaders>>,
) -> Option<HeaderRecord> {
    let mut merged: IndexMap<String, (String, String)> = IndexMap::new();
    for source in sources.into_iter().flatten() {
        for name in super::js_json::ordered_keys(source.keys()) {
            let normalized_name = name.to_lowercase();
            // JavaScript Map.delete followed by Map.set moves the key to the end.
            merged.shift_remove(&normalized_name);
            if let Some(value) = &source[name] {
                merged.insert(normalized_name, (name.clone(), value.clone()));
            }
        }
    }
    (!merged.is_empty()).then(|| merged.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_sources_replace_spelling_move_position_and_remove_case_insensitively() {
        let first = ProviderHeaders::from([
            ("Authorization".into(), Some("one".into())),
            ("Keep".into(), Some("kept".into())),
            ("Remove".into(), Some("removed".into())),
        ]);
        let second = ProviderHeaders::from([
            ("authorization".into(), Some("two".into())),
            ("REMOVE".into(), None),
        ]);
        let result = provider_headers_to_record([Some(&first), None, Some(&second)]).unwrap();
        assert_eq!(
            result.into_iter().collect::<Vec<_>>(),
            vec![
                ("Keep".into(), "kept".into()),
                ("authorization".into(), "two".into())
            ]
        );
        assert!(provider_headers_to_record([None]).is_none());
    }
}
