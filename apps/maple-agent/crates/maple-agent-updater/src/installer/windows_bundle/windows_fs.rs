//! Handle-checked, per-user Windows storage. No path is followed through a
//! junction/reparse point and existing ACLs are never repaired by taking over
//! a directory. Pointer publication uses a same-directory write-through move.

use std::{
    ffi::{OsStr, c_void},
    fs::{self, File},
    io::{self, Read},
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr::{null, null_mut},
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GENERIC_ALL, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
        LocalFree,
    },
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
        },
        CreateWellKnownSid, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
        GetTokenInformation, INHERIT_ONLY_ACE, IsValidAcl, IsValidSid, OWNER_SECURITY_INFORMATION,
        PSID, SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, TOKEN_QUERY, TOKEN_USER, TokenUser,
        WinBuiltinAdministratorsSid, WinCreatorOwnerRightsSid, WinLocalSystemSid,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, DELETE, FILE_APPEND_DATA,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
        FILE_WRITE_DATA, FILE_WRITE_EA, GetFileInformationByHandle, MOVEFILE_REPLACE_EXISTING,
        MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_ALWAYS, OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
        WRITE_OWNER,
    },
    System::{
        SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE},
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

const WRITE_RIGHTS: u32 = GENERIC_ALL
    | GENERIC_WRITE
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_WRITE_ATTRIBUTES
    | FILE_DELETE_CHILD;

fn invalid() -> io::Error {
    invalid_at("unspecified-guard")
}

// Fixed operation labels make native qualification failures actionable without
// exposing filesystem paths, user names, SIDs, ACL contents or other state.
fn invalid_at(stage: &'static str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("unsafe Windows updater storage ({stage})"),
    )
}

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut result: Vec<u16> = value.encode_wide().collect();
    if result.contains(&0) || result.len() > 32_000 {
        return Err(invalid_at("path-encoding-or-length"));
    }
    result.push(0);
    Ok(result)
}

/// Only absolute local drive paths are accepted. Verbatim disk paths produced
/// by canonicalize are accepted; device paths, UNC paths and ADS are refused.
fn checked_components(path: &Path) -> io::Result<Vec<PathBuf>> {
    let mut components = path.components();
    let prefix = components.next().ok_or_else(invalid)?;
    if !matches!(prefix, Component::Prefix(p) if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
        || !matches!(components.next(), Some(Component::RootDir))
    {
        return Err(invalid_at("nonlocal-or-relative-path"));
    }
    let mut current = PathBuf::from(prefix.as_os_str());
    current.push("\\");
    let mut paths = vec![current.clone()];
    for part in components {
        let Component::Normal(name) = part else {
            return Err(invalid_at("nonnormal-path-component"));
        };
        let name = name.to_str().ok_or_else(invalid)?;
        // Windows aliases (trailing space/dot), ADS and reserved device names
        // must not name different objects after a Win32 operation normalizes.
        let stem = name
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if name.is_empty()
            || name.ends_with('.')
            || name.ends_with(' ')
            || name
                .chars()
                .any(|c| c.is_control() || matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*'))
            || matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            )
            || ["COM", "LPT"].iter().any(|prefix| {
                stem.strip_prefix(prefix).is_some_and(|suffix| {
                    let mut chars = suffix.chars();
                    chars
                        .next()
                        .is_some_and(|c| c.is_ascii_digit() || matches!(c, '¹' | '²' | '³'))
                        && chars.next().is_none()
                })
            })
        {
            return Err(invalid_at("ambiguous-path-component"));
        }
        current.push(name);
        paths.push(current.clone());
    }
    Ok(paths)
}

fn open_raw(
    path: &Path,
    access: u32,
    disposition: u32,
    security: *const SECURITY_ATTRIBUTES,
) -> io::Result<File> {
    open_raw_shared(
        path,
        access,
        disposition,
        security,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
    )
}

fn open_raw_shared(
    path: &Path,
    access: u32,
    disposition: u32,
    security: *const SECURITY_ATTRIBUTES,
    sharing: u32,
) -> io::Result<File> {
    let name = wide(path.as_os_str())?;
    // SAFETY: name and optional security descriptor outlive CreateFileW. The
    // returned noninheritable handle is transferred exactly once into File.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            sharing,
            security,
            disposition,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle.cast()) })
}

