//! Fit a model-specific Responses chat view without changing its stored history.

use crate::ApiError;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ops::Range;

const TRUNCATION_NOTICE: &str = "[Previous messages truncated due to context limits]";

pub(crate) struct FittedPrompt {
    pub(crate) request: Value,
    pub(crate) prompt_tokens: usize,
    /// Index of the same current-user message in the fitted request.
    pub(crate) required_from: usize,
}

/// Remove only optional historical groups from a model-specific chat request.
///
/// `required_from` identifies this request's current user, rather than an
/// inferred last message: all following tool exchanges are also mandatory.
/// The counter sees the complete candidate request, including tool definitions
/// and provider rendering controls. No request option or output cap is changed.
pub(crate) fn fit_prompt_request(
    request: Value,
    context_window: usize,
    output_tokens: Option<usize>,
    required_from: usize,
    count: impl Fn(&Value) -> Result<usize, ApiError>,
) -> Result<FittedPrompt, ApiError> {
    let budget = context_window
        .checked_sub(output_tokens.unwrap_or(1).max(1))
        .filter(|budget| *budget > 0)
        .ok_or(ApiError::MessageExceedsContextLimit)?;
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(ApiError::InternalServerError)?;
    if messages.get(required_from).and_then(role) != Some("user") {
        return Err(ApiError::InternalServerError);
    }
    let tokens = count(&request)?;
    if tokens <= budget {
        return Ok(FittedPrompt {
            request,
            prompt_tokens: tokens,
            required_from,
        });
    }
    // An all-fit prompt must retain the existing incomplete-item lifecycle.
    // Only a prompt which actually needs trimming requires atomic boundaries.
    let groups = atomic_groups(messages, required_from)?;

    let first_user = messages[..required_from]
        .iter()
        .position(|message| role(message) == Some("user"));
    let notice_boundary = first_user.and_then(|index| {
        groups
            .iter()
            .find(|group| group.contains(&index))
            .map(|group| {
                if group.end <= required_from {
                    group.end
                } else {
                    group.start
                }
            })
    });
    let mut removable = Vec::new();
    let mut preferred = Vec::new();
    for group in &groups {
        if group.end > required_from || messages[group.clone()].iter().any(is_instruction) {
            continue;
        }
        if first_user.is_some_and(|index| group.contains(&index)) {
            preferred.push(group.clone());
        } else {
            removable.push(group.clone());
        }
    }
    // Preserve the first historical user while optional middle turns remain.
    removable.extend(preferred);
    if removable.is_empty() {
        return Err(ApiError::MessageExceedsContextLimit);
    }

    let measure = |removed: usize, notice: bool| {
        let (candidate, current_user) = candidate_request(
            &request,
            messages,
            &removable[..removed],
            required_from,
            first_user,
            notice_boundary,
            notice,
        );
        let prompt_tokens = count(&candidate)?;
        Ok::<_, ApiError>(FittedPrompt {
            request: candidate,
            prompt_tokens,
            required_from: current_user,
        })
    };

    // Find a known-valid endpoint before searching. This bounds expensive
    // full-prompt tokenization to O(log(history groups)) calls, rather than one
    // tokenization for every message in a large context.
    let mut best = measure(removable.len(), true)?;
    if best.prompt_tokens > budget {
        // Match the existing system + required-tail fallback: a synthetic
        // notice must not crowd out the content the user actually supplied.
        let bare = measure(removable.len(), false)?;
        return if bare.prompt_tokens <= budget {
            Ok(bare)
        } else {
            Err(ApiError::MessageExceedsContextLimit)
        };
    }

    let mut low = 1;
    let mut high = removable.len();
    while low < high {
        let middle = low + (high - low) / 2;
        let candidate = measure(middle, true)?;
        if candidate.prompt_tokens <= budget {
            high = middle;
            best = candidate;
        } else {
            low = middle + 1;
        }
    }
    // `best` is an actually measured fitting candidate, not an additive token
    // estimate. Even a nonmonotonic tokenizer boundary cannot return overflow.
    Ok(best)
}

