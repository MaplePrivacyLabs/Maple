//! Cosmetic, idempotent shell metadata for an already admitted installation.
//! Persistent public artwork is outside both installed generations and store-v1.

use super::*;
use std::ffi::{OsStr, OsString};
use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
use windows::{
    Win32::{
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree, CoUninitialize, IPersistFile, STGM_READ,
        },
        UI::Shell::{
            FOLDERID_Programs, IShellLinkW, KF_FLAG_DEFAULT, SHCNE_UPDATEITEM, SHCNF_PATHW,
            SHChangeNotify, SHGetKnownFolderPath, SLGP_RAWPATH, ShellLink,
        },
    },
    core::{HSTRING, Interface},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_SUCCESS, FILETIME, HANDLE, WAIT_OBJECT_0},
    Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle},
    System::{
        Registry::*,
        Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            WaitForSingleObject,
        },
    },
};

const MAX_ICON: usize = 1024 * 1024;
const MAX_LINK: u64 = 1024 * 1024;
const MAX_WIDE: usize = 32_000;

fn unavailable() -> InstallError {
    InstallError::Invalid("Windows shell metadata is unavailable")
}

fn native<T>(value: windows::core::Result<T>) -> Result<T> {
    value.map_err(|_| unavailable())
}

struct Apartment;
impl Apartment {
    fn new() -> Result<Self> {
        native(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok())?;
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn wide_path(path: &Path) -> Result<HSTRING> {
    windows_fs::validate_path_syntax(path)?;
    Ok(HSTRING::from(path.as_os_str()))
}

fn from_wide(value: &[u16]) -> Result<OsString> {
    let end = value
        .iter()
        .position(|unit| *unit == 0)
        .ok_or_else(unavailable)?;
    Ok(OsString::from_wide(&value[..end]))
}

fn programs() -> Result<PathBuf> {
    let value = native(unsafe { SHGetKnownFolderPath(&FOLDERID_Programs, KF_FLAG_DEFAULT, None) })?;
    let result = unsafe { value.to_string() }
        .map(PathBuf::from)
        .map_err(|_| unavailable());
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    result
}

fn notify(path: &Path) {
    if let Ok(path) = wide_path(path) {
        unsafe {
            SHChangeNotify(
                SHCNE_UPDATEITEM,
                SHCNF_PATHW,
                Some(path.as_ptr().cast()),
                None,
            )
        };
    }
}

fn admitted_root(executable: &Path, profile: &str) -> Result<PathBuf> {
    let executable = executable.canonicalize()?;
    let generation = executable.parent().ok_or_else(unavailable)?;
    let versions = generation.parent().ok_or_else(unavailable)?;
    if executable.file_name() != Some(OsStr::new("maple-agent.exe"))
        || versions.file_name() != Some(OsStr::new("versions"))
    {
        return Err(unavailable());
    }
    let root = versions.parent().ok_or_else(unavailable)?;
    // This reuses registered-root, DACL/reparse, bootstrap and selected
    // generation signature/identity admission. It never trusts a shortcut.
    if resolve_windows_launcher(&root.join("maple-agent.exe"), profile)?.as_ref()
        != Some(&executable)
    {
        return Err(unavailable());
    }
    Ok(root.to_owned())
}

struct Process(HANDLE);
impl Drop for Process {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn wait_completed_installer(root: &Path, profile: &str) -> Result<()> {
    if !root.join(MANUAL_INTENT).try_exists()? {
        return Ok(());
    }
    let bytes = match read_bounded(&root.join(MANUAL_INTENT), DESCRIPTOR_LIMIT) {
        Ok(bytes) => bytes,
        // Activation may finish between the existence probe and this read.
        Err(InstallError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let intent: ManualIntent = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
    let (_, current, bundle) = generation(root, profile)?;
    if intent.format != "maple-agent-windows-installer-intent-v1"
        || intent.profile != profile
        || intent.generation != bundle.generation()
        || intent.version != bundle.version
        || intent.source_sha != bundle.source_sha
        || intent.executable_sha256 != exact_file_digest(&current.join("maple-agent.exe"))?
    {
        return Err(InstallError::Busy);
    }
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            intent.owner_pid,
        )
    };
    if process.is_null() {
        return if io::Error::last_os_error().raw_os_error() == Some(87) {
            Ok(())
        } else {
            Err(unavailable())
        };
    }
    let process = Process(process);
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(process.0, &mut created, &mut exited, &mut kernel, &mut user) } == 0
    {
        return Err(unavailable());
    }
    let created = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    if created != intent.owner_created {
        return Err(unavailable());
    }
    // No lock or COM object is retained during this one exact-owner wait.
    if unsafe { WaitForSingleObject(process.0, 10_000) } != WAIT_OBJECT_0 {
        return Err(InstallError::Busy);
    }
    Ok(())
}

fn mutation_lock(root: &Path) -> Result<File> {
    let lock = open_install_lock(root)?;
    lock.try_lock_exclusive().map_err(|_| InstallError::Busy)?;
    if root.join(MANUAL_INTENT).try_exists()? || root.join(UNINSTALL_INTENT).try_exists()? {
        return Err(InstallError::Busy);
    }
    Ok(lock)
}

fn materialize_icon(directory: &Path, icon: &[u8]) -> Result<PathBuf> {
    if !(6..=MAX_ICON).contains(&icon.len()) || icon[..4] != [0, 0, 1, 0] {
        return Err(unavailable());
    }
    windows_fs::private_directory(directory)?;
    let path = directory.join(format!("{:x}.ico", sha2::Sha256::digest(icon)));
    if path.try_exists()? {
        if read_bounded(&path, MAX_ICON as u64)? != icon {
            return Err(unavailable());
        }
        return Ok(path);
    }
    let mut temporary = tempfile::Builder::new()
        .prefix(".maple-icon-")
        .tempfile_in(directory)?;
    temporary.write_all(icon)?;
    temporary.as_file().sync_all()?;
    windows_fs::atomic_replace(&temporary.into_temp_path(), &path)?;
    Ok(path)
}

fn file_identity(file: &File) -> Result<[u32; 3]> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info) } == 0 {
        return Err(unavailable());
    }
    Ok([
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ])
}

