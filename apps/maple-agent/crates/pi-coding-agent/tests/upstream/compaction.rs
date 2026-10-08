use super::compaction_common::*;
use pi_agent_core::types::AgentMessage;
use pi_ai::types::Usage;
use pi_coding_agent::core::compaction::compaction::{
    CompactionSettings, DEFAULT_COMPACTION_SETTINGS, calculate_context_tokens,
    estimate_context_tokens, find_cut_point, get_last_assistant_usage, prepare_compaction,
    should_compact,
};
use pi_coding_agent::core::session_manager::{
    SessionEntry, build_session_context, migrate_session_entries, parse_session_entries,
};
use serde_json::{Value, json as j};

fn entries(values: Vec<Value>) -> Vec<SessionEntry> {
    json(j!(values))
}
fn settings(keep_recent_tokens: f64) -> CompactionSettings {
    CompactionSettings {
        keep_recent_tokens,
        ..DEFAULT_COMPACTION_SETTINGS
    }
}
fn context(entries: &[SessionEntry]) -> Value {
    observed(&build_session_context(entries, None, None))
}
fn load_large_session_entries() -> Vec<SessionEntry> {
    let contents = include_str!("../fixtures/large-session.jsonl");
    let mut entries = parse_session_entries(&contents.into());
    migrate_session_entries(&mut entries, env().as_ref()).expect("pinned legacy session migrates");
    entries
        .into_iter()
        .filter(|entry| entry.kind() != "session")
        .collect()
}

mod token_calculation {
    use super::*;
    #[test]
    fn should_calculate_total_context_tokens_from_usage() {
        let usage: Usage = json(usage(1000.0, 500.0, 200.0, 100.0));
        assert_eq!(calculate_context_tokens(&usage), 1800.0);
    }
    #[test]
    fn should_handle_zero_values() {
        let usage: Usage = json(usage(0.0, 0.0, 0.0, 0.0));
        assert_eq!(calculate_context_tokens(&usage), 0.0);
    }
}

mod get_last_assistant_usage {
    use super::*;
    #[test]
    fn should_find_the_last_non_aborted_assistant_message_usage() {
        let mut b = EntryBuilder::default();
        let entries = entries(vec![
            b.message(user("Hello")),
            b.message(assistant("Hi", Some(usage(100.0, 50.0, 0.0, 0.0)))),
            b.message(user("How are you?")),
            b.message(assistant("Good", Some(usage(200.0, 100.0, 0.0, 0.0)))),
        ]);
        let usage = get_last_assistant_usage(&entries).expect("usage must be present");
        assert_eq!(observed(&usage)["input"], 200);
    }
    #[test]
    fn should_skip_aborted_messages() {
        let mut b = EntryBuilder::default();
        let mut aborted = assistant("Aborted", Some(usage(300.0, 150.0, 0.0, 0.0)));
        aborted["stopReason"] = j!("aborted");
        let entries = entries(vec![
            b.message(user("Hello")),
            b.message(assistant("Hi", Some(usage(100.0, 50.0, 0.0, 0.0)))),
            b.message(user("How are you?")),
            b.message(aborted),
        ]);
        let usage = get_last_assistant_usage(&entries).expect("usage must be present");
        assert_eq!(observed(&usage)["input"], 100);
    }
    #[test]
    fn should_skip_all_zero_assistant_usage() {
        let mut b = EntryBuilder::default();
        let entries = entries(vec![
            b.message(user("Hello")),
            b.message(assistant("Hi", Some(usage(100.0, 50.0, 0.0, 0.0)))),
            b.message(user("continue")),
            b.message(assistant("Partial", Some(usage(0.0, 0.0, 0.0, 0.0)))),
        ]);
        let usage = get_last_assistant_usage(&entries).expect("usage must be present");
        assert_eq!(observed(&usage)["input"], 100);
    }
    #[test]
    fn should_return_undefined_if_no_assistant_messages() {
        let mut b = EntryBuilder::default();
        assert!(get_last_assistant_usage(&entries(vec![b.message(user("Hello"))])).is_none());
    }
}

mod estimate_context_tokens {
    use super::*;
    #[test]
    fn uses_the_last_non_zero_assistant_usage_as_the_context_anchor() {
        let messages: Vec<AgentMessage> = json(j!([
            user("Hello"),
            assistant("Hi", Some(usage(100.0, 50.0, 0.0, 0.0))),
            user("continue"),
            assistant("Partial thinking", Some(usage(0.0, 0.0, 0.0, 0.0)))
        ]));
        let estimate = estimate_context_tokens(&messages).unwrap();
        assert_eq!(estimate.usage_tokens, 150.0);
        assert_eq!(estimate.last_usage_index, Some(1));
        assert!(estimate.trailing_tokens > 0.0);
        assert_eq!(estimate.tokens, 150.0 + estimate.trailing_tokens);
    }
}

