//! The remote surface is independently versioned and deliberately smaller than
//! local IPC. Adding a local backend operation never makes it remotely callable.
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

// Keep the ALPN family stable so older peers receive the explicit version refusal.
pub const ALPN: &[u8] = b"maple-agent/remote/1";
pub const VERSION: u32 = 7;
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
pub const MAX_PREFACE: usize = 4096;
pub const MAX_MANAGEMENT_REPLY: usize = 16 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub version: u32,
    pub minimum_version: u32,
    pub account: String,
    pub project: String,
    pub deployment: String,
    pub channel: String,
    pub mode: Mode,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Mode {
    Control,
    ConfirmGrant { window: [u8; 16] },
    Pair { window: [u8; 16], admission: String },
}

// Refusals are readable at every handshake stage, including before the peer's
// full Hello can be decoded. Only loss of authority/compatibility ends recovery.
#[derive(Debug, Serialize, Deserialize)]
pub struct Refusal {
    pub terminal: bool,
    pub message: String,
}
impl Refusal {
    pub fn temporary(message: impl Into<String>) -> Self {
        Self {
            terminal: false,
            message: message.into(),
        }
    }
    pub fn terminal(message: impl Into<String>) -> Self {
        Self {
            terminal: true,
            message: message.into(),
        }
    }
    pub fn into_error(self) -> String {
        if self.terminal {
            format!("Remote access ended: {}", self.message)
        } else {
            self.message
        }
    }
}
impl From<String> for Refusal {
    fn from(message: String) -> Self {
        Self::temporary(message)
    }
}
impl From<&str> for Refusal {
    fn from(message: &str) -> Self {
        Self::temporary(message)
    }
}
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub async fn read_hello(reader: &mut (impl AsyncRead + Unpin)) -> Result<Hello, Refusal> {
    let value: serde_json::Value = read(reader, MAX_PREFACE)
        .await
        .map_err(|error| Refusal::terminal(error.to_string()))?;
    #[derive(Deserialize)]
    struct Version {
        version: u32,
        minimum_version: u32,
    }
    let version: Version = serde_json::from_value(value.clone())
        .map_err(|_| Refusal::terminal(Error::InvalidFrame.to_string()))?;
    check_version(version.version, version.minimum_version)
        .map_err(|error| Refusal::terminal(error.to_string()))?;
    serde_json::from_value(value).map_err(|_| Refusal::terminal(Error::InvalidFrame.to_string()))
}

fn check_version(version: u32, minimum: u32) -> Result<(), Error> {
    if minimum == 0 || minimum > version || minimum > VERSION || version < VERSION {
        Err(Error::Incompatible)
    } else {
        Ok(())
    }
}

#[cfg(test)]
impl Hello {
    pub fn check_version(&self) -> Result<(), Error> {
        check_version(self.version, self.minimum_version)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    PeerInfo,
    RemoteUpdateInstall,
    RemoteUpdateRefresh,
    NavigationBootstrap,
    ListTasks,
    ListProjects,
    PinnedTasks,
    GetTasks,
    ListProjectGroups,
    SearchTasks,
    OpenTask,
    HistoryBefore,
    HistoryForward,
    TaskFacts,
    ReadRow,
    ReadAttachment,
    UploadAttachment,
    AttachmentUploadStatus,
    CancelAttachmentUpload,
    QueuedMessage,
    Subscribe,
    ListMcpDefinitions,
    SaveMcpDefinitionEdits,
    TaskBootstrap,
    BrowseProjectFolders,
    CreateProjectFolder,
    SelectProjectRoot,
    RenameProjectRoot,
    CreateSession,
    SetSessionState,
    DeleteSession,
    RemoveProjectRoot,
    ArchiveProjectRoot,
    RenameSession,
    ProjectTrust,
    SetProjectTrust,
    CancelQueuedMessage,
    BeginQueuedMessageEdit,
    EndQueuedMessageEdit,
    SendMessage,
    SendOutcome,
    CancelRun,
    AvailableModelIds,
    ModelCatalogEntry,
    ListSessionMcpServers,
    SetSessionMcpServerEnabled,
    ListMcpServers,
    ListProjectMcpServers,
    SetProjectMcpServerEnabled,
    ListIntegrations,
    SetIntegrationEnabled,
    SaveMcpServers,
    SetSessionWebEnabled,
    CompactSession,
    SessionSubagents,
    CancelExternalAgent,
    ListSlashCommands,
    ResolveSlashCommand,
    SummarizeToolCall,
    SummarizeToolCallStart,
    SummarizeThinking,
    AskSideQuestion,
    ContextUsage,
    AnswerQuestion,
    ThinkingState,
    SetThinkingLevel,
    SaveDefaultModel,
    SavedModel,
    StoreToolSummary,
}

impl Method {
    pub fn priority(self) -> i32 {
        if self.urgent() {
            10
        } else if self == Self::UploadAttachment {
            -5
        } else if matches!(self, Self::OpenTask | Self::HistoryBefore) {
            5
        } else {
            0
        }
    }

