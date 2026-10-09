//! The file tools, after Pi's tool tests.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use base64::Engine;
use pi_agent_core::{AgentTool, AgentToolResult, ToolInvocation, ToolUpdates};
use pi_ai::{Content, content_text};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::*;

fn context() -> ToolContext {
    ToolContext::new("Test")
}

async fn call(tool: &dyn AgentTool, args: Value) -> Result<AgentToolResult, String> {
    let args = tool.prepare_arguments(args.as_object().cloned().unwrap_or_default());
    tool.execute(ToolInvocation {
        call_id: "call".to_string(),
        args: Value::Object(args),
        cancel: CancellationToken::new(),
        updates: ToolUpdates::none(),
    })
    .await
    .map_err(|error| error.to_string())
}

fn text(result: &AgentToolResult) -> String {
    content_text(&result.content)
}

fn read_tool(dir: &Path) -> ReadTool {
    ReadTool::new(dir, ReadToolOptions::default(), context())
}

fn edit_tool(dir: &Path) -> EditTool {
    EditTool::new(dir, EditToolOptions::default(), context())
}

fn numbered(count: usize) -> String {
    (1..=count)
        .map(|i| format!("Line {i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn read_returns_a_small_file_whole() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("test.txt"), "Hello, world!\nLine 2\nLine 3").unwrap();
    let result = call(&read_tool(dir.path()), json!({"path": "test.txt"}))
        .await
        .unwrap();
    assert_eq!(text(&result), "Hello, world!\nLine 2\nLine 3");
    assert!(result.details.is_none());
}

#[tokio::test]
async fn read_reports_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let error = call(&read_tool(dir.path()), json!({"path": "nonexistent.txt"}))
        .await
        .unwrap_err();
    assert!(error.starts_with("Could not read"), "{error}");
}

#[tokio::test]
async fn read_pages_long_files_by_lines_and_bytes() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("large.txt"), numbered(2500)).unwrap();
    let result = call(&read_tool(dir.path()), json!({"path": "large.txt"}))
        .await
        .unwrap();
    let output = text(&result);
    assert!(output.contains("Line 2000"));
    assert!(!output.contains("Line 2001"));
    assert!(output.ends_with("[Showing lines 1-2000 of 2500. Use offset=2001 to continue.]"));
    let truncation = &result.details.unwrap()["truncation"];
    assert_eq!(truncation["truncatedBy"], "lines");
    assert_eq!(truncation["totalLines"], 2500);
    assert_eq!(truncation["outputLines"], 2000);

    let wide: Vec<String> = (1..=500)
        .map(|i| format!("Line {i}: {}", "x".repeat(200)))
        .collect();
    fs::write(dir.path().join("wide.txt"), wide.join("\n")).unwrap();
    let output = text(
        &call(&read_tool(dir.path()), json!({"path": "wide.txt"}))
            .await
            .unwrap(),
    );
    assert!(output.contains("Line 1:"));
    assert!(
        output.contains(" of 500 (50.0KB limit). Use offset="),
        "{}",
        &output[output.len() - 80..]
    );
}

#[tokio::test]
async fn read_honours_offset_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), numbered(100)).unwrap();
    let tool = read_tool(dir.path());

    let output = text(
        &call(&tool, json!({"path": "f.txt", "offset": 51}))
            .await
            .unwrap(),
    );
    assert!(!output.contains("Line 50\n"));
    assert!(output.starts_with("Line 51\n"));
    assert!(output.ends_with("Line 100"));

    let output = text(
        &call(&tool, json!({"path": "f.txt", "limit": 10}))
            .await
            .unwrap(),
    );
    assert!(output.contains("Line 10\n"));
    assert!(!output.contains("Line 11"));
    assert!(output.ends_with("[90 more lines in file. Use offset=11 to continue.]"));

    let output = text(
        &call(&tool, json!({"path": "f.txt", "offset": 41, "limit": 20}))
            .await
            .unwrap(),
    );
    assert!(output.starts_with("Line 41\n"));
    assert!(!output.contains("Line 61"));
    assert!(output.ends_with("[40 more lines in file. Use offset=61 to continue.]"));

    fs::write(dir.path().join("short.txt"), "Line 1\nLine 2\nLine 3").unwrap();
    let error = call(&tool, json!({"path": "short.txt", "offset": 100}))
        .await
        .unwrap_err();
    assert_eq!(error, "Offset 100 is beyond end of file (3 lines total)");
}

