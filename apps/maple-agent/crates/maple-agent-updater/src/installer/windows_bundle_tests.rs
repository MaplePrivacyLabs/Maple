//! Data-only fixtures: these tests never run a downloaded executable.
use super::*;
use flate2::{Compression, write::GzEncoder};

#[derive(Clone)]
struct Member {
    name: String,
    bytes: Vec<u8>,
    kind: u8,
    mode: u32,
    link: Option<String>,
}

fn bundle(profile: &str) -> Bundle {
    Bundle {
        format: "maple-agent-windows-bundle-v1".into(),
        product: "maple-agent".into(),
        profile: profile.into(),
        version: "0.0.10".into(),
        build: "702".into(),
        source_sha: "a".repeat(40),
        bundle_id: profile_identity(profile).unwrap().into(),
        entrypoint: "maple-agent.exe".into(),
    }
}

fn members(bundle: &Bundle) -> Vec<Member> {
    let mut members: Vec<Member> = DIRS
        .iter()
        .map(|name| Member {
            name: if name.is_empty() {
                ROOT.into()
            } else {
                format!("{ROOT}/{name}")
            },
            bytes: Vec::new(),
            kind: b'5',
            mode: 0o755,
            link: None,
        })
        .collect();
    members.extend(FILES.iter().map(|name| Member {
        name: format!("{ROOT}/{name}"),
        bytes: if *name == DESCRIPTOR {
            serde_json::to_vec(bundle).unwrap()
        } else {
            format!("non-executable fixture: {name}").into_bytes()
        },
        kind: b'0',
        mode: 0o644,
        link: None,
    }));
    members
}

fn private_fixture() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    windows_fs::private_directory(directory.path()).unwrap();
    directory
}

fn write_archive(path: &Path, members: &[Member]) {
    let output = File::create(path).unwrap();
    let mut writer = tar::Builder::new(GzEncoder::new(output, Compression::fast()));
    for member in members {
        let mut header = tar::Header::new_ustar();
        header.set_mode(member.mode);
        header.set_size(member.bytes.len() as u64);
        header.set_entry_type(tar::EntryType::new(member.kind));
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        // Unsafe fixtures reach the real reader instead of tar's safe writer.
        assert!(member.name.len() < 100);
        header.as_mut_bytes()[..100].fill(0);
        header.as_mut_bytes()[..member.name.len()].copy_from_slice(member.name.as_bytes());
        if let Some(link) = &member.link {
            header.set_link_name(link).unwrap();
        }
        header.set_cksum();
        writer.append(&header, member.bytes.as_slice()).unwrap();
    }
    writer
        .into_inner()
        .unwrap()
        .finish()
        .unwrap()
        .sync_all()
        .unwrap();
}

#[test]
fn exact_archive_round_trip_binds_all_nine_files_and_three_directories() {
    let directory = private_fixture();
    let payload = directory.path().join("update.tar.gz");
    let output = directory.path().join("extract");
    fs::create_dir(&output).unwrap();
    let descriptor = bundle("prod");
    write_archive(&payload, &members(&descriptor));
    validate_windows_bundle_archive(&payload, "prod").unwrap();
    assert!(validate_windows_bundle_archive(&payload, "dev").is_err());
    let (expected, actual) = archive(&payload, Some(&output), "prod").unwrap();
    assert_eq!(actual, descriptor);
    assert_eq!(expected.len(), 12);
    assert_eq!(
        expected
            .values()
            .filter(|node| matches!(node, Node::File { .. }))
            .count(),
        9
    );
    assert_eq!(
        expected
            .values()
            .filter(|node| matches!(node, Node::Directory))
            .count(),
        3
    );
    assert_eq!(scan_generation(&output.join(ROOT)).unwrap(), expected);
    fs::write(output.join(ROOT).join("LICENSE.txt"), b"tampered").unwrap();
    assert_ne!(scan_generation(&output.join(ROOT)).unwrap(), expected);
}

