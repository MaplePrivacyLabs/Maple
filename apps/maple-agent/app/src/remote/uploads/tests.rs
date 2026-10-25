use super::*;
use crate::remote::streams::tests::{command, endpoints};
use maple_agent::store::{AccountId, AttachmentUploadStatus, NewTask, Store, StorePaths, TaskKind};
use sha2::{Digest, Sha256};

const TASK: &str = "01234567-89ab-7def-8123-456789abcdef";
const OP: &str = "11111111-1111-4111-8111-111111111111";

fn store(root: &std::path::Path) -> (Arc<Store>, AccountId) {
    let store = Arc::new(Store::open(StorePaths::new(root.join("execution"))).unwrap());
    let account = store.upsert_account("account", 1).unwrap();
    store
        .create_task(
            account,
            &NewTask {
                uuid: TASK.into(),
                project_root: root.to_string_lossy().into_owned(),
                kind: TaskKind::Desktop,
                title: "Upload".into(),
                provisional: true,
                model: None,
                created_ms: 1,
            },
        )
        .unwrap();
    (store, account)
}

fn metadata(bytes: &[u8]) -> AttachmentUploadMetadata {
    AttachmentUploadMetadata {
        name: "notes.bin".into(),
        mime_type: "application/octet-stream".into(),
        size: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(bytes)),
    }
}

#[test]
fn upload_metadata_cannot_name_a_controller_or_executor_source_path() {
    let value = serde_json::json!({"user":"account","sessionId":TASK,"operationId":OP,"metadata":metadata(b"x"),"path":"/private/controller/secret"});
    assert!(serde_json::from_value::<Upload>(value).is_err());
    assert!(wire::Method::parse("import_attachment").is_err());
}

#[test]
fn bulk_budgets_leave_urgent_capacity_available() {
    let capacity = streams::Capacity::new();
    let _bulk: Vec<_> = (0..2)
        .map(|_| capacity.acquire(wire::Method::UploadAttachment).unwrap())
        .collect();
    let _ordinary: Vec<_> = (0..32)
        .map(|_| capacity.acquire(wire::Method::ListTasks).unwrap())
        .collect();
    assert!(capacity.acquire(wire::Method::UploadAttachment).is_err());
    assert!(capacity.acquire(wire::Method::CancelRun).is_ok());
    assert!(
        capacity
            .acquire(wire::Method::CancelAttachmentUpload)
            .is_ok()
    );
    assert_eq!(wire::MAX_FRAME, 16 * 1024 * 1024);
}

