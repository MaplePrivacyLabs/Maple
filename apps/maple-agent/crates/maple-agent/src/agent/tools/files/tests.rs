use super::*;

fn read(path: &str, offset: Option<usize>, limit: Option<usize>) -> ReadParams {
    ReadParams {
        path: path.to_string(),
        offset,
        limit,
    }
}

fn edits(path: &str, replacements: &[(&str, &str)]) -> EditParams {
    EditParams {
        path: path.to_string(),
        edits: replacements
            .iter()
            .map(|(old_text, new_text)| Replacement {
                old_text: old_text.to_string(),
                new_text: new_text.to_string(),
            })
            .collect(),
    }
}

#[tokio::test]
async fn read_supports_offsets_limits_and_continuation() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("notes.txt"), "one\ntwo\nthree\nfour").unwrap();
    let text = read_file(
        read("notes.txt", Some(2), Some(2)),
        Some(temp.path()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        text,
        "two\nthree\n\n[Showing lines 2-3. Use offset=4 to continue.]"
    );

    let error = read_file(
        read("notes.txt", Some(9), None),
        Some(temp.path()),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(error.contains("beyond end of file"), "{error}");
}

#[tokio::test]
async fn read_truncates_on_whole_lines_and_names_the_next_offset() {
    let temp = tempfile::tempdir().unwrap();
    let content = (1..=MAX_READ_LINES + 1)
        .map(|line| format!("line-{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(temp.path().join("large.txt"), content).unwrap();
    let text = read_file(
        read("large.txt", None, None),
        Some(temp.path()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(text.contains("Showing lines 1-2000"));
    assert!(text.contains("Use offset=2001 to continue"));

    // A file that ends exactly at the limit has no phantom next line.
    fs::write(
        temp.path().join("exact.txt"),
        "line\n".repeat(MAX_READ_LINES),
    )
    .unwrap();
    let text = read_file(
        read("exact.txt", None, None),
        Some(temp.path()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!text.contains("Use offset="));
    assert_eq!(text.lines().count(), MAX_READ_LINES);
}

#[tokio::test]
async fn a_line_over_the_byte_limit_says_how_to_move_past_it() {
    let temp = tempfile::tempdir().unwrap();
    let mut two_lines = vec![b'a'; MAX_READ_BYTES + 1];
    two_lines.extend_from_slice(b"\nshort\n");
    fs::write(temp.path().join("two-lines.txt"), two_lines).unwrap();
    let text = read_file(
        read("two-lines.txt", None, None),
        Some(temp.path()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        text.contains("Line 1 exceeds the 50KB read limit"),
        "{text}"
    );
    assert!(text.contains("Use offset=2 to continue."), "{text}");
    let rest = read_file(
        read("two-lines.txt", Some(2), None),
        Some(temp.path()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(rest, "short");
}

#[tokio::test]
async fn read_refuses_directories_and_images_and_honors_cancellation() {
    let temp = tempfile::tempdir().unwrap();
    let error = read_file(
        read(".", None, None),
        Some(temp.path()),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(error.contains("not a regular file"), "{error}");

    fs::write(temp.path().join("pixel.png"), b"\x89PNG\r\n\x1a\nrest").unwrap();
    let text = read_file(
        read("pixel.png", None, None),
        Some(temp.path()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(text.contains("Use read_image"), "{text}");

    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = read_file(read("pixel.png", None, None), Some(temp.path()), cancel)
        .await
        .unwrap_err();
    assert!(error.contains("cancelled"), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn special_files_are_refused_without_blocking() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::time::Duration;

    let temp = tempfile::tempdir().unwrap();
    let fifo = temp.path().join("agent.fifo");
    let fifo_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: `fifo_path` is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
    let within = Duration::from_secs(1);

    let read = tokio::time::timeout(
        within,
        read_file(
            read("agent.fifo", None, None),
            Some(temp.path()),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("read must not block on a FIFO");
    assert!(read.is_err());
    let edit = tokio::time::timeout(
        within,
        edit_file(
            edits("agent.fifo", &[("before", "after")]),
            Some(temp.path()),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("edit must not block on a FIFO");
    assert!(edit.is_err());
    let write = tokio::time::timeout(
        within,
        write_file(
            WriteParams {
                path: "agent.fifo".to_string(),
                content: "content".to_string(),
            },
            Some(temp.path()),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("write must not block on a FIFO");
    assert!(write.is_err());
}

#[tokio::test]
async fn edit_checks_every_replacement_before_writing() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("notes.txt");
    fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
    edit_file(
        edits("notes.txt", &[("alpha", "first"), ("gamma", "third")]),
        Some(temp.path()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "first\nbeta\nthird\n");

    let original = "alpha alpha beta";
    fs::write(&path, original).unwrap();
    let duplicate = edit_file(
        edits("notes.txt", &[("alpha", "first")]),
        Some(temp.path()),
        CancellationToken::new(),
    )
    .await;
    assert!(duplicate.unwrap_err().contains("more than once"));
    let overlap = edit_file(
        edits(
            "notes.txt",
            &[("alpha alpha", "first"), ("alpha beta", "second")],
        ),
        Some(temp.path()),
        CancellationToken::new(),
    )
    .await;
    assert!(overlap.unwrap_err().contains("overlapping"));
    let no_op = edit_file(
        edits("notes.txt", &[("beta", "beta")]),
        Some(temp.path()),
        CancellationToken::new(),
    )
    .await;
    assert!(no_op.unwrap_err().contains("would not change"));
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}

#[tokio::test]
async fn edit_keeps_a_files_bom_and_line_endings() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("windows.txt");
    fs::write(&path, "\u{feff}alpha\r\nbeta\r\n").unwrap();
    edit_file(
        edits("windows.txt", &[("alpha\nbeta", "first\nsecond")]),
        Some(temp.path()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "\u{feff}first\r\nsecond\r\n"
    );

    let mixed = temp.path().join("mixed.txt");
    fs::write(&mixed, "alpha\r\nbeta\ngamma\n").unwrap();
    let error = edit_file(
        edits("mixed.txt", &[("gamma", "third")]),
        Some(temp.path()),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(error.contains("mixed line endings"), "{error}");
    assert_eq!(
        fs::read_to_string(&mixed).unwrap(),
        "alpha\r\nbeta\ngamma\n"
    );
}

#[tokio::test]
async fn edit_refuses_oversized_files_before_reading_them() {
    let temp = tempfile::tempdir().unwrap();
    let file = fs::File::create(temp.path().join("oversized.txt")).unwrap();
    file.set_len(MAX_EDIT_BYTES as u64 + 1).unwrap();
    let error = edit_file(
        edits("oversized.txt", &[("before", "after")]),
        Some(temp.path()),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(error.contains("too large to edit safely"), "{error}");
}

#[test]
fn edits_may_arrive_as_the_json_text_of_an_array() {
    let params: EditParams = serde_json::from_value(serde_json::json!({
        "path": "notes.txt",
        "edits": "[{\"oldText\":\"a\",\"newText\":\"b\"}]",
    }))
    .unwrap();
    assert_eq!(params.edits.len(), 1);
}

#[tokio::test]
async fn write_creates_parents_and_reports_what_it_did() {
    let temp = tempfile::tempdir().unwrap();
    let created = write_file(
        WriteParams {
            path: "nested/dir/file.txt".to_string(),
            content: "héllo".to_string(),
        },
        Some(temp.path()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(created, "Created nested/dir/file.txt (6 bytes)");
    let rewritten = write_file(
        WriteParams {
            path: "nested/dir/file.txt".to_string(),
            content: "bye".to_string(),
        },
        Some(temp.path()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(rewritten, "Wrote nested/dir/file.txt (3 bytes)");
    assert_eq!(
        fs::read_to_string(temp.path().join("nested/dir/file.txt")).unwrap(),
        "bye"
    );
}
