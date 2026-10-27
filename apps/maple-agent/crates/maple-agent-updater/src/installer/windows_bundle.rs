//! Windows uses immutable version directories behind a stable signed launcher.
//! Committing current.json never replaces a running executable. Retained old
//! generations remain usable through interruptions and are deleted only after
//! the new application's authenticated startup checkpoint.
#![cfg_attr(not(windows), allow(dead_code))]

use super::*;
use crate::{Url, manifest};
use std::io::{Seek, SeekFrom};

#[cfg(windows)]
mod authenticode;
#[cfg(windows)]
mod windows_fs;

const ROOT: &str = "maple-agent-windows";
const DESCRIPTOR: &str = "bundle-info.json";
const POINTER: &str = "current.json";
const MANUAL_INTENT: &str = ".installer-intent.json";
const UNINSTALL_INTENT: &str = ".uninstaller-intent.json";
const TRANSACTION_TEMP_PREFIX: &str = ".maple-transaction-";
const POINTER_FORMAT: &str = "maple-agent-windows-current-v1";
const RECEIPT_FORMAT: &str = "maple-agent-windows-install-v1";
const DESCRIPTOR_LIMIT: u64 = 16 * 1024;
const RECEIPT_LIMIT: u64 = (manifest::MAX_ENVELOPE_BYTES * 6 + 16 * 1024) as u64;
const FILES: &[&str] = &[
    "maple-agent.exe",
    "LICENSE.txt",
    "build-info.json",
    DESCRIPTOR,
];
const DIRS: &[&str] = &[""];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Bundle {
    format: String,
    product: String,
    profile: String,
    version: String,
    build: String,
    source_sha: String,
    bundle_id: String,
    entrypoint: String,
}

fn profile_identity(profile: &str) -> Result<&'static str> {
    match profile {
        "prod" => Ok("cloud.opensecret.maple.agent"),
        "dev" => Ok("cloud.opensecret.maple.agent.dev"),
        _ => Err(InstallError::Invalid("invalid Windows bundle profile")),
    }
}

impl Bundle {
    fn parse(bytes: &[u8], profile: &str) -> Result<Self> {
        if bytes.is_empty() || bytes.len() as u64 > DESCRIPTOR_LIMIT {
            return Err(InstallError::Invalid("Windows bundle descriptor size"));
        }
        let bundle: Self = serde_json::from_slice(bytes)
            .map_err(|_| InstallError::Invalid("invalid Windows bundle descriptor"))?;
        if bundle.format != "maple-agent-windows-bundle-v2"
            || bundle.product != "maple-agent"
            || bundle.profile != profile
            || bundle.bundle_id != profile_identity(profile)?
            || bundle.entrypoint != "maple-agent.exe"
            || !valid_build(&bundle.build)
            || !valid_source(&bundle.source_sha)
            || manifest::stable_version(&bundle.version).is_err()
        {
            return Err(InstallError::Invalid("Windows bundle identity changed"));
        }
        Ok(bundle)
    }

    fn generation(&self) -> String {
        format!("{}-{}", self.version, self.build)
    }

    fn matches_release(&self, release: &crate::VerifiedRelease) -> Result<()> {
        if self.profile != "prod"
            || self.version != release.manifest.version
            || self.build != release.manifest.build
            || self.source_sha != release.manifest.source_sha
        {
            return Err(InstallError::Invalid(
                "Windows bundle differs from signed release",
            ));
        }
        Ok(())
    }
}

fn valid_build(build: &str) -> bool {
    !build.is_empty()
        && build.len() <= 18
        && !build.starts_with('0')
        && build.bytes().all(|b| b.is_ascii_digit())
}
fn valid_source(source: &str) -> bool {
    source.len() == 40
        && source
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_generation(generation: &str) -> bool {
    generation.len() <= 84
        && generation.rsplit_once('-').is_some_and(|(version, build)| {
            manifest::stable_version(version).is_ok() && valid_build(build)
        })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Current {
    format: String,
    generation: String,
}
impl Current {
    fn new(bundle: &Bundle) -> Self {
        Self {
            format: POINTER_FORMAT.into(),
            generation: bundle.generation(),
        }
    }
    fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > 1024 {
            return Err(InstallError::Invalid("Windows pointer size"));
        }
        let pointer: Self = serde_json::from_slice(bytes)
            .map_err(|_| InstallError::Invalid("invalid Windows pointer"))?;
        if pointer.format != POINTER_FORMAT || !valid_generation(&pointer.generation) {
            return Err(InstallError::Invalid("Windows pointer identity changed"));
        }
        Ok(pointer)
    }
    fn path(&self, root: &Path) -> PathBuf {
        root.join("versions").join(&self.generation)
    }
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    #[cfg(windows)]
    {
        windows_fs::read_bounded(path, limit)?
            .ok_or_else(|| InstallError::Io(io::Error::from(io::ErrorKind::NotFound)))
    }
    #[cfg(not(windows))]
    {
        let file = open_regular(path, false, false)?;
        if file.metadata()?.len() > limit {
            return Err(InstallError::Invalid("Windows metadata size"));
        }
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(InstallError::Invalid("Windows metadata grew"));
        }
        Ok(bytes)
    }
}

fn archive(
    payload: &Path,
    output: Option<&Path>,
    profile: &str,
) -> Result<(BTreeMap<String, Node>, Bundle)> {
    // Inspect the entire namespace before creating any files. Only fixed plain
    // names are admitted, excluding ADS, case aliases, links and extra members.
    let (nodes, descriptor) = scan_archive_root(payload, None, ROOT)?;
    validate_inventory(&nodes)?;
    let bundle = Bundle::parse(
        &descriptor.ok_or(InstallError::Invalid("missing Windows bundle descriptor"))?,
        profile,
    )?;
    if let Some(output) = output {
        let extracted = scan_archive_root(payload, Some(output), ROOT)?;
        if extracted.0 != nodes {
            return Err(InstallError::Invalid(
                "Windows archive changed during extraction",
            ));
        }
    }
    Ok((nodes, bundle))
}

fn validate_inventory(nodes: &BTreeMap<String, Node>) -> Result<()> {
    let expected: BTreeSet<String> = FILES
        .iter()
        .map(|file| format!("{ROOT}/{file}"))
        .chain(DIRS.iter().map(|dir| {
            if dir.is_empty() {
                ROOT.into()
            } else {
                format!("{ROOT}/{dir}")
            }
        }))
        .collect();
    if nodes.keys().cloned().collect::<BTreeSet<_>>() != expected {
        return Err(InstallError::Invalid("Windows bundle inventory differs"));
    }
    for file in FILES {
        if !matches!(
            nodes.get(&format!("{ROOT}/{file}")),
            Some(Node::File { mode: 0o644, .. })
        ) {
            return Err(InstallError::Invalid(
                "Windows bundle requires regular files",
            ));
        }
    }
    for dir in DIRS {
        let path = if dir.is_empty() {
            ROOT.into()
        } else {
            format!("{ROOT}/{dir}")
        };
        if !matches!(nodes.get(&path), Some(Node::Directory)) {
            return Err(InstallError::Invalid(
                "Windows bundle requires ordinary directories",
            ));
        }
    }
    Ok(())
}

/// Data-only bounded archive verification, shared with release qualification.
pub fn validate_windows_bundle_archive(payload: &Path, profile: &str) -> Result<()> {
    let _ = archive(payload, None, profile)?;
    Ok(())
}

fn checked_entry(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(InstallError::Invalid("Windows path is a link"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(InstallError::Invalid("Windows path is a reparse point"));
        }
        windows_fs::validate_path(path)?;
        if metadata.is_dir() {
            windows_fs::private_directory(path)?;
        }
    }
    Ok(metadata)
}

