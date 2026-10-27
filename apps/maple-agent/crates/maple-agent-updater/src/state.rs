#[cfg(not(windows))]
use std::{fs::OpenOptions, io::Read};
use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::manifest::{VerifiedRelease, stable_version};

const MAX_STATE_BYTES: u64 = 512 * 1024;

#[derive(Clone, Debug)]
pub struct PreparedUpdate {
    pub release: VerifiedRelease,
    pub payload_path: PathBuf,
    pub installed_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredPreparation {
    pub envelope: String,
    pub origin: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    format: String,
    installed_path: PathBuf,
    highest_verified: Option<String>,
    prepared: Option<StoredPreparation>,
}

pub(crate) struct StateStore {
    pub root: PathBuf,
    installed_path: PathBuf,
}

impl StateStore {
    pub fn new(root: &Path, installed_path: &Path) -> Result<Self> {
        private_directory(root)?;
        let store = Self {
            root: root.canonicalize()?,
            installed_path: installed_path
                .canonicalize()
                .context("installed application no longer exists")?,
        };
        store.transaction(|_| Ok(()))?;
        private_directory(&store.root.join("payloads"))?;
        Ok(store)
    }

    pub fn installed_path(&self) -> &Path {
        &self.installed_path
    }

    pub fn highest_verified(&self) -> Result<Option<Version>> {
        self.transaction(|record| {
            record
                .highest_verified
                .as_deref()
                .map(stable_version)
                .transpose()
        })
    }

    pub fn advance(&self, version: &Version) -> Result<()> {
        self.transaction(|record| {
            if let Some(highest) = &record.highest_verified {
                ensure!(
                    version >= &stable_version(highest)?,
                    "update is below highest verified version"
                );
            }
            record.highest_verified = Some(version.to_string());
            Ok(())
        })
    }

    pub fn prepared(&self) -> Result<Option<StoredPreparation>> {
        self.transaction(|record| Ok(record.prepared.clone()))
    }

    pub fn save_prepared(&self, release: &VerifiedRelease) -> Result<()> {
        self.transaction(|record| {
            ensure!(
                record
                    .highest_verified
                    .as_deref()
                    .map(stable_version)
                    .transpose()?
                    .as_ref()
                    .is_some_and(|highest| highest == &release.version()),
                "preparation was superseded"
            );
            record.prepared = Some(StoredPreparation {
                envelope: release.envelope.clone(),
                origin: release.origin.as_str().to_owned(),
            });
            Ok(())
        })
    }

    pub fn take_prepared(
        &self,
        expected_envelope: Option<&str>,
    ) -> Result<Option<StoredPreparation>> {
        self.transaction(|record| {
            if expected_envelope.is_some_and(|expected| {
                record
                    .prepared
                    .as_ref()
                    .is_none_or(|prepared| prepared.envelope != expected)
            }) {
                return Ok(None);
            }
            Ok(record.prepared.take())
        })
    }

    pub fn with_commit_guard<T>(
        &self,
        version: &Version,
        envelope: &str,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let lock = open_private(&self.root.join("state.lock"), false)?;
        FileExt::try_lock_exclusive(&lock).context("another update transaction is in progress")?;
        let bytes = read_bounded(&self.root.join("state.json"), MAX_STATE_BYTES)?
            .context("prepared update state is missing")?;
        let record: Record = serde_json::from_slice(&bytes).context("invalid updater state")?;
        ensure!(
            record.format == "maple-agent-updater-state-v1",
            "unsupported updater state"
        );
        ensure!(
            record.installed_path == self.installed_path,
            "preparation belongs to another installation"
        );
        ensure!(
            record
                .highest_verified
                .as_deref()
                .map(stable_version)
                .transpose()?
                .as_ref()
                .is_none_or(|highest| version >= highest),
            "prepared update was superseded by a newer verified release"
        );
        ensure!(
            record
                .prepared
                .as_ref()
                .is_some_and(|prepared| prepared.envelope == envelope),
            "prepared update is no longer current"
        );
        // The callback may atomically commit a replacement. Nothing fallible
        // can follow it: no serialization, persistence or explicit unlock that
        // could incorrectly turn a committed installation into a failure.
        // File's RAII close releases the retained lock after the result exists.
        operation()
    }

    pub fn payload_path(&self, digest: &str, partial: bool) -> PathBuf {
        self.root.join("payloads").join(format!(
            "{digest}.{}",
            if partial { "part" } else { "payload" }
        ))
    }