#[test]
fn rejected_archive_never_begins_extraction() {
    let directory = private_fixture();
    let payload = directory.path().join("update.tar.gz");
    let output = directory.path().join("extract");
    fs::create_dir(&output).unwrap();
    let valid = members(&bundle("prod"));
    let executable_index = valid
        .iter()
        .position(|entry| entry.name.ends_with("/maple-agent.exe"))
        .unwrap();
    let mut cases = Vec::new();
    let mut missing = valid.clone();
    missing.remove(executable_index);
    cases.push(missing);
    let mut duplicate = valid.clone();
    duplicate.push(valid[executable_index].clone());
    cases.push(duplicate);
    let mut extra = valid.clone();
    extra.push(Member {
        name: format!("{ROOT}/uninstall.exe"),
        bytes: b"not an updater payload".to_vec(),
        kind: b'0',
        mode: 0o644,
        link: None,
    });
    cases.push(extra);
    for name in [
        format!("{ROOT}/../outside"),
        format!("{ROOT}/notices/../../outside"),
        format!("{ROOT}/Maple-Agent.exe"),
        format!("{ROOT}/maple-agent.exe:alternate"),
        format!("{ROOT}/notices\\other"),
        format!("{ROOT}/maple-agent.exe."),
        "wrong-root/maple-agent.exe".into(),
        format!("/{ROOT}/maple-agent.exe"),
    ] {
        let mut altered = valid.clone();
        altered[executable_index].name = name;
        cases.push(altered);
    }
    for (kind, mode, link) in [
        (b'2', 0o777, Some("LICENSE.txt")),
        (b'1', 0o644, Some("LICENSE.txt")),
        (b'6', 0o644, None),
        (b'0', 0o755, None),
    ] {
        let mut altered = valid.clone();
        altered[executable_index].kind = kind;
        altered[executable_index].mode = mode;
        altered[executable_index].link = link.map(str::to_owned);
        if kind != b'0' {
            altered[executable_index].bytes.clear();
        }
        cases.push(altered);
    }
    for (index, case) in cases.iter().enumerate() {
        write_archive(&payload, case);
        assert!(
            archive(&payload, Some(&output), "prod").is_err(),
            "case {index}"
        );
        assert_eq!(
            fs::read_dir(&output).unwrap().count(),
            0,
            "case {index} wrote files before admission"
        );
    }
}