mod should_compact {
    use super::*;
    #[test]
    fn should_return_true_when_context_exceeds_threshold() {
        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: 10000.0,
            keep_recent_tokens: 20000.0,
        };
        assert!(should_compact(95000.0, 100000.0, &settings));
        assert!(!should_compact(89000.0, 100000.0, &settings));
    }
    #[test]
    fn should_return_false_when_disabled() {
        let settings = CompactionSettings {
            enabled: false,
            reserve_tokens: 10000.0,
            keep_recent_tokens: 20000.0,
        };
        assert!(!should_compact(95000.0, 100000.0, &settings));
    }
}

mod find_cut_point {
    use super::*;
    #[test]
    fn should_find_cut_point_based_on_actual_token_differences() {
        let mut b = EntryBuilder::default();
        let mut values = Vec::new();
        for i in 0..10 {
            values.push(b.message(user(&format!("User {i}"))));
            values.push(b.message(assistant(
                &format!("Assistant {i}"),
                Some(usage(0.0, 100.0, f64::from(i + 1) * 1000.0, 0.0)),
            )));
        }
        let entries = entries(values);
        let result = find_cut_point(&entries, 0, entries.len(), 2500.0).unwrap();
        let cut = observed(&entries[result.first_kept_entry_index]);
        assert_eq!(cut["type"], "message");
        assert!(matches!(
            cut["message"]["role"].as_str(),
            Some("user" | "assistant")
        ));
    }
    #[test]
    fn should_return_start_index_if_no_valid_cut_points_in_range() {
        let mut b = EntryBuilder::default();
        let entries = entries(vec![b.message(assistant("a", None))]);
        let result = find_cut_point(&entries, 0, entries.len(), 1000.0).unwrap();
        assert_eq!(result.first_kept_entry_index, 0);
    }
    #[test]
    fn should_keep_everything_if_all_messages_fit_within_budget() {
        let mut b = EntryBuilder::default();
        let entries = entries(vec![
            b.message(user("1")),
            b.message(assistant("a", Some(usage(0.0, 50.0, 500.0, 0.0)))),
            b.message(user("2")),
            b.message(assistant("b", Some(usage(0.0, 50.0, 1000.0, 0.0)))),
        ]);
        let result = find_cut_point(&entries, 0, entries.len(), 50000.0).unwrap();
        assert_eq!(result.first_kept_entry_index, 0);
    }
    #[test]
    fn should_indicate_split_turn_when_cutting_at_assistant_message() {
        let mut b = EntryBuilder::default();
        let entries = entries(vec![
            b.message(user("Turn 1")),
            b.message(assistant("A1", Some(usage(0.0, 100.0, 1000.0, 0.0)))),
            b.message(user("Turn 2")),
            b.message(assistant("A2-1", Some(usage(0.0, 100.0, 5000.0, 0.0)))),
            b.message(assistant("A2-2", Some(usage(0.0, 100.0, 8000.0, 0.0)))),
            b.message(assistant("A2-3", Some(usage(0.0, 100.0, 10000.0, 0.0)))),
        ]);
        let result = find_cut_point(&entries, 0, entries.len(), 3000.0).unwrap();
        if observed(&entries[result.first_kept_entry_index])["message"]["role"] == "assistant" {
            assert!(result.is_split_turn);
            assert_eq!(result.turn_start_index, 2);
        }
    }
    #[test]
    fn should_budget_context_visible_custom_message_entries() {
        let mut b = EntryBuilder::default();
        let entries = entries(vec![
            b.message(user("hi")),
            b.message(assistant("hello", None)),
            b.custom(&"x".repeat(4000)),
            b.message(assistant("ok", None)),
        ]);
        let tiny = find_cut_point(&entries, 0, entries.len(), 1.0).unwrap();
        assert_eq!(tiny.first_kept_entry_index, 3);
        assert!(tiny.is_split_turn);
        assert_eq!(tiny.turn_start_index, 2);
        let fits = find_cut_point(&entries, 0, entries.len(), 2.0).unwrap();
        assert_eq!(fits.first_kept_entry_index, 2);
        assert!(!fits.is_split_turn);
        assert_eq!(fits.turn_start_index, -1);
    }
    #[test]
    fn should_fall_back_to_the_latest_valid_cut_point_before_oversized_trailing_tool_results() {
        // Upstream regression test for #9740.
        let mut b = EntryBuilder::default();
        let old_user = b.message(user("old history"));
        let old_assistant = b.message(assistant("old answer", None));
        let current_user = b.message(user("read the large file"));
        let mut call = assistant("", None);
        call["content"] =
            j!([{"type":"toolCall","id":"call-1","name":"read","arguments":{"path":"big.txt"}}]);
        call["stopReason"] = j!("toolUse");
        let call = b.message(call);
        let mut result_message = tool_result(&"x".repeat(8000));
        result_message["toolCallId"] = j!("call-1");
        let result = b.message(result_message);
        let entries = entries(vec![
            old_user.clone(),
            old_assistant.clone(),
            current_user.clone(),
            call.clone(),
            result,
        ]);
        assert_eq!(
            observed(&find_cut_point(&entries, 0, entries.len(), 1000.0).unwrap()),
            j!({"firstKeptEntryIndex":3,"turnStartIndex":2,"isSplitTurn":true})
        );
        let preparation = observed(
            &prepare_compaction(&entries, &settings(1000.0))
                .unwrap()
                .expect("preparation must exist"),
        );
        assert_eq!(preparation["firstKeptEntryId"], call["id"]);
        assert_eq!(
            preparation["messagesToSummarize"],
            j!([old_user["message"], old_assistant["message"]])
        );
        assert_eq!(
            preparation["turnPrefixMessages"],
            j!([current_user["message"]])
        );
    }
}