fn validate_path_binding(path: &Path, file: &File) -> io::Result<()> {
    let first = information(file)?;
    let current = open_raw(path, FILE_READ_ATTRIBUTES, OPEN_EXISTING, null())?;
    let second = information(&current)?;
    if (
        first.dwVolumeSerialNumber,
        first.nFileIndexHigh,
        first.nFileIndexLow,
    ) != (
        second.dwVolumeSerialNumber,
        second.nFileIndexHigh,
        second.nFileIndexLow,
    ) {
        return Err(invalid_at("opened-path-identity-mismatch"));
    }
    Ok(())
}

fn information(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid_at("reparse-point"));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 && info.nNumberOfLinks != 1 {
        return Err(invalid_at("file-hardlink-count"));
    }
    Ok(info)
}

/// Retaining ancestor handles without FILE_SHARE_DELETE fences rename/removal
/// of those exact components while the privileged open/create is performed.
fn ancestors(path: &Path, include_leaf: bool) -> io::Result<Vec<File>> {
    let paths = checked_components(path)?;
    let count = paths.len().saturating_sub(usize::from(!include_leaf));
    let mut guards = Vec::with_capacity(count);
    for path in paths.iter().take(count) {
        let file = open_raw(path, FILE_READ_ATTRIBUTES, OPEN_EXISTING, null())?;
        let info = information(&file)?;
        if (path != paths.last().unwrap() || !include_leaf)
            && info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        {
            return Err(invalid_at("ancestor-not-directory"));
        }
        guards.push(file);
    }
    Ok(guards)
}

pub(super) fn validate_path_syntax(path: &Path) -> io::Result<()> {
    checked_components(path).map(|_| ())
}

pub(super) fn validate_path(path: &Path) -> io::Result<()> {
    ancestors(path, true).map(|_| ())
}

struct Token(HANDLE);
impl Drop for Token {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
}

struct CurrentUser {
    storage: Vec<usize>,
}
impl CurrentUser {
    fn new() -> io::Result<Self> {
        let mut token = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Token(token);
        let mut length = 0;
        unsafe {
            GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut length);
        }
        if length < size_of::<TOKEN_USER>() as u32 || length > 4096 {
            return Err(invalid());
        }
        let mut storage = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                storage.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { storage })
    }
    fn sid(&self) -> PSID {
        // SAFETY: GetTokenInformation initialized an aligned TOKEN_USER and its
        // referenced SID in storage, which stays allocated for this borrow.
        unsafe { (*(self.storage.as_ptr().cast::<TOKEN_USER>())).User.Sid }
    }
    fn string(&self) -> io::Result<String> {
        let mut value = null_mut();
        if unsafe { ConvertSidToStringSidW(self.sid(), &mut value) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let allocation = Local(value.cast());
        let mut length = 0;
        while length < 256 && unsafe { *value.add(length) } != 0 {
            length += 1;
        }
        if length == 256 {
            return Err(invalid());
        }
        let result = String::from_utf16(unsafe { std::slice::from_raw_parts(value, length) })
            .map_err(|_| invalid())?;
        drop(allocation);
        Ok(result)
    }
}

