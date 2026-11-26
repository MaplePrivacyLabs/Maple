//! Pictures a reply names in Markdown keep what they showed when it was saved.

use super::*;

fn png(width: u32, height: u32, pixel: [u8; 4]) -> Vec<u8> {
    let picture = image::RgbaImage::from_pixel(width, height, image::Rgba(pixel));
    let mut bytes = std::io::Cursor::new(Vec::new());
    picture
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

fn pictures(row: &crate::store::TimelineRow) -> Vec<crate::store::MarkdownImage> {
    row.item
        .input
        .as_ref()
        .and_then(|input| input.get(crate::store::MARKDOWN_IMAGES_KEY))
        .map(|value| serde_json::from_value(value.clone()).unwrap())
        .unwrap_or_default()
}

async fn replies(harness: &Harness, task: &str) -> Vec<crate::store::TimelineRow> {
    let page = harness
        .handle
        .open_task_page(
            crate::store::OpenTaskPageRequest {
                task: task.into(),
                limit_rows: None,
                limit_bytes: None,
            },
            crate::store::PageTransport::Remote,
        )
        .await
        .unwrap();
    page.rows
        .into_iter()
        .filter(|row| {
            row.item.item_type == "message" && row.item.role.as_deref() == Some("assistant")
        })
        .collect()
}

#[tokio::test]
async fn reply_pictures_are_saved_with_the_reply_and_survive_edits_and_rebuilds() {
    let harness = Harness::new().await;
    let task = harness.create_task().await;
    let (store, account) = harness.handle.test_persistence().unwrap();
    let project = harness.project.path();
    let first = png(2, 1, [255, 0, 0, 255]);
    let second = png(3, 1, [0, 0, 255, 255]);
    std::fs::create_dir(project.join("out")).unwrap();
    std::fs::write(project.join("out").join("My Shot.png"), &first).unwrap();
    std::fs::write(project.join("notes.txt"), b"not a picture").unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_picture = outside.path().join("outside.png");
    std::fs::write(&outside_picture, &first).unwrap();

    harness.faux.push_text(&format!(
        "Here it is:\n\n![first](out/My%20Shot.png)\n\n![notes](notes.txt)\n\n\
         ![outside]({})\n\n![web](https://example.com/x.png)",
        outside_picture.display()
    ));
    let mut run = harness.send(&task, "Show me the screenshot").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);

    // The agent overwrites its screenshot and shows it again.
    std::fs::write(project.join("out").join("My Shot.png"), &second).unwrap();
    harness
        .faux
        .push_text("Updated:\n\n![second](<out/My Shot.png>)");
    let mut run = harness.send(&task, "Again, after the fix").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);

    let rows = replies(&harness, &task).await;
    assert_eq!(rows.len(), 2);
    let (earlier, later) = (pictures(&rows[0]), pictures(&rows[1]));
    assert_eq!(earlier.len(), 1, "only the picture inside the project");
    assert_eq!(earlier[0].source, "out/My%20Shot.png");
    assert_eq!(earlier[0].name, "My Shot.png");
    assert_eq!(earlier[0].mime_type, "image/png");
    assert_eq!(later.len(), 1);
    assert_eq!(later[0].source, "out/My Shot.png");
    assert_ne!(earlier[0].id, later[0].id);
    for (picture, bytes) in [(&earlier[0], &first), (&later[0], &second)] {
        let range = store
            .read_attachment(account, &task, &picture.id, 0, 1024 * 1024)
            .unwrap();
        assert_eq!(&range.data, bytes, "each reply keeps its own picture");
    }

    // Deleting the file changes nothing, and a rebuild of the rows finds the
    // saved copies without reading the project again.
    std::fs::remove_file(project.join("out").join("My Shot.png")).unwrap();
    store.retire_task_timeline_for_test(account, &task).unwrap();
    harness.faux.push_text("Nothing to show.");
    let mut run = harness.send(&task, "Thanks").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    assert!(!store.history_needs_rebuild(account, &task).unwrap());
    let rebuilt = replies(&harness, &task).await;
    assert_eq!(rebuilt.len(), 3);
    assert_eq!(pictures(&rebuilt[0]), earlier);
    assert_eq!(pictures(&rebuilt[1]), later);
    assert!(rebuilt[2].item.input.is_none());
}
