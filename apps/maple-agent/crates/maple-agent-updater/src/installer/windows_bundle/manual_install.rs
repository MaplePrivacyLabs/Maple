//! One native owner for the manual install. NSIS only extracts and presents.
use super::*;
use std::{ffi::OsStr, os::windows::ffi::OsStrExt};
use windows_sys::Win32::{Foundation::ERROR_SUCCESS, System::Registry::*};

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain([0]).collect()
}
fn text(value: &str) -> Vec<u16> {
    wide(OsStr::new(value))
}
fn invalid() -> InstallError {
    InstallError::Invalid("Windows installation registration")
}

pub(super) fn shell_path(path: &Path) -> PathBuf {
    // Persist the ordinary drive spelling expected by existing NSIS uninstallers.
    PathBuf::from(
        path.to_str()
            .unwrap_or_default()
            .strip_prefix(r"\\?\")
            .unwrap_or(path.to_str().unwrap_or_default()),
    )
}

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Value {
    kind: u32,
    bytes: Vec<u8>,
}
impl Value {
    fn string(value: &str) -> Self {
        Self {
            kind: REG_SZ,
            bytes: value
                .encode_utf16()
                .chain([0])
                .flat_map(u16::to_le_bytes)
                .collect(),
        }
    }
    fn dword(value: u32) -> Self {
        Self {
            kind: REG_DWORD,
            bytes: value.to_le_bytes().to_vec(),
        }
    }
    fn path(&self) -> Result<PathBuf> {
        if self.kind != REG_SZ || !self.bytes.len().is_multiple_of(2) {
            return Err(invalid());
        }
        let units: Vec<_> = self
            .bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|v| u16::from_le_bytes([v[0], v[1]]))
            .collect();
        if units.last() != Some(&0) || units[..units.len().saturating_sub(1)].contains(&0) {
            return Err(invalid());
        }
        Ok(PathBuf::from(
            String::from_utf16(&units[..units.len() - 1]).map_err(|_| invalid())?,
        ))
    }
}

fn read(key: HKEY, name: &str) -> Result<Option<Value>> {
    let mut kind = 0;
    let mut length = 0;
    let name = text(name);
    let status = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            std::ptr::null_mut(),
            &mut length,
        )
    };
    if status == 2 {
        return Ok(None);
    }
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32).into());
    }
    if length > 65536 {
        return Err(invalid());
    }
    let mut bytes = vec![0; length as usize];
    let status = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            bytes.as_mut_ptr(),
            &mut length,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32).into());
    }
    bytes.truncate(length as usize);
    Ok(Some(Value { kind, bytes }))
}

fn write(key: HKEY, name: &str, value: Option<&Value>) -> Result<()> {
    let name = text(name);
    let status = unsafe {
        match value {
            Some(value) => RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                value.kind,
                value.bytes.as_ptr(),
                value.bytes.len() as u32,
            ),
            None => RegDeleteValueW(key, name.as_ptr()),
        }
    };
    if status != 0 && !(value.is_none() && status == 2) {
        return Err(io::Error::from_raw_os_error(status as i32).into());
    }
    Ok(())
}

struct Registration {
    subkey: Vec<u16>,
    before: BTreeMap<&'static str, Option<Value>>,
    desired: BTreeMap<&'static str, Value>,
    written: Vec<&'static str>,
    key: Option<Key>,
    committed: bool,
}
impl Registration {
    fn admit(root: &Path, profile: &str, version: &str) -> Result<Self> {
        let subkey = text(&format!(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{}",
            profile_identity(profile)?
        ));
        let display = match profile {
            "dev" => "Maple Work Dev",
            "prod" => "Maple Work",
            _ => return Err(invalid()),
        };
        let root = shell_path(root);
        let desired = BTreeMap::from([
            ("DisplayName", Value::string(display)),
            ("DisplayVersion", Value::string(version)),
            ("Publisher", Value::string("Maple Privacy Labs")),
            ("InstallLocation", Value::string(&root.to_string_lossy())),
            (
                "UninstallString",
                Value::string(&format!("\"{}\"", root.join("uninstall.exe").display())),
            ),
            (
                "QuietUninstallString",
                Value::string(&format!("\"{}\" /S", root.join("uninstall.exe").display())),
            ),
            (
                "DisplayIcon",
                Value::string(&format!("\"{}\",0", root.join("maple-agent.exe").display())),
            ),
            ("NoModify", Value::dword(1)),
            ("NoRepair", Value::dword(1)),
        ]);
        let mut handle = std::ptr::null_mut();
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                KEY_QUERY_VALUE | KEY_WOW64_32KEY,
                &mut handle,
            )
        };
        let mut before = BTreeMap::new();
        if status == ERROR_SUCCESS {
            let key = Key(handle);
            for name in desired.keys() {
                before.insert(*name, read(key.0, name)?);
            }
            if let Some(Some(value)) = before.get("InstallLocation") {
                let installed = value.path()?;
                windows_fs::validate_path_syntax(&installed)?;
                let same = match (installed.canonicalize(), root.canonicalize()) {
                    (Ok(a), Ok(b)) => a == b,
                    _ => installed
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&root.to_string_lossy()),
                };
                if !same {
                    return Err(InstallError::NotifyOnly(
                        "use the registered installation location",
                    ));
                }
            }
        } else if status == 2 {
            before.extend(desired.keys().map(|name| (*name, None)));
        } else {
            return Err(io::Error::from_raw_os_error(status as i32).into());
        }
        Ok(Self {
            subkey,
            before,
            desired,
            written: Vec::new(),
            key: None,
            committed: false,
        })
    }

    fn apply(&mut self) -> Result<()> {
        let mut handle = std::ptr::null_mut();
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                self.subkey.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_WOW64_32KEY,
                std::ptr::null(),
                &mut handle,
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32).into());
        }
        self.key = Some(Key(handle));
        // Detect a registration changed between admission and root-lock acquisition.
        for (name, old) in &self.before {
            if read(handle, name)? != *old {
                return Err(InstallError::Busy);
            }
        }
        for (name, value) in &self.desired {
            write(handle, name, Some(value))?;
            self.written.push(name);
        }
        Ok(())
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Some(key) = &self.key {
            // Never overwrite a concurrent registration's result on rollback.
            if self
                .written
                .iter()
                .all(|name| read(key.0, name).is_ok_and(|v| v.as_ref() == self.desired.get(name)))
            {
                for name in self.written.iter().rev() {
                    let _ = write(key.0, name, self.before[name].as_ref());
                }
            }
        }
        self.key.take();
    }
}