fn load_link(path: &Path) -> Result<IShellLinkW> {
    let link: IShellLinkW =
        native(unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) })?;
    let persist: IPersistFile = native(link.cast())?;
    native(unsafe { persist.Load(&wide_path(path)?, STGM_READ) })?;
    Ok(link)
}

fn matching_target(link: &IShellLinkW, expected: &Path) -> Result<bool> {
    let mut target = vec![0; MAX_WIDE];
    let mut arguments = vec![0; MAX_WIDE];
    native(unsafe { link.GetPath(&mut target, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32) })?;
    native(unsafe { link.GetArguments(&mut arguments) })?;
    if !from_wide(&arguments)?.is_empty() {
        return Ok(false);
    }
    let target = PathBuf::from(from_wide(&target)?);
    // Reject UNC/device/reparse targets without invoking Shell Resolve or a
    // search. Only a private fixed installed executable can match.
    let _target = windows_fs::open_readonly(&target)?;
    Ok(target.canonicalize()? == expected.canonicalize()?)
}

fn refresh_link(path: &Path, expected: &Path, icon: &Path) -> Result<bool> {
    let pin = match windows_fs::open_readonly(path) {
        Ok(pin) => pin,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if pin.metadata()?.len() > MAX_LINK {
        return Err(unavailable());
    }
    let identity = file_identity(&pin)?;
    let original = read_bounded(path, MAX_LINK)?;
    let link = load_link(path)?;
    if !matching_target(&link, expected)? {
        return Ok(false);
    }
    let mut current_icon = vec![0; MAX_WIDE];
    let mut index = 0;
    native(unsafe { link.GetIconLocation(&mut current_icon, &mut index) })?;
    if Path::new(&from_wide(&current_icon)?) == icon && index == 0 {
        return Ok(false);
    }
    native(unsafe { link.SetIconLocation(&wide_path(icon)?, 0) })?;
    let parent = path.parent().ok_or_else(unavailable)?;
    windows_fs::private_directory(parent)?;
    let temporary = tempfile::Builder::new()
        .prefix(".maple-icon-")
        .suffix(".lnk")
        .tempfile_in(parent)?
        .into_temp_path();
    let persist: IPersistFile = native(link.cast())?;
    native(unsafe { persist.Save(&wide_path(&temporary)?, false) })?;
    let copied = load_link(&temporary)?;
    let current = windows_fs::open_readonly(path)?;
    if !matching_target(&copied, expected)?
        || read_bounded(path, MAX_LINK)? != original
        || file_identity(&current)? != identity
    {
        return Err(unavailable());
    }
    drop(copied);
    drop(persist);
    drop(link);
    drop(current);
    // The original read pin forbids writes/deletion while COM prepares the
    // private sibling. Release it only for the checked atomic replacement.
    drop(pin);
    windows_fs::atomic_replace(&temporary, path)?;
    Ok(true)
}

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn wide_text(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

fn registry_text(key: HKEY, name: &str) -> Result<Option<Vec<u16>>> {
    let name = wide_text(name);
    let mut kind = 0;
    let mut length = 0;
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
    if status != ERROR_SUCCESS
        || kind != REG_SZ
        || !(2..=65536).contains(&length)
        || length % 2 != 0
    {
        return Err(unavailable());
    }
    let mut value = vec![0u16; length as usize / 2];
    if unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            value.as_mut_ptr().cast(),
            &mut length,
        )
    } != ERROR_SUCCESS
        || kind != REG_SZ
        || length as usize != value.len() * 2
        || value.last() != Some(&0)
        || value[..value.len() - 1].contains(&0)
    {
        return Err(unavailable());
    }
    Ok(Some(value))
}

