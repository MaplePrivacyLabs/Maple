//! Optional remote-management messages within the admitted control-wire major.
use super::manager::Node;
use iroh::EndpointId;
use semver::Version;
use serde::{Deserialize, Deserializer, Serialize};
use std::sync::{Arc, Weak};
use uuid::Uuid;

pub const PROTOCOL_VERSION: &str = "7.0.0";
pub const PEER_INFO_CAPABILITY: &str = "peer_info_v1";
pub const INSTALL_CAPABILITY: &str = "remote_update_install_v1";
pub const PROJECT_FOLDERS_CAPABILITY: &str = "project_folders_v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolInfo {
    pub version: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl ProtocolInfo {
    pub fn current() -> Self {
        Self {
            version: PROTOCOL_VERSION.into(),
            capabilities: vec![
                PEER_INFO_CAPABILITY.into(),
                INSTALL_CAPABILITY.into(),
                PROJECT_FOLDERS_CAPABILITY.into(),
            ],
        }
    }

    fn supports(&self, capability: &str) -> bool {
        Version::parse(&self.version)
            .is_ok_and(|version| version.major == u64::from(super::wire::VERSION))
            && self.capabilities.iter().any(|value| value == capability)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningApplication {
    pub version: String,
    /// Packaging assigns the numeric build after compiling the executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    pub source_sha: String,
    pub platform: String,
}

impl RunningApplication {
    pub fn current() -> Self {
        let info = crate::profile::build_info();
        let value = |name: &str| info[name].as_str().unwrap_or("unknown").to_owned();
        Self {
            version: value("version"),
            build: None,
            source_sha: value("source_sha"),
            platform: match (std::env::consts::OS, std::env::consts::ARCH) {
                ("macos", "aarch64") => "darwin-aarch64".into(),
                (os, arch) => format!("{os}-{arch}"),
            },
        }
    }

    /// Recovery readiness only; the execution host verifies the full candidate.
    #[cfg(any(feature = "desktop", test))]
    pub fn matches_release(&self, release: &ReleaseIdentity) -> bool {
        self.version == release.version
            && self.source_sha == release.source_sha
            && self
                .build
                .as_ref()
                .is_none_or(|build| *build == release.build)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseIdentity {
    pub version: String,
    pub build: String,
    pub source_sha: String,
    /// Exact signed platform envelope; macOS and Linux documents differ.
    pub manifest_sha256: String,
}

impl ReleaseIdentity {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.version.len() > 64 {
            return Err(invalid_install());
        }
        let version = Version::parse(&self.version).map_err(|_| invalid_install())?;
        if !version.pre.is_empty()
            || !version.build.is_empty()
            || version.to_string() != self.version
            || self
                .build
                .parse::<u64>()
                .ok()
                .filter(|build| *build > 0)
                .is_none_or(|build| build.to_string() != self.build)
            || !lower_hex(&self.source_sha, 40)
            || !lower_hex(&self.manifest_sha256, 64)
        {
            return Err(invalid_install());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    Disabled,
    Idle,
    Checking,
    Current,
    Available,
    Downloading,
    Ready,
    Installing,
    RestartRequired,
    Error,
    #[default]
    #[serde(other)]
    Unknown,
}

impl UpdateState {
    #[cfg(feature = "desktop")]
    pub fn label(self) -> &'static str {
        match self {
            Self::Disabled => "Updates unavailable",
            Self::Idle => "Not checked yet",
            Self::Checking => "Checking for updates…",
            Self::Current => "Up to date",
            Self::Available => "Update available",
            Self::Downloading => "Downloading update…",
            Self::Ready => "Ready to update",
            Self::Installing => "Updating remote computer…",
            Self::RestartRequired => "Restart required",
            Self::Error => "Update needs attention",
            Self::Unknown => "Update status unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedReason {
    PasswordProtected,
    UnsupportedInstallation,
    UpdatesDisabled,
    UpdateNotReady,
    Busy,
    DesktopUnavailable,
    CoordinatorUnavailable,
    #[serde(other)]
    Unknown,
}

impl BlockedReason {
    #[cfg(feature = "desktop")]
    pub fn message(self) -> &'static str {
        match self {
            Self::PasswordProtected => {
                "Update this computer locally: its data is password protected."
            }
            Self::UnsupportedInstallation => {
                "This installation must be updated on the remote computer."
            }
            Self::UpdatesDisabled => "Updates are unavailable for this build.",
            Self::UpdateNotReady => "A verified update is not ready on this computer.",
            Self::Busy => "The remote computer is busy. Try again after it finishes.",
            Self::DesktopUnavailable => "Open Maple on the remote computer before updating it.",
            Self::CoordinatorUnavailable => {
                "Remote update controls need attention on this computer. Update it locally."
            }
            Self::Unknown => "The remote computer cannot install this update remotely.",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    PendingAck,
    Queued,
    Installing,
    Succeeded,
    Failed,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteUpdateOperation {
    pub operation_id: String,
    pub target: ReleaseIdentity,
    pub state: OperationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteUpdateStatus {
    #[serde(default)]
    pub state: UpdateState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offered: Option<ReleaseIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<BlockedReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<RemoteUpdateOperation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub protocol: ProtocolInfo,
    pub running: RunningApplication,
    #[serde(default)]
    pub update: RemoteUpdateStatus,
}

impl PeerInfo {
    pub fn supports_remote_updates(&self) -> bool {
        self.protocol.supports(INSTALL_CAPABILITY)
    }

    pub fn supports_project_folders(&self) -> bool {
        self.protocol.supports(PROJECT_FOLDERS_CAPABILITY)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if !text(&self.protocol.version, 64)
            || Version::parse(&self.protocol.version).is_err()
            || self.protocol.capabilities.len() > 32
            || self
                .protocol
                .capabilities
                .iter()
                .any(|value| !text(value, 64))
            || !text(&self.running.version, 64)
            || !text(&self.running.source_sha, 64)
            || !text(&self.running.platform, 64)
            || self
                .running
                .build
                .as_ref()
                .is_some_and(|value| !text(value, 64))
        {
            return Err("Invalid remote update information".into());
        }
        if let Some(offered) = &self.update.offered {
            offered.validate()?;
        }
        if let Some(operation) = &self.update.operation {
            InstallRequest {
                operation_id: operation.operation_id.clone(),
                target: operation.target.clone(),
            }
            .validate()?;
            if operation
                .message
                .as_ref()
                .is_some_and(|value| !text(value, 512))
            {
                return Err("Invalid remote update information".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InstallRequest {
    pub operation_id: String,
    pub target: ReleaseIdentity,
}

// Response documents allow future optional fields. Mutation inputs deliberately
// use separate strict nested shapes instead of inheriting that tolerance.
impl<'de> Deserialize<'de> for InstallRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Target {
            version: String,
            build: String,
            source_sha: String,
            manifest_sha256: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Request {
            operation_id: String,
            target: Target,
        }
        let value = Request::deserialize(deserializer)?;
        let request = Self {
            operation_id: value.operation_id,
            target: ReleaseIdentity {
                version: value.target.version,
                build: value.target.build,
                source_sha: value.target.source_sha,
                manifest_sha256: value.target.manifest_sha256,
            },
        };
        request.validate().map_err(serde::de::Error::custom)?;
        Ok(request)
    }
}

impl InstallRequest {
    pub fn validate(&self) -> Result<(), String> {
        self.operation_bytes()?;
        self.target.validate()
    }

    pub fn operation_bytes(&self) -> Result<[u8; 16], String> {
        let id = Uuid::parse_str(&self.operation_id).map_err(|_| invalid_install())?;
        if self.operation_id.len() != 36
            || id.hyphenated().to_string() != self.operation_id
            || id.get_version_num() != 4
            || id.get_variant() != uuid::Variant::RFC4122
        {
            return Err(invalid_install());
        }
        Ok(*id.as_bytes())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallAccepted {
    pub operation_id: String,
    pub target: ReleaseIdentity,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InstallCommand {
    pub expected_grant: [u8; 16],
    pub request: InstallRequest,
}

#[derive(Clone)]
pub(crate) struct RemoteInstallAuthority {
    node: Weak<Node>,
    account: String,
    peer: String,
    grant: [u8; 16],
}

impl RemoteInstallAuthority {
    pub(super) fn capture(
        node: &Arc<Node>,
        peer: EndpointId,
        expected_grant: [u8; 16],
    ) -> Result<Self, String> {
        let peer = peer.to_string();
        let store = node.store.lock().map_err(|_| "Remote grant unavailable")?;
        if !store.permits(peer.parse().map_err(|_| "Remote grant unavailable")?)
            || !store
                .peers()
                .get(&peer)
                .is_some_and(|value| value.controls_this_computer && value.grant == expected_grant)
        {
            return Err("Remote update authorization changed. Reopen this computer.".into());
        }
        Ok(Self {
            node: Arc::downgrade(node),
            account: node.account.clone(),
            peer,
            grant: expected_grant,
        })
    }

    pub(crate) fn account(&self) -> &str {
        &self.account
    }

    pub(crate) fn peer(&self) -> &str {
        &self.peer
    }

    pub(crate) fn grant_binding(&self) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(self.grant))
    }

    pub(crate) async fn ensure_current(&self) -> Result<(), String> {
        let node = self.node.upgrade().ok_or("Remote access ended")?;
        node.ensure_valid().await?;
        self.with_current_grant(|| Ok(()))
    }

    /// Serialize final consumption with revoke/re-pair writes to this grant.
    /// No async work or re-entry into the grant store belongs in `effect`.
    pub(crate) fn with_current_grant<T>(
        &self,
        effect: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let node = self.node.upgrade().ok_or("Remote access ended")?;
        let peer = self.peer.parse().map_err(|_| "Remote access ended")?;
        let store = node.store.lock().map_err(|_| "Remote grant unavailable")?;
        if node.stopped.is_cancelled()
            || node.backend.saved_user_id().as_deref() != Some(&self.account)
            || !store.permits(peer)
            || !store
                .peers()
                .get(&self.peer)
                .is_some_and(|value| value.grant == self.grant)
        {
            return Err("Remote update authorization changed. Reopen this computer.".into());
        }
        effect()
    }
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn text(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

fn invalid_install() -> String {
    "Invalid remote update request".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn target() -> ReleaseIdentity {
        ReleaseIdentity {
            version: "0.0.7".into(),
            build: "551".into(),
            source_sha: "a".repeat(40),
            manifest_sha256: "b".repeat(64),
        }
    }

    #[test]
    fn install_requests_reject_unknown_nested_fields_and_noncanonical_identities() {
        let value =
            json!({"operation_id":"11111111-1111-4111-8111-111111111111", "target":target()});
        assert!(serde_json::from_value::<InstallRequest>(value.clone()).is_ok());
        let command = json!({"expected_grant":vec![0u8;16],"request":value.clone()});
        assert!(serde_json::from_value::<InstallCommand>(command.clone()).is_ok());
        let mut unknown_command = command;
        unknown_command["force"] = json!(true);
        assert!(serde_json::from_value::<InstallCommand>(unknown_command).is_err());
        let mut changed = value.clone();
        changed["url"] = json!("https://example.invalid/package");
        assert!(serde_json::from_value::<InstallRequest>(changed).is_err());
        let mut changed = value.clone();
        changed["target"]["force"] = json!(true);
        assert!(serde_json::from_value::<InstallRequest>(changed).is_err());
        for id in [
            "",
            "not-a-uuid",
            "11111111-1111-1111-8111-111111111111",
            "11111111-1111-4111-1111-111111111111",
        ] {
            let mut changed = value.clone();
            changed["operation_id"] = json!(id);
            assert!(serde_json::from_value::<InstallRequest>(changed).is_err());
        }
        for (field, invalid) in [
            ("build", "0551"),
            ("source_sha", "ABC"),
            ("manifest_sha256", "bad"),
            ("version", "0.0.7-beta.1"),
        ] {
            let mut changed = value.clone();
            changed["target"][field] = json!(invalid);
            assert!(serde_json::from_value::<InstallRequest>(changed).is_err());
        }
    }

    #[test]
    fn management_protocol_major_matches_the_control_wire() {
        let current = ProtocolInfo::current();
        assert_eq!(
            Version::parse(&current.version).unwrap().major,
            u64::from(super::super::wire::VERSION)
        );
        assert!(current.supports(INSTALL_CAPABILITY));
        assert!(current.supports(PROJECT_FOLDERS_CAPABILITY));
    }

    #[test]
    fn responses_accept_future_fields_and_unknown_status_without_enabling_mutation() {
        let value = json!({
            "protocol":{"version":"7.1.0","capabilities":[INSTALL_CAPABILITY,"future_feature"],"future":true},
            "running":{"version":"0.0.6","source_sha":"a".repeat(40),"platform":"linux-x86_64","future":true},
            "update":{"state":"future_state","future":true},
            "future":true
        });
        let info: PeerInfo = serde_json::from_value(value).unwrap();
        info.validate().unwrap();
        assert_eq!(info.update.state, UpdateState::Unknown);
        assert!(info.supports_remote_updates());
        let mut next_major = info.clone();
        next_major.protocol.version = "8.0.0".into();
        assert!(!next_major.supports_remote_updates());
        let mut previous_major = info.clone();
        previous_major.protocol.version = "6.2.0".into();
        assert!(!previous_major.supports_remote_updates());
        let mut no_capability = info;
        no_capability.protocol.capabilities.clear();
        assert!(!no_capability.supports_remote_updates());
    }

    // Frozen response decoder and validation rules from the 0.0.7 source at
    // 87c05bbdf3d6897a6428fc3022458bab4c5acb94, remote/update.rs (6.1.0).
    // Keep these types independent of the production DTOs and capability
    // constants: changing both ends together would hide compatibility breaks.
    mod released_v6_1 {
        use semver::Version;
        use serde::{Deserialize, Serialize};
        use uuid::Uuid;

        #[derive(Debug, Serialize, Deserialize)]
        pub struct ProtocolInfo {
            pub version: String,
            #[serde(default)]
            pub capabilities: Vec<String>,
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub struct RunningApplication {
            pub version: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub build: Option<String>,
            pub source_sha: String,
            pub platform: String,
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub struct ReleaseIdentity {
            pub version: String,
            pub build: String,
            pub source_sha: String,
            pub manifest_sha256: String,
        }

        impl ReleaseIdentity {
            fn valid(&self) -> bool {
                if self.version.len() > 64 {
                    return false;
                }
                let Ok(version) = Version::parse(&self.version) else {
                    return false;
                };
                version.pre.is_empty()
                    && version.build.is_empty()
                    && version.to_string() == self.version
                    && self
                        .build
                        .parse::<u64>()
                        .ok()
                        .filter(|build| *build > 0)
                        .is_some_and(|build| build.to_string() == self.build)
                    && lower_hex(&self.source_sha, 40)
                    && lower_hex(&self.manifest_sha256, 64)
            }
        }

        #[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum UpdateState {
            Disabled,
            Idle,
            Checking,
            Current,
            Available,
            Downloading,
            Ready,
            Installing,
            RestartRequired,
            Error,
            #[default]
            #[serde(other)]
            Unknown,
        }

        #[derive(Debug, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum BlockedReason {
            PasswordProtected,
            UnsupportedInstallation,
            UpdatesDisabled,
            UpdateNotReady,
            Busy,
            DesktopUnavailable,
            CoordinatorUnavailable,
            #[serde(other)]
            Unknown,
        }

        #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum OperationState {
            PendingAck,
            Queued,
            Installing,
            Succeeded,
            Failed,
            #[serde(other)]
            Unknown,
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub struct RemoteUpdateOperation {
            pub operation_id: String,
            pub target: ReleaseIdentity,
            pub state: OperationState,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub message: Option<String>,
        }

        #[derive(Debug, Default, Serialize, Deserialize)]
        pub struct RemoteUpdateStatus {
            #[serde(default)]
            pub state: UpdateState,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub offered: Option<ReleaseIdentity>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub blocked: Option<BlockedReason>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub operation: Option<RemoteUpdateOperation>,
        }

        #[derive(Debug, Deserialize)]
        pub struct PeerInfo {
            pub protocol: ProtocolInfo,
            pub running: RunningApplication,
            #[serde(default)]
            pub update: RemoteUpdateStatus,
        }

        impl PeerInfo {
            pub fn supports_remote_updates(&self) -> bool {
                Version::parse(&self.protocol.version).is_ok_and(|version| version.major == 6)
                    && self
                        .protocol
                        .capabilities
                        .iter()
                        .any(|value| value == "remote_update_install_v1")
            }

            pub fn valid(&self) -> bool {
                text(&self.protocol.version, 64)
                    && Version::parse(&self.protocol.version).is_ok()
                    && self.protocol.capabilities.len() <= 32
                    && self
                        .protocol
                        .capabilities
                        .iter()
                        .all(|value| text(value, 64))
                    && text(&self.running.version, 64)
                    && text(&self.running.source_sha, 64)
                    && text(&self.running.platform, 64)
                    && self
                        .running
                        .build
                        .as_ref()
                        .is_none_or(|value| text(value, 64))
                    && self
                        .update
                        .offered
                        .as_ref()
                        .is_none_or(ReleaseIdentity::valid)
                    && self.update.operation.as_ref().is_none_or(|operation| {
                        Uuid::parse_str(&operation.operation_id).is_ok_and(|id| {
                            operation.operation_id.len() == 36
                                && id.hyphenated().to_string() == operation.operation_id
                                && id.get_version_num() == 4
                                && id.get_variant() == uuid::Variant::RFC4122
                        }) && operation.target.valid()
                            && operation
                                .message
                                .as_ref()
                                .is_none_or(|value| text(value, 512))
                    })
            }
        }

        fn lower_hex(value: &str, length: usize) -> bool {
            value.len() == length
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }

        fn text(value: &str, maximum: usize) -> bool {
            !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
        }
    }

    fn current_peer_for_compatibility() -> PeerInfo {
        let installed = target();
        let offered = ReleaseIdentity {
            version: "0.0.8".into(),
            build: "552".into(),
            source_sha: "c".repeat(40),
            manifest_sha256: "d".repeat(64),
        };
        PeerInfo {
            protocol: ProtocolInfo::current(),
            running: RunningApplication {
                version: installed.version.clone(),
                build: Some(installed.build.clone()),
                source_sha: installed.source_sha.clone(),
                platform: "linux-x86_64".into(),
            },
            update: RemoteUpdateStatus {
                state: UpdateState::Ready,
                offered: Some(offered),
                blocked: None,
                operation: Some(RemoteUpdateOperation {
                    operation_id: "11111111-1111-4111-8111-111111111111".into(),
                    target: installed,
                    state: OperationState::Succeeded,
                    message: Some("Requested release was running after this update".into()),
                }),
            },
        }
    }

    #[test]
    fn released_v6_decoder_recognizes_a_new_major_without_install_authority() {
        let mut current = current_peer_for_compatibility();
        for state in [UpdateState::Ready, UpdateState::RestartRequired] {
            current.update.state = state;
            current.validate().unwrap();
            let bytes = serde_json::to_vec(&current).unwrap();
            assert!(
                bytes.len() < 16 * 1024,
                "the released management reply limit"
            );
            let legacy: released_v6_1::PeerInfo = serde_json::from_slice(&bytes).unwrap();
            assert!(legacy.valid());
            assert_eq!(legacy.protocol.version, "7.0.0");
            assert!(
                !legacy.supports_remote_updates(),
                "no cross-major management bridge"
            );
            assert!(
                legacy
                    .protocol
                    .capabilities
                    .iter()
                    .any(|value| value == "project_folders_v1")
            );
            assert_eq!(
                serde_json::to_value(&legacy.running).unwrap(),
                serde_json::to_value(&current.running).unwrap()
            );
            assert_eq!(
                serde_json::to_value(&legacy.update).unwrap(),
                serde_json::to_value(&current.update).unwrap(),
                "status and the exact durable operation, including its signed manifest identity, survive the old decoder"
            );
            let receipt = legacy.update.operation.as_ref().unwrap();
            assert_eq!(receipt.operation_id, "11111111-1111-4111-8111-111111111111");
            assert_eq!(receipt.state, released_v6_1::OperationState::Succeeded);
            let mut wrong_operation = legacy;
            wrong_operation
                .update
                .operation
                .as_mut()
                .unwrap()
                .operation_id = "invalid".into();
            assert!(
                !wrong_operation.valid(),
                "the old receipt validator still runs"
            );
        }

        // An unknown folder capability cannot substitute for install authority.
        current.protocol.capabilities = vec![PROJECT_FOLDERS_CAPABILITY.into()];
        let legacy: released_v6_1::PeerInfo =
            serde_json::from_value(serde_json::to_value(&current).unwrap()).unwrap();
        assert!(legacy.valid());
        assert!(!legacy.supports_remote_updates());
    }

    #[test]
    fn project_folder_support_requires_its_capability_and_current_major() {
        let mut info = current_peer_for_compatibility();
        assert!(info.supports_project_folders());
        for version in ["7.0.0", "7.1.0", "7.2.0"] {
            info.protocol.version = version.into();
            assert!(
                info.supports_project_folders(),
                "explicit capability in {version}"
            );
        }
        for version in ["6.2.0", "8.0.0", "invalid"] {
            info.protocol.version = version.into();
            assert!(!info.supports_project_folders());
        }
        info.protocol.version = "7.0.0".into();
        info.protocol.capabilities = vec!["peer_info_v1".into(), "remote_update_install_v1".into()];
        assert!(
            info.supports_remote_updates(),
            "explicit current-major update support remains"
        );
        assert!(
            !info.supports_project_folders(),
            "folder support still requires its capability"
        );
        info.protocol.version = "7.1.0".into();
        assert!(
            !info.supports_project_folders(),
            "a minor version alone is not a capability"
        );
    }

    #[test]
    fn running_release_matching_honors_source_and_known_build() {
        let release = target();
        let mut running = RunningApplication {
            version: release.version.clone(),
            source_sha: release.source_sha.clone(),
            build: None,
            platform: "darwin-aarch64".into(),
        };
        assert!(running.matches_release(&release));
        running.build = Some("552".into());
        assert!(!running.matches_release(&release));
    }

    #[test]
    fn running_metadata_does_not_invent_a_packaging_build() {
        let running = RunningApplication::current();
        assert_eq!(running.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(running.source_sha, env!("MAPLE_GIT_SOURCE_SHA"));
        assert!(running.build.is_none());
    }
}
