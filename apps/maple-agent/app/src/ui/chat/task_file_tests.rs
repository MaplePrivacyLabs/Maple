use std::future::Ready;

use super::*;

/// Serve `bytes` the way the execution computer does: the first read (no
/// version) carries the digest, every read at most `chunk` bytes.
fn serve(
    name: &str,
    bytes: Vec<u8>,
    chunk: usize,
) -> impl FnMut(u64, Option<String>) -> Ready<Result<TaskFileRange, Failure>> {
    let name = name.to_owned();
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    move |offset, version| {
        let start = offset as usize;
        let end = (start + chunk).min(bytes.len());
        std::future::ready(Ok(TaskFileRange {
            name: name.clone(),
            size: bytes.len() as u64,
            version: "v1".into(),
            sha256: version.is_none().then(|| digest.clone()),
            data: STANDARD.encode(&bytes[start..end]),
        }))
    }
}

fn listing(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn code(failure: Failure) -> &'static str {
    match failure {
        Failure::Local(key, _) => key,
        other => panic!("unexpected failure {:?}", other.raw_message()),
    }
}

#[tokio::test]
async fn a_download_is_whole_checked_and_never_replaces_a_file() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = b"%PDF-1.7 a small report".to_vec();
    let saved = download(directory.path(), 5, serve("report.pdf", bytes.clone(), 5))
        .await
        .unwrap();
    assert!(saved.marked);
    let saved = saved.path;
    assert_eq!(saved.file_name().unwrap(), "report.pdf");
    assert_eq!(std::fs::read(&saved).unwrap(), bytes);
    let again = download(
        directory.path(),
        5,
        serve("report.pdf", b"newer".to_vec(), 5),
    )
    .await
    .unwrap()
    .path;
    assert_eq!(again.file_name().unwrap(), "report (1).pdf");
    assert_eq!(std::fs::read(&saved).unwrap(), bytes);
    assert_eq!(listing(directory.path()), ["report (1).pdf", "report.pdf"]);
    let empty = download(directory.path(), 5, serve("empty.txt", Vec::new(), 5))
        .await
        .unwrap()
        .path;
    assert_eq!(std::fs::read(&empty).unwrap(), b"");
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(saved.as_os_str().as_bytes()).unwrap();
        let mut value = [0u8; 128];
        // SAFETY: the buffer and both strings outlive the call.
        let len = unsafe {
            libc::getxattr(
                path.as_ptr(),
                c"com.apple.quarantine".as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        assert!(len > 0, "the download is quarantined");
        assert!(value[..len as usize].starts_with(b"0081;"));
    }
    // The mark is written on the part file and must survive its rename.
    #[cfg(windows)]
    {
        let mut stream = saved.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        let mark = std::fs::read_to_string(std::path::PathBuf::from(stream)).unwrap();
        assert!(
            mark.contains("ZoneId=3"),
            "the download has the mark of the web"
        );
    }
}

