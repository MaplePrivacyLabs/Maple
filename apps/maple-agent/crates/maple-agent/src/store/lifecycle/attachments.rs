//! Attachment filesystem ownership during generation replacement.
//!
//! A reset removes only the named old generation. Salvage copies retained ready
//! files before publishing its replacement database, keeping the two generations
//! independent and making both crash cleanup and later reset unambiguous.

use sha2::{Digest, Sha256};

use super::{
    unclassified_identity::{FileIdentity, capture_path},
    *,
};

const COPY_BYTES: usize = 1024 * 1024;
const ROW_BATCH: i64 = 100;

pub(super) fn root_identity(paths: &StorePaths) -> StoreResult<Option<FileIdentity>> {
    let root = paths.root().join("attachments");
    match capture_path(&root, true) {
        Ok((identity, _)) => Ok(Some(identity)),
        Err(StoreError::Io {
            kind: io::ErrorKind::NotFound,
        }) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn verify_root(paths: &StorePaths, expected: &Option<FileIdentity>) -> StoreResult<()> {
    let root = paths.root().join("attachments");
    match (capture_path(&root, true), expected) {
        (Ok((actual, _)), Some(expected)) if &actual == expected => Ok(()),
        (
            Err(StoreError::Io {
                kind: io::ErrorKind::NotFound,
            }),
            _,
        ) => Ok(()),
        (Err(error), _) => Err(error),
        _ => Err(StoreError::UnsafePath),
    }
}

pub(super) fn remove_root(paths: &StorePaths, expected: &Option<FileIdentity>) -> StoreResult<()> {
    verify_root(paths, expected)?;
    let root = paths.root().join("attachments");
    if path_present(&root).map_err(startup_error)? {
        collect_tree(&root)?;
    }
    sync_directory(paths.root())?;
    Ok(())
}

pub(super) fn verify_generation(paths: &StorePaths, id: &str) -> StoreResult<()> {
    if decode_id(id).is_none() {
        return Err(StoreError::InvalidData);
    }
    let root = paths.root().join("attachments");
    if !path_present(&root).map_err(startup_error)? {
        return Ok(());
    }
    verify_private_path(&root, true)?;
    let generation = root.join(id);
    if path_present(&generation).map_err(startup_error)? {
        capture_path(&generation, true)?;
    }
    Ok(())
}

pub(super) fn remove_generation(paths: &StorePaths, id: &str) -> StoreResult<()> {
    verify_generation(paths, id)?;
    let root = paths.root().join("attachments");
    let generation = root.join(id);
    if path_present(&generation).map_err(startup_error)? {
        collect_tree(&generation)?;
    }
    if path_present(&root).map_err(startup_error)? {
        sync_directory(&root)?;
    }
    Ok(())
}

fn collect_tree(path: &Path) -> StoreResult<()> {
    while !crate::store::attachment_cleanup::collect_owned_attachment_tree_step(path)?.done {}
    Ok(())
}

struct RetainedFile {
    id: String,
    account: i64,
    task: String,
    name: String,
    size: u64,
    hash: String,
}

pub(super) fn copy_retained(
    paths: &StorePaths,
    destination: &Connection,
    old_id: &str,
    new_id: &str,
    control: &LifecycleControl,
) -> LifecycleResult<()> {
    if old_id == new_id || decode_id(old_id).is_none() || decode_id(new_id).is_none() {
        return Err(LifecycleError::Generation);
    }
    // This is the unpublished replacement namespace from this exact journal.
    // A failed previous build may have left a strict subset of its files.
    remove_generation(paths, new_id)?;
    let mut cursor = String::new();
    loop {
        control.check_cancelled()?;
        let batch = {
            let mut statement = destination.prepare(
                "SELECT a.public_id,a.account_id,t.uuid,a.name,a.size,a.sha256 FROM file_attachments a \
                 JOIN tasks t ON t.id=a.task_id WHERE a.state='ready' AND a.public_id>?1 ORDER BY a.public_id LIMIT ?2")?;
            statement
                .query_map(params![cursor, ROW_BATCH], |row| {
                    Ok(RetainedFile {
                        id: row.get(0)?,
                        account: row.get(1)?,
                        task: row.get(2)?,
                        name: row.get(3)?,
                        size: u64::try_from(row.get::<_, i64>(4)?)
                            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(4, -1))?,
                        hash: row.get(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        if batch.is_empty() {
            return Ok(());
        }
        for file in batch {
            control.check_cancelled()?;
            cursor.clone_from(&file.id);
            if file.account <= 0
                || decode_id(&file.id).is_none()
                || !component(&file.task)
                || !component(&file.name)
                || file.size > 30 * 1024 * 1024
                || file.hash.len() != 64
                || !file.hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(StoreError::InvalidData.into());
            }
            let source = match file_path(paths, old_id, &file, false) {
                Ok(path) => path,
                Err(StoreError::Io {
                    kind: io::ErrorKind::NotFound,
                }) => continue,
                Err(error) => return Err(error.into()),
            };
            let (identity, size) = match capture_path(&source, false) {
                Ok(value) => value,
                Err(StoreError::Io {
                    kind: io::ErrorKind::NotFound,
                }) => continue,
                Err(error) => return Err(error.into()),
            };
            if size != file.size {
                return Err(StoreError::InvalidData.into());
            }
            let mut input = open_input(&source)?;
            if capture_path(&source, false)?.0 != identity {
                return Err(StoreError::UnsafePath.into());
            }
            let target = file_path(paths, new_id, &file, true)?;
            let partial = target.parent().ok_or(StoreError::UnsafePath)?.join(".part");
            let mut output = private_new_file(&partial)?;
            let mut hash = Sha256::new();
            let mut copied = 0_u64;
            let mut bytes = vec![0_u8; COPY_BYTES];
            loop {
                control.check_cancelled()?;
                let count = input.read(&mut bytes)?;
                if count == 0 {
                    break;
                }
                copied = copied
                    .checked_add(count as u64)
                    .ok_or(StoreError::InvalidData)?;
                if copied > file.size {
                    return Err(StoreError::InvalidData.into());
                }
                hash.update(&bytes[..count]);
                output.write_all(&bytes[..count])?;
            }
            if copied != file.size
                || format!("{:x}", hash.finalize()) != file.hash
                || capture_path(&source, false)?.0 != identity
            {
                return Err(StoreError::InvalidData.into());
            }
            keys::sync_key_file(&output)?;
            drop(output);
            if path_present(&target)? {
                return Err(StoreError::Conflict.into());
            }
            keys::replace_key_file(&partial, &target)?;
            sync_directory(target.parent().ok_or(StoreError::UnsafePath)?)?;
        }
    }
}

fn component(value: &str) -> bool {
    let mut parts = Path::new(value).components();
    matches!(parts.next(), Some(std::path::Component::Normal(_)))
        && parts.next().is_none()
        && !value.contains(['/', '\\'])
}

fn file_path(
    paths: &StorePaths,
    generation: &str,
    file: &RetainedFile,
    create: bool,
) -> StoreResult<PathBuf> {
    let mut path = paths.root().to_owned();
    verify_private_path(&path, true)?;
    for part in [
        "attachments".to_owned(),
        generation.to_owned(),
        file.account.to_string(),
        file.task.clone(),
        file.id.clone(),
    ] {
        path.push(part);
        if create {
            private_directory(&path)?;
        } else {
            verify_private_path(&path, true)?;
        }
    }
    Ok(path.join(&file.name))
}

fn open_input(path: &Path) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn generation_cleanup_unlinks_model_links_without_touching_their_targets() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(directory.path()).prepare().unwrap();
        let root = paths.root().join("attachments");
        private_directory(&root).unwrap();
        let generation = root.join(encode_id(&[7; 16]));
        private_directory(&generation).unwrap();
        let mut owned = private_new_file(&generation.join("owned-file")).unwrap();
        owned
            .write_all(b"keep until the entire tree is qualified")
            .unwrap();
        drop(owned);
        let outside = directory.path().join("outside");
        fs::write(&outside, b"never follow this link").unwrap();
        symlink(&outside, generation.join("unsafe-file")).unwrap();
        remove_generation(&paths, &encode_id(&[7; 16])).unwrap();
        assert!(!generation.exists());
        assert_eq!(fs::read(outside).unwrap(), b"never follow this link");
    }

    #[test]
    fn exact_generation_cleanup_is_repeatable_after_a_partial_delete() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(directory.path()).prepare().unwrap();
        let root = paths.root().join("attachments");
        private_directory(&root).unwrap();
        let generation = root.join(encode_id(&[7; 16]));
        private_directory(&generation).unwrap();
        let nested = generation.join("1");
        private_directory(&nested).unwrap();
        let mut owned = private_new_file(&nested.join("remaining-file")).unwrap();
        owned.write_all(b"remove after retry").unwrap();
        drop(owned);
        let other = root.join(encode_id(&[8; 16]));
        private_directory(&other).unwrap();
        remove_generation(&paths, &encode_id(&[7; 16])).unwrap();
        remove_generation(&paths, &encode_id(&[7; 16])).unwrap();
        assert!(!generation.exists());
        assert!(other.exists());
    }
}