fn role(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

fn is_instruction(message: &Value) -> bool {
    matches!(role(message), Some("system" | "developer"))
}

fn is_notice(message: &Value) -> bool {
    role(message) == Some("assistant")
        && message.get("content").and_then(Value::as_str) == Some(TRUNCATION_NOTICE)
        && message.get("tool_calls").is_none()
}

/// Group user-led history turns and union any overlapping tool exchange spans.
/// Embedded reasoning remains attached to its complete assistant JSON object.
/// Existing partial items are not repaired or rejected: unmatched calls/results
/// stay in their containing turn; ambiguous repeated IDs share one whole span.
fn atomic_groups(messages: &[Value], required_from: usize) -> Result<Vec<Range<usize>>, ApiError> {
    let mut tool_spans: HashMap<&str, Range<usize>> = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        if role(message).is_none() || !message.is_object() {
            return Err(ApiError::InternalServerError);
        }
        let call_ids = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|call| call.get("id").and_then(Value::as_str));
        let output_id = (role(message) == Some("tool"))
            .then(|| message.get("tool_call_id").and_then(Value::as_str))
            .flatten();
        for id in call_ids.chain(output_id).filter(|id| !id.is_empty()) {
            tool_spans
                .entry(id)
                .and_modify(|span| {
                    span.start = span.start.min(index);
                    span.end = span.end.max(index + 1);
                })
                .or_insert(index..index + 1);
        }
    }

    let first_user = messages[..required_from]
        .iter()
        .position(|message| role(message) == Some("user"));
    let mut spans = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let start = index;
        index += 1;
        if start >= required_from {
            spans.push(start..messages.len());
            break;
        }
        if !is_instruction(&messages[start]) && Some(start) != first_user {
            while index < required_from
                && !is_instruction(&messages[index])
                && role(&messages[index]) != Some("user")
            {
                index += 1;
            }
        }
        spans.push(start..index);
    }
    spans.extend(tool_spans.into_values());
    spans.sort_by_key(|span| (span.start, span.end));
    let mut groups: Vec<Range<usize>> = Vec::new();
    for span in spans {
        match groups.last_mut() {
            Some(previous) if span.start < previous.end => {
                previous.end = previous.end.max(span.end);
            }
            _ => groups.push(span),
        }
    }
    Ok(groups)
}