#[tokio::test]
async fn read_sends_images_by_their_bytes_not_their_names() {
    let dir = tempfile::tempdir().unwrap();
    let png = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAX+XDSwAAAABJRU5ErkJggg==")
        .unwrap();
    fs::write(dir.path().join("image.txt"), &png).unwrap();
    let result = call(&read_tool(dir.path()), json!({"path": "image.txt"}))
        .await
        .unwrap();
    assert!(
        matches!(&result.content[0], Content::Text(note) if note.text.starts_with("Read image file [image/png]"))
    );
    assert!(
        matches!(&result.content[1], Content::Image(image) if image.mime_type == "image/png" && !image.data.is_empty())
    );

    fs::write(dir.path().join("not-an-image.png"), "definitely not a png").unwrap();
    let result = call(&read_tool(dir.path()), json!({"path": "not-an-image.png"}))
        .await
        .unwrap();
    assert_eq!(text(&result), "definitely not a png");
    assert!(
        result
            .content
            .iter()
            .all(|block| matches!(block, Content::Text(_)))
    );
}

#[tokio::test]
async fn read_converts_a_bmp_to_png() {
    let dir = tempfile::tempdir().unwrap();
    let mut bmp = vec![0u8; 58];
    bmp[..2].copy_from_slice(b"BM");
    bmp[2..6].copy_from_slice(&58u32.to_le_bytes());
    bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
    bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
    bmp[18..22].copy_from_slice(&1i32.to_le_bytes());
    bmp[22..26].copy_from_slice(&1i32.to_le_bytes());
    bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
    bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
    bmp[34..38].copy_from_slice(&4u32.to_le_bytes());
    bmp[56] = 0xff;
    fs::write(dir.path().join("image.bmp"), bmp).unwrap();
    let result = call(&read_tool(dir.path()), json!({"path": "image.bmp"}))
        .await
        .unwrap();
    let note = text(&result);
    assert!(note.contains("Read image file [image/png]"), "{note}");
    assert!(
        note.contains("[Image converted from image/bmp to image/png.]"),
        "{note}"
    );
    let Content::Image(image) = &result.content[1] else {
        panic!("no image");
    };
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .unwrap()[0],
        0x89
    );
}

#[tokio::test]
async fn write_creates_the_file_and_its_folders() {
    let dir = tempfile::tempdir().unwrap();
    let tool = WriteTool::new(dir.path(), WriteToolOptions::default(), context());
    let result = call(
        &tool,
        json!({"path": "nested/dir/test.txt", "content": "Nested content"}),
    )
    .await
    .unwrap();
    assert_eq!(text(&result), "Successfully wrote to nested/dir/test.txt");
    assert_eq!(
        fs::read_to_string(dir.path().join("nested/dir/test.txt")).unwrap(),
        "Nested content"
    );
    assert!(result.details.is_none());
}

#[tokio::test]
async fn edit_replaces_text_and_describes_the_change() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("e.txt"), "Hello, world!").unwrap();
    let result = call(
        &edit_tool(dir.path()),
        json!({"path": "e.txt", "edits": [{"oldText": "world", "newText": "testing"}]}),
    )
    .await
    .unwrap();
    assert_eq!(text(&result), "Successfully replaced 1 block(s) in e.txt.");
    assert_eq!(
        fs::read_to_string(dir.path().join("e.txt")).unwrap(),
        "Hello, testing!"
    );
    let details = result.details.unwrap();
    assert!(details["diff"].as_str().unwrap().contains("testing"));
    let patch = details["patch"].as_str().unwrap();
    for part in ["--- ", "+++ ", "@@", "-Hello, world!", "+Hello, testing!"] {
        assert!(patch.contains(part), "{patch}");
    }
    assert_eq!(details["firstChangedLine"], 1);
}

