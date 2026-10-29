//! Synchronous, fail-closed installation primitives. Run these off the UI thread.
//!
//! The caller authenticates discovery again and holds `InstallLock` plus the
//! profile's startup/host fences through `replace`. Serialized preparation is
//! never authority: every file is compared with the signed payload again.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use flate2::read::MultiGzDecoder;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{Artifact, PreparedUpdate, Target};

#[cfg(target_os = "macos")]
use std::ffi::CString;
#[cfg(target_os = "macos")]
use std::os::macos::fs::MetadataExt as _;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::{fs::MetadataExt, fs::OpenOptionsExt, fs::PermissionsExt};

mod linux_bundle;
mod windows_bundle;

#[cfg(windows)]
pub use windows_bundle::refresh_windows_shell_integration;
pub use windows_bundle::{
    activate_windows_installation, cleanup_current_windows_bundle, cleanup_windows_installation,
    discover_windows_installation, preflight_windows_installation, prepare_windows_installation,
    resolve_windows_launcher, validate_windows_bundle_archive,
};
#[cfg(windows)]
pub(crate) use windows_bundle::{
    windows_atomic_replace, windows_open_private, windows_open_readonly, windows_private_directory,
    windows_read_bounded, windows_sync_directory,
};

pub use linux_bundle::{
    cleanup_current_linux_bundle, discover_linux_installation, validate_linux_bundle_archive,
};

// Keep the staging/receipt name stable for preparations made before the rebrand.
const APP_NAME: &str = "Maple Agent.app";
const WORK_APP_NAME: &str = "Maple Work.app";
const STAGING_PREFIX: &str = ".maple-agent-update-";
const STAGING_RANDOM_LENGTH: usize = 16;
const RECEIPT_NAME: &str = "installed.json";
const MAX_EXPANDED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;
const MAX_PATH_BYTES: usize = 1024;
const MAX_EXTENSION_BYTES: u64 = 16 * 1024;
#[cfg(any(target_os = "macos", all(test, unix)))]
const CODESIGN_REQUIREMENT: &str = "=anchor apple generic and identifier \"cloud.opensecret.maple.agent\" and certificate leaf[subject.OU] = \"X773Y823TN\" and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists";

/// Fixed public operation labels for the native Windows installer child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum WindowsActivationStage {
    Input = 1,
    RootStorage = 2,
    Registration = 3,
    BootstrapIdentity = 4,
    InstallLock = 5,
    IntentIdentity = 6,
    GenerationIdentity = 7,
    GenerationVerification = 8,
    GenerationGuards = 9,
    PointerCommit = 10,
}

impl std::fmt::Display for WindowsActivationStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Input => "activation-input",
            Self::RootStorage => "root-storage",
            Self::Registration => "registration",
            Self::BootstrapIdentity => "bootstrap-identity",
            Self::InstallLock => "install-lock",
            Self::IntentIdentity => "intent-identity",
            Self::GenerationIdentity => "generation-identity",
            Self::GenerationVerification => "generation-verification",
            Self::GenerationGuards => "generation-guards",
            Self::PointerCommit => "pointer-commit",
        })
    }
}

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("This installation needs a manual download: {0}")]
    NotifyOnly(&'static str),
    #[error("Another updater is installing this application; try again shortly")]
    Busy,
    #[error("The prepared update is invalid: {0}")]
    Invalid(&'static str),
    #[error("The update does not have the required Maple code signature and identity")]
    CodeSignature,
    #[error("The update could not be installed: {0}")]
    Io(#[from] io::Error),
    #[error("Windows activation failed during {stage}: {source}")]
    WindowsActivation {
        stage: WindowsActivationStage,
        #[source]
        source: Box<InstallError>,
    },
}

impl InstallError {
    /// These failures describe an installation this account cannot replace.
    /// Keep them typed until the controller offers a verified manual download.
    pub fn requires_manual_installation(&self) -> bool {
        match self {
            Self::NotifyOnly(_) => true,
            Self::Io(error) => permission_error(error),
            Self::WindowsActivation { source, .. } => source.requires_manual_installation(),
            _ => false,
        }
    }

    /// Encode only an activation failure, without paths, SIDs or error text.
    /// The original typed source remains available to direct callers.
    pub fn windows_activation_exit_code(&self) -> Option<i32> {
        let Self::WindowsActivation { stage, source } = self else {
            return None;
        };
        let raw = match source.as_ref() {
            Self::Io(error) => error
                .raw_os_error()
                .and_then(|code| u16::try_from(code).ok())
                .unwrap_or(0),
            _ => 0,
        };
        Some(0x4D00_0000 | ((*stage as i32) << 16) | i32::from(raw))
    }
}

type Result<T> = std::result::Result<T, InstallError>;

#[cfg(test)]
mod windows_activation_error_tests {
    use super::*;

    #[test]
    fn activation_status_preserves_fixed_stage_and_original_native_error() {
        for (stage, number, label) in [
            (WindowsActivationStage::Input, 1, "activation-input"),
            (WindowsActivationStage::RootStorage, 2, "root-storage"),
            (WindowsActivationStage::Registration, 3, "registration"),
            (
                WindowsActivationStage::BootstrapIdentity,
                4,
                "bootstrap-identity",
            ),
            (WindowsActivationStage::InstallLock, 5, "install-lock"),
            (WindowsActivationStage::IntentIdentity, 6, "intent-identity"),
            (
                WindowsActivationStage::GenerationIdentity,
                7,
                "generation-identity",
            ),
            (
                WindowsActivationStage::GenerationVerification,
                8,
                "generation-verification",
            ),
            (
                WindowsActivationStage::GenerationGuards,
                9,
                "generation-guards",
            ),
            (WindowsActivationStage::PointerCommit, 10, "pointer-commit"),
        ] {
            let error = InstallError::WindowsActivation {
                stage,
                source: Box::new(InstallError::Io(io::Error::from_raw_os_error(32))),
            };
            assert_eq!(
                error.windows_activation_exit_code(),
                Some(0x4D00_0020 | (number << 16))
            );
            assert!(error.to_string().contains(label));
            let InstallError::WindowsActivation { source, .. } = error else {
                unreachable!()
            };
            let InstallError::Io(source) = *source else {
                unreachable!()
            };
            assert_eq!(source.raw_os_error(), Some(32));
        }
    }

    #[test]
    fn activation_status_keeps_error_policy_and_never_encodes_unrelated_errors() {
        for source in [
            InstallError::Busy,
            InstallError::CodeSignature,
            InstallError::Invalid("public fixture"),
            InstallError::NotifyOnly("public fixture"),
            InstallError::Io(io::Error::from(io::ErrorKind::PermissionDenied)),
            InstallError::Io(io::Error::from_raw_os_error(-1)),
            InstallError::Io(io::Error::from_raw_os_error(65536)),
        ] {
            assert_eq!(source.windows_activation_exit_code(), None);
            let manual = source.requires_manual_installation();
            let error = InstallError::WindowsActivation {
                stage: WindowsActivationStage::Input,
                source: Box::new(source),
            };
            assert_eq!(error.requires_manual_installation(), manual);
            assert_eq!(error.windows_activation_exit_code(), Some(0x4D01_0000));
        }
    }
}

/// The on-disk values are untrusted on restoration. Keep the authenticated
/// `PreparedUpdate` separately and pass it to every revalidation/replacement.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallPreparation {
    pub installed_path: PathBuf,
    pub staging_dir: PathBuf,
    pub payload_path: PathBuf,
    pub staged_path: PathBuf,
    pub version: String,
    pub build: String,
    pub size: u64,
    pub sha256: String,
}

/// The previous bundle stays at `backup_path` until a successful new startup.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledUpdate {
    pub installed_path: PathBuf,
    pub staging_dir: PathBuf,
    pub backup_path: PathBuf,
    pub version: String,
    pub build: String,
    /// The swap committed, but a later directory synchronization failed.
    /// Relaunch the new app and keep the previous app until its checkpoint.
    #[serde(default)]
    pub durability_warning: bool,
}

/// Separate from profile locks: two profiles must not replace the same app.
pub struct InstallLock {
    _file: File,
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        // BSD flock follows the open file description. A transient fork before
        // exec may retain that description even though the descriptor has
        // CLOEXEC. End this guard's authority explicitly rather than waiting
        // for an inherited descriptor's last close.
        let _ = FileExt::unlock(&self._file);
    }
}

/// Find a supported installation from the real executable, never a saved path.
pub fn discover_installation(executable: &Path, _appimage: Option<&Path>) -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let executable = executable.canonicalize()?;
        let app = executable
            .parent()
            .filter(|p| p.file_name().is_some_and(|n| n == "MacOS"))
            .and_then(Path::parent)
            .filter(|p| p.file_name().is_some_and(|n| n == "Contents"))
            .and_then(Path::parent)
            .ok_or(InstallError::NotifyOnly(
                "run the installed application bundle",
            ))?;
        preflight_installation(app)?;
        Ok(app.to_owned())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = executable;
        Err(InstallError::NotifyOnly(
            "automatic installation is not enabled on this platform",
        ))
    }
}

/// Check replacement eligibility before offering Restart and again before
/// stopping the host. This is a preflight, not authority: permissions may still
/// change, and callers must handle a failed atomic replacement safely.
pub fn preflight_installation(installed: &Path) -> Result<()> {
    if cfg!(windows) {
        return windows_bundle::preflight(installed);
    }
    #[cfg(target_os = "linux")]
    if linux_bundle::is_bundle_path(installed) {
        return linux_bundle::preflight(installed);
    }
    ensure_supported()?;
    validate_destination(installed)?;
    let parent = installed.parent().unwrap();
    #[cfg(target_os = "macos")]
    {
        // Darwin's extended access rights check effective ACLs and delete
        // authorization without requiring ownership or write access to the
        // bundle's contents. A root-owned app can still be replaceable.
        const DELETE_OK: libc::c_int = 1 << 12;
        const ADD_SUBDIRECTORY_REMOVE_CHILD_SEARCH: libc::c_int = (1 << 13) | (1 << 14) | (1 << 11);
        replacement_access(installed, DELETE_OK)?;
        replacement_access(parent, ADD_SUBDIRECTORY_REMOVE_CHILD_SEARCH)?;
        // SF_NOUNLINK is public in Darwin's sys/stat.h but not in libc.
        const SF_NOUNLINK: u32 = 0x0010_0000;
        const NO_RENAME_FLAGS: u32 = libc::UF_IMMUTABLE
            | libc::UF_APPEND
            | libc::SF_IMMUTABLE
            | libc::SF_APPEND
            | SF_NOUNLINK;
        if fs::symlink_metadata(installed)?.st_flags() & NO_RENAME_FLAGS != 0 {
            return Err(InstallError::NotifyOnly(
                "this application's file flags prevent replacement; install the update manually",
            ));
        }
    }
    let _probe = tempfile::Builder::new()
        .prefix(".maple-agent-write-probe-")
        .tempfile_in(parent)
        .map_err(destination_error)?;
    // Discover legacy locks owned privately by another account before any
    // running tasks are stopped. Opening alone does not contend with Ready.
    let _lock = open_install_lock(installed)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn replacement_access(path: &Path, rights: libc::c_int) -> Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| InstallError::Invalid("invalid installation path"))?;
    // Extended access bits are defined in Darwin's sys/unistd.h. AT_EACCESS
    // uses the same credentials as the subsequent renamex_np operation.
    if unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), rights, libc::AT_EACCESS) } != 0 {
        return Err(destination_error(io::Error::last_os_error()));
    }
    Ok(())
}

