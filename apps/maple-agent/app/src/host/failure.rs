//! Stable presentation codes alongside untouched diagnostic fallback text.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostFailure {
    pub code: String,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
    pub message: String,
}
impl HostFailure {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            args: BTreeMap::new(),
            message: message.into(),
        }
    }
}
impl From<String> for HostFailure {
    fn from(message: String) -> Self {
        Self::new("unknown", message)
    }
}
impl From<&str> for HostFailure {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
impl std::fmt::Display for HostFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for HostFailure {}

impl From<maple_agent::store::keys::KeyError> for HostFailure {
    fn from(error: maple_agent::store::keys::KeyError) -> Self {
        use maple_agent::store::keys::KeyError as E;
        let message = error.to_string();
        let code = match error {
            E::Damaged => "key-damaged",
            E::MismatchedDatabase => "key-mismatch",
            E::WrongPassword => "key-wrong-password",
            E::InvalidPassword => "key-invalid-password",
            E::WeakPassword => "key-weak-password",
            E::MemoryLock => "key-memory-lock",
            E::Random => "key-random",
            E::UnsafePath => "key-unsafe-path",
            E::Io { .. } => "key-io",
            E::SynchronizationPending { .. } => "key-sync-pending",
            E::NotEnoughMemory { required_mib } => {
                let mut failure = Self::new("key-memory", message);
                failure
                    .args
                    .insert("required_mib".into(), required_mib.to_string());
                return failure;
            }
        };
        Self::new(code, message)
    }
}

impl From<maple_agent::store::lifecycle::LifecycleError> for HostFailure {
    fn from(error: maple_agent::store::lifecycle::LifecycleError) -> Self {
        use maple_agent::store::lifecycle::LifecycleError as E;
        let message = error.to_string();
        let code = match error {
            E::Cancelled => "lifecycle-cancelled",
            E::Confirmation => "lifecycle-confirmation",
            E::NotEnoughSpace => "lifecycle-space",
            E::VerificationIncomplete => "lifecycle-verification",
            E::Generation => "lifecycle-generation",
            E::RecoveryRequired { .. } => "lifecycle-recovery",
            E::Store(error) => return error.into(),
            E::Key(error) => return error.into(),
        };
        Self::new(code, message)
    }
}

impl From<maple_agent::store::StoreError> for HostFailure {
    fn from(error: maple_agent::store::StoreError) -> Self {
        use maple_agent::store::StoreError as E;
        let message = error.to_string();
        let code = match error {
            E::Locked => "storage-locked",
            E::NeedsSetup => "storage-setup",
            E::Busy => "storage-busy",
            E::DiskFull => "storage-disk-full",
            E::Damaged { .. } => "storage-damaged",
            E::NotFound => "storage-not-found",
            E::Conflict => "storage-conflict",
            E::EpochChanged => "storage-epoch",
            E::InvalidInput => "storage-input",
            E::InvalidData => "storage-data",
            E::UnsafePath => "storage-unsafe-path",
            E::UnsupportedFilesystem => "storage-filesystem",
            E::SchemaMismatch => "storage-schema",
            E::NewerVersion { .. } => "storage-newer",
            E::SchemaEpoch { .. } => "storage-epoch-schema",
            E::Database { .. } => "storage-database",
            E::Io { .. } => "storage-io",
            E::Refused { reason } => return reason.into(),
            E::UnsupportedOperationFormat => {
                return maple_agent::store::RefusalReason::UnsupportedOperationFormat.into();
            }
        };
        Self::new(code, message)
    }
}

impl From<maple_agent::store::RefusalReason> for HostFailure {
    fn from(error: maple_agent::store::RefusalReason) -> Self {
        use maple_agent::store::RefusalReason as E;
        let message = error.message();
        let code = match error {
            E::NewerVersion => "refusal-newer",
            E::IncompatibleSchema => "refusal-schema",
            E::UnsupportedFilesystem => "refusal-filesystem",
            E::UnsafePath => "refusal-path",
            E::UnsupportedOperationFormat => "refusal-operation",
            E::Unavailable => "refusal-unavailable",
        };
        Self::new(code, message)
    }
}

impl From<maple_agent::store::integrity::SalvageError> for HostFailure {
    fn from(error: maple_agent::store::integrity::SalvageError) -> Self {
        use maple_agent::store::integrity::SalvageError as E;
        let message = error.to_string();
        let code = match error {
            E::Lifecycle(error) => return error.into(),
            E::UnenumerableSource => "salvage-boundary",
            E::ReviewTooLarge => "salvage-review",
            E::StalePreview => "salvage-stale",
            E::Cancelled => "salvage-cancelled",
        };
        Self::new(code, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salvage_failures_retain_distinct_codes_and_lifecycle_safety_details() {
        use maple_agent::store::{
            integrity::SalvageError, keys::KeyError, lifecycle::LifecycleError,
        };
        for (error, expected) in [
            (SalvageError::UnenumerableSource, "salvage-boundary"),
            (SalvageError::ReviewTooLarge, "salvage-review"),
            (SalvageError::StalePreview, "salvage-stale"),
            (SalvageError::Cancelled, "salvage-cancelled"),
            (
                SalvageError::Lifecycle(LifecycleError::Key(KeyError::WrongPassword)),
                "key-wrong-password",
            ),
        ] {
            let original = error.to_string();
            let failure = HostFailure::from(error);
            assert_eq!(failure.code, expected);
            assert_eq!(failure.message, original);
            let decoded: HostFailure =
                serde_json::from_value(serde_json::to_value(&failure).unwrap()).unwrap();
            assert_eq!(decoded, failure);
        }
    }
}