#[tokio::test]
async fn edit_failures_name_the_problem_and_change_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let tool = edit_tool(dir.path());
    fs::write(dir.path().join("e.txt"), "foo foo foo").unwrap();
    let error = call(
        &tool,
        json!({"path": "e.txt", "edits": [{"oldText": "nonexistent", "newText": "x"}]}),
    )
    .await
    .unwrap_err();
    assert!(
        error.starts_with("Could not find the exact text in e.txt."),
        "{error}"
    );
    let error = call(
        &tool,
        json!({"path": "e.txt", "edits": [{"oldText": "foo", "newText": "bar"}]}),
    )
    .await
    .unwrap_err();
    assert!(
        error.starts_with("Found 3 occurrences of the text in e.txt."),
        "{error}"
    );
    let error = call(
        &tool,
        json!({"path": "missing.txt", "edits": [{"oldText": "a", "newText": "b"}]}),
    )
    .await
    .unwrap_err();
    assert!(
        error.starts_with("Could not edit file: missing.txt."),
        "{error}"
    );
    let error = call(&tool, json!({"path": "e.txt", "edits": []}))
        .await
        .unwrap_err();
    assert!(
        error.contains("edits must contain at least one replacement"),
        "{error}"
    );

    fs::write(dir.path().join("o.txt"), "one\ntwo\nthree\n").unwrap();
    let error = call(
        &tool,
        json!({"path": "o.txt", "edits": [
            {"oldText": "one\ntwo\n", "newText": "ONE\nTWO\n"},
            {"oldText": "two\nthree\n", "newText": "TWO\nTHREE\n"}
        ]}),
    )
    .await
    .unwrap_err();
    assert!(error.contains("overlap"), "{error}");

    fs::write(dir.path().join("p.txt"), "alpha\nbeta\ngamma\n").unwrap();
    let error = call(
        &tool,
        json!({"path": "p.txt", "edits": [
            {"oldText": "alpha\n", "newText": "ALPHA\n"},
            {"oldText": "missing\n", "newText": "MISSING\n"}
        ]}),
    )
    .await
    .unwrap_err();
    assert!(error.starts_with("Could not find edits[1]"), "{error}");
    assert_eq!(
        fs::read_to_string(dir.path().join("p.txt")).unwrap(),
        "alpha\nbeta\ngamma\n"
    );
}

#[tokio::test]
async fn edit_applies_several_edits_against_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let tool = edit_tool(dir.path());
    fs::write(dir.path().join("m.txt"), "alpha\nbeta\ngamma\ndelta\n").unwrap();
    let result = call(
        &tool,
        json!({"path": "m.txt", "edits": [
            {"oldText": "alpha\n", "newText": "ALPHA\n"},
            {"oldText": "gamma\n", "newText": "GAMMA\n"}
        ]}),
    )
    .await
    .unwrap();
    assert!(text(&result).contains("Successfully replaced 2 block(s)"));
    assert_eq!(
        fs::read_to_string(dir.path().join("m.txt")).unwrap(),
        "ALPHA\nbeta\nGAMMA\ndelta\n"
    );

    fs::write(dir.path().join("i.txt"), "foo\nbar\nbaz\n").unwrap();
    call(
        &tool,
        json!({"path": "i.txt", "edits": [
            {"oldText": "foo\n", "newText": "foo bar\n"},
            {"oldText": "bar\n", "newText": "BAR\n"}
        ]}),
    )
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("i.txt")).unwrap(),
        "foo bar\nBAR\nbaz\n"
    );
}

#[tokio::test]
async fn edit_diffs_collapse_large_unchanged_gaps() {
    let dir = tempfile::tempdir().unwrap();
    let lines: Vec<String> = (1..=600).map(|i| format!("line {i:03}")).collect();
    fs::write(dir.path().join("g.txt"), format!("{}\n", lines.join("\n"))).unwrap();
    let result = call(
        &edit_tool(dir.path()),
        json!({"path": "g.txt", "edits": [
            {"oldText": "line 100\n", "newText": "LINE 100\n"},
            {"oldText": "line 300\n", "newText": "LINE 300\n"},
            {"oldText": "line 500\n", "newText": "LINE 500\n"}
        ]}),
    )
    .await
    .unwrap();
    let details = result.details.unwrap();
    let diff = details["diff"].as_str().unwrap();
    for part in ["LINE 100", "LINE 300", "LINE 500", "..."] {
        assert!(diff.contains(part), "{diff}");
    }
    assert!(!diff.contains("line 250"));
    assert!(diff.lines().count() < 50);
}