fn scan_generation(directory: &Path) -> Result<BTreeMap<String, Node>> {
    let mut nodes = BTreeMap::new();
    let mut pending = vec![(directory.to_owned(), ROOT.to_owned())];
    let mut total = 0u64;
    while let Some((path, name)) = pending.pop() {
        if nodes.len() >= 32 {
            return Err(InstallError::Invalid(
                "unexpected Windows generation entries",
            ));
        }
        let metadata = checked_entry(&path)?;
        let node = if metadata.is_dir() {
            for entry in fs::read_dir(&path)?.take(33) {
                let entry = entry?;
                let leaf = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| InstallError::Invalid("Windows filename encoding"))?;
                if leaf.is_empty()
                    || !leaf
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                {
                    return Err(InstallError::Invalid("unexpected Windows filename"));
                }
                pending.push((entry.path(), format!("{name}/{leaf}")));
            }
            Node::Directory
        } else if metadata.is_file() {
            #[cfg(unix)]
            if metadata.nlink() != 1 {
                return Err(InstallError::Invalid("Windows payload is hard-linked"));
            }
            total = total
                .checked_add(metadata.len())
                .ok_or(InstallError::Invalid("Windows size overflow"))?;
            if metadata.len() > MAX_FILE_BYTES || total > MAX_EXPANDED_BYTES {
                return Err(InstallError::Invalid("Windows generation size"));
            }
            let mut file = open_regular(&path, false, false)?;
            Node::File {
                mode: 0o644,
                size: metadata.len(),
                digest: hash_reader(&mut file, metadata.len())?,
            }
        } else {
            return Err(InstallError::Invalid("Windows payload has a special file"));
        };
        if nodes.insert(name, node).is_some() {
            return Err(InstallError::Invalid("duplicate Windows tree entry"));
        }
    }
    validate_inventory(&nodes)?;
    Ok(nodes)
}

fn descriptor(directory: &Path, profile: &str) -> Result<Bundle> {
    Bundle::parse(
        &read_bounded(&directory.join(DESCRIPTOR), DESCRIPTOR_LIMIT)?,
        profile,
    )
}

// Read the signed .maple PE section without executing downloaded code. Windows
// Authenticode validates the executable; metadata must additionally identify
// this product/profile/version/source rather than any other Maple-signed app.
fn pe_build_info(path: &Path) -> Result<serde_json::Value> {
    let mut file = open_regular(path, false, false)?;
    let file_size = file.metadata()?.len();
    let mut read = |offset: u64, length: usize| -> Result<Vec<u8>> {
        if offset
            .checked_add(length as u64)
            .is_none_or(|end| end > file_size)
        {
            return Err(InstallError::Invalid("truncated Windows PE"));
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    };
    let dos = read(0, 64)?;
    if &dos[..2] != b"MZ" {
        return Err(InstallError::CodeSignature);
    }
    let position = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
    if !(64..=1024 * 1024).contains(&position) {
        return Err(InstallError::CodeSignature);
    }
    let header = read(position, 24)?;
    let u16at = |bytes: &[u8], n: usize| u16::from_le_bytes(bytes[n..n + 2].try_into().unwrap());
    let u32at = |bytes: &[u8], n: usize| u32::from_le_bytes(bytes[n..n + 4].try_into().unwrap());
    let count = usize::from(u16at(&header, 6));
    let optional_size = usize::from(u16at(&header, 20));
    let flags = u16at(&header, 22);
    if &header[..4] != b"PE\0\0"
        || u16at(&header, 4) != 0x8664
        || !(1..=96).contains(&count)
        || !(112..=4096).contains(&optional_size)
        || flags & 2 == 0
        || flags & 0x2000 != 0
    {
        return Err(InstallError::CodeSignature);
    }
    let optional = read(position + 24, optional_size)?;
    if u16at(&optional, 0) != 0x20b {
        return Err(InstallError::CodeSignature);
    }
    let headers_size = u32at(&optional, 60) as u64;
    let table_offset = position + 24 + optional_size as u64;
    if table_offset + count as u64 * 40 > headers_size
        || headers_size > 1024 * 1024
        || headers_size > file_size
    {
        return Err(InstallError::CodeSignature);
    }
    let table = read(table_offset, count * 40)?;
    let mut found = None;
    let mut ranges = Vec::new();
    for section in table.as_chunks::<40>().0 {
        let size = u32at(section, 16) as u64;
        let offset = u32at(section, 20) as u64;
        if size > 0 {
            if offset < headers_size
                || offset.checked_add(size).is_none_or(|end| end > file_size)
                || ranges
                    .iter()
                    .any(|(start, end)| offset < *end && *start < offset + size)
            {
                return Err(InstallError::CodeSignature);
            }
            ranges.push((offset, offset + size));
        }
        if &section[..8] != b".maple\0\0" {
            continue;
        }
        if found.is_some() {
            return Err(InstallError::CodeSignature);
        }
        let length = u32at(section, 8) as usize;
        let flags = u32at(section, 36);
        if length == 0
            || length > DESCRIPTOR_LIMIT as usize
            || size < length as u64
            || size > DESCRIPTOR_LIMIT + 65536
            || flags & 0x40000040 != 0x40000040
            || flags & 0xa0000000 != 0
        {
            return Err(InstallError::CodeSignature);
        }
        let bytes = read(offset, size as usize)?;
        if bytes[length..].iter().any(|b| *b != 0) {
            return Err(InstallError::CodeSignature);
        }
        found = Some(
            serde_json::from_slice(&bytes[..length]).map_err(|_| InstallError::CodeSignature)?,
        );
    }
    found.ok_or(InstallError::CodeSignature)
}

#[derive(Clone, Copy)]
enum IdentityPurpose {
    Admission,
    Startup,
}

fn verify_identity(directory: &Path, bundle: &Bundle) -> Result<()> {
    verify_identity_for(directory, bundle, IdentityPurpose::Admission)
}

fn verify_identity_metadata(directory: &Path, bundle: &Bundle) -> Result<PathBuf> {
    let info: serde_json::Value = serde_json::from_slice(&read_bounded(
        &directory.join("build-info.json"),
        DESCRIPTOR_LIMIT,
    )?)
    .map_err(|_| InstallError::Invalid("invalid Windows build metadata"))?;
    let executable = directory.join("maple-agent.exe");
    let embedded = pe_build_info(&executable)?;
    if embedded != info
        || info["profile"] != bundle.profile
        || info["bundle_id"] != bundle.bundle_id
        || info["version"] != bundle.version
        || info["source_sha"] != bundle.source_sha
        || info["update_feed_url"]
            != if bundle.profile == "prod" {
                serde_json::json!("https://updates.maple.ai/agent/stable/windows/latest.json")
            } else {
                serde_json::Value::Null
            }
    {
        return Err(InstallError::CodeSignature);
    }
    Ok(executable)
}

fn verify_identity_for(directory: &Path, bundle: &Bundle, purpose: IdentityPurpose) -> Result<()> {
    let executable = verify_identity_metadata(directory, bundle)?;
    #[cfg(windows)]
    if !match purpose {
        IdentityPurpose::Admission => {
            authenticode::verify_or_unsigned(&executable, bundle.profile == "dev")
        }
        IdentityPurpose::Startup => {
            authenticode::verify_startup_or_unsigned(&executable, bundle.profile == "dev")
        }
    } {
        return Err(InstallError::CodeSignature);
    }
    #[cfg(not(windows))]
    let _ = (purpose, executable);
    #[cfg(not(windows))]
    {
        Err(InstallError::NotifyOnly(
            "native Windows signature verification requires Windows",
        ))
    }
    #[cfg(windows)]
    Ok(())
}

fn generation(root: &Path, profile: &str) -> Result<(Current, PathBuf, Bundle)> {
    generation_for(root, profile, IdentityPurpose::Admission)
}

fn generation_for(
    root: &Path,
    profile: &str,
    purpose: IdentityPurpose,
) -> Result<(Current, PathBuf, Bundle)> {
    let current = Current::parse(&read_bounded(&root.join(POINTER), 1024)?)?;
    let path = current.path(root);
    let bundle = descriptor(&path, profile)?;
    if bundle.generation() != current.generation {
        return Err(InstallError::Invalid(
            "Windows generation differs from pointer",
        ));
    }
    let _ = scan_generation(&path)?;
    verify_identity_for(&path, &bundle, purpose)?;
    Ok((current, path, bundle))
}

fn expected_artifact(update: &PreparedUpdate) -> Result<&Artifact> {
    manifest::validate_manifest(&update.release.manifest)
        .map_err(|_| InstallError::Invalid("Windows manifest fields changed"))?;
    update
        .release
        .artifact(Target::WindowsX86_64)
        .map_err(|_| InstallError::Invalid("missing Windows updater bundle"))
}

#[cfg(windows)]
fn registered_root(root: &Path, profile: &str) -> Result<()> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::*;
    let key: Vec<u16> = format!(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{}",
        profile_identity(profile)?
    )
    .encode_utf16()
    .chain([0])
    .collect();
    let mut handle = std::ptr::null_mut();
    if unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            0,
            KEY_QUERY_VALUE | KEY_WOW64_32KEY,
            &mut handle,
        )
    } != ERROR_SUCCESS
    {
        return Err(InstallError::NotifyOnly(
            "run the registered per-user Windows installation",
        ));
    }
    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
    let handle = Key(handle);
    let name: Vec<u16> = "InstallLocation".encode_utf16().chain([0]).collect();
    let mut kind = 0;
    let mut length = 0;
    if unsafe {
        RegQueryValueExW(
            handle.0,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            std::ptr::null_mut(),
            &mut length,
        )
    } != ERROR_SUCCESS
        || kind != REG_SZ
        || !(4..=65536).contains(&length)
        || length % 2 != 0
    {
        return Err(InstallError::Invalid(
            "Windows install registration changed",
        ));
    }
    let mut value = vec![0u16; length as usize / 2];
    if unsafe {
        RegQueryValueExW(
            handle.0,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            value.as_mut_ptr().cast(),
            &mut length,
        )
    } != ERROR_SUCCESS
        || kind != REG_SZ
        || value.last() != Some(&0)
        || value[..value.len() - 1].contains(&0)
    {
        return Err(InstallError::Invalid(
            "Windows install registration changed",
        ));
    }
    let path = PathBuf::from(std::ffi::OsString::from_wide(&value[..value.len() - 1]));
    if path.canonicalize()? != root {
        return Err(InstallError::Invalid(
            "Windows registration belongs to another installation",
        ));
    }
    Ok(())
}

