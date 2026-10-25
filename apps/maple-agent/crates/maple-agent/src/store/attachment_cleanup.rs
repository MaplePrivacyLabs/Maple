//! Bounded cleanup of an attachment directory whose owner has authorized deletion.
//!
//! Model-created adjacent files and extracted directories are part of that owned
//! directory too. Children need not have Maple's private creation permissions.
//! Symlinks are unlinked as entries and never traversed. Nested directories move
//! one level toward the root under generated names so each bounded step makes
//! durable progress without recursion or a persistent in-memory traversal cursor.

use std::{io, path::Path};

use super::{StoreError, StoreResult};

const ENTRY_CAP: usize = 128;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct AttachmentCleanupProgress {
    pub done: bool,
    pub entries: usize,
}

pub(super) fn collect_owned_attachment_tree_step(
    root: &Path,
) -> StoreResult<AttachmentCleanupProgress> {
    implementation::step(root)
}

#[cfg(unix)]
mod implementation {
    use super::*;
    use std::{
        ffi::{CStr, CString},
        fs::File,
        os::{
            fd::{AsRawFd, FromRawFd, RawFd},
            unix::ffi::OsStrExt,
        },
        path::Component,
    };

    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }

    fn entries(file: &File) -> StoreResult<Directory> {
        let descriptor = unsafe { libc::dup(file.as_raw_fd()) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let directory = unsafe { libc::fdopendir(descriptor) };
        if directory.is_null() {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(descriptor);
            }
            return Err(error.into());
        }
        Ok(Directory(directory))
    }

    fn next(directory: &Directory) -> StoreResult<Option<CString>> {
        loop {
            unsafe {
                *errno() = 0;
            }
            let entry = unsafe { libc::readdir(directory.0) };
            if entry.is_null() {
                let code = unsafe { *errno() };
                return if code == 0 {
                    Ok(None)
                } else {
                    Err(io::Error::from_raw_os_error(code).into())
                };
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            return Ok(Some(name.to_owned()));
        }
    }

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    unsafe fn errno() -> *mut libc::c_int {
        unsafe { libc::__error() }
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
    unsafe fn errno() -> *mut libc::c_int {
        unsafe { libc::__errno_location() }
    }

    fn open_at(parent: RawFd, name: &CStr) -> StoreResult<File> {
        let fd = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn metadata_at(parent: RawFd, name: &CStr) -> StoreResult<libc::stat> {
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                parent,
                name.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(unsafe { metadata.assume_init() })
    }

    fn same_directory(parent: RawFd, name: &CStr, file: &File) -> StoreResult<()> {
        let named = metadata_at(parent, name)?;
        let mut opened = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(file.as_raw_fd(), opened.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let opened = unsafe { opened.assume_init() };
        if named.st_dev != opened.st_dev
            || named.st_ino != opened.st_ino
            || named.st_mode & libc::S_IFMT != libc::S_IFDIR
        {
            return Err(StoreError::UnsafePath);
        }
        Ok(())
    }

    fn unlink(parent: RawFd, name: &CStr, directory: bool) -> StoreResult<()> {
        if unsafe {
            libc::unlinkat(
                parent,
                name.as_ptr(),
                if directory { libc::AT_REMOVEDIR } else { 0 },
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn parent_and_name(root: &Path) -> StoreResult<(File, CString)> {
        if !root.is_absolute() {
            return Err(StoreError::UnsafePath);
        }
        let mut components = root.components();
        if !matches!(components.next(), Some(Component::RootDir)) {
            return Err(StoreError::UnsafePath);
        }
        let mut names = components
            .map(|part| match part {
                Component::Normal(name) => {
                    CString::new(name.as_bytes()).map_err(|_| StoreError::UnsafePath)
                }
                _ => Err(StoreError::UnsafePath),
            })
            .collect::<StoreResult<Vec<_>>>()?;
        let name = names.pop().ok_or(StoreError::UnsafePath)?;
        let mut parent = File::open("/")?;
        for name in names {
            parent = open_at(parent.as_raw_fd(), &name)?;
        }
        Ok((parent, name))
    }

    pub(super) fn step(root: &Path) -> StoreResult<AttachmentCleanupProgress> {
        let (parent, name) = match parent_and_name(root) {
            Ok(value) => value,
            Err(StoreError::Io {
                kind: io::ErrorKind::NotFound,
            }) => {
                return Ok(AttachmentCleanupProgress {
                    done: true,
                    entries: 0,
                });
            }
            Err(error) => return Err(error),
        };
        let root = match open_at(parent.as_raw_fd(), &name) {
            Ok(value) => value,
            Err(StoreError::Io {
                kind: io::ErrorKind::NotFound,
            }) => {
                parent.sync_all()?;
                return Ok(AttachmentCleanupProgress {
                    done: true,
                    entries: 0,
                });
            }
            Err(error) => return Err(error),
        };
        same_directory(parent.as_raw_fd(), &name, &root)?;
        let meta = metadata_at(parent.as_raw_fd(), &name)?;
        if meta.st_uid != unsafe { libc::geteuid() } || meta.st_mode & 0o077 != 0 {
            return Err(StoreError::UnsafePath);
        }
        let mut progress = AttachmentCleanupProgress::default();
        let names = entries(&root)?;
        while progress.entries < ENTRY_CAP {
            let Some(child_name) = next(&names)? else {
                break;
            };
            same_directory(parent.as_raw_fd(), &name, &root)?;
            let child_meta = metadata_at(root.as_raw_fd(), &child_name)?;
            if child_meta.st_mode & libc::S_IFMT == libc::S_IFDIR {
                let child = open_at(root.as_raw_fd(), &child_name)?;
                same_directory(root.as_raw_fd(), &child_name, &child)?;
                let children = entries(&child)?;
                while progress.entries < ENTRY_CAP {
                    let Some(nested) = next(&children)? else {
                        break;
                    };
                    same_directory(parent.as_raw_fd(), &name, &root)?;
                    same_directory(root.as_raw_fd(), &child_name, &child)?;
                    let nested_meta = metadata_at(child.as_raw_fd(), &nested)?;
                    if nested_meta.st_mode & libc::S_IFMT == libc::S_IFDIR {
                        let generated =
                            CString::new(format!(".cleanup-{:032x}", rand::random::<u128>()))
                                .map_err(|_| StoreError::InvalidData)?;
                        match metadata_at(root.as_raw_fd(), &generated) {
                            Err(StoreError::Io {
                                kind: io::ErrorKind::NotFound,
                            }) => {}
                            Ok(_) => return Err(StoreError::Conflict),
                            Err(error) => return Err(error),
                        }
                        if unsafe {
                            libc::renameat(
                                child.as_raw_fd(),
                                nested.as_ptr(),
                                root.as_raw_fd(),
                                generated.as_ptr(),
                            )
                        } != 0
                        {
                            return Err(io::Error::last_os_error().into());
                        }
                    } else {
                        unlink(child.as_raw_fd(), &nested, false)?;
                    }
                    progress.entries += 1;
                }
                child.sync_all()?;
                if progress.entries < ENTRY_CAP {
                    same_directory(root.as_raw_fd(), &child_name, &child)?;
                    match unlink(root.as_raw_fd(), &child_name, true) {
                        Ok(()) => progress.entries += 1,
                        Err(StoreError::Io {
                            kind: io::ErrorKind::DirectoryNotEmpty,
                        }) => {}
                        Err(error) => return Err(error),
                    }
                }
            } else {
                unlink(root.as_raw_fd(), &child_name, false)?;
                progress.entries += 1;
            }
        }
        root.sync_all()?;
        if progress.entries < ENTRY_CAP {
            same_directory(parent.as_raw_fd(), &name, &root)?;
            match unlink(parent.as_raw_fd(), &name, true) {
                Ok(()) => {
                    progress.entries += 1;
                    progress.done = true;
                }
                Err(StoreError::Io {
                    kind: io::ErrorKind::DirectoryNotEmpty,
                }) => {}
                Err(error) => return Err(error),
            }
        }
        parent.sync_all()?;
        Ok(progress)
    }
}

#[cfg(windows)]
mod implementation {
    use super::*;
    use crate::store::paths::{sync_directory, verify_private_path};
    use std::{
        fs::{self, File},
        os::windows::{
            ffi::OsStrExt,
            fs::MetadataExt,
            io::{AsRawHandle, FromRawHandle},
        },
        path::PathBuf,
    };
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
            OPEN_EXISTING,
        },
    };

    /// A no-reparse directory handle without DELETE sharing keeps each ancestor
    /// and the currently enumerated child from being renamed under path-based
    /// std filesystem calls. Descendants may have ordinary inherited ACLs.
    fn hold_directory(path: &Path) -> StoreResult<File> {
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_handle(handle) };
        identity(&file)?;
        Ok(file)
    }

    fn identity(file: &File) -> StoreResult<(u32, u64)> {
        let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let info = unsafe { info.assume_init() };
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        {
            return Err(StoreError::UnsafePath);
        }
        Ok((
            info.dwVolumeSerialNumber,
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ))
    }

    // Rust's FileType::is_dir excludes directory symlinks/junctions. Windows
    // still requires RemoveDirectory for those reparse entries; it removes the
    // link itself. Attribute classification never follows the target.
    fn directory_attribute(attributes: u32) -> bool {
        attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    fn hold_ancestors(root: &Path) -> StoreResult<Vec<File>> {
        if !root.is_absolute() {
            return Err(StoreError::UnsafePath);
        }
        let mut paths = root
            .ancestors()
            .skip(1)
            .filter(|path| !path.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .collect::<Vec<PathBuf>>();
        paths.reverse();
        paths.iter().map(|path| hold_directory(path)).collect()
    }

    pub(super) fn step(root: &Path) -> StoreResult<AttachmentCleanupProgress> {
        let _ancestors = match hold_ancestors(root) {
            Ok(value) => value,
            Err(StoreError::Io {
                kind: io::ErrorKind::NotFound,
            }) => {
                return Ok(AttachmentCleanupProgress {
                    done: true,
                    entries: 0,
                });
            }
            Err(error) => return Err(error),
        };
        match verify_private_path(root, true) {
            Ok(()) => {}
            Err(StoreError::Io {
                kind: io::ErrorKind::NotFound,
            }) => {
                return Ok(AttachmentCleanupProgress {
                    done: true,
                    entries: 0,
                });
            }
            Err(error) => return Err(error),
        }
        let guard = hold_directory(root)?;
        let root_id = identity(&guard)?;
        let mut progress = AttachmentCleanupProgress::default();
        for child in fs::read_dir(root)? {
            if progress.entries >= ENTRY_CAP {
                break;
            }
            let child = child?;
            let metadata = fs::symlink_metadata(child.path())?;
            let reparse = metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
            if metadata.is_dir() && !reparse {
                let child_guard = hold_directory(&child.path())?;
                for nested in fs::read_dir(child.path())? {
                    if progress.entries >= ENTRY_CAP {
                        break;
                    }
                    let nested = nested?;
                    let info = fs::symlink_metadata(nested.path())?;
                    if info.is_dir() && info.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                        let target = root.join(format!(".cleanup-{:032x}", rand::random::<u128>()));
                        match fs::symlink_metadata(&target) {
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                            Ok(_) => return Err(StoreError::Conflict),
                            Err(error) => return Err(error.into()),
                        }
                        fs::rename(nested.path(), target)?;
                    } else if directory_attribute(info.file_attributes()) {
                        fs::remove_dir(nested.path())?;
                    } else {
                        fs::remove_file(nested.path())?;
                    }
                    progress.entries += 1;
                }
                let child_id = identity(&child_guard)?;
                drop(child_guard);
                if progress.entries < ENTRY_CAP {
                    let verified = hold_directory(&child.path())?;
                    if identity(&verified)? != child_id {
                        return Err(StoreError::UnsafePath);
                    }
                    drop(verified);
                    match fs::remove_dir(child.path()) {
                        Ok(()) => progress.entries += 1,
                        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {}
                        Err(error) => return Err(error.into()),
                    }
                }
            } else if directory_attribute(metadata.file_attributes()) {
                fs::remove_dir(child.path())?;
                progress.entries += 1;
            } else {
                fs::remove_file(child.path())?;
                progress.entries += 1;
            }
        }
        if identity(&guard)? != root_id {
            return Err(StoreError::UnsafePath);
        }
        drop(guard);
        if progress.entries < ENTRY_CAP {
            let verified = hold_directory(root)?;
            if identity(&verified)? != root_id {
                return Err(StoreError::UnsafePath);
            }
            drop(verified);
            match fs::remove_dir(root) {
                Ok(()) => {
                    progress.entries += 1;
                    progress.done = true;
                }
                Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {}
                Err(error) => return Err(error.into()),
            }
        }
        if let Some(parent) = root.parent() {
            sync_directory(parent)?;
        }
        Ok(progress)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn directory_reparse_entries_select_directory_unlink() {
            assert!(directory_attribute(FILE_ATTRIBUTE_DIRECTORY));
            assert!(directory_attribute(
                FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT
            ));
            assert!(!directory_attribute(FILE_ATTRIBUTE_REPARSE_POINT));
            assert!(!directory_attribute(0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::paths::private_directory;
    use std::fs;

    #[test]
    fn removes_adjacent_outputs_and_nested_extractions_in_bounded_steps() {
        let directory = tempfile::tempdir().unwrap();
        let directory_path = directory.path().canonicalize().unwrap();
        let root = directory_path.join("owned");
        private_directory(&root).unwrap();
        fs::write(root.join(".DS_Store"), b"model adjacent metadata").unwrap();
        let nested = root.join("extracted");
        fs::create_dir(&nested).unwrap();
        let deeper = nested.join("nested");
        fs::create_dir(&deeper).unwrap();
        for index in 0..300 {
            fs::write(deeper.join(format!("file-{index}")), b"ordinary user file").unwrap();
        }
        let mut calls = 0;
        loop {
            let progress = collect_owned_attachment_tree_step(&root).unwrap();
            assert!(progress.entries <= ENTRY_CAP);
            calls += 1;
            if progress.done {
                break;
            }
            assert!(
                progress.entries > 0,
                "each unfinished step must make progress"
            );
            assert!(calls < 20, "cleanup must remain restartable and bounded");
        }
        assert!(calls > 1);
        assert!(!root.exists());
        assert!(collect_owned_attachment_tree_step(&root).unwrap().done);
    }

    #[cfg(unix)]
    #[test]
    fn deeply_nested_extractions_make_restartable_progress_without_recursion() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("owned");
        private_directory(&root).unwrap();
        let mut nested = root.clone();
        for _ in 0..96 {
            nested = nested.join("d");
            fs::create_dir(&nested).unwrap();
        }
        fs::write(nested.join("payload"), b"deep model output").unwrap();
        for _ in 0..100 {
            let progress = collect_owned_attachment_tree_step(&root).unwrap();
            assert!(progress.entries <= ENTRY_CAP);
            if progress.done {
                assert!(!root.exists());
                return;
            }
            assert!(progress.entries > 0);
        }
        panic!("bounded cleanup failed to finish a deep tree");
    }

    #[cfg(unix)]
    #[test]
    fn unlinks_nested_symlinks_without_following_file_or_directory_targets() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let directory_path = directory.path().canonicalize().unwrap();
        let root = directory_path.join("owned");
        private_directory(&root).unwrap();
        let nested = root.join("extracted");
        fs::create_dir(&nested).unwrap();
        let outside = directory_path.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), b"never delete external content").unwrap();
        symlink(&outside, nested.join("directory-link")).unwrap();
        symlink(outside.join("sentinel"), root.join("file-link")).unwrap();
        while !collect_owned_attachment_tree_step(&root).unwrap().done {}
        assert_eq!(
            fs::read(outside.join("sentinel")).unwrap(),
            b"never delete external content"
        );
    }

    #[cfg(unix)]
    #[test]
    fn root_and_ancestor_symlinks_never_authorize_cleanup() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let directory_path = directory.path().canonicalize().unwrap();
        let outside = directory_path.join("outside");
        private_directory(&outside).unwrap();
        let root = outside.join("owned");
        private_directory(&root).unwrap();
        fs::write(root.join("sentinel"), b"leave this path intact").unwrap();
        let alias = directory_path.join("alias");
        symlink(&outside, &alias).unwrap();
        assert!(collect_owned_attachment_tree_step(&alias.join("owned")).is_err());
        let root_alias = directory_path.join("root-alias");
        symlink(&root, &root_alias).unwrap();
        assert!(collect_owned_attachment_tree_step(&root_alias).is_err());
        assert_eq!(
            fs::read(root.join("sentinel")).unwrap(),
            b"leave this path intact"
        );
    }
}
