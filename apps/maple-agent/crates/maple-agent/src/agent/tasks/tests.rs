use super::*;

#[test]
fn a_prompt_title_is_one_shortened_line() {
    assert_eq!(
        session_title_from_prompt("  Fix\nthe   build "),
        "Fix the build"
    );
    let long = "word ".repeat(40);
    let title = session_title_from_prompt(&long);
    assert_eq!(title.chars().count(), MAX_AGENT_SESSION_TITLE_CHARS);
    assert!(title.ends_with('…'));
    assert!(!title.ends_with(" …"));
}

#[test]
fn user_titles_are_single_visible_lines() {
    assert_eq!(
        normalize_user_provided_session_title("  Release notes ").unwrap(),
        "Release notes"
    );
    assert!(normalize_user_provided_session_title("   ").is_err());
    assert!(normalize_user_provided_session_title("two\nlines").is_err());
    assert!(normalize_user_provided_session_title("\u{200b}\u{2060}").is_err());
    assert!(normalize_user_provided_session_title(&"x".repeat(81)).is_err());
}

#[test]
fn only_an_unnamed_unstarted_task_takes_its_title_from_the_prompt() {
    let mut row = TaskRow::new(
        "task".into(),
        DEFAULT_AGENT_SESSION_TITLE.into(),
        "/project".into(),
        TaskKind::Desktop,
        None,
        1,
    );
    assert!(names_from_prompt(&row));
    row.title = ACP_SESSION_FALLBACK_TITLE.into();
    assert!(names_from_prompt(&row));
    row.message_count = 2;
    assert!(!names_from_prompt(&row));
    row.message_count = 0;
    row.title = "Chosen".into();
    assert!(!names_from_prompt(&row));
    row.title = DEFAULT_AGENT_SESSION_TITLE.into();
    row.title_user_set = true;
    assert!(!names_from_prompt(&row));
}