    pub fn reply_limit(self) -> usize {
        if matches!(
            self,
            Self::PeerInfo
                | Self::RemoteUpdateInstall
                | Self::RemoteUpdateRefresh
                | Self::UploadAttachment
                | Self::AttachmentUploadStatus
                | Self::CancelAttachmentUpload
        ) {
            MAX_MANAGEMENT_REPLY
        } else if matches!(self, Self::BrowseProjectFolders | Self::CreateProjectFolder) {
            256 * 1024
        } else if matches!(
            self,
            Self::NavigationBootstrap
                | Self::ListTasks
                | Self::ListProjects
                | Self::PinnedTasks
                | Self::GetTasks
                | Self::ListProjectGroups
                | Self::SearchTasks
                | Self::OpenTask
                | Self::HistoryBefore
                | Self::HistoryForward
                | Self::TaskFacts
                | Self::ReadRow
                | Self::ReadAttachment
                | Self::QueuedMessage
        ) {
            4 * 1024 * 1024
        } else {
            MAX_FRAME
        }
    }
    pub fn urgent(self) -> bool {
        matches!(
            self,
            Self::CancelRun
                | Self::AnswerQuestion
                | Self::CancelExternalAgent
                | Self::CancelAttachmentUpload
        )
    }
    pub fn parse(value: &str) -> Result<Self, String> {
        serde_json::from_value(serde_json::Value::String(value.into()))
            .map_err(|_| "This operation is available only on the local computer.".into())
    }
    pub fn name(self) -> String {
        // This enum is serialized only as a unit string.
        serde_json::to_value(self)
            .expect("unit enum serializes")
            .as_str()
            .expect("unit enum is string")
            .into()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub id: u64,
    pub method: Method,
    pub send_id: Option<String>,
    pub params: serde_json::Value,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Disconnected,
    TimedOut,
    InvalidFrame,
    Incompatible,
}

const INCOMPATIBLE_PROTOCOL_MESSAGE: &str =
    "These Maple remote protocol versions are incompatible. Update both computers.";

/// The GUI receives terminal refusals through the host's string error boundary.
/// Only the explicit protocol refusal establishes that an update is required;
/// malformed frames, network failures and revoked authority do not.
pub(crate) fn is_protocol_incompatibility_refusal(message: &str) -> bool {
    message.strip_prefix("Remote access ended: ") == Some(INCOMPATIBLE_PROTOCOL_MESSAGE)
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TimedOut => "Remote transfer stopped making progress",
            Self::Disconnected => "Remote computer disconnected",
            Self::InvalidFrame => "The remote computer sent an invalid message. Update both computers before reconnecting.",
            Self::Incompatible => INCOMPATIBLE_PROTOCOL_MESSAGE,
        })
    }
}
impl std::error::Error for Error {}

pub async fn read<T: DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
    limit: usize,
) -> Result<T, Error> {
    let size = reader.read_u32().await.map_err(|_| Error::Disconnected)? as usize;
    if size == 0 || size > limit.min(MAX_FRAME) {
        return Err(Error::InvalidFrame);
    }
    let mut bytes = Zeroizing::new(vec![0; size]);
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(|_| Error::Disconnected)?;
    serde_json::from_slice(&bytes).map_err(|_| Error::InvalidFrame)
}

pub async fn write<T: Serialize>(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &T,
    limit: usize,
) -> Result<(), Error> {
    let bytes = Zeroizing::new(encode(value, limit)?);
    write_encoded(writer, &bytes).await
}

pub fn encode<T: Serialize>(value: &T, limit: usize) -> Result<Vec<u8>, Error> {
    let mut bytes =
        crate::framing::encode(value, limit.min(MAX_FRAME)).map_err(|_| Error::InvalidFrame)?;
    Ok(std::mem::take(&mut *bytes))
}

