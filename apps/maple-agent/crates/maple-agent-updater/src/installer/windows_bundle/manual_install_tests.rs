use super::*;

const METADATA: &[u8] = br#"{"profile":"dev","bundle_id":"cloud.opensecret.maple.agent.dev","version":"0.0.18","source_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","update_feed_url":null}"#;
// A real unsigned native PE, exercising the normal explicit Dev exception.
// This section is absent from library/application release builds.
#[used]
#[unsafe(link_section = ".maple")]
static TEST_METADATA: [u8; METADATA.len()] = {
    let mut bytes = [0u8; METADATA.len()];
    bytes.copy_from_slice(METADATA);
    bytes
};

struct IsolatedRegistry {
    key: Key,
    name: Vec<u16>,
}
impl IsolatedRegistry {
    fn new() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = text(&format!(
            "Software\\Maple Work Installer Tests\\{}-{now}",
            std::process::id()
        ));
        let mut key = std::ptr::null_mut();
        assert_eq!(
            unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    name.as_ptr(),
                    0,
                    std::ptr::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_ALL_ACCESS | KEY_WOW64_32KEY,
                    std::ptr::null(),
                    &mut key,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        assert_eq!(unsafe { RegOverridePredefKey(HKEY_CURRENT_USER, key) }, 0);
        Self {
            key: Key(key),
            name,
        }
    }
}
impl Drop for IsolatedRegistry {
    fn drop(&mut self) {
        unsafe {
            RegOverridePredefKey(HKEY_CURRENT_USER, std::ptr::null_mut());
            // Only the unique test key opened above; never a product key.
            RegDeleteTreeW(self.key.0, std::ptr::null());
            RegDeleteKeyExW(HKEY_CURRENT_USER, self.name.as_ptr(), KEY_WOW64_32KEY, 0);
        }
    }
}

fn payload(root: &Path, version: &str, build: &str) -> PathBuf {
    let directory = root.join(format!("payload-{version}-{build}"));
    windows_fs::private_directory(&directory).unwrap();
    let mut bytes = fs::read(std::env::current_exe().unwrap()).unwrap();
    let info: serde_json::Value = serde_json::from_slice(METADATA).unwrap();
    let mut updated = info.clone();
    updated["version"] = version.into();
    let updated_bytes = serde_json::to_vec(&updated).unwrap();
    assert_eq!(updated_bytes.len(), METADATA.len());
    let pe = u32::from_le_bytes(bytes[60..64].try_into().unwrap()) as usize;
    let count = u16::from_le_bytes(bytes[pe + 6..pe + 8].try_into().unwrap()) as usize;
    let optional = u16::from_le_bytes(bytes[pe + 20..pe + 22].try_into().unwrap()) as usize;
    let table = pe + 24 + optional;
    let section = (0..count)
        .map(|n| table + n * 40)
        .find(|n| &bytes[*n..*n + 8] == b".maple\0\0")
        .unwrap();
    let offset = u32::from_le_bytes(bytes[section + 20..section + 24].try_into().unwrap()) as usize;
    bytes[offset..offset + updated_bytes.len()].copy_from_slice(&updated_bytes);
    let bundle = Bundle {
        format: "maple-agent-windows-bundle-v2".into(),
        product: "maple-agent".into(),
        profile: "dev".into(),
        version: version.into(),
        build: build.into(),
        source_sha: "a".repeat(40),
        bundle_id: profile_identity("dev").unwrap().into(),
        entrypoint: "maple-agent.exe".into(),
    };
    for (name, contents) in [
        ("maple-agent.exe", bytes),
        (
            "uninstall.exe",
            fs::read(std::env::current_exe().unwrap()).unwrap(),
        ),
        ("build-info.json", updated_bytes),
        (DESCRIPTOR, serde_json::to_vec(&bundle).unwrap()),
        ("LICENSE.txt", b"public fixture license".to_vec()),
    ] {
        let mut file = windows_fs::create_private(&directory.join(name)).unwrap();
        file.write_all(&contents).unwrap();
        file.sync_all().unwrap();
    }
    directory.join("maple-agent.exe")
}

fn abandon_intent(root: &Path) {
    let mut intent: ManualIntent =
        serde_json::from_slice(&fs::read(root.join(MANUAL_INTENT)).unwrap()).unwrap();
    intent.owner_pid = u32::MAX;
    write_private_json(&root.join(MANUAL_INTENT), &intent).unwrap();
}