struct ProfileLock {
    _file: File,
    _namespace: Vec<File>,
}

fn profile_lock(profile: &str) -> Result<ProfileLock> {
    profile_identity(profile)?;
    let directory = PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or_else(invalid)?)
        .join("Maple Work Installer Locks");
    windows_fs::private_directory(&directory)?;
    let lock = windows_fs::open_private(&directory.join(format!("{profile}.lock")), false)?;
    lock.try_lock_exclusive().map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock
            || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
        {
            InstallError::Busy
        } else {
            error.into()
        }
    })?;
    let namespace = windows_fs::pin_directory(&directory)?;
    windows_fs::validate_path_binding(&directory.join(format!("{profile}.lock")), &lock)?;
    Ok(ProfileLock {
        _file: lock,
        _namespace: namespace,
    })
}

/// NSIS unpacks its plug-ins into an `ns*.tmp` folder and the payload into a
/// protected `<run>` folder under `%LOCALAPPDATA%\Maple Work Installer Staging`
/// and deletes both when it exits. A killed installer leaves them behind, about
/// 150 MB, so a later install removes folders there older than an hour. Only
/// that fixed root is touched, never this run's own folder, and never a link:
/// `remove_dir_all` does not follow them.
fn remove_stale_staging(payload: &Path) {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    const STAGING: &str = "Maple Work Installer Staging";
    let Some(run) = payload.parent() else {
        return;
    };
    let Some(root) = run
        .parent()
        .filter(|r| r.file_name() == Some(OsStr::new(STAGING)))
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.take(64).flatten() {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let stale = metadata
            .created()
            .ok()
            .and_then(|created| created.elapsed().ok())
            .is_some_and(|age| age > std::time::Duration::from_secs(3600));
        if path != run
            && stale
            && metadata.is_dir()
            && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
        {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

fn digest(file: &File) -> Result<String> {
    let mut copied = file.try_clone()?;
    copied.seek(SeekFrom::Start(0))?;
    Ok(hex::encode(hash_reader(
        &mut copied,
        file.metadata()?.len(),
    )?))
}

fn copy_new(source: &File, destination: &Path) -> Result<()> {
    if destination.try_exists()? {
        let existing = windows_fs::open_readonly(destination)?;
        if digest(source)? != digest(&existing)? {
            return Err(InstallError::Invalid("existing installer payload differs"));
        }
        return Ok(());
    }
    let mut output = windows_fs::create_private(destination)?;
    let mut source = source.try_clone()?;
    source.seek(SeekFrom::Start(0))?;
    io::copy(&mut source, &mut output)?;
    output.sync_all()?;
    Ok(())
}

fn install_uninstaller(source: &File, root: &Path, profile: &str) -> Result<()> {
    let destination = root.join("uninstall.exe");
    if destination.try_exists()? {
        let _pin = windows_fs::open_readonly(&destination)?;
        return authenticode::verify_or_unsigned_detailed(&destination, profile == "dev");
    }
    let temporary = tempfile::Builder::new()
        .prefix(TRANSACTION_TEMP_PREFIX)
        .rand_bytes(STAGING_RANDOM_LENGTH)
        .tempfile_in(root)?
        .into_temp_path();
    let mut output = windows_fs::open_private(&temporary, true)?;
    let mut copied = source.try_clone()?;
    copied.seek(SeekFrom::Start(0))?;
    io::copy(&mut copied, &mut output)?;
    output.sync_all()?;
    drop(output);
    if exact_file_digest(&temporary)? != digest(source)? {
        return Err(InstallError::CodeSignature);
    }
    windows_fs::publish_new(&temporary, &destination)?;
    Ok(())
}

/// Legacy entrypoints remain available; new NSIS packages use only this one.
pub fn install_windows_bundle(
    checker: &Path,
    root: &Path,
    profile: &str,
    generation: &str,
) -> Result<()> {
    install_windows_bundle_with_progress(checker, root, profile, generation, &mut |_, _| {})
}

pub fn install_windows_bundle_with_progress(
    checker: &Path,
    root: &Path,
    profile: &str,
    generation: &str,
    progress: &mut dyn FnMut(WindowsInstallOperation, bool),
) -> Result<()> {
    install_with_progress(
        checker,
        root,
        profile,
        generation,
        None,
        || Ok(()),
        progress,
    )
}

#[cfg(test)]
fn install(
    checker: &Path,
    root: &Path,
    profile: &str,
    generation: &str,
    menu: Option<&Path>,
    before_commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    install_with_progress(
        checker,
        root,
        profile,
        generation,
        menu,
        before_commit,
        &mut |_, _| {},
    )
}

fn install_with_progress(
    checker: &Path,
    root: &Path,
    profile: &str,
    generation_name: &str,
    menu: Option<&Path>,
    before_commit: impl FnOnce() -> Result<()>,
    progress: &mut dyn FnMut(WindowsInstallOperation, bool),
) -> Result<()> {
    let mut operation = WindowsInstallOperation::InstallLock;
    let mut mutations_started = false;
    let result = (|| -> Result<()> {
        progress(operation, mutations_started);
        let _profile_lock = profile_lock(profile)?;
        operation = WindowsInstallOperation::Checker;
        progress(operation, mutations_started);
        let checker = checker.canonicalize()?;
        let payload = checker.parent().ok_or_else(invalid)?;
        let _payload_paths = windows_fs::pin_directory(payload)?;
        remove_stale_staging(payload);
        // Pin all payload bytes and their namespace before authenticating them.
        let sources: BTreeMap<_, _> = FILES
            .iter()
            .chain([&"uninstall.exe"])
            .map(|name| Ok((*name, windows_fs::open_readonly(&payload.join(name))?)))
            .collect::<Result<_>>()?;
        let info = admit_windows_installation(&checker, root, profile)?;
        let bundle = descriptor(payload, profile)?;
        if checker.file_name() != Some(OsStr::new("maple-agent.exe"))
            || bundle.generation() != generation_name
            || info["version"] != bundle.version
            || info["source_sha"] != bundle.source_sha
        {
            return Err(InstallError::CodeSignature);
        }
        verify_identity_metadata(payload, &bundle)?;
        authenticode::verify_or_unsigned_detailed(
            &payload.join("uninstall.exe"),
            profile == "dev",
        )?;
        if fs::read_dir(payload)?.count() != FILES.len() + 1 {
            return Err(InstallError::Invalid("manual payload inventory"));
        }
        operation = WindowsInstallOperation::Registration;
        progress(operation, mutations_started);
        let admitted_registration = Registration::admit(root, profile, &bundle.version)?;
        operation = WindowsInstallOperation::Preparation;
        mutations_started = true;
        progress(operation, mutations_started);
        let lock =
            prepare_admitted_windows_installation(&checker, root, profile, generation_name, &info)?;
        let root = root.canonicalize()?;
        let _root_paths = windows_fs::pin_directory(&root)?;
        windows_fs::validate_path_binding(&root.join(".install.lock"), &lock)?;
        operation = WindowsInstallOperation::Payload;
        progress(operation, mutations_started);
        let target = root.join("versions").join(generation_name);
        windows_fs::private_directory(&target)?;
        let _target_paths = windows_fs::pin_directory(&target)?;
        for name in FILES {
            copy_new(&sources[name], &target.join(name))?;
        }
        install_uninstaller(&sources["uninstall.exe"], &root, profile)?;
        operation = WindowsInstallOperation::Registration;
        progress(operation, mutations_started);
        // Roll registration back before releasing the root/profile locks.
        let mut registration = admitted_registration;
        registration.apply()?;
        let mut links = shell_integration::install_links(&root, profile, menu)?;
        operation = WindowsInstallOperation::Activation;
        progress(operation, mutations_started);
        before_commit()?;
        activate_windows_installation_with_lock(
            &root.join("maple-agent.exe"),
            profile,
            generation_name,
            Some(&lock),
        )?;
        registration.committed = true;
        links.commit();
        Ok(())
    })();
    result.map_err(|error| {
        error
            .during(operation)
            .with_mutations_started(mutations_started)
    })
}

#[cfg(test)]
#[path = "manual_install_tests.rs"]
mod tests;
