use super::super::session_support::*;
use pi_ai::env::CancellationToken;
use pi_coding_agent::{
    config::HostConfig,
    core::session_manager::{
        SessionInfo, SessionManager, find_most_recent_session, load_entries_from_file,
    },
};
use pi_testkit::VirtualEnv;
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs::{self, File, FileTimes, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

const HEADER_SCAN_LIMIT_BYTES: usize = 1024 * 1024;
const HEADER: &str = "{\"type\":\"session\",\"id\":\"abc\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n";
const MESSAGE: &str = "{\"type\":\"message\",\"id\":\"1\",\"parentId\":null,\"timestamp\":\"2025-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n";

struct Fixture {
    temp: tempfile::TempDir,
    env: Arc<VirtualEnv>,
    config: Arc<HostConfig>,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("pi-session-test-")
            .tempdir()
            .unwrap();
        let config = config(temp.path());
        Self {
            temp,
            env: env(),
            config,
        }
    }

    fn dir(&self) -> &str {
        self.temp.path().to_str().unwrap()
    }
    fn path(&self, name: &str) -> PathBuf {
        self.temp.path().join(name)
    }
    fn open(&self, path: &Path) -> SessionManager {
        SessionManager::open(
            path.to_str().unwrap(),
            Some(self.dir()),
            None,
            self.env.clone(),
            self.config.clone(),
        )
        .unwrap()
    }
    fn create(&self, cwd: &str) -> SessionManager {
        SessionManager::create(
            cwd,
            Some(self.dir()),
            None,
            self.env.clone(),
            self.config.clone(),
        )
        .unwrap()
    }
    fn recent(&self) -> Option<String> {
        find_most_recent_session(self.dir(), None, &self.config)
    }
    fn projects(&self) -> (PathBuf, PathBuf) {
        let a = self.path("project-a");
        let b = self.path("project-b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        (a, b)
    }
    fn persisted(&self, cwd: &Path, label: &str) -> String {
        let mut session = self.create(cwd.to_str().unwrap());
        session.append_message(user(label)).unwrap();
        session
            .append_message(assistant(&format!("reply to {label}")))
            .unwrap();
        session
            .get_session_file()
            .expect("Expected persisted session file")
    }
}

fn write_header(file: &Path, cwd: &str, id: &str, prefix: &str) {
    let header = json!({"type":"session", "version":3, "id":id, "timestamp":"2025-01-01T00:00:00Z", "cwd":cwd});
    fs::write(file, format!("{prefix}{header}\n")).unwrap();
}

// The upstream 10 ms sleeps only establish mtime order. Set that order explicitly.
fn modified_at(file: &Path, milliseconds: u64) {
    File::open(file)
        .unwrap()
        .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_millis(milliseconds)))
        .unwrap();
}