fn candidate_request(
    request: &Value,
    messages: &[Value],
    removed: &[Range<usize>],
    required_from: usize,
    first_user: Option<usize>,
    notice_boundary: Option<usize>,
    notice: bool,
) -> (Value, usize) {
    let mut omitted = vec![false; messages.len()];
    for span in removed {
        omitted[span.clone()].fill(true);
    }
    let retained = messages
        .iter()
        .enumerate()
        .filter(|(index, _)| !omitted[*index])
        .collect::<Vec<_>>();
    let mut current_user = retained
        .iter()
        .position(|(index, _)| *index == required_from)
        .expect("required user is never removable");
    let insert_at = first_user
        .filter(|first| retained.iter().any(|(index, _)| index == first))
        .and(notice_boundary)
        .map(|boundary| {
            retained
                .iter()
                .position(|(index, _)| *index >= boundary)
                .unwrap_or(retained.len())
        })
        .unwrap_or_else(|| {
            retained
                .iter()
                .position(|(_, message)| !is_instruction(message))
                .unwrap_or(current_user)
        });
    let needs_notice = notice && !retained.iter().any(|(_, message)| is_notice(message));
    let mut fitted = retained
        .into_iter()
        .map(|(_, message)| message.clone())
        .collect::<Vec<_>>();
    if needs_notice {
        fitted.insert(
            insert_at,
            json!({"role": "assistant", "content": TRUNCATION_NOTICE}),
        );
        if insert_at <= current_user {
            current_user += 1;
        }
    }
    let mut candidate = request.clone();
    candidate["messages"] = Value::Array(fitted);
    (candidate, current_user)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn counted(request: &Value) -> Result<usize, ApiError> {
        Ok(serde_json::to_vec(request).unwrap().len())
    }

    fn message(role: &str, content: &str) -> Value {
        json!({"role": role, "content": content})
    }

    fn request(messages: Vec<Value>) -> Value {
        json!({
            "model": "test-model", "messages": messages,
            "tools": [{"type": "function", "function": {"name": "lookup"}}],
            "temperature": 0.7, "max_tokens": 64,
            "chat_template_kwargs": {"thinking": true}
        })
    }

    fn call(id: &str) -> Value {
        json!({"role": "assistant", "reasoning": "associated reasoning", "tool_calls": [{
            "id": id, "type": "function", "function": {"name": "lookup", "arguments": "{}"}
        }]})
    }

    fn output(id: &str) -> Value {
        json!({"role": "tool", "tool_call_id": id, "content": "lookup result"})
    }

    #[test]
    fn all_fit_preserves_every_request_field() {
        let original = request(vec![
            message("system", "instructions"),
            message("user", "current"),
        ]);
        let tokens = counted(&original).unwrap();
        let fit = fit_prompt_request(original.clone(), tokens + 64, Some(64), 1, counted).unwrap();
        assert_eq!(fit.request, original);
        assert_eq!(fit.prompt_tokens, tokens);
        assert_eq!(fit.required_from, 1);
    }

    #[test]
    fn final_count_can_remove_underestimated_history_without_changing_the_source() {
        let original = request(vec![
            message("system", "instructions"),
            message("user", "first"),
            message("assistant", &"old answer ".repeat(200)),
            message("user", "current"),
        ]);
        let before = original.clone();
        let fit = fit_prompt_request(original.clone(), 600, Some(64), 3, counted).unwrap();
        assert_eq!(original, before);
        assert_eq!(fit.request["messages"][0], original["messages"][0]);
        assert_eq!(fit.request["messages"][1], original["messages"][1]);
        assert_eq!(
            fit.request["messages"][fit.required_from],
            original["messages"][3]
        );
        assert_eq!(
            fit.request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| is_notice(m))
                .count(),
            1
        );
        assert!(fit.prompt_tokens + 64 <= 600);
        assert_eq!(fit.prompt_tokens, counted(&fit.request).unwrap());
        for field in [
            "model",
            "tools",
            "temperature",
            "max_tokens",
            "chat_template_kwargs",
        ] {
            assert_eq!(fit.request[field], original[field]);
        }
    }

    #[test]
    fn history_tools_and_embedded_reasoning_are_removed_atomically() {
        let original = request(vec![
            message("system", "instructions"),
            message("user", "first"),
            call("old"),
            output("old"),
            message("user", "current"),
            call("required"),
            output("required"),
        ]);
        let fit = fit_prompt_request(
            original.clone(),
            counted(&original).unwrap() + 63,
            Some(64),
            4,
            counted,
        )
        .unwrap();
        let messages = fit.request["messages"].as_array().unwrap();
        assert!(!messages
            .iter()
            .any(|m| m.get("tool_call_id") == Some(&json!("old"))));
        assert!(!messages
            .iter()
            .any(|m| m.get("tool_calls").is_some_and(|v| v[0]["id"] == "old")));
        assert_eq!(
            &messages[fit.required_from..],
            &original["messages"].as_array().unwrap()[4..]
        );
    }

    #[test]
    fn reordered_tool_results_remain_atomic_even_across_an_intervening_user() {
        let messages = vec![
            message("system", "instructions"),
            message("user", "first"),
            call("old"),
            message("user", "concurrent"),
            output("old"),
            message("user", "current"),
        ];
        let groups = atomic_groups(&messages, 5).unwrap();
        assert!(groups.contains(&(2..5)));
        let original = request(messages);
        let fit = fit_prompt_request(
            original.clone(),
            counted(&original).unwrap() + 63,
            Some(64),
            5,
            counted,
        )
        .unwrap();
        let fitted = fit.request["messages"].as_array().unwrap();
        assert!(!fitted
            .iter()
            .any(|m| m.get("content") == Some(&json!("concurrent"))));
        assert_eq!(fitted[fit.required_from], original["messages"][5]);
    }

    #[test]
    fn required_tool_exchange_and_system_are_never_trimmed_to_fit() {
        let original = request(vec![
            message("system", "instructions"),
            message("user", "current"),
            call("required"),
            output("required"),
        ]);
        let window = counted(&original).unwrap() + 63;
        assert!(matches!(
            fit_prompt_request(original, window, Some(64), 1, counted),
            Err(ApiError::MessageExceedsContextLimit)
        ));
    }

    #[test]
    fn output_options_are_immutable_and_none_only_reserves_one_token() {
        for cap in [Value::Null, json!(64), json!(usize::MAX)] {
            let mut original = request(vec![message("user", "current")]);
            original["max_tokens"] = cap.clone();
            let tokens = counted(&original).unwrap();
            let fit = fit_prompt_request(original.clone(), tokens + 1, None, 0, counted).unwrap();
            assert_eq!(fit.request["max_tokens"], cap);
            assert_eq!(fit.request, original);
        }
        let mut absent = request(vec![message("user", "current")]);
        absent.as_object_mut().unwrap().remove("max_tokens");
        let tokens = counted(&absent).unwrap();
        assert!(fit_prompt_request(absent.clone(), tokens, None, 0, counted).is_err());
        let fit = fit_prompt_request(absent.clone(), tokens + 1, None, 0, counted).unwrap();
        assert_eq!(fit.request, absent);

        let original = request(vec![message("user", "current")]);
        let tokens = counted(&original).unwrap();
        let fit = fit_prompt_request(original.clone(), tokens + 64, Some(64), 0, counted).unwrap();
        assert_eq!(fit.request, original);
        assert!(matches!(
            fit_prompt_request(original.clone(), usize::MAX, Some(usize::MAX), 0, counted),
            Err(ApiError::MessageExceedsContextLimit)
        ));
        assert_eq!(original["max_tokens"], 64);
    }

    #[test]
    fn first_historical_user_is_optional_when_only_system_and_current_fit() {
        let original = request(vec![
            message("system", "instructions"),
            message("user", &"old ".repeat(200)),
            message("assistant", "old reply"),
            message("user", "current"),
        ]);
        let minimal = request(vec![
            original["messages"][0].clone(),
            original["messages"][3].clone(),
        ]);
        let fit = fit_prompt_request(
            original,
            counted(&minimal).unwrap() + 64,
            Some(64),
            3,
            counted,
        )
        .unwrap();
        assert_eq!(fit.request, minimal);
        assert_eq!(fit.required_from, 1);
    }

    #[test]
    fn a_tool_span_touching_the_required_user_and_all_instruction_messages_stay_required() {
        let messages = vec![
            message("system", "first instructions"),
            message("user", "first"),
            message("assistant", &"old ".repeat(200)),
            message("user", "earlier tool question"),
            call("crossing"),
            message("user", "current"),
            message("developer", "additional instructions"),
            output("crossing"),
        ];
        let original = request(messages);
        let fit = fit_prompt_request(
            original.clone(),
            counted(&original).unwrap() + 63,
            Some(64),
            5,
            counted,
        )
        .unwrap();
        let fitted = fit.request["messages"].as_array().unwrap();
        let call_index = fitted
            .iter()
            .position(|m| m.get("tool_calls").is_some())
            .unwrap();
        assert_eq!(
            &fitted[call_index..],
            &original["messages"].as_array().unwrap()[4..]
        );
        assert_eq!(fitted[0], original["messages"][0]);
        assert_eq!(fitted[fit.required_from], original["messages"][5]);
    }

    #[test]
    fn an_existing_truncation_notice_is_not_duplicated() {
        let original = request(vec![
            message("system", "instructions"),
            message("user", "first"),
            message("assistant", TRUNCATION_NOTICE),
            message("user", "old turn"),
            message("assistant", &"old ".repeat(200)),
            message("user", "current"),
        ]);
        let fit = fit_prompt_request(original, 650, Some(64), 5, counted).unwrap();
        assert_eq!(
            fit.request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| is_notice(m))
                .count(),
            1
        );
    }

    #[test]
    fn all_fit_partial_and_ambiguous_tool_relations_are_not_newly_rejected() {
        for messages in [
            vec![message("user", "current"), call("orphan")],
            vec![message("user", "current"), output("missing")],
            vec![
                message("user", "current"),
                call("duplicate"),
                output("duplicate"),
                output("duplicate"),
            ],
            vec![
                message("user", "current"),
                call("duplicate"),
                call("duplicate"),
                output("duplicate"),
            ],
        ] {
            let calls = Cell::new(0);
            let original = request(messages);
            let result = fit_prompt_request(original.clone(), 100_000, None, 0, |_| {
                calls.set(calls.get() + 1);
                Ok(1)
            })
            .unwrap();
            assert_eq!(result.request, original);
            assert_eq!(calls.get(), 1);
        }
    }

    #[test]
    fn overflow_can_remove_a_whole_optional_incomplete_tool_call() {
        let mut partial = call("partial");
        partial["reasoning"] = json!("partial historical reasoning ".repeat(100));
        let original = request(vec![
            message("system", "instructions"),
            message("user", "first"),
            partial,
            message("user", "current"),
        ]);
        let fit = fit_prompt_request(original.clone(), 650, Some(64), 3, counted).unwrap();
        let fitted = fit.request["messages"].as_array().unwrap();
        assert!(!fitted
            .iter()
            .any(|message| message.get("tool_calls").is_some()));
        assert_eq!(fitted[fit.required_from], original["messages"][3]);
        assert_eq!(fitted[0], original["messages"][0]);
        assert!(fit.prompt_tokens + 64 <= 650);
    }

    #[test]
    fn required_incomplete_suffix_is_unchanged_or_fails_only_its_budget() {
        let original = request(vec![
            message("system", "instructions"),
            message("user", "first"),
            message("assistant", &"old answer ".repeat(100)),
            message("user", "current"),
            call("required-partial"),
        ]);
        let minimal = request(vec![
            original["messages"][0].clone(),
            original["messages"][3].clone(),
            original["messages"][4].clone(),
        ]);
        let tokens = counted(&minimal).unwrap();
        let fit = fit_prompt_request(original.clone(), tokens + 64, Some(64), 3, counted).unwrap();
        assert_eq!(fit.request, minimal);
        assert_eq!(fit.required_from, 1);
        assert!(matches!(
            fit_prompt_request(original, tokens + 63, Some(64), 3, counted),
            Err(ApiError::MessageExceedsContextLimit)
        ));
    }

    #[test]
    fn trimming_does_not_split_ambiguous_repeated_tool_ids() {
        let messages = vec![
            message("system", "instructions"),
            message("user", "first"),
            call("repeated"),
            message("user", "concurrent"),
            output("repeated"),
            call("repeated"),
            output("repeated"),
            message("user", "current"),
        ];
        let groups = atomic_groups(&messages, 7).unwrap();
        assert!(groups.contains(&(2..7)));
        let original = request(messages);
        let fit = fit_prompt_request(
            original.clone(),
            counted(&original).unwrap() + 63,
            Some(64),
            7,
            counted,
        )
        .unwrap();
        let fitted = fit.request["messages"].as_array().unwrap();
        assert!(!fitted
            .iter()
            .any(|message| message.get("tool_calls").is_some() || role(message) == Some("tool")));
        assert_eq!(fitted[fit.required_from], original["messages"][7]);
    }

    #[test]
    fn fitting_uses_a_bounded_number_of_full_request_counts() {
        let mut messages = vec![message("system", "instructions"), message("user", "first")];
        for _ in 0..512 {
            messages.push(message("user", &"old ".repeat(100)));
            messages.push(message("assistant", "reply"));
        }
        messages.push(message("user", "current"));
        let required_from = messages.len() - 1;
        let calls = Cell::new(0);
        let fit = fit_prompt_request(request(messages), 600, Some(64), required_from, |value| {
            calls.set(calls.get() + 1);
            counted(value)
        })
        .unwrap();
        assert!(fit.prompt_tokens + 64 <= 600);
        assert!(calls.get() <= 13, "full request counts: {}", calls.get());
    }

    #[test]
    fn nonmonotonic_counter_still_returns_an_actually_measured_fitting_request() {
        let mut messages = vec![message("system", "instructions"), message("user", "first")];
        for _ in 0..200 {
            messages.push(message("user", "historical"));
            messages.push(message("assistant", "reply"));
        }
        messages.push(message("user", "current"));
        let required_from = messages.len() - 1;
        let calls = Cell::new(0);
        let nonmonotonic = |value: &Value| {
            let len = value["messages"].as_array().unwrap().len();
            // A template branch can increase overhead after removing messages.
            Ok::<_, ApiError>(if len <= 4 || len.is_multiple_of(7) {
                10
            } else {
                1_000
            })
        };
        let fit = fit_prompt_request(request(messages), 100, Some(64), required_from, |value| {
            calls.set(calls.get() + 1);
            nonmonotonic(value)
        })
        .unwrap();
        assert_eq!(fit.prompt_tokens, nonmonotonic(&fit.request).unwrap());
        assert!(fit.prompt_tokens + 64 <= 100);
        assert!(calls.get() <= 12);
    }
}
