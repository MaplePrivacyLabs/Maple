use std::sync::Arc;

use rusqlite::params;

use super::*;
use crate::store::{Durability, StorePaths};

const TASK: &str = "00000000-0000-7000-8000-000000000001";

struct Fixture {
    _store_dir: tempfile::TempDir,
    project: tempfile::TempDir,
    outside: tempfile::TempDir,
    store: Arc<Store>,
    account: AccountId,
}

fn fixture() -> Fixture {
    let store_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(StorePaths::new(store_dir.path())).unwrap());
    let account = store.upsert_account("alice", 1).unwrap();
    let root = project.path().to_str().unwrap().to_owned();
    store
        .write(Durability::Durable, |tx| {
            tx.execute(
                "INSERT INTO projects(account_id,root,last_used_ms) VALUES(?1,?2,1)",
                params![account.0, root],
            )?;
            let project = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO tasks(uuid,account_id,project_id,kind,state,title,title_fold,created_ms,updated_ms) \
                 VALUES(?1,?2,?3,'desktop','active','fixture','fixture',1,1)",
                params![TASK, account.0, project],
            )?;
            Ok(())
        })
        .unwrap();
    Fixture {
        _store_dir: store_dir,
        project,
        outside,
        store,
        account,
    }
}

impl Fixture {
    fn write(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = self.project.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        std::fs::canonicalize(path).unwrap()
    }

    fn locate(&self, link: &str) -> Result<TaskFileLocation, TaskFileRefusal> {
        self.store
            .locate_task_file(self.account, TASK, link)
            .unwrap()
    }

    fn read(
        &self,
        link: &str,
        offset: u64,
        version: Option<&str>,
    ) -> Result<TaskFileRange, TaskFileRefusal> {
        self.store
            .read_task_file(self.account, TASK, link, offset, 4, version)
            .unwrap()
    }
}

fn bytes(range: &TaskFileRange) -> Vec<u8> {
    STANDARD.decode(&range.data).unwrap()
}

#[test]
fn links_lead_only_inside_the_task_folder() {
    let fixture = fixture();
    let report = fixture.write("out/My Report.pdf", b"%PDF");
    let folder = report.parent().unwrap().to_owned();
    for link in [
        "out/My Report.pdf",
        "out/My%20Report.pdf",
        "./out/../out/My Report.pdf",
        report.to_str().unwrap(),
        &format!("file://{}", report.to_str().unwrap().replace(' ', "%20")),
    ] {
        assert_eq!(
            fixture.locate(link),
            Ok(TaskFileLocation {
                path: report.clone(),
                folder: false
            }),
            "{link}"
        );
    }
    assert_eq!(
        fixture.locate("out"),
        Ok(TaskFileLocation {
            path: folder,
            folder: true
        })
    );
    std::fs::write(fixture.outside.path().join("secret.txt"), b"no").unwrap();
    let secret = fixture.outside.path().join("secret.txt");
    for link in [secret.to_str().unwrap(), "../", "."] {
        assert_eq!(
            fixture.locate(link),
            Err(TaskFileRefusal::Outside),
            "{link}"
        );
    }
    for link in ["out/missing.pdf", "https://example.com/a.pdf", "#top", ""] {
        assert_eq!(
            fixture.locate(link),
            Err(TaskFileRefusal::Missing),
            "{link}"
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&secret, fixture.project.path().join("link.txt")).unwrap();
        assert_eq!(fixture.locate("link.txt"), Err(TaskFileRefusal::Outside));
        assert_eq!(
            fixture.read("link.txt", 0, None),
            Err(TaskFileRefusal::Outside)
        );
        std::os::unix::fs::symlink(&report, fixture.project.path().join("inside.pdf")).unwrap();
        assert_eq!(fixture.locate("inside.pdf").unwrap().path, report);
    }
}

#[test]
fn a_download_reads_one_version_of_the_file() {
    let fixture = fixture();
    fixture.write("notes.txt", b"hello world");
    let first = fixture.read("notes.txt", 0, None).unwrap();
    assert_eq!(first.name, "notes.txt");
    assert_eq!(first.size, 11);
    assert_eq!(
        first.sha256.as_deref(),
        Some(hex(&Sha256::digest(b"hello world")).as_str())
    );
    assert_eq!(bytes(&first), b"hell");
    let next = fixture.read("notes.txt", 8, Some(&first.version)).unwrap();
    assert_eq!(bytes(&next), b"rld");
    assert_eq!(next.sha256, None);
    assert_eq!(next.version, first.version);
    let end = fixture.read("notes.txt", 11, Some(&first.version)).unwrap();
    assert!(bytes(&end).is_empty());
    assert!(matches!(
        fixture.store.read_task_file(
            fixture.account,
            TASK,
            "notes.txt",
            12,
            4,
            Some(&first.version)
        ),
        Err(StoreError::InvalidInput)
    ));
    // The digest comes only with a read from the start.
    assert!(matches!(
        fixture
            .store
            .read_task_file(fixture.account, TASK, "notes.txt", 4, 4, None),
        Err(StoreError::InvalidInput)
    ));
    assert!(matches!(
        fixture.store.read_task_file(
            fixture.account,
            TASK,
            "notes.txt",
            0,
            MAX_TASK_FILE_RANGE_BYTES + 1,
            None
        ),
        Err(StoreError::InvalidInput)
    ));

    // A rewrite, even to the same size, is a new version.
    std::thread::sleep(std::time::Duration::from_millis(5));
    fixture.write("notes.txt", b"HELLO WORLD");
    assert_eq!(
        fixture.read("notes.txt", 4, Some(&first.version)),
        Err(TaskFileRefusal::Changed)
    );
    let again = fixture.read("notes.txt", 0, None).unwrap();
    assert_ne!(again.version, first.version);
    assert_eq!(bytes(&again), b"HELL");
}