fn known_sid(kind: i32) -> io::Result<Vec<u32>> {
    let mut data = vec![0u32; (SECURITY_MAX_SID_SIZE as usize).div_ceil(4)];
    let mut length = SECURITY_MAX_SID_SIZE;
    if unsafe { CreateWellKnownSid(kind, null_mut(), data.as_mut_ptr().cast(), &mut length) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(data)
}

fn same_sid(a: PSID, b: PSID) -> bool {
    !a.is_null() && !b.is_null() && unsafe { EqualSid(a, b) } != 0
}

fn trusted_storage_sid(sid: PSID, user: PSID, system: PSID, admins: PSID) -> bool {
    same_sid(sid, user) || same_sid(sid, system) || same_sid(sid, admins)
}

fn validate_private_handle(file: &File) -> io::Result<()> {
    let info = information(file)?;
    // FILE_DELETE_CHILD is meaningful only for a directory. NSIS can leave
    // that bit in an inherited read ACE on a regular file; it grants no file
    // mutation authority. Classify the same opened handle, retaining every
    // other mutating right and the full directory mask.
    let write_rights = if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        WRITE_RIGHTS
    } else {
        WRITE_RIGHTS & !FILE_DELETE_CHILD
    };
    let user = CurrentUser::new()?;
    let system = known_sid(WinLocalSystemSid)?;
    let admins = known_sid(WinBuiltinAdministratorsSid)?;
    let mut owner = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor = null_mut();
    let code = unsafe {
        GetSecurityInfo(
            file.as_raw_handle().cast(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    let _allocation = Local(descriptor);
    // TokenOwner may be Administrators for an elevated process even when
    // TokenUser is the signed-in user. Owners have implicit WRITE_DAC, so the
    // owner must belong to the exact same authority set as allowed writers.
    // Explicit updater-created objects continue to specify O:<current user>.
    if !trusted_storage_sid(
        owner,
        user.sid(),
        system.as_ptr().cast_mut().cast(),
        admins.as_ptr().cast_mut().cast(),
    ) {
        return Err(invalid_at("owner-outside-trusted-set"));
    }
    // OWNER RIGHTS (S-1-3-4) grants rights only to this object's current owner.
    // Resolve its authority only after the actual handle owner passed the
    // fixed owner allowlist above; it is never itself an allowed owner SID.
    let owner_rights = known_sid(WinCreatorOwnerRightsSid)?;
    if dacl.is_null() {
        return Err(invalid_at("null-dacl"));
    }
    if unsafe { IsValidAcl(dacl) } == 0 {
        return Err(invalid_at("invalid-dacl"));
    }
    let mut size = ACL_SIZE_INFORMATION::default();
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
        || size.AceCount > 1024
    {
        return Err(invalid_at("dacl-information-or-count"));
    }
    for index in 0..size.AceCount {
        let mut raw = null_mut();
        if unsafe { GetAce(dacl, index, &mut raw) } == 0 || raw.is_null() {
            return Err(invalid_at("dacl-ace-read"));
        }
        let header = unsafe { &*raw.cast::<ACE_HEADER>() };
        if header.AceFlags as u32 & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        if header.AceType as u32 == ACCESS_DENIED_ACE_TYPE {
            continue;
        }
        if header.AceType as u32 != ACCESS_ALLOWED_ACE_TYPE {
            return Err(invalid_at("unsupported-effective-ace-type"));
        }
        if (header.AceSize as usize) < size_of::<ACCESS_ALLOWED_ACE>() + 4 {
            return Err(invalid_at("short-allowed-ace"));
        }
        let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
        let sid = (&ace.SidStart as *const u32).cast_mut().cast::<c_void>();
        // Bound the SID inside its ACE before asking Windows to inspect it.
        let subauthorities = unsafe { *(sid.cast::<u8>().add(1)) } as usize;
        if subauthorities > 15
            || 8 + 8 + subauthorities * 4 > header.AceSize as usize
            || unsafe { IsValidSid(sid) } == 0
        {
            return Err(invalid_at("ace-sid-bounds-or-validity"));
        }
        if ace.Mask & write_rights != 0
            && !trusted_storage_sid(
                sid,
                user.sid(),
                system.as_ptr().cast_mut().cast(),
                admins.as_ptr().cast_mut().cast(),
            )
            && !same_sid(sid, owner_rights.as_ptr().cast_mut().cast())
        {
            return Err(invalid_at("foreign-write-ace"));
        }
    }
    Ok(())
}

fn private_security() -> io::Result<Local> {
    let sid = CurrentUser::new()?.string()?;
    let sddl = wide(OsStr::new(&format!(
        "O:{sid}D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{sid})"
    )))?;
    let mut descriptor = null_mut();
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
    Ok(Local(descriptor))
}

fn security_attributes(descriptor: &Local) -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    }
}

pub(super) fn private_directory(path: &Path) -> io::Result<()> {
    checked_components(path)?;
    if fs::symlink_metadata(path).is_err_and(|e| e.kind() == io::ErrorKind::NotFound) {
        let parent = path.parent().ok_or_else(invalid)?;
        if !parent.exists() {
            private_directory(parent)?;
        }
        let _parents = ancestors(path, false)?;
        let descriptor = private_security()?;
        let attributes = security_attributes(&descriptor);
        let name = wide(path.as_os_str())?;
        if unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } == 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
    }
    let _parents = ancestors(path, false)?;
    let directory = open_raw(
        path,
        READ_CONTROL | FILE_READ_ATTRIBUTES,
        OPEN_EXISTING,
        null(),
    )?;
    if information(&directory)?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(invalid());
    }
    validate_private_handle(&directory)?;
    validate_path_binding(path, &directory)
}