fn refresh_display_icon(subkey: &str, root: &Path, icon: &Path) -> Result<bool> {
    let mut handle = std::ptr::null_mut();
    if unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide_text(subkey).as_ptr(),
            0,
            KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_WOW64_32KEY,
            &mut handle,
        )
    } != ERROR_SUCCESS
    {
        return Err(unavailable());
    }
    let key = Key(handle);
    let registered = registry_text(key.0, "InstallLocation")?.ok_or_else(unavailable)?;
    let registered = PathBuf::from(OsString::from_wide(&registered[..registered.len() - 1]));
    if registered.canonicalize()? != root {
        return Err(unavailable());
    }
    let value = wide_text(&format!("\"{}\",0", icon.display()));
    if registry_text(key.0, "DisplayIcon")? == Some(value.clone()) {
        return Ok(false);
    }
    let bytes = value
        .iter()
        .flat_map(|unit| unit.to_le_bytes())
        .collect::<Vec<_>>();
    if unsafe {
        RegSetValueExW(
            key.0,
            wide_text("DisplayIcon").as_ptr(),
            0,
            REG_SZ,
            bytes.as_ptr(),
            bytes.len() as u32,
        )
    } != ERROR_SUCCESS
    {
        return Err(unavailable());
    }
    Ok(true)
}

