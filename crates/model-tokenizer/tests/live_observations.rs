//! Replays fixed synthetic requests offline against recorded provider usage.
//!
//! Numeric agreement here applies to these finite request contracts. It does
//! not certify mutable shared deployments or arbitrary request shapes.
//! Later source-profile implementations
//! can replay the same unchanged capture; no network or key is used.

use model_tokenizer::{count_chat_request, RenderContext, RendererId};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Captures {
    capture_date_utc: String,
    date_context: String,
    request_object_order: String,
    observations: Vec<Observation>,
}

#[derive(Deserialize)]
struct Observation {
    id: String,
    provider_label: String,
    request: Value,
    provider_reported_prompt_tokens: usize,
    qualification: String,
    candidate_snapshots: Vec<Candidate>,
}

#[derive(Deserialize)]
struct Candidate {
    renderer_id: String,
    profile: String,
    local_prompt_tokens: usize,
}

fn captures() -> Captures {
    let captures: Captures = serde_json::from_str(include_str!("fixtures/live_observations.json"))
        .expect("reviewed synthetic provider observation fixture");
    assert_eq!(captures.request_object_order, "recursively_lexicographic");
    for observation in &captures.observations {
        assert_captured_wire_order(&observation.request);
    }
    captures
}

fn assert_captured_wire_order(value: &Value) {
    match value {
        Value::Object(object) => {
            // The live harness parsed and reserialized with default serde_json
            // before sending. Its wire order was sorted even though the saved
            // pre-SDK fixture used insertion order. Preserve that same request
            // when this test runs with the preserve_order feature union.
            let keys: Vec<_> = object.keys().collect();
            assert!(keys.windows(2).all(|pair| pair[0] <= pair[1]));
            for child in object.values() {
                assert_captured_wire_order(child);
            }
        }
        Value::Array(array) => {
            for child in array {
                assert_captured_wire_order(child);
            }
        }
        _ => {}
    }
}

fn profile(name: &str) -> RendererId {
    match name {
        "DeepSeekV41Flash1017" => RendererId::DeepSeekV41Flash1017,
        "Glm53Official" => RendererId::Glm53Official,
        "Glm53FlashOfficial" => RendererId::Glm53FlashOfficial,
        "Glm53TinfoilNvfp4" => RendererId::Glm53TinfoilNvfp4,
        // The captured candidate predates native serving normalization. Retain
        // its original source identity; test the new native profile separately.
        "KimiK3" => RendererId::KimiK3OfficialReference,
        "GptOss120bJinjaReference" => RendererId::GptOss120bJinjaReference,
        "GptOssSafeguard120bJinjaReference" => RendererId::GptOssSafeguard120bJinjaReference,
        "Gemma4Tinfoil" => RendererId::Gemma4Tinfoil,
        "Llama33Tinfoil" => RendererId::Llama33Tinfoil,
        _ => panic!("unknown reviewed candidate profile"),
    }
}

#[test]
fn captured_synthetic_candidate_counts_remain_stable() {
    let captures = captures();
    let context = RenderContext {
        date: Some(captures.date_context),
    };
    for observed in captures.observations {
        for candidate in observed.candidate_snapshots {
            let counted =
                count_chat_request(profile(&candidate.renderer_id), &observed.request, &context)
                    .expect("supported synthetic request contract");
            assert_eq!(counted.profile, candidate.profile, "{}", observed.id);
            assert_eq!(
                counted.tokens, candidate.local_prompt_tokens,
                "{}",
                observed.id
            );
            println!(
                "capture={} provider={} case={} candidate={} observed={} difference={}",
                captures.capture_date_utc,
                observed.provider_label,
                observed.id,
                counted.tokens,
                observed.provider_reported_prompt_tokens,
                counted.tokens as i64 - observed.provider_reported_prompt_tokens as i64,
            );
        }
    }
}

#[test]
fn native_harmony_matches_two_captured_shared_baselines() {
    let captures = captures();
    let context = RenderContext {
        date: Some(captures.date_context),
    };
    let mut checked = 0;
    for observed in captures.observations {
        let profile = match observed.id.as_str() {
            "tinfoil-gpt-oss-120b" => RendererId::GptOss120bHarmony,
            "tinfoil-gpt-oss-safeguard-120b" => RendererId::GptOssSafeguard120bHarmony,
            _ => continue,
        };
        let counted = count_chat_request(profile, &observed.request, &context).unwrap();
        assert_eq!(counted.tokens, observed.provider_reported_prompt_tokens);
        assert_eq!(observed.provider_reported_prompt_tokens, 107);
        checked += 1;
    }
    assert_eq!(checked, 2);
}

#[test]
fn native_kimi_matches_captured_shared_baseline() {
    let captures = captures();
    let context = RenderContext {
        date: Some(captures.date_context),
    };
    let observed = captures
        .observations
        .into_iter()
        .find(|observed| observed.id == "tinfoil-kimi-k3")
        .unwrap();
    let counted = count_chat_request(RendererId::KimiK3, &observed.request, &context).unwrap();
    assert_eq!(counted.tokens, observed.provider_reported_prompt_tokens);
    assert_eq!(observed.provider_reported_prompt_tokens, 130);
}

#[test]
fn pinned_own_deepseek_matches_live_baseline_and_tool_observations() {
    let captures = captures();
    let context = RenderContext {
        date: Some(captures.date_context),
    };
    let mut baselines = 0;
    let mut tool_schemas = 0;
    for observed in captures
        .observations
        .into_iter()
        .filter(|o| o.qualification == "own_release_pinned")
    {
        let counted = count_chat_request(
            RendererId::DeepSeekV41Flash1017,
            &observed.request,
            &context,
        )
        .expect("supported pinned DeepSeek request contract");
        assert_eq!(
            counted.tokens, observed.provider_reported_prompt_tokens,
            "{}",
            observed.id,
        );
        if observed.request.get("tools").is_some() {
            tool_schemas += 1;
            assert_eq!(observed.provider_reported_prompt_tokens, 331);
        } else {
            baselines += 1;
            assert_eq!(observed.provider_reported_prompt_tokens, 72);
        }
    }
    assert_eq!(baselines, 2);
    assert_eq!(tool_schemas, 1);
}
