//! A fresh, private namespace. No previous application paths are inspected.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use super::{StoreError, StoreResult};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorePaths {
    root: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilesystemKind {
    Local,
    Network,
    Virtual,
    Unknown,
}

impl StorePaths {
    /// `profile_data_root` is the selected profile's device-local data root,
    /// already including maple-agent[-dev|-prod]. Config roots never enter here.
    pub fn new(profile_data_root: impl Into<PathBuf>) -> Self {
        Self {
            root: profile_data_root.into().join("store-v1"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db(&self) -> PathBuf {
        self.root.join("db")
    }

    pub fn database(&self) -> PathBuf {
        self.db().join("maple.db")
    }

    pub fn owner_lock(&self) -> PathBuf {
        self.root.join("owner.lock")
    }

    /// Inspect the existing authority inode without creating, truncating or
    /// repairing it. Missing or unsafe control metadata cannot prove shutdown.
    pub fn open_existing_owner_lock(&self) -> StoreResult<File> {
        verify_private_path(&self.root, true)?;
        let path = self.owner_lock();
        verify_private_path(&path, false)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            // Retain the opened name while proving authority: no delete share.
            options
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
        }
        let file = options.open(&path)?;
        verify_open_log(&file)?;
        verify_private_path(&path, false)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let opened = file.metadata()?;
            let named = fs::symlink_metadata(&path)?;
            if opened.dev() != named.dev() || opened.ino() != named.ino() {
                return Err(StoreError::UnsafePath);
            }
        }
        Ok(file)
    }

    pub fn attachments(&self) -> PathBuf {
        self.root.join("attachments")
    }

    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Open only this store's diagnostic log. Existing files are checked, never
    /// chmodded or truncated, and no bytes may be appended before descriptor
    /// ownership, permissions, file type and link count have been verified.
    pub fn open_log(&self) -> StoreResult<File> {
        verify_private_path(&self.root, true)?;
        let directory = self.logs();
        private_directory(&directory)?;
        let path = directory.join("maple-agent.log");
        let (file, created) = match open_append_file(&path, true) {
            Ok(file) => (file, true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                verify_private_path(&path, false)?;
                (open_append_file(&path, false)?, false)
            }
            Err(error) => return Err(error.into()),
        };
        verify_open_log(&file)?;
        // Recheck the path after opening as well as the actual descriptor. This
        // never repairs unsafe metadata or writes through an existing alias.
        verify_private_path(&path, false)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let opened = file.metadata()?;
            let named = fs::symlink_metadata(&path)?;
            if opened.dev() != named.dev() || opened.ino() != named.ino() {
                return Err(StoreError::UnsafePath);
            }
        }
        if created {
            sync_directory(&directory)?;
        }
        Ok(file)
    }

    pub fn generation(&self) -> PathBuf {
        self.db().join("generation.json")
    }

    pub fn keys(&self) -> PathBuf {
        self.db().join("keys.json")
    }

    pub fn setup(&self) -> PathBuf {
        self.root.join("setup.json")
    }

    /// Only generated general instruction Markdown, prompt templates and
    /// delegation skills may live here. App settings and history use SQL.
    pub fn resources(&self) -> PathBuf {
        self.root.join("resources")
    }

    /// Create a resource directory without following a link out of this
    /// store. Callers supply individual names, never arbitrary relative paths.
    pub fn prepare_resource_directory(&self, names: &[&str]) -> StoreResult<PathBuf> {
        if !self.root.is_absolute() {
            return Err(StoreError::UnsafePath);
        }
        private_directory(&self.root)?;
        let mut directory = self.resources();
        private_directory(&directory)?;
        for name in names {
            let mut components = Path::new(name).components();
            if !matches!(components.next(), Some(std::path::Component::Normal(_)))
                || components.next().is_some()
            {
                return Err(StoreError::UnsafePath);
            }
            directory.push(name);
            private_directory(&directory)?;
        }
        Ok(directory)
    }

