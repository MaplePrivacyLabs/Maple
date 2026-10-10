use super::*;

#[test]
fn generated_titles_are_cleaned_and_bounded() {
    assert_eq!(
        normalize_generated_title("<think>which words?</think>\n\n  \"Fix   the login bug\"\nmore"),
        Some("Fix the login bug".to_string())
    );
    assert_eq!(
        normalize_generated_title("'Rust port plan'"),
        Some("Rust port plan".to_string())
    );
    assert_eq!(normalize_generated_title("<think>never closed"), None);
    assert_eq!(normalize_generated_title(" \n\t\n"), None);
    let long = normalize_generated_title(&"word ".repeat(40)).unwrap();
    assert_eq!(long.chars().count(), 80);
    assert!(long.ends_with('…'));
}

#[test]
fn summaries_are_one_short_line() {
    assert_eq!(
        normalize_summary("<analysis>x</analysis>\"Listed the files\"\nsecond line"),
        Some("Listed the files".to_string())
    );
    assert_eq!(
        normalize_summary(&"a".repeat(300)).unwrap().chars().count(),
        SUMMARY_MAX_CHARS
    );
    assert_eq!(normalize_summary("\u{7}"), None);
}

#[test]
fn side_questions_keep_the_tools_but_cannot_call_them() {
    assert_eq!(
        without_tool_calls(json!({"model": "m", "tools": []}))["tool_choice"],
        "none"
    );
    assert!(
        without_tool_calls(json!({"model": "m"}))
            .get("tool_choice")
            .is_none()
    );
}

#[test]
fn only_the_first_side_question_is_framed() {
    let model = maple_model("glm-5-3", None, None);
    let prior = [SideQuestionTurn {
        question: "What file?".to_string(),
        answer: "main.rs".to_string(),
    }];
    let messages = side_question_messages(&model, &prior, "Why?");
    let shown: Vec<(&str, String)> = messages
        .iter()
        .map(|message| match message {
            Message::User(user) => ("user", pi_ai::content_text(&user.content)),
            Message::Assistant(assistant) => ("assistant", assistant.text()),
            _ => ("other", String::new()),
        })
        .collect();
    assert_eq!(
        shown,
        [
            ("user", format!("{SIDE_QUESTION_PREFIX}\n\nWhat file?")),
            ("assistant", "main.rs".to_string()),
            ("user", "Why?".to_string()),
        ]
    );
}