pub(super) fn open_private(path: &Path, truncate: bool) -> io::Result<File> {
    let _parents = ancestors(path, false)?;
    let descriptor = private_security()?;
    let attributes = security_attributes(&descriptor);
    let file = open_raw(
        path,
        GENERIC_READ | GENERIC_WRITE | READ_CONTROL,
        OPEN_ALWAYS,
        &attributes,
    )?;
    if information(&file)?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(invalid());
    }
    validate_private_handle(&file)?;
    validate_path_binding(path, &file)?;
    if truncate {
        file.set_len(0)?;
    }
    Ok(file)
}

pub(super) fn open_readonly(path: &Path) -> io::Result<File> {
    let _parents = ancestors(path, false)?;
    let file = open_raw_shared(
        path,
        GENERIC_READ | READ_CONTROL,
        OPEN_EXISTING,
        null(),
        FILE_SHARE_READ,
    )?;
    if information(&file)?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(invalid());
    }
    validate_private_handle(&file)?;
    validate_path_binding(path, &file)?;
    Ok(file)
}

/// Validate/read an existing mapped image or a file already protected by the
/// caller's mutation guard. This does not pin bytes against concurrent writes;
/// use open_readonly when mutation-boundary validation needs that guarantee.
pub(super) fn open_shared_readonly(path: &Path) -> io::Result<File> {
    let _parents = ancestors(path, false)?;
    let file = open_raw_shared(
        path,
        GENERIC_READ | READ_CONTROL,
        OPEN_EXISTING,
        null(),
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )?;
    if information(&file)?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(invalid());
    }
    validate_private_handle(&file)?;
    validate_path_binding(path, &file)?;
    Ok(file)
}

/// Acquire only after tree, PE identity and signature validation. Holding write
/// access prevents a new executable image mapping while delete sharing permits
/// the owner's validated removal to retain this guard through unlink.
pub(super) fn open_executable_mutation_guard(path: &Path) -> io::Result<File> {
    let _parents = ancestors(path, false)?;
    let file = open_raw_shared(
        path,
        GENERIC_READ | GENERIC_WRITE | READ_CONTROL,
        OPEN_EXISTING,
        null(),
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )?;
    if information(&file)?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(invalid());
    }
    validate_private_handle(&file)?;
    validate_path_binding(path, &file)?;
    Ok(file)
}

pub(super) fn read_bounded(path: &Path, maximum: u64) -> io::Result<Option<Vec<u8>>> {
    let file = match open_readonly(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > maximum {
        return Err(invalid());
    }
    let mut data = Vec::new();
    file.take(maximum.saturating_add(1))
        .read_to_end(&mut data)?;
    if data.len() as u64 > maximum {
        return Err(invalid());
    }
    Ok(Some(data))
}

/// Windows does not implement Unix directory fsync. Keep all path/ACL checks,
/// and require atomic_replace for durable pointer/state-name publication.
pub(super) fn sync_directory(path: &Path) -> io::Result<()> {
    let _parents = ancestors(path, false)?;
    let directory = open_raw(
        path,
        READ_CONTROL | FILE_READ_ATTRIBUTES,
        OPEN_EXISTING,
        null(),
    )?;
    if information(&directory)?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(invalid());
    }
    validate_private_handle(&directory)?;
    validate_path_binding(path, &directory)
}

