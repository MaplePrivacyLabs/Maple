//! Usage totals and stable cost grouping from `core/usage-totals.ts`.
use super::session_manager::SessionEntry;
use indexmap::IndexMap;
use pi_ai::utils::raw_message::string;
use pi_ai::{
    types::{JsString, JsValue, Usage, UsageCost},
    utils::js_value::from_js_value,
};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTotals {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub cost: f64,
}
pub fn create_usage_totals() -> UsageTotals {
    UsageTotals::default()
}
pub fn add_usage_to_totals(totals: &mut UsageTotals, usage: &Usage) {
    totals.input += usage.input;
    totals.output += usage.output;
    totals.cache_read += usage.cache_read;
    totals.cache_write += usage.cache_write;
    totals.cost += usage.cost.total;
}
pub fn combine_usage(first: &Usage, second: &Usage) -> Usage {
    Usage {
        input: first.input + second.input,
        output: first.output + second.output,
        cache_read: first.cache_read + second.cache_read,
        cache_write: first.cache_write + second.cache_write,
        cache_write1h: if first.cache_write1h.is_some() || second.cache_write1h.is_some() {
            Some(first.cache_write1h.unwrap_or(0.0) + second.cache_write1h.unwrap_or(0.0))
        } else {
            None
        },
        reasoning: if first.reasoning.is_some() || second.reasoning.is_some() {
            Some(first.reasoning.unwrap_or(0.0) + second.reasoning.unwrap_or(0.0))
        } else {
            None
        },
        total_tokens: if first.total_tokens_present && second.total_tokens_present {
            first.total_tokens + second.total_tokens
        } else {
            f64::NAN
        },
        total_tokens_present: true,
        cost: UsageCost {
            input: first.cost.input + second.cost.input,
            output: first.cost.output + second.cost.output,
            cache_read: first.cost.cache_read + second.cost.cache_read,
            cache_write: first.cost.cache_write + second.cost.cache_write,
            total: first.cost.total + second.cost.total,
        },
    }
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UsageCostBreakdownEntry {
    pub key: JsString,
    pub cost: f64,
    pub tokens: f64,
}
pub fn get_usage_cost_breakdown(entries: &[SessionEntry]) -> Vec<UsageCostBreakdownEntry> {
    let mut totals = IndexMap::<JsString, UsageTotals>::new();
    for entry in entries {
        let raw = entry.value();
        let message = raw.get("message");
        let role = message
            .and_then(|m| m.get("role"))
            .and_then(JsValue::as_str);
        let (key, usage) = match (entry.kind().as_str(), role) {
            (Some("message"), Some("assistant")) => {
                let m = message.expect("message exists");
                let mut key = string(m.get("provider"));
                key.push_str("/");
                key.push(&string(
                    m.get("responseModel")
                        .filter(|v| !v.is_null())
                        .or_else(|| m.get("model")),
                ));
                (key, m.get("usage"))
            }
            (Some("usage"), _) => {
                let mut key = string(raw.get("provider"));
                key.push_str("/");
                key.push(&string(raw.get("model")));
                (key, raw.get("usage"))
            }
            (Some("message"), Some("toolResult")) => (
                "Tools/summaries".into(),
                message.and_then(|m| m.get("usage")),
            ),
            (Some("branch_summary" | "compaction"), _) => {
                ("Tools/summaries".into(), raw.get("usage"))
            }
            _ => continue,
        };
        if let Some(usage) = usage
            && let Ok(usage) = from_js_value::<Usage>(usage.clone())
        {
            add_usage_to_totals(totals.entry(key).or_default(), &usage);
        }
    }
    let mut result = totals
        .into_iter()
        .map(|(key, totals)| UsageCostBreakdownEntry {
            key,
            cost: totals.cost,
            tokens: totals.input + totals.output + totals.cache_read + totals.cache_write,
        })
        .filter(|entry| entry.cost > 0.0 || entry.tokens > 0.0)
        .collect::<Vec<_>>();
    result.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    result
}