#[test]
fn native_install_transaction_covers_fresh_repeat_upgrade_rollback_and_recovery() {
    const TEST: &str = "installer::windows_bundle::manual_install::tests::native_install_transaction_covers_fresh_repeat_upgrade_rollback_and_recovery";
    let Some(directory) = std::env::var_os("MAPLE_NATIVE_INSTALL_TEST") else {
        let temporary = tempfile::tempdir().unwrap();
        let local = temporary.path().join("local");
        fs::create_dir(&local).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
            .env("MAPLE_NATIVE_INSTALL_TEST", temporary.path())
            .env("LOCALAPPDATA", local)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "isolated native transaction failed: {}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("NATIVE_INSTALL_TRANSACTION_OK"));
        return;
    };
    let _registry = IsolatedRegistry::new();
    let scratch = PathBuf::from(directory);
    let export = scratch.join("support-report.txt");
    let mut trace = super::super::super::WindowsInstallerTrace::begin(
        "installation",
        &serde_json::json!({"version":"0.0.18","profile":"dev","api_url":"CANARY-PRIVATE"}),
        Some(&export),
    );
    trace.record(WindowsInstallOperation::Checker, false);
    // Checkpoints exist before completion, including if the child crashes.
    assert!(
        fs::read_to_string(&export)
            .unwrap()
            .contains("checkpoint: verify-checker-image")
    );
    let failed = InstallError::Io(io::Error::new(
        io::ErrorKind::PermissionDenied,
        WindowsStorageGuard::ForeignWriter,
    ))
    .during(WindowsInstallOperation::Parent)
    .with_mutations_started(false)
    .windows_installer_diagnostic();
    let report = trace.finish(Some(&failed));
    assert_eq!(fs::read_to_string(&export).unwrap(), report);
    assert!(report.contains("installation changes: none"));
    assert!(report.contains("foreign-write-ace"));
    assert!(!report.contains("CANARY-PRIVATE"));
    let logs =
        PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("Maple Work Installer Logs");
    let log = fs::read_dir(&logs).unwrap().next().unwrap().unwrap().path();
    let contents = fs::read_to_string(log).unwrap();
    assert!(contents.contains("checkpoint: verify-checker-image"));
    assert!(contents.contains("foreign-write-ace"));
    assert!(!contents.contains("CANARY-PRIVATE"));
    let root = scratch.join("app installation with spaces");
    let menu = scratch.join("start menu");
    let first = payload(&scratch, "0.0.18", "9001");
    windows_fs::private_directory(&menu).unwrap();
    fs::write(menu.join("user notes.txt"), b"preserve unrelated menu data").unwrap();
    let failed = install(&first, &root, "dev", "0.0.18-9001", Some(&menu), || {
        Err(InstallError::Invalid("injected fresh-install failure"))
    })
    .unwrap_err();
    assert_eq!(
        failed.windows_installer_diagnostic().mutations_started,
        Some(true)
    );
    assert!(!root.join(POINTER).exists());
    assert!(!menu.join("Maple Work Dev.lnk").exists());
    assert!(!menu.join("Uninstall.lnk").exists());
    assert_eq!(
        fs::read(menu.join("user notes.txt")).unwrap(),
        b"preserve unrelated menu data"
    );
    assert!(
        Registration::admit(&root, "dev", "0.0.18")
            .unwrap()
            .before
            .values()
            .all(Option::is_none)
    );
    abandon_intent(&root);
    install(&first, &root, "dev", "0.0.18-9001", Some(&menu), || Ok(())).unwrap();
    assert_eq!(
        generation(&root, "dev").unwrap().0.generation,
        "0.0.18-9001"
    );
    let bootstrap = fs::read(root.join("maple-agent.exe")).unwrap();
    let uninstaller = fs::read(root.join("uninstall.exe")).unwrap();
    let pointer = fs::read(root.join(POINTER)).unwrap();
    let link = fs::read(menu.join("Maple Work Dev.lnk")).unwrap();
    install(&first, &root, "dev", "0.0.18-9001", Some(&menu), || Ok(())).unwrap();
    assert_eq!(fs::read(root.join(POINTER)).unwrap(), pointer);
    assert_eq!(fs::read(menu.join("Maple Work Dev.lnk")).unwrap(), link);
    let next = payload(&scratch, "0.0.19", "9002");
    let failed = install(&next, &root, "dev", "0.0.19-9002", Some(&menu), || {
        Err(InstallError::Invalid("injected precommit failure"))
    })
    .unwrap_err();
    assert_eq!(
        failed.windows_installer_diagnostic().mutations_started,
        Some(true)
    );
    assert_eq!(fs::read(root.join(POINTER)).unwrap(), pointer);
    let registration = Registration::admit(&root, "dev", "0.0.19").unwrap();
    assert_eq!(
        registration.before["DisplayVersion"].as_ref(),
        Some(&Value::string("0.0.18"))
    );
    drop(registration);
    assert!(root.join(MANUAL_INTENT).is_file());
    abandon_intent(&root);
    install(&next, &root, "dev", "0.0.19-9002", Some(&menu), || Ok(())).unwrap();
    assert_eq!(
        generation(&root, "dev").unwrap().0.generation,
        "0.0.19-9002"
    );
    assert_eq!(fs::read(root.join("maple-agent.exe")).unwrap(), bootstrap);
    assert_eq!(fs::read(root.join("uninstall.exe")).unwrap(), uninstaller);
    assert!(root.join("versions/0.0.18-9001/maple-agent.exe").is_file());
    assert!(!root.join(MANUAL_INTENT).exists());
    // A different destination cannot steal this profile's registration.
    let other = scratch.join("different installation");
    let error = install(&next, &other, "dev", "0.0.19-9002", Some(&menu), || Ok(())).unwrap_err();
    assert_eq!(
        error.windows_installer_diagnostic().mutations_started,
        Some(false)
    );
    assert!(!other.exists());
    // Two admitted manual installers cannot enter different roots concurrently.
    let _held = profile_lock("dev").unwrap();
    let error = install(&next, &root, "dev", "0.0.19-9002", Some(&menu), || Ok(())).unwrap_err();
    assert_eq!(error.windows_installer_exit_code(), 0x4E00_0002);
    println!("NATIVE_INSTALL_TRANSACTION_OK");
}

#[test]
fn exclusive_publication_preserves_raced_destination() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let destination = temporary.path().join("destination");
    fs::write(&source, b"new").unwrap();
    fs::write(&destination, b"preserve").unwrap();
    assert!(windows_fs::publish_new(&source, &destination).is_err());
    assert_eq!(fs::read(destination).unwrap(), b"preserve");
    assert_eq!(fs::read(source).unwrap(), b"new");
}