pub fn acquire_install_lock(installed: &Path) -> Result<InstallLock> {
    if cfg!(windows) {
        windows_bundle::validate_destination(installed)?;
    } else if linux_bundle::is_bundle_path(installed) {
        linux_bundle::validate_destination(installed)?;
    } else {
        validate_destination(installed)?;
    }
    let file = open_install_lock(installed)?;
    file.try_lock_exclusive().map_err(|e| {
        // fs2 reports native lock errors. On Windows ERROR_LOCK_VIOLATION
        // is not classified as WouldBlock by std::io, unlike Unix flock.
        let native_contended = fs2::lock_contended_error().raw_os_error();
        let contended = e
            .raw_os_error()
            .is_some_and(|code| Some(code) == native_contended);
        if e.kind() == io::ErrorKind::WouldBlock || contended {
            InstallError::Busy
        } else {
            destination_error(e)
        }
    })?;
    validate_install_lock_path(installed, &file)?;
    #[cfg(windows)]
    windows_bundle::validate_destination(installed)?;
    Ok(InstallLock { _file: file })
}

#[cfg(windows)]
fn install_lock_path(installed: &Path) -> Result<PathBuf> {
    Ok(installed.join(".install.lock"))
}

#[cfg(not(windows))]
fn install_lock_path(installed: &Path) -> Result<PathBuf> {
    let parent = installed
        .parent()
        .ok_or(InstallError::Invalid("missing installation parent"))?;
    // Both macOS names share a lock: their staging directories share a parent,
    // and startup cleanup must not sweep another name's live preparation.
    // Use the legacy key so already-running older clients also serialize.
    let lock_identity = if installed
        .file_name()
        .is_some_and(|name| name == WORK_APP_NAME)
    {
        parent.join(APP_NAME)
    } else {
        installed.to_owned()
    };
    let digest = hex::encode(Sha256::digest(lock_identity.as_os_str().as_encoded_bytes()));
    Ok(parent.join(format!(".maple-agent-install-{digest}.lock")))
}

#[cfg(windows)]
fn open_install_lock(installed: &Path) -> Result<File> {
    let file = windows_open_private(&install_lock_path(installed)?, false)?;
    validate_install_lock_path(installed, &file)?;
    Ok(file)
}

#[cfg(not(windows))]
fn open_install_lock(installed: &Path) -> Result<File> {
    let path = install_lock_path(installed)?;
    let mut create = OpenOptions::new();
    create.read(true).write(true).create_new(true);
    #[cfg(unix)]
    create
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let file = match create.open(&path) {
        Ok(file) => {
            // The process umask may be private; all accounts using this app
            // need to open the same inode. No account needs to write its bytes.
            #[cfg(unix)]
            file.set_permissions(fs::Permissions::from_mode(0o644))?;
            file
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let mut open = OpenOptions::new();
            open.read(true);
            #[cfg(unix)]
            open.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
            open.open(&path).map_err(destination_error)?
        }
        Err(error) => return Err(destination_error(error)),
    };
    #[cfg(unix)]
    {
        let metadata = file.metadata()?;
        if metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o7777 == 0o600
        {
            // Upgrade the original private lock in place. Replacing its inode
            // would allow an already-running updater to escape serialization.
            file.set_permissions(fs::Permissions::from_mode(0o644))?;
        }
    }
    validate_install_lock_path(installed, &file)?;
    Ok(file)
}

fn validate_install_lock_path(installed: &Path, file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    let path_metadata = fs::symlink_metadata(install_lock_path(installed)?)?;
    if !metadata.is_file() || !path_metadata.is_file() {
        return Err(InstallError::NotifyOnly(
            "the shared updater lock is not a regular file; install the update manually",
        ));
    }
    #[cfg(unix)]
    if metadata.nlink() != 1
        || metadata.mode() & 0o7022 != 0
        || metadata.dev() != path_metadata.dev()
        || metadata.ino() != path_metadata.ino()
    {
        return Err(InstallError::NotifyOnly(
            "the shared updater lock changed or has unsafe permissions; install the update manually",
        ));
    }
    Ok(())
}

/// The caller retains the application lock for this preparation's entire
/// lifetime, including while Ready, until it is installed or discarded.
pub fn prepare(update: &PreparedUpdate) -> Result<InstallPreparation> {
    if cfg!(windows) {
        windows_bundle::prepare(update)
    } else if cfg!(target_os = "linux") {
        linux_bundle::prepare(update)
    } else {
        preflight_installation(&update.installed_path)?;
        prepare_inner(update, Verifier::Native)
    }
}

pub fn revalidate(preparation: &InstallPreparation, update: &PreparedUpdate) -> Result<()> {
    if cfg!(windows) {
        windows_bundle::revalidate(preparation, update)
    } else if cfg!(target_os = "linux") {
        linux_bundle::revalidate(preparation, update)
    } else {
        preflight_installation(&update.installed_path)?;
        revalidate_inner(preparation, update, Verifier::Native)
    }
}

/// Apply the updater's exact archive acceptance rules to a production release
/// payload in CI, without extracting or trusting any archive entry.
pub fn validate_archive(payload: &Path) -> Result<()> {
    scan_archive(payload, None).map(|_| ())
}

/// Remove a withdrawn/cancelled preparation before replacement. Once an install
/// receipt exists the directory may contain the prior app, so only a verified
/// new startup may clear it through checkpoint cleanup.
pub fn discard_preparation(preparation: &InstallPreparation) -> Result<()> {
    if cfg!(windows) {
        return windows_bundle::discard(preparation);
    }
    if linux_bundle::is_bundle_path(&preparation.installed_path) {
        return linux_bundle::discard(preparation);
    }
    validate_destination(&preparation.installed_path)?;
    validate_staging(&preparation.installed_path, &preparation.staging_dir)?;
    if preparation.payload_path != preparation.staging_dir.join("payload")
        || preparation.staged_path != preparation.staging_dir.join(APP_NAME)
        || fs::symlink_metadata(preparation.staging_dir.join(RECEIPT_NAME)).is_ok()
    {
        return Err(InstallError::Invalid(
            "cannot discard a changed preparation or retained backup",
        ));
    }
    fs::remove_dir_all(&preparation.staging_dir)?;
    sync_directory(preparation.installed_path.parent().unwrap())
}

/// Requires the caller to retain the application lock and profile host fences.
/// It rechecks the downloaded archive, every staged byte, and platform identity
/// immediately before the atomic swap. No downloaded executable is launched.
pub fn replace(
    preparation: &InstallPreparation,
    update: &PreparedUpdate,
) -> Result<InstalledUpdate> {
    if cfg!(windows) {
        windows_bundle::replace(preparation, update)
    } else if cfg!(target_os = "linux") {
        linux_bundle::replace(preparation, update)
    } else {
        ensure_supported()?;
        replace_inner(preparation, update, Verifier::Native, Synchronizer::Native)
    }
}

fn replace_inner(
    preparation: &InstallPreparation,
    update: &PreparedUpdate,
    verifier: Verifier,
    synchronizer: Synchronizer,
) -> Result<InstalledUpdate> {
    revalidate_inner(preparation, update, verifier)?;
    let mut installed = InstalledUpdate {
        installed_path: preparation.installed_path.clone(),
        staging_dir: preparation.staging_dir.clone(),
        backup_path: preparation.staged_path.clone(),
        version: preparation.version.clone(),
        build: preparation.build.clone(),
        durability_warning: false,
    };
    // A receipt preceding the swap survives a crash before profile-state save.
    // Old startups cannot clear it: checkpoint requires the NEW version/build.
    write_receipt(&installed)?;
    if let Err(error) = swap_bundles(&preparation.installed_path, &preparation.staged_path) {
        // Failed atomic swaps leave both paths unchanged. Remove the pending
        // receipt so the caller can discard or retry this preparation.
        let _ = fs::remove_file(preparation.staging_dir.join(RECEIPT_NAME));
        return Err(error);
    }
    // Once swap succeeds this is a committed install, including on an I/O
    // error while making directory changes durable. Returning an ordinary Err
    // would keep the old process running after its host has been stopped.
    // Always hand the new installation to relaunch; retain the receipt and old
    // bundle for its checkpoint regardless of synchronization outcome.
    let stage_synced = synchronizer.sync(&preparation.staging_dir).is_ok();
    let parent_synced = synchronizer
        .sync(preparation.installed_path.parent().unwrap())
        .is_ok();
    installed.durability_warning = !stage_synced || !parent_synced;
    Ok(installed)
}

impl InstalledUpdate {
    /// The installed directory remains state identity; Linux executes AppRun.
    pub fn restart_path(&self) -> PathBuf {
        if cfg!(windows) {
            self.installed_path.join("maple-agent.exe")
        } else if linux_bundle::is_bundle_path(&self.installed_path) {
            self.installed_path.join("AppRun")
        } else {
            self.installed_path.clone()
        }
    }

    /// Call only after the new app reached its startup checkpoint. On failure,
    /// retain the backup; automatic crash rollback is deliberately not implied.
    pub fn checkpoint_cleanup(&self) -> Result<()> {
        let _lock = acquire_install_lock(&self.installed_path)?;
        self.cleanup_locked()
    }

    fn cleanup_locked(&self) -> Result<()> {
        validate_staging(&self.installed_path, &self.staging_dir)?;
        if self.backup_path != self.staging_dir.join(APP_NAME) {
            return Err(InstallError::Invalid("backup path changed"));
        }
        verify_bundle(&self.installed_path, &self.version, &self.build)?;
        fs::remove_dir_all(&self.staging_dir)?;
        sync_directory(self.installed_path.parent().unwrap())
    }
}

/// Recover receipts even if the process exited after swapping but before saving
/// its profile state. Invalid/stale receipts leave their backups untouched.
pub fn cleanup_after_startup(installed: &Path, version: &str, build: &str) -> Result<usize> {
    ensure_supported()?;
    let _lock = acquire_install_lock(installed)?;
    verify_bundle(installed, version, build)?;
    cleanup_staging_locked(installed, version, build)
}