mod build_session_context {
    use super::*;
    #[test]
    fn should_load_all_messages_when_no_compaction() {
        let mut b = EntryBuilder::default();
        let entries = entries(vec![
            b.message(user("1")),
            b.message(assistant("a", None)),
            b.message(user("2")),
            b.message(assistant("b", None)),
        ]);
        let loaded = context(&entries);
        assert_eq!(loaded["messages"].as_array().unwrap().len(), 4);
        assert_eq!(loaded["thinkingLevel"], "off");
        assert_eq!(
            loaded["model"],
            j!({"provider":"anthropic","modelId":"claude-sonnet-4-5"})
        );
    }
    #[test]
    fn should_handle_single_compaction() {
        let mut b = EntryBuilder::default();
        let u1 = b.message(user("1"));
        let a1 = b.message(assistant("a", None));
        let u2 = b.message(user("2"));
        let a2 = b.message(assistant("b", None));
        let compact = b.compaction("Summary of 1,a,2,b", &u2["id"]);
        let u3 = b.message(user("3"));
        let a3 = b.message(assistant("c", None));
        let loaded = context(&entries(vec![u1, a1, u2, a2, compact, u3, a3]));
        assert_eq!(loaded["messages"].as_array().unwrap().len(), 5);
        assert_eq!(loaded["messages"][0]["role"], "compactionSummary");
        assert!(
            loaded["messages"][0]["summary"]
                .as_str()
                .unwrap()
                .contains("Summary of 1,a,2,b")
        );
    }
    #[test]
    fn should_handle_multiple_compactions_only_latest_matters() {
        let mut b = EntryBuilder::default();
        let u1 = b.message(user("1"));
        let a1 = b.message(assistant("a", None));
        let c1 = b.compaction("First summary", &u1["id"]);
        let u2 = b.message(user("2"));
        let a2 = b.message(assistant("b", None));
        let u3 = b.message(user("3"));
        let a3 = b.message(assistant("c", None));
        let c2 = b.compaction("Second summary", &u3["id"]);
        let u4 = b.message(user("4"));
        let a4 = b.message(assistant("d", None));
        let loaded = context(&entries(vec![u1, a1, c1, u2, a2, u3, a3, c2, u4, a4]));
        assert_eq!(loaded["messages"].as_array().unwrap().len(), 5);
        assert!(
            loaded["messages"][0]["summary"]
                .as_str()
                .unwrap()
                .contains("Second summary")
        );
    }
    #[test]
    fn should_keep_all_messages_when_first_kept_entry_id_is_first_entry() {
        let mut b = EntryBuilder::default();
        let u1 = b.message(user("1"));
        let a1 = b.message(assistant("a", None));
        let c1 = b.compaction("First summary", &u1["id"]);
        let u2 = b.message(user("2"));
        let a2 = b.message(assistant("b", None));
        let loaded = context(&entries(vec![u1, a1, c1, u2, a2]));
        assert_eq!(loaded["messages"].as_array().unwrap().len(), 5);
    }
    #[test]
    fn should_track_model_and_thinking_level_changes() {
        let mut b = EntryBuilder::default();
        let entries = entries(vec![
            b.message(user("1")),
            b.model_change("openai", "gpt-4"),
            b.message(assistant("a", None)),
            b.thinking("high"),
        ]);
        let loaded = context(&entries);
        assert_eq!(
            loaded["model"],
            j!({"provider":"anthropic","modelId":"claude-sonnet-4-5"})
        );
        assert_eq!(loaded["thinkingLevel"], "high");
    }
}

