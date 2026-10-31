use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use blake2::Blake2b512;
use ed25519_dalek::{Signer, SigningKey};
use flate2::{Compression, write::GzEncoder};

enum Member {
    Directory(String),
    File(String, Vec<u8>, u32),
    Link(String, String),
    Special(String, u8),
}

fn write_archive(path: &Path, members: &[Member]) {
    let output = File::create(path).unwrap();
    let mut writer = tar::Builder::new(GzEncoder::new(output, Compression::fast()));
    for member in members {
        let mut header = tar::Header::new_ustar();
        let (name, bytes, kind, mode, link) = match member {
            Member::Directory(name) => (name, &[][..], b'5', 0o755, None),
            Member::File(name, bytes, mode) => (name, bytes.as_slice(), b'0', *mode, None),
            Member::Link(name, target) => (name, &[][..], b'2', 0o777, Some(target)),
            Member::Special(name, kind) => (name, &[][..], *kind, 0o644, None),
        };
        header.set_mode(mode);
        header.set_size(bytes.len() as u64);
        header.set_entry_type(tar::EntryType::new(kind));
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        // Preserve deliberately unsafe paths in negative fixtures instead of
        // letting tar's convenience writer reject them before our parser runs.
        assert!(name.len() < 100);
        header.as_mut_bytes()[..100].fill(0);
        header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
        if let Some(target) = link {
            header.set_link_name(target).unwrap();
        }
        header.set_cksum();
        writer.append(&header, bytes).unwrap();
    }
    writer
        .into_inner()
        .unwrap()
        .finish()
        .unwrap()
        .sync_all()
        .unwrap();
}

fn descriptor(version: &str, build: &str, source: &str) -> Bundle {
    Bundle {
        format: "maple-agent-linux-bundle-v1".into(),
        product: "maple-agent".into(),
        profile: "prod".into(),
        version: version.into(),
        build: build.into(),
        source_sha: source.into(),
        bundle_id: "cloud.opensecret.maple.agent".into(),
        entrypoint: "AppRun".into(),
        executable: "usr/bin/maple-agent".into(),
    }
}

fn members(bundle: &Bundle) -> Vec<Member> {
    vec![
        Member::Directory(PROD_ROOT.into()),
        Member::Directory(format!("{PROD_ROOT}/usr")),
        Member::Directory(format!("{PROD_ROOT}/usr/bin")),
        Member::Directory(format!("{PROD_ROOT}/usr/lib")),
        Member::File(
            format!("{PROD_ROOT}/bundle.json"),
            serde_json::to_vec(bundle).unwrap(),
            0o644,
        ),
        // These are filesystem fixtures, never executable test programs.
        Member::File(
            format!("{PROD_ROOT}/AppRun"),
            format!("launcher {}", bundle.version).into_bytes(),
            0o755,
        ),
        Member::File(
            format!("{PROD_ROOT}/usr/bin/maple-agent"),
            format!("payload {}", bundle.version).into_bytes(),
            0o755,
        ),
        Member::File(
            format!("{PROD_ROOT}/usr/lib/ld-linux-x86-64.so.2"),
            b"loader fixture".to_vec(),
            0o755,
        ),
        Member::Link(
            format!("{PROD_ROOT}/usr/lib/current-loader"),
            "ld-linux-x86-64.so.2".into(),
        ),
    ]
}

fn public_key(seed: u8) -> String {
    let signing = SigningKey::from_bytes(&[seed; 32]);
    let mut bytes = b"Ed12345678".to_vec();
    bytes.extend_from_slice(signing.verifying_key().as_bytes());
    STANDARD.encode(bytes)
}