#[tokio::test]
async fn edit_matches_loosely_when_the_exact_text_is_not_there() {
    let dir = tempfile::tempdir().unwrap();
    let tool = edit_tool(dir.path());
    let cases = [
        (
            "line one   \nline two  \nline three\n",
            "line one\nline two\n",
            "replaced\n",
            "replaced\nline three\n",
        ),
        (
            "你好，世界\n你好（世界）\n",
            "你好,世界\n你好(世界)\n",
            "你好，pi\n你好(pi)\n",
            "你好，pi\n你好(pi)\n",
        ),
        (
            "ＡＢＣ１２３\ncafe\u{301}\n",
            "ABC123\ncafé\n",
            "XYZ789\ncoffee\n",
            "XYZ789\ncoffee\n",
        ),
        (
            "console.log(\u{2018}hello\u{2019});\n",
            "console.log('hello');",
            "console.log('world');",
            "console.log('world');\n",
        ),
        (
            "const msg = \u{201C}Hello World\u{201D};\n",
            "const msg = \"Hello World\";",
            "const msg = \"Goodbye\";",
            "const msg = \"Goodbye\";\n",
        ),
        (
            "range: 1\u{2013}5\nbreak\u{2014}here\n",
            "range: 1-5\nbreak-here",
            "range: 10-50\nbreak--here",
            "range: 10-50\nbreak--here\n",
        ),
        (
            "hello\u{A0}world\n",
            "hello world",
            "hello universe",
            "hello universe\n",
        ),
        (
            "const x = 'exact';\nconst y = 'other';\n",
            "const x = 'exact';",
            "const x = 'changed';",
            "const x = 'changed';\nconst y = 'other';\n",
        ),
    ];
    for (original, old, new, expected) in cases {
        fs::write(dir.path().join("f.txt"), original).unwrap();
        call(
            &tool,
            json!({"path": "f.txt", "edits": [{"oldText": old, "newText": new}]}),
        )
        .await
        .unwrap_or_else(|error| panic!("{old:?}: {error}"));
        assert_eq!(
            fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            expected,
            "{old:?}"
        );
    }

    fs::write(dir.path().join("d.txt"), "hello world   \nhello world\n").unwrap();
    let error = call(
        &tool,
        json!({"path": "d.txt", "edits": [{"oldText": "hello world", "newText": "x"}]}),
    )
    .await
    .unwrap_err();
    assert!(error.starts_with("Found 2 occurrences"), "{error}");

    let original = "replace me   \nafter   \n";
    fs::write(dir.path().join("p.txt"), original).unwrap();
    call(
        &tool,
        json!({"path": "p.txt", "edits": [{"oldText": "replace me\n", "newText": "after\n"}]}),
    )
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("p.txt")).unwrap(),
        "after\nafter   \n"
    );
}

#[tokio::test]
async fn edit_keeps_line_endings_and_the_byte_order_mark() {
    let dir = tempfile::tempdir().unwrap();
    let tool = edit_tool(dir.path());
    let cases = [
        (
            "first\r\nsecond\r\nthird\r\n",
            "first\r\nREPLACED\r\nthird\r\n",
        ),
        ("first\nsecond\nthird\n", "first\nREPLACED\nthird\n"),
        (
            "\u{FEFF}first\r\nsecond\r\nthird\r\n",
            "\u{FEFF}first\r\nREPLACED\r\nthird\r\n",
        ),
    ];
    for (original, expected) in cases {
        fs::write(dir.path().join("f.txt"), original).unwrap();
        call(
            &tool,
            json!({"path": "f.txt", "edits": [{"oldText": "second\n", "newText": "REPLACED\n"}]}),
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            expected
        );
    }

    fs::write(
        dir.path().join("mixed.txt"),
        "hello\r\nworld\r\n---\r\nhello\nworld\n",
    )
    .unwrap();
    let error = call(
        &tool,
        json!({"path": "mixed.txt", "edits": [{"oldText": "hello\nworld\n", "newText": "replaced\n"}]}),
    )
    .await
    .unwrap_err();
    assert!(error.starts_with("Found 2 occurrences"), "{error}");
}