    /// Locate existing generated resources without creating directories or
    /// following symlinks. Missing resources are an ordinary empty result.
    pub fn existing_resource_directory(&self, names: &[&str]) -> StoreResult<Option<PathBuf>> {
        if !self.root.is_absolute() {
            return Err(StoreError::UnsafePath);
        }
        verify_private_path(&self.root, true)?;
        let mut directory = self.resources();
        for name in std::iter::once(None).chain(names.iter().map(Some)) {
            if let Some(name) = name {
                let mut components = Path::new(name).components();
                if !matches!(components.next(), Some(std::path::Component::Normal(_)))
                    || components.next().is_some()
                {
                    return Err(StoreError::UnsafePath);
                }
                directory.push(name);
            }
            match verify_private_path(&directory, true) {
                Ok(()) => {}
                Err(StoreError::Io {
                    kind: std::io::ErrorKind::NotFound,
                }) => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        Ok(Some(directory))
    }

    /// Prepare only this namespace's control and diagnostic directories.
    /// Frontends may resolve IPC before acquiring ownership, including while
    /// the owner has temporarily renamed db/. Never create that directory here.
    /// Existing private components are checked, never chmodded or adopted.
    pub fn prepare_control_root(&self) -> StoreResult<Self> {
        if !self.root.is_absolute() {
            return Err(StoreError::UnsafePath);
        }
        let parent = self.root.parent().ok_or(StoreError::UnsafePath)?;
        fs::create_dir_all(parent)?;
        private_directory(&self.root)?;
        let canonical = Self {
            root: fs::canonicalize(&self.root)?,
        };
        private_directory(&canonical.logs())?;
        Ok(canonical)
    }

    /// Prepare the database directory for the owning host. The host must hold
    /// its ownership lock before opening or recovering this store; client IPC
    /// resolution uses prepare_control_root instead.
    pub fn prepare(&self) -> StoreResult<Self> {
        let canonical = self.prepare_control_root()?;
        private_directory(&canonical.db())?;
        Ok(canonical)
    }

    pub(super) fn precreate_database(&self) -> StoreResult<bool> {
        private_directory(&self.root)?;
        private_directory(&self.db())?;
        match private_new_file(&self.database()) {
            Ok(file) => {
                file.sync_all()?;
                sync_directory(&self.db())?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.verify_database()?;
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn verify_database(&self) -> StoreResult<()> {
        verify_private_path(&self.root, true)?;
        verify_private_path(&self.db(), true)?;
        verify_private_path(&self.database(), false)?;
        // Never follow a pre-existing WAL/shm symlink when SQLite opens them.
        for suffix in ["maple.db-wal", "maple.db-shm"] {
            let path = self.db().join(suffix);
            match fs::symlink_metadata(&path) {
                Ok(_) => verify_private_path(&path, false)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub fn filesystem_kind(&self) -> StoreResult<FilesystemKind> {
        filesystem_kind(&self.root)
    }
}

pub(super) fn private_directory(path: &Path) -> StoreResult<()> {
    match create_private_directory(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    verify_private_path(path, true)
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(unix)]
pub(super) fn private_new_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(unix)]
fn open_append_file(path: &Path, create_new: bool) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .append(true)
        .create_new(create_new)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
}

fn verify_open_log(file: &File) -> StoreResult<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(StoreError::UnsafePath);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o7777 != 0o600
            || metadata.nlink() != 1
        {
            return Err(StoreError::UnsafePath);
        }
    }
    #[cfg(windows)]
    windows_private::verify_open_log(file)?;
    Ok(())
}

pub(super) fn verify_private_path(path: &Path, directory: bool) -> StoreResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(StoreError::UnsafePath);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Group/world-readable directories may expose filenames; private files
        // must never be readable or writable by another OS principal.
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || (!directory && metadata.nlink() != 1)
        {
            return Err(StoreError::UnsafePath);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(StoreError::UnsafePath);
        }
        windows_private::verify_acl(path, directory)?;
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(windows)]
pub(super) fn sync_directory(_path: &Path) -> std::io::Result<()> {
    // Windows creation uses an explicit protected descriptor. Directory
    // FlushFileBuffers is not supported like Unix fsync; mode swaps use the
    // platform-specific durable rename protocol in the lifecycle module.
    Ok(())
}

#[cfg(target_os = "macos")]
fn filesystem_kind(path: &Path) -> StoreResult<FilesystemKind> {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| StoreError::UnsafePath)?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stat = unsafe { stat.assume_init() };
    let name = unsafe { CStr::from_ptr(stat.f_fstypename.as_ptr()) }.to_bytes();
    Ok(classify_unix_filesystem(name))
}

#[cfg(target_os = "macos")]
fn classify_unix_filesystem(name: &[u8]) -> FilesystemKind {
    if [b"nfs".as_slice(), b"smbfs", b"webdav"].contains(&name) {
        FilesystemKind::Network
    } else if name.windows(4).any(|part| part == b"fuse")
        || [b"devfs".as_slice(), b"procfs"].contains(&name)
    {
        FilesystemKind::Virtual
    } else if [b"apfs".as_slice(), b"hfs", b"msdos", b"exfat", b"tmpfs"].contains(&name) {
        FilesystemKind::Local
    } else {
        FilesystemKind::Unknown
    }
}

#[cfg(target_os = "linux")]
fn filesystem_kind(path: &Path) -> StoreResult<FilesystemKind> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| StoreError::UnsafePath)?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let kind = unsafe { stat.assume_init() }.f_type as u64;
    Ok(match kind {
        0x6969 | 0x517b | 0xff53_4d42 | 0x0102_1997 => FilesystemKind::Network,
        0x6573_5546 | 0x9fa0 | 0x6265_6572 => FilesystemKind::Virtual,
        // ext*, xfs, btrfs, tmpfs, overlayfs, zfs and ramfs.
        0xef53 | 0x5846_5342 | 0x9123_683e | 0x0102_1994 | 0x794c_7630 | 0x2fc1_2fc1
        | 0x8584_58f6 => FilesystemKind::Local,
        _ => FilesystemKind::Unknown,
    })
}

#[cfg(windows)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    windows_private::create_directory(path)
}