fn signed_release(payload: &Path, bundle: &Bundle) -> crate::VerifiedRelease {
    let artifact = Artifact {
        path: format!(
            "/agent/stable/releases/{}/Maple-Agent-linux-x86_64.tar.gz",
            bundle.version
        ),
        size: fs::metadata(payload).unwrap().len(),
        sha256: hex::encode(Sha256::digest(fs::read(payload).unwrap())),
    };
    let manifest = crate::Manifest {
        format: "maple-agent-update-v2".into(),
        product: "maple-agent".into(),
        channel: "stable".into(),
        version: bundle.version.clone(),
        build: bundle.build.clone(),
        source_sha: bundle.source_sha.clone(),
        pub_date: "2026-10-23T00:00:00Z".into(),
        notes: "Linux filesystem fixture".into(),
        artifacts: BTreeMap::from([("linux-x86_64".into(), artifact.clone())]),
        downloads: crate::Downloads {
            macos: None,
            linux_appimage: None,
            linux_bundle: Some(artifact),
            windows: None,
            windows_arm64: None,
        },
    };
    let text = serde_json::to_string(&manifest).unwrap();
    let comment = format!(
        "product=maple-agent channel=stable version={}",
        bundle.version
    );
    // The deterministic key is test-only and never an application trust pin.
    let signing = SigningKey::from_bytes(&[7; 32]);
    let signature = signing.sign(&Blake2b512::digest(text.as_bytes()));
    let mut packet = b"ED12345678".to_vec();
    packet.extend_from_slice(&signature.to_bytes());
    let mut global_message = signature.to_bytes().to_vec();
    global_message.extend_from_slice(comment.as_bytes());
    let global_signature = signing.sign(&global_message);
    let envelope = serde_json::to_vec(&serde_json::json!({
        "manifest": text,
        "signature": format!("untrusted comment: TEST ONLY\n{}\ntrusted comment: {comment}\n{}\n", STANDARD.encode(packet), STANDARD.encode(global_signature.to_bytes())),
    })).unwrap();
    manifest::verify_envelope(&envelope, &origin(), &[public_key(7)]).unwrap()
}

fn origin() -> Url {
    "https://updates.example.test".parse().unwrap()
}

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    update: PreparedUpdate,
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let parent = root.join("installation with spaces");
    fs::create_dir(&parent).unwrap();
    set_mode(&parent, 0o755).unwrap();
    let old_payload = root.join("old.tar.gz");
    write_archive(
        &old_payload,
        &members(&descriptor("0.0.1", "1", &"a".repeat(40))),
    );
    archive(&old_payload, Some(&parent), "prod").unwrap();
    let payload_path = root.join("download.tar.gz");
    let bundle = descriptor("0.0.2", "2", &"b".repeat(40));
    write_archive(&payload_path, &members(&bundle));
    let release = signed_release(&payload_path, &bundle);
    Fixture {
        _directory: directory,
        root,
        update: PreparedUpdate {
            release,
            payload_path,
            installed_path: parent.join(PROD_ROOT),
        },
    }
}

#[test]
fn archive_and_discovery_require_the_exact_bundle_identity() {
    let fixture = fixture();
    validate_linux_bundle_archive(&fixture.update.payload_path, "prod").unwrap();
    assert!(validate_linux_bundle_archive(&fixture.update.payload_path, "dev").is_err());
    assert_eq!(
        discover_linux_installation(
            &fixture.update.installed_path.join("AppRun"),
            "0.0.1",
            &"a".repeat(40)
        )
        .unwrap(),
        fixture.update.installed_path,
    );
    assert!(
        discover_linux_installation(
            &fixture.update.installed_path.join("AppRun"),
            "0.0.1",
            &"b".repeat(40)
        )
        .is_err()
    );
    let alias = fixture.root.join("launcher-alias");
    std::os::unix::fs::symlink(fixture.update.installed_path.join("AppRun"), &alias).unwrap();
    assert!(discover_linux_installation(&alias, "0.0.1", &"a".repeat(40)).is_err());
}