pub(super) fn validate_destination(root: &Path) -> Result<()> {
    validate_destination_for(root, IdentityPurpose::Admission)
}

fn validate_destination_for(root: &Path, purpose: IdentityPurpose) -> Result<()> {
    #[cfg(windows)]
    {
        windows_fs::private_directory(root)?;
        if root.join(MANUAL_INTENT).try_exists()?
            && let Ok((_, _, current)) = generation_for(root, "prod", IdentityPurpose::Startup)
        {
            recover_completed_manual_intent(root, &current)?;
        }
        if root.join(MANUAL_INTENT).try_exists()? || root.join(UNINSTALL_INTENT).try_exists()? {
            return Err(InstallError::Busy);
        }
        windows_fs::private_directory(&root.join("versions"))?;
        if !checked_entry(root)?.is_dir() || !checked_entry(&root.join("versions"))?.is_dir() {
            return Err(InstallError::Invalid(
                "Windows installation directories changed",
            ));
        }
        registered_root(root, "prod")?;
        let bootstrap = pe_build_info(&root.join("maple-agent.exe"))?;
        if bootstrap["profile"] != "prod"
            || bootstrap["bundle_id"] != profile_identity("prod")?
            || !match purpose {
                IdentityPurpose::Admission => authenticode::verify(&root.join("maple-agent.exe")),
                IdentityPurpose::Startup => {
                    authenticode::verify_startup_or_unsigned(&root.join("maple-agent.exe"), false)
                }
            }
        {
            return Err(InstallError::CodeSignature);
        }
        let _ = generation_for(root, "prod", purpose)?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (root, purpose);
        Err(InstallError::NotifyOnly(
            "Windows installation requires native Windows",
        ))
    }
}

/// Resolve only a root bootstrap or a registered generation, never an arbitrary
/// path supplied by a pointer. This is called before normal app/host startup.
pub fn resolve_windows_launcher(executable: &Path, profile: &str) -> Result<Option<PathBuf>> {
    #[cfg(windows)]
    {
        let executable = executable.canonicalize()?;
        checked_entry(&executable)?;
        let parent = executable
            .parent()
            .ok_or(InstallError::Invalid("Windows executable parent"))?;
        if executable
            .file_name()
            .is_none_or(|leaf| leaf != "maple-agent.exe")
        {
            return Ok(None);
        }
        // A generation executes normally. It is still checked against its
        // descriptor; old generations may remain alive during pointer commit.
        if let Some(versions) = parent
            .parent()
            .filter(|p| p.file_name().is_some_and(|n| n == "versions"))
        {
            if !parent
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(valid_generation)
            {
                return Err(InstallError::Invalid("invalid Windows generation path"));
            }
            let root = versions
                .parent()
                .ok_or(InstallError::Invalid("missing Windows root"))?;
            windows_fs::private_directory(root)?;
            registered_root(root, profile)?;
            if live_uninstall_owner(root, profile)? {
                return Err(InstallError::Busy);
            }
            let pointer = Current::parse(&read_bounded(&root.join(POINTER), 1024)?)?;
            if pointer.path(root) != parent {
                return Err(InstallError::Invalid(
                    "inactive Windows generation cannot start",
                ));
            }
            let bundle = descriptor(parent, profile)?;
            if bundle.generation() != parent.file_name().unwrap().to_string_lossy() {
                return Err(InstallError::Invalid("Windows generation identity changed"));
            }
            let _ = scan_generation(parent)?;
            verify_identity_for(parent, &bundle, IdentityPurpose::Startup)?;
            recover_completed_manual_intent(root, &bundle)?;
            return Ok(None);
        }
        if !parent.join(POINTER).try_exists()? {
            if parent.join("versions").try_exists()? || registered_root(parent, profile).is_ok() {
                return Err(InstallError::Invalid(
                    "installed Windows pointer is missing; retry the installer",
                ));
            }
            return Err(InstallError::NotifyOnly(
                "run the registered Windows installation; reinstall to restore its pointer",
            ));
        }
        windows_fs::private_directory(parent)?;
        registered_root(parent, profile)?;
        if live_uninstall_owner(parent, profile)? {
            return Err(InstallError::Busy);
        }
        let bootstrap = pe_build_info(&executable)?;
        if bootstrap["profile"] != profile
            || bootstrap["bundle_id"] != profile_identity(profile)?
            || !authenticode::verify_startup_or_unsigned(&executable, profile == "dev")
        {
            return Err(InstallError::CodeSignature);
        }
        let (_, path, bundle) = generation_for(parent, profile, IdentityPurpose::Startup)?;
        recover_completed_manual_intent(parent, &bundle)?;
        // Deliberately construct from validated fixed segments, not JSON paths.
        Ok(Some(path.join("maple-agent.exe")))
    }
    #[cfg(not(windows))]
    {
        let _ = (executable, profile);
        Ok(None)
    }
}

pub fn discover_windows_installation(
    executable: &Path,
    version: &str,
    source_sha: &str,
) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let executable = executable.canonicalize()?;
        let directory = executable
            .parent()
            .ok_or(InstallError::Invalid("missing Windows generation"))?;
        let versions = directory
            .parent()
            .filter(|p| p.file_name().is_some_and(|name| name == "versions"))
            .ok_or(InstallError::NotifyOnly(
                "run the installed Windows launcher",
            ))?;
        let root = versions
            .parent()
            .ok_or(InstallError::Invalid("missing Windows installation"))?;
        validate_destination_for(root, IdentityPurpose::Startup)?;
        let (_, current, bundle) = generation_for(root, "prod", IdentityPurpose::Startup)?;
        if current != directory || bundle.version != version || bundle.source_sha != source_sha {
            return Err(InstallError::Invalid(
                "Windows running identity differs from installed generation",
            ));
        }
        Ok(root.to_owned())
    }
    #[cfg(not(windows))]
    {
        let _ = (executable, version, source_sha);
        Err(InstallError::NotifyOnly(
            "Windows installation requires native Windows",
        ))
    }
}

pub(super) fn preflight(root: &Path) -> Result<()> {
    validate_destination(root)?;
    #[cfg(windows)]
    {
        let probe = tempfile::NamedTempFile::new_in(root)?;
        windows_fs::open_private(probe.path(), false)?.sync_all()?;
        probe.close()?;
    }
    let _lock = open_install_lock(root)?;
    Ok(())
}

fn stage_prefix(root: &Path) -> String {
    format!(
        "{STAGING_PREFIX}{}-",
        hex::encode(Sha256::digest(root.as_os_str().as_encoded_bytes()))
    )
}
fn validate_stage(root: &Path, stage: &Path) -> Result<()> {
    if stage.parent() != Some(root)
        || !stage
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.strip_prefix(&stage_prefix(root))
                    .is_some_and(|suffix| {
                        suffix.len() == STAGING_RANDOM_LENGTH
                            && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
                    })
            })
        || stage.canonicalize()? != stage
        || !checked_entry(stage)?.is_dir()
    {
        return Err(InstallError::Invalid("Windows staging directory changed"));
    }
    Ok(())
}

