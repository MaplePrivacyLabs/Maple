//! Pi `packages/ai/test/supports-xhigh.test.ts`, using frozen catalog records.
use pi_ai::{
    models::get_supported_thinking_levels,
    types::{Model, ModelThinkingLevel},
};
use serde_json::{Value, json};

fn fixture(provider: &str, id: &str) -> Model {
    let models: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../pi-conformance/corpus/models/supports-xhigh.json"
    )))
    .expect("frozen model fixture must be valid JSON");
    serde_json::from_value(models[format!("{provider}/{id}")].clone())
        .expect("model named by upstream test must exist")
}
fn assert_levels(model: &Model, expected: &[&str]) {
    assert_eq!(
        get_supported_thinking_levels(model)
            .iter()
            .map(|level| level.as_str())
            .collect::<Vec<_>>(),
        expected
    );
}
fn assert_subset(actual: &Value, expected: &Value) {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => {
            for (key, expected) in expected {
                assert_subset(
                    actual.get(key).unwrap_or_else(|| panic!("missing {key}")),
                    expected,
                );
            }
        }
        (Value::Array(actual), Value::Array(expected)) => {
            assert_eq!(actual.len(), expected.len());
            for (actual, expected) in actual.iter().zip(expected) {
                assert_subset(actual, expected);
            }
        }
        (Value::Number(actual), Value::Number(expected)) => {
            assert_eq!(actual.as_f64(), expected.as_f64())
        }
        _ => assert_eq!(actual, expected),
    }
}

mod get_supported_thinking_levels {
    use super::*;
    #[test]
    fn includes_max_but_not_xhigh_for_anthropic_opus_4_6_on_anthropic_messages_api() {
        let model = fixture("anthropic", "claude-opus-4-6");
        let levels = get_supported_thinking_levels(&model);
        assert!(levels.contains(&ModelThinkingLevel::Max));
        assert!(!levels.contains(&ModelThinkingLevel::Xhigh));
    }

    #[test]
    fn includes_xhigh_and_max_for_anthropic_opus_4_8_on_anthropic_messages_api() {
        let model = fixture("anthropic", "claude-opus-4-8");
        let levels = get_supported_thinking_levels(&model);
        assert!(levels.contains(&ModelThinkingLevel::Xhigh));
        assert!(levels.contains(&ModelThinkingLevel::Max));
    }

    #[test]
    fn includes_xhigh_and_max_for_anthropic_opus_5_on_anthropic_messages_api() {
        let model = fixture("anthropic", "claude-opus-5");
        let levels = get_supported_thinking_levels(&model);
        assert!(levels.contains(&ModelThinkingLevel::Xhigh));
        assert!(levels.contains(&ModelThinkingLevel::Max));
    }

    #[test]
    fn includes_claude_opus_5_5_with_its_always_on_effort_levels_and_official_pricing() {
        let model = fixture("anthropic", "claude-opus-5-5");
        assert_subset(
            &serde_json::to_value(&model).unwrap(),
            &json!({"cost": {"input": 4, "output": 20, "cacheRead": 0.2, "cacheWrite": 5}, "contextWindow": 1000000, "maxTokens": 128000, "compat": {"forceAdaptiveThinking": true, "supportsMidConvoEffort": true, "supportsMidConvoSystemMessages": true, "supportsMidConvoToolChanges": true}}),
        );
        assert_levels(&model, &["low", "medium", "high", "xhigh", "max"]);
    }

    #[test]
    fn includes_claude_sonnet_5_5_with_managed_effort_levels_and_official_pricing() {
        let model = fixture("anthropic", "claude-sonnet-5-5");
        assert_subset(
            &serde_json::to_value(&model).unwrap(),
            &json!({"cost": {"input": 2, "output": 10, "cacheRead": 0.2, "cacheWrite": 2.5}, "contextWindow": 1000000, "maxTokens": 128000, "compat": {"forceAdaptiveThinking": true, "supportsMidConvoEffort": true, "supportsMidConvoSystemMessages": true, "supportsMidConvoToolChanges": true, "supportsTemperature": false}}),
        );
        assert_levels(&model, &["low", "medium", "high", "xhigh", "max"]);
    }