// An active preparation retains the application lock even while the user
// defers Restart. Once this lock is available, no receipt means a crashed or
// cancelled preparation, rather than another process's live Ready update.
fn cleanup_staging_locked(installed: &Path, version: &str, build: &str) -> Result<usize> {
    let parent = installed
        .parent()
        .ok_or(InstallError::Invalid("missing installation parent"))?;
    let mut cleaned = 0;
    for entry in fs::read_dir(parent)?.take(4096) {
        let entry = entry?;
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(STAGING_PREFIX))
        {
            continue;
        }
        let root = entry.path();
        if validate_staging(installed, &root).is_err() {
            continue;
        }
        match fs::symlink_metadata(root.join(RECEIPT_NAME)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::remove_dir_all(&root)?;
                sync_directory(parent)?;
                cleaned += 1;
                continue;
            }
            // Any receipt, including malformed or unreadable state, may be a
            // retained prior app after a committed swap. Preserve uncertainty.
            Ok(_) => {}
            Err(_) => continue,
        }
        let Ok(mut receipt) = open_regular(&root.join(RECEIPT_NAME), false, false) else {
            continue;
        };
        if receipt.metadata()?.len() > 16 * 1024 {
            continue;
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut receipt)
            .take(16 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        let Ok(saved) = serde_json::from_slice::<InstalledUpdate>(&bytes) else {
            continue;
        };
        if saved.installed_path == installed
            && saved.staging_dir == root
            && saved.version == version
            && saved.build == build
            && saved.cleanup_locked().is_ok()
        {
            cleaned += 1;
        }
    }
    Ok(cleaned)
}

/// Packaging assigns CFBundleVersion after compilation. Read it from the
/// current bundle, then authenticate it before trusting a matching receipt.
pub fn cleanup_current_installation(installed: &Path, running_version: &str) -> Result<usize> {
    ensure_supported()?;
    let (version, build) = bundle_version(installed)?;
    if version != running_version {
        return Err(InstallError::Invalid(
            "installed version differs from running app",
        ));
    }
    verify_bundle(installed, &version, &build)?;
    cleanup_after_startup(installed, &version, &build)
}

fn ensure_supported() -> Result<()> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Ok(())
    } else {
        Err(InstallError::NotifyOnly(
            "automatic installation is not enabled on this platform",
        ))
    }
}

#[derive(Clone, Copy)]
enum Verifier {
    Native,
    #[cfg(all(test, unix))]
    Test(fn(&Path, &str, &str) -> Result<()>),
}

#[derive(Clone, Copy)]
enum Synchronizer {
    Native,
    #[cfg(all(test, unix))]
    Test(fn(&Path) -> Result<()>),
}

impl Synchronizer {
    fn sync(self, directory: &Path) -> Result<()> {
        match self {
            Self::Native => sync_directory(directory),
            #[cfg(all(test, unix))]
            Self::Test(sync) => sync(directory),
        }
    }
}

impl Verifier {
    fn verify(self, bundle: &Path, version: &str, build: &str) -> Result<()> {
        match self {
            Self::Native => verify_bundle(bundle, version, build),
            #[cfg(all(test, unix))]
            Self::Test(verify) => verify(bundle, version, build),
        }
    }
}

fn expected_artifact(update: &PreparedUpdate) -> Result<&Artifact> {
    crate::manifest::validate_manifest(&update.release.manifest)
        .map_err(|_| InstallError::Invalid("manifest fields changed"))?;
    update
        .release
        .manifest
        .artifacts
        .get(Target::DarwinAarch64.key())
        .ok_or(InstallError::Invalid("missing macOS payload"))
}

fn prepare_inner(update: &PreparedUpdate, verifier: Verifier) -> Result<InstallPreparation> {
    validate_destination(&update.installed_path)?;
    let artifact = expected_artifact(update)?;
    validate_upgrade_destination(update, verifier)?;
    let parent = update.installed_path.parent().unwrap();
    let temporary = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .rand_bytes(STAGING_RANDOM_LENGTH)
        .tempdir_in(parent)
        .map_err(destination_error)?;
    #[cfg(unix)]
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
    let payload_path = temporary.path().join("payload");
    copy_verified_payload(&update.payload_path, &payload_path, artifact)?;
    let expected = scan_archive(&payload_path, Some(temporary.path()))?;
    let staged_path = temporary.path().join(APP_NAME);
    if scan_tree(&staged_path)? != expected {
        return Err(InstallError::Invalid("staged content differs from archive"));
    }
    verifier.verify(
        &staged_path,
        &update.release.manifest.version,
        &update.release.manifest.build,
    )?;
    sync_directory(temporary.path())?;
    let preparation = InstallPreparation {
        installed_path: update.installed_path.clone(),
        staging_dir: temporary.path().to_owned(),
        payload_path,
        staged_path,
        version: update.release.manifest.version.clone(),
        build: update.release.manifest.build.clone(),
        size: artifact.size,
        sha256: artifact.sha256.clone(),
    };
    let _ = temporary.keep();
    Ok(preparation)
}

fn revalidate_inner(
    preparation: &InstallPreparation,
    update: &PreparedUpdate,
    verifier: Verifier,
) -> Result<()> {
    validate_destination(&update.installed_path)?;
    validate_upgrade_destination(update, verifier)?;
    validate_staging(&update.installed_path, &preparation.staging_dir)?;
    let artifact = expected_artifact(update)?;
    if preparation.installed_path != update.installed_path
        || preparation.payload_path != preparation.staging_dir.join("payload")
        || preparation.staged_path != preparation.staging_dir.join(APP_NAME)
        || preparation.version != update.release.manifest.version
        || preparation.build != update.release.manifest.build
        || preparation.size != artifact.size
        || preparation.sha256 != artifact.sha256
    {
        return Err(InstallError::Invalid("persisted preparation changed"));
    }
    verify_payload(&preparation.payload_path, artifact)?;
    let expected = scan_archive(&preparation.payload_path, None)?;
    if scan_tree(&preparation.staged_path)? != expected {
        return Err(InstallError::Invalid(
            "staged content differs from signed archive",
        ));
    }
    verifier.verify(
        &preparation.staged_path,
        &preparation.version,
        &preparation.build,
    )
}

fn validate_upgrade_destination(update: &PreparedUpdate, verifier: Verifier) -> Result<()> {
    let (version, build) = bundle_version(&update.installed_path)?;
    let installed = crate::manifest::stable_version(&version)
        .map_err(|_| InstallError::Invalid("invalid installed version"))?;
    let proposed = crate::manifest::stable_version(&update.release.manifest.version)
        .map_err(|_| InstallError::Invalid("invalid proposed version"))?;
    if proposed <= installed {
        return Err(InstallError::Invalid(
            "installation already has this version or a newer version",
        ));
    }
    verifier.verify(&update.installed_path, &version, &build)
}

fn validate_destination(installed: &Path) -> Result<()> {
    if !installed.is_absolute()
        || installed
            .to_str()
            .is_none_or(|path| path.chars().any(char::is_control))
        || installed
            .file_name()
            .is_none_or(|n| n != APP_NAME && n != WORK_APP_NAME)
        || installed
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        || installed
            .components()
            .any(|c| c.as_os_str() == "AppTranslocation")
        || installed.starts_with("/Volumes")
    {
        return Err(InstallError::NotifyOnly(
            "move Maple Work to a writable installed application directory",
        ));
    }
    if fs::symlink_metadata(installed)?.file_type().is_symlink()
        || !fs::symlink_metadata(installed)?.is_dir()
        || installed.canonicalize()? != installed
    {
        return Err(InstallError::Invalid(
            "installation path is not a canonical application directory",
        ));
    }
    Ok(())
}

fn destination_error(error: io::Error) -> InstallError {
    if permission_error(&error) {
        InstallError::NotifyOnly(
            "this account cannot replace the application; install the update manually",
        )
    } else {
        error.into()
    }
}

fn permission_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
        || matches!(
            error.raw_os_error(),
            Some(libc::EACCES | libc::EROFS | libc::EPERM)
        )
}

fn validate_staging(installed: &Path, root: &Path) -> Result<()> {
    if root.parent() != installed.parent()
        || !root.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
            n.strip_prefix(STAGING_PREFIX).is_some_and(|suffix| {
                suffix.len() == STAGING_RANDOM_LENGTH
                    && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
            })
        })
        || root.canonicalize()? != root
    {
        return Err(InstallError::Invalid(
            "staging is not a private installation sibling",
        ));
    }
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(InstallError::Invalid("staging directory changed"));
    }
    #[cfg(unix)]
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
        || metadata.dev() != fs::metadata(installed)?.dev()
    {
        return Err(InstallError::Invalid(
            "staging ownership, permissions or volume changed",
        ));
    }
    Ok(())
}

fn open_regular(path: &Path, create: bool, exclusive: bool) -> Result<File> {
    #[cfg(windows)]
    if !create {
        return windows_open_readonly(path);
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(create)
        .create(create)
        .create_new(exclusive);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(InstallError::Invalid("expected a regular file"));
    }
    Ok(file)
}

fn verify_payload(path: &Path, artifact: &Artifact) -> Result<()> {
    let mut file = open_regular(path, false, false)?;
    if file.metadata()?.len() != artifact.size {
        return Err(InstallError::Invalid("payload size changed"));
    }
    if hex::encode(hash_reader(&mut file, artifact.size)?) != artifact.sha256 {
        return Err(InstallError::Invalid("payload digest changed"));
    }
    Ok(())
}

fn copy_verified_payload(source: &Path, destination: &Path, artifact: &Artifact) -> Result<()> {
    let mut input = open_regular(source, false, false)?;
    if input.metadata()?.len() != artifact.size {
        return Err(InstallError::Invalid("payload size changed"));
    }
    let mut output = open_regular(destination, true, true)?;
    let mut hasher = Sha256::new();
    let mut remaining = artifact.size;
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let length = remaining.min(buffer.len() as u64) as usize;
        let read = input.read(&mut buffer[..length])?;
        if read == 0 {
            return Err(InstallError::Invalid("payload truncated"));
        }
        output.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    if input.read(&mut buffer[..1])? != 0 || hex::encode(hasher.finalize()) != artifact.sha256 {
        return Err(InstallError::Invalid("payload digest changed"));
    }
    output.sync_all()?;
    Ok(())
}

fn hash_reader(reader: &mut impl Read, expected: u64) -> Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut read = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        read = read
            .checked_add(count as u64)
            .ok_or(InstallError::Invalid("size overflow"))?;
        if read > expected {
            return Err(InstallError::Invalid("file grew during verification"));
        }
        hasher.update(&buffer[..count]);
    }
    if read != expected {
        return Err(InstallError::Invalid("file truncated during verification"));
    }
    Ok(hasher.finalize().into())
}

#[derive(Debug, PartialEq, Eq)]
enum Node {
    Directory,
    File {
        mode: u32,
        size: u64,
        digest: [u8; 32],
    },
    Symlink(String),
}

type ArchiveInventory = (BTreeMap<String, Node>, Option<Vec<u8>>);

/// Raw iteration prevents tar's GNU/PAX preprocessing from allocating an
/// attacker-declared extension size. Extensions are bounded and parsed here;
/// no ownership, xattrs, flags, hard links, sparse or special files are applied.
fn scan_archive(payload: &Path, output: Option<&Path>) -> Result<BTreeMap<String, Node>> {
    scan_archive_with_alias(payload, output, APP_NAME, true).map(|(nodes, _)| nodes)
}

