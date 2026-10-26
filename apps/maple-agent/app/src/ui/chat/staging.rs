//! Controller-local stable attachment copies. No staged path is interpreted by
//! a remote execution host. Disk work is called only on a blocking worker.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, SystemTime},
};

pub(super) const MAX_FILE_BYTES: u64 = 30 * 1024 * 1024;
/// Crashed processes cannot replay drafts. Reclaim their abandoned copies after
/// seven days; an OS lock excludes every still-live window/process.
const STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_SWEEP_ENTRIES: usize = 512;

const MAX_LIVE_FILES: usize = 64;
const MAX_LIVE_BYTES: u64 = 300 * 1024 * 1024;

#[derive(Default)]
struct StagingBudget {
    files: AtomicUsize,
    bytes: AtomicU64,
}

struct Reservation {
    budget: Arc<StagingBudget>,
    bytes: u64,
}

impl StagingBudget {
    fn reserve(self: &Arc<Self>, bytes: u64) -> Result<Reservation, String> {
        self.files
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |files| {
                (files < MAX_LIVE_FILES).then_some(files + 1)
            })
            .map_err(
                |_| "Too many retained attachments. Remove some draft files before attaching more.",
            )?;
        if self
            .bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|next| *next <= MAX_LIVE_BYTES)
            })
            .is_err()
        {
            self.files.fetch_sub(1, Ordering::AcqRel);
            return Err("Retained attachments exceed 300 MiB. Remove some draft files before attaching more.".into());
        }
        Ok(Reservation {
            budget: self.clone(),
            bytes,
        })
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.bytes.fetch_sub(self.bytes, Ordering::AcqRel);
        self.budget.files.fetch_sub(1, Ordering::AcqRel);
    }
}

fn budget() -> &'static Arc<StagingBudget> {
    static BUDGET: OnceLock<Arc<StagingBudget>> = OnceLock::new();
    BUDGET.get_or_init(|| Arc::new(StagingBudget::default()))
}

pub(super) struct StagedFile {
    pub path: PathBuf,
    owner: Option<File>,
    _reservation: Reservation,
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        // Last-drop can happen during a GPUI update. Never remove files there.
        self.owner.take();
        if let Some(directory) = self.path.parent()
            && let Some(sender) = cleanup_sender()
        {
            let _ = sender.send(directory.to_path_buf());
        }
    }
}

fn cleanup_sender() -> &'static Option<mpsc::Sender<PathBuf>> {
    static SENDER: OnceLock<Option<mpsc::Sender<PathBuf>>> = OnceLock::new();
    SENDER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<PathBuf>();
        std::thread::Builder::new()
            .name("attachment-cleanup".into())
            .spawn(move || {
                for directory in receiver {
                    remove_staged_directory(&directory);
                }
            })
            .ok()?;
        Some(sender)
    })
}

fn remove_staged_directory(directory: &Path) {
    // Remove only our known files, never recurse through unknown entries.
    let _ = fs::remove_file(directory.join("payload"));
    let _ = fs::remove_file(directory.join("owner"));
    let _ = fs::remove_dir(directory);
}

pub(super) fn root() -> PathBuf {
    #[cfg(not(test))]
    return crate::backend::profile_data_root().join("draft-attachments");
    #[cfg(test)]
    {
        static ROOT: OnceLock<tempfile::TempDir> = OnceLock::new();
        ROOT.get_or_init(|| tempfile::tempdir().unwrap())
            .path()
            .join("draft-attachments")
    }
}

fn private_directory(path: &Path) -> Result<(), String> {
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err("Attachment staging directory is unsafe".into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o077 != 0 {
                    return Err("Attachment staging directory is not private".into());
                }
            }
            Ok(())
        }
        Err(error) => Err(format!("Could not prepare attachment staging: {error}")),
    }
}

fn file_options(create: bool) -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options
}