pub(super) fn prepare(update: &PreparedUpdate) -> Result<InstallPreparation> {
    preflight(&update.installed_path)?;
    let (_, _, current) = generation(&update.installed_path, "prod")?;
    if manifest::stable_version(&update.release.manifest.version)
        .map_err(|_| InstallError::Invalid("Windows proposed version"))?
        <= manifest::stable_version(&current.version)
            .map_err(|_| InstallError::Invalid("Windows current version"))?
    {
        return Err(InstallError::Invalid("Windows version is not newer"));
    }
    let artifact = expected_artifact(update)?;
    let temporary = tempfile::Builder::new()
        .prefix(&stage_prefix(&update.installed_path))
        .rand_bytes(STAGING_RANDOM_LENGTH)
        .tempdir_in(&update.installed_path)?;
    #[cfg(windows)]
    windows_fs::private_directory(temporary.path())?;
    let payload_path = temporary.path().join("payload");
    copy_verified_payload(&update.payload_path, &payload_path, artifact)?;
    let (expected, bundle) = archive(&payload_path, Some(temporary.path()), "prod")?;
    bundle.matches_release(&update.release)?;
    let staged_path = temporary.path().join(ROOT);
    if scan_generation(&staged_path)? != expected {
        return Err(InstallError::Invalid(
            "staged Windows bytes differ from signed archive",
        ));
    }
    verify_identity(&staged_path, &bundle)?;
    windows_sync_directory(temporary.path())?;
    let preparation = InstallPreparation {
        installed_path: update.installed_path.clone(),
        staging_dir: temporary.path().to_owned(),
        payload_path,
        staged_path,
        version: bundle.version,
        build: bundle.build,
        size: artifact.size,
        sha256: artifact.sha256.clone(),
    };
    let _ = temporary.keep();
    Ok(preparation)
}

pub(super) fn revalidate(prepared: &InstallPreparation, update: &PreparedUpdate) -> Result<()> {
    validate_destination(&update.installed_path)?;
    validate_stage(&update.installed_path, &prepared.staging_dir)?;
    let artifact = expected_artifact(update)?;
    if prepared.installed_path != update.installed_path
        || prepared.payload_path != prepared.staging_dir.join("payload")
        || prepared.staged_path != prepared.staging_dir.join(ROOT)
        || prepared.version != update.release.manifest.version
        || prepared.build != update.release.manifest.build
        || prepared.size != artifact.size
        || prepared.sha256 != artifact.sha256
        || prepared.staging_dir.join(RECEIPT_NAME).exists()
    {
        return Err(InstallError::Invalid("Windows preparation changed"));
    }
    let (_, _, current) = generation(&update.installed_path, "prod")?;
    if manifest::stable_version(&prepared.version)
        .map_err(|_| InstallError::Invalid("Windows prepared version"))?
        <= manifest::stable_version(&current.version)
            .map_err(|_| InstallError::Invalid("Windows current version"))?
    {
        return Err(InstallError::Invalid("Windows version is not newer"));
    }
    verify_payload(&prepared.payload_path, artifact)?;
    let (expected, bundle) = archive(&prepared.payload_path, None, "prod")?;
    bundle.matches_release(&update.release)?;
    if scan_generation(&prepared.staged_path)? != expected {
        return Err(InstallError::Invalid("Windows staged bytes changed"));
    }
    verify_identity(&prepared.staged_path, &bundle)
}

pub(super) fn discard(prepared: &InstallPreparation) -> Result<()> {
    validate_destination(&prepared.installed_path)?;
    validate_stage(&prepared.installed_path, &prepared.staging_dir)?;
    if prepared.payload_path != prepared.staging_dir.join("payload")
        || prepared.staged_path != prepared.staging_dir.join(ROOT)
        || prepared.staging_dir.join(RECEIPT_NAME).exists()
    {
        return Err(InstallError::Invalid(
            "cannot discard retained Windows install",
        ));
    }
    safe_remove_tree(&prepared.staging_dir)?;
    windows_sync_directory(&prepared.installed_path)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    format: String,
    installed: InstalledUpdate,
    envelope: String,
}

fn write_private_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| InstallError::Invalid("Windows transaction serialization"))?;
    let parent = path
        .parent()
        .ok_or(InstallError::Invalid("missing Windows transaction parent"))?;
    let mut temporary = tempfile::Builder::new()
        .prefix(TRANSACTION_TEMP_PREFIX)
        .rand_bytes(STAGING_RANDOM_LENGTH)
        .tempfile_in(parent)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    let temp = temporary.into_temp_path();
    windows_atomic_replace(&temp, path)?;
    // MoveFileEx consumed the original path. TempPath's best-effort cleanup is a no-op.
    Ok(())
}

pub(super) fn replace(
    prepared: &InstallPreparation,
    update: &PreparedUpdate,
) -> Result<InstalledUpdate> {
    revalidate(prepared, update)?;
    let (previous, old_path, _) = generation(&prepared.installed_path, "prod")?;
    let (_, bundle) = archive(&prepared.payload_path, None, "prod")?;
    let next = Current::new(&bundle);
    if next.generation == previous.generation {
        return Err(InstallError::Invalid("Windows generation already current"));
    }
    let installed = InstalledUpdate {
        installed_path: prepared.installed_path.clone(),
        staging_dir: prepared.staging_dir.clone(),
        backup_path: old_path,
        version: prepared.version.clone(),
        build: prepared.build.clone(),
        durability_warning: false,
    };
    let receipt = Receipt {
        format: RECEIPT_FORMAT.into(),
        installed: installed.clone(),
        envelope: update.release.envelope.clone(),
    };
    write_private_json(&prepared.staging_dir.join(RECEIPT_NAME), &receipt)?;
    commit_generation(
        &prepared.installed_path,
        &prepared.staged_path,
        &next,
        &prepared.staging_dir.join(RECEIPT_NAME),
        || Ok(()),
    )?;
    // Atomic pointer publication is the commit point. The prior generation is
    // untouched, and the running old executable remains valid until restart.
    Ok(installed)
}

// The same filesystem commit primitive is exercised by native interruption
// tests. Authentication happens immediately above this function in replace;
// no test verifier or runtime fault switch can bypass it in production.
fn commit_generation(
    root: &Path,
    staged: &Path,
    next: &Current,
    receipt_path: &Path,
    before_publish: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if !valid_generation(&next.generation) || next.format != POINTER_FORMAT {
        return Err(InstallError::Invalid("Windows commit pointer changed"));
    }
    let next_path = next.path(root);
    let reused = next_path.exists();
    let result = (|| -> Result<()> {
        if reused {
            if scan_generation(&next_path)? != scan_generation(staged)? {
                return Err(InstallError::Invalid("Windows generation collision"));
            }
        } else {
            fs::rename(staged, &next_path)?;
        }
        windows_sync_directory(&root.join("versions"))?;
        before_publish()?;
        write_private_json(&root.join(POINTER), next)
    })();
    if let Err(error) = result {
        // No pointer was committed on any reported failure. A failure to move
        // back leaves authenticated new bytes as an inactive generation; retry
        // compares every byte and can reuse it without touching the old one.
        if !reused && next_path.exists() {
            let _ = fs::rename(&next_path, staged);
        }
        let _ = fs::remove_file(receipt_path);
        return Err(error);
    }
    Ok(())
}

fn safe_remove_tree(root: &Path) -> Result<()> {
    let metadata = checked_entry(root)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(root)? {
            safe_remove_tree(&entry?.path())?;
        }
        fs::remove_dir(root)?;
    } else if metadata.is_file() {
        fs::remove_file(root)?;
    } else {
        return Err(InstallError::Invalid(
            "Windows removal encountered a special file",
        ));
    }
    Ok(())
}