#[tokio::test]
async fn edit_accepts_the_other_argument_shapes_models_send() {
    let dir = tempfile::tempdir().unwrap();
    let tool = edit_tool(dir.path());
    let shapes = [
        json!({"path": "f.txt", "edits": "[{\"oldText\": \"a\", \"newText\": \"b\"}]"}),
        json!({"path": "f.txt", "edits": "{\"oldText\": \"a\", \"newText\": \"b\"}"}),
        json!({"path": "f.txt", "edits": {"oldText": "a", "newText": "b"}}),
        json!({"path": "f.txt", "oldText": "a", "newText": "b"}),
    ];
    for args in shapes {
        fs::write(dir.path().join("f.txt"), "a\n").unwrap();
        call(&tool, args.clone())
            .await
            .unwrap_or_else(|error| panic!("{args}: {error}"));
        assert_eq!(
            fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "b\n",
            "{args}"
        );
    }
}

#[tokio::test]
async fn edit_reports_the_operations_own_access_error() {
    struct Offline;
    #[async_trait::async_trait]
    impl EditOperations for Offline {
        async fn read_file(&self, _path: &Path) -> std::io::Result<Vec<u8>> {
            Ok(b"hello\n".to_vec())
        }
        async fn write_file(&self, _path: &Path, _content: &str) -> std::io::Result<()> {
            Ok(())
        }
        async fn access(&self, _path: &Path) -> std::io::Result<()> {
            Err(std::io::Error::other("disk offline"))
        }
    }
    let tool = EditTool::new(
        "/",
        EditToolOptions {
            operations: Some(std::sync::Arc::new(Offline)),
        },
        context(),
    );
    let error = call(
        &tool,
        json!({"path": "broken.txt", "edits": [{"oldText": "hello", "newText": "world"}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(error, "Could not edit file: broken.txt. disk offline.");
}

#[test]
fn every_built_in_tool_is_created_with_the_defaults_active() {
    let dir = tempfile::tempdir().unwrap();
    let active: Vec<String> = DEFAULT_TOOL_NAMES
        .iter()
        .map(|name| name.to_string())
        .collect();
    let tools = create_all_tools(dir.path(), &ToolsOptions::default(), &context(), &active);
    let names: Vec<(&str, bool)> = tools
        .iter()
        .map(|tool| (tool.tool.name(), tool.active))
        .collect();
    assert_eq!(
        names,
        [
            ("read", true),
            ("bash", true),
            ("powershell", false),
            ("edit", true),
            ("write", true),
            ("grep", false),
            ("find", false),
            ("ls", false)
        ]
    );
    let bash = &tools[1].prompt;
    assert_eq!(
        bash.snippet.as_deref(),
        Some("Execute bash commands (ls, grep, find, etc.)")
    );
    assert_eq!(
        bash.guidelines,
        ["You can inspect TEST_* environment variables for current model and session details."]
    );
}

fn grep_tool(dir: &Path) -> GrepTool {
    GrepTool::new(dir, GrepToolOptions::default(), context())
}

fn find_tool(dir: &Path) -> FindTool {
    FindTool::new(dir, FindToolOptions::default(), context())
}

fn ls_tool(dir: &Path) -> LsTool {
    LsTool::new(dir, LsToolOptions::default(), context())
}

fn path_arg(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[tokio::test]
async fn grep_names_the_file_it_searched() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("example.txt");
    fs::write(&file, "first line\nmatch line\nlast line").unwrap();
    let result = call(
        &grep_tool(dir.path()),
        json!({"pattern": "match", "path": path_arg(&file)}),
    )
    .await
    .unwrap();
    assert_eq!(text(&result), "example.txt:2: match line");
    assert!(result.details.is_none());
}

#[tokio::test]
async fn grep_stops_at_the_limit_and_shows_context() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("context.txt"),
        "before\nmatch one\nafter\nmiddle\nmatch two\nafter two",
    )
    .unwrap();
    let result = call(
        &grep_tool(dir.path()),
        json!({"pattern": "match", "path": "context.txt", "limit": 1, "context": 1}),
    )
    .await
    .unwrap();
    assert_eq!(
        text(&result),
        "context.txt-1- before\ncontext.txt:2: match one\ncontext.txt-3- after\n\n\
         [1 matches limit reached. Use limit=2 for more, or refine pattern]"
    );
    assert_eq!(result.details.unwrap()["matchLimitReached"], 1);
}

#[tokio::test]
async fn grep_takes_flag_like_patterns_as_text() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("grep-injection-marker");
    let payload = dir.path().join("payload.sh");
    fs::write(
        &payload,
        format!(
            "#!/bin/sh\necho executed > {}\ncat \"$1\"\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::write(dir.path().join("target.txt"), "target\n").unwrap();
    let tool = grep_tool(dir.path());
    // The payload's name, not its path: a Windows path is not a valid pattern.
    let result = call(&tool, json!({"pattern": "--pre=payload.sh"}))
        .await
        .unwrap();
    assert_eq!(text(&result), "No matches found");
    assert!(!marker.exists());

    fs::write(dir.path().join("flags.txt"), "run with --verbose\n").unwrap();
    let result = call(&tool, json!({"pattern": "--verbose"})).await.unwrap();
    assert_eq!(text(&result), "flags.txt:1: run with --verbose");
}

#[tokio::test]
async fn grep_searches_a_folder_by_ripgreps_rules() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for folder in [".git", ".hidden", "build", "src"] {
        fs::create_dir(root.join(folder)).unwrap();
    }
    fs::write(root.join(".git/config"), "needle").unwrap();
    fs::write(root.join(".gitignore"), "ignored.txt\nbuild/\n").unwrap();
    fs::write(root.join("ignored.txt"), "needle").unwrap();
    fs::write(root.join("build/out.txt"), "needle").unwrap();
    fs::write(root.join(".hidden/secret.txt"), "needle").unwrap();
    fs::write(root.join("binary.bin"), b"\0binary\nneedle\n").unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n// needle here\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn needle() {}\r\n").unwrap();
    let tool = grep_tool(root);

    let result = call(&tool, json!({"pattern": "needle"})).await.unwrap();
    assert_eq!(
        text(&result),
        ".hidden/secret.txt:1: needle\nsrc/lib.rs:1: pub fn needle() {}\nsrc/main.rs:2: // needle here"
    );

    let only_rust = call(&tool, json!({"pattern": "needle", "glob": "*.rs"}))
        .await
        .unwrap();
    assert_eq!(
        text(&only_rust),
        "src/lib.rs:1: pub fn needle() {}\nsrc/main.rs:2: // needle here"
    );
    let one_file = call(&tool, json!({"pattern": "needle", "glob": "**/main.rs"}))
        .await
        .unwrap();
    assert_eq!(text(&one_file), "src/main.rs:2: // needle here");

    // A file named directly is searched even when ignored.
    let named = call(&tool, json!({"pattern": "needle", "path": "ignored.txt"}))
        .await
        .unwrap();
    assert_eq!(text(&named), "ignored.txt:1: needle");
}

#[tokio::test]
async fn grep_respects_gitignore_outside_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(dir.path().join("ignored.txt"), "needle").unwrap();
    fs::write(dir.path().join("kept.txt"), "needle").unwrap();
    let result = call(&grep_tool(dir.path()), json!({"pattern": "needle"}))
        .await
        .unwrap();
    assert_eq!(text(&result), "kept.txt:1: needle");
}

#[tokio::test]
async fn grep_takes_literal_and_case_options_and_reports_bad_input() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file.txt"), "a.c\nabc\nA.C\n").unwrap();
    let tool = grep_tool(dir.path());
    let regex = call(&tool, json!({"pattern": "a.c"})).await.unwrap();
    assert_eq!(text(&regex), "file.txt:1: a.c\nfile.txt:2: abc");
    let literal = call(&tool, json!({"pattern": "a.c", "literal": true}))
        .await
        .unwrap();
    assert_eq!(text(&literal), "file.txt:1: a.c");
    let any_case = call(
        &tool,
        json!({"pattern": "a.c", "literal": true, "ignoreCase": true}),
    )
    .await
    .unwrap();
    assert_eq!(text(&any_case), "file.txt:1: a.c\nfile.txt:3: A.C");

    let bad = call(&tool, json!({"pattern": "("})).await.unwrap_err();
    assert!(bad.contains("regex parse error"), "{bad}");
    let missing = call(&tool, json!({"pattern": "x", "path": "missing"}))
        .await
        .unwrap_err();
    assert_eq!(
        missing,
        format!("Path not found: {}", dir.path().join("missing").display())
    );
}