/// Refresh only existing shell metadata for this admitted running generation.
/// Failures are cosmetic: callers must never gate application/update startup.
pub fn refresh_windows_shell_integration(
    executable: &Path,
    profile: &str,
    display_name: &str,
    directory: &Path,
    icon: &[u8],
) -> Result<()> {
    let root = admitted_root(executable, profile)?;
    wait_completed_installer(&root, profile)?;
    // The existing resolver can now retire a matching dead-owner completed
    // intent. A live/incomplete journal remains a strict mutation fence.
    if admitted_root(executable, profile)? != root {
        return Err(unavailable());
    }
    let _lock = mutation_lock(&root)?;
    if admitted_root(executable, profile)? != root {
        return Err(unavailable());
    }
    let info = pe_build_info(executable)?;
    if info["display_name"] != display_name
        || directory.file_name() != Some(OsStr::new("windows-integration"))
        || directory.parent().and_then(Path::file_name)
            != info["data_namespace"].as_str().map(OsStr::new)
    {
        return Err(unavailable());
    }
    // Reject a redirected profile root before creating anything. Existing
    // ancestors receive validation only, never an ACL takeover.
    let mut ancestor = directory;
    let mut missing = Vec::new();
    while !ancestor.try_exists()? {
        missing.push(ancestor.file_name().ok_or_else(unavailable)?);
        ancestor = ancestor.parent().ok_or_else(unavailable)?;
    }
    windows_fs::private_directory(ancestor)?;
    let mut prospective = ancestor.canonicalize()?;
    for name in missing.into_iter().rev() {
        prospective.push(name);
    }
    if prospective.starts_with(&root) {
        return Err(unavailable());
    }
    let _apartment = Apartment::new()?;
    let icon = materialize_icon(directory, icon)?;
    let shortcut_directory = programs()?.join(display_name);
    let mut failed = false;
    let mut changed_links = Vec::new();
    for (name, target) in [
        (format!("{display_name}.lnk"), "maple-agent.exe"),
        ("Uninstall.lnk".into(), "uninstall.exe"),
    ] {
        let path = shortcut_directory.join(name);
        match refresh_link(&path, &root.join(target), &icon) {
            Ok(true) => changed_links.push(path),
            Ok(false) => {}
            Err(_) => failed = true,
        }
    }
    let subkey = format!(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{}",
        profile_identity(profile)?
    );
    // This is deliberately last, after both known shortcut attempts. The same
    // opened 32-bit-view key must still name the admitted installation; missing
    // uninstall registration is never recreated during shutdown/uninstall.
    let changed = refresh_display_icon(&subkey, &root, &icon)?;
    drop(_apartment);
    drop(_lock);
    for path in changed_links {
        notify(&path);
    }
    if changed {
        notify(&icon);
    }
    if failed { Err(unavailable()) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("private");
        windows_fs::private_directory(&root).unwrap();
        (temporary, root)
    }

    fn write(path: &Path, bytes: &[u8]) {
        let mut file = windows_fs::open_private(path, true).unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }

    fn icon(root: &Path) -> PathBuf {
        materialize_icon(
            &root.join("artwork"),
            b"\0\0\x01\0\x01\0ordinary icon fixture",
        )
        .unwrap()
    }

    fn link(path: &Path, target: &Path, arguments: &str, working: &Path) {
        let link: IShellLinkW =
            native(unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }).unwrap();
        unsafe {
            link.SetPath(&wide_path(target).unwrap()).unwrap();
            link.SetArguments(&HSTRING::from(arguments)).unwrap();
            link.SetWorkingDirectory(&wide_path(working).unwrap())
                .unwrap();
            link.SetDescription(&HSTRING::from("retain this shortcut description"))
                .unwrap();
            link.SetIconLocation(&wide_path(target).unwrap(), -3)
                .unwrap();
            link.SetHotkey(0x0655).unwrap();
        }
        let persist: IPersistFile = link.cast().unwrap();
        native(unsafe { persist.Save(&wide_path(path).unwrap(), true) }).unwrap();
    }

    fn properties(path: &Path) -> (OsString, OsString, OsString, OsString, u16, OsString, i32) {
        let link = load_link(path).unwrap();
        let mut target = vec![0; MAX_WIDE];
        let mut args = vec![0; MAX_WIDE];
        let mut working = vec![0; MAX_WIDE];
        let mut description = vec![0; MAX_WIDE];
        let mut artwork = vec![0; MAX_WIDE];
        let mut index = 0;
        unsafe {
            link.GetPath(&mut target, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)
                .unwrap();
            link.GetArguments(&mut args).unwrap();
            link.GetWorkingDirectory(&mut working).unwrap();
            link.GetDescription(&mut description).unwrap();
            link.GetIconLocation(&mut artwork, &mut index).unwrap();
        }
        (
            from_wide(&target).unwrap(),
            from_wide(&args).unwrap(),
            from_wide(&working).unwrap(),
            from_wide(&description).unwrap(),
            unsafe { link.GetHotkey() }.unwrap(),
            from_wide(&artwork).unwrap(),
            index,
        )
    }

    #[test]
    fn native_python_shortcut_probe_matches_typed_com_and_preserves_properties() {
        use std::{
            process::{Command, Stdio},
            time::{Duration, Instant},
        };
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWMINNOACTIVE;

        fn shortcut(root: &Path, name: &str) -> PathBuf {
            root.join(if name == "main" {
                "Maple Agent Dev.lnk"
            } else {
                "Uninstall.lnk"
            })
        }

        fn snapshot(path: &Path) -> serde_json::Value {
            let value = properties(path);
            let link = load_link(path).unwrap();
            serde_json::json!({
                "target": value.0.to_string_lossy(),
                "arguments": value.1.to_string_lossy(),
                "working_directory": value.2.to_string_lossy(),
                "description": value.3.to_string_lossy(),
                "hotkey": value.4,
                "window_style": unsafe { link.GetShowCmd() }.unwrap().0,
                "icon_location": format!("{},{}", value.5.to_string_lossy(), value.6),
            })
        }

        fn expected_aliases(target: &Path) -> serde_json::Value {
            use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
            let canonical = target.canonicalize().unwrap();
            let mut short = vec![0; MAX_WIDE];
            let length = unsafe {
                GetShortPathNameW(
                    wide_path(&canonical).unwrap().as_ptr(),
                    short.as_mut_ptr(),
                    short.len() as u32,
                )
            };
            assert!(length > 0 && (length as usize) < short.len());
            // With 8.3 naming disabled Windows returns the ordinary spelling.
            // All aliases still run; none of this test is skipped or retried.
            serde_json::json!([
                target.to_string_lossy(),
                canonical.to_string_lossy(),
                from_wide(&short).unwrap().to_string_lossy(),
            ])
        }

        fn probe(root: &Path, mode: &str) -> serde_json::Value {
            let stdout_path = root.join(format!("probe-{mode}.json"));
            let stderr_path = root.join(format!("probe-{mode}.log"));
            let module = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../scripts/windows-release.py")
                .canonicalize()
                .unwrap();
            let mut child = Command::new("python3")
                .arg("-I")
                .arg(root.join("probe.py"))
                .arg(module)
                .arg(root)
                .arg(mode)
                .stdin(Stdio::null())
                .stdout(windows_fs::open_private(&stdout_path, true).unwrap())
                .stderr(windows_fs::open_private(&stderr_path, true).unwrap())
                .spawn()
                .expect("the Windows native lane must provide Python");
            let deadline = Instant::now() + Duration::from_secs(20);
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) => {}
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("could not observe exact Python probe child: {error}");
                    }
                }
                if Instant::now() >= deadline
                    || [&stdout_path, &stderr_path]
                        .iter()
                        .any(|path| fs::metadata(path).map_or(true, |info| info.len() > 64 * 1024))
                {
                    let _ = child.kill();
                    let _ = child.wait();
                    let phases = read_bounded(&stderr_path, 64 * 1024).unwrap_or_default();
                    panic!(
                        "exact Python shortcut probe exceeded time/output bound: {}",
                        String::from_utf8_lossy(&phases)
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let phases = read_bounded(&stderr_path, 64 * 1024).unwrap();
            assert!(
                status.success(),
                "Python shortcut probe failed ({status}): {}\nOwned fixture expected paths and typed COM metadata: {}",
                String::from_utf8_lossy(&phases),
                String::from_utf8_lossy(
                    &read_bounded(&root.join("fixtures.json"), 64 * 1024).unwrap()
                )
            );
            serde_json::from_slice(&read_bounded(&stdout_path, 64 * 1024).unwrap()).unwrap()
        }

        let (_temporary, root) = fixture();
        let _apartment = Apartment::new().unwrap();
        let artwork = icon(&root);
        let mut before = serde_json::Map::new();
        let mut fixtures = serde_json::Map::new();
        for (name, target) in [("main", "maple-agent.exe"), ("uninstall", "uninstall.exe")] {
            let target = root.join(target);
            write(&target, b"ordinary path fixture; never executed");
            let path = shortcut(&root, name);
            link(&path, &target, "", &root);
            let link = load_link(&path).unwrap();
            unsafe { link.SetShowCmd(SW_SHOWMINNOACTIVE) }.unwrap();
            let persist: IPersistFile = link.cast().unwrap();
            unsafe { persist.Save(&wide_path(&path).unwrap(), false) }.unwrap();
            before.insert(name.to_owned(), snapshot(&path));
            fixtures.insert(
                name.to_owned(),
                serde_json::json!({
                    "expected_aliases": expected_aliases(&target),
                    "typed_metadata": before[name],
                }),
            );
        }
        write(
            &root.join("fixtures.json"),
            &serde_json::to_vec(&fixtures).unwrap(),
        );
        let foreign = root.join("another.exe");
        write(&foreign, b"unrelated path fixture");
        link(&root.join("foreign.lnk"), &foreign, "", &root);
        link(
            &root.join("arguments.lnk"),
            &root.join("maple-agent.exe"),
            "--custom-argument",
            &root,
        );
        let hardlink_target = root.join("same-file-alias.exe");
        fs::hard_link(root.join("maple-agent.exe"), &hardlink_target).unwrap();
        link(&root.join("hardlink.lnk"), &hardlink_target, "", &root);
        write(&root.join("malformed.lnk"), b"not a shell link");
        let rejected = [
            "foreign.lnk",
            "arguments.lnk",
            "hardlink.lnk",
            "malformed.lnk",
        ]
        .map(|name| (name, fs::read(root.join(name)).unwrap()));
        write(
            &root.join("probe.py"),
            br#"import importlib.util, json, sys
from pathlib import Path
spec = importlib.util.spec_from_file_location("windows_release_native_probe", sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
root, mode = Path(sys.argv[2]), sys.argv[3]
fixtures = json.loads((root / "fixtures.json").read_text(encoding="utf-8"))
def phase(value):
    print(value, file=sys.stderr, flush=True)
result = {}
for name, shortcut, filename in (("main", "Maple Agent Dev.lnk", "maple-agent.exe"), ("uninstall", "Uninstall.lnk", "uninstall.exe")):
    path, target = root / shortcut, root / filename
    before = module.probe_windows_shortcut(path, target, phase=phase)
    for alias in fixtures[name]["expected_aliases"]:
        assert module.probe_windows_shortcut(path, Path(alias), phase=phase) == before
    if mode == "seed":
        seeded = module.probe_windows_shortcut(path, target, seed=True,
            working_directory=root, description=before["description"], phase=phase)
        assert {key: value for key, value in seeded.items() if key != "icon_location"} == {key: value for key, value in before.items() if key != "icon_location"}
    after = module.probe_windows_shortcut(path, target, phase=phase)
    if mode == "seed":
        assert after == seeded
    result[name] = {"before": before, "after": after}
if mode == "seed":
    for name in ("foreign.lnk", "arguments.lnk", "hardlink.lnk", "malformed.lnk"):
        path = root / name
        before = path.read_bytes()
        try:
            module.probe_windows_shortcut(path, root / "maple-agent.exe", seed=True,
                working_directory=root, phase=phase)
        except (ValueError, OSError):
            pass
        else:
            raise AssertionError("foreign/malformed shortcut was accepted")
        assert path.read_bytes() == before
print(json.dumps(result), flush=True)
"#,
        );
        let seeded = probe(&root, "seed");
        // The negative fixture is a same-file alias, not a legitimate path
        // spelling. Remove that owned hard link before product DACL/link checks.
        fs::remove_file(&hardlink_target).unwrap();
        for (name, target) in [("main", "maple-agent.exe"), ("uninstall", "uninstall.exe")] {
            let path = shortcut(&root, name);
            assert_eq!(seeded[name]["before"], before[name]);
            assert_eq!(seeded[name]["after"], snapshot(&path));
            let mut expected = before[name].clone();
            expected["icon_location"] =
                serde_json::json!(format!("{},0", root.join(target).display()));
            assert_eq!(seeded[name]["after"], expected);
            assert!(refresh_link(&path, &root.join(target), &artwork).unwrap());
            expected["icon_location"] = serde_json::json!(format!("{},0", artwork.display()));
            assert_eq!(snapshot(&path), expected);
        }
        let refreshed = probe(&root, "read");
        for name in ["main", "uninstall"] {
            assert_eq!(refreshed[name]["before"], snapshot(&shortcut(&root, name)));
            assert_eq!(refreshed[name]["after"], refreshed[name]["before"]);
        }
        for (name, bytes) in rejected {
            assert_eq!(fs::read(root.join(name)).unwrap(), bytes);
        }
    }

    #[test]
    fn native_shell_icon_refresh_preserves_shortcut_properties_and_is_idempotent() {
        let (_temporary, root) = fixture();
        let _apartment = Apartment::new().unwrap();
        let artwork = icon(&root);
        for target in ["maple-agent.exe", "uninstall.exe"] {
            let target = root.join(target);
            write(&target, b"ordinary executable-path fixture; never executed");
            let shortcut = root.join(format!(
                "{}.lnk",
                target.file_stem().unwrap().to_string_lossy()
            ));
            link(&shortcut, &target, "", &root);
            let before = properties(&shortcut);
            assert!(refresh_link(&shortcut, &target, &artwork).unwrap());
            let after = properties(&shortcut);
            assert_eq!(
                (&after.0, &after.1, &after.2, &after.3, after.4),
                (&before.0, &before.1, &before.2, &before.3, before.4)
            );
            assert_eq!(Path::new(&after.5), artwork);
            assert_eq!(after.6, 0);
            let once = fs::read(&shortcut).unwrap();
            assert!(!refresh_link(&shortcut, &target, &artwork).unwrap());
            assert_eq!(fs::read(&shortcut).unwrap(), once);
        }
    }

    #[test]
    fn native_shell_icon_refresh_does_not_retarget_foreign_or_argument_shortcuts() {
        let (_temporary, root) = fixture();
        let _apartment = Apartment::new().unwrap();
        let expected = root.join("maple-agent.exe");
        let foreign = root.join("another.exe");
        write(&expected, b"expected fixture");
        write(&foreign, b"unrelated fixture");
        let artwork = icon(&root);
        let shortcut = root.join("known-name.lnk");
        for (target, arguments) in [(&foreign, ""), (&expected, "--custom-argument")] {
            link(&shortcut, target, arguments, &root);
            let before = fs::read(&shortcut).unwrap();
            assert!(!refresh_link(&shortcut, &expected, &artwork).unwrap());
            assert_eq!(fs::read(&shortcut).unwrap(), before);
        }
        assert!(!refresh_link(&root.join("absent.lnk"), &expected, &artwork).unwrap());
        assert!(!root.join("absent.lnk").exists());
    }

    #[test]
    fn native_shell_icon_refresh_refuses_malformed_oversized_linked_and_reparse_files() {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        let (_temporary, root) = fixture();
        let _apartment = Apartment::new().unwrap();
        let expected = root.join("maple-agent.exe");
        write(&expected, b"expected fixture");
        let artwork = icon(&root);
        let shortcut = root.join("known-name.lnk");
        for contents in [b"not a shell link".to_vec(), vec![0; MAX_LINK as usize + 1]] {
            write(&shortcut, &contents);
            assert!(refresh_link(&shortcut, &expected, &artwork).is_err());
            assert_eq!(fs::read(&shortcut).unwrap(), contents);
        }
        fs::remove_file(&shortcut).unwrap();
        let actual = root.join("actual.lnk");
        link(&actual, &expected, "", &root);
        let before = fs::read(&actual).unwrap();
        fs::hard_link(&actual, &shortcut).unwrap();
        assert!(refresh_link(&shortcut, &expected, &artwork).is_err());
        assert_eq!(fs::read(&actual).unwrap(), before);
        fs::remove_file(&shortcut).unwrap();
        symlink_file(&actual, &shortcut)
            .expect("native shell tests require the existing Windows reparse fixture privilege");
        assert!(refresh_link(&shortcut, &expected, &artwork).is_err());
        assert_eq!(fs::read(&actual).unwrap(), before);
        fs::remove_file(&shortcut).unwrap();
        let alias = root.join("alias");
        symlink_dir(&root, &alias).unwrap();
        assert!(refresh_link(&alias.join("actual.lnk"), &expected, &artwork).is_err());
        assert_eq!(fs::read(&actual).unwrap(), before);
    }

    #[test]
    fn native_shell_artwork_is_bounded_private_and_content_addressed() {
        use std::os::windows::fs::symlink_dir;
        let (_temporary, root) = fixture();
        let directory = root.join("artwork");
        let bytes = b"\0\0\x01\0\x01\0ordinary icon fixture";
        let first = materialize_icon(&directory, bytes).unwrap();
        assert_eq!(
            first.file_name().unwrap(),
            OsStr::new(&format!("{:x}.ico", sha2::Sha256::digest(bytes)))
        );
        assert_eq!(fs::read(&first).unwrap(), bytes);
        assert_eq!(materialize_icon(&directory, bytes).unwrap(), first);
        assert!(materialize_icon(&directory, b"not an ICO").is_err());
        assert!(materialize_icon(&directory, &vec![0; MAX_ICON + 1]).is_err());
        let alias = root.join("alias");
        symlink_dir(&directory, &alias)
            .expect("native shell tests require the existing Windows reparse fixture privilege");
        assert!(materialize_icon(&alias, bytes).is_err());
        let hard_link = root.join("another-icon");
        fs::hard_link(&first, &hard_link).unwrap();
        assert!(materialize_icon(&directory, bytes).is_err());
        assert_eq!(fs::read(&hard_link).unwrap(), bytes);
    }

    #[test]
    fn native_shell_mutation_fence_refuses_live_or_incomplete_journals_and_busy_lock() {
        let (_temporary, root) = fixture();
        let sentinel = root.join("shell-metadata");
        write(&sentinel, b"unchanged shell metadata");
        let owner = mutation_lock(&root).unwrap();
        assert!(matches!(mutation_lock(&root), Err(InstallError::Busy)));
        fs2::FileExt::unlock(&owner).unwrap();
        drop(owner);
        for name in [MANUAL_INTENT, UNINSTALL_INTENT] {
            // Any journal fences cosmetics, independent of live/completed or
            // partial syntax; the normal authenticated lifecycle owns recovery.
            for bytes in [
                b"incomplete".as_slice(),
                br#"{"owner_pid":1,"owner_created":1}"#,
            ] {
                write(&root.join(name), bytes);
                assert!(matches!(mutation_lock(&root), Err(InstallError::Busy)));
                assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged shell metadata");
                fs::remove_file(root.join(name)).unwrap();
            }
        }
        drop(mutation_lock(&root).unwrap());
    }

    struct RegistryFixture {
        path: Vec<u16>,
        key: Option<Key>,
    }
    impl RegistryFixture {
        fn new(root: &Path) -> Self {
            let path = wide_text(&format!(
                "Software\\MapleAgentIntegrationTests\\{}",
                rand_name()
            ));
            let mut key = std::ptr::null_mut();
            assert_eq!(
                unsafe {
                    RegCreateKeyExW(
                        HKEY_CURRENT_USER,
                        path.as_ptr(),
                        0,
                        std::ptr::null(),
                        REG_OPTION_NON_VOLATILE,
                        KEY_ALL_ACCESS | KEY_WOW64_32KEY,
                        std::ptr::null(),
                        &mut key,
                        std::ptr::null_mut(),
                    )
                },
                ERROR_SUCCESS
            );
            let fixture = Self {
                path,
                key: Some(Key(key)),
            };
            fixture.set("InstallLocation", &root.to_string_lossy());
            fixture.set("DisplayIcon", "old icon");
            fixture
        }
        fn set(&self, name: &str, value: &str) {
            let bytes = wide_text(value)
                .iter()
                .flat_map(|unit| unit.to_le_bytes())
                .collect::<Vec<_>>();
            assert_eq!(
                unsafe {
                    RegSetValueExW(
                        self.key.as_ref().unwrap().0,
                        wide_text(name).as_ptr(),
                        0,
                        REG_SZ,
                        bytes.as_ptr(),
                        bytes.len() as u32,
                    )
                },
                ERROR_SUCCESS
            );
        }
        fn path(&self) -> String {
            String::from_utf16(&self.path[..self.path.len() - 1]).unwrap()
        }
    }
    impl Drop for RegistryFixture {
        fn drop(&mut self) {
            drop(self.key.take());
            unsafe { RegDeleteKeyExW(HKEY_CURRENT_USER, self.path.as_ptr(), KEY_WOW64_32KEY, 0) };
        }
    }

    fn rand_name() -> String {
        tempfile::Builder::new()
            .prefix("registry-")
            .tempdir()
            .unwrap()
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn native_shell_registry_updates_only_existing_exact_registered_root() {
        let (_temporary, root) = fixture();
        let root = root.canonicalize().unwrap();
        let artwork = icon(&root);
        let registry = RegistryFixture::new(&root);
        assert!(refresh_display_icon(&registry.path(), &root, &artwork).unwrap());
        let actual = registry_text(registry.key.as_ref().unwrap().0, "DisplayIcon")
            .unwrap()
            .unwrap();
        assert_eq!(actual, wide_text(&format!("\"{}\",0", artwork.display())));
        assert!(!refresh_display_icon(&registry.path(), &root, &artwork).unwrap());
        let other = root.join("foreign");
        windows_fs::private_directory(&other).unwrap();
        registry.set("InstallLocation", &other.to_string_lossy());
        assert!(refresh_display_icon(&registry.path(), &root, &artwork).is_err());
        assert_eq!(
            registry_text(registry.key.as_ref().unwrap().0, "DisplayIcon").unwrap(),
            Some(actual)
        );
        let path = registry.path();
        drop(registry);
        assert!(refresh_display_icon(&path, &root, &artwork).is_err());
        let mut key = std::ptr::null_mut();
        assert_eq!(
            unsafe {
                RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    wide_text(&path).as_ptr(),
                    0,
                    KEY_QUERY_VALUE | KEY_WOW64_32KEY,
                    &mut key,
                )
            },
            2
        );
    }
}