    #[test]
    fn includes_max_but_not_xhigh_for_anthropic_sonnet_4_6_on_anthropic_messages_api() {
        let model = fixture("anthropic", "claude-sonnet-4-6");
        let levels = get_supported_thinking_levels(&model);
        assert!(levels.contains(&ModelThinkingLevel::Max));
        assert!(!levels.contains(&ModelThinkingLevel::Xhigh));
    }

    #[test]
    fn includes_xhigh_and_max_for_anthropic_sonnet_5_on_anthropic_messages_api() {
        let model = fixture("anthropic", "claude-sonnet-5");
        let levels = get_supported_thinking_levels(&model);
        assert!(levels.contains(&ModelThinkingLevel::Xhigh));
        assert!(levels.contains(&ModelThinkingLevel::Max));
    }

    #[test]
    fn includes_xhigh_and_max_but_not_off_for_anthropic_claude_fable_5_on_anthropic_messages_api() {
        let model = fixture("anthropic", "claude-fable-5");
        let levels = get_supported_thinking_levels(&model);
        assert!(levels.contains(&ModelThinkingLevel::Xhigh));
        assert!(levels.contains(&ModelThinkingLevel::Max));
        assert!(!levels.contains(&ModelThinkingLevel::Off));
    }

    #[test]
    fn does_not_include_xhigh_or_max_for_claude_sonnet_4_5() {
        let model = fixture("anthropic", "claude-sonnet-4-5");
        let levels = get_supported_thinking_levels(&model);
        assert!(!levels.contains(&ModelThinkingLevel::Xhigh));
        assert!(!levels.contains(&ModelThinkingLevel::Max));
    }

    #[test]
    fn includes_xhigh_for_openai_codex_gpt_5_5_models() {
        let model = fixture("openai-codex", "gpt-5.5");
        let levels = get_supported_thinking_levels(&model);
        assert!(levels.contains(&ModelThinkingLevel::Xhigh));
    }

    #[test]
    fn includes_xhigh_for_openai_codex_gpt_5_6_sol_models() {
        let model = fixture("openai-codex", "gpt-5.6-sol");
        let levels = get_supported_thinking_levels(&model);
        assert!(levels.contains(&ModelThinkingLevel::Xhigh));
    }

    #[test]
    fn includes_xhigh_for_openai_codex_gpt_5_6_terra_models() {
        let model = fixture("openai-codex", "gpt-5.6-terra");
        let levels = get_supported_thinking_levels(&model);
        assert!(levels.contains(&ModelThinkingLevel::Xhigh));
    }

    #[test]
    fn includes_xhigh_for_openai_codex_gpt_5_6_luna_models() {
        let model = fixture("openai-codex", "gpt-5.6-luna");
        let levels = get_supported_thinking_levels(&model);
        assert!(levels.contains(&ModelThinkingLevel::Xhigh));
    }

    #[test]
    fn includes_xhigh_for_openai_codex_gpt_6_astra_models() {
        let model = fixture("openai-codex", "gpt-6-astra");
        let levels = get_supported_thinking_levels(&model);
        assert!(levels.contains(&ModelThinkingLevel::Xhigh));
    }

    #[test]
    fn includes_xhigh_for_openai_codex_gpt_6_sol_models() {
        let model = fixture("openai-codex", "gpt-6-sol");
        let levels = get_supported_thinking_levels(&model);
        assert!(levels.contains(&ModelThinkingLevel::Xhigh));
    }

    #[test]
    fn includes_xhigh_for_openai_codex_gpt_6_luna_models() {
        let model = fixture("openai-codex", "gpt-6-luna");
        let levels = get_supported_thinking_levels(&model);
        assert!(levels.contains(&ModelThinkingLevel::Xhigh));
    }

    #[test]
    fn includes_xhigh_for_openai_codex_gpt_6_1_sol_models() {
        let model = fixture("openai-codex", "gpt-6.1-sol");
        let levels = get_supported_thinking_levels(&model);
        assert!(levels.contains(&ModelThinkingLevel::Xhigh));
    }

    #[test]
    fn includes_xhigh_and_max_for_openai_gpt_5_6_sol_models() {
        let model = fixture("openai", "gpt-5.6-sol");
        assert_levels(&model, &["off", "low", "medium", "high", "xhigh", "max"]);
    }