fn scan_archive_root(
    payload: &Path,
    output: Option<&Path>,
    app_name: &str,
) -> Result<ArchiveInventory> {
    scan_archive_with_alias(payload, output, app_name, false)
}

fn scan_archive_with_alias(
    payload: &Path,
    output: Option<&Path>,
    app_name: &str,
    allow_work_name: bool,
) -> Result<ArchiveInventory> {
    let mut archive_root = None;
    let mut descriptor = None;
    let input = open_regular(payload, false, false)?;
    let decoded = MultiGzDecoder::new(input);
    let bounded = ExpansionLimit {
        inner: decoded,
        remaining: MAX_EXPANDED_BYTES,
    };
    let mut archive = tar::Archive::new(bounded);
    let mut nodes = BTreeMap::new();
    let mut explicit = BTreeSet::new();
    let mut pax = BTreeMap::new();
    let mut long_name = None;
    let mut long_link = None;
    let mut expanded = 0u64;
    for (count, entry) in archive.entries()?.raw(true).enumerate() {
        if count >= MAX_ENTRIES {
            return Err(InstallError::Invalid("too many archive members"));
        }
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        let size = entry.size();
        if kind.is_pax_local_extensions() || kind.is_gnu_longname() || kind.is_gnu_longlink() {
            if size > MAX_EXTENSION_BYTES {
                return Err(InstallError::Invalid("oversized archive extension"));
            }
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            if kind.is_pax_local_extensions() {
                if !pax.is_empty() {
                    return Err(InstallError::Invalid("duplicate archive extensions"));
                }
                for extension in tar::PaxExtensions::new(&bytes) {
                    let extension = extension?;
                    let key = extension
                        .key()
                        .map_err(|_| InstallError::Invalid("invalid archive extension"))?;
                    let value = extension
                        .value()
                        .map_err(|_| InstallError::Invalid("invalid archive extension"))?;
                    if !matches!(
                        key,
                        "path"
                            | "linkpath"
                            | "mtime"
                            | "atime"
                            | "ctime"
                            | "uid"
                            | "gid"
                            | "uname"
                            | "gname"
                    ) || pax.insert(key.to_owned(), value.to_owned()).is_some()
                    {
                        return Err(InstallError::Invalid("unsupported archive metadata"));
                    }
                }
            } else {
                let value = std::str::from_utf8(bytes.strip_suffix(&[0]).unwrap_or(&bytes))
                    .map_err(|_| InstallError::Invalid("non-UTF-8 archive path"))?
                    .to_owned();
                let previous = if kind.is_gnu_longname() {
                    long_name.replace(value)
                } else {
                    long_link.replace(value)
                };
                if previous.is_some() {
                    return Err(InstallError::Invalid("duplicate archive long name"));
                }
            }
            continue;
        }
        let header_path = entry.header().path_bytes();
        let header_path = std::str::from_utf8(&header_path)
            .map_err(|_| InstallError::Invalid("non-UTF-8 archive path"))?;
        let name = pax
            .remove("path")
            .or(long_name.take())
            .unwrap_or_else(|| header_path.to_owned());
        // Admit exactly one known root, then normalize only that root to the
        // legacy staging name. Mixed roots are rejected even for disjoint files.
        let root = name.split('/').next().unwrap_or_default();
        if root != app_name && !(allow_work_name && root == WORK_APP_NAME) {
            return Err(InstallError::Invalid("unexpected application root"));
        }
        if archive_root
            .as_deref()
            .is_some_and(|previous| previous != root)
        {
            return Err(InstallError::Invalid("mixed application roots"));
        }
        archive_root = Some(root.to_owned());
        let checked = archive_path(&name, kind.is_dir(), root)?;
        let name = format!("{app_name}{}", &checked[root.len()..]);
        let name = archive_path(&name, kind.is_dir(), app_name)?;
        if !explicit.insert(name.clone()) {
            return Err(InstallError::Invalid("duplicate archive path"));
        }
        let mode = entry.header().mode()?;
        let node = if kind.is_dir() {
            if mode != 0o755 || size != 0 {
                return Err(InstallError::Invalid("unsafe archive directory metadata"));
            }
            if let Some(root) = output {
                create_public_directory(&root.join(&name))?;
            }
            Node::Directory
        } else if kind.is_file() {
            if !matches!(mode, 0o644 | 0o755) || size > MAX_FILE_BYTES {
                return Err(InstallError::Invalid("unsafe file size or permissions"));
            }
            expanded = expanded
                .checked_add(size)
                .ok_or(InstallError::Invalid("expanded size overflow"))?;
            if expanded > MAX_EXPANDED_BYTES {
                return Err(InstallError::Invalid("archive expansion limit exceeded"));
            }
            let digest = if app_name != APP_NAME
                && (name == format!("{app_name}/bundle.json")
                    || (app_name == "maple-agent-windows"
                        && name == format!("{app_name}/bundle-info.json")))
            {
                if size > 16 * 1024 {
                    return Err(InstallError::Invalid("bundle descriptor is too large"));
                }
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                if bytes.len() as u64 != size {
                    return Err(InstallError::Invalid("truncated bundle descriptor"));
                }
                if let Some(root) = output {
                    let destination = root.join(&name);
                    create_public_directory(destination.parent().unwrap())?;
                    let mut file = open_regular(&destination, true, true)?;
                    file.write_all(&bytes)?;
                    set_mode(&destination, mode)?;
                    file.sync_all()?;
                }
                let digest = Sha256::digest(&bytes).into();
                descriptor = Some(bytes);
                digest
            } else if let Some(root) = output {
                let destination = root.join(&name);
                create_public_directory(destination.parent().unwrap())?;
                let mut file = open_regular(&destination, true, true)?;
                let mut hasher = Sha256::new();
                let mut copied = 0u64;
                let mut buffer = [0u8; 64 * 1024];
                loop {
                    let length = entry.read(&mut buffer)?;
                    if length == 0 {
                        break;
                    }
                    copied += length as u64;
                    if copied > size {
                        return Err(InstallError::Invalid("archive member grew"));
                    }
                    file.write_all(&buffer[..length])?;
                    hasher.update(&buffer[..length]);
                }
                if copied != size {
                    return Err(InstallError::Invalid("truncated archive member"));
                }
                set_mode(&destination, mode)?;
                file.sync_all()?;
                hasher.finalize().into()
            } else {
                hash_reader(&mut entry, size)?
            };
            Node::File { mode, size, digest }
        } else if kind.is_symlink() {
            if size != 0 {
                return Err(InstallError::Invalid("symlink contains file data"));
            }
            let header_link = entry
                .header()
                .link_name_bytes()
                .ok_or(InstallError::Invalid("missing symlink target"))?;
            let header_link = std::str::from_utf8(&header_link)
                .map_err(|_| InstallError::Invalid("non-UTF-8 symlink"))?;
            let link = pax
                .remove("linkpath")
                .or(long_link.take())
                .unwrap_or_else(|| header_link.to_owned());
            validate_link(&name, &link, app_name)?;
            Node::Symlink(link)
        } else {
            return Err(InstallError::Invalid(
                "hard links, sparse and special files are not allowed",
            ));
        };
        if pax.contains_key("linkpath") || long_link.is_some() {
            return Err(InstallError::Invalid("link metadata on a non-link"));
        }
        pax.clear();
        nodes.insert(name, node);
    }
    if !pax.is_empty() || long_name.is_some() || long_link.is_some() {
        return Err(InstallError::Invalid("unconsumed archive extension"));
    }
    if nodes.get(app_name) != Some(&Node::Directory) {
        return Err(InstallError::Invalid("missing application root"));
    }
    let paths: Vec<String> = nodes.keys().cloned().collect();
    for name in paths {
        let mut parent = Path::new(&name).parent();
        while let Some(path) = parent.filter(|p| !p.as_os_str().is_empty()) {
            let parent_name = path
                .to_str()
                .ok_or(InstallError::Invalid("invalid archive path"))?
                .to_owned();
            match nodes.get(&parent_name) {
                Some(Node::Directory) => {}
                Some(_) => {
                    return Err(InstallError::Invalid(
                        "archive writes through a file or symlink",
                    ));
                }
                None => {
                    nodes.insert(parent_name, Node::Directory);
                }
            }
            parent = path.parent();
        }
    }
    validate_symlink_graph(&nodes)?;
    if let Some(root) = output {
        for (name, node) in &nodes {
            if let Node::Symlink(link) = node {
                let destination = root.join(name);
                create_public_directory(destination.parent().unwrap())?;
                #[cfg(unix)]
                std::os::unix::fs::symlink(link, destination)?;
                #[cfg(not(unix))]
                {
                    let _ = link;
                    return Err(InstallError::NotifyOnly(
                        "archive links are unsupported on this platform",
                    ));
                }
            }
        }
        for (name, node) in nodes.iter().rev() {
            if matches!(node, Node::Directory) {
                sync_directory(&root.join(name))?;
            }
        }
    }
    Ok((nodes, descriptor))
}

fn archive_path(raw: &str, directory: bool, app_name: &str) -> Result<String> {
    let name = if directory {
        raw.strip_suffix('/').unwrap_or(raw)
    } else {
        raw
    };
    if name.len() > MAX_PATH_BYTES
        || name.is_empty()
        || name.contains('\\')
        || name.chars().any(char::is_control)
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || name.split('/').next() != Some(app_name)
    {
        return Err(InstallError::Invalid("unsafe archive path"));
    }
    Ok(name.to_owned())
}

fn validate_link(name: &str, link: &str, app_name: &str) -> Result<()> {
    if link.is_empty()
        || link.len() > MAX_PATH_BYTES
        || link.starts_with('/')
        || link.contains('\\')
        || link.chars().any(char::is_control)
    {
        return Err(InstallError::Invalid("unsafe symlink target"));
    }
    let mut resolved: Vec<&str> = name.split('/').collect();
    resolved.pop();
    for part in link.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if resolved.len() <= 1 {
                    return Err(InstallError::Invalid("symlink escapes application"));
                }
                resolved.pop();
            }
            _ => resolved.push(part),
        }
    }
    if resolved.first() != Some(&app_name) {
        return Err(InstallError::Invalid("symlink escapes application"));
    }
    Ok(())
}

// Lexical containment alone is insufficient: `dir/parent/../outside` escapes
// when `dir/parent -> ..`. Resolve links against the complete virtual tree,
// including links appearing later in the archive, before creating any of them.
fn validate_symlink_graph(nodes: &BTreeMap<String, Node>) -> Result<()> {
    for (name, node) in nodes {
        let Node::Symlink(target) = node else {
            continue;
        };
        let parent = name
            .rsplit_once('/')
            .ok_or(InstallError::Invalid("invalid archive symlink"))?
            .0;
        let mut pending: VecDeque<String> = parent
            .split('/')
            .chain(target.split('/'))
            .map(str::to_owned)
            .collect();
        let mut resolved = Vec::new();
        let mut followed = 0;
        while let Some(part) = pending.pop_front() {
            match part.as_str() {
                "" | "." => continue,
                ".." => {
                    if resolved.len() <= 1 {
                        return Err(InstallError::Invalid("symlink chain escapes application"));
                    }
                    resolved.pop();
                }
                _ => {
                    resolved.push(part);
                    match nodes.get(&resolved.join("/")) {
                        Some(Node::Symlink(link)) => {
                            followed += 1;
                            if followed > 40 {
                                return Err(InstallError::Invalid("cyclic or excessive symlinks"));
                            }
                            resolved.pop();
                            for part in link.split('/').rev() {
                                pending.push_front(part.to_owned());
                            }
                        }
                        Some(Node::Directory) => {}
                        Some(Node::File { .. }) if pending.is_empty() => {}
                        Some(Node::File { .. }) => {
                            return Err(InstallError::Invalid("symlink traverses a file"));
                        }
                        None => return Err(InstallError::Invalid("dangling archive symlink")),
                    }
                }
            }
        }
    }
    Ok(())
}

