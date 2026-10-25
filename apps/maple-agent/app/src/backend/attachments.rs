//! Controller-local imports and execution-host upload receipts. Only the local
//! IPC endpoint accepts a filesystem path; remote upload metadata never does.
use super::{AgentBackend, LocalBackend};
use crate::host::protocol::Request;
use maple_agent::store::{AccountId, Store};
use maple_agent::store::{AttachmentUploadMetadata, AttachmentUploadStart};
pub use maple_agent::store::{AttachmentUploadStatus, StoredAttachment};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, SeekFrom},
    sync::Arc,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportAttachmentRequest {
    pub session_id: String,
    pub operation_id: String,
    pub path: String,
    pub name: String,
    pub mime_type: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttachmentUploadRequest {
    pub session_id: String,
    pub operation_id: String,
}

#[derive(Debug)]
pub(crate) struct PreparedImport {
    pub file: std::fs::File,
    pub metadata: AttachmentUploadMetadata,
}

/// Hash and transfer the same open regular file. A changed source cannot be
/// silently published: the execution store validates the advertised digest.
pub(crate) async fn prepare(request: &ImportAttachmentRequest) -> Result<PreparedImport, String> {
    let request = request.clone();
    tokio::task::spawn_blocking(move || {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // A caller-controlled FIFO must not trap the import worker before
            // we can verify that the opened object is a regular file.
            options.custom_flags(libc::O_NONBLOCK);
        }
        let mut file = options
            .open(&request.path)
            .map_err(|_| "The staged attachment could not be opened")?;
        let attributes = file
            .metadata()
            .map_err(|_| "Could not inspect the attachment")?;
        if !attributes.is_file()
            || attributes.len() > maple_agent::store::MAX_ATTACHMENT_BYTES as u64
        {
            return Err("Attachments must be regular files of at most 30 MiB".into());
        }
        let mut hash = Sha256::new();
        let mut count = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|_| "Could not read the attachment")?;
            if read == 0 {
                break;
            }
            count += read as u64;
            if count > maple_agent::store::MAX_ATTACHMENT_BYTES as u64 {
                return Err("Attachment exceeds the 30 MiB limit".into());
            }
            hash.update(&buffer[..read]);
        }
        if count != attributes.len() {
            return Err("Attachment changed while preparing it".into());
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|_| "Could not read the attachment")?;
        Ok(PreparedImport {
            file,
            metadata: AttachmentUploadMetadata {
                name: request.name,
                mime_type: request.mime_type,
                size: count,
                sha256: format!("{:x}", hash.finalize()),
            },
        })
    })
    .await
    .map_err(|_| "Attachment worker did not complete".to_string())?
}

impl AgentBackend {
    pub async fn import_attachment(
        &self,
        user: &str,
        request: ImportAttachmentRequest,
    ) -> Result<StoredAttachment, String> {
        self.call("import_attachment", (user, request)).await
    }
    pub async fn attachment_upload_status(
        &self,
        user: &str,
        request: AttachmentUploadRequest,
    ) -> Result<AttachmentUploadStatus, String> {
        self.call("attachment_upload_status", (user, request)).await
    }
    pub async fn cancel_attachment_upload(
        &self,
        user: &str,
        request: AttachmentUploadRequest,
    ) -> Result<(), String> {
        self.call("cancel_attachment_upload", (user, request)).await
    }
}

pub(crate) fn handles(method: &str) -> bool {
    matches!(
        method,
        "import_attachment" | "attachment_upload_status" | "cancel_attachment_upload"
    )
}

pub(crate) async fn context(
    backend: &LocalBackend,
    user: &str,
) -> Result<(Arc<Store>, AccountId), String> {
    backend.authorize_account(user)?;
    let store = backend
        .store()
        .cloned()
        .ok_or("Local store is unavailable")?;
    let account_store = store.clone();
    let user = user.to_owned();
    let account = blocking(move || {
        account_store
            .account_id(&user)?
            .ok_or(maple_agent::store::StoreError::NotFound)
    })
    .await?;
    Ok((store, account))
}

pub(crate) async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> maple_agent::store::StoreResult<T> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| "Attachment worker did not complete".to_string())?
        .map_err(|error| error.to_string())
}