mod prepare_compaction {
    use super::*;
    #[test]
    fn does_not_treat_system_messages_as_conversation_history() {
        let mut b = EntryBuilder::default();
        let system = b.message(j!({"role":"system","content":"","sections":{"preamble":"current prompt"},"timestamp":0}));
        let user = b.message(user("one long turn"));
        let assistant = b.message(assistant("assistant suffix", None));
        let preparation = observed(
            &prepare_compaction(
                &entries(vec![system, user.clone(), assistant.clone()]),
                &settings(1.0),
            )
            .unwrap()
            .expect("preparation must exist"),
        );
        assert_eq!(preparation["firstKeptEntryId"], assistant["id"]);
        assert_eq!(preparation["isSplitTurn"], true);
        assert_eq!(preparation["messagesToSummarize"], j!([]));
        assert_eq!(preparation["turnPrefixMessages"], j!([user["message"]]));
    }
}

mod prepare_compaction_with_previous_compaction {
    use super::*;
    #[test]
    fn should_skip_repeated_compactions_when_kept_messages_still_fit() {
        let mut b = EntryBuilder::default();
        let u1 = b.message(user("user msg 1 (summarized by compaction1)"));
        let a1 = b.message(assistant("assistant msg 1", None));
        let u2 = b.message(user("user msg 2 - kept by compaction1"));
        let a2 = b.message(assistant("assistant msg 2", None));
        let u3 = b.message(user("user msg 3 - kept by compaction1"));
        let a3 = b.message(assistant(
            "assistant msg 3",
            Some(usage(5000.0, 1000.0, 0.0, 0.0)),
        ));
        let c1 = b.compaction("First summary", &u2["id"]);
        let u4 = b.message(user("user msg 4 (new after compaction1)"));
        let a4 = b.message(assistant(
            "assistant msg 4",
            Some(usage(8000.0, 2000.0, 0.0, 0.0)),
        ));
        assert!(
            prepare_compaction(
                &entries(vec![u1, a1, u2, a2, u3, a3, c1, u4, a4]),
                &DEFAULT_COMPACTION_SETTINGS
            )
            .unwrap()
            .is_none()
        );
    }
    #[test]
    fn should_re_summarize_previously_kept_messages_when_the_recent_window_moves_past_them() {
        let mut b = EntryBuilder::default();
        let u1 = b.message(user(&"user msg 1 (summarized by compaction1)".repeat(4)));
        let a1 = b.message(assistant(&"assistant msg 1".repeat(4), None));
        let u2 = b.message(user(&"user msg 2 - kept by compaction1 ".repeat(12)));
        let a2 = b.message(assistant(&"assistant msg 2 ".repeat(12), None));
        let u3 = b.message(user(&"user msg 3 - kept by compaction1 ".repeat(12)));
        let a3 = b.message(assistant(
            &"assistant msg 3 ".repeat(12),
            Some(usage(5000.0, 1000.0, 0.0, 0.0)),
        ));
        let c1 = b.compaction("First summary", &u2["id"]);
        let u4 = b.message(user(&"user msg 4 (new after compaction1) ".repeat(12)));
        let a4 = b.message(assistant(
            &"assistant msg 4 ".repeat(12),
            Some(usage(8000.0, 2000.0, 0.0, 0.0)),
        ));
        let preparation = prepare_compaction(
            &entries(vec![u1, a1, u2, a2, u3, a3, c1, u4, a4]),
            &settings(100.0),
        )
        .unwrap()
        .expect("preparation must exist");
        let text = extract_text(&preparation.messages_to_summarize);
        assert!(text.contains("user msg 2 - kept by compaction1"));
        assert!(text.contains("user msg 3 - kept by compaction1"));
        assert!(!text.contains("First summary"));
        assert_eq!(observed(&preparation.previous_summary), "First summary");
    }
}

mod large_session_fixture {
    use super::*;
    #[test]
    fn should_parse_the_large_session() {
        let entries = load_large_session_entries();
        assert!(entries.len() > 100);
        assert!(
            entries
                .iter()
                .filter(|entry| entry.kind() == "message")
                .count()
                > 100
        );
    }
    #[test]
    fn should_find_cut_point_in_large_session() {
        let entries = load_large_session_entries();
        let result = find_cut_point(
            &entries,
            0,
            entries.len(),
            DEFAULT_COMPACTION_SETTINGS.keep_recent_tokens,
        )
        .unwrap();
        let cut = observed(&entries[result.first_kept_entry_index]);
        assert_eq!(cut["type"], "message");
        assert!(matches!(
            cut["message"]["role"].as_str(),
            Some("user" | "assistant")
        ));
    }
    #[test]
    fn should_load_session_correctly() {
        let loaded = context(&load_large_session_entries());
        assert!(loaded["messages"].as_array().unwrap().len() > 100);
        assert!(!loaded["model"].is_null());
    }
}

// The two LLM summarization tests remain excluded with reason live-provider,
// exactly as the authoritative coverage overrides specify. No ignored pass stubs.