fn create_public_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(InstallError::Invalid("archive parent is a file or symlink"));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or(InstallError::Invalid("missing archive parent"))?;
            // Recurse only for missing ancestors, preserving the private 0700
            // staging root while giving implicit bundle directories 0755.
            if !parent.exists() {
                create_public_directory(parent)?;
            }
            fs::create_dir(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    set_mode(path, 0o755)
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

fn scan_tree(bundle: &Path) -> Result<BTreeMap<String, Node>> {
    scan_tree_root(bundle, APP_NAME)
}

fn scan_tree_root(bundle: &Path, app_name: &str) -> Result<BTreeMap<String, Node>> {
    let mut nodes = BTreeMap::new();
    let parent = bundle
        .parent()
        .ok_or(InstallError::Invalid("missing bundle parent"))?;
    let mut pending = vec![bundle.to_owned()];
    let mut total = 0u64;
    while let Some(path) = pending.pop() {
        if nodes.len() >= MAX_ENTRIES {
            return Err(InstallError::Invalid("too many staged files"));
        }
        let name = path
            .strip_prefix(parent)
            .ok()
            .and_then(Path::to_str)
            .ok_or(InstallError::Invalid("invalid staged path"))?;
        let metadata = fs::symlink_metadata(&path)?;
        let node = if metadata.file_type().is_symlink() {
            let link = fs::read_link(&path)?;
            let link = link
                .to_str()
                .ok_or(InstallError::Invalid("invalid staged symlink"))?;
            validate_link(name, link, app_name)?;
            Node::Symlink(link.to_owned())
        } else if metadata.is_dir() {
            #[cfg(unix)]
            if metadata.mode() & 0o7777 != 0o755 {
                return Err(InstallError::Invalid(
                    "staged directory permissions changed",
                ));
            }
            for entry in fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
            Node::Directory
        } else if metadata.is_file() {
            #[cfg(unix)]
            if metadata.nlink() != 1 {
                return Err(InstallError::Invalid("staged file is hard-linked"));
            }
            total = total
                .checked_add(metadata.len())
                .ok_or(InstallError::Invalid("staged size overflow"))?;
            if metadata.len() > MAX_FILE_BYTES || total > MAX_EXPANDED_BYTES {
                return Err(InstallError::Invalid("staged content is too large"));
            }
            let mut file = open_regular(&path, false, false)?;
            #[cfg(unix)]
            let mode = metadata.mode() & 0o7777;
            #[cfg(not(unix))]
            let mode = 0o644;
            Node::File {
                mode,
                size: metadata.len(),
                digest: hash_reader(&mut file, metadata.len())?,
            }
        } else {
            return Err(InstallError::Invalid("staged special file"));
        };
        nodes.insert(name.to_owned(), node);
    }
    validate_symlink_graph(&nodes)?;
    Ok(nodes)
}

struct ExpansionLimit<R> {
    inner: R,
    remaining: u64,
}
impl<R: Read> Read for ExpansionLimit<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(io::Error::other("archive expansion limit exceeded"));
        }
        let length = output.len().min(self.remaining as usize);
        let read = self.inner.read(&mut output[..length])?;
        self.remaining -= read as u64;
        Ok(read)
    }
}

fn bundle_dictionary(bundle: &Path) -> Result<plist::Dictionary> {
    let plist_path = bundle.join("Contents/Info.plist");
    let plist = open_regular(&plist_path, false, false)?;
    if plist.metadata()?.len() > 1024 * 1024 {
        return Err(InstallError::CodeSignature);
    }
    let value = plist::Value::from_reader(plist).map_err(|_| InstallError::CodeSignature)?;
    value.into_dictionary().ok_or(InstallError::CodeSignature)
}

fn bundle_version(bundle: &Path) -> Result<(String, String)> {
    let dictionary = bundle_dictionary(bundle)?;
    let version = dictionary
        .get("CFBundleShortVersionString")
        .and_then(plist::Value::as_string)
        .ok_or(InstallError::CodeSignature)?
        .to_owned();
    let build = dictionary
        .get("CFBundleVersion")
        .and_then(plist::Value::as_string)
        .ok_or(InstallError::CodeSignature)?
        .to_owned();
    Ok((version, build))
}