#[tokio::test]
async fn grep_cuts_long_lines_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("long.txt"), "x".repeat(600) + "needle").unwrap();
    let result = call(&grep_tool(dir.path()), json!({"pattern": "needle"}))
        .await
        .unwrap();
    assert_eq!(
        text(&result),
        format!(
            "long.txt:1: {}... [truncated]\n\n\
             [Some lines truncated to 500 chars. Use read tool to see full lines]",
            "x".repeat(500)
        )
    );
    assert_eq!(result.details.unwrap()["linesTruncated"], true);
}

#[tokio::test]
async fn find_includes_hidden_files_that_are_not_ignored() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".secret")).unwrap();
    fs::write(dir.path().join(".secret/hidden.txt"), "hidden").unwrap();
    fs::write(dir.path().join("visible.txt"), "visible").unwrap();
    let result = call(
        &find_tool(dir.path()),
        json!({"pattern": "**/*.txt", "path": path_arg(dir.path())}),
    )
    .await
    .unwrap();
    assert_eq!(text(&result), ".secret/hidden.txt\nvisible.txt");
}

#[tokio::test]
async fn find_respects_gitignore_outside_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(dir.path().join("ignored.txt"), "ignored").unwrap();
    fs::write(dir.path().join("kept.txt"), "kept").unwrap();
    let result = call(&find_tool(dir.path()), json!({"pattern": "**/*.txt"}))
        .await
        .unwrap();
    assert_eq!(text(&result), "kept.txt");
}