fn sweep(root: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.take(MAX_SWEEP_ENTRIES).flatten() {
        if uuid::Uuid::parse_str(&entry.file_name().to_string_lossy()).is_err() {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let stale = metadata
            .modified()
            .ok()
            .and_then(|time| now.duration_since(time).ok())
            .is_some_and(|age| age >= STALE_AFTER);
        if !stale {
            continue;
        }
        let Ok(owner) = file_options(false).open(path.join("owner")) else {
            continue;
        };
        if owner.try_lock().is_err() {
            continue;
        }
        drop(owner);
        remove_staged_directory(&path);
    }
}

fn begin(root: &Path, reservation: Reservation) -> Result<(Arc<StagedFile>, File), String> {
    // The profile root is already owned by Maple; tests use an isolated parent.
    // Create the staging child with its own private permissions in both cases.
    if let Some(parent) = root.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    private_directory(root)?;
    sweep(root, SystemTime::now());
    let directory = root.join(uuid::Uuid::new_v4().to_string());
    private_directory(&directory)?;
    let mut staged = StagedFile {
        path: directory.join("payload"),
        owner: None,
        _reservation: reservation,
    };
    let owner = file_options(true)
        .open(directory.join("owner"))
        .map_err(|e| e.to_string())?;
    owner.try_lock().map_err(|e| e.to_string())?;
    staged.owner = Some(owner);
    let output = file_options(true)
        .open(&staged.path)
        .map_err(|e| e.to_string())?;
    let staged = Arc::new(staged);
    Ok((staged, output))
}

pub(super) fn copy_file(root: &Path, source: &Path) -> Result<(Arc<StagedFile>, u64), String> {
    let mut input_options = OpenOptions::new();
    input_options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        input_options.custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let input = input_options
        .open(source)
        .map_err(|e| format!("Could not open the attachment: {e}"))?;
    let metadata = input.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("Choose a file, not a folder".into());
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err("Files must be 30 MiB or smaller".into());
    }
    let reserved_size = metadata.len();
    let reservation = budget().reserve(reserved_size)?;
    let (staged, mut output) = begin(root, reservation)?;
    let copied = std::io::copy(&mut input.take(reserved_size + 1), &mut output)
        .map_err(|e| e.to_string())?;
    if copied > reserved_size {
        return Err("The file grew while it was being attached. Please attach it again.".into());
    }
    output.sync_all().map_err(|e| e.to_string())?;
    Ok((staged, copied))
}

