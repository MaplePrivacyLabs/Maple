//! Host-only password decoding and serialized blocking key work.
use super::HostFailure;
use maple_agent::store::{
    Store,
    keys::{self, GeneratedPassphrase, KdfParams, KeyError, Password},
    lifecycle::{LifecycleError, LifecycleResult, LockStatus},
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

enum SelectedPassword {
    Typed(Password),
    Suggested {
        id: String,
        issued: Instant,
        phrase: GeneratedPassphrase,
    },
}
impl SelectedPassword {
    fn choice(&self) -> maple_agent::store::lifecycle::PasswordChoice<'_> {
        match self {
            Self::Typed(password) => maple_agent::store::lifecycle::PasswordChoice::Typed(password),
            Self::Suggested { phrase, .. } => {
                maple_agent::store::lifecycle::PasswordChoice::Suggested(phrase)
            }
        }
    }
}

#[derive(Default)]
pub(super) struct PasswordHost {
    calibrated: tokio::sync::Mutex<Option<KdfParams>>,
    generated: Mutex<Option<(String, Instant, GeneratedPassphrase)>>,
}

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        "unlock"
            | "setup_store"
            | "start_fresh"
            | "set_store_password"
            | "change_store_password"
            | "remove_store_password"
            | "erase_local_data"
            | "generate_store_passphrase"
            | "assess_store_password"
    )
}

pub(super) fn changes_store(method: &str) -> bool {
    handles(method)
        && !matches!(
            method,
            "generate_store_passphrase" | "assess_store_password"
        )
}

/// Key-only rewraps keep the database, runtime and every attached transport.
/// Decode the full request so malformed/default flags cannot choose a lane
/// that disagrees with the eventual password operation.
pub(super) fn requires_drain(method: &str, input: &Value) -> Result<bool, HostFailure> {
    if method == "change_store_password" {
        let input: ChangePassword<'_> = decode(input)?;
        return Ok(input.reencrypt);
    }
    Ok(changes_store(method))
}