pub(crate) async fn dispatch(
    backend: &LocalBackend,
    request: Request,
) -> Result<serde_json::Value, String> {
    if request.method == "import_attachment" {
        let (user, request): (String, ImportAttachmentRequest) =
            serde_json::from_value(request.params)
                .map_err(|_| "Invalid attachment import parameters")?;
        backend.authorize_account(&user)?;
        let prepared = prepare(&request).await?;
        let (store, start) = {
            let _account = backend.account_gate.read().await;
            let (store, account) = context(backend, &user).await?;
            let target = store.clone();
            let session = request.session_id;
            let operation = request.operation_id;
            let metadata = prepared.metadata;
            let start = blocking(move || {
                target.begin_attachment_upload(account, &session, &operation, &metadata)
            })
            .await?;
            (store, start)
        };
        let attachment = match start {
            AttachmentUploadStart::Complete(attachment) => attachment,
            AttachmentUploadStart::Writing(mut writer) => {
                let mut file = prepared.file;
                // No account or SQL lock spans the bulk copy.
                let writer = tokio::task::spawn_blocking(move || {
                    let mut buffer = [0u8; 64 * 1024];
                    loop {
                        let n = file
                            .read(&mut buffer)
                            .map_err(|_| "Could not read staged attachment".to_string())?;
                        if n == 0 {
                            break;
                        }
                        writer
                            .write_chunk(&buffer[..n])
                            .map_err(|e| e.to_string())?;
                    }
                    Ok::<_, String>(writer)
                })
                .await
                .map_err(|_| "Attachment worker did not complete")??;
                let _account = backend.account_gate.read().await;
                backend.authorize_account(&user)?;
                blocking(move || writer.finish(&store)).await?
            }
        };
        return serde_json::to_value(attachment).map_err(|_| "Could not encode attachment".into());
    }
    let (user, upload): (String, AttachmentUploadRequest) =
        serde_json::from_value(request.params).map_err(|_| "Invalid attachment operation")?;
    // Remote dispatch already holds a read-side account guard. The caller of
    // local dispatch owns this short guard, never across an upload.
    let (store, account) = context(backend, &user).await?;
    let value = match request.method.as_str() {
        "attachment_upload_status" => serde_json::to_value(
            blocking(move || {
                store.attachment_upload_status(account, &upload.session_id, &upload.operation_id)
            })
            .await?,
        ),
        "cancel_attachment_upload" => {
            blocking(move || {
                store.cancel_attachment_upload(account, &upload.session_id, &upload.operation_id)
            })
            .await?;
            Ok(serde_json::Value::Null)
        }
        _ => return Err("Unknown attachment operation".into()),
    };
    value.map_err(|_| "Could not encode attachment operation".into())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use maple_agent::store::{NewTask, TaskKind};

    pub(crate) fn fixture(root: &std::path::Path) -> LocalBackend {
        let mut backend = LocalBackend::isolated_for_test(root);
        backend.ui_fixture = true;
        let store = backend.store().unwrap();
        let account = store.upsert_account("account", 1).unwrap();
        store
            .create_task(
                account,
                &NewTask {
                    uuid: "01234567-89ab-7def-8123-456789abcdef".into(),
                    project_root: root.to_string_lossy().into_owned(),
                    kind: TaskKind::Desktop,
                    title: "Upload".into(),
                    provisional: true,
                    model: None,
                    created_ms: 1,
                },
            )
            .unwrap();
        backend
    }

    #[tokio::test]
    async fn local_import_publishes_a_stable_copy_and_keeps_only_ids_for_send() {
        let root = tempfile::tempdir().unwrap();
        let backend = fixture(root.path());
        let source = root.path().join("notes.txt");
        std::fs::write(&source, b"stable upload").unwrap();
        let import = ImportAttachmentRequest {
            session_id: "01234567-89ab-7def-8123-456789abcdef".into(),
            operation_id: "11111111-1111-4111-8111-111111111111".into(),
            path: source.to_string_lossy().into_owned(),
            name: "notes.txt".into(),
            mime_type: "text/plain".into(),
        };
        let request = || Request {
            id: 1,
            method: "import_attachment".into(),
            send_id: None,
            params: serde_json::json!(["account", import]),
        };
        let first: StoredAttachment =
            serde_json::from_value(super::super::dispatch(&backend, request()).await.unwrap())
                .unwrap();
        let second: StoredAttachment =
            serde_json::from_value(super::super::dispatch(&backend, request()).await.unwrap())
                .unwrap();
        assert_eq!(first.id, second.id);
        std::fs::remove_file(&source).unwrap();
        assert_eq!(
            std::fs::read(&first.execution_path).unwrap(),
            b"stable upload"
        );
        let status = dispatch(
            &backend,
            Request {
                id: 2,
                method: "attachment_upload_status".into(),
                send_id: None,
                params: serde_json::json!([
                    "account",
                    AttachmentUploadRequest {
                        session_id: import.session_id,
                        operation_id: import.operation_id
                    }
                ]),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_value::<AttachmentUploadStatus>(status).unwrap(),
            AttachmentUploadStatus::Complete(first)
        );
    }

    #[tokio::test]
    async fn local_import_refuses_directories_oversized_files_and_wrong_account_before_reading() {
        let root = tempfile::tempdir().unwrap();
        let mut backend = fixture(root.path());
        let path = root.path().join("large");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(maple_agent::store::MAX_ATTACHMENT_BYTES as u64 + 1)
            .unwrap();
        let mut import = ImportAttachmentRequest {
            session_id: "01234567-89ab-7def-8123-456789abcdef".into(),
            operation_id: "11111111-1111-4111-8111-111111111111".into(),
            path: path.to_string_lossy().into_owned(),
            name: "large".into(),
            mime_type: "application/octet-stream".into(),
        };
        assert!(prepare(&import).await.unwrap_err().contains("30 MiB"));
        import.path = root.path().to_string_lossy().into_owned();
        assert!(prepare(&import).await.is_err());
        backend.ui_fixture = false;
        import.path = "/missing-do-not-open".into();
        let error = dispatch(
            &backend,
            Request {
                id: 1,
                method: "import_attachment".into(),
                send_id: None,
                params: serde_json::json!(["other-account", import]),
            },
        )
        .await
        .unwrap_err();
        assert!(error.contains("account changed"));
    }
}