#[tokio::test]
async fn binary_file_larger_than_a_control_frame_round_trips_and_reuses_operation() {
    let root = tempfile::tempdir().unwrap();
    let (store, account) = store(root.path());
    let bytes = vec![0xabu8; wire::MAX_FRAME + 123];
    let path = root.path().join("staged.bin");
    std::fs::write(&path, &bytes).unwrap();
    let import = ImportAttachmentRequest {
        session_id: TASK.into(),
        operation_id: OP.into(),
        path: path.to_string_lossy().into_owned(),
        name: "notes.bin".into(),
        mime_type: "application/octet-stream".into(),
    };
    let (a, b, client, server) = endpoints().await;
    let destination = store.clone();
    let serving = tokio::spawn(async move {
        // The second import repeats metadata and returns its committed receipt
        // before admitting any bytes; it never creates a second attachment.
        for _ in 0..2 {
            let (mut tx, mut rx) = server.accept_bi().await.unwrap();
            let header = streams::Header::read(&mut rx).await.unwrap();
            assert_eq!(header.method, wire::Method::UploadAttachment);
            assert!(header.send_id.is_none());
            let upload: Upload = wire::read(&mut rx, wire::MAX_PREFACE).await.unwrap();
            assert_eq!(upload.metadata.size, (wire::MAX_FRAME + 123) as u64);
            match destination
                .begin_attachment_upload(account, TASK, OP, &upload.metadata)
                .unwrap()
            {
                AttachmentUploadStart::Complete(attachment) => {
                    send_admission(&mut tx, Ok(Admission::Complete(attachment)), true)
                        .await
                        .unwrap()
                }
                AttachmentUploadStart::Writing(writer) => {
                    send_admission(&mut tx, Ok(Admission::Ready), false)
                        .await
                        .unwrap();
                    let writer = receive_bytes(&mut rx, *writer).await.unwrap();
                    let attachment = writer.finish(&destination).unwrap();
                    streams::reply(
                        &mut tx,
                        &Message::Response {
                            id: header.id,
                            result: Ok(serde_json::to_value(attachment).unwrap()),
                        },
                    )
                    .await
                    .unwrap();
                }
            }
        }
    });
    let mut ids = Vec::new();
    for id in 1..=2 {
        let prepared = attachments::prepare(&import).await.unwrap();
        let response = call(&client, id, "account".into(), import.clone(), prepared)
            .await
            .unwrap();
        let Message::Response {
            result: Ok(value), ..
        } = response
        else {
            panic!("upload failed")
        };
        let result: StoredAttachment = serde_json::from_value(value).unwrap();
        assert_eq!(std::fs::read(&result.execution_path).unwrap(), bytes);
        assert_ne!(std::path::Path::new(&result.execution_path), path.as_path());
        ids.push(result.id);
    }
    assert_eq!(ids[0], ids[1]);
    serving.await.unwrap();
    assert!(matches!(
        store.attachment_upload_status(account, TASK, OP).unwrap(),
        AttachmentUploadStatus::Complete(_)
    ));
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn stalled_raw_upload_does_not_delay_stop_or_question_answer() {
    let root = tempfile::tempdir().unwrap();
    let (store, account) = store(root.path());
    let (a, b, client, server) = endpoints().await;
    let capacity = streams::Capacity::new();
    let (mut tx, mut rx) = client.open_bi().await.unwrap();
    wire::write(
        &mut tx,
        &streams::Header {
            id: 1,
            method: wire::Method::UploadAttachment,
            send_id: None,
        },
        wire::MAX_PREFACE,
    )
    .await
    .unwrap();
    let upload = Upload {
        user: "account".into(),
        session_id: TASK.into(),
        operation_id: OP.into(),
        metadata: metadata(&vec![7; 1024 * 1024]),
    };
    wire::write(&mut tx, &upload, wire::MAX_PREFACE)
        .await
        .unwrap();
    let (mut response, mut body) = server.accept_bi().await.unwrap();
    let header = streams::Header::read(&mut body).await.unwrap();
    let slot = capacity.acquire(header.method).unwrap();
    let received: Upload = wire::read(&mut body, wire::MAX_PREFACE).await.unwrap();
    let AttachmentUploadStart::Writing(writer) = store
        .begin_attachment_upload(account, TASK, OP, &received.metadata)
        .unwrap()
    else {
        panic!()
    };
    send_admission(&mut response, Ok(Admission::Ready), false)
        .await
        .unwrap();
    let _: Result<Admission, String> = wire::read(&mut rx, wire::MAX_MANAGEMENT_REPLY)
        .await
        .unwrap();
    tx.write_all(&[7; 128]).await.unwrap();
    let upload_task = tokio::spawn(async move {
        let _slot = slot;
        receive_bytes(&mut body, *writer).await
    });
    for (id, method) in [
        (2, wire::Method::CancelRun),
        (3, wire::Method::AnswerQuestion),
    ] {
        let caller = streams::call(&client, command(id, method));
        let answer = async {
            let (mut tx, mut rx) = server.accept_bi().await.unwrap();
            let header = streams::Header::read(&mut rx).await.unwrap();
            let _slot = capacity.acquire(header.method).unwrap();
            header.body(&mut rx).await.unwrap();
            streams::reply(
                &mut tx,
                &Message::Response {
                    id,
                    result: Ok(serde_json::Value::Null),
                },
            )
            .await
            .unwrap();
        };
        let (reply, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(caller, answer)
        })
        .await
        .unwrap();
        assert!(matches!(
            reply.unwrap(),
            Message::Response { result: Ok(_), .. }
        ));
        assert!(!upload_task.is_finished());
    }
    tx.reset(1u32.into()).unwrap();
    assert!(upload_task.await.unwrap().is_err());
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn raw_truncation_digest_mismatch_and_cancel_never_publish() {
    for kind in ["truncated", "digest", "cancelled", "oversized"] {
        let root = tempfile::tempdir().unwrap();
        let (store, account) = store(root.path());
        let (a, b, client, server) = endpoints().await;
        let declared = metadata(b"abcd");
        let AttachmentUploadStart::Writing(writer) = store
            .begin_attachment_upload(account, TASK, OP, &declared)
            .unwrap()
        else {
            panic!()
        };
        let (mut tx, _rx) = client.open_bi().await.unwrap();
        tx.write_all(if kind == "truncated" {
            b"ab"
        } else if kind == "oversized" {
            b"abcde"
        } else {
            b"zzzz"
        })
        .await
        .unwrap();
        tx.finish().unwrap();
        let (_reply, mut rx) = server.accept_bi().await.unwrap();
        if kind == "cancelled" {
            store.cancel_attachment_upload(account, TASK, OP).unwrap();
        }
        let result = match receive_bytes(&mut rx, *writer).await {
            Ok(writer) => writer.finish(&store).map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };
        assert!(result.is_err(), "{kind}");
        assert!(
            !matches!(
                store.attachment_upload_status(account, TASK, OP).unwrap(),
                AttachmentUploadStatus::Complete(_)
            ),
            "{kind}"
        );
        // A failed bulk request does not terminate the underlying connection.
        assert!(client.close_reason().is_none());
        a.close().await;
        b.close().await;
    }
}

#[tokio::test]
async fn lost_final_ack_is_resolved_by_the_upload_receipt_without_prompt_replay() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(crate::backend::attachments::tests::fixture(root.path()));
    let store = backend.store().unwrap().clone();
    let account = store.account_id("account").unwrap().unwrap();
    let path = root.path().join("stable-upload.bin");
    std::fs::write(&path, b"abcd").unwrap();
    let request = ImportAttachmentRequest {
        session_id: TASK.into(),
        operation_id: OP.into(),
        path: path.to_string_lossy().into_owned(),
        name: "notes.bin".into(),
        mime_type: "application/octet-stream".into(),
    };
    let (a, b, client, server) = endpoints().await;
    let serving = tokio::spawn(async move {
        let (mut tx, mut rx) = server.accept_bi().await.unwrap();
        let header = streams::Header::read(&mut rx).await.unwrap();
        assert_eq!(header.method, wire::Method::UploadAttachment);
        let upload: Upload = read_metadata(&mut rx, streams::IO_TIMEOUT).await.unwrap();
        let AttachmentUploadStart::Writing(writer) = store
            .begin_attachment_upload(account, TASK, OP, &upload.metadata)
            .unwrap()
        else {
            panic!()
        };
        send_admission(&mut tx, Ok(Admission::Ready), false)
            .await
            .unwrap();
        let writer = receive_bytes(&mut rx, *writer).await.unwrap();
        let attachment = writer.finish(&store).unwrap();
        // Lose the response after durable publication, not before admission.
        tx.reset(1u32.into()).unwrap();

        let (mut tx, mut rx) = server.accept_bi().await.unwrap();
        let header = streams::Header::read(&mut rx).await.unwrap();
        assert_eq!(header.method, wire::Method::AttachmentUploadStatus);
        let command = header.body(&mut rx).await.unwrap();
        let result = crate::backend::dispatch(
            &backend,
            crate::host::protocol::Request {
                id: command.id,
                method: command.method.name(),
                send_id: None,
                params: command.params,
            },
        )
        .await;
        streams::reply(
            &mut tx,
            &Message::Response {
                id: command.id,
                result,
            },
        )
        .await
        .unwrap();

        let (mut tx, mut rx) = server.accept_bi().await.unwrap();
        let header = streams::Header::read(&mut rx).await.unwrap();
        assert_eq!(header.method, wire::Method::UploadAttachment);
        let upload: Upload = read_metadata(&mut rx, streams::IO_TIMEOUT).await.unwrap();
        let AttachmentUploadStart::Complete(repeated) = store
            .begin_attachment_upload(account, TASK, OP, &upload.metadata)
            .unwrap()
        else {
            panic!("retry must not admit another byte transfer")
        };
        assert_eq!(attachment.id, repeated.id);
        send_admission(&mut tx, Ok(Admission::Complete(repeated)), true)
            .await
            .unwrap();
        // All three requests were uploads/status: none can enter send_message.
        attachment
    });
    assert!(
        call(
            &client,
            1,
            "account".into(),
            request.clone(),
            attachments::prepare(&request).await.unwrap()
        )
        .await
        .is_err()
    );
    let status = streams::call(
        &client,
        wire::Command {
            id: 2,
            method: wire::Method::AttachmentUploadStatus,
            send_id: None,
            params: serde_json::json!([
                "account",
                crate::backend::AttachmentUploadRequest {
                    session_id: TASK.into(),
                    operation_id: OP.into()
                }
            ]),
        },
    )
    .await
    .unwrap();
    let Message::Response {
        result: Ok(value), ..
    } = status
    else {
        panic!("status lookup failed")
    };
    let AttachmentUploadStatus::Complete(recovered) = serde_json::from_value(value).unwrap() else {
        panic!("committed receipt missing")
    };
    let retry = call(
        &client,
        3,
        "account".into(),
        request.clone(),
        attachments::prepare(&request).await.unwrap(),
    )
    .await
    .unwrap();
    let Message::Response {
        result: Ok(value), ..
    } = retry
    else {
        panic!("idempotent retry failed")
    };
    assert_eq!(
        serde_json::from_value::<StoredAttachment>(value)
            .unwrap()
            .id,
        recovered.id
    );
    assert_eq!(serving.await.unwrap().id, recovered.id);
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn cancelling_a_stalled_upload_releases_its_stream_without_waiting_for_idle_timeout() {
    let root = tempfile::tempdir().unwrap();
    let (store, account) = store(root.path());
    let (a, b, client, server) = endpoints().await;
    let AttachmentUploadStart::Writing(writer) = store
        .begin_attachment_upload(account, TASK, OP, &metadata(b"abcd"))
        .unwrap()
    else {
        panic!()
    };
    let (mut tx, _rx) = client.open_bi().await.unwrap();
    tx.write_all(b"a").await.unwrap();
    let (_response, mut body) = server.accept_bi().await.unwrap();
    let receiving = tokio::spawn(async move { receive_bytes(&mut body, *writer).await });
    store.cancel_attachment_upload(account, TASK, OP).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), receiving)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(error) if error=="Attachment upload cancelled"));
    assert_eq!(
        store.attachment_upload_status(account, TASK, OP).unwrap(),
        AttachmentUploadStatus::Cancelled
    );
    a.close().await;
    b.close().await;
}

