//! Bounded local CPU observation; never calls a provider or reads a secret.

use model_tokenizer::{count_chat_request, count_text, RenderContext, RendererId, TokenizerId};
use serde_json::json;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let context = RenderContext {
        date: Some("2026-10-20".into()),
    };
    if std::env::args().any(|argument| argument == "--large") {
        return large_counts(&context);
    }
    let profiles = [
        RendererId::DeepSeekV41Flash1017,
        RendererId::Glm53Official,
        RendererId::Glm53FlashOfficial,
        RendererId::KimiK3,
        RendererId::GptOss120bJinjaReference,
        RendererId::GptOssSafeguard120bJinjaReference,
        RendererId::Gemma4Tinfoil,
        RendererId::Llama33Tinfoil,
    ];
    let samples = [
        ("whitespace_free", "a".repeat(65_536)),
        (
            "code",
            "fn count(values: &[u64]) -> u64 { values.iter().sum() }\n".repeat(1_024),
        ),
        (
            "unicode",
            "中文日本語🌲 café e\u{301} Σίσυφος ".repeat(1_024),
        ),
    ];
    for profile in profiles {
        for (name, content) in &samples {
            let request = json!({"messages":[{"role":"user","content":content}]});
            let started = Instant::now();
            let result = count_chat_request(profile, &request, &context)?;
            println!(
                "{}",
                json!({
                    "profile": result.profile,
                    "sample": name,
                    "input_utf8_bytes": content.len(),
                    "tokens": result.tokens,
                    "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
                    "cold_profile_first_sample": *name == "whitespace_free",
                })
            );
        }
    }
    Ok(())
}

fn large_counts(context: &RenderContext) -> Result<(), Box<dyn std::error::Error>> {
    let profile = RendererId::DeepSeekV41Flash1017;
    let started = Instant::now();
    let cold = count_chat_request(
        profile,
        &json!({"messages":[{"role":"user","content":"Hello."}]}),
        context,
    )?;
    println!(
        "{}",
        json!({"stage":"cold_init_and_first_count","profile":cold.profile,"tokens":cold.tokens,"elapsed_ms":started.elapsed().as_secs_f64()*1000.0})
    );
    let unit = "fn count(values: &[u64]) -> u64 { values.iter().sum() }\n";
    let unit_tokens = count_text(TokenizerId::DeepSeekV41Flash, unit)?;
    for target in [80_000usize, 256_000, 1_000_000] {
        let content = unit.repeat(target.div_ceil(unit_tokens));
        let request = json!({"messages":[{"role":"user","content":content}]});
        for sample in 0..3 {
            let started = Instant::now();
            let raw = count_text(TokenizerId::DeepSeekV41Flash, &content)?;
            println!(
                "{}",
                json!({"stage":"warm_raw","profile":profile.profile(),"target_tokens":target,"sample":sample,"input_utf8_bytes":content.len(),"tokens":raw,"elapsed_ms":started.elapsed().as_secs_f64()*1000.0})
            );
            let started = Instant::now();
            let full = count_chat_request(profile, &request, context)?;
            println!(
                "{}",
                json!({"stage":"warm_full_prompt","profile":full.profile,"target_tokens":target,"sample":sample,"input_utf8_bytes":content.len(),"tokens":full.tokens,"elapsed_ms":started.elapsed().as_secs_f64()*1000.0})
            );
        }
    }
    for (name, content) in [
        ("run25k_marker", "a".repeat(24_999) + "<|open|>"),
        ("unicode25k_marker", "界".repeat(24_999) + "<|open|>"),
        ("outer400k_marker", "x ".repeat(199_999) + "a<|open|>"),
    ] {
        let started = Instant::now();
        let raw = count_text(TokenizerId::KimiK3, &content)?;
        println!(
            "{}",
            json!({"stage":"kimi_boundary_raw","sample":name,"input_utf8_bytes":content.len(),"input_chars":content.chars().count(),"tokens":raw,"elapsed_ms":started.elapsed().as_secs_f64()*1000.0})
        );
        let request = json!({"messages":[{"role":"user","content":content}]});
        let started = Instant::now();
        let full = count_chat_request(RendererId::KimiK3, &request, context)?;
        println!(
            "{}",
            json!({"stage":"kimi_boundary_full_prompt","sample":name,"profile":full.profile,"tokens":full.tokens,"elapsed_ms":started.elapsed().as_secs_f64()*1000.0})
        );
    }
    Ok(())
}