/// Authenticate a new startup before deleting previous bytes or staged state.
pub fn cleanup_current_windows_bundle(
    root: &Path,
    version: &str,
    source: &str,
    origin: &Url,
    keys: &[String],
) -> Result<usize> {
    validate_destination(root)?;
    let _lock = acquire_install_lock(root)?;
    let (pointer, current_path, current) = generation(root, "prod")?;
    if current.version != version || current.source_sha != source {
        return Err(InstallError::Invalid("Windows startup identity changed"));
    }
    let mut cleaned = 0;
    for entry in fs::read_dir(root)?.take(4096) {
        let entry = entry?;
        let stage = entry.path();
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(&stage_prefix(root)))
            || validate_stage(root, &stage).is_err()
        {
            continue;
        }
        let Ok(bytes) = read_bounded(&stage.join(RECEIPT_NAME), RECEIPT_LIMIT) else {
            continue;
        };
        let Ok(receipt) = serde_json::from_slice::<Receipt>(&bytes) else {
            continue;
        };
        let authenticated = (|| -> Result<()> {
            if receipt.format != RECEIPT_FORMAT
                || receipt.installed.installed_path != root
                || receipt.installed.staging_dir != stage
                || receipt.installed.version != version
                || receipt.installed.build != current.build
                || receipt.installed.backup_path == current_path
                || receipt.installed.backup_path.parent() != Some(&root.join("versions"))
                || !receipt
                    .installed
                    .backup_path
                    .file_name()
                    .and_then(|x| x.to_str())
                    .is_some_and(valid_generation)
            {
                return Err(InstallError::Invalid("Windows retained receipt changed"));
            }
            let release = manifest::verify_envelope(receipt.envelope.as_bytes(), origin, keys)
                .map_err(|_| InstallError::CodeSignature)?;
            current.matches_release(&release)?;
            let artifact = release
                .artifact(Target::WindowsX86_64)
                .map_err(|_| InstallError::Invalid("Windows receipt target"))?;
            verify_payload(&stage.join("payload"), artifact)?;
            let (expected, bundle) = archive(&stage.join("payload"), None, "prod")?;
            bundle.matches_release(&release)?;
            if scan_generation(&current_path)? != expected
                || pointer.generation != bundle.generation()
            {
                return Err(InstallError::Invalid("Windows committed bytes differ"));
            }
            if receipt.installed.backup_path.exists() {
                let old = descriptor(&receipt.installed.backup_path, "prod")?;
                if old.generation()
                    != receipt
                        .installed
                        .backup_path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                {
                    return Err(InstallError::Invalid("Windows backup identity changed"));
                }
                let _ = scan_generation(&receipt.installed.backup_path)?;
                verify_identity(&receipt.installed.backup_path, &old)?;
                #[cfg(windows)]
                let guard = Some(windows_fs::open_executable_mutation_guard(
                    &receipt.installed.backup_path.join("maple-agent.exe"),
                )?);
                #[cfg(not(windows))]
                let guard = None;
                remove_generation_files(&receipt.installed.backup_path, guard)?;
            }
            safe_remove_tree(&stage)?;
            windows_sync_directory(root)?;
            Ok(())
        })();
        if authenticated.is_ok() {
            cleaned += 1;
        }
    }
    Ok(cleaned)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManualIntent {
    format: String,
    profile: String,
    generation: String,
    version: String,
    source_sha: String,
    executable_sha256: String,
    owner_pid: u32,
    owner_created: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UninstallIntent {
    format: String,
    profile: String,
    owner_pid: u32,
    owner_created: u64,
    generations: Vec<String>,
}

impl UninstallIntent {
    fn parse(bytes: &[u8], profile: &str) -> Result<Self> {
        let intent: Self = serde_json::from_slice(bytes)
            .map_err(|_| InstallError::Invalid("Windows uninstall transaction"))?;
        if intent.format != "maple-agent-windows-uninstall-intent-v1"
            || intent.profile != profile
            || intent.generations.len() > 256
            || intent
                .generations
                .iter()
                .any(|name| !valid_generation(name))
            || intent.generations.iter().collect::<BTreeSet<_>>().len() != intent.generations.len()
        {
            return Err(InstallError::Invalid(
                "Windows uninstall transaction identity",
            ));
        }
        Ok(intent)
    }
}

#[cfg(windows)]
fn live_uninstall_owner(root: &Path, profile: &str) -> Result<bool> {
    if !root.join(UNINSTALL_INTENT).try_exists()? {
        return Ok(false);
    }
    let intent =
        UninstallIntent::parse(&read_bounded(&root.join(UNINSTALL_INTENT), 65536)?, profile)?;
    Ok(process_created(intent.owner_pid)?.is_some_and(|created| created == intent.owner_created))
}

#[cfg(windows)]
fn trusted_installer(executable: &Path, profile: &str) -> Result<serde_json::Value> {
    let info = pe_build_info(executable)?;
    if info["profile"] != profile
        || info["bundle_id"] != profile_identity(profile)?
        || !info["version"]
            .as_str()
            .is_some_and(|v| manifest::stable_version(v).is_ok())
        || !info["source_sha"].as_str().is_some_and(valid_source)
        || !authenticode::verify_or_unsigned(executable, profile == "dev")
    {
        return Err(InstallError::CodeSignature);
    }
    Ok(info)
}

fn exact_file_digest(path: &Path) -> Result<String> {
    let mut file = open_regular(path, false, false)?;
    let size = file.metadata()?.len();
    Ok(hex::encode(hash_reader(&mut file, size)?))
}

#[cfg(windows)]
fn process_created(pid: u32) -> Result<Option<u64>> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, FILETIME},
        System::Threading::{
            GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    };
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            Ok(None)
        } else {
            Err(error.into())
        };
    }
    struct Handle(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    let process = Handle(process);
    let mut status = 0;
    if unsafe { GetExitCodeProcess(process.0, &mut status) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    if status != 259 {
        return Ok(None);
    }
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(process.0, &mut created, &mut exited, &mut kernel, &mut user) } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(Some(
        (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
    ))
}

#[cfg(windows)]
fn installer_owner() -> Result<(u32, u64)> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            Threading::GetCurrentProcessId,
        },
    };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error().into());
    }
    struct Handle(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    let snapshot = Handle(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let current = unsafe { GetCurrentProcessId() };
    let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    while more != 0 {
        if entry.th32ProcessID == current {
            let created = process_created(entry.th32ParentProcessID)?
                .ok_or(InstallError::Invalid("installer parent exited"))?;
            return Ok((entry.th32ParentProcessID, created));
        }
        more = unsafe { Process32NextW(snapshot.0, &mut entry) };
    }
    Err(InstallError::Invalid("installer parent unavailable"))
}

// The caller has already validated the actual current generation's signature,
// metadata and fixed tree. Only a completed matching intent can be removed;
// an interrupted pending install leaves the previous app launchable unchanged.
fn recover_completed_manual_intent(root: &Path, current: &Bundle) -> Result<()> {
    if !root.join(MANUAL_INTENT).try_exists()? {
        return Ok(());
    }
    let recovery = (|| -> Result<()> {
        let lock = open_install_lock(root)?;
        lock.try_lock_exclusive().map_err(|_| InstallError::Busy)?;
        let intent: ManualIntent =
            serde_json::from_slice(&read_bounded(&root.join(MANUAL_INTENT), DESCRIPTOR_LIMIT)?)
                .map_err(|_| InstallError::Invalid("Windows recovery intent"))?;
        #[cfg(windows)]
        if process_created(intent.owner_pid)?.is_some_and(|created| created == intent.owner_created)
        {
            return Err(InstallError::Busy);
        }
        let pointer = Current::parse(&read_bounded(&root.join(POINTER), 1024)?)?;
        if intent.format != "maple-agent-windows-installer-intent-v1"
            || intent.profile != current.profile
            || intent.generation != current.generation()
            || pointer.generation != current.generation()
            || intent.version != current.version
            || intent.source_sha != current.source_sha
            || intent.executable_sha256
                != exact_file_digest(&pointer.path(root).join("maple-agent.exe"))?
        {
            return Err(InstallError::Invalid(
                "Windows manual transaction is still pending",
            ));
        }
        #[cfg(windows)]
        retire_abandoned_uninstall(root, &current.profile, &current.generation())?;
        fs::remove_file(root.join(MANUAL_INTENT))?;
        windows_sync_directory(root)
    })();
    // Collection/repair failure cannot turn a still-valid committed generation
    // into a startup outage. Auto-update remains Busy while intent is retained.
    let _ = recovery;
    Ok(())
}

#[cfg(windows)]
fn generation_write_guards(root: &Path) -> Result<Vec<File>> {
    let mut guards = Vec::new();
    for entry in fs::read_dir(root.join("versions"))?.take(4096) {
        let entry = entry?;
        if !entry.file_name().to_str().is_some_and(valid_generation) {
            return Err(InstallError::Invalid("unknown Windows generation"));
        }
        known_partial_generation(&entry.path())?;
        let executable = entry.path().join("maple-agent.exe");
        if executable.try_exists()? {
            guards.push(windows_fs::open_private(&executable, false)?);
        }
    }
    Ok(guards)
}

// A partial generation is repairable only when every existing entry is one of
// our fixed regular files/directories. Never remove arbitrary files on retry.
fn known_partial_generation(path: &Path) -> Result<()> {
    let mut pending = vec![(path.to_owned(), String::new())];
    while let Some((path, name)) = pending.pop() {
        let metadata = checked_entry(&path)?;
        if metadata.is_dir() {
            if !DIRS.contains(&name.as_str()) {
                return Err(InstallError::Invalid("unknown Windows installer directory"));
            }
            #[cfg(windows)]
            windows_fs::private_directory(&path)?;
            for entry in fs::read_dir(&path)?.take(32) {
                let entry = entry?;
                let leaf = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| InstallError::Invalid("Windows installer filename"))?;
                let name = if name.is_empty() {
                    leaf
                } else {
                    format!("{name}/{leaf}")
                };
                pending.push((entry.path(), name));
            }
        } else if metadata.is_file() && FILES.contains(&name.as_str()) {
            // Cleanup may retain a delete-capable write guard on the exe.
            // A READ-only-sharing open would deny that existing write handle.
            // This existing-only read validates owner/DACL/link invariants,
            // permits concurrent guard writes, and works on the mapped current
            // executable during an authenticated startup checkpoint.
            #[cfg(windows)]
            let _ = windows_fs::open_shared_readonly(&path)?;
            #[cfg(not(windows))]
            let _ = open_regular(&path, false, false)?;
        } else {
            return Err(InstallError::Invalid("unknown Windows installer file"));
        }
    }
    Ok(())
}