pub async fn write_encoded(
    writer: &mut (impl AsyncWrite + Unpin),
    bytes: &[u8],
) -> Result<(), Error> {
    writer
        .write_u32(bytes.len() as u32)
        .await
        .map_err(|_| Error::Disconnected)?;
    writer
        .write_all(bytes)
        .await
        .map_err(|_| Error::Disconnected)?;
    writer.flush().await.map_err(|_| Error::Disconnected)
}

/// Per-progress deadline, not a minimum network throughput requirement. The
/// first reply byte may wait for backend work; once a frame starts, bounded
/// buffers and an idle deadline prevent a stalled transfer retaining capacity.
pub async fn read_progress<T: DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
    limit: usize,
    idle: std::time::Duration,
    wait_for_reply: bool,
) -> Result<T, Error> {
    let mut prefix = [0; 4];
    if wait_for_reply {
        reader
            .read_exact(&mut prefix[..1])
            .await
            .map_err(|_| Error::Disconnected)?;
        read_progress_bytes(reader, &mut prefix[1..], idle).await?;
    } else {
        read_progress_bytes(reader, &mut prefix, idle).await?;
    }
    let size = u32::from_be_bytes(prefix) as usize;
    if size == 0 || size > limit.min(MAX_FRAME) {
        return Err(Error::InvalidFrame);
    }
    let mut bytes = Zeroizing::new(vec![0; size]);
    read_progress_bytes(reader, &mut bytes, idle).await?;
    serde_json::from_slice(&bytes).map_err(|_| Error::InvalidFrame)
}
async fn read_progress_bytes(
    reader: &mut (impl AsyncRead + Unpin),
    mut bytes: &mut [u8],
    idle: std::time::Duration,
) -> Result<(), Error> {
    while !bytes.is_empty() {
        let count = tokio::time::timeout(idle, reader.read(bytes))
            .await
            .map_err(|_| Error::TimedOut)?
            .map_err(|_| Error::Disconnected)?;
        if count == 0 {
            return Err(Error::Disconnected);
        }
        bytes = &mut bytes[count..];
    }
    Ok(())
}
pub async fn write_progress(
    writer: &mut (impl AsyncWrite + Unpin),
    bytes: &[u8],
    idle: std::time::Duration,
) -> Result<(), Error> {
    let prefix = (bytes.len() as u32).to_be_bytes();
    for mut chunk in std::iter::once(prefix.as_slice()).chain(bytes.chunks(64 * 1024)) {
        while !chunk.is_empty() {
            let count = tokio::time::timeout(idle, writer.write(chunk))
                .await
                .map_err(|_| Error::TimedOut)?
                .map_err(|_| Error::Disconnected)?;
            if count == 0 {
                return Err(Error::Disconnected);
            }
            chunk = &chunk[count..];
        }
    }
    tokio::time::timeout(idle, writer.flush())
        .await
        .map_err(|_| Error::TimedOut)?
        .map_err(|_| Error::Disconnected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privileged_local_operations_are_not_remote_commands() {
        for name in [
            "login",
            "logout_and_clear",
            "change_password",
            "create_api_key",
            "delete_api_key",
            "account",
            "plan_usage",
            "billing_status",
            "start_checkout",
            "api_credit_balance",
            "purchase_api_credits",
            "confirm_account_deletion",
            "setup_integration",
            "stop_runtime",
            "start_runtime",
            "detach",
            "stop",
            "remote_enable",
            "remote_set_name",
            "migrate_project_names",
            "save_settings",
            "update_settings",
            "remote_pair",
            "transcribe_audio",
            "synthesize_speech",
            "store_status",
            "host_status",
            "unlock",
            "setup_store",
            "set_store_password",
            "change_store_password",
            "remove_store_password",
            "cancel_store_operation",
            "erase_local_data",
            "get_ui_settings",
            "set_ui_settings",
            "flush_ui_settings",
            "dictionary",
            "add_word",
            "set_task_pinned",
            "set_project_display_name",
            "get_remote_sidebars",
            "set_remote_sidebar",
            "account_deletion_status",
            "store_progress",
            "start_fresh",
            "generate_store_passphrase",
            "assess_store_password",
            "repair_store",
            "damaged_copies",
            "delete_damaged_copy",
            "repair_status",
            "repair_progress",
            "cancel_store_repair",
            "authenticate_repair_password",
            "preview_salvage",
            "salvage_preview_page",
            "salvage_store",
            "check_store_integrity",
            "get_acp_config",
            "set_acp_config",
            "list_sessions",
            "load_session",
            "local_bootstrap",
            "recovery_snapshot",
            "read_image_attachment",
            "load_tool_summaries",
        ] {
            assert!(Method::parse(name).is_err(), "{name}");
        }
        for name in [
            "send_message",
            "send_outcome",
            "cancel_run",
            "answer_question",
            "set_thinking_level",
            "rename_project_root",
            "browse_project_folders",
            "create_project_folder",
        ] {
            assert_eq!(Method::parse(name).unwrap().name(), name);
        }
    }

    #[test]
    fn project_folder_commands_have_bounded_replies() {
        for method in [Method::BrowseProjectFolders, Method::CreateProjectFolder] {
            assert_eq!(Method::parse(&method.name()).unwrap(), method);
            assert_eq!(method.reply_limit(), 256 * 1024);
            assert!(!method.urgent());
        }
    }

    #[tokio::test]
    async fn oversized_and_unknown_commands_fail_terminally() {
        let (mut tx, mut rx) = tokio::io::duplex(256);
        tx.write_u32(MAX_PREFACE as u32 + 1).await.unwrap();
        assert!(matches!(
            read::<Hello>(&mut rx, MAX_PREFACE).await,
            Err(Error::InvalidFrame)
        ));
        let command =
            serde_json::json!({"id": 1, "method": "login", "params": [], "send_id": null});
        write(&mut tx, &command, MAX_PREFACE).await.unwrap();
        assert!(matches!(
            read::<Command>(&mut rx, MAX_PREFACE).await,
            Err(Error::InvalidFrame)
        ));
    }

    #[test]
    fn only_explicit_terminal_protocol_refusal_requires_an_update() {
        let incompatible = Error::Incompatible.to_string();
        assert!(is_protocol_incompatibility_refusal(
            &Refusal::terminal(incompatible.clone()).into_error()
        ));
        for error in [
            Refusal::temporary(incompatible).into_error(),
            Refusal::terminal(Error::InvalidFrame.to_string()).into_error(),
            Refusal::terminal("This computer is no longer paired").into_error(),
            Error::Disconnected.to_string(),
            Error::TimedOut.to_string(),
        ] {
            assert!(!is_protocol_incompatibility_refusal(&error), "{error}");
        }
    }

    #[test]
    fn compatible_versions_do_not_depend_on_build_identity() {
        let mut hello = Hello {
            version: VERSION,
            minimum_version: VERSION,
            account: "a".into(),
            project: "p".into(),
            deployment: "d".into(),
            channel: "unpackaged".into(),
            mode: Mode::Control,
        };
        assert!(hello.check_version().is_ok());
        hello.version = VERSION - 1;
        hello.minimum_version = VERSION - 1;
        assert_eq!(hello.check_version(), Err(Error::Incompatible));
        hello.minimum_version = VERSION;
        hello.version = VERSION + 1;
        assert!(hello.check_version().is_ok());
        hello.minimum_version = VERSION + 1;
        assert_eq!(hello.check_version(), Err(Error::Incompatible));
        hello.version = VERSION;
        assert_eq!(hello.check_version(), Err(Error::Incompatible));
    }

    #[test]
    fn optional_management_keeps_the_shipped_control_ack_and_alpn() {
        assert_eq!(ALPN, b"maple-agent/remote/1");
        assert_eq!(VERSION, 7);
        assert_eq!(
            serde_json::to_string(&Ok::<(), Refusal>(())).unwrap(),
            r#"{"Ok":null}"#
        );
        assert_eq!(Method::PeerInfo.name(), "peer_info");
        assert_eq!(Method::RemoteUpdateInstall.name(), "remote_update_install");
    }
    #[tokio::test]
    async fn paging_wire_upgrade_refuses_version_five_before_decoding() {
        // Version 5 shipped whole catalogs and transcripts. Reject the old
        // handshake before any bounded page or metadata delta is sent.
        let previous: serde_json::Value =
            serde_json::from_str(include_str!("wire_v5.json")).unwrap();
        let previous = &previous["hello"];
        let (mut tx, mut rx) = tokio::io::duplex(4096);
        write(&mut tx, &previous, MAX_PREFACE).await.unwrap();
        let refusal = match read_hello(&mut rx).await {
            Err(refusal) => refusal,
            Ok(_) => panic!("version 5 must update before paged state is sent"),
        };
        assert!(refusal.terminal);
        assert!(refusal.message.contains("Update both computers"));
        let response = serde_json::to_value(Err::<(), _>(refusal)).unwrap();
        let decoded: Result<(), Refusal> = serde_json::from_value(response).unwrap();
        assert!(decoded.unwrap_err().terminal);
    }

    #[tokio::test]
    async fn file_upload_wire_upgrade_refuses_six_before_decoding_requests() {
        // Frozen released fixture: version 6 carries inline image data URLs.
        // A clean baseline refuses it before those requests reach the decoder.
        let previous: serde_json::Value =
            serde_json::from_str(include_str!("wire_v6.json")).unwrap();
        for kind in ["hello", "pair"] {
            let (mut tx, mut rx) = tokio::io::duplex(4096);
            write(&mut tx, &previous[kind], MAX_PREFACE).await.unwrap();
            let refusal = match read_hello(&mut rx).await {
                Err(refusal) => refusal,
                Ok(_) => panic!("wire 6 must update before file attachments are sent"),
            };
            assert!(is_protocol_incompatibility_refusal(&refusal.into_error()));
        }
    }

    #[tokio::test]
    async fn released_wire_six_refuses_seven_before_reading_any_request() {
        // Freeze the released reader's version policy and refusal shape. Do not
        // reuse the current VERSION/check_version when testing the other side.
        #[derive(Deserialize)]
        struct ReleasedVersion {
            version: u32,
            minimum_version: u32,
        }
        #[derive(Serialize)]
        struct ReleasedRefusal {
            terminal: bool,
            message: &'static str,
        }
        let current: serde_json::Value =
            serde_json::from_str(include_str!("wire_v7.json")).unwrap();
        for kind in ["hello", "pair"] {
            let (mut client, mut server) = tokio::io::duplex(4096);
            write(&mut client, &current[kind], MAX_PREFACE)
                .await
                .unwrap();
            let untouched = b"request-body-must-not-be-read";
            client.write_all(untouched).await.unwrap();
            let value: serde_json::Value = read(&mut server, 4096).await.unwrap();
            let version: ReleasedVersion = serde_json::from_value(value).unwrap();
            let refused = version.minimum_version == 0
                || version.minimum_version > version.version
                || version.minimum_version > 6
                || version.version < 6;
            assert!(refused, "released wire 6 must refuse the new baseline");
            write(&mut server, &Err::<(), _>(ReleasedRefusal {
                terminal: true,
                message: "These Maple remote protocol versions are incompatible. Update both computers.",
            }), 4096).await.unwrap();
            let reply: Result<(), Refusal> = read(&mut client, MAX_PREFACE).await.unwrap();
            assert!(is_protocol_incompatibility_refusal(
                &reply.unwrap_err().into_error()
            ));
            let mut remaining = vec![0; untouched.len()];
            server.read_exact(&mut remaining).await.unwrap();
            assert_eq!(
                remaining, untouched,
                "version refusal does not decode or admit queued sends"
            );
        }
    }

    #[tokio::test]
    async fn future_and_malformed_hello_receive_explicit_terminal_refusals() {
        for message in [
            serde_json::json!({"version": VERSION + 1, "minimum_version": VERSION + 1, "future_field": true}),
            serde_json::json!({"version": VERSION, "minimum_version": VERSION, "future_field": true}),
        ] {
            let (mut tx, mut rx) = tokio::io::duplex(4096);
            write(&mut tx, &message, MAX_PREFACE).await.unwrap();
            let refusal = match read_hello(&mut rx).await {
                Err(r) => r,
                Ok(_) => panic!("accepted malformed hello"),
            };
            assert!(refusal.terminal);
            assert!(refusal.message.contains("Update both computers"));
            // All pairing/confirmation stages can decode the same refusal.
            let encoded = serde_json::to_value(Err::<(), _>(refusal)).unwrap();
            let proof: Result<(Vec<u8>, Vec<u8>), Refusal> =
                serde_json::from_value(encoded.clone()).unwrap();
            let grant: Result<[u8; 16], Refusal> = serde_json::from_value(encoded).unwrap();
            assert_eq!(proof.unwrap_err().message, grant.unwrap_err().message);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn transfer_deadline_measures_inactivity_not_total_duration() {
        use std::time::Duration;
        let idle = Duration::from_secs(10);
        let (mut tx, mut rx) = tokio::io::duplex(16);
        let sender = tokio::spawn(async move {
            let bytes = b"\"slow\"";
            tx.write_u32(bytes.len() as u32).await.unwrap();
            for byte in bytes {
                tokio::time::sleep(Duration::from_secs(6)).await;
                tx.write_all(&[*byte]).await.unwrap();
            }
        });
        let start = tokio::time::Instant::now();
        assert_eq!(
            read_progress::<String>(&mut rx, 64, idle, false)
                .await
                .unwrap(),
            "slow"
        );
        assert!(start.elapsed() > idle);
        sender.await.unwrap();
        let (_tx, mut rx) = tokio::io::duplex(16);
        assert_eq!(
            read_progress::<String>(&mut rx, 64, idle, false).await,
            Err(Error::TimedOut)
        );
    }
    #[test]
    fn wire_seven_examples_are_stable() {
        let examples: serde_json::Value =
            serde_json::from_str(include_str!("wire_v7.json")).unwrap();
        fn check<T: serde::de::DeserializeOwned + serde::Serialize>(value: &serde_json::Value) {
            let decoded: T = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), *value);
        }
        check::<Hello>(&examples["hello"]);
        check::<Hello>(&examples["pair"]);
        check::<super::super::streams::Header>(&examples["header"]);
        check::<Command>(&examples["command"]);
        check::<Result<(), Refusal>>(&examples["refusal"]);
        for key in ["image", "failure", "tasks", "receipt", "resync", "project"] {
            check::<crate::host::protocol::Message>(&examples[key]);
        }
        check::<Command>(&examples["send"]);
        check::<super::super::streams::Header>(&examples["upload_header"]);
        check::<super::super::uploads::Upload>(&examples["upload"]);
        for key in [
            "upload_admission_ready",
            "upload_admission_complete",
            "upload_admission_error",
        ] {
            check::<Result<super::super::uploads::Admission, String>>(&examples[key]);
        }
        check::<crate::host::protocol::Message>(&examples["upload_ack"]);
        check::<maple_agent::store::StoredAttachment>(
            &examples["upload_ack"]["Response"]["result"]["Ok"],
        );
        for key in ["upload_status", "cancel_upload"] {
            check::<Command>(&examples[key]);
            check::<(String, crate::backend::AttachmentUploadRequest)>(&examples[key]["params"]);
        }
        for value in examples["upload_status_replies"].as_array().unwrap() {
            check::<crate::host::protocol::Message>(value);
            check::<maple_agent::store::AttachmentUploadStatus>(&value["Response"]["result"]["Ok"]);
        }
        check::<crate::host::protocol::Message>(&examples["cancel_upload_reply"]);
        check::<()>(&examples["cancel_upload_reply"]["Response"]["result"]["Ok"]);
        assert!(
            Method::parse("import_attachment").is_err(),
            "local source paths never reach a remote filesystem importer"
        );
        check::<Command>(&examples["rename_project"]);
        check::<maple_agent::agent::RecentProjectRoot>(
            &examples["project"]["Response"]["result"]["Ok"],
        );
        check::<crate::backend::paging::NavigationBootstrap>(&examples["bootstrap"]);
        check::<crate::backend::SelectedProject>(&examples["selected_project"]);
        check::<maple_agent::store::NavigationProjectPage>(&examples["project_page"]);
        check::<(String, maple_agent::agent::AgentSendMessageRequest)>(&examples["send"]["params"]);
        check::<maple_agent::store::NavigationTaskPage>(
            &examples["tasks"]["Response"]["result"]["Ok"],
        );
        check::<maple_agent::store::AttachmentRangeReply>(
            &examples["image"]["Response"]["result"]["Ok"],
        );
        for value in examples["paged_commands"].as_array().unwrap() {
            check::<Command>(value);
        }
        check::<crate::host::receipts::Outcome>(&examples["receipt"]["Response"]["result"]["Ok"]);
        for value in examples["events"].as_array().unwrap() {
            check::<crate::host::protocol::Message>(value);
            let crate::host::protocol::Message::Event(event) =
                serde_json::from_value(value.clone()).unwrap()
            else {
                panic!("expected event");
            };
            assert!(
                super::super::control::remote_event(&event),
                "fixture must exercise a forwarded event"
            );
        }
        assert_eq!(examples["hello"]["version"], VERSION);
    }

    #[test]
    fn optional_folder_command_examples_match_typed_requests() {
        let examples: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("wire_v6_folders.json")).unwrap();
        for value in examples {
            let command: Command = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(&command).unwrap(), value);
            match command.method {
                Method::BrowseProjectFolders => {
                    let _: (String, crate::backend::ProjectFolderRequest) =
                        serde_json::from_value(command.params).unwrap();
                }
                Method::CreateProjectFolder => {
                    let _: (String, String, String) =
                        serde_json::from_value(command.params).unwrap();
                }
                _ => panic!("expected an optional folder command"),
            }
        }
    }
}