pub(super) fn copy_bytes(root: &Path, bytes: &[u8]) -> Result<Arc<StagedFile>, String> {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("Files must be 30 MiB or smaller".into());
    }
    let reservation = budget().reserve(bytes.len() as u64)?;
    let (staged, mut output) = begin(root, reservation)?;
    output.write_all(bytes).map_err(|e| e.to_string())?;
    output.sync_all().map_err(|e| e.to_string())?;
    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_staging_budget_counts_files_and_bytes_and_releases_on_drop() {
        let budget = Arc::new(StagingBudget::default());
        let reservation = budget.reserve(MAX_LIVE_BYTES).unwrap();
        assert!(budget.reserve(1).is_err());
        assert_eq!(budget.files.load(Ordering::Acquire), 1);
        drop(reservation);
        assert_eq!(budget.bytes.load(Ordering::Acquire), 0);
        let reservations: Vec<_> = (0..MAX_LIVE_FILES)
            .map(|_| budget.reserve(0).unwrap())
            .collect();
        assert!(budget.reserve(0).is_err());
        drop(reservations);
        assert_eq!(budget.files.load(Ordering::Acquire), 0);
        assert!(budget.reserve(MAX_LIVE_BYTES).is_ok());
    }

    #[test]
    fn staging_path_failure_releases_its_reservation() {
        let directory = tempfile::tempdir().unwrap();
        let not_a_directory = directory.path().join("occupied");
        fs::write(&not_a_directory, b"unrelated").unwrap();
        let budget = Arc::new(StagingBudget::default());
        let reserved = budget.reserve(123).unwrap();
        assert!(begin(&not_a_directory, reserved).is_err());
        assert_eq!(budget.files.load(Ordering::Acquire), 0);
        assert_eq!(budget.bytes.load(Ordering::Acquire), 0);
        assert_eq!(fs::read(not_a_directory).unwrap(), b"unrelated");
    }

    #[test]
    fn zip_copy_survives_source_changes_and_removal() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("report.zip");
        fs::write(&source, b"PK\x03\x04original archive").unwrap();
        let (staged, size) = copy_file(&directory.path().join("staging"), &source).unwrap();
        fs::write(&source, b"changed").unwrap();
        fs::remove_file(source).unwrap();
        assert_eq!(
            fs::read(&staged.path).unwrap(),
            b"PK\x03\x04original archive"
        );
        assert_eq!(size, 20);
    }

    #[cfg(unix)]
    #[test]
    fn special_files_are_rejected_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("upload.pipe");
        let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let result = copy_file(&directory.path().join("staging"), &fifo);
        assert!(result.err().unwrap().contains("not a folder"));
        assert!(!directory.path().join("staging").exists());
    }

    #[test]
    fn final_draft_drop_cleans_only_its_staged_copy() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("staging");
        let staged = copy_bytes(&root, b"kept").unwrap();
        let path = staged.path.clone();
        let second_owner = staged.clone();
        drop(staged);
        assert!(path.exists());
        drop(second_owner);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!path.exists());
        assert!(root.exists());
    }

    #[test]
    fn restart_sweep_keeps_live_files_even_after_seven_days() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("staging");
        let staged = copy_bytes(&root, b"live draft").unwrap();
        let stale_at = fs::metadata(staged.path.parent().unwrap())
            .unwrap()
            .modified()
            .unwrap()
            + STALE_AFTER
            + Duration::from_secs(1);
        sweep(&root, stale_at);
        assert_eq!(fs::read(&staged.path).unwrap(), b"live draft");
        // Model an abandoned copy without running StagedFile::drop. Closing
        // one File alone does not release a lease held by duplicated or
        // inherited descriptors; establish the unlocked precondition explicitly.
        let path = staged.path.clone();
        let mut staged = Arc::try_unwrap(staged).ok().unwrap();
        let owner = staged.owner.take().unwrap();
        owner.unlock().unwrap();
        drop(owner);
        sweep(&root, stale_at);
        assert!(!path.exists());
        drop(staged);
    }

    #[cfg(unix)]
    #[test]
    fn restart_sweep_preserves_a_lease_held_by_a_duplicate() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("staging");
        let staged = copy_bytes(&root, b"live duplicate").unwrap();
        let mut staged = Arc::try_unwrap(staged).ok().unwrap();
        let duplicate = staged.owner.as_ref().unwrap().try_clone().unwrap();
        let stale_at = fs::metadata(staged.path.parent().unwrap())
            .unwrap()
            .modified()
            .unwrap()
            + STALE_AFTER
            + Duration::from_secs(1);
        drop(staged.owner.take());

        let contender = file_options(false)
            .open(staged.path.parent().unwrap().join("owner"))
            .unwrap();
        assert!(matches!(
            contender.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        sweep(&root, stale_at);
        assert_eq!(fs::read(&staged.path).unwrap(), b"live duplicate");

        duplicate.unlock().unwrap();
        sweep(&root, stale_at);
        assert!(!staged.path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn permissive_staging_root_is_rejected_without_changing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("staging");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let error = copy_bytes(&root, b"private attachment").err().unwrap();
        assert!(error.contains("not private"));
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn concurrent_first_copies_share_a_privately_created_staging_root() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("staging");
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    copy_bytes(&root, b"private attachment").unwrap()
                })
            })
            .collect();
        let staged: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        for file in &staged {
            assert_eq!(fs::read(&file.path).unwrap(), b"private attachment");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&root).unwrap().permissions().mode() & 0o077, 0);
        }
    }
}