#[tokio::test]
async fn a_damaged_or_changed_download_leaves_nothing_behind() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = b"0123456789abcdef".to_vec();

    let mut honest = serve("data.csv", bytes.clone(), 4);
    let tampered = move |offset: u64, version: Option<String>| {
        let mut range = honest(offset, version).into_inner().unwrap();
        if offset == 8 {
            range.data = STANDARD.encode(b"XXXX");
        }
        std::future::ready(Ok(range))
    };
    let failure = download(directory.path(), 4, tampered).await.unwrap_err();
    assert_eq!(code(failure), "chat-file-damaged");

    let mut honest = serve("data.csv", bytes.clone(), 4);
    let changed = move |offset: u64, version: Option<String>| {
        let mut range = honest(offset, version).into_inner().unwrap();
        if offset >= 8 {
            range.version = "v2".into();
        }
        std::future::ready(Ok(range))
    };
    let failure = download(directory.path(), 4, changed).await.unwrap_err();
    assert_eq!(code(failure), "runtime-error-task-file-changed");

    let mut honest = serve("data.csv", bytes.clone(), 4);
    let unsigned = move |offset: u64, version: Option<String>| {
        let mut range = honest(offset, version).into_inner().unwrap();
        range.sha256 = None;
        std::future::ready(Ok(range))
    };
    let failure = download(directory.path(), 4, unsigned).await.unwrap_err();
    assert_eq!(code(failure), "chat-file-damaged");

    let mut honest = serve("data.csv", bytes, 4);
    let overlong = move |offset: u64, version: Option<String>| {
        let mut range = honest(offset, version).into_inner().unwrap();
        if offset == 12 {
            range.data = STANDARD.encode(b"cdefXX");
        }
        std::future::ready(Ok(range))
    };
    let failure = download(directory.path(), 4, overlong).await.unwrap_err();
    assert_eq!(code(failure), "chat-file-damaged");

    // A short piece before the end would let a host stretch a download
    // into endless tiny reads.
    let short = serve("data.csv", b"0123456789".to_vec(), 3);
    let failure = download(directory.path(), 4, short).await.unwrap_err();
    assert_eq!(code(failure), "chat-file-damaged");

    assert!(listing(directory.path()).is_empty());
}

#[test]
fn names_from_another_computer_are_made_safe_to_save() {
    for (name, saved) in [
        ("report.pdf", "report.pdf"),
        ("../../etc/passwd", "passwd"),
        (r"C:\Users\me\notes.txt", "notes.txt"),
        (".env", "env"),
        ("  ..hidden.txt. ", "hidden.txt"),
        ("a<b>:c|d?.txt", "abcd.txt"),
        ("CON.txt", "download CON.txt"),
        ("invoice\u{202E}fdp.exe", "invoicefdp.exe"),
        ("zero\u{200B}width.pdf", "zerowidth.pdf"),
        ("lpt1", "download lpt1"),
        ("", "download"),
        ("...", "download"),
    ] {
        assert_eq!(saved_name(name), saved, "{name:?}");
    }
    assert!(saved_name(&"x".repeat(400)).len() <= MAX_SAVED_NAME_BYTES);
    assert_eq!(link_name("out/My%20Report.pdf"), "My Report.pdf");
    assert_eq!(link_name(r"C:\work\plan.docx"), "plan.docx");
    assert_eq!(link_name("file:///tmp/a.csv#row=2"), "a.csv");
    assert_eq!(link_name("out/notes#1.txt"), "notes#1.txt");
    assert_eq!(numbered("report.pdf", 0), "report.pdf");
    assert_eq!(numbered("report.pdf", 2), "report (2).pdf");
    assert_eq!(numbered("archive.tar.gz", 1), "archive.tar (1).gz");
    assert_eq!(numbered("README", 3), "README (3)");
}

#[test]
fn only_files_that_cannot_run_anything_open_in_place() {
    let directory = tempfile::tempdir().unwrap();
    let make = |name: &str| {
        let path = directory.path().join(name);
        std::fs::write(&path, b"x").unwrap();
        path
    };
    for name in [
        "report.PDF",
        "chart.png",
        "data.csv",
        "notes.md",
        "plan.docx",
    ] {
        assert!(opens_in_place(&make(name)), "{name}");
    }
    for name in [
        "run.sh",
        "setup.command",
        "tool.exe",
        "macro.xlsm",
        "script.py",
        "shortcut.lnk",
        "page.html",
        "sheet.ods",
        "noextension",
    ] {
        assert!(!opens_in_place(&make(name)), "{name}");
    }
    // A folder never opens in place: it may be a bundle whatever its name.
    let folder = directory.path().join("out");
    std::fs::create_dir(&folder).unwrap();
    assert!(!opens_in_place(&folder));
    assert!(!opens_in_place(&directory.path().join("missing.pdf")));
    #[cfg(unix)]
    {
        let link = directory.path().join("link.pdf");
        std::os::unix::fs::symlink(directory.path().join("run.sh"), &link).unwrap();
        assert!(!opens_in_place(&link));
    }
}