fn verify_bundle(bundle: &Path, version: &str, build: &str) -> Result<()> {
    let dictionary = bundle_dictionary(bundle)?;
    for (key, expected) in [
        ("CFBundleIdentifier", "cloud.opensecret.maple.agent"),
        ("CFBundleExecutable", "maple-agent"),
        ("CFBundleShortVersionString", version),
        ("CFBundleVersion", build),
    ] {
        if dictionary.get(key).and_then(plist::Value::as_string) != Some(expected) {
            return Err(InstallError::CodeSignature);
        }
    }
    if dictionary.contains_key("LSEnvironment") {
        return Err(InstallError::CodeSignature);
    }
    let executable = fs::symlink_metadata(bundle.join("Contents/MacOS/maple-agent"))?;
    if !executable.is_file() || executable.file_type().is_symlink() {
        return Err(InstallError::CodeSignature);
    }
    #[cfg(unix)]
    if executable.mode() & 0o111 != 0o111 || executable.mode() & 0o6000 != 0 {
        return Err(InstallError::CodeSignature);
    }
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("/usr/bin/codesign")
            .args([
                "--verify",
                "--deep",
                "--strict",
                "--all-architectures",
                "--test-requirement",
                CODESIGN_REQUIREMENT,
            ])
            .arg(bundle)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if !status.success() {
            return Err(InstallError::CodeSignature);
        }
        // CodeDirectory's numeric CS_RUNTIME flag is authoritative. Do not
        // accept the word "runtime" appearing elsewhere in diagnostics.
        let mut child = std::process::Command::new("/usr/bin/codesign")
            .args(["--display", "--verbose=4"])
            .arg(bundle)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let mut output = Vec::new();
        let read = child
            .stderr
            .take()
            .ok_or(InstallError::CodeSignature)?
            .take(32 * 1024 + 1)
            .read_to_end(&mut output);
        if read.is_err() || output.len() > 32 * 1024 {
            let _ = child.kill();
            let _ = child.wait();
            return Err(InstallError::CodeSignature);
        }
        if !child.wait()?.success() || !has_hardened_runtime(&output) {
            return Err(InstallError::CodeSignature);
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    Err(InstallError::NotifyOnly(
        "macOS signature verification is unavailable",
    ))
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn has_hardened_runtime(output: &[u8]) -> bool {
    let Ok(output) = std::str::from_utf8(output) else {
        return false;
    };
    let mut directories = output
        .lines()
        .filter(|line| line.starts_with("CodeDirectory "));
    let Some(directory) = directories.next() else {
        return false;
    };
    if directories.next().is_some() {
        return false;
    }
    let mut flags = directory
        .split_ascii_whitespace()
        .filter_map(|part| part.strip_prefix("flags=0x"));
    let Some(flag) = flags.next() else {
        return false;
    };
    if flags.next().is_some() {
        return false;
    }
    let hexadecimal = flag.split('(').next().unwrap_or("");
    u32::from_str_radix(hexadecimal, 16).is_ok_and(|flags| flags & 0x10000 != 0)
}

fn write_receipt(installed: &InstalledUpdate) -> Result<()> {
    let destination = installed.staging_dir.join(RECEIPT_NAME);
    let bytes = serde_json::to_vec(installed)
        .map_err(|_| InstallError::Invalid("invalid install receipt"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(&installed.staging_dir)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&destination)
        .map_err(|error| InstallError::Io(error.error))?;
    sync_directory(&installed.staging_dir)
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        windows_sync_directory(path)
    }
    #[cfg(not(windows))]
    {
        let file = File::open(path)?;
        file.sync_all()?;
        Ok(())
    }
}

fn swap_bundles(installed: &Path, staged: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let installed = CString::new(installed.as_os_str().as_bytes())
            .map_err(|_| InstallError::Invalid("invalid installed path"))?;
        let staged = CString::new(staged.as_os_str().as_bytes())
            .map_err(|_| InstallError::Invalid("invalid staged path"))?;
        // Both paths were revalidated as ordinary directories on one volume.
        // RENAME_SWAP preserves the prior app at staged even if power is lost.
        let result =
            unsafe { libc::renamex_np(installed.as_ptr(), staged.as_ptr(), libc::RENAME_SWAP) };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (installed, staged);
        Err(InstallError::NotifyOnly(
            "atomic bundle replacement is unavailable",
        ))
    }
}

// These cases intentionally exercise Unix permissions, directory fsync, and
// framework symlinks. Windows CI provides compilation coverage, not installation.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{Downloads, Manifest, VerifiedRelease};
    use flate2::{Compression, write::GzEncoder};

    enum Member<'a> {
        Directory(&'a str),
        File(&'a str, &'a [u8], u32),
        Link(&'a str, &'a str),
        Special(&'a str, u8),
    }

    fn archive(path: &Path, members: &[Member<'_>]) {
        let output = File::create(path).unwrap();
        let encoder = GzEncoder::new(output, Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for member in members {
            let mut header = tar::Header::new_ustar();
            let (name, data, kind, mode, link) = match member {
                Member::Directory(name) => (*name, &[][..], b'5', 0o755, None),
                Member::File(name, data, mode) => (*name, *data, b'0', *mode, None),
                Member::Link(name, target) => (*name, &[][..], b'2', 0o777, Some(*target)),
                Member::Special(name, kind) => (*name, &[][..], *kind, 0o644, None),
            };
            header.set_mode(mode);
            header.set_size(data.len() as u64);
            header.set_entry_type(tar::EntryType::new(kind));
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            // Raw mutation permits deliberately unsafe path fixtures that the
            // tar writer's convenience path API would correctly reject.
            let bytes = header.as_mut_bytes();
            assert!(name.len() < 100);
            bytes[..100].fill(0);
            bytes[..name.len()].copy_from_slice(name.as_bytes());
            if let Some(link) = link {
                header.set_link_name(link).unwrap();
            }
            header.set_cksum();
            builder.append(&header, data).unwrap();
        }
        builder
            .into_inner()
            .unwrap()
            .finish()
            .unwrap()
            .sync_all()
            .unwrap();
    }

    fn fixture_members() -> Vec<Member<'static>> {
        vec![
            Member::Directory(APP_NAME),
            Member::Directory("Maple Agent.app/Contents"),
            Member::File(
                "Maple Agent.app/Contents/maple-agent",
                b"verified package executable",
                0o755,
            ),
            Member::Directory("Maple Agent.app/Contents/Frameworks"),
            Member::Directory("Maple Agent.app/Contents/Frameworks/A.framework"),
            Member::Directory("Maple Agent.app/Contents/Frameworks/A.framework/Versions"),
            Member::Directory("Maple Agent.app/Contents/Frameworks/A.framework/Versions/A"),
            Member::File(
                "Maple Agent.app/Contents/Frameworks/A.framework/Versions/A/A",
                b"dylib",
                0o755,
            ),
            Member::Link(
                "Maple Agent.app/Contents/Frameworks/A.framework/Versions/Current",
                "A",
            ),
            Member::Link(
                "Maple Agent.app/Contents/Frameworks/A.framework/A",
                "Versions/Current/A",
            ),
        ]
    }

    fn write_version(bundle: &Path, version: &str, build: &str) {
        fs::create_dir_all(bundle.join("Contents")).unwrap();
        let mut dictionary = plist::Dictionary::new();
        for (key, value) in [
            ("CFBundleIdentifier", "cloud.opensecret.maple.agent"),
            ("CFBundleExecutable", "maple-agent"),
            ("CFBundleShortVersionString", version),
            ("CFBundleVersion", build),
        ] {
            dictionary.insert(key.to_owned(), value.into());
        }
        plist::Value::Dictionary(dictionary)
            .to_file_xml(bundle.join("Contents/Info.plist"))
            .unwrap();
    }

    fn update_fixture(root: &Path) -> PreparedUpdate {
        let installed = root.join(APP_NAME);
        write_version(&installed, "0.0.1", "1");
        let payload = root.join("archive.tar.gz");
        archive(&payload, &fixture_members());
        let bytes = fs::read(&payload).unwrap();
        let artifact = Artifact {
            path: "/agent/stable/releases/0.0.2/maple-agent.app.tar.gz".into(),
            size: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(&bytes)),
        };
        let dmg = Artifact {
            path: "/agent/stable/releases/0.0.2/maple-agent.dmg".into(),
            ..artifact.clone()
        };
        PreparedUpdate {
            release: VerifiedRelease {
                manifest: Manifest {
                    format: "maple-agent-update-v1".into(),
                    product: "maple-agent".into(),
                    channel: "stable".into(),
                    version: "0.0.2".into(),
                    build: "2".into(),
                    source_sha: "a".repeat(40),
                    pub_date: "2026-10-22T00:00:00Z".into(),
                    notes: "fixture".into(),
                    artifacts: BTreeMap::from([("darwin-aarch64".into(), artifact)]),
                    downloads: Downloads {
                        macos: Some(dmg),
                        linux_appimage: None,
                        linux_bundle: None,
                        windows: None,
                    },
                },
                envelope: "test-only authenticated caller fixture".into(),
                origin: "https://updates.example.test".parse().unwrap(),
            },
            payload_path: payload,
            installed_path: installed.canonicalize().unwrap(),
        }
    }

    fn named_update_fixture(
        root: &Path,
        installed_name: &str,
        archive_name: &str,
    ) -> PreparedUpdate {
        let mut update = update_fixture(root);
        if installed_name != APP_NAME {
            let destination = root.join(installed_name);
            fs::rename(&update.installed_path, &destination).unwrap();
            update.installed_path = destination.canonicalize().unwrap();
        }
        let original = fixture_members();
        let names: Vec<_> = original
            .iter()
            .map(|member| {
                let name = match member {
                    Member::Directory(name)
                    | Member::File(name, ..)
                    | Member::Link(name, ..)
                    | Member::Special(name, ..) => name,
                };
                name.replacen(APP_NAME, archive_name, 1)
            })
            .collect();
        let members: Vec<_> = original
            .iter()
            .zip(&names)
            .map(|(member, name)| match member {
                Member::Directory(_) => Member::Directory(name),
                Member::File(_, bytes, mode) => Member::File(name, bytes, *mode),
                Member::Link(_, target) => Member::Link(name, target),
                Member::Special(_, kind) => Member::Special(name, *kind),
            })
            .collect();
        archive(&update.payload_path, &members);
        let bytes = fs::read(&update.payload_path).unwrap();
        let artifact = update
            .release
            .manifest
            .artifacts
            .get_mut("darwin-aarch64")
            .unwrap();
        artifact.size = bytes.len() as u64;
        artifact.sha256 = hex::encode(Sha256::digest(&bytes));
        update
    }

    #[test]
    fn both_bundle_names_prepare_restore_revalidate_and_discard_both_archive_names() {
        for installed_name in [APP_NAME, WORK_APP_NAME] {
            for archive_name in [APP_NAME, WORK_APP_NAME] {
                let root = tempfile::tempdir().unwrap();
                let update = named_update_fixture(root.path(), installed_name, archive_name);
                let verifier = Verifier::Test(accept_test_signature);
                let prepared = prepare_inner(&update, verifier).unwrap();
                assert_eq!(prepared.installed_path.file_name().unwrap(), installed_name);
                assert_eq!(prepared.staged_path, prepared.staging_dir.join(APP_NAME));
                let restored: InstallPreparation =
                    serde_json::from_slice(&serde_json::to_vec(&prepared).unwrap()).unwrap();
                revalidate_inner(&restored, &update, verifier).unwrap();
                let mut retargeted = restored.clone();
                retargeted.staged_path = retargeted.staging_dir.join(WORK_APP_NAME);
                assert!(revalidate_inner(&retargeted, &update, verifier).is_err());
                fs::write(
                    prepared.staged_path.join("Contents/maple-agent"),
                    b"tampered",
                )
                .unwrap();
                assert!(revalidate_inner(&restored, &update, verifier).is_err());
                discard_preparation(&prepared).unwrap();
                assert!(!prepared.staging_dir.exists());
                assert!(update.installed_path.is_dir());
            }
        }
    }

    #[test]
    fn bundle_aliases_share_the_legacy_install_lock_and_exclude_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(APP_NAME);
        let work = root.path().join(WORK_APP_NAME);
        fs::create_dir(&legacy).unwrap();
        fs::create_dir(&work).unwrap();
        let legacy = legacy.canonicalize().unwrap();
        let work = work.canonicalize().unwrap();
        let old_digest = hex::encode(Sha256::digest(legacy.as_os_str().as_encoded_bytes()));
        assert_eq!(
            install_lock_path(&work).unwrap(),
            legacy
                .parent()
                .unwrap()
                .join(format!(".maple-agent-install-{old_digest}.lock"))
        );
        let guard = acquire_install_lock(&legacy).unwrap();
        assert!(matches!(
            acquire_install_lock(&work),
            Err(InstallError::Busy)
        ));
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert!(matches!(
            cleanup_after_startup(&work, "0.0.1", "1"),
            Err(InstallError::Busy)
        ));
        drop(guard);
        let guard = acquire_install_lock(&work).unwrap();
        assert!(matches!(
            acquire_install_lock(&legacy),
            Err(InstallError::Busy)
        ));
        drop(guard);
    }

    #[test]
    fn rebrand_does_not_admit_mixed_unknown_or_unsafe_archive_roots() {
        let root = tempfile::tempdir().unwrap();
        let payload = root.path().join("package");
        for members in [
            vec![
                Member::Directory(APP_NAME),
                Member::Directory(WORK_APP_NAME),
            ],
            vec![
                Member::Directory(WORK_APP_NAME),
                Member::File("Maple Agent.app/Contents/file", b"x", 0o644),
            ],
            vec![Member::Directory("Maple Research.app")],
            vec![Member::Directory("Maple Work Dev.app")],
            vec![
                Member::Directory(WORK_APP_NAME),
                Member::File("Maple Work.app/../outside", b"x", 0o644),
            ],
            vec![
                Member::Directory(WORK_APP_NAME),
                Member::Link("Maple Work.app/escape", "../outside"),
            ],
        ] {
            archive(&payload, &members);
            assert!(scan_archive(&payload, None).is_err());
        }
        // The generic (Linux) scanner keeps its exact-root contract.
        archive(&payload, &[Member::Directory(WORK_APP_NAME)]);
        assert!(scan_archive_root(&payload, None, "maple-agent.app").is_err());
        // The former exact macOS root contract cannot consume the future name.
        assert!(scan_archive_root(&payload, None, APP_NAME).is_err());
    }

    #[test]
    fn only_the_two_production_bundle_names_are_eligible_destinations() {
        let root = tempfile::tempdir().unwrap();
        for name in [
            APP_NAME,
            WORK_APP_NAME,
            "Maple Work Dev.app",
            "Maple Research.app",
        ] {
            let bundle = root.path().join(name);
            fs::create_dir(&bundle).unwrap();
            let bundle = bundle.canonicalize().unwrap();
            assert_eq!(
                validate_destination(&bundle).is_ok(),
                matches!(name, APP_NAME | WORK_APP_NAME)
            );
        }
        let alias_parent = root.path().join("alias-parent");
        fs::create_dir(&alias_parent).unwrap();
        let alias = alias_parent.join(WORK_APP_NAME);
        std::os::unix::fs::symlink(root.path().join(WORK_APP_NAME), &alias).unwrap();
        assert!(validate_destination(&alias).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn both_bundle_names_swap_in_place_and_preserve_sibling_installations_and_backups() {
        for installed_name in [APP_NAME, WORK_APP_NAME] {
            for archive_name in [APP_NAME, WORK_APP_NAME] {
                let root = tempfile::tempdir().unwrap();
                let update = named_update_fixture(root.path(), installed_name, archive_name);
                let sibling_name = if installed_name == APP_NAME {
                    WORK_APP_NAME
                } else {
                    APP_NAME
                };
                let sibling = root.path().join(sibling_name);
                fs::create_dir(&sibling).unwrap();
                fs::write(sibling.join("untouched"), b"sibling").unwrap();
                let verifier = Verifier::Test(accept_test_signature);
                let prepared = prepare_inner(&update, verifier).unwrap();
                let committed =
                    replace_inner(&prepared, &update, verifier, Synchronizer::Native).unwrap();
                assert_eq!(committed.restart_path(), update.installed_path);
                assert_eq!(
                    bundle_version(&committed.backup_path).unwrap(),
                    ("0.0.1".into(), "1".into())
                );
                assert_eq!(
                    fs::read(committed.installed_path.join("Contents/maple-agent")).unwrap(),
                    b"verified package executable"
                );
                assert_eq!(fs::read(sibling.join("untouched")).unwrap(), b"sibling");
                let restored: InstalledUpdate = serde_json::from_slice(
                    &fs::read(committed.staging_dir.join(RECEIPT_NAME)).unwrap(),
                )
                .unwrap();
                assert_eq!(restored.installed_path, update.installed_path);
                assert_eq!(restored.backup_path, committed.backup_path);
                assert!(discard_preparation(&prepared).is_err());
                assert!(
                    restored.checkpoint_cleanup().is_err(),
                    "unsigned fixture cannot authorize backup deletion"
                );
                assert!(restored.backup_path.is_dir());
            }
        }
    }

    fn accept_test_signature(_: &Path, _: &str, _: &str) -> Result<()> {
        Ok(())
    }

    #[test]
    fn extracts_internal_framework_symlinks_and_compares_every_staged_byte() {
        let root = tempfile::tempdir().unwrap();
        let payload = root.path().join("package");
        let staging = root.path().join("staging");
        fs::create_dir(&staging).unwrap();
        set_mode(&staging, 0o700).unwrap();
        archive(&payload, &fixture_members());
        let expected = scan_archive(&payload, Some(&staging)).unwrap();
        assert_eq!(expected, scan_tree(&staging.join(APP_NAME)).unwrap());
        assert_eq!(expected, scan_archive(&payload, None).unwrap());
        fs::write(
            staging.join("Maple Agent.app/Contents/maple-agent"),
            b"tampered",
        )
        .unwrap();
        assert_ne!(expected, scan_tree(&staging.join(APP_NAME)).unwrap());
        #[cfg(unix)]
        assert_eq!(fs::metadata(&staging).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn rejects_traversal_duplicate_hardlink_special_and_privileged_members() {
        let root = tempfile::tempdir().unwrap();
        let payload = root.path().join("package");
        for member in [
            Member::File("Maple Agent.app/../outside", b"x", 0o644),
            Member::File("/Maple Agent.app/absolute", b"x", 0o644),
            Member::File("Other.app/Contents/file", b"x", 0o644),
            Member::File("Maple Agent.app/./ambiguous", b"x", 0o644),
            Member::File("Maple Agent.app/private", b"x", 0o600),
            Member::File("Maple Agent.app/setuid", b"x", 0o4755),
            Member::Special("Maple Agent.app/fifo", b'6'),
            Member::Special("Maple Agent.app/hardlink", b'1'),
            Member::Special("Maple Agent.app/sparse", b'S'),
            Member::Directory(APP_NAME),
        ] {
            archive(&payload, &[Member::Directory(APP_NAME), member]);
            assert!(scan_archive(&payload, None).is_err());
        }
    }

    #[test]
    fn rejects_escaping_links_and_writes_through_internal_links_in_both_orders() {
        let root = tempfile::tempdir().unwrap();
        let payload = root.path().join("package");
        for target in ["/outside", "../../outside", "../outside", "\\outside"] {
            archive(
                &payload,
                &[
                    Member::Directory(APP_NAME),
                    Member::Link("Maple Agent.app/link", target),
                ],
            );
            assert!(scan_archive(&payload, None).is_err());
        }
        for reversed in [false, true] {
            let mut members = vec![
                Member::Directory(APP_NAME),
                Member::Link("Maple Agent.app/alias", "Contents"),
                Member::File("Maple Agent.app/alias/file", b"x", 0o644),
            ];
            if reversed {
                members.swap(1, 2);
            }
            archive(&payload, &members);
            assert!(scan_archive(&payload, None).is_err());
        }
        archive(
            &payload,
            &[
                Member::Directory(APP_NAME),
                Member::Directory("Maple Agent.app/dir"),
                Member::Link("Maple Agent.app/dir/parent", ".."),
                Member::Link("Maple Agent.app/link", "dir/parent/../outside"),
            ],
        );
        assert!(matches!(
            scan_archive(&payload, None),
            Err(InstallError::Invalid("symlink chain escapes application"))
        ));
        archive(
            &payload,
            &[
                Member::Directory(APP_NAME),
                Member::Link("Maple Agent.app/cycle", "cycle"),
            ],
        );
        assert!(scan_archive(&payload, None).is_err());
    }

    #[test]
    fn bounds_extensions_before_allocating_and_rejects_unsafe_metadata() {
        let root = tempfile::tempdir().unwrap();
        let payload = root.path().join("package");
        let mut encoder = GzEncoder::new(File::create(&payload).unwrap(), Compression::fast());
        let mut header = tar::Header::new_ustar();
        header.set_path("extended").unwrap();
        header.set_entry_type(tar::EntryType::new(b'x'));
        header.set_size(MAX_EXTENSION_BYTES + 1);
        header.set_cksum();
        // A malicious size must fail before reading its missing body.
        encoder.write_all(header.as_bytes()).unwrap();
        encoder.finish().unwrap();
        assert!(matches!(
            scan_archive(&payload, None),
            Err(InstallError::Invalid("oversized archive extension"))
        ));
        let encoder = GzEncoder::new(File::create(&payload).unwrap(), Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        builder
            .append_pax_extensions([(
                "SCHILY.xattr.com.apple.quarantine",
                b"do not apply".as_slice(),
            )])
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        assert!(matches!(
            scan_archive(&payload, None),
            Err(InstallError::Invalid("unsupported archive metadata"))
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reads_native_release_tar_format_and_preserves_bundle_modes() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let bundle = source.join(APP_NAME);
        fs::create_dir_all(bundle.join("Contents/Frameworks/A.framework/Versions/A")).unwrap();
        write_version(&bundle, "0.0.2", "2");
        fs::write(
            bundle.join("Contents/Frameworks/A.framework/Versions/A/A"),
            b"dylib",
        )
        .unwrap();
        let mut pending = vec![source.clone()];
        while let Some(path) = pending.pop() {
            if path.is_dir() {
                set_mode(&path, 0o755).unwrap();
                for entry in fs::read_dir(&path).unwrap() {
                    pending.push(entry.unwrap().path());
                }
            } else {
                set_mode(&path, 0o644).unwrap();
            }
        }
        std::os::unix::fs::symlink(
            "A",
            bundle.join("Contents/Frameworks/A.framework/Versions/Current"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "Versions/Current/A",
            bundle.join("Contents/Frameworks/A.framework/A"),
        )
        .unwrap();
        let payload = root.path().join("release.app.tar.gz");
        let status = std::process::Command::new("/usr/bin/tar")
            .env("COPYFILE_DISABLE", "1")
            .arg("-czf")
            .arg(&payload)
            .arg("-C")
            .arg(&source)
            .arg(APP_NAME)
            .status()
            .unwrap();
        assert!(status.success());
        let output = root.path().join("output");
        fs::create_dir(&output).unwrap();
        let expected = scan_archive(&payload, Some(&output)).unwrap();
        assert_eq!(expected, scan_tree(&output.join(APP_NAME)).unwrap());
    }

    #[test]
    fn preparation_rechecks_saved_paths_archive_staging_and_installed_version() {
        let root = tempfile::tempdir().unwrap();
        let update = update_fixture(root.path());
        let verifier = Verifier::Test(accept_test_signature);
        let prepared = prepare_inner(&update, verifier).unwrap();
        revalidate_inner(&prepared, &update, verifier).unwrap();
        let mut changed = prepared.clone();
        changed.staged_path = update.installed_path.clone();
        assert!(revalidate_inner(&changed, &update, verifier).is_err());
        fs::write(prepared.staged_path.join("extra"), b"not in signed archive").unwrap();
        assert!(revalidate_inner(&prepared, &update, verifier).is_err());
        fs::remove_file(prepared.staged_path.join("extra")).unwrap();
        fs::write(&prepared.payload_path, b"corrupted archive").unwrap();
        assert!(revalidate_inner(&prepared, &update, verifier).is_err());
        write_version(&update.installed_path, "0.0.3", "3");
        assert!(matches!(
            prepare_inner(&update, verifier),
            Err(InstallError::Invalid(
                "installation already has this version or a newer version"
            ))
        ));
    }

    #[test]
    fn separate_installers_exclude_each_other_across_profiles() {
        let root = tempfile::tempdir().unwrap();
        let installed = root.path().join(APP_NAME);
        fs::create_dir(&installed).unwrap();
        let installed = installed.canonicalize().unwrap();
        let first = acquire_install_lock(&installed).unwrap();
        assert!(matches!(
            acquire_install_lock(&installed),
            Err(InstallError::Busy)
        ));
        drop(first);
        assert!(acquire_install_lock(&installed).is_ok());
    }

    #[test]
    fn shared_read_only_lock_excludes_installers_without_owner_only_permissions() {
        let root = tempfile::tempdir().unwrap();
        let installed = root.path().join(APP_NAME);
        fs::create_dir(&installed).unwrap();
        let installed = installed.canonicalize().unwrap();
        let lock = acquire_install_lock(&installed).unwrap();
        let path = install_lock_path(&installed).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o644);
        drop(lock);

        // A different account opens this same shared inode read-only. flock
        // still grants exclusive locks on that descriptor, on macOS and Linux.
        let read_only = OpenOptions::new().read(true).open(&path).unwrap();
        FileExt::try_lock_exclusive(&read_only).unwrap();
        assert!(matches!(
            acquire_install_lock(&installed),
            Err(InstallError::Busy)
        ));
        FileExt::unlock(&read_only).unwrap();
        drop(read_only);
        assert!(acquire_install_lock(&installed).is_ok());
    }

    #[test]
    fn dropping_install_guard_unlocks_while_an_inherited_description_remains_open() {
        let root = tempfile::tempdir().unwrap();
        let installed = root.path().join(APP_NAME);
        fs::create_dir(&installed).unwrap();
        let installed = installed.canonicalize().unwrap();
        let first = acquire_install_lock(&installed).unwrap();
        // dup and fork share the same open file description. Retaining this
        // duplicate models a child between fork and exec without forking from
        // the multithreaded test harness.
        let inherited = first._file.try_clone().unwrap();
        assert!(matches!(
            acquire_install_lock(&installed),
            Err(InstallError::Busy)
        ));
        drop(first);
        let second = acquire_install_lock(&installed)
            .expect("the guard releases ownership even before an inherited descriptor closes");
        drop(inherited);
        assert!(matches!(
            acquire_install_lock(&installed),
            Err(InstallError::Busy)
        ));
        drop(second);
        assert!(acquire_install_lock(&installed).is_ok());
    }

    #[test]
    fn migrates_owned_private_install_lock_without_replacing_or_unlocking_it() {
        let root = tempfile::tempdir().unwrap();
        let installed = root.path().join(APP_NAME);
        fs::create_dir(&installed).unwrap();
        let installed = installed.canonicalize().unwrap();
        let path = install_lock_path(&installed).unwrap();
        let original = open_regular(&path, true, true).unwrap();
        original
            .set_permissions(fs::Permissions::from_mode(0o600))
            .unwrap();
        FileExt::try_lock_exclusive(&original).unwrap();
        let inode = original.metadata().unwrap().ino();
        assert!(matches!(
            acquire_install_lock(&installed),
            Err(InstallError::Busy)
        ));
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(metadata.ino(), inode);
        assert_eq!(metadata.mode() & 0o777, 0o644);
        // This fixture is a raw File, not an InstallLock guard. Explicitly
        // release it before close, including when concurrent native commands
        // temporarily inherited its open description before their exec.
        FileExt::unlock(&original).unwrap();
        drop(original);
        acquire_install_lock(&installed).expect("the released migrated lock can be reacquired");
    }

    #[test]
    fn shared_install_lock_rejects_writable_links_and_replaced_inodes() {
        let root = tempfile::tempdir().unwrap();
        let installed = root.path().join(APP_NAME);
        fs::create_dir(&installed).unwrap();
        let installed = installed.canonicalize().unwrap();
        let path = install_lock_path(&installed).unwrap();
        let file = open_install_lock(&installed).unwrap();
        file.set_permissions(fs::Permissions::from_mode(0o666))
            .unwrap();
        assert!(matches!(
            acquire_install_lock(&installed),
            Err(InstallError::NotifyOnly(_))
        ));
        file.set_permissions(fs::Permissions::from_mode(0o644))
            .unwrap();
        let alias = root.path().join("lock-alias");
        fs::hard_link(&path, &alias).unwrap();
        assert!(validate_install_lock_path(&installed, &file).is_err());
        fs::remove_file(alias).unwrap();
        fs::remove_file(&path).unwrap();
        let _replacement = open_install_lock(&installed).unwrap();
        assert!(validate_install_lock_path(&installed, &file).is_err());
    }

    #[test]
    fn permission_failures_require_manual_installation_without_hiding_busy_or_integrity_errors() {
        for code in [libc::EACCES, libc::EPERM, libc::EROFS] {
            assert!(
                InstallError::Io(io::Error::from_raw_os_error(code)).requires_manual_installation()
            );
        }
        assert!(InstallError::NotifyOnly("manual").requires_manual_installation());
        assert!(!InstallError::Busy.requires_manual_installation());
        assert!(!InstallError::CodeSignature.requires_manual_installation());
        assert!(!InstallError::Invalid("changed payload").requires_manual_installation());
        assert!(
            !InstallError::Io(io::Error::from_raw_os_error(libc::ENOSPC))
                .requires_manual_installation()
        );
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn native_replacement_preflight_accepts_a_supported_bundle_and_detects_delete_acl() {
        let root = tempfile::tempdir().unwrap();
        let installed = root.path().join(APP_NAME);
        fs::create_dir(&installed).unwrap();
        let installed = installed.canonicalize().unwrap();
        preflight_installation(&installed).unwrap();
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone deny delete"])
            .arg(&installed)
            .status()
            .unwrap();
        assert!(status.success());
        let result = preflight_installation(&installed);
        let cleared = std::process::Command::new("/bin/chmod")
            .arg("-N")
            .arg(&installed)
            .status()
            .unwrap();
        assert!(cleared.success(), "restore the fixture's deletability");
        assert!(matches!(result, Err(InstallError::NotifyOnly(_))));
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn native_replacement_preflight_rejects_immutable_app_flags() {
        let root = tempfile::tempdir().unwrap();
        let installed = root.path().join(APP_NAME);
        fs::create_dir(&installed).unwrap();
        let installed = installed.canonicalize().unwrap();
        preflight_installation(&installed).unwrap();
        let status = std::process::Command::new("/usr/bin/chflags")
            .arg("uchg")
            .arg(&installed)
            .status()
            .unwrap();
        assert!(status.success());
        let result = preflight_installation(&installed);
        let cleared = std::process::Command::new("/usr/bin/chflags")
            .arg("nouchg")
            .arg(&installed)
            .status()
            .unwrap();
        assert!(cleared.success(), "restore the fixture's deletability");
        assert!(matches!(result, Err(InstallError::NotifyOnly(_))));
    }

    #[test]
    fn deferred_preparation_is_protected_then_reclaimed_after_process_lock_release() {
        let root = tempfile::tempdir().unwrap();
        let update = update_fixture(root.path());
        let ready_guard = acquire_install_lock(&update.installed_path).unwrap();
        let prepared = prepare_inner(&update, Verifier::Test(accept_test_signature)).unwrap();
        assert!(matches!(
            acquire_install_lock(&update.installed_path),
            Err(InstallError::Busy)
        ));
        assert!(
            prepared.staging_dir.is_dir(),
            "another startup cannot sweep a live Ready preparation"
        );
        // A process exit releases this guard even when its preparation was
        // deferred. The next startup can safely reclaim its entire stage.
        drop(ready_guard);
        let _startup_guard = acquire_install_lock(&update.installed_path).unwrap();
        assert_eq!(
            cleanup_staging_locked(&update.installed_path, "0.0.1", "1").unwrap(),
            1
        );
        assert!(!prepared.staging_dir.exists());
        assert!(update.installed_path.is_dir());
        assert!(update.payload_path.is_file());
    }

    #[test]
    fn orphan_sweep_preserves_any_receipt_and_unrelated_private_directories() {
        let root = tempfile::tempdir().unwrap();
        let update = update_fixture(root.path());
        let _startup_guard = acquire_install_lock(&update.installed_path).unwrap();
        let prepared = prepare_inner(&update, Verifier::Test(accept_test_signature)).unwrap();
        fs::write(
            prepared.staging_dir.join(RECEIPT_NAME),
            b"truncated uncertain committed receipt",
        )
        .unwrap();
        let unrelated = tempfile::Builder::new()
            .prefix(".maple-agent-update-other-")
            .tempdir_in(update.installed_path.parent().unwrap())
            .unwrap();
        assert_eq!(
            cleanup_staging_locked(&update.installed_path, "0.0.1", "1").unwrap(),
            0
        );
        assert!(prepared.staging_dir.is_dir());
        assert!(unrelated.path().is_dir());
    }

    #[test]
    fn rejects_saved_symlink_staging_and_cross_installation_binding() {
        let root = tempfile::tempdir().unwrap();
        let update = update_fixture(root.path());
        let prepared = prepare_inner(&update, Verifier::Test(accept_test_signature)).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let mut changed = prepared.clone();
        changed.staging_dir = elsewhere.path().to_owned();
        assert!(
            revalidate_inner(&changed, &update, Verifier::Test(accept_test_signature)).is_err()
        );
        #[cfg(unix)]
        {
            let alias = root.path().join(format!("{STAGING_PREFIX}alias"));
            std::os::unix::fs::symlink(&prepared.staging_dir, &alias).unwrap();
            assert!(validate_staging(&update.installed_path, &alias).is_err());
        }
    }

    #[test]
    fn native_identity_validation_never_accepts_fake_bundle() {
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join(APP_NAME);
        write_version(&bundle, "0.0.2", "2");
        assert!(verify_bundle(&bundle, "0.0.2", "2").is_err());
        assert!(verify_bundle(&bundle, "0.0.3", "2").is_err());
        assert!(verify_bundle(&bundle, "0.0.2", "3").is_err());
    }

    #[test]
    fn developer_id_requirement_and_numeric_hardened_runtime_are_mandatory() {
        assert!(
            CODESIGN_REQUIREMENT.starts_with('='),
            "codesign requires = for inline requirement text"
        );
        assert!(CODESIGN_REQUIREMENT.contains("identifier \"cloud.opensecret.maple.agent\""));
        assert!(CODESIGN_REQUIREMENT.contains("certificate leaf[subject.OU] = \"X773Y823TN\""));
        assert!(
            CODESIGN_REQUIREMENT.contains("certificate 1[field.1.2.840.113635.100.6.2.6] exists")
        );
        assert!(
            CODESIGN_REQUIREMENT
                .contains("certificate leaf[field.1.2.840.113635.100.6.1.13] exists")
        );
        assert!(has_hardened_runtime(b"CodeDirectory v=20500 size=1024 flags=0x10000(runtime) hashes=10+7 location=embedded\n"));
        for output in [
            "CodeDirectory v=20500 flags=0x0(none)\nruntime=true\n",
            "CodeDirectory v=20500 flags=0x2(adhoc)\n",
            "CodeDirectory v=20500 flags=0x20000(linker-signed)\n",
            "CodeDirectory v=20500 flags=0x100000000(runtime)\n",
            "CodeDirectory flags=0x10000(runtime) flags=0x0\n",
            "CodeDirectory flags=0x10000(runtime)\nCodeDirectory flags=0x0\n",
            "Authority=runtime\n",
        ] {
            assert!(
                !has_hardened_runtime(output.as_bytes()),
                "accepted {output:?}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn requirement_is_accepted_by_the_native_requirement_compiler() {
        let status = std::process::Command::new("/usr/bin/csreq")
            .args(["-r", CODESIGN_REQUIREMENT])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn replacement_failure_preserves_current_app_and_staging() {
        let root = tempfile::tempdir().unwrap();
        let update = update_fixture(root.path());
        let prepared = prepare_inner(&update, Verifier::Test(accept_test_signature)).unwrap();
        assert!(swap_bundles(&prepared.installed_path, &root.path().join("missing")).is_err());
        assert!(prepared.installed_path.is_dir());
        assert!(prepared.staged_path.is_dir());
        assert_eq!(bundle_version(&prepared.installed_path).unwrap().0, "0.0.1");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn successful_swap_returns_committed_restart_path_when_post_swap_sync_fails() {
        let root = tempfile::tempdir().unwrap();
        let update = update_fixture(root.path());
        let verifier = Verifier::Test(accept_test_signature);
        let prepared = prepare_inner(&update, verifier).unwrap();
        let committed = replace_inner(
            &prepared,
            &update,
            verifier,
            Synchronizer::Test(|_| Err(io::Error::other("injected post-swap sync failure").into())),
        )
        .expect("successful swap must always return the committed installation");
        assert!(committed.durability_warning);
        assert_eq!(committed.installed_path, update.installed_path);
        assert_eq!(
            fs::read(committed.installed_path.join("Contents/maple-agent")).unwrap(),
            b"verified package executable"
        );
        assert_eq!(bundle_version(&committed.backup_path).unwrap().0, "0.0.1");
        assert!(committed.staging_dir.join(RECEIPT_NAME).is_file());
        assert!(
            discard_preparation(&prepared).is_err(),
            "backup must survive discard attempts"
        );
        assert!(
            committed.checkpoint_cleanup().is_err(),
            "fake unsigned new app cannot pass checkpoint"
        );
        assert!(committed.backup_path.is_dir());
    }
}

// This fixture exercises the Unix bundle path contract. Windows public lock
// admission additionally requires a registered, signed versioned installation;
// its native storage/locking primitive is exercised by the required Windows
// native_install_lock_rejects_concurrent_writers test.
#[cfg(all(test, unix))]
mod unix_lock_tests {
    use super::*;

    #[test]
    fn contention_is_busy_and_the_existing_lock_can_be_reacquired_after_drop() {
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().canonicalize().unwrap().join(APP_NAME);
        fs::create_dir(&installed).unwrap();
        let first = acquire_install_lock(&installed).unwrap();
        let error = match acquire_install_lock(&installed) {
            Ok(_) => panic!("a second updater acquired the held installation lock"),
            Err(error) => error,
        };
        assert!(
            matches!(&error, InstallError::Busy),
            "unexpected contention result: {error:?}"
        );
        drop(first);
        let _next = acquire_install_lock(&installed)
            .expect("the existing lock must be acquirable after its owner drops it");
    }
}