    fn transaction<T>(&self, operation: impl FnOnce(&mut Record) -> Result<T>) -> Result<T> {
        let lock = open_private(&self.root.join("state.lock"), false)?;
        // A commit may retain this lock while replacing an application. Other
        // async callers fail busy rather than blocking a Tokio worker thread.
        FileExt::try_lock_exclusive(&lock).context("another update transaction is in progress")?;
        let path = self.root.join("state.json");
        let mut rebound = false;
        let mut record = match read_bounded(&path, MAX_STATE_BYTES)? {
            Some(bytes) => {
                let mut record: Record = serde_json::from_slice(&bytes)
                    .context("invalid updater state; refusing to reset rollback protection")?;
                ensure!(
                    record.format == "maple-agent-updater-state-v1",
                    "unsupported updater state"
                );
                if let Some(highest) = &record.highest_verified {
                    stable_version(highest)?;
                }
                // Copying a DMG app into /Applications, relocating it, and a
                // trusted manual reinstall must not permanently disable the
                // updater. Preparation belongs to an installation; rollback
                // memory belongs to this profile and survives the move.
                if record.installed_path != self.installed_path {
                    record.installed_path = self.installed_path.clone();
                    record.prepared = None;
                    rebound = true;
                }
                record
            }
            None => Record {
                format: "maple-agent-updater-state-v1".to_owned(),
                installed_path: self.installed_path.clone(),
                highest_verified: None,
                prepared: None,
            },
        };
        let before = serde_json::to_vec(&record)?;
        let result = operation(&mut record)?;
        let after = serde_json::to_vec(&record)?;
        if before != after || rebound {
            ensure!(
                after.len() <= MAX_STATE_BYTES as usize,
                "updater state is too large"
            );
            let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
            temporary.write_all(&after)?;
            temporary.as_file().sync_all()?;
            #[cfg(windows)]
            {
                let (file, temporary_path) = temporary
                    .keep()
                    .context("cannot retain updater state temporary")?;
                drop(file);
                if let Err(error) = crate::installer::windows_atomic_replace(&temporary_path, &path)
                {
                    let _ = fs::remove_file(&temporary_path);
                    return Err(error).context("cannot persist updater state");
                }
            }
            #[cfg(not(windows))]
            {
                temporary
                    .persist(&path)
                    .context("cannot persist updater state")?;
            }
            sync_directory(&self.root)?;
        }
        FileExt::unlock(&lock)?;
        Ok(result)
    }
}

pub(crate) fn private_directory(path: &Path) -> Result<()> {
    #[cfg(windows)]
    return crate::installer::windows_private_directory(path).map_err(Into::into);
    #[cfg(not(windows))]
    {
        fs::create_dir_all(path)?;
        ensure!(
            fs::symlink_metadata(path)?.file_type().is_dir(),
            "updater directory is not a real directory"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

pub(crate) fn open_private(path: &Path, truncate: bool) -> Result<File> {
    #[cfg(windows)]
    return crate::installer::windows_open_private(path, truncate).map_err(Into::into);
    #[cfg(not(windows))]
    {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .truncate(truncate);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(path)?;
        ensure!(
            file.metadata()?.file_type().is_file(),
            "updater file is not regular"
        );
        Ok(file)
    }
}

pub(crate) fn open_readonly(path: &Path) -> Result<File> {
    #[cfg(windows)]
    return crate::installer::windows_open_readonly(path).map_err(Into::into);
    #[cfg(not(windows))]
    {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(path)?;
        ensure!(file.metadata()?.is_file(), "updater file is not regular");
        Ok(file)
    }
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(windows)]
    return crate::installer::windows_sync_directory(path).map_err(Into::into);
    #[cfg(not(windows))]
    {
        File::open(path)?.sync_all()?;
        Ok(())
    }
}

pub(crate) fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(windows)]
    return crate::installer::windows_atomic_replace(source, destination).map_err(Into::into);
    #[cfg(not(windows))]
    {
        fs::rename(source, destination)?;
        Ok(())
    }
}

pub(crate) fn read_bounded(path: &Path, maximum: u64) -> Result<Option<Vec<u8>>> {
    #[cfg(windows)]
    return crate::installer::windows_read_bounded(path, maximum).map_err(Into::into);
    #[cfg(not(windows))]
    {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            file.metadata()?.is_file(),
            "updater state is not a regular file"
        );
        ensure!(
            file.metadata()?.len() <= maximum,
            "updater state is too large"
        );
        let mut bytes = Vec::new();
        file.take(maximum + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= maximum as usize,
            "updater state is too large"
        );
        Ok(Some(bytes))
    }
}