/// Refusals here must precede runtime, ACP, remote or spill teardown. Core
/// lifecycle methods repeat their own checks before changing durable state.
pub(super) async fn validate_before_drain(
    store: Arc<Store>,
    method: &str,
    input: &Value,
) -> Result<(), HostFailure> {
    match method {
        "unlock" => {
            let input: NewPassword<'_> = decode(input)?;
            let _password = normalize(input.password)?;
            if store.state() == maple_agent::store::StoreState::Open {
                return Err(HostFailure::new(
                    "passwords-already-unlocked",
                    "Maple local data is already unlocked.",
                ));
            }
            tokio::task::spawn_blocking(move || store.validate_unlock_slot())
                .await
                .map_err(|_| {
                    HostFailure::new(
                        "passwords-unlock-validation",
                        "Unlock validation did not finish",
                    )
                })?
                .map_err(HostFailure::from)
        }
        "erase_local_data" | "start_fresh" => {
            let input: Confirmation<'_> = decode(input)?;
            let expected = if method == "erase_local_data" {
                maple_agent::store::lifecycle::ERASE_CONFIRMATION
            } else {
                maple_agent::store::lifecycle::START_FRESH_CONFIRMATION
            };
            if input.confirmation != expected {
                return Err(HostFailure::from(LifecycleError::Confirmation));
            }
            if method == "start_fresh"
                && !matches!(
                    store.state(),
                    maple_agent::store::StoreState::Damaged { .. }
                )
            {
                return Err(HostFailure::from(maple_agent::store::StoreError::Conflict));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Retain the typed reset boundary until the owner has disposed of the old
/// drained runtime. RecoveryRequired can mean the confirmed reset intent is
/// already durable; its old source must never be retried into a new generation.
pub(super) async fn reset_store(
    store: Arc<Store>,
    method: &str,
    input: &Value,
) -> Result<LifecycleResult<LockStatus>, HostFailure> {
    let input: Confirmation<'_> = decode(input)?;
    let confirmation = input.confirmation.to_owned();
    let erase = match method {
        "erase_local_data" => true,
        "start_fresh" => false,
        _ => {
            return Err(HostFailure::new(
                "passwords-reset-unknown",
                "Unknown local store reset",
            ));
        }
    };
    tokio::task::spawn_blocking(move || {
        if erase {
            store.erase_local_data(&confirmation)
        } else {
            store.start_fresh(&confirmation)
        }
    })
    .await
    .map_err(|_| HostFailure::new("passwords-reset-worker", "Local store reset did not finish"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewPassword<'a> {
    password: &'a str,
    #[serde(default)]
    generated_id: Option<&'a str>,
    #[serde(default, rename = "ui_operation_id")]
    _ui_operation_id: Option<&'a str>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangePassword<'a> {
    current_password: &'a str,
    password: &'a str,
    #[serde(default)]
    reencrypt: bool,
    #[serde(default)]
    generated_id: Option<&'a str>,
    #[serde(default, rename = "ui_operation_id")]
    _ui_operation_id: Option<&'a str>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CurrentPassword<'a> {
    current_password: &'a str,
    #[serde(default, rename = "ui_operation_id")]
    _ui_operation_id: Option<&'a str>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Setup<'a> {
    password: Option<&'a str>,
    #[serde(default)]
    generated_id: Option<&'a str>,
    #[serde(default, rename = "ui_operation_id")]
    _ui_operation_id: Option<&'a str>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirmation<'a> {
    confirmation: &'a str,
}

fn decode<'a, T: Deserialize<'a>>(value: &'a Value) -> Result<T, HostFailure> {
    T::deserialize(value)
        .map_err(|_| HostFailure::new("passwords-request", "Invalid local store request"))
}
fn encode<T: serde::Serialize>(value: T) -> Result<Value, HostFailure> {
    serde_json::to_value(value).map_err(|_| {
        HostFailure::new(
            "passwords-response",
            "Could not encode local store response",
        )
    })
}
fn normalize(value: &str) -> Result<Password, HostFailure> {
    Password::normalize(value).map_err(HostFailure::from)
}

pub(super) fn is_setup_acknowledgement(store: &Store, input: &Value) -> Result<bool, HostFailure> {
    let input: Setup<'_> = decode(input)?;
    if store.state() != maple_agent::store::StoreState::Open {
        return Ok(false);
    }
    let mode = store.lock_status().map_err(HostFailure::from)?.mode;
    Ok(matches!(
        (mode, input.password.is_some()),
        (maple_agent::store::lifecycle::StoreMode::Password, true)
            | (maple_agent::store::lifecycle::StoreMode::Plain, false)
    ))
}

pub(super) async fn acknowledge_setup(
    store: Arc<Store>,
    input: &Value,
) -> Result<Value, HostFailure> {
    let input: Setup<'_> = decode(input)?;
    let password = input.password.map(normalize).transpose()?;
    // The core rechecks Open and the installed generation under its lifecycle
    // gate. This entrypoint cannot fall through to creating or changing data.
    tokio::task::spawn_blocking(move || store.acknowledge_completed_setup(password.as_ref()))
        .await
        .map_err(|_| {
            HostFailure::new(
                "passwords-setup-ack",
                "Local store setup acknowledgement did not finish",
            )
        })?
        .map_err(HostFailure::from)
        .and_then(encode)
}

impl PasswordHost {
    #[cfg(test)]
    pub(super) async fn use_floor_parameters_for_test(&self) {
        *self.calibrated.lock().await = Some(KdfParams::floor());
    }

    async fn parameters(&self) -> Result<KdfParams, HostFailure> {
        let mut cached = self.calibrated.lock().await;
        if let Some(params) = *cached {
            return Ok(params);
        }
        let params = tokio::task::spawn_blocking(|| KdfParams::calibrate(total_memory()))
            .await
            .map_err(|_| {
                HostFailure::new(
                    "passwords-calibration",
                    "Password calibration did not finish",
                )
            })?
            .map_err(HostFailure::from)?;
        *cached = Some(params);
        Ok(params)
    }

    fn check_generated(&self, id: Option<&str>, password: &Password) -> Result<(), HostFailure> {
        let Some(id) = id else {
            return Ok(());
        };
        let generated = self.generated.lock().map_err(|_| {
            HostFailure::new("passwords-generator", "Passphrase generator is unavailable")
        })?;
        if !generated.as_ref().is_some_and(|(receipt, issued, phrase)| {
            receipt == id
                && issued.elapsed() < Duration::from_secs(600)
                && phrase.password().as_str() == password.as_str()
        }) {
            return Err(HostFailure::new(
                "passwords-expired",
                "Generate a new passphrase or choose your own password.",
            ));
        }
        Ok(())
    }

    fn select(
        &self,
        id: Option<&str>,
        password: Password,
    ) -> Result<SelectedPassword, HostFailure> {
        let Some(id) = id else {
            return Ok(SelectedPassword::Typed(password));
        };
        // Validate and take the same receipt under one lock. A concurrent new
        // suggestion must never substitute a different phrase after validation.
        let mut generated = self.generated.lock().map_err(|_| {
            HostFailure::new("passwords-generator", "Passphrase generator is unavailable")
        })?;
        if !generated.as_ref().is_some_and(|(receipt, issued, phrase)| {
            receipt == id
                && issued.elapsed() < Duration::from_secs(600)
                && phrase.password().as_str() == password.as_str()
        }) {
            return Err(HostFailure::new(
                "passwords-expired",
                "Generate a new passphrase or choose your own password.",
            ));
        }
        let (id, issued, phrase) = generated.take().ok_or_else(|| {
            HostFailure::new(
                "passwords-expired",
                "Generate a new passphrase or choose your own password.",
            )
        })?;
        Ok(SelectedPassword::Suggested { id, issued, phrase })
    }

    fn finish_selection<T>(&self, selected: SelectedPassword, result: &LifecycleResult<T>) {
        // These exact errors precede a password/DB commit. Generic I/O,
        // generation, reopen and task failures may follow a commit and must
        // never restore authority to blindly replay the operation.
        let definitely_uncommitted = matches!(
            result,
            Err(LifecycleError::Cancelled
                | LifecycleError::NotEnoughSpace
                | LifecycleError::Key(KeyError::WrongPassword | KeyError::WeakPassword))
        );
        let SelectedPassword::Suggested { id, issued, phrase } = selected else {
            return;
        };
        if definitely_uncommitted
            && issued.elapsed() < Duration::from_secs(600)
            && let Ok(mut generated) = self.generated.lock()
            && generated.is_none()
        {
            // Preserve the original expiry and never overwrite a newer receipt.
            *generated = Some((id, issued, phrase));
        }
    }

    pub(super) async fn dispatch(
        &self,
        store: Arc<Store>,
        method: &str,
        input: &Value,
        context: Vec<String>,
    ) -> Result<Value, HostFailure> {
        match method {
            "generate_store_passphrase" => {
                let phrase = keys::generate_passphrase().map_err(HostFailure::from)?;
                let id = uuid::Uuid::new_v4().to_string();
                let result = serde_json::json!({"id": id, "passphrase": phrase.password().as_str(), "assessment": phrase.assessment()});
                *self.generated.lock().map_err(|_| {
                    HostFailure::new("passwords-generator", "Passphrase generator is unavailable")
                })? = Some((id, Instant::now(), phrase));
                Ok(result)
            }
            "assess_store_password" => {
                let input: NewPassword<'_> = decode(input)?;
                let password = normalize(input.password)?;
                self.check_generated(input.generated_id, &password)?;
                let context = context.iter().map(String::as_str).collect::<Vec<_>>();
                if input.generated_id.is_some() {
                    let generated = self.generated.lock().map_err(|_| {
                        HostFailure::new(
                            "passwords-generator",
                            "Passphrase generator is unavailable",
                        )
                    })?;
                    encode(
                        generated
                            .as_ref()
                            .ok_or_else(|| {
                                HostFailure::new(
                                    "passwords-generate-new",
                                    "Generate a new passphrase.",
                                )
                            })?
                            .2
                            .assessment(),
                    )
                } else {
                    encode(password.assess(&context))
                }
            }
            "unlock" => {
                let input: NewPassword<'_> = decode(input)?;
                let password = normalize(input.password)?;
                let validating = store.clone();
                tokio::task::spawn_blocking(move || validating.validate_unlock_slot())
                    .await
                    .map_err(|_| {
                        HostFailure::new(
                            "passwords-unlock-validation",
                            "Unlock validation did not finish",
                        )
                    })?
                    .map_err(HostFailure::from)?;
                let params = self.parameters().await?;
                tokio::task::spawn_blocking(move || {
                    let result = store.unlock_store(&password, Some(params));
                    if result.is_ok() {
                        super::initial_integrity_check(&store);
                    }
                    result
                })
                .await
                .map_err(|_| HostFailure::new("passwords-unlock-worker", "Unlock did not finish"))?
                .map_err(HostFailure::from)
                .and_then(encode)
            }
            "set_store_password" => {
                let input: NewPassword<'_> = decode(input)?;
                let password = normalize(input.password)?;
                self.check_generated(input.generated_id, &password)?;
                let params = self.parameters().await?;
                let password = self.select(input.generated_id, password)?;
                let (password, result) = tokio::task::spawn_blocking(move || {
                    let context = context.iter().map(String::as_str).collect::<Vec<_>>();
                    let result =
                        store.set_store_password_choice(password.choice(), &context, params);
                    (password, result)
                })
                .await
                .map_err(|_| {
                    HostFailure::new("passwords-change-worker", "Password change did not finish")
                })?;
                self.finish_selection(password, &result);
                result.map_err(HostFailure::from).and_then(encode)
            }
            "change_store_password" => {
                let input: ChangePassword<'_> = decode(input)?;
                let current = normalize(input.current_password)?;
                let password = normalize(input.password)?;
                self.check_generated(input.generated_id, &password)?;
                let reencrypt = input.reencrypt;
                let params = self.parameters().await?;
                let password = self.select(input.generated_id, password)?;
                let (password, result) = tokio::task::spawn_blocking(move || {
                    let context = context.iter().map(String::as_str).collect::<Vec<_>>();
                    let result = store.change_store_password_choice(
                        &current,
                        password.choice(),
                        &context,
                        params,
                        reencrypt,
                    );
                    (password, result)
                })
                .await
                .map_err(|_| {
                    HostFailure::new("passwords-change-worker", "Password change did not finish")
                })?;
                self.finish_selection(password, &result);
                result.map_err(HostFailure::from).and_then(encode)
            }
            "remove_store_password" => {
                let input: CurrentPassword<'_> = decode(input)?;
                let current = normalize(input.current_password)?;
                tokio::task::spawn_blocking(move || store.remove_store_password(&current))
                    .await
                    .map_err(|_| {
                        HostFailure::new(
                            "passwords-remove-worker",
                            "Password removal did not finish",
                        )
                    })?
                    .map_err(HostFailure::from)
                    .and_then(encode)
            }
            "setup_store" => {
                if is_setup_acknowledgement(&store, input)? {
                    // A lost setup response may repeat a consumed receipt.
                    // Current-password proof grants no generated classification.
                    return acknowledge_setup(store, input).await;
                }
                let input: Setup<'_> = decode(input)?;
                let password = input.password.map(normalize).transpose()?;
                if let Some(password) = &password {
                    self.check_generated(input.generated_id, password)?;
                }
                let params = if password.is_some() {
                    self.parameters().await?
                } else {
                    KdfParams {
                        m_kib: 64 * 1024,
                        t: 3,
                        p: 4,
                    }
                };
                let password = password
                    .map(|password| self.select(input.generated_id, password))
                    .transpose()?;
                let (password, result) = tokio::task::spawn_blocking(move || {
                    let context = context.iter().map(String::as_str).collect::<Vec<_>>();
                    let result = store.setup_store_choice(
                        password.as_ref().map(SelectedPassword::choice),
                        &context,
                        params,
                    );
                    (password, result)
                })
                .await
                .map_err(|_| {
                    HostFailure::new("passwords-setup-worker", "Local store setup did not finish")
                })?;
                if let Some(password) = password {
                    self.finish_selection(password, &result);
                }
                result.map_err(HostFailure::from).and_then(encode)
            }
            "erase_local_data" | "start_fresh" => reset_store(store, method, input)
                .await?
                .map_err(HostFailure::from)
                .and_then(encode),
            _ => Err(HostFailure::new(
                "passwords-operation",
                "Unknown local store operation",
            )),
        }
    }
}

fn total_memory() -> u64 {
    #[cfg(target_os = "macos")]
    {
        let mut bytes: u64 = 0;
        let mut length = std::mem::size_of_val(&bytes);
        // SAFETY: fixed sysctl name and correctly sized initialized output.
        if unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&mut bytes as *mut u64).cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        } == 0
        {
            return bytes;
        }
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: these sysconf queries have no pointer arguments.
        let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if let (Ok(pages), Ok(size)) = (u64::try_from(pages), u64::try_from(size)) {
            return pages.saturating_mul(size);
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: initialized struct advertises its exact native size.
        if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 {
            return status.ullTotalPhys;
        }
    }
    // A failed capability query uses the documented conservative floor.
    512 * 1024 * 1024
}

#[cfg(test)]
mod tests {
    use super::*;
    use maple_agent::store::{StorePaths, StoreState, lifecycle::StoreMode};
    use std::io::Read;

    #[tokio::test]
    async fn expired_generated_passphrase_is_a_typed_source_failure() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(StorePaths::new(root.path())).unwrap());
        let host = PasswordHost::default();
        let error = host
            .dispatch(
                store,
                "assess_store_password",
                &serde_json::json!({
                    "password":"an unregistered passphrase", "generated_id":"expired"
                }),
                vec![],
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "passwords-expired");
        assert_eq!(
            error.message,
            "Generate a new passphrase or choose your own password."
        );
    }

    #[tokio::test]
    async fn repeated_generated_setup_authenticates_without_reusing_its_receipt() {
        let root = tempfile::tempdir().unwrap();
        let host = PasswordHost::default();
        let phrase = keys::generate_passphrase().unwrap();
        let key_bytes = [0x58; 32];
        let store = Arc::new(
            Store::open_keyed(
                StorePaths::new(root.path()),
                keys::LockedKey::from_bytes(key_bytes).unwrap(),
            )
            .unwrap(),
        );
        let mut salt = [0_u8; 16];
        std::fs::File::open(store.paths().database())
            .unwrap()
            .read_exact(&mut salt)
            .unwrap();
        let slot = keys::KeyFile::create_generated(
            &phrase,
            &keys::LockedKey::from_bytes(key_bytes).unwrap(),
            salt,
            KdfParams::floor(),
        )
        .unwrap();
        keys::KeyFilePair::write(&store.paths().db(), &slot).unwrap();
        let request = serde_json::json!({
            "password":phrase.password().as_str(), "generated_id":"consumed-setup-receipt"
        });
        let normalized = normalize(phrase.password().as_str()).unwrap();
        *host.generated.lock().unwrap() =
            Some(("consumed-setup-receipt".into(), Instant::now(), phrase));
        drop(
            host.select(Some("consumed-setup-receipt"), normalized)
                .unwrap(),
        );
        let account = store.upsert_account("retained-after-setup", 1).unwrap();
        let status = store.lock_status().unwrap();
        let files = [
            store.paths().keys(),
            store.paths().db().join("keys.backup.json"),
            store.paths().generation(),
        ];
        let before = files
            .iter()
            .map(std::fs::read)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        for _ in 0..2 {
            let acknowledgement = is_setup_acknowledgement(&store, &request).unwrap();
            assert!(acknowledgement);
            assert!(super::super::password_creation_allowed(
                false,
                "setup_store",
                &request,
                acknowledgement
            ));
            assert!(!super::super::password_creation_allowed(
                false,
                "setup_store",
                &request,
                false
            ));
            let result = host
                .dispatch(store.clone(), "setup_store", &request, vec![])
                .await
                .unwrap();
            assert_eq!(result["store_id"], serde_json::json!(status.store_id));
            assert_eq!(result["mode"], "password");
            assert!(host.generated.lock().unwrap().is_none());
            assert!(host.calibrated.lock().await.is_none());
        }
        let wrong = serde_json::json!({"password":"wrong current password", "generated_id":"consumed-setup-receipt"});
        assert!(super::super::password_creation_allowed(
            false,
            "setup_store",
            &wrong,
            is_setup_acknowledgement(&store, &wrong).unwrap()
        ));
        assert!(
            host.dispatch(store.clone(), "setup_store", &wrong, vec![])
                .await
                .is_err()
        );
        assert_eq!(store.state(), StoreState::Open);
        assert_eq!(
            store.account_id("retained-after-setup").unwrap(),
            Some(account)
        );
        let after = files
            .iter()
            .map(std::fs::read)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            before, after,
            "acknowledgement cannot rewrite slots or identity"
        );
        let mut changed: Value = serde_json::from_slice(&before[2]).unwrap();
        changed["store_id"] = serde_json::json!(if status.store_id.as_deref()
            == Some("00000000000000000000000000000000")
        {
            "11111111111111111111111111111111"
        } else {
            "00000000000000000000000000000000"
        });
        let changed = serde_json::to_vec(&changed).unwrap();
        std::fs::write(store.paths().generation(), &changed).unwrap();
        assert!(super::super::password_creation_allowed(
            false,
            "setup_store",
            &request,
            is_setup_acknowledgement(&store, &request).unwrap()
        ));
        assert!(acknowledge_setup(store.clone(), &request).await.is_err());
        assert_eq!(std::fs::read(store.paths().generation()).unwrap(), changed);
        assert_eq!(std::fs::read(store.paths().keys()).unwrap(), before[0]);
        std::fs::write(store.paths().generation(), &before[2]).unwrap();
        store.close_for_shutdown().unwrap();
        assert!(acknowledge_setup(store.clone(), &request).await.is_err());
        assert_eq!(
            store.state(),
            StoreState::Maintenance,
            "a stale no-op route must not reopen or create a store"
        );
    }

    #[tokio::test]
    async fn consumed_setup_receipts_never_authorize_a_mode_or_setup_change() {
        let root = tempfile::tempdir().unwrap();
        let plain = Arc::new(Store::open(StorePaths::new(root.path())).unwrap());
        let host = PasswordHost::default();
        let request =
            serde_json::json!({"password":"some typed password", "generated_id":"consumed"});
        assert!(!is_setup_acknowledgement(&plain, &request).unwrap());
        assert!(!super::super::password_creation_allowed(
            false,
            "setup_store",
            &request,
            false
        ));
        assert!(
            host.dispatch(plain.clone(), "setup_store", &request, vec![])
                .await
                .is_err()
        );
        assert_eq!(plain.lock_status().unwrap().mode, StoreMode::Plain);
        assert!(!plain.paths().keys().exists());
        let pending_root = tempfile::tempdir().unwrap();
        let pending = Arc::new(
            Store::unavailable(StorePaths::new(pending_root.path()), StoreState::NeedsSetup)
                .unwrap(),
        );
        assert!(!is_setup_acknowledgement(&pending, &request).unwrap());
        assert!(!super::super::password_creation_allowed(
            false,
            "setup_store",
            &request,
            false
        ));
        assert!(
            host.dispatch(pending.clone(), "setup_store", &request, vec![])
                .await
                .is_err()
        );
        assert_eq!(pending.state(), StoreState::NeedsSetup);
        assert!(!pending.paths().database().exists());
        assert!(host.calibrated.lock().await.is_none());
        for method in ["set_store_password", "change_store_password"] {
            assert!(!super::super::password_creation_allowed(
                false, method, &request, true
            ));
        }
        plain.close_for_shutdown().unwrap();
    }

    #[test]
    fn suggested_strength_requires_the_unchanged_host_owned_suggestion() {
        let host = PasswordHost::default();
        let phrase = keys::generate_passphrase().unwrap();
        let password = Password::normalize(phrase.password().as_str()).unwrap();
        *host.generated.lock().unwrap() =
            Some(("opaque-host-receipt".into(), Instant::now(), phrase));
        assert!(host.check_generated(Some("forged"), &password).is_err());
        let edited = Password::normalize("a completely different strong typed password").unwrap();
        assert!(
            host.check_generated(Some("opaque-host-receipt"), &edited)
                .is_err()
        );
        let selected = host.select(Some("opaque-host-receipt"), password).unwrap();
        assert!(matches!(selected, SelectedPassword::Suggested { .. }));
        assert!(host.generated.lock().unwrap().is_none());
    }

    #[test]
    fn expired_suggestions_do_not_become_trusted_by_a_client_receipt() {
        let host = PasswordHost::default();
        let phrase = keys::generate_passphrase().unwrap();
        let password = Password::normalize(phrase.password().as_str()).unwrap();
        *host.generated.lock().unwrap() = Some((
            "expired".into(),
            Instant::now() - Duration::from_secs(601),
            phrase,
        ));
        assert!(host.select(Some("expired"), password).is_err());
    }

    #[test]
    fn generated_receipt_lease_restores_only_proven_precommit_rejections() {
        for error in [
            LifecycleError::Cancelled,
            LifecycleError::NotEnoughSpace,
            LifecycleError::Key(KeyError::WrongPassword),
            LifecycleError::Key(KeyError::WeakPassword),
        ] {
            let host = PasswordHost::default();
            let phrase = keys::generate_passphrase().unwrap();
            let password = normalize(phrase.password().as_str()).unwrap();
            let issued = Instant::now();
            *host.generated.lock().unwrap() = Some(("same-receipt".into(), issued, phrase));
            let selected = host.select(Some("same-receipt"), password).unwrap();
            assert!(host.generated.lock().unwrap().is_none());
            let result: LifecycleResult<()> = Err(error);
            host.finish_selection(selected, &result);
            let generated = host.generated.lock().unwrap();
            let (id, restored_issued, _) = generated.as_ref().unwrap();
            assert_eq!(id, "same-receipt");
            assert_eq!(
                *restored_issued, issued,
                "a retry must not extend receipt expiry"
            );
        }
    }

    #[test]
    fn uncertain_or_committed_outcomes_cannot_restore_generated_authority() {
        for result in [
            Ok(()),
            Err(LifecycleError::RecoveryRequired {
                mode: StoreMode::Password,
            }),
            Err(LifecycleError::Generation),
            Err(LifecycleError::Store(maple_agent::store::StoreError::Busy)),
        ] {
            let host = PasswordHost::default();
            let phrase = keys::generate_passphrase().unwrap();
            let password = normalize(phrase.password().as_str()).unwrap();
            *host.generated.lock().unwrap() = Some(("receipt".into(), Instant::now(), phrase));
            let selected = host.select(Some("receipt"), password).unwrap();
            host.finish_selection(selected, &result);
            assert!(host.generated.lock().unwrap().is_none());
        }
    }

    #[test]
    fn returning_a_lease_never_overwrites_a_newer_suggestion() {
        let host = PasswordHost::default();
        let phrase = keys::generate_passphrase().unwrap();
        let password = normalize(phrase.password().as_str()).unwrap();
        *host.generated.lock().unwrap() = Some(("old".into(), Instant::now(), phrase));
        let selected = host.select(Some("old"), password).unwrap();
        *host.generated.lock().unwrap() = Some((
            "new".into(),
            Instant::now(),
            keys::generate_passphrase().unwrap(),
        ));
        let result: LifecycleResult<()> = Err(LifecycleError::Key(KeyError::WrongPassword));
        host.finish_selection(selected, &result);
        assert_eq!(host.generated.lock().unwrap().as_ref().unwrap().0, "new");
    }

    #[tokio::test]
    async fn wrong_current_password_preserves_the_exact_generated_receipt_for_an_explicit_retry() {
        const CURRENT: &str = "Z7jv!9qP#3nMx@8Ks^2dH";
        let root = tempfile::tempdir().unwrap();
        let host = PasswordHost::default();
        *host.calibrated.lock().await = Some(KdfParams::floor());
        let bytes = [0x39; 32];
        let store = Arc::new(
            Store::open_keyed(
                StorePaths::new(root.path()),
                keys::LockedKey::from_bytes(bytes).unwrap(),
            )
            .unwrap(),
        );
        let mut salt = [0; 16];
        std::fs::File::open(store.paths().database())
            .unwrap()
            .read_exact(&mut salt)
            .unwrap();
        let slot = keys::KeyFile::create(
            &normalize(CURRENT).unwrap(),
            &[],
            &keys::LockedKey::from_bytes(bytes).unwrap(),
            salt,
            KdfParams::floor(),
        )
        .unwrap();
        keys::KeyFilePair::write(&store.paths().db(), &slot).unwrap();
        let account = store.upsert_account("retained-lease-fixture", 1).unwrap();
        let phrase = keys::generate_passphrase().unwrap();
        let new_password = normalize(phrase.password().as_str()).unwrap();
        let mut request = serde_json::json!({"current_password":"wrong synthetic current password", "password":phrase.password().as_str(), "generated_id":"retry-exact", "reencrypt":false});
        let issued = Instant::now();
        *host.generated.lock().unwrap() = Some(("retry-exact".into(), issued, phrase));
        assert!(
            host.dispatch(store.clone(), "change_store_password", &request, vec![])
                .await
                .is_err()
        );
        assert!(
            host.check_generated(Some("retry-exact"), &new_password)
                .is_ok()
        );
        assert_eq!(host.generated.lock().unwrap().as_ref().unwrap().1, issued);
        assert!(
            keys::KeyFilePair::load(&store.paths().db(), &salt)
                .unwrap()
                .primary()
                .unlock(&normalize(CURRENT).unwrap())
                .is_ok()
        );
        request["current_password"] = serde_json::json!(CURRENT);
        assert!(
            host.dispatch(store.clone(), "change_store_password", &request, vec![])
                .await
                .is_ok()
        );
        assert!(host.generated.lock().unwrap().is_none());
        assert!(
            keys::KeyFilePair::load(&store.paths().db(), &salt)
                .unwrap()
                .primary()
                .unlock(&new_password)
                .is_ok()
        );
        assert_eq!(
            store.account_id("retained-lease-fixture").unwrap(),
            Some(account)
        );
        store.close_for_shutdown().unwrap();
    }
}