#[tokio::test]
async fn find_keeps_a_nested_repository_apart_from_its_parent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("sub/.git")).unwrap();
    fs::write(root.join(".gitignore"), "*.dat\n").unwrap();
    fs::write(root.join("a.dat"), "").unwrap();
    fs::write(root.join("sub/b.dat"), "").unwrap();
    let result = call(&find_tool(root), json!({"pattern": "*.dat"}))
        .await
        .unwrap();
    assert_eq!(text(&result), "sub/b.dat");
}

#[tokio::test]
async fn find_reports_glob_errors_and_takes_flags_as_patterns() {
    let dir = tempfile::tempdir().unwrap();
    let tool = find_tool(dir.path());
    let error = call(&tool, json!({"pattern": "["})).await.unwrap_err();
    assert!(error.contains("error parsing glob"), "{error}");
    let flag = call(&tool, json!({"pattern": "--help"})).await.unwrap();
    assert_eq!(text(&flag), "No files found matching pattern");
    let missing = call(&tool, json!({"pattern": "*", "path": "missing"}))
        .await
        .unwrap_err();
    assert_eq!(
        missing,
        format!("Path not found: {}", dir.path().join("missing").display())
    );
}

#[tokio::test]
async fn find_matches_names_paths_and_folders_like_fd() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for folder in ["crates/a/src", "docs", "node_modules/pkg", "src"] {
        fs::create_dir_all(root.join(folder)).unwrap();
    }
    for file in [
        "README.md",
        "docs/readme.txt",
        "crates/a/src/lib.rs",
        "node_modules/pkg/index.js",
        "src/main.rs",
    ] {
        fs::write(root.join(file), "").unwrap();
    }
    let tool = find_tool(root);
    let found = |pattern: &'static str| {
        let tool = &tool;
        async move { text(&call(tool, json!({ "pattern": pattern })).await.unwrap()) }
    };
    assert_eq!(found("*.rs").await, "crates/a/src/lib.rs\nsrc/main.rs");
    assert_eq!(found("src/*.rs").await, "crates/a/src/lib.rs\nsrc/main.rs");
    assert_eq!(found("./*.md").await, "README.md");
    assert_eq!(found("crates/**/lib.rs").await, "crates/a/src/lib.rs");
    assert_eq!(found("src").await, "crates/a/src/\nsrc/");
    assert_eq!(found("readme*").await, "README.md\ndocs/readme.txt");
    assert_eq!(found("README*").await, "README.md");
    assert_eq!(found("*.js").await, "No files found matching pattern");

    let limited = call(&tool, json!({"pattern": "*", "limit": 2}))
        .await
        .unwrap();
    assert_eq!(
        text(&limited),
        "README.md\ncrates/\n\n[2 results limit reached. Use limit=4 for more, or refine pattern]"
    );
    assert_eq!(limited.details.unwrap()["resultLimitReached"], 2);
}