    #[test]
    fn includes_xhigh_and_max_for_openai_gpt_5_6_terra_models() {
        let model = fixture("openai", "gpt-5.6-terra");
        assert_levels(&model, &["off", "low", "medium", "high", "xhigh", "max"]);
    }

    #[test]
    fn includes_xhigh_and_max_for_openai_gpt_5_6_luna_models() {
        let model = fixture("openai", "gpt-5.6-luna");
        assert_levels(&model, &["off", "low", "medium", "high", "xhigh", "max"]);
    }

    #[test]
    fn includes_xhigh_and_max_for_openai_gpt_6_sol_models() {
        let model = fixture("openai", "gpt-6-sol");
        assert_levels(&model, &["off", "low", "medium", "high", "xhigh", "max"]);
    }

    #[test]
    fn includes_xhigh_and_max_for_openai_gpt_6_luna_models() {
        let model = fixture("openai", "gpt-6-luna");
        assert_levels(&model, &["off", "low", "medium", "high", "xhigh", "max"]);
    }

    #[test]
    fn does_not_support_off_for_gpt_6_1_sol() {
        for (provider, expected) in [
            ("openai", vec!["low", "medium", "high", "xhigh", "max"]),
            ("azure", vec!["low", "medium", "high", "xhigh", "max"]),
            (
                "openai-codex",
                vec!["minimal", "low", "medium", "high", "xhigh", "max"],
            ),
        ] {
            let model = fixture(provider, "gpt-6.1-sol");
            assert_levels(&model, &expected);
            assert_eq!(
                model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|map| map.get(&ModelThinkingLevel::Off)),
                Some(&None)
            );
        }
    }

    #[test]
    fn includes_official_metadata_for_openai_and_codex_gpt_6_sol() {
        for provider in ["openai", "openai-codex"] {
            let model = fixture(provider, "gpt-6-sol");
            assert_subset(
                &serde_json::to_value(model).unwrap(),
                &json!({"input": ["text", "image"], "cost": {"input": 2, "output": 10, "cacheRead": 0.2, "cacheWrite": 2.5, "tiers": [{"inputTokensAbove": 272000, "input": 4, "output": 15.0, "cacheRead": 0.4, "cacheWrite": 5.0}]}, "contextWindow": 272000, "maxTokens": 128000, "compat": {"supportsAdditionalTools": true, "supportsMidConvoSystemMessages": true, "supportsOpenAIGrammarTools": true, "supportsToolSearch": true}}),
            );
        }
    }

    #[test]
    fn includes_official_metadata_for_openai_and_codex_gpt_6_luna() {
        for provider in ["openai", "openai-codex"] {
            let model = fixture(provider, "gpt-6-luna");
            assert_subset(
                &serde_json::to_value(model).unwrap(),
                &json!({"input": ["text", "image"], "cost": {"input": 0.1, "output": 0.5, "cacheRead": 0.01, "cacheWrite": 0.125, "tiers": [{"inputTokensAbove": 272000, "input": 0.2, "output": 0.75, "cacheRead": 0.02, "cacheWrite": 0.25}]}, "contextWindow": 272000, "maxTokens": 128000, "compat": {"supportsAdditionalTools": true, "supportsMidConvoSystemMessages": true, "supportsOpenAIGrammarTools": true, "supportsToolSearch": true}}),
            );
        }
    }

    #[test]
    fn includes_official_metadata_for_openai_and_codex_gpt_6_1_sol() {
        for provider in ["openai", "openai-codex"] {
            let model = fixture(provider, "gpt-6.1-sol");
            assert_subset(
                &serde_json::to_value(model).unwrap(),
                &json!({"input": ["text", "image"], "cost": {"input": 2, "output": 10, "cacheRead": 0.1, "cacheWrite": 2.5, "tiers": [{"inputTokensAbove": 272000, "input": 4, "output": 15.0, "cacheRead": 0.2, "cacheWrite": 5.0}]}, "contextWindow": 272000, "maxTokens": 128000, "compat": {"supportsAdditionalTools": true, "supportsMidConvoSystemMessages": true, "supportsOpenAIGrammarTools": true, "supportsToolSearch": true}}),
            );
        }
    }

    #[test]
    fn includes_only_medium_high_xhigh_for_openai_gpt_5_5_pro() {
        let model = fixture("openai", "gpt-5.5-pro");
        assert_levels(&model, &["medium", "high", "xhigh"]);
    }

    #[test]
    fn includes_only_medium_high_xhigh_for_openrouter_gpt_5_5_pro() {
        let model = fixture("openrouter", "openai/gpt-5.5-pro");
        assert_levels(&model, &["medium", "high", "xhigh"]);
    }

    #[test]
    fn includes_low_high_max_plus_off_for_deepseek_v4_1_flash_on_the_deepseek_provider() {
        let model = fixture("deepseek", "deepseek-flash");
        assert_levels(&model, &["off", "low", "high", "max"]);
    }

    #[test]
    fn includes_low_high_max_plus_off_for_deepseek_v4_flash_on_opencode_go() {
        let model = fixture("opencode-go", "deepseek-v4-flash");
        assert_levels(&model, &["off", "low", "high", "max"]);
    }

    #[test]
    fn preserves_low_high_max_metadata_for_deepseek_v4_1_flash_on_openrouter() {
        let model = fixture("openrouter", "deepseek/deepseek-v4.1-flash");
        assert_levels(&model, &["off", "low", "high", "max"]);
    }

    #[test]
    fn preserves_low_high_max_metadata_for_deepseek_v4_1_flash_on_opencode_go() {
        let model = fixture("opencode-go", "deepseek-v4.1-flash");
        assert_levels(&model, &["low", "high", "max"]);
    }

    #[test]
    fn excludes_thinking_off_for_moonshot_kimi_k2_7_code_models() {
        for provider in ["moonshotai", "moonshotai-cn"] {
            let model = fixture(provider, "kimi-k2.7-code");
            assert_levels(&model, &["minimal", "low", "medium", "high"]);
        }
    }

    #[test]
    fn uses_the_verified_effort_options_for_moonshotai_kimi_k3() {
        let model = fixture("moonshotai", "kimi-k3");
        assert_levels(&model, &["low", "high", "max"]);
    }

    #[test]
    fn uses_the_verified_effort_options_for_moonshotai_cn_kimi_k3() {
        let model = fixture("moonshotai-cn", "kimi-k3");
        assert_levels(&model, &["low", "high", "max"]);
    }

    #[test]
    fn includes_only_low_high_max_for_kimi_coding_k3() {
        let model = fixture("kimi-coding", "k3");
        assert_levels(&model, &["low", "high", "max"]);
    }

    #[test]
    fn includes_only_high_for_opencode_grok_build() {
        let model = fixture("opencode", "grok-build-0.1");
        assert_levels(&model, &["high"]);
    }

    #[test]
    fn includes_only_high_xhigh_plus_off_for_deepseek_v4_flash_on_openrouter() {
        let model = fixture("openrouter", "deepseek/deepseek-v4-flash");
        assert_levels(&model, &["off", "high", "xhigh"]);
    }

    #[test]
    fn includes_max_but_not_xhigh_for_openrouter_opus_4_6_openai_completions_api() {
        let model = fixture("openrouter", "anthropic/claude-opus-4.6");
        let levels = get_supported_thinking_levels(&model);
        assert!(levels.contains(&ModelThinkingLevel::Max));
        assert!(!levels.contains(&ModelThinkingLevel::Xhigh));
    }

    #[test]
    fn includes_xhigh_and_max_for_bedrock_claude_opus_5() {
        let model = fixture("amazon-bedrock", "global.anthropic.claude-opus-5");
        let levels = get_supported_thinking_levels(&model);
        assert!(levels.contains(&ModelThinkingLevel::Xhigh));
        assert!(levels.contains(&ModelThinkingLevel::Max));
    }

    #[test]
    fn includes_xhigh_but_not_off_or_max_for_xai_grok_4_6() {
        let model = fixture("xai", "grok-4.6");
        assert_levels(&model, &["low", "medium", "high", "xhigh"]);
    }

    #[test]
    fn includes_xhigh_and_max_but_not_off_for_bedrock_claude_fable_5() {
        let model = fixture("amazon-bedrock", "global.anthropic.claude-fable-5");
        let levels = get_supported_thinking_levels(&model);
        assert!(levels.contains(&ModelThinkingLevel::Xhigh));
        assert!(levels.contains(&ModelThinkingLevel::Max));
        assert!(!levels.contains(&ModelThinkingLevel::Off));
    }
}