mod load_entries_from_file {
    use super::*;

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > loadEntriesFromFile > returns empty array for non-existent file
    #[test]
    fn returns_empty_array_for_non_existent_file() {
        let f = Fixture::new();
        assert!(
            super::load_entries_from_file(f.path("nonexistent.jsonl").to_str().unwrap())
                .unwrap()
                .is_empty()
        );
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > loadEntriesFromFile > returns empty array for empty file
    #[test]
    fn returns_empty_array_for_empty_file() {
        let f = Fixture::new();
        let file = f.path("empty.jsonl");
        fs::write(&file, "").unwrap();
        assert!(
            super::load_entries_from_file(file.to_str().unwrap())
                .unwrap()
                .is_empty()
        );
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > loadEntriesFromFile > returns empty array for file without valid session header
    #[test]
    fn returns_empty_array_for_file_without_valid_session_header() {
        let f = Fixture::new();
        let file = f.path("no-header.jsonl");
        fs::write(&file, "{\"type\":\"message\",\"id\":\"1\"}\n").unwrap();
        assert!(
            super::load_entries_from_file(file.to_str().unwrap())
                .unwrap()
                .is_empty()
        );
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > loadEntriesFromFile > returns empty array for malformed JSON
    #[test]
    fn returns_empty_array_for_malformed_json() {
        let f = Fixture::new();
        let file = f.path("malformed.jsonl");
        fs::write(&file, "not json\n").unwrap();
        assert!(
            super::load_entries_from_file(file.to_str().unwrap())
                .unwrap()
                .is_empty()
        );
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > loadEntriesFromFile > loads valid session file
    #[test]
    fn loads_valid_session_file() {
        let f = Fixture::new();
        let file = f.path("valid.jsonl");
        fs::write(&file, format!("{HEADER}{MESSAGE}")).unwrap();
        let entries = super::load_entries_from_file(file.to_str().unwrap()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind(), "session");
        assert_eq!(entries[1].kind(), "message");
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > loadEntriesFromFile > skips malformed lines but keeps valid ones
    #[test]
    fn skips_malformed_lines_but_keeps_valid_ones() {
        let f = Fixture::new();
        let file = f.path("mixed.jsonl");
        fs::write(&file, format!("{HEADER}not valid json\n{MESSAGE}")).unwrap();
        assert_eq!(
            super::load_entries_from_file(file.to_str().unwrap())
                .unwrap()
                .len(),
            2
        );
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > loadEntriesFromFile > adds a newline after an unterminated valid record
    #[test]
    fn adds_a_newline_after_an_unterminated_valid_record() {
        let f = Fixture::new();
        let file = f.path("unterminated.jsonl");
        let content = format!("{HEADER}{}", MESSAGE.trim_end_matches('\n'));
        fs::write(&file, &content).unwrap();
        assert_eq!(
            super::load_entries_from_file(file.to_str().unwrap())
                .unwrap()
                .len(),
            2
        );
        assert_eq!(fs::read_to_string(file).unwrap(), format!("{content}\n"));
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > loadEntriesFromFile > adds a newline after an unterminated malformed final fragment
    #[test]
    fn adds_a_newline_after_an_unterminated_malformed_final_fragment() {
        let f = Fixture::new();
        let file = f.path("malformed-tail.jsonl");
        let content = format!("{HEADER}{{\"type\":\"message\"");
        fs::write(&file, &content).unwrap();
        assert_eq!(
            super::load_entries_from_file(file.to_str().unwrap())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(fs::read_to_string(file).unwrap(), format!("{content}\n"));
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > loadEntriesFromFile > does not modify an unterminated non-session file
    #[test]
    fn does_not_modify_an_unterminated_non_session_file() {
        let f = Fixture::new();
        let file = f.path("invalid.jsonl");
        let content = "{\"type\":\"message\",\"id\":\"1\"}";
        fs::write(&file, content).unwrap();
        assert!(
            super::load_entries_from_file(file.to_str().unwrap())
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read_to_string(file).unwrap(), content);
    }

    fn reads_cwd(prefix: &str, session_id: &str) {
        let f = Fixture::new();
        let file = f.path("header.jsonl");
        let cwd = f.path("stored-project");
        write_header(&file, cwd.to_str().unwrap(), session_id, prefix);
        let session = f.open(&file);
        assert_eq!(session.get_session_id(), session_id);
        assert_eq!(session.get_cwd(), cwd.to_str().unwrap());
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > loadEntriesFromFile > reads cwd from a session with leading blank lines
    #[test]
    fn reads_cwd_from_a_session_with_leading_blank_lines() {
        reads_cwd("\n  \n", "leading-blank");
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > loadEntriesFromFile > reads cwd from a session with leading malformed lines
    #[test]
    fn reads_cwd_from_a_session_with_leading_malformed_lines() {
        reads_cwd("not json\n{broken json\n", "leading-malformed");
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > loadEntriesFromFile > reads cwd from a session with a multi-buffer header
    #[test]
    fn reads_cwd_from_a_session_with_a_multi_buffer_header() {
        reads_cwd("", &"a".repeat(8192));
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > loadEntriesFromFile > opens compatible sessions beyond the discovery scan limit
    #[test]
    fn opens_compatible_sessions_beyond_the_discovery_scan_limit() {
        let f = Fixture::new();
        let cwd = f.path("stored-project");
        let override_cwd = f.path("override-project");
        for (name, id, prefix) in [
            (
                "large-header",
                "a".repeat(HEADER_SCAN_LIMIT_BYTES + 1),
                String::new(),
            ),
            (
                "large-prefix",
                "large-prefix".into(),
                format!("{}\n", "x".repeat(HEADER_SCAN_LIMIT_BYTES + 1)),
            ),
        ] {
            let file = f.path(&format!("{name}.jsonl"));
            write_header(&file, cwd.to_str().unwrap(), &id, &prefix);
            for override_cwd in [None, Some(override_cwd.to_str().unwrap())] {
                let session = SessionManager::open(
                    file.to_str().unwrap(),
                    Some(f.dir()),
                    override_cwd,
                    f.env.clone(),
                    f.config.clone(),
                )
                .unwrap();
                assert_eq!(session.get_session_id(), id.as_str());
                assert_eq!(
                    session.get_cwd(),
                    override_cwd.unwrap_or(cwd.to_str().unwrap())
                );
            }
        }
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > loadEntriesFromFile > opens session files larger than Node's max string length
    #[test]
    fn opens_session_files_larger_than_nodes_max_string_length() {
        let f = Fixture::new();
        let file = f.path("large.jsonl");
        write_header(&file, "/tmp", "abc", "");
        // node:buffer.constants.MAX_STRING_LENGTH, verified with pinned Node 22.23.2.
        // Keep the sparse file larger than the JS string limit without allocating its size.
        const NODE_MAX_STRING_LENGTH: u64 = 536_870_888;
        const STRIDE: u64 = 16 * 1024 * 1024;
        {
            let mut fd = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&file)
                .unwrap();
            let mut offset = STRIDE;
            while offset <= NODE_MAX_STRING_LENGTH + STRIDE {
                fd.seek(SeekFrom::Start(offset)).unwrap();
                fd.write_all(b"\n").unwrap();
                offset += STRIDE;
            }
        }
        OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(MESSAGE.as_bytes())
            .unwrap();
        assert!(fs::metadata(&file).unwrap().len() > NODE_MAX_STRING_LENGTH);
        let session = f.open(&file);
        assert_eq!(session.get_session_id(), "abc");
        assert_eq!(session.get_entries().len(), 1);
        assert_eq!(
            observed(&session.build_session_context().messages),
            json!([{"role":"user", "content":"hi", "timestamp":1}])
        );
    }
}

mod find_most_recent_session {
    use super::*;

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > findMostRecentSession > returns null for empty directory
    #[test]
    fn returns_null_for_empty_directory() {
        assert_eq!(Fixture::new().recent(), None);
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > findMostRecentSession > returns null for non-existent directory
    #[test]
    fn returns_null_for_non_existent_directory() {
        let f = Fixture::new();
        assert_eq!(
            super::find_most_recent_session(
                f.path("nonexistent").to_str().unwrap(),
                None,
                &f.config
            ),
            None
        );
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > findMostRecentSession > ignores non-jsonl files
    #[test]
    fn ignores_non_jsonl_files() {
        let f = Fixture::new();
        fs::write(f.path("file.txt"), "hello").unwrap();
        fs::write(f.path("file.json"), "{}").unwrap();
        assert_eq!(f.recent(), None);
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > findMostRecentSession > ignores jsonl files without valid session header
    #[test]
    fn ignores_jsonl_files_without_valid_session_header() {
        let f = Fixture::new();
        fs::write(f.path("invalid.jsonl"), "{\"type\":\"message\"}\n").unwrap();
        assert_eq!(f.recent(), None);
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > findMostRecentSession > returns single valid session file
    #[test]
    fn returns_single_valid_session_file() {
        let f = Fixture::new();
        let file = f.path("session.jsonl");
        fs::write(&file, HEADER).unwrap();
        assert_eq!(f.recent().as_deref(), file.to_str());
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > findMostRecentSession > returns most recently modified session
    #[test]
    fn returns_most_recently_modified_session() {
        let f = Fixture::new();
        let older = f.path("older.jsonl");
        let newer = f.path("newer.jsonl");
        fs::write(&older, HEADER.replace("abc", "old")).unwrap();
        fs::write(&newer, HEADER.replace("abc", "new")).unwrap();
        modified_at(&older, NOW as u64);
        modified_at(&newer, NOW as u64 + 10);
        assert_eq!(f.recent().as_deref(), newer.to_str());
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > findMostRecentSession > skips invalid files and returns valid one
    #[test]
    fn skips_invalid_files_and_returns_valid_one() {
        let f = Fixture::new();
        let invalid = f.path("invalid.jsonl");
        let valid = f.path("valid.jsonl");
        fs::write(&invalid, "{\"type\":\"not-session\"}\n").unwrap();
        fs::write(&valid, HEADER).unwrap();
        modified_at(&invalid, NOW as u64);
        modified_at(&valid, NOW as u64 + 10);
        assert_eq!(f.recent().as_deref(), valid.to_str());
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > findMostRecentSession > skips oversized corrupt files and returns a valid session
    #[test]
    fn skips_oversized_corrupt_files_and_returns_a_valid_session() {
        let f = Fixture::new();
        let valid = f.path("valid.jsonl");
        fs::write(
            f.path("oversized.jsonl"),
            "x".repeat(HEADER_SCAN_LIMIT_BYTES + 1),
        )
        .unwrap();
        fs::write(&valid, HEADER).unwrap();
        assert_eq!(f.recent().as_deref(), valid.to_str());
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > findMostRecentSession > filters most recent session by cwd
    #[test]
    fn filters_most_recent_session_by_cwd() {
        let f = Fixture::new();
        let project_a = f.path("project-a");
        let project_b = f.path("project-b");
        let file_a = f.path("a.jsonl");
        let file_b = f.path("b.jsonl");
        for (file, id, cwd) in [(&file_a, "a", &project_a), (&file_b, "b", &project_b)] {
            fs::write(file, format!("{}\n", json!({"type":"session", "id":id, "timestamp":"2025-01-01T00:00:00Z", "cwd":cwd}))).unwrap();
        }
        modified_at(&file_a, NOW as u64);
        modified_at(&file_b, NOW as u64 + 10);
        assert_eq!(
            super::find_most_recent_session(f.dir(), project_a.to_str(), &f.config).as_deref(),
            file_a.to_str()
        );
        assert_eq!(
            super::find_most_recent_session(f.dir(), project_b.to_str(), &f.config).as_deref(),
            file_b.to_str()
        );
    }
}

mod custom_flat_session_directory {
    use super::*;

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > SessionManager custom flat session directory > scopes current-folder APIs by cwd while listing all flat sessions
    #[tokio::test(flavor = "current_thread")]
    async fn scopes_current_folder_apis_by_cwd_while_listing_all_flat_sessions() {
        let f = Fixture::new();
        let (project_a, project_b) = f.projects();
        let session_a = f.persisted(&project_a, "from A");
        f.env.advance(10).await;
        let session_b = f.persisted(&project_b, "from B");
        let current_a = SessionManager::list(
            project_a.to_str().unwrap(),
            Some(f.dir()),
            None,
            None,
            &f.config,
        )
        .await
        .unwrap();
        assert_eq!(
            current_a
                .into_iter()
                .map(|session| session.path)
                .collect::<Vec<_>>(),
            std::slice::from_ref(&session_a)
        );
        let all = SessionManager::list_all(Some(f.dir()), None, None, &f.config)
            .await
            .unwrap();
        assert_eq!(
            all.into_iter()
                .map(|session| session.path)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([session_a.clone(), session_b])
        );
        let continued_a = SessionManager::continue_recent(
            project_a.to_str().unwrap(),
            Some(f.dir()),
            f.env.clone(),
            f.config.clone(),
        )
        .unwrap();
        assert_eq!(continued_a.get_session_file(), Some(session_a));
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > SessionManager custom flat session directory > rejects a cancelled session listing
    #[tokio::test(flavor = "current_thread")]
    async fn rejects_a_cancelled_session_listing() {
        let f = Fixture::new();
        let (project_a, project_b) = f.projects();
        f.persisted(&project_a, "from A");
        f.persisted(&project_b, "from B");
        let signal = CancellationToken::new();
        let mut progress = |_loaded, _total, partial: Option<&[SessionInfo]>| {
            if partial.is_some() {
                signal.cancel();
            }
        };
        let error =
            SessionManager::list_all(Some(f.dir()), Some(&mut progress), Some(&signal), &f.config)
                .await
                .expect_err("cancelled listing must reject");
        assert_eq!(error.name, "AbortError");
        let error = SessionManager::list_all(None, None, Some(&signal), &f.config)
            .await
            .expect_err("already-cancelled listing must reject");
        assert_eq!(error.name, "AbortError");
    }
}

mod set_session_file_with_corrupted_files {
    use super::*;

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > SessionManager.setSessionFile with corrupted files > truncates and rewrites empty file with valid header
    #[test]
    fn truncates_and_rewrites_empty_file_with_valid_header() {
        let f = Fixture::new();
        let file = f.path("empty.jsonl");
        fs::write(&file, "").unwrap();
        let session = f.open(&file);
        assert!(!session.get_session_id().is_empty());
        assert_eq!(session.get_header().unwrap().kind(), "session");
        let content = fs::read_to_string(file).unwrap();
        let lines = content
            .trim()
            .split('\n')
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["type"], "session");
        assert_eq!(header["id"], observed(&session.get_session_id()));
    }

    fn assert_preserved(file_name: &str, original: &str) {
        let f = Fixture::new();
        let file = f.path(file_name);
        fs::write(&file, original).unwrap();
        let Err(error) = SessionManager::open(
            file.to_str().unwrap(),
            Some(f.dir()),
            None,
            f.env.clone(),
            f.config.clone(),
        ) else {
            panic!("invalid file must reject");
        };
        assert_eq!(
            error.to_string(),
            format!("Session file is not a valid pi session: {}", file.display())
        );
        assert_eq!(fs::read(file).unwrap(), original.as_bytes());
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > SessionManager.setSessionFile with corrupted files > throws and preserves non-empty file without valid header
    #[test]
    fn throws_and_preserves_non_empty_file_without_valid_header() {
        assert_preserved(
            "no-header.jsonl",
            "{\"type\":\"message\",\"id\":\"abc\",\"parentId\":\"orphaned\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"message\":{\"role\":\"assistant\",\"content\":\"test\"}}\n",
        );
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > SessionManager.setSessionFile with corrupted files > throws and preserves non-session JSONL files
    #[test]
    fn throws_and_preserves_non_session_jsonl_files() {
        assert_preserved(
            "not-a-session.log",
            "{\"type\":\"event\",\"data\":\"not a session\"}\n",
        );
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > SessionManager.setSessionFile with corrupted files > preserves explicit session file path when recovering from corrupted file
    #[test]
    fn preserves_explicit_session_file_path_when_recovering_from_corrupted_file() {
        let f = Fixture::new();
        let file = f.path("my-session.jsonl");
        fs::write(&file, "").unwrap();
        assert_eq!(f.open(&file).get_session_file().as_deref(), file.to_str());
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > SessionManager.setSessionFile with corrupted files > subsequent loads of initialized empty file work correctly
    #[test]
    fn subsequent_loads_of_initialized_empty_file_work_correctly() {
        let f = Fixture::new();
        let file = f.path("empty.jsonl");
        fs::write(&file, "").unwrap();
        let id = f.open(&file).get_session_id();
        let session = f.open(&file);
        assert_eq!(session.get_session_id(), id);
        assert_eq!(session.get_header().unwrap().kind(), "session");
    }
}

mod session_file_creation {
    use super::*;

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > SessionManager session file creation > does not create a file for a session with only setup entries
    #[test]
    fn does_not_create_a_file_for_a_session_with_only_setup_entries() {
        let f = Fixture::new();
        let mut session = f.create(f.dir());
        session
            .append_model_change("anthropic", "claude-sonnet-4-5")
            .unwrap();
        session.append_thinking_level_change("off").unwrap();
        assert!(!Path::new(&session.get_session_file().unwrap()).exists());
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > SessionManager session file creation > creates the file when the first user message is appended
    #[test]
    fn creates_the_file_when_the_first_user_message_is_appended() {
        let f = Fixture::new();
        let mut session = f.create(f.dir());
        session
            .append_model_change("anthropic", "claude-sonnet-4-5")
            .unwrap();
        session.append_message(user("first question")).unwrap();
        let file = session.get_session_file().unwrap();
        assert_eq!(file_roles(&file), ["session", "model_change", "user"]);
        assert_eq!(
            f.open(Path::new(&file))
                .build_session_context()
                .messages
                .len(),
            1
        );
    }

    // Upstream: packages/coding-agent/test/session-manager/file-operations.test.ts > SessionManager session file creation > appends later entries to the file without rewriting earlier ones
    #[test]
    fn appends_later_entries_to_the_file_without_rewriting_earlier_ones() {
        let f = Fixture::new();
        let mut session = f.create(f.dir());
        session.append_message(user("first question")).unwrap();
        session
            .append_custom_entry("preset-state", Some(value(json!({"name":"plan"}))))
            .unwrap();
        session.append_message(assistant("first answer")).unwrap();
        assert_eq!(
            file_roles(session.get_session_file().unwrap()),
            ["session", "user", "custom", "assistant"]
        );
    }
}
