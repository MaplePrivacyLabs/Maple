//! Third-party image strings cannot poison the runtime's persistence barrier.

use super::*;

#[derive(Clone, Copy)]
enum Finish {
    DeleteTask,
    SignOut,
    Shutdown,
}

#[tokio::test]
async fn mcp_images_remain_saveable_and_do_not_block_lifecycle_operations() {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAX+XDSwAAAABJRU5ErkJggg==";
    let wrapped = format!("{}\n{}\n", &PNG[..76], &PNG[76..]);
    for data in [wrapped, "not base64!".into()] {
        for finish in [Finish::DeleteTask, Finish::SignOut, Finish::Shutdown] {
            let harness = Harness::new().await;
            let fake = mcp::fake_server::FakeServer::start(None).await;
            fake.add_image_tool();
            harness
                .handle
                .save_mcp_servers(vec![http_mcp_server("Fake", &fake.url, false)])
                .await
                .unwrap();
            harness
                .handle
                .set_project_mcp_server_enabled(
                    path_string(harness.project.path()),
                    "Fake".into(),
                    true,
                )
                .await
                .unwrap();
            let task = harness.create_task().await;
            harness.faux.push_message(vec![faux_tool_call(
                "mcp__fake__image",
                json!({"text": data}),
            )]);
            harness.faux.push_text("The tool returned its image.");
            let mut run = harness.send(&task, "Read the screenshot").await;
            assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
            assert_eq!(fake.calls().len(), 1);
            assert!(
                harness.faux.requests().iter().any(|request| {
                    request.context.messages.iter().any(|message| {
                        matches!(message, pi_ai::Message::ToolResult(result)
                        if result.content.iter().any(|block| matches!(block,
                            pi_ai::Content::Image(image) if image.data == data)))
                    })
                }),
                "the real MCP response reaches the provider without normalization loss"
            );

            let runtime = harness.runtime().await;
            let session = runtime.loaded_session(&task).await.unwrap();
            let retained = session.with_session(|manager| manager.entries().to_vec());
            assert!(!session.has_unpersisted());
            let store = harness.service.host.store.as_ref().unwrap();
            assert!(store.persistence_failure().is_none());
            harness.service.flush_persistence().await.unwrap();
            let persisted = store
                .load_session(
                    runtime.store.account(),
                    &task,
                    crate::store::SessionLoad::Full,
                )
                .unwrap()
                .entries;
            assert_eq!(persisted.len(), retained.len());
            let images: Vec<_> = persisted.iter().filter_map(|entry| match &entry.kind {
                pi_coding_agent::session::EntryKind::Message {
                    message: pi_coding_agent::SessionMessage::Llm(pi_ai::Message::ToolResult(result))
                } => Some(result),
                _ => None,
            }).flat_map(|result| {
                assert!(!result.is_error, "unavailable media does not turn a successful tool into failure");
                result.content.iter().filter_map(|block| match block {
                    pi_ai::Content::Image(image) => Some(image.data.clone()), _ => None,
                })
            }).collect();
            assert_eq!(images.len(), 1);
            if data == "not base64!" {
                assert_eq!(images[0], "maple-attachment-unavailable:retired");
            } else {
                assert!(images[0].starts_with("maple-attachment://"));
            }
            // The argument that happens to contain base64 is ordinary text;
            // only the typed image result is externalized.
            assert!(persisted.iter().any(|entry| match &entry.kind {
                pi_coding_agent::session::EntryKind::Message {
                    message:
                        pi_coding_agent::SessionMessage::Llm(pi_ai::Message::Assistant(message)),
                } => message.content.iter().any(|block| matches!(block,
                    pi_ai::AssistantContent::ToolCall(call) if call.arguments["text"] == data)),
                _ => false,
            }));
            // Exercise each lifecycle barrier while the image-bearing context
            // is still loaded; deleting it first would trivialize the others.
            match finish {
                Finish::DeleteTask => {
                    harness.handle.delete_session(task.clone()).await.unwrap();
                    assert!(runtime.loaded_session(&task).await.is_none());
                    assert!(
                        store
                            .task(runtime.store.account(), &task)
                            .unwrap()
                            .is_none()
                    );
                }
                Finish::SignOut => {
                    harness.service.sign_out(USER).await.unwrap();
                    assert!(harness.service.state.runtime.lock().await.is_none());
                }
                Finish::Shutdown => {
                    harness.service.shutdown().await.unwrap();
                    assert!(harness.service.state.runtime.lock().await.is_none());
                }
            }
            harness.service.shutdown().await.unwrap();
        }
    }
}