// Revalidate every remaining path before deleting a known partial or complete
// generation. A delete-capable write guard prevents any new executable image
// mapping while permitting our own unlink; close it before removing directories.
fn remove_generation_files(path: &Path, guard: Option<File>) -> Result<()> {
    known_partial_generation(path)?;
    for name in FILES {
        let file = path.join(name);
        if file.try_exists()? {
            fs::remove_file(file)?;
        }
    }
    drop(guard);
    for name in DIRS.iter().rev() {
        let directory = path.join(name);
        if directory.try_exists()? {
            fs::remove_dir(directory)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn incomplete_generation_guard(path: &Path) -> Result<Option<File>> {
    known_partial_generation(path)?;
    let executable = path.join("maple-agent.exe");
    if executable.try_exists()? {
        Ok(Some(windows_fs::open_executable_mutation_guard(
            &executable,
        )?))
    } else {
        Ok(None)
    }
}

#[cfg(windows)]
fn complete_generation(path: &Path, profile: &str, name: &str) -> Result<()> {
    let bundle = descriptor(path, profile)?;
    if bundle.generation() != name {
        return Err(InstallError::Invalid("Windows generation identity changed"));
    }
    let _ = scan_generation(path)?;
    verify_identity(path, &bundle)
}

#[cfg(windows)]
fn structurally_complete_generation(path: &Path, profile: &str, name: &str) -> Result<()> {
    let bundle = descriptor(path, profile)?;
    if bundle.generation() != name {
        return Err(InstallError::Invalid(
            "Windows retained generation identity",
        ));
    }
    let _ = scan_generation(path)?;
    let _ = verify_identity_metadata(path, &bundle)?;
    // Retaining existing rollback bytes must not depend on a revocation
    // network response. This grants no launch/install authority: every later
    // use still performs its purpose-specific native signature verification.
    Ok(())
}

#[cfg(windows)]
fn abandoned_uninstall(root: &Path, profile: &str) -> Result<Option<UninstallIntent>> {
    if !root.join(UNINSTALL_INTENT).try_exists()? {
        return Ok(None);
    }
    let intent =
        UninstallIntent::parse(&read_bounded(&root.join(UNINSTALL_INTENT), 65536)?, profile)?;
    if process_created(intent.owner_pid)?.is_some_and(|created| created == intent.owner_created) {
        return Err(InstallError::Busy);
    }
    Ok(Some(intent))
}

// A dead uninstall journal can explain missing/partial old app bytes. It never
// authorizes arbitrary paths: all present children must be journal-owned, the
// exact current generation, or the separately recorded manual install target.
#[cfg(windows)]
fn validate_uninstall_repair(root: &Path, profile: &str, intent: &UninstallIntent) -> Result<()> {
    let current = if root.join(POINTER).try_exists()? {
        Some(Current::parse(&read_bounded(&root.join(POINTER), 1024)?)?)
    } else {
        None
    };
    let manual = if root.join(MANUAL_INTENT).try_exists()? {
        let manual: ManualIntent =
            serde_json::from_slice(&read_bounded(&root.join(MANUAL_INTENT), DESCRIPTOR_LIMIT)?)
                .map_err(|_| InstallError::Invalid("Windows repair manual intent"))?;
        if manual.format != "maple-agent-windows-installer-intent-v1"
            || manual.profile != profile
            || !valid_generation(&manual.generation)
        {
            return Err(InstallError::Invalid("Windows repair manual identity"));
        }
        Some(manual.generation)
    } else {
        None
    };
    if let Some(current) = &current
        && !intent.generations.contains(&current.generation)
        && generation_for(root, profile, IdentityPurpose::Startup).is_err()
    {
        return Err(InstallError::Invalid(
            "Windows missing generation is not in uninstall journal",
        ));
    }
    let versions = root.join("versions");
    if versions.try_exists()? {
        windows_fs::private_directory(&versions)?;
        for (index, entry) in fs::read_dir(&versions)?.enumerate() {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| InstallError::Invalid("Windows repair generation name"))?;
            if index >= 256
                || !valid_generation(&name)
                || !(intent.generations.contains(&name)
                    || current
                        .as_ref()
                        .is_some_and(|pointer| pointer.generation == name)
                    || manual
                        .as_ref()
                        .is_some_and(|generation| *generation == name))
            {
                return Err(InstallError::Invalid(
                    "unknown Windows uninstall repair generation",
                ));
            }
            known_partial_generation(&entry.path())?;
        }
    }
    Ok(())
}

// Called under the install lock after a complete new current generation is
// authenticated. Valid rollback generations are retained; only old journal-
// owned incomplete fixed-file trees are removed. The journal survives failure.
#[cfg(windows)]
fn retire_abandoned_uninstall(root: &Path, profile: &str, current_generation: &str) -> Result<()> {
    let Some(intent) = abandoned_uninstall(root, profile)? else {
        return Ok(());
    };
    validate_uninstall_repair(root, profile, &intent)?;
    for name in &intent.generations {
        if name == current_generation {
            continue;
        }
        let path = root.join("versions").join(name);
        if path.try_exists()? && structurally_complete_generation(&path, profile, name).is_err() {
            let guard = incomplete_generation_guard(&path)?;
            remove_generation_files(&path, guard)?;
        }
    }
    fs::remove_file(root.join(UNINSTALL_INTENT))?;
    windows_sync_directory(root)
}

#[cfg(windows)]
fn inspect_manual_root(root: &Path, profile: &str) -> Result<()> {
    windows_fs::private_directory(root)?;
    if live_uninstall_owner(root, profile)? {
        return Err(InstallError::Busy);
    }
    for entry in fs::read_dir(root)?.take(4096) {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or(InstallError::Invalid("Windows root filename"))?;
        if matches!(
            name,
            "maple-agent.exe"
                | "uninstall.exe"
                | POINTER
                | MANUAL_INTENT
                | UNINSTALL_INTENT
                | ".install.lock"
        ) || name
            .strip_prefix(TRANSACTION_TEMP_PREFIX)
            .is_some_and(|suffix| {
                suffix.len() == STAGING_RANDOM_LENGTH
                    && suffix.bytes().all(|c| c.is_ascii_alphanumeric())
            })
        {
            if !checked_entry(&entry.path())?.is_file() {
                return Err(InstallError::Invalid("Windows root special file"));
            }
            if name == ".install.lock" {
                let _ = windows_fs::open_private(&entry.path(), false)?;
            } else {
                let _ = open_regular(&entry.path(), false, false)?;
            }
        } else if name == "versions" {
            windows_fs::private_directory(&entry.path())?;
        } else if name.starts_with(&stage_prefix(root)) {
            validate_stage(root, &entry.path())?;
        } else {
            return Err(InstallError::Invalid(
                "unknown files in Windows installation; choose an empty directory",
            ));
        }
    }
    if root.join("maple-agent.exe").try_exists()? {
        let _ = trusted_installer(&root.join("maple-agent.exe"), profile)?;
    }
    if let Some(intent) = abandoned_uninstall(root, profile)? {
        validate_uninstall_repair(root, profile, &intent)?;
        return Ok(());
    }
    if root.join(POINTER).try_exists()? {
        registered_root(root, profile)?;
        let _ = generation(root, profile)?;
    } else if !root.join(MANUAL_INTENT).try_exists()? {
        // Creation of the private lock/versions directory can itself be
        // interrupted before the intent's atomic rename. Only this empty
        // structural prefix is resumable without an intent; a flat/unknown
        // previous installation is deliberately unsupported.
        if root.join("maple-agent.exe").try_exists()?
            || root.join("uninstall.exe").try_exists()?
            || (root.join("versions").try_exists()?
                && fs::read_dir(root.join("versions"))?.next().is_some())
        {
            return Err(InstallError::Invalid(
                "Windows installation is incomplete without a recovery intent",
            ));
        }
    }
    Ok(())
}

/// NSIS calls this read-only admission check from its extracted native checker
/// before writing application files or per-user registration.
pub fn preflight_windows_installation(checker: &Path, root: &Path, profile: &str) -> Result<()> {
    #[cfg(windows)]
    {
        let checker = checker.canonicalize()?;
        let _ = trusted_installer(&checker, profile)?;
        windows_fs::validate_path_syntax(root)?;
        if root.file_name().is_none() {
            return Err(InstallError::Invalid("Windows installer destination"));
        }
        let mut existing = root;
        while !existing.try_exists()? {
            existing = existing
                .parent()
                .ok_or(InstallError::Invalid("Windows installer parent"))?;
        }
        // Existing destinations are never taken over by changing their ACL.
        windows_fs::private_directory(existing)?;
        if root.try_exists()? {
            let root = root.canonicalize()?;
            if fs::read_dir(&root)?.next().is_some() {
                inspect_manual_root(&root, profile)?;
            }
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (checker, root, profile);
        Err(InstallError::NotifyOnly(
            "Windows installer requires native Windows",
        ))
    }
}

/// Start/resume the explicit signed manual-install transaction. The durable
/// intent excludes automatic updates during NSIS copying; the exact committed
/// generation stays launchable if an installer is interrupted. The marker
/// it is removed only after a fully validated atomic pointer commit.
pub fn prepare_windows_installation(
    checker: &Path,
    root: &Path,
    profile: &str,
    generation_name: &str,
) -> Result<()> {
    #[cfg(windows)]
    {
        preflight_windows_installation(checker, root, profile)?;
        if !valid_generation(generation_name) {
            return Err(InstallError::Invalid("Windows manual generation"));
        }
        let checker = checker.canonicalize()?;
        let info = trusted_installer(&checker, profile)?;
        if generation_name.rsplit_once('-').unwrap().0 != info["version"].as_str().unwrap() {
            return Err(InstallError::Invalid(
                "Windows installer generation identity",
            ));
        }
        windows_fs::private_directory(root)?;
        let root = root.canonicalize()?;
        let lock = open_install_lock(&root)?;
        lock.try_lock_exclusive().map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock
                || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
            {
                InstallError::Busy
            } else {
                error.into()
            }
        })?;
        // Recheck after acquiring the lock: an updater may have committed while
        // this installer was performing its read-only admission checks.
        if root.join(POINTER).try_exists()? || root.join(MANUAL_INTENT).try_exists()? {
            inspect_manual_root(&root, profile)?;
        }
        if root.join(MANUAL_INTENT).try_exists()? {
            let previous: ManualIntent =
                serde_json::from_slice(&read_bounded(&root.join(MANUAL_INTENT), DESCRIPTOR_LIMIT)?)
                    .map_err(|_| InstallError::Invalid("Windows installer recovery intent"))?;
            if process_created(previous.owner_pid)?
                .is_some_and(|created| created == previous.owner_created)
            {
                return Err(InstallError::Busy);
            }
            if previous.format != "maple-agent-windows-installer-intent-v1"
                || previous.profile != profile
                || !valid_generation(&previous.generation)
            {
                return Err(InstallError::Invalid("Windows prior installer intent"));
            }
            let current = if root.join(POINTER).try_exists()? {
                Some(Current::parse(&read_bounded(&root.join(POINTER), 1024)?)?)
            } else {
                None
            };
            let prior_path = root.join("versions").join(&previous.generation);
            if prior_path.try_exists()?
                && !current
                    .as_ref()
                    .is_some_and(|pointer| pointer.generation == previous.generation)
                && structurally_complete_generation(&prior_path, profile, &previous.generation)
                    .is_err()
            {
                let guard = incomplete_generation_guard(&prior_path)?;
                remove_generation_files(&prior_path, guard)?;
            }
        }
        if let Some(intent) = abandoned_uninstall(&root, profile)? {
            validate_uninstall_repair(&root, profile, &intent)?;
            let current = if root.join(POINTER).try_exists()? {
                Some(Current::parse(&read_bounded(&root.join(POINTER), 1024)?)?)
            } else {
                None
            };
            for name in &intent.generations {
                if current
                    .as_ref()
                    .is_some_and(|pointer| pointer.generation == *name)
                {
                    continue;
                }
                let path = root.join("versions").join(name);
                if path.try_exists()?
                    && structurally_complete_generation(&path, profile, name).is_err()
                {
                    let guard = incomplete_generation_guard(&path)?;
                    remove_generation_files(&path, guard)?;
                }
            }
        }
        let owner = installer_owner()?;
        windows_fs::private_directory(&root.join("versions"))?;
        let current = if root.join(POINTER).try_exists()? {
            Some(Current::parse(&read_bounded(&root.join(POINTER), 1024)?)?)
        } else {
            None
        };
        // Manual repair never modifies a mapped generation, even a rollback.
        for entry in fs::read_dir(root.join("versions"))?.take(4096) {
            let entry = entry?;
            if !entry.file_name().to_str().is_some_and(valid_generation) {
                return Err(InstallError::Invalid("unknown Windows generation"));
            }
            known_partial_generation(&entry.path())?;
            if entry.path().join("maple-agent.exe").try_exists()? {
                let probe = OpenOptions::new()
                    .append(true)
                    .open(entry.path().join("maple-agent.exe"))?;
                drop(probe);
            }
        }
        let intent = ManualIntent {
            format: "maple-agent-windows-installer-intent-v1".into(),
            profile: profile.into(),
            generation: generation_name.into(),
            version: info["version"].as_str().unwrap().into(),
            source_sha: info["source_sha"].as_str().unwrap().into(),
            executable_sha256: exact_file_digest(&checker)?,
            owner_pid: owner.0,
            owner_created: owner.1,
        };
        // Publish admission intent before copying any owned installation file.
        write_private_json(&root.join(MANUAL_INTENT), &intent)?;
        let target = root.join("versions").join(generation_name);
        if target.try_exists()? {
            let complete = descriptor(&target, profile)
                .and_then(|bundle| {
                    if bundle.generation() != generation_name {
                        return Err(InstallError::Invalid("Windows repair identity"));
                    }
                    let _ = scan_generation(&target)?;
                    verify_identity(&target, &bundle)
                })
                .is_ok();
            if !complete {
                if current
                    .as_ref()
                    .is_some_and(|pointer| pointer.generation == generation_name)
                {
                    return Err(InstallError::Invalid(
                        "cannot repair active Windows generation in place",
                    ));
                }
                let guard = incomplete_generation_guard(&target)?;
                remove_generation_files(&target, guard)?;
            }
        }
        if !root.join("maple-agent.exe").try_exists()? {
            let temporary = tempfile::Builder::new()
                .prefix(TRANSACTION_TEMP_PREFIX)
                .rand_bytes(STAGING_RANDOM_LENGTH)
                .tempfile_in(&root)?
                .into_temp_path();
            fs::copy(&checker, &temporary)?;
            if exact_file_digest(&temporary)? != intent.executable_sha256
                || !authenticode::verify_or_unsigned(&temporary, profile == "dev")
            {
                return Err(InstallError::CodeSignature);
            }
            windows_atomic_replace(&temporary, &root.join("maple-agent.exe"))?;
        }
        windows_sync_directory(&root)?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (checker, root, profile, generation_name);
        Err(InstallError::NotifyOnly(
            "Windows installer preparation requires native Windows",
        ))
    }
}

/// Activation keeps the stable bootstrap and authenticates the exact new
/// generation against the durable signed-checker intent before publication.
pub fn activate_windows_installation(
    executable: &Path,
    profile: &str,
    generation_name: &str,
) -> Result<()> {
    #[cfg(windows)]
    {
        if !valid_generation(generation_name) {
            return Err(InstallError::Invalid("Windows activation generation"));
        }
        let executable = executable.canonicalize()?;
        let root = executable
            .parent()
            .ok_or(InstallError::Invalid("Windows activation root"))?;
        if executable
            .file_name()
            .is_none_or(|x| x != "maple-agent.exe")
        {
            return Err(InstallError::Invalid("Windows activation executable"));
        }
        windows_fs::private_directory(root)?;
        registered_root(root, profile)?;
        let _ = trusted_installer(&executable, profile)?;
        let lock = open_install_lock(root)?;
        lock.try_lock_exclusive().map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock
                || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
            {
                InstallError::Busy
            } else {
                error.into()
            }
        })?;
        let intent: ManualIntent =
            serde_json::from_slice(&read_bounded(&root.join(MANUAL_INTENT), DESCRIPTOR_LIMIT)?)
                .map_err(|_| InstallError::Invalid("Windows installer intent"))?;
        if intent.format != "maple-agent-windows-installer-intent-v1"
            || intent.profile != profile
            || intent.generation != generation_name
            || (intent.owner_pid, intent.owner_created) != installer_owner()?
        {
            return Err(InstallError::Invalid("Windows installer intent identity"));
        }
        let path = root.join("versions").join(generation_name);
        let bundle = descriptor(&path, profile)?;
        if bundle.generation() != generation_name
            || bundle.version != intent.version
            || bundle.source_sha != intent.source_sha
            || exact_file_digest(&path.join("maple-agent.exe"))? != intent.executable_sha256
        {
            return Err(InstallError::Invalid(
                "Windows installer activation identity",
            ));
        }
        let _ = scan_generation(&path)?;
        verify_identity(&path, &bundle)?;
        let _generation_guards = generation_write_guards(root)?;
        write_private_json(&root.join(POINTER), &Current::new(&bundle))?;
        drop(_generation_guards);
        // Publication committed. Cleanup cannot report an uncommitted failure
        // to NSIS, which would otherwise roll back valid registration. Startup
        // authenticates this exact generation and retries retained intent.
        if retire_abandoned_uninstall(root, profile, &bundle.generation()).is_ok() {
            let _ = fs::remove_file(root.join(MANUAL_INTENT));
        }
        let _ = windows_sync_directory(root);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (executable, profile, generation_name);
        Err(InstallError::NotifyOnly(
            "Windows activation requires native Windows",
        ))
    }
}