#[tokio::test]
async fn find_relativizes_what_custom_operations_return() {
    struct Listed {
        results: Vec<String>,
        asked: std::sync::Mutex<Option<FindGlobOptions>>,
    }

    #[async_trait::async_trait]
    impl FindOperations for Listed {
        async fn exists(&self, _path: &Path) -> bool {
            true
        }

        async fn glob(
            &self,
            _pattern: &str,
            _search_path: &Path,
            options: &FindGlobOptions,
        ) -> std::io::Result<Vec<String>> {
            *self.asked.lock().unwrap() = Some(options.clone());
            Ok(self.results.clone())
        }
    }

    let root = std::env::temp_dir().join("remote-root");
    let operations = Arc::new(Listed {
        results: vec![
            format!("{}", root.join("src").join("a.rs").display()),
            format!("{}/", root.join("src").display()),
            "rel/b.rs".to_string(),
        ],
        asked: std::sync::Mutex::new(None),
    });
    let tool = FindTool::new(
        &root,
        FindToolOptions {
            operations: Some(operations.clone()),
        },
        context(),
    );
    let result = call(&tool, json!({"pattern": "*.rs", "limit": 10}))
        .await
        .unwrap();
    assert_eq!(text(&result), "src/a.rs\nsrc/\nrel/b.rs");
    let asked = operations.asked.lock().unwrap().clone().unwrap();
    assert_eq!(asked.ignore, ["**/node_modules/**", "**/.git/**"]);
    assert_eq!(asked.limit, 10);
}

#[tokio::test]
async fn ls_lists_dotfiles_and_folders_sorted() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(".hidden-file"), "secret").unwrap();
    fs::create_dir(dir.path().join(".hidden-dir")).unwrap();
    fs::write(dir.path().join("B.txt"), "").unwrap();
    fs::write(dir.path().join("a.txt"), "").unwrap();
    fs::create_dir(dir.path().join("c")).unwrap();
    let result = call(&ls_tool(dir.path()), json!({})).await.unwrap();
    assert_eq!(
        text(&result),
        ".hidden-dir/\n.hidden-file\na.txt\nB.txt\nc/"
    );
    assert!(result.details.is_none());
}

#[tokio::test]
async fn ls_stops_at_the_limit_and_names_bad_paths() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a", "b", "c"] {
        fs::write(dir.path().join(name), "").unwrap();
    }
    fs::create_dir(dir.path().join("empty")).unwrap();
    let tool = ls_tool(dir.path());
    let limited = call(&tool, json!({"limit": 2})).await.unwrap();
    assert_eq!(
        text(&limited),
        "a\nb\n\n[2 entries limit reached. Use limit=4 for more]"
    );
    assert_eq!(limited.details.unwrap()["entryLimitReached"], 2);

    let empty = call(&tool, json!({"path": "empty"})).await.unwrap();
    assert_eq!(text(&empty), "(empty directory)");
    let file = call(&tool, json!({"path": "a"})).await.unwrap_err();
    assert_eq!(
        file,
        format!("Not a directory: {}", dir.path().join("a").display())
    );
    let missing = call(&tool, json!({"path": "missing"})).await.unwrap_err();
    assert_eq!(
        missing,
        format!("Path not found: {}", dir.path().join("missing").display())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn ls_leaves_out_broken_links() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("real.txt"), "").unwrap();
    std::os::unix::fs::symlink(dir.path().join("gone"), dir.path().join("broken")).unwrap();
    let result = call(&ls_tool(dir.path()), json!({})).await.unwrap();
    assert_eq!(text(&result), "real.txt");
}