#[test]
fn both_archive_roots_reject_escape_links_special_files_and_wrong_roots() {
    let directory = tempfile::tempdir().unwrap();
    let sentinel = directory.path().join("outside");
    fs::write(&sentinel, b"untouched").unwrap();
    for root in [APP_NAME, PROD_ROOT] {
        let cases = [
            vec![Member::File(
                "../outside".into(),
                b"changed".to_vec(),
                0o644,
            )],
            vec![Member::Directory("another.app".into())],
            vec![Member::Link(format!("{root}/escape"), "../outside".into())],
            vec![Member::Special(format!("{root}/hardlink"), b'1')],
            vec![Member::Special(format!("{root}/fifo"), b'6')],
            vec![
                Member::Directory(format!("{root}/sub")),
                Member::Link(format!("{root}/sub/up"), "..".into()),
                Member::Link(format!("{root}/escape"), "sub/up/../outside".into()),
            ],
        ];
        for (index, mut extra) in cases.into_iter().enumerate() {
            let payload = directory.path().join(format!("bad-{index}.tar.gz"));
            let mut members = vec![Member::Directory(root.into())];
            members.append(&mut extra);
            write_archive(&payload, &members);
            let output = directory.path().join(format!("output-{index}"));
            fs::create_dir_all(&output).unwrap();
            assert!(scan_archive_root(&payload, Some(&output), root).is_err());
            assert_eq!(fs::read(&sentinel).unwrap(), b"untouched");
            fs::remove_dir_all(output).unwrap();
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn preparation_revalidates_signed_bytes_modes_links_and_extra_files() {
    for mutation in 0..5 {
        let fixture = fixture();
        let _lock = acquire_install_lock(&fixture.update.installed_path).unwrap();
        let prepared = prepare(&fixture.update).unwrap();
        revalidate(&prepared, &fixture.update).unwrap();
        let cache = fs::metadata(&fixture.update.payload_path).unwrap();
        let copy = fs::metadata(&prepared.payload_path).unwrap();
        assert_ne!((cache.dev(), cache.ino()), (copy.dev(), copy.ino()));
        match mutation {
            0 => fs::write(prepared.staged_path.join("AppRun"), b"changed launcher").unwrap(),
            1 => set_mode(&prepared.staged_path.join("AppRun"), 0o644).unwrap(),
            2 => {
                fs::remove_file(prepared.staged_path.join("AppRun")).unwrap();
                std::os::unix::fs::symlink(
                    "usr/bin/maple-agent",
                    prepared.staged_path.join("AppRun"),
                )
                .unwrap();
            }
            3 => fs::write(prepared.staged_path.join("unexpected"), b"extra file").unwrap(),
            4 => fs::write(&prepared.payload_path, b"changed signed archive").unwrap(),
            _ => unreachable!(),
        }
        assert!(revalidate(&prepared, &fixture.update).is_err());
        assert_eq!(
            installed_bundle(&fixture.update.installed_path)
                .unwrap()
                .version,
            "0.0.1"
        );
        discard(&prepared).unwrap();
        assert!(fixture.update.payload_path.exists());
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn stale_preparation_cannot_replace_an_equal_or_newer_installation() {
    for current_version in ["0.0.2", "0.0.3"] {
        let fixture = fixture();
        let _lock = acquire_install_lock(&fixture.update.installed_path).unwrap();
        let prepared = prepare(&fixture.update).unwrap();
        let current = descriptor(current_version, "3", &"c".repeat(40));
        fs::write(
            fixture.update.installed_path.join("bundle.json"),
            serde_json::to_vec(&current).unwrap(),
        )
        .unwrap();
        fs::write(
            fixture.update.installed_path.join("AppRun"),
            b"newer live installation",
        )
        .unwrap();
        assert!(revalidate(&prepared, &fixture.update).is_err());
        assert!(replace(&prepared, &fixture.update).is_err());
        assert_eq!(
            fs::read(fixture.update.installed_path.join("AppRun")).unwrap(),
            b"newer live installation"
        );
        assert!(!prepared.staging_dir.join(RECEIPT_NAME).exists());
        discard(&prepared).unwrap();
    }
}

#[test]
fn orphan_cleanup_preserves_other_installations_and_uncertain_receipts() {
    let fixture = fixture();
    let installed = &fixture.update.installed_path;
    let make_stage = |path: &Path| {
        let stage = tempfile::Builder::new()
            .prefix(&stage_prefix(path))
            .rand_bytes(STAGING_RANDOM_LENGTH)
            .tempdir_in(installed.parent().unwrap())
            .unwrap();
        set_mode(stage.path(), 0o700).unwrap();
        let path = stage.keep();
        fs::write(path.join("sentinel"), b"retained").unwrap();
        path
    };
    let orphan = make_stage(installed);
    let other = make_stage(&installed.with_file_name(DEV_ROOT));
    let uncertain = make_stage(installed);
    fs::write(uncertain.join(RECEIPT_NAME), b"truncated receipt").unwrap();
    assert_eq!(
        cleanup_current_linux_bundle(
            installed,
            "0.0.1",
            &"a".repeat(40),
            &origin(),
            &[public_key(7)]
        )
        .unwrap(),
        1
    );
    assert!(!orphan.exists());
    assert_eq!(fs::read(other.join("sentinel")).unwrap(), b"retained");
    assert_eq!(fs::read(uncertain.join("sentinel")).unwrap(), b"retained");
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn commit(fixture: &Fixture) -> (InstallPreparation, InstalledUpdate) {
    let lock = acquire_install_lock(&fixture.update.installed_path).unwrap();
    let prepared = prepare(&fixture.update).unwrap();
    let installed = replace(&prepared, &fixture.update).unwrap();
    drop(lock);
    (prepared, installed)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn native_exchange_retains_backup_until_authenticated_new_startup() {
    let fixture = fixture();
    let (prepared, installed) = commit(&fixture);
    assert_eq!(
        installed.restart_path(),
        fixture.update.installed_path.join("AppRun")
    );
    assert_eq!(
        installed_bundle(&installed.installed_path).unwrap().version,
        "0.0.2"
    );
    assert_eq!(
        installed_bundle(&installed.backup_path).unwrap().version,
        "0.0.1"
    );
    assert!(discard(&prepared).is_err());
    assert!(installed.checkpoint_cleanup().is_err());
    assert!(
        cleanup_current_linux_bundle(
            &installed.installed_path,
            "0.0.1",
            &"a".repeat(40),
            &origin(),
            &[public_key(7)]
        )
        .is_err()
    );
    assert_eq!(
        cleanup_current_linux_bundle(
            &installed.installed_path,
            "0.0.2",
            &"b".repeat(40),
            &origin(),
            &[public_key(9)]
        )
        .unwrap(),
        0
    );
    assert!(installed.backup_path.exists());
    // No profile-state save occurred: the signed receipt alone recovers the
    // commit once the caller supplies the new runtime's startup identity.
    assert_eq!(
        cleanup_current_linux_bundle(
            &installed.installed_path,
            "0.0.2",
            &"b".repeat(40),
            &origin(),
            &[public_key(7)]
        )
        .unwrap(),
        1
    );
    assert!(!installed.staging_dir.exists());
    assert!(fixture.update.payload_path.exists());
    assert_eq!(
        installed_bundle(&installed.installed_path).unwrap().version,
        "0.0.2"
    );
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn checkpoint_preserves_backups_when_receipt_payload_or_installed_tree_changes() {
    for mutation in 0..4 {
        let fixture = fixture();
        let (prepared, installed) = commit(&fixture);
        match mutation {
            0 => fs::write(
                installed.staging_dir.join(RECEIPT_NAME),
                b"malformed receipt",
            )
            .unwrap(),
            1 => {
                let path = installed.staging_dir.join(RECEIPT_NAME);
                let mut receipt: serde_json::Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                receipt["installed"]["installed_path"] =
                    serde_json::json!(fixture.root.join("another.app"));
                fs::write(path, serde_json::to_vec(&receipt).unwrap()).unwrap();
            }
            2 => fs::write(&prepared.payload_path, b"changed retained payload").unwrap(),
            3 => fs::write(
                installed.installed_path.join("AppRun"),
                b"changed installed launcher",
            )
            .unwrap(),
            _ => unreachable!(),
        }
        assert_eq!(
            cleanup_current_linux_bundle(
                &installed.installed_path,
                "0.0.2",
                &"b".repeat(40),
                &origin(),
                &[public_key(7)]
            )
            .unwrap(),
            0
        );
        assert!(installed.backup_path.exists());
        assert_eq!(
            installed_bundle(&installed.backup_path).unwrap().version,
            "0.0.1"
        );
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn native_exchange_failure_preserves_the_running_installation() {
    let fixture = fixture();
    let prepared = prepare(&fixture.update).unwrap();
    assert!(
        exchange(
            &fixture.update.installed_path,
            &fixture.root.join("missing")
        )
        .is_err()
    );
    assert_eq!(
        installed_bundle(&fixture.update.installed_path)
            .unwrap()
            .version,
        "0.0.1"
    );
    assert_eq!(
        installed_bundle(&prepared.staged_path).unwrap().version,
        "0.0.2"
    );
    discard(&prepared).unwrap();
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn post_exchange_sync_failure_is_a_committed_update() {
    let fixture = fixture();
    let lock = acquire_install_lock(&fixture.update.installed_path).unwrap();
    let prepared = prepare(&fixture.update).unwrap();
    let installed = replace_with_synchronizer(
        &prepared,
        &fixture.update,
        Synchronizer::Test(|_| Err(io::Error::other("injected post-commit sync failure").into())),
    )
    .unwrap();
    assert!(installed.durability_warning);
    assert_eq!(
        installed_bundle(&installed.installed_path).unwrap().version,
        "0.0.2"
    );
    assert_eq!(
        installed_bundle(&installed.backup_path).unwrap().version,
        "0.0.1"
    );
    assert!(installed.staging_dir.join(RECEIPT_NAME).is_file());
    assert!(discard(&prepared).is_err());
    drop(lock);
}