/// Uninstaller entry derives authority from its own registered bootstrap path.
/// Unknown files and live mapped generations are preserved rather than followed.
pub fn cleanup_windows_installation(executable: &Path, profile: &str) -> Result<usize> {
    #[cfg(windows)]
    {
        let executable = executable.canonicalize()?;
        if executable
            .file_name()
            .is_none_or(|x| x != "maple-agent.exe")
        {
            return Err(InstallError::Invalid("Windows uninstaller executable"));
        }
        let root = executable
            .parent()
            .ok_or(InstallError::Invalid("Windows uninstall parent"))?;
        windows_fs::validate_path(root)?;
        registered_root(root, profile)?;
        if pe_build_info(&executable)?["profile"] != profile
            || !authenticode::verify_or_unsigned(&executable, profile == "dev")
        {
            return Err(InstallError::CodeSignature);
        }
        let lock = open_install_lock(root)?;
        lock.try_lock_exclusive().map_err(|_| InstallError::Busy)?;
        let owner = installer_owner()?;
        if root.join(MANUAL_INTENT).try_exists()? {
            let intent: ManualIntent =
                serde_json::from_slice(&read_bounded(&root.join(MANUAL_INTENT), DESCRIPTOR_LIMIT)?)
                    .map_err(|_| InstallError::Invalid("Windows uninstall manual intent"))?;
            if process_created(intent.owner_pid)?
                .is_some_and(|created| created == intent.owner_created)
            {
                return Err(InstallError::Busy);
            }
        }
        let previous = if root.join(UNINSTALL_INTENT).try_exists()? {
            let intent = UninstallIntent::parse(
                &read_bounded(&root.join(UNINSTALL_INTENT), 65536)?,
                profile,
            )?;
            if (intent.owner_pid, intent.owner_created) != owner
                && process_created(intent.owner_pid)?
                    .is_some_and(|created| created == intent.owner_created)
            {
                return Err(InstallError::Busy);
            }
            Some(intent)
        } else {
            None
        };
        let versions = root.join("versions");
        let mut directories = Vec::new();
        if versions.try_exists()? {
            checked_entry(&versions)?;
            for entry in fs::read_dir(&versions)? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| InstallError::Invalid("Windows uninstall generation name"))?;
                if directories.len() >= 256 || !valid_generation(&name) {
                    return Err(InstallError::Invalid(
                        "unknown Windows installed generation",
                    ));
                }
                if let Some(previous) = &previous {
                    if !previous.generations.contains(&name) {
                        return Err(InstallError::Invalid(
                            "generation appeared during Windows uninstall",
                        ));
                    }
                    known_partial_generation(&entry.path())?;
                } else {
                    complete_generation(&entry.path(), profile, &name)?;
                }
                // Retained write handles exclude mapped images and block new
                // mappings through file deletion, while FILE_SHARE_DELETE
                // permits the uninstaller's own fixed-file unlink operations.
                let guard = incomplete_generation_guard(&entry.path())?;
                directories.push((name, entry.path(), guard));
            }
        }
        let intent = UninstallIntent {
            format: "maple-agent-windows-uninstall-intent-v1".into(),
            profile: profile.into(),
            owner_pid: owner.0,
            owner_created: owner.1,
            generations: previous
                .map(|intent| intent.generations)
                .unwrap_or_else(|| {
                    directories
                        .iter()
                        .map(|(name, _, _)| name.clone())
                        .collect()
                }),
        };
        write_private_json(&root.join(UNINSTALL_INTENT), &intent)?;
        let count = directories.len();
        for (_, path, guard) in directories {
            remove_generation_files(&path, guard)?;
        }
        if versions.try_exists()? {
            fs::remove_dir(&versions)?;
        }
        // NSIS retains this live-owner intent after the helper exits, performs
        // all fixed root/shortcut/registry removals, and deletes this marker
        // last. No later recursive/root-file mutation may follow its release.
        Ok(count)
    }
    #[cfg(not(windows))]
    {
        let _ = (executable, profile);
        Err(InstallError::NotifyOnly(
            "Windows uninstall requires native Windows",
        ))
    }
}