pub(super) fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    if source == destination || source.parent() != destination.parent() {
        return Err(invalid());
    }
    let parent = source.parent().ok_or_else(invalid)?;
    private_directory(parent)?;
    let _parents = ancestors(source, false)?;
    {
        let input = open_raw(
            source,
            GENERIC_READ | GENERIC_WRITE | READ_CONTROL,
            OPEN_EXISTING,
            null(),
        )?;
        if information(&input)?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            return Err(invalid());
        }
        validate_private_handle(&input)?;
        input.sync_all()?;
    }
    match open_raw(
        destination,
        READ_CONTROL | FILE_READ_ATTRIBUTES,
        OPEN_EXISTING,
        null(),
    ) {
        Ok(file) => {
            if information(&file)?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                return Err(invalid());
            }
            validate_private_handle(&file)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let old = wide(source.as_os_str())?;
    let new = wide(destination.as_os_str())?;
    if unsafe {
        MoveFileExW(
            old.as_ptr(),
            new.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory_with_inheritable_everyone_access(path: &Path, access: u32) {
        let sid = CurrentUser::new().unwrap().string().unwrap();
        directory_with_sddl(
            path,
            &format!("O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;0x{access:x};;;WD)"),
        );
    }

    fn directory_with_sddl(path: &Path, sddl: &str) {
        let sddl = wide(OsStr::new(sddl)).unwrap();
        let mut descriptor = null_mut();
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    null_mut(),
                )
            },
            0
        );
        let descriptor = Local(descriptor);
        let attributes = security_attributes(&descriptor);
        let name = wide(path.as_os_str()).unwrap();
        assert_ne!(unsafe { CreateDirectoryW(name.as_ptr(), &attributes) }, 0);
    }

    fn assert_effective_allow_access(file: &File, kind: i32, access: u32, inherited: bool) {
        use windows_sys::Win32::Security::INHERITED_ACE;

        let expected = known_sid(kind).unwrap();
        let mut dacl: *mut ACL = null_mut();
        let mut descriptor = null_mut();
        assert_eq!(
            unsafe {
                GetSecurityInfo(
                    file.as_raw_handle().cast(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    &mut dacl,
                    null_mut(),
                    &mut descriptor,
                )
            },
            0
        );
        let _allocation = Local(descriptor);
        assert!(!dacl.is_null());
        assert_ne!(unsafe { IsValidAcl(dacl) }, 0);
        let mut size = ACL_SIZE_INFORMATION::default();
        assert_ne!(
            unsafe {
                GetAclInformation(
                    dacl,
                    (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
                    size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            },
            0
        );
        assert!(size.AceCount <= 1024);
        let mut matches = 0;
        for index in 0..size.AceCount {
            let mut raw = null_mut();
            assert_ne!(unsafe { GetAce(dacl, index, &mut raw) }, 0);
            let header = unsafe { &*raw.cast::<ACE_HEADER>() };
            if u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE {
                continue;
            }
            let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
            let sid = (&ace.SidStart as *const u32).cast_mut().cast::<c_void>();
            if same_sid(sid, expected.as_ptr().cast_mut().cast()) {
                assert_eq!(u32::from(header.AceFlags) & INHERITED_ACE != 0, inherited);
                assert_eq!(u32::from(header.AceFlags) & INHERIT_ONLY_ACE, 0);
                assert_eq!(ace.Mask, access);
                matches += 1;
            }
        }
        assert_eq!(matches, 1, "fixture must retain the exact effective ACE");
    }

    #[test]
    fn native_nsis_inherited_read_acl_accepts_files_but_rejects_directories() {
        use windows_sys::Win32::Security::WinWorldSid;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("nsis-style");
        // NSIS's administrator temp-directory ACE is inheritable Everyone
        // 0x1200c1: SYNCHRONIZE | READ_CONTROL | FILE_READ_ATTRIBUTES |
        // FILE_DELETE_CHILD | FILE_READ_DATA. Let Windows inherit the actual
        // ACE onto the file, instead of testing only an arithmetic mask.
        directory_with_inheritable_everyone_access(&root, 0x0012_00c1);
        let state = root.join("fixture");
        fs::write(&state, b"preserve").unwrap();
        let file = open_raw(&state, READ_CONTROL, OPEN_EXISTING, null()).unwrap();
        assert_effective_allow_access(&file, WinWorldSid, 0x0012_00c1, true);
        drop(file);
        let directory = open_raw(
            &root,
            READ_CONTROL | FILE_READ_ATTRIBUTES,
            OPEN_EXISTING,
            null(),
        )
        .unwrap();
        let error = validate_private_handle(&directory).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("foreign-write-ace"));
        drop(directory);

        assert_eq!(read_bounded(&state, 64).unwrap().unwrap(), b"preserve");
        drop(open_private(&state, false).unwrap());
        drop(open_executable_mutation_guard(&state).unwrap());
        assert_eq!(fs::read(&state).unwrap(), b"preserve");
    }

    #[test]
    fn native_python_owner_rights_acl_accepts_files_and_directories() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("python-0700");
        // This is CPython's exact os.mkdir(mode=0o700) Windows descriptor.
        // It deliberately omits O:, letting the token supply the actual owner.
        directory_with_sddl(&root, "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;OW)");
        let directory = open_raw(
            &root,
            READ_CONTROL | FILE_READ_ATTRIBUTES,
            OPEN_EXISTING,
            null(),
        )
        .unwrap();
        assert_effective_allow_access(&directory, WinCreatorOwnerRightsSid, 0x001f_01ff, false);
        drop(directory);
        private_directory(&root).unwrap();

        let state = root.join("fixture");
        fs::write(&state, b"preserve").unwrap();
        let file = open_raw(&state, READ_CONTROL, OPEN_EXISTING, null()).unwrap();
        assert_effective_allow_access(&file, WinCreatorOwnerRightsSid, 0x001f_01ff, true);
        drop(file);
        assert_eq!(read_bounded(&state, 64).unwrap().unwrap(), b"preserve");
        drop(open_private(&state, false).unwrap());
        drop(open_executable_mutation_guard(&state).unwrap());
        assert_eq!(fs::read(&state).unwrap(), b"preserve");
    }

    #[test]
    fn native_owner_rights_sid_is_not_a_trusted_storage_owner() {
        let user = CurrentUser::new().unwrap();
        let system = known_sid(WinLocalSystemSid).unwrap();
        let admins = known_sid(WinBuiltinAdministratorsSid).unwrap();
        let owner_rights = known_sid(WinCreatorOwnerRightsSid).unwrap();
        assert!(!trusted_storage_sid(
            owner_rights.as_ptr().cast_mut().cast(),
            user.sid(),
            system.as_ptr().cast_mut().cast(),
            admins.as_ptr().cast_mut().cast(),
        ));
    }

    #[test]
    fn native_owner_rights_acl_still_rejects_foreign_writers() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("python-0700-with-foreign-writer");
        directory_with_sddl(
            &root,
            "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;OW)(A;OICI;FW;;;WD)",
        );
        let error = private_directory(&root).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("foreign-write-ace"));

        let state = root.join("fixture");
        fs::write(&state, b"preserve").unwrap();
        let error = open_readonly(&state).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("foreign-write-ace"));
        assert!(open_private(&state, true).is_err());
        assert!(open_executable_mutation_guard(&state).is_err());
        assert_eq!(fs::read(&state).unwrap(), b"preserve");
    }

    #[test]
    fn native_foreign_file_mutation_rights_remain_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        for (index, access) in [
            GENERIC_ALL,
            GENERIC_WRITE,
            DELETE,
            WRITE_DAC,
            WRITE_OWNER,
            FILE_WRITE_DATA,
            FILE_APPEND_DATA,
            FILE_WRITE_EA,
            FILE_WRITE_ATTRIBUTES,
        ]
        .into_iter()
        .enumerate()
        {
            let root = temporary.path().join(format!("foreign-access-{index}"));
            directory_with_inheritable_everyone_access(&root, 0x0012_00c1 | access);
            let state = root.join("fixture");
            fs::write(&state, b"preserve").unwrap();
            let error = open_readonly(&state).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(error.to_string().contains("foreign-write-ace"));
            assert!(open_private(&state, true).is_err());
            assert!(open_executable_mutation_guard(&state).is_err());
            assert_eq!(fs::read(&state).unwrap(), b"preserve");
        }
    }

    #[test]
    fn native_owner_allowlist_rejects_unprivileged_sids() {
        use windows_sys::Win32::Security::{WinBuiltinUsersSid, WinWorldSid};

        let user = CurrentUser::new().unwrap();
        let system = known_sid(WinLocalSystemSid).unwrap();
        let admins = known_sid(WinBuiltinAdministratorsSid).unwrap();
        let users = known_sid(WinBuiltinUsersSid).unwrap();
        let everyone = known_sid(WinWorldSid).unwrap();
        let allowed = |sid| {
            trusted_storage_sid(
                sid,
                user.sid(),
                system.as_ptr().cast_mut().cast(),
                admins.as_ptr().cast_mut().cast(),
            )
        };
        assert!(allowed(user.sid()));
        assert!(allowed(system.as_ptr().cast_mut().cast()));
        assert!(allowed(admins.as_ptr().cast_mut().cast()));
        assert!(!allowed(users.as_ptr().cast_mut().cast()));
        assert!(!allowed(everyone.as_ptr().cast_mut().cast()));
        assert!(!allowed(null_mut()));
    }

    #[test]
    fn native_default_owner_storage_and_named_temp_atomic_replace() {
        // Default Windows ownership is TokenOwner, which can be the exact
        // Administrators SID under an elevated native runner/installer.
        let temporary = tempfile::tempdir().unwrap();
        private_directory(temporary.path()).unwrap();
        let destination = temporary.path().join("state.json");
        fs::write(&destination, b"previous").unwrap();
        assert_eq!(
            read_bounded(&destination, 64).unwrap().unwrap(),
            b"previous"
        );
        let mut pending = tempfile::NamedTempFile::new_in(temporary.path()).unwrap();
        std::io::Write::write_all(&mut pending, b"replacement").unwrap();
        pending.as_file().sync_all().unwrap();
        let pending = pending.into_temp_path();
        atomic_replace(&pending, &destination).unwrap();
        assert_eq!(
            read_bounded(&destination, 64).unwrap().unwrap(),
            b"replacement"
        );
        assert!(!pending.exists());
    }

    #[test]
    fn refuses_nonlocal_and_ambiguous_windows_names() {
        for value in [
            r"relative\state",
            r"\\server\share\state",
            r"\\.\C:\state",
            r"C:\state:other",
            r"C:\state.\x",
            r"C:\CON\x",
            r"C:\COM1.txt",
            r"C:\LPT¹.txt",
            r"C:\CONOUT$",
        ] {
            assert!(checked_components(Path::new(value)).is_err(), "{value}");
        }
    }

    #[test]
    fn private_storage_round_trip_and_atomic_replacement() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("protected");
        private_directory(&root).unwrap();
        let target = root.join("state.json");
        let mut old = open_private(&target, false).unwrap();
        std::io::Write::write_all(&mut old, b"old").unwrap();
        old.sync_all().unwrap();
        drop(old);
        assert_eq!(read_bounded(&target, 64).unwrap().unwrap(), b"old");
        let pending = root.join("pending.json");
        let mut file = open_private(&pending, false).unwrap();
        std::io::Write::write_all(&mut file, b"new").unwrap();
        file.sync_all().unwrap();
        drop(file);
        atomic_replace(&pending, &target).unwrap();
        assert_eq!(read_bounded(&target, 64).unwrap().unwrap(), b"new");
        assert!(!pending.exists());
    }

    #[test]
    fn rejects_hard_linked_state_and_preserves_oversized_file() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("protected");
        private_directory(&root).unwrap();
        let state = root.join("state");
        let file = open_private(&state, false).unwrap();
        file.set_len(128).unwrap();
        drop(file);
        assert!(read_bounded(&state, 64).is_err());
        fs::hard_link(&state, root.join("alias")).unwrap();
        assert!(open_private(&state, true).is_err());
        assert!(open_executable_mutation_guard(&state).is_err());
        assert_eq!(fs::metadata(&state).unwrap().len(), 128);
    }

    #[test]
    fn retained_delete_capable_guard_allows_owned_unlink() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("protected");
        private_directory(&root).unwrap();
        let executable = root.join("fixture.exe");
        let mut file = open_private(&executable, false).unwrap();
        std::io::Write::write_all(&mut file, b"unchanged executable fixture").unwrap();
        file.sync_all().unwrap();
        drop(file);
        let guard = open_executable_mutation_guard(&executable).unwrap();
        assert_eq!(
            fs::read(&executable).unwrap(),
            b"unchanged executable fixture"
        );
        fs::remove_file(&executable).unwrap();
        drop(guard);
        assert!(!executable.exists());
        assert!(open_executable_mutation_guard(&executable).is_err());
        assert!(!executable.exists(), "guard must not create absent files");
    }

    #[test]
    fn native_mutation_guard_blocks_loader_until_released() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("protected");
        private_directory(&root).unwrap();
        let executable = root.join("maple-agent.exe");
        // Copy this native updater test binary into explicitly private storage.
        // --list only enumerates its tests; it starts no app, GUI or backend.
        let mut source = File::open(std::env::current_exe().unwrap()).unwrap();
        let mut copied = open_private(&executable, false).unwrap();
        io::copy(&mut source, &mut copied).unwrap();
        copied.sync_all().unwrap();
        drop(copied);
        drop(source);

        let guard = open_executable_mutation_guard(&executable).unwrap();
        assert!(
            std::process::Command::new(&executable)
                .arg("--list")
                .output()
                .is_err(),
            "a retained mutation guard must prevent Windows from mapping the image"
        );
        drop(guard);
        let result = std::process::Command::new(&executable)
            .arg("--list")
            .output()
            .expect("the same validated test image must load after releasing the guard");
        assert!(
            result.status.success(),
            "native test listing failed: {result:?}"
        );
        assert!(
            String::from_utf8(result.stdout)
                .unwrap()
                .contains("native_mutation_guard_blocks_loader_until_released"),
            "the launched image must be this native updater test binary"
        );
    }

    #[test]
    fn native_shared_read_validation_accepts_mapped_image_and_mutation_guard() {
        let mapped = open_shared_readonly(&std::env::current_exe().unwrap())
            .expect("shared read validation must admit the mapped native test image");
        assert!(mapped.metadata().unwrap().len() > 0);
        drop(mapped);

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("protected");
        private_directory(&root).unwrap();
        let path = root.join("fixture.exe");
        let bytes = b"unchanged guarded fixture";
        let mut file = open_private(&path, false).unwrap();
        std::io::Write::write_all(&mut file, bytes).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let guard = open_executable_mutation_guard(&path).unwrap();
        let mut shared = open_shared_readonly(&path).unwrap();
        let mut actual = Vec::new();
        shared.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, bytes);
        assert_eq!(shared.metadata().unwrap().len(), bytes.len() as u64);
        drop(shared);
        drop(guard);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(open_shared_readonly(&root.join("absent")).is_err());
        assert!(!root.join("absent").exists());
    }

    #[test]
    fn rejects_existing_foreign_write_acl_before_truncation() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("protected");
        private_directory(&root).unwrap();
        let state = root.join("unsafe-state");
        let sid = CurrentUser::new().unwrap().string().unwrap();
        let sddl = wide(OsStr::new(&format!(
            "O:{sid}D:P(A;;FA;;;{sid})(A;;FW;;;WD)"
        )))
        .unwrap();
        let mut descriptor = null_mut();
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    null_mut(),
                )
            },
            0
        );
        let descriptor = Local(descriptor);
        let attributes = security_attributes(&descriptor);
        let mut file = open_raw(
            &state,
            GENERIC_READ | GENERIC_WRITE | READ_CONTROL,
            OPEN_ALWAYS,
            &attributes,
        )
        .unwrap();
        std::io::Write::write_all(&mut file, b"preserve").unwrap();
        drop(file);
        assert!(open_readonly(&state).is_err());
        assert!(open_private(&state, true).is_err());
        assert!(open_executable_mutation_guard(&state).is_err());
        assert_eq!(fs::read(&state).unwrap(), b"preserve");
    }

    #[test]
    fn rejects_file_and_ancestor_reparse_points_when_symlink_privilege_available() {
        use std::os::windows::fs::{symlink_dir, symlink_file};

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("protected");
        private_directory(&root).unwrap();
        let state = root.join("state");
        drop(open_private(&state, false).unwrap());
        let alias = root.join("alias");
        if let Err(error) = symlink_file(&state, &alias) {
            // Non-developer Windows accounts may not hold SeCreateSymbolicLinkPrivilege.
            // The native qualification harness must exercise reparse denial on
            // a host that supports the fixture; other errors remain failures.
            if error.raw_os_error() == Some(1314) {
                return;
            }
            panic!("create file symlink fixture: {error}");
        }
        assert!(validate_path(&alias).is_err());
        assert!(open_readonly(&alias).is_err());
        assert!(open_private(&alias, true).is_err());
        assert!(open_executable_mutation_guard(&alias).is_err());
        let directory_alias = temporary.path().join("alias-directory");
        symlink_dir(&root, &directory_alias).unwrap();
        assert!(open_private(&directory_alias.join("new-state"), false).is_err());
        assert!(!root.join("new-state").exists());
    }
}