#[tokio::test(start_paused = true)]
async fn slowly_dripped_upload_metadata_has_an_overall_deadline() {
    use tokio::io::AsyncWriteExt;
    let (mut tx, mut rx) = tokio::io::duplex(64);
    let send = tokio::spawn(async move {
        tx.write_u32(100).await.unwrap();
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if tx.write_all(b" ").await.is_err() {
                break;
            }
        }
    });
    let started = tokio::time::Instant::now();
    let error = read_metadata(&mut rx, Duration::from_secs(3))
        .await
        .unwrap_err();
    assert_eq!(error, "Attachment metadata timed out");
    assert_eq!(started.elapsed(), Duration::from_secs(3));
    send.abort();
}

#[test]
fn revocation_disable_and_repair_do_not_publish_an_upload_admitted_under_an_old_grant() {
    for mode in ["revoke", "disable", "re-pair", "account stopped"] {
        let root = tempfile::tempdir().unwrap();
        let (store, account) = store(root.path());
        let mut policy = crate::remote::store::Store::open(
            store.clone(),
            "local",
            "project",
            "unpackaged",
            "account",
        )
        .unwrap();
        let peer = iroh::SecretKey::generate().public();
        let grant = crate::remote::store::Peer {
            name: "Controller".into(),
            grant: [1; 16],
            controls_this_computer: true,
            can_connect: false,
            pending_confirmation: None,
        };
        policy.set_enabled(true).unwrap();
        policy.grant(peer, grant.clone()).unwrap();
        let AttachmentUploadStart::Writing(mut writer) = store
            .begin_attachment_upload(account, TASK, OP, &metadata(b"abcd"))
            .unwrap()
        else {
            panic!()
        };
        writer.write_chunk(b"abcd").unwrap();
        match mode {
            "revoke" => policy.revoke(peer).unwrap(),
            "disable" => policy.set_enabled(false).unwrap(),
            "re-pair" => {
                policy.revoke(peer).unwrap();
                let mut next = grant;
                next.grant = [2; 16];
                policy.grant(peer, next).unwrap();
            }
            _ => {}
        }
        assert!(
            finish_authorized(
                *writer,
                &store,
                &policy,
                peer,
                [1; 16],
                mode == "account stopped"
            )
            .is_err(),
            "{mode}"
        );
        assert!(
            !matches!(
                store.attachment_upload_status(account, TASK, OP).unwrap(),
                AttachmentUploadStatus::Complete(_)
            ),
            "{mode}"
        );
    }
}