#[cfg(windows)]
pub(crate) fn windows_private_directory(path: &Path) -> Result<()> {
    Ok(windows_fs::private_directory(path)?)
}
#[cfg(windows)]
pub(crate) fn windows_open_private(path: &Path, truncate: bool) -> Result<File> {
    Ok(windows_fs::open_private(path, truncate)?)
}
#[cfg(windows)]
pub(crate) fn windows_open_readonly(path: &Path) -> Result<File> {
    Ok(windows_fs::open_readonly(path)?)
}
#[cfg(windows)]
pub(crate) fn windows_read_bounded(path: &Path, maximum: u64) -> Result<Option<Vec<u8>>> {
    Ok(windows_fs::read_bounded(path, maximum)?)
}
#[cfg(windows)]
pub(crate) fn windows_sync_directory(path: &Path) -> Result<()> {
    Ok(windows_fs::sync_directory(path)?)
}
#[cfg(not(windows))]
fn windows_sync_directory(path: &Path) -> Result<()> {
    sync_directory(path)
}
#[cfg(windows)]
pub(crate) fn windows_atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    Ok(windows_fs::atomic_replace(source, destination)?)
}
#[cfg(not(windows))]
fn windows_atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)?;
    sync_directory(destination.parent().unwrap())
}

#[cfg(test)]
#[path = "windows_bundle_tests.rs"]
mod tests;

#[cfg(all(test, windows))]
#[path = "windows_bundle_transaction_tests.rs"]
mod transaction_tests;