#[cfg(windows)]
pub(super) fn private_new_file(path: &Path) -> std::io::Result<File> {
    windows_private::create_file(path)
}

#[cfg(windows)]
fn open_append_file(path: &Path, create_new: bool) -> std::io::Result<File> {
    windows_private::open_append_file(path, create_new)
}

#[cfg(windows)]
fn filesystem_kind(path: &Path) -> StoreResult<FilesystemKind> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    let prefix = path.components().next().ok_or(StoreError::UnsafePath)?;
    // Joining a rooted path keeps the prefix: `C:` -> `C:\`, `\\server\share` -> `\\server\share\`.
    #[allow(clippy::join_absolute_paths)]
    let root = PathBuf::from(prefix.as_os_str()).join("\\");
    let root: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    Ok(match unsafe { GetDriveTypeW(root.as_ptr()) } {
        4 => FilesystemKind::Network,       // DRIVE_REMOTE
        2 | 3 | 6 => FilesystemKind::Local, // removable, fixed, RAM disk
        _ => FilesystemKind::Unknown,
    })
}

#[cfg(windows)]
mod windows_private {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    };
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetNamedSecurityInfoW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetSecurityDescriptorControl, GetTokenInformation, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ALL_ACCESS,
        FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileInformationByHandle, OPEN_EXISTING, READ_CONTROL,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::*;

    struct LocalAllocation(*mut c_void);
    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0) };
            }
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn user_token() -> io::Result<Vec<usize>> {
        let mut token = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut length = 0;
        unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut length) };
        let mut buffer = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        };
        let error = io::Error::last_os_error();
        unsafe { CloseHandle(token) };
        if ok == 0 { Err(error) } else { Ok(buffer) }
    }

    fn descriptor() -> io::Result<LocalAllocation> {
        let token = user_token()?;
        let user = unsafe { &*token.as_ptr().cast::<TOKEN_USER>() };
        let mut sid = null_mut();
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid_owner = LocalAllocation(sid.cast());
        let mut length = 0;
        while unsafe { *sid.add(length) } != 0 {
            length += 1;
        }
        let sid_text = String::from_utf16(unsafe { std::slice::from_raw_parts(sid, length) })
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        let sddl: Vec<u16> = format!("O:{sid_text}D:P(A;OICI;FA;;;{sid_text})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        drop(sid_owner);
        Ok(LocalAllocation(descriptor))
    }

    pub(super) fn create_directory(path: &Path) -> io::Result<()> {
        let descriptor = descriptor()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        if unsafe { CreateDirectoryW(wide(path).as_ptr(), &attributes) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn create_file(path: &Path) -> io::Result<File> {
        let descriptor = descriptor()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let handle = unsafe {
            CreateFileW(
                wide(path).as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_handle(handle) })
        }
    }

    pub(super) fn open_append_file(path: &Path, create_new: bool) -> io::Result<File> {
        let descriptor = descriptor()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let handle = unsafe {
            CreateFileW(
                wide(path).as_ptr(),
                FILE_APPEND_DATA | FILE_READ_ATTRIBUTES | READ_CONTROL,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &attributes,
                if create_new {
                    CREATE_NEW
                } else {
                    OPEN_EXISTING
                },
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_handle(handle) })
        }
    }

    pub(super) fn verify_open_log(file: &File) -> StoreResult<()> {
        let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) }
            == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let information = unsafe { information.assume_init() };
        if information.nNumberOfLinks != 1
            || information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(StoreError::UnsafePath);
        }
        let mut owner = null_mut();
        let mut acl = null_mut();
        let mut descriptor = null_mut();
        let result = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut acl,
                null_mut(),
                &mut descriptor,
            )
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32).into());
        }
        let _allocation = LocalAllocation(descriptor);
        verify_security(owner, acl, descriptor, false)
    }

    pub(super) fn verify_acl(path: &Path, directory: bool) -> StoreResult<()> {
        let mut owner = null_mut();
        let mut acl = null_mut();
        let mut descriptor = null_mut();
        let result = unsafe {
            GetNamedSecurityInfoW(
                wide(path).as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut acl,
                null_mut(),
                &mut descriptor,
            )
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32).into());
        }
        let _owner = LocalAllocation(descriptor);
        verify_security(owner, acl, descriptor, directory)
    }

    fn verify_security(
        owner: PSID,
        acl: *mut ACL,
        descriptor: PSECURITY_DESCRIPTOR,
        directory: bool,
    ) -> StoreResult<()> {
        let token = user_token()?;
        let user = unsafe { &*token.as_ptr().cast::<TOKEN_USER>() };
        let mut control = 0;
        let mut revision = 0;
        if owner.is_null()
            || acl.is_null()
            || unsafe { EqualSid(owner, user.User.Sid) } == 0
            || unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
            || (directory && control & SE_DACL_PROTECTED == 0)
            || unsafe { (*acl).AceCount } != 1
        {
            return Err(StoreError::UnsafePath);
        }
        let mut ace = null_mut();
        if unsafe { GetAce(acl, 0, &mut ace) } == 0 || ace.is_null() {
            return Err(StoreError::UnsafePath);
        }
        let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        let sid = (&ace.SidStart as *const u32).cast_mut().cast();
        // ACCESS_ALLOWED_ACE_TYPE is zero. Require exactly one allow ACE for
        // the current owner; broad inherited permissions are never adopted.
        if ace.Header.AceType != 0
            || ace.Mask & FILE_ALL_ACCESS != FILE_ALL_ACCESS
            || unsafe { EqualSid(sid, user.User.Sid) } == 0
        {
            return Err(StoreError::UnsafePath);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_root_preparation_does_not_create_the_owner_database_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(fixture.path())
            .prepare_control_root()
            .unwrap();
        assert!(paths.root().is_dir());
        assert!(paths.logs().is_dir());
        assert!(!paths.db().exists());
        assert!(!paths.database().exists());
        assert!(!paths.owner_lock().exists());
        assert_eq!(paths.prepare_control_root().unwrap(), paths);
        assert!(!paths.db().exists());
        // The separate owner-facing path can still prepare a fresh database.
        assert_eq!(paths.prepare().unwrap(), paths);
        assert!(paths.db().is_dir());
        assert!(!paths.database().exists());
    }

    #[cfg(unix)]
    #[test]
    fn control_root_preparation_preserves_unsafe_paths_instead_of_adopting_them() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let fixture = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("sentinel"), b"untouched").unwrap();
        let paths = StorePaths::new(fixture.path());
        symlink(outside.path(), paths.root()).unwrap();
        assert_eq!(paths.prepare_control_root(), Err(StoreError::UnsafePath));
        assert_eq!(
            fs::read(outside.path().join("sentinel")).unwrap(),
            b"untouched"
        );
        assert!(!outside.path().join("db").exists());
        fs::remove_file(paths.root()).unwrap();
        let paths = paths.prepare_control_root().unwrap();
        fs::set_permissions(paths.root(), fs::Permissions::from_mode(0o770)).unwrap();
        assert_eq!(paths.prepare_control_root(), Err(StoreError::UnsafePath));
        assert_eq!(
            fs::metadata(paths.root()).unwrap().permissions().mode() & 0o777,
            0o770
        );
        assert!(!paths.db().exists());
        fs::set_permissions(paths.root(), fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn existing_owner_probe_never_creates_and_preserves_the_locked_inode() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temporary.path()).prepare().unwrap();
        assert!(paths.open_existing_owner_lock().is_err());
        assert!(!paths.owner_lock().exists());
        let mut owner = private_new_file(&paths.owner_lock()).unwrap();
        std::io::Write::write_all(&mut owner, b"owner-control-sentinel").unwrap();
        owner.try_lock().unwrap();
        let probe = paths.open_existing_owner_lock().unwrap();
        assert!(matches!(
            probe.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(probe);
        drop(owner);
        let probe = paths.open_existing_owner_lock().unwrap();
        probe.try_lock().unwrap();
        drop(probe);
        assert_eq!(
            fs::read(paths.owner_lock()).unwrap(),
            b"owner-control-sentinel"
        );
        fs::hard_link(paths.owner_lock(), temporary.path().join("owner-alias")).unwrap();
        assert!(paths.open_existing_owner_lock().is_err());
        assert_eq!(
            fs::read(paths.owner_lock()).unwrap(),
            b"owner-control-sentinel"
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_owner_probe_refuses_symlinks_and_unsafe_modes_without_repair() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temporary = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temporary.path()).prepare().unwrap();
        let sentinel = temporary.path().join("unrelated-control");
        fs::write(&sentinel, b"unchanged").unwrap();
        symlink(&sentinel, paths.owner_lock()).unwrap();
        assert!(paths.open_existing_owner_lock().is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
        fs::remove_file(paths.owner_lock()).unwrap();
        drop(private_new_file(&paths.owner_lock()).unwrap());
        fs::set_permissions(paths.owner_lock(), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(paths.open_existing_owner_lock().is_err());
        assert_eq!(
            fs::metadata(paths.owner_lock())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }

    #[test]
    fn diagnostic_log_appends_privately_and_rejects_hardlinks_before_writing() {
        use std::io::Write;
        let temporary = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temporary.path()).prepare().unwrap();
        let log = paths.logs().join("maple-agent.log");
        paths.open_log().unwrap().write_all(b"first\n").unwrap();
        paths.open_log().unwrap().write_all(b"second\n").unwrap();
        assert_eq!(fs::read(&log).unwrap(), b"first\nsecond\n");
        let alias = temporary.path().join("previous-log-sentinel");
        fs::hard_link(&log, &alias).unwrap();
        assert!(matches!(paths.open_log(), Err(StoreError::UnsafePath)));
        assert_eq!(fs::read(&alias).unwrap(), b"first\nsecond\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&log).unwrap().permissions().mode() & 0o7777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn diagnostic_log_rejects_symlinks_and_broad_modes_without_mutation() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temporary = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temporary.path()).prepare().unwrap();
        let sentinel = temporary.path().join("previous-log-sentinel");
        fs::write(&sentinel, b"untouched").unwrap();
        let log = paths.logs().join("maple-agent.log");
        symlink(&sentinel, &log).unwrap();
        assert!(matches!(paths.open_log(), Err(StoreError::UnsafePath)));
        assert_eq!(fs::read(&sentinel).unwrap(), b"untouched");
        fs::remove_file(&log).unwrap();
        paths.open_log().unwrap();
        fs::set_permissions(&log, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(paths.open_log(), Err(StoreError::UnsafePath)));
        assert_eq!(
            fs::metadata(&log).unwrap().permissions().mode() & 0o7777,
            0o644
        );
        assert!(fs::read(&log).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn generated_resources_refuse_links_to_previous_data() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let prior = root.path().join("previous");
        fs::create_dir(&prior).unwrap();
        fs::write(prior.join("sentinel"), b"unchanged").unwrap();
        let paths = StorePaths::new(root.path().join("profile"))
            .prepare()
            .unwrap();
        assert!(
            paths
                .existing_resource_directory(&["accounts"])
                .unwrap()
                .is_none()
        );
        assert!(
            !paths.resources().exists(),
            "a lookup must not create resources"
        );
        symlink(&prior, paths.resources()).unwrap();
        assert!(paths.existing_resource_directory(&["accounts"]).is_err());
        assert!(paths.prepare_resource_directory(&["accounts"]).is_err());
        assert!(!prior.join("accounts").exists());
        assert_eq!(fs::read(prior.join("sentinel")).unwrap(), b"unchanged");
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_previous_subdirectories_do_not_block_new_store_paths() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        fs::create_dir(&profile).unwrap();
        for name in ["store", "agent", "acp", "logs", "remote"] {
            let previous = profile.join(name);
            fs::create_dir(&previous).unwrap();
            fs::write(previous.join("sentinel"), b"not consulted").unwrap();
            fs::set_permissions(&previous, fs::Permissions::from_mode(0o0)).unwrap();
        }
        let paths = StorePaths::new(&profile).prepare().unwrap();
        paths.precreate_database().unwrap();
        assert!(paths.database().is_file());
        assert!(!paths.root().join("staging").exists());
        for name in ["store", "agent", "acp", "logs", "remote"] {
            let previous = profile.join(name);
            assert_eq!(
                fs::metadata(&previous).unwrap().permissions().mode() & 0o777,
                0
            );
            fs::set_permissions(&previous, fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(
                fs::read(previous.join("sentinel")).unwrap(),
                b"not consulted"
            );
        }
    }

    #[test]
    fn creates_only_a_fresh_namespace_and_secure_file_before_sqlite() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("store")).unwrap();
        fs::write(temp.path().join("store/old-data"), b"untouched").unwrap();
        let paths = StorePaths::new(temp.path()).prepare().unwrap();
        assert!(paths.precreate_database().unwrap());
        assert!(!paths.precreate_database().unwrap());
        assert_eq!(
            fs::read(temp.path().join("store/old-data")).unwrap(),
            b"untouched"
        );
        assert_eq!(fs::metadata(paths.database()).unwrap().len(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(paths.root()).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(paths.db()).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(paths.database()).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinks_broad_permissions_and_hardlinks_without_fixing_them() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        symlink(other.path(), temp.path().join("store-v1")).unwrap();
        assert_eq!(
            StorePaths::new(temp.path()).prepare(),
            Err(StoreError::UnsafePath)
        );
        fs::remove_file(temp.path().join("store-v1")).unwrap();
        let paths = StorePaths::new(temp.path()).prepare().unwrap();
        paths.precreate_database().unwrap();
        fs::set_permissions(paths.db(), fs::Permissions::from_mode(0o770)).unwrap();
        assert_eq!(paths.verify_database(), Err(StoreError::UnsafePath));
        assert_eq!(
            fs::metadata(paths.db()).unwrap().permissions().mode() & 0o777,
            0o770
        );
        fs::set_permissions(paths.db(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::hard_link(paths.database(), temp.path().join("alias")).unwrap();
        assert_eq!(paths.verify_database(), Err(StoreError::UnsafePath));
    }

    #[cfg(unix)]
    #[test]
    fn refuses_database_and_sidecar_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path()).prepare().unwrap();
        let target = temp.path().join("target");
        fs::write(&target, b"do not touch").unwrap();
        symlink(&target, paths.database()).unwrap();
        assert_eq!(paths.precreate_database(), Err(StoreError::UnsafePath));
        fs::remove_file(paths.database()).unwrap();
        paths.precreate_database().unwrap();
        symlink(&target, paths.db().join("maple.db-wal")).unwrap();
        assert_eq!(paths.verify_database(), Err(StoreError::UnsafePath));
        assert_eq!(fs::read(target).unwrap(), b"do not touch");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn network_and_userspace_filesystems_are_classified() {
        assert_eq!(classify_unix_filesystem(b"nfs"), FilesystemKind::Network);
        assert_eq!(classify_unix_filesystem(b"smbfs"), FilesystemKind::Network);
        assert_eq!(
            classify_unix_filesystem(b"osxfuse"),
            FilesystemKind::Virtual
        );
        assert_eq!(classify_unix_filesystem(b"apfs"), FilesystemKind::Local);
    }
}