#[test]
fn folders_large_files_and_other_accounts_are_refused() {
    let fixture = fixture();
    fixture.write("out/a.txt", b"a");
    assert_eq!(fixture.read("out", 0, None), Err(TaskFileRefusal::Folder));
    let large = fixture.write("big.bin", b"");
    std::fs::File::options()
        .write(true)
        .open(&large)
        .unwrap()
        .set_len(MAX_TASK_FILE_BYTES + 1)
        .unwrap();
    assert_eq!(
        fixture.read("big.bin", 0, None),
        Err(TaskFileRefusal::TooLarge)
    );
    let bob = fixture.store.upsert_account("bob", 1).unwrap();
    assert!(
        fixture
            .store
            .locate_task_file(bob, TASK, "out/a.txt")
            .is_err()
    );
    assert!(
        fixture
            .store
            .read_task_file(bob, TASK, "out/a.txt", 0, 1, None)
            .is_err()
    );
    #[cfg(unix)]
    {
        let fifo = fixture.project.path().join("pipe");
        let path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert_eq!(fixture.locate("pipe"), Err(TaskFileRefusal::Missing));
        assert_eq!(fixture.read("pipe", 0, None), Err(TaskFileRefusal::Missing));
    }
}

#[test]
fn candidates_admit_paths_and_file_urls_only() {
    for (destination, expected) in [
        ("shot.png", vec!["shot.png"]),
        (
            "out/My%20Shot.png",
            vec!["out/My%20Shot.png", "out/My Shot.png"],
        ),
        ("/work/a.gif", vec!["/work/a.gif"]),
        ("file:///work/a%20b.png", vec!["/work/a b.png"]),
        ("file://localhost/work/a.png", vec!["/work/a.png"]),
        ("file:///work/a.png?v=2#top", vec!["/work/a.png"]),
        (r"C:\work\a.png", vec![r"C:\work\a.png"]),
    ] {
        assert_eq!(
            local_candidates(destination),
            expected.into_iter().map(PathBuf::from).collect::<Vec<_>>(),
            "{destination}"
        );
    }
    for destination in [
        "",
        "https://example.com/a.png",
        "data:image/png;base64,AAAA",
        "maple-attachment://0123456789abcdef0123456789abcdef",
        "file://server/share/a.png",
        "javascript:alert(1)",
    ] {
        assert!(local_candidates(destination).is_empty(), "{destination}");
    }
    assert!(local_candidates(&"a".repeat(MAX_LINK_BYTES + 1)).is_empty());
}

fn check_within(root: &ProjectRoot, cases: &[(&str, Option<&str>)]) {
    for (candidate, expected) in cases {
        assert_eq!(
            root.within(Path::new(candidate)),
            expected.map(PathBuf::from),
            "{candidate}"
        );
    }
}

/// Only the text decides; nothing outside the project is looked up, so
/// a share on another computer is never contacted.
#[cfg(unix)]
#[test]
fn only_paths_inside_the_project_are_looked_up() {
    let root = ProjectRoot {
        recorded: PathBuf::from("/work/project"),
        canonical: PathBuf::from("/private/work/project"),
    };
    check_within(
        &root,
        &[
            ("out/a.png", Some("/private/work/project/out/a.png")),
            ("./out/../b.png", Some("/private/work/project/b.png")),
            ("/work/project/c.png", Some("/work/project/c.png")),
            (
                "/private/work/project/d.png",
                Some("/private/work/project/d.png"),
            ),
            ("../e.png", None),
            ("out/../../f.png", None),
            ("/etc/passwd", None),
            ("/work/projectx/g.png", None),
            ("//attacker.example/share/a.png", None),
            ("/work/project", None),
            ("/work/project/..", None),
        ],
    );
}

#[cfg(windows)]
#[test]
fn only_paths_inside_the_project_are_looked_up() {
    let root = ProjectRoot {
        recorded: PathBuf::from(r"C:\work\project"),
        canonical: PathBuf::from(r"\\?\C:\work\project"),
    };
    check_within(
        &root,
        &[
            (r"out\a.png", Some(r"\\?\C:\work\project\out\a.png")),
            // Joining onto a `\\?\` path splits `/` and works out `.`
            // and `..` too.
            ("out/a.png", Some(r"\\?\C:\work\project\out\a.png")),
            ("./out/../b.png", Some(r"\\?\C:\work\project\b.png")),
            (r"C:\work\project\c.png", Some(r"C:\work\project\c.png")),
            ("C:/work/project/f.png", Some(r"C:\work\project\f.png")),
            // Drive letters compare in either case.
            (r"c:\work\project\d.png", Some(r"C:\work\project\d.png")),
            (r"..\e.png", None),
            ("../e.png", None),
            (r"C:\Windows\win.ini", None),
            (r"\\attacker.example\share\a.png", None),
            ("//attacker.example/share/a.png", None),
            (r"\\?\UNC\attacker.example\share\a.png", None),
            (r"\\.\pipe\x", None),
            (r"C:a.png", None),
        ],
    );
}