#[test]
fn descriptors_bind_profile_product_release_and_strict_json_fields() {
    let original = serde_json::to_value(bundle("prod")).unwrap();
    assert_eq!(
        Bundle::parse(&serde_json::to_vec(&original).unwrap(), "prod").unwrap(),
        bundle("prod")
    );
    assert!(Bundle::parse(&serde_json::to_vec(&original).unwrap(), "dev").is_err());
    let development = bundle("dev");
    Bundle::parse(&serde_json::to_vec(&development).unwrap(), "dev").unwrap();
    for (field, value) in [
        ("format", "maple-agent-windows-bundle-v2"),
        ("product", "maple-research"),
        ("profile", "local"),
        ("bundle_id", "cloud.opensecret.maple.agent.dev"),
        ("entrypoint", "../maple-agent.exe"),
        ("version", "0.0.10-beta.1"),
        ("version", "0.0.10+other"),
        ("build", "0"),
        ("build", "0702"),
        ("build", "-1"),
        ("build", "1e3"),
        ("build", "1234567890123456789"),
        ("source_sha", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        ("source_sha", "aabbcc"),
    ] {
        let mut altered = original.clone();
        altered[field] = value.into();
        assert!(
            Bundle::parse(&serde_json::to_vec(&altered).unwrap(), "prod").is_err(),
            "accepted {field}={value}"
        );
    }
    let mut extra = original.clone();
    extra["destination"] = "C:\\other".into();
    assert!(Bundle::parse(&serde_json::to_vec(&extra).unwrap(), "prod").is_err());
    assert!(Bundle::parse(&vec![b' '; DESCRIPTOR_LIMIT as usize + 1], "prod").is_err());
    assert!(Bundle::parse(b"", "prod").is_err());
    assert!(Bundle::parse(b"{\"format\":\"one\",\"format\":\"two\"}", "prod").is_err());
}

#[test]
fn descriptor_comparison_rejects_changed_release_version_build_or_source() {
    let descriptor = bundle("prod");
    // This tests the post-authentication field comparison only. Signature
    // verification is covered separately by manifest tests.
    let release = crate::VerifiedRelease {
        manifest: crate::Manifest {
            format: "maple-agent-update-v3".into(),
            product: "maple-agent".into(),
            channel: "stable".into(),
            version: descriptor.version.clone(),
            build: descriptor.build.clone(),
            source_sha: descriptor.source_sha.clone(),
            pub_date: "2026-10-27T00:00:00Z".into(),
            notes: "Data-only identity comparison fixture".into(),
            artifacts: BTreeMap::new(),
            downloads: crate::Downloads {
                macos: None,
                linux_appimage: None,
                linux_bundle: None,
                windows: None,
            },
        },
        envelope: String::new(),
        origin: "https://updates.example.test".parse().unwrap(),
    };
    descriptor.matches_release(&release).unwrap();
    let mut altered = release.clone();
    altered.manifest.version = "0.0.11".into();
    assert!(descriptor.matches_release(&altered).is_err());
    let mut altered = release.clone();
    altered.manifest.build = "703".into();
    assert!(descriptor.matches_release(&altered).is_err());
    let mut altered = release.clone();
    altered.manifest.source_sha = "b".repeat(40);
    assert!(descriptor.matches_release(&altered).is_err());
    assert!(bundle("dev").matches_release(&release).is_err());
}

#[test]
fn current_pointer_cannot_name_a_path_or_ambiguous_generation() {
    let descriptor = bundle("prod");
    let current = Current::new(&descriptor);
    let bytes = serde_json::to_vec(&current).unwrap();
    assert_eq!(Current::parse(&bytes).unwrap(), current);
    assert_eq!(
        current.path(Path::new("installation")),
        Path::new("installation")
            .join("versions")
            .join("0.0.10-702")
    );
    for value in [
        "../0.0.10-702",
        "0.0.10-702/other",
        "0.0.10-702\\other",
        "C:\\0.0.10-702",
        "0.0.10-0702",
        "0.0.10-0",
        "0.0.10--1",
        "0.0.10-beta-702",
        "0.0.10+other-702",
        "0.0.10-702.",
        "0.0.10-702 ",
        "0.0.10-702:other",
        "0.0.10",
    ] {
        let bytes =
            serde_json::to_vec(&serde_json::json!({"format": POINTER_FORMAT, "generation": value}))
                .unwrap();
        assert!(
            Current::parse(&bytes).is_err(),
            "accepted generation {value}"
        );
    }
    for value in [
        serde_json::json!({"format":"other","generation":"0.0.10-702"}),
        serde_json::json!({"format":POINTER_FORMAT,"generation":"0.0.10-702","path":"elsewhere"}),
        serde_json::json!({"format":POINTER_FORMAT}),
        serde_json::json!({"format":POINTER_FORMAT,"generation":702}),
    ] {
        assert!(Current::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    assert!(Current::parse(&vec![b' '; 1025]).is_err());
    assert!(Current::parse(b"").is_err());
}

const PE_OFFSET: usize = 0x80;
const OPTIONAL_OFFSET: usize = PE_OFFSET + 24;
const SECTION_OFFSET: usize = OPTIONAL_OFFSET + 240;

fn put16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn pe_fixture(info: &serde_json::Value) -> Vec<u8> {
    let metadata = serde_json::to_vec(info).unwrap();
    assert!(metadata.len() < 512);
    let mut bytes = vec![0; 0x800];
    bytes[..2].copy_from_slice(b"MZ");
    put32(&mut bytes, 60, PE_OFFSET as u32);
    bytes[PE_OFFSET..PE_OFFSET + 4].copy_from_slice(b"PE\0\0");
    put16(&mut bytes, PE_OFFSET + 4, 0x8664);
    put16(&mut bytes, PE_OFFSET + 6, 2);
    put16(&mut bytes, PE_OFFSET + 20, 240);
    put16(&mut bytes, PE_OFFSET + 22, 2);
    put16(&mut bytes, OPTIONAL_OFFSET, 0x20b);
    put32(&mut bytes, OPTIONAL_OFFSET + 60, 0x200);
    bytes[SECTION_OFFSET..SECTION_OFFSET + 8].copy_from_slice(b".maple\0\0");
    put32(&mut bytes, SECTION_OFFSET + 8, metadata.len() as u32);
    put32(&mut bytes, SECTION_OFFSET + 16, 0x200);
    put32(&mut bytes, SECTION_OFFSET + 20, 0x200);
    put32(&mut bytes, SECTION_OFFSET + 36, 0x40000040);
    let text = SECTION_OFFSET + 40;
    bytes[text..text + 8].copy_from_slice(b".text\0\0\0");
    put32(&mut bytes, text + 8, 16);
    put32(&mut bytes, text + 16, 0x200);
    put32(&mut bytes, text + 20, 0x400);
    put32(&mut bytes, text + 36, 0x60000020);
    bytes[0x200..0x200 + metadata.len()].copy_from_slice(&metadata);
    bytes
}

fn read_pe(bytes: &[u8]) -> Result<serde_json::Value> {
    let directory = private_fixture();
    let path = directory.path().join("fixture.exe");
    fs::write(&path, bytes).unwrap();
    pe_build_info(&path)
}

#[test]
fn pe_metadata_parser_reads_x64_maple_section_without_running_code() {
    let info = serde_json::json!({"profile":"prod","bundle_id":"cloud.opensecret.maple.agent","version":"0.0.10","source_sha":"a".repeat(40)});
    assert_eq!(read_pe(&pe_fixture(&info)).unwrap(), info);
}

#[test]
fn pe_metadata_parser_rejects_header_architecture_and_bounds_changes() {
    let good = pe_fixture(&serde_json::json!({"version":"0.0.10"}));
    let mut cases = Vec::new();
    for (offset, value) in [
        (PE_OFFSET + 4, 0x14c),
        (PE_OFFSET + 6, 0),
        (PE_OFFSET + 6, 97),
        (PE_OFFSET + 20, 111),
        (PE_OFFSET + 20, 4097),
        (PE_OFFSET + 22, 0),
        (PE_OFFSET + 22, 0x2002),
        (OPTIONAL_OFFSET, 0x10b),
    ] {
        let mut altered = good.clone();
        put16(&mut altered, offset, value);
        cases.push(altered);
    }
    for (offset, value) in [
        (60, 63),
        (60, 1024 * 1024 + 1),
        (OPTIONAL_OFFSET + 60, 0x180),
        (OPTIONAL_OFFSET + 60, 1024 * 1024 + 1),
        (SECTION_OFFSET + 20, 0x100),
        (SECTION_OFFSET + 20, 0x800),
        (SECTION_OFFSET + 16, 0xffff_ffff),
    ] {
        let mut altered = good.clone();
        put32(&mut altered, offset, value);
        cases.push(altered);
    }
    let mut missing_mz = good.clone();
    missing_mz[0] = b'X';
    cases.push(missing_mz);
    let mut missing_pe = good.clone();
    missing_pe[PE_OFFSET] = b'X';
    cases.push(missing_pe);
    for length in [
        0,
        63,
        PE_OFFSET + 23,
        OPTIONAL_OFFSET + 239,
        SECTION_OFFSET + 79,
        0x3ff,
    ] {
        cases.push(good[..length].to_vec());
    }
    for (index, bytes) in cases.iter().enumerate() {
        assert!(read_pe(bytes).is_err(), "accepted changed header {index}");
    }
}

#[test]
fn pe_metadata_parser_rejects_duplicate_writeable_executable_or_overlapping_sections() {
    let good = pe_fixture(&serde_json::json!({"version":"0.0.10"}));
    let mut cases = Vec::new();
    for flags in [0xc0000040, 0x60000040, 0x40000000, 0x40] {
        let mut altered = good.clone();
        put32(&mut altered, SECTION_OFFSET + 36, flags);
        cases.push(altered);
    }
    let mut duplicate = good.clone();
    let first: Vec<u8> = duplicate[SECTION_OFFSET..SECTION_OFFSET + 40].to_vec();
    duplicate[SECTION_OFFSET + 40..SECTION_OFFSET + 80].copy_from_slice(&first);
    put32(&mut duplicate, SECTION_OFFSET + 40 + 20, 0x400);
    cases.push(duplicate);
    let mut overlap = good.clone();
    put32(&mut overlap, SECTION_OFFSET + 40 + 20, 0x300);
    cases.push(overlap);
    let mut missing = good.clone();
    missing[SECTION_OFFSET] = b'X';
    cases.push(missing);
    for length in [0, DESCRIPTOR_LIMIT as u32 + 1, 513] {
        let mut altered = good.clone();
        put32(&mut altered, SECTION_OFFSET + 8, length);
        cases.push(altered);
    }
    let mut padding = good.clone();
    padding[0x3ff] = 1;
    cases.push(padding);
    let mut malformed = good.clone();
    malformed[0x200] = b'[';
    cases.push(malformed);
    for (index, bytes) in cases.iter().enumerate() {
        assert!(read_pe(bytes).is_err(), "accepted unsafe section {index}");
    }
}

#[test]
fn uninstall_journal_cannot_authorize_arbitrary_paths() {
    let valid = serde_json::json!({"format":"maple-agent-windows-uninstall-intent-v1","profile":"prod","owner_pid":1,"owner_created":1,"generations":["0.0.9-701"]});
    assert!(UninstallIntent::parse(&serde_json::to_vec(&valid).unwrap(), "prod").is_ok());
    assert!(UninstallIntent::parse(&serde_json::to_vec(&valid).unwrap(), "dev").is_err());
    for name in [
        "../outside",
        "C:\\outside",
        "0.0.9-0",
        "0.0.9-01",
        "0.0.9-701/extra",
        "0.0.9-701:stream",
    ] {
        let mut changed = valid.clone();
        changed["generations"] = serde_json::json!([name]);
        assert!(UninstallIntent::parse(&serde_json::to_vec(&changed).unwrap(), "prod").is_err());
    }
    let mut duplicate = valid.clone();
    duplicate["generations"] = serde_json::json!(["0.0.9-701", "0.0.9-701"]);
    assert!(UninstallIntent::parse(&serde_json::to_vec(&duplicate).unwrap(), "prod").is_err());
    let mut unknown = valid.clone();
    unknown["root"] = serde_json::json!("C:\\outside");
    assert!(UninstallIntent::parse(&serde_json::to_vec(&unknown).unwrap(), "prod").is_err());
    let mut oversized = valid;
    oversized["generations"] = serde_json::Value::Array(
        (0..257)
            .map(|n| serde_json::json!(format!("0.0.9-{}", n + 1)))
            .collect(),
    );
    assert!(UninstallIntent::parse(&serde_json::to_vec(&oversized).unwrap(), "prod").is_err());
}
