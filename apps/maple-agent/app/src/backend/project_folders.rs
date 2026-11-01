//! The execution host owns folder semantics for local and remote pickers.
//! These operations expose directory metadata only. A selected project is a
//! context/trust choice, not a filesystem sandbox for a paired controller.

use std::{
    collections::BinaryHeap,
    fs, io,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::{LocalBackend, SelectedProject};

const MAX_PATH_BYTES: usize = 4096;
const MAX_NAME_BYTES: usize = 1024;
const MAX_PAGE_ENTRIES: usize = 100;
const MAX_SCANNED_ENTRIES: usize = 50_000;
// Leave room for the response envelope under the 256 KiB remote frame limit.
const MAX_PAGE_BYTES: usize = 192 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectFolderRequest {
    /// None starts at this daemon's home; a supplied path must be absolute.
    pub path: Option<String>,
    /// Exclusive child-name cursor, scoped to the same path and hidden setting.
    pub after: Option<String>,
    pub limit: u16,
    #[serde(default)]
    pub show_hidden: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectFolderEntry {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectFolderPage {
    pub path: String,
    pub parent_path: Option<String>,
    pub entries: Vec<ProjectFolderEntry>,
    pub next: Option<String>,
}

impl LocalBackend {
    pub async fn recent_project_roots(
        &self,
        user_id: &str,
    ) -> Result<Vec<maple_agent::agent::RecentProjectRoot>, String> {
        self.authorize_account(user_id)?;
        self.open_store()?;
        self.service
            .handle_for_user(user_id)
            .await?
            .recent_project_roots()
            .await
    }

    pub async fn browse_project_folders(
        &self,
        user_id: &str,
        request: ProjectFolderRequest,
    ) -> Result<ProjectFolderPage, String> {
        self.authorize_account(user_id)?;
        self.open_store()?;
        tokio::task::spawn_blocking(move || browse(request, dirs::home_dir()))
            .await
            .map_err(|_| "Folder browser worker did not complete".to_string())?
    }

    /// Create on the execution host, then register with the ordinary project
    /// metadata and trust flow. No user-supplied path or prompt names the folder.
    pub async fn create_scratch_project(&self, user_id: &str) -> Result<SelectedProject, String> {
        self.authorize_account(user_id)?;
        self.open_store()?;
        let parent = tokio::task::spawn_blocking(|| {
            maple_agent::agent::default_maple_workspace_path()
                .ok_or_else(|| "This computer's documents folder is unavailable".to_string())
        })
        .await
        .map_err(|_| "Project creation worker did not complete".to_string())??;
        self.create_scratch_project_in(user_id, parent).await
    }

    // Private location seam keeps tests off the real Documents directory. The
    // host-facing operation above always chooses this execution host's default.
    async fn create_scratch_project_in(
        &self,
        user_id: &str,
        parent: PathBuf,
    ) -> Result<SelectedProject, String> {
        self.authorize_account(user_id)?;
        self.open_store()?;
        let handle = self.service.handle_for_user(user_id).await?;
        let path = tokio::task::spawn_blocking(move || {
            create_scratch_folder(&parent, || uuid::Uuid::new_v4().to_string())
        })
        .await
        .map_err(|_| "Project creation worker did not complete".to_string())??;
        let project_root = handle.save_recent_project_root(path).await?;
        let project = handle
            .rename_project_root(project_root.clone(), "Untitled project".into())
            .await?;
        Ok(SelectedProject {
            project_root,
            project,
        })
    }

    pub async fn create_project_folder(
        &self,
        user_id: &str,
        parent_path: String,
        name: String,
    ) -> Result<String, String> {
        self.authorize_account(user_id)?;
        self.open_store()?;
        // Account dispatch holds its read gate until this admitted operation
        // finishes, including when a remote controller disconnects.
        tokio::task::spawn_blocking(move || create(&parent_path, &name))
            .await
            .map_err(|_| "Folder creation worker did not complete".to_string())?
    }
}

fn path_text(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or("This folder path is not valid Unicode")?;
    if text.len() > MAX_PATH_BYTES {
        return Err("This folder path is too long".into());
    }
    Ok(text.to_owned())
}

fn absolute_path(path: &str) -> Result<&Path, String> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES || path.contains('\0') {
        return Err("Enter a valid absolute folder path".into());
    }
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err("Enter an absolute folder path on this computer".into());
    }
    Ok(path)
}

fn folder_error(error: io::Error) -> String {
    match error.kind() {
        io::ErrorKind::NotFound => "This folder no longer exists".into(),
        io::ErrorKind::PermissionDenied => "Permission denied for this folder".into(),
        io::ErrorKind::AlreadyExists => "A file or folder with this name already exists".into(),
        io::ErrorKind::NotADirectory => "This path is not a folder".into(),
        _ => "Could not access this folder".into(),
    }
}

fn canonical_folder(path: &Path) -> Result<PathBuf, String> {
    let path = path.canonicalize().map_err(folder_error)?;
    if !path.metadata().map_err(folder_error)?.is_dir() {
        return Err("This path is not a folder".into());
    }
    path_text(&path)?;
    Ok(path)
}

fn browse(
    request: ProjectFolderRequest,
    home: Option<PathBuf>,
) -> Result<ProjectFolderPage, String> {
    let root = match request.path.as_deref() {
        Some(path) => canonical_folder(absolute_path(path)?)?,
        None => canonical_folder(&home.ok_or("This computer's home folder is unavailable")?)?,
    };
    if let Some(after) = &request.after
        && (after.is_empty() || after.len() > MAX_NAME_BYTES || after.contains('\0'))
    {
        return Err("Invalid folder page cursor".into());
    }
    let limit = usize::from(request.limit).clamp(1, MAX_PAGE_ENTRIES);
    let names = scan_names(&root, &request, limit, MAX_SCANNED_ENTRIES)?;
    let mut page = ProjectFolderPage {
        path: path_text(&root)?,
        parent_path: root.parent().map(path_text).transpose()?,
        entries: Vec::new(),
        next: None,
    };
    // Keep only the earliest page plus one name during the scan. Each request
    // re-scans the current directory; it does not retain an OS handle/session.
    let mut names = names.into_iter().peekable();
    while let Some(name) = names.next() {
        if page.entries.len() == limit {
            page.next = page.entries.last().map(|entry| entry.name.clone());
            break;
        }
        let entry = ProjectFolderEntry {
            path: path_text(&root.join(&name))?,
            name,
        };
        page.entries.push(entry);
        page.next = names
            .peek()
            .map(|_| page.entries.last().unwrap().name.clone());
        if serde_json::to_vec(&page)
            .map_err(|_| "Could not encode folder page")?
            .len()
            > MAX_PAGE_BYTES
        {
            page.entries.pop();
            page.next = page.entries.last().map(|entry| entry.name.clone());
            if page.entries.is_empty() {
                return Err("This folder entry is too large to display".into());
            }
            break;
        }
    }
    Ok(page)
}

fn scan_names(
    root: &Path,
    request: &ProjectFolderRequest,
    limit: usize,
    scan_limit: usize,
) -> Result<Vec<String>, String> {
    let mut names = BinaryHeap::with_capacity(limit + 2);
    for (index, entry) in fs::read_dir(root).map_err(folder_error)?.enumerate() {
        if index == scan_limit {
            return Err(
                "This folder has too many entries to browse. Enter a child folder's full path."
                    .into(),
            );
        }
        let entry = entry.map_err(folder_error)?;
        // Non-Unicode names cannot round-trip through the JSON path contract.
        // Never replace bytes with U+FFFD and risk opening a different folder.
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.len() > MAX_NAME_BYTES
            || (!request.show_hidden && name.starts_with('.'))
            || request.after.as_ref().is_some_and(|after| name <= *after)
        {
            continue;
        }
        let kind = entry.file_type().map_err(folder_error)?;
        if !kind.is_dir() && !(kind.is_symlink() && entry.path().is_dir()) {
            continue;
        }
        names.push(name);
        if names.len() > limit + 1 {
            names.pop();
        }
    }
    Ok(names.into_sorted_vec())
}

fn validate_child_name(name: &str) -> Result<(), String> {
    let mut components = Path::new(name).components();
    if name.trim().is_empty()
        || name.len() > MAX_NAME_BYTES
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err("Enter one folder name without path separators".into());
    }
    Ok(())
}

fn create_scratch_folder(
    parent: &Path,
    mut unique_suffix: impl FnMut() -> String,
) -> Result<String, String> {
    path_text(parent)?;
    fs::create_dir_all(parent).map_err(folder_error)?;
    let parent = canonical_folder(parent)?;
    for _ in 0..4 {
        let path = parent.join(format!("Untitled project-{}", unique_suffix()));
        path_text(&path)?;
        // Atomic creation rejects existing directories, files and symlinks.
        // Never adopt a collision, and never delete files after a later error.
        match fs::create_dir(&path) {
            Ok(()) => return path_text(&canonical_folder(&path)?),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(folder_error(error)),
        }
    }
    Err("Could not create a unique project folder. Try again.".into())
}

fn create(parent: &str, name: &str) -> Result<String, String> {
    validate_child_name(name)?;
    let parent = canonical_folder(absolute_path(parent)?)?;
    let path = parent.join(name);
    path_text(&path)?;
    // Intentionally not recursive and not idempotent: existing files, folders,
    // and symlinks must all be rejected rather than adopted or overwritten.
    fs::create_dir(&path).map_err(folder_error)?;
    path_text(&canonical_folder(&path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(path: &Path) -> ProjectFolderRequest {
        ProjectFolderRequest {
            path: Some(path_text(path).unwrap()),
            after: None,
            limit: 2,
            show_hidden: false,
        }
    }

    #[tokio::test]
    async fn scratch_project_registration_persists_and_can_be_named_later() {
        const USER: &str = "scratch-project-owner";
        let storage = tempfile::tempdir().unwrap();
        let folders = tempfile::tempdir().unwrap();
        let parent = folders.path().join("Documents").join("Maple");
        let backend = LocalBackend::isolated_for_test(storage.path());
        assert!(backend.persisted_auth.claim_and_persist(
            &maple_agent::maple_api::MapleApiAuthSnapshot {
                user_id: USER.into(),
                access_token: "fixture-token".into(),
                refresh_token: None,
                native_instance_id: "fixture-native".into(),
                session_id: "fixture-session".into(),
                revision: 1,
            }
        ));
        assert!(
            backend
                .create_scratch_project_in("another-account", parent.clone())
                .await
                .is_err()
        );
        assert!(
            !parent.exists(),
            "rejection must precede filesystem effects"
        );
        let first = backend
            .create_scratch_project_in(USER, parent.clone())
            .await
            .unwrap();
        let second = backend
            .create_scratch_project_in(USER, parent.clone())
            .await
            .unwrap();
        assert_ne!(first.project_root, second.project_root);
        for selected in [&first, &second] {
            assert_eq!(selected.project.name, "Untitled project");
            assert_eq!(selected.project.path, selected.project_root);
            assert!(!selected.project.is_default_workspace);
            assert_eq!(
                Path::new(&selected.project_root).parent(),
                Some(parent.canonicalize().unwrap().as_path())
            );
            assert_eq!(fs::read_dir(&selected.project_root).unwrap().count(), 0);
        }
        let renamed = backend
            .rename_project_root(USER, first.project_root.clone(), "My new project".into())
            .await
            .unwrap();
        assert_eq!(renamed.name, "My new project");
        let snapshot = crate::backend::dispatch(
            &backend,
            crate::host::protocol::Request {
                id: 1,
                method: "navigation_bootstrap".into(),
                send_id: None,
                params: serde_json::json!([USER]),
            },
        )
        .await
        .unwrap();
        let snapshot: crate::backend::paging::NavigationBootstrap =
            serde_json::from_value(snapshot).unwrap();
        for (selected, name) in [(&first, "My new project"), (&second, "Untitled project")] {
            let project = snapshot
                .recent_projects
                .iter()
                .find(|project| project.root == selected.project_root)
                .unwrap();
            assert_eq!(project.display_name.as_deref(), Some(name));
            let reselected = backend
                .select_project_root(USER, selected.project_root.clone())
                .await
                .unwrap();
            assert_eq!(reselected.project.name, name);
        }
    }

    #[test]
    fn scratch_projects_are_distinct_empty_persistent_children() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("Documents").join("Maple");
        let first = create_scratch_folder(&parent, || uuid::Uuid::new_v4().to_string()).unwrap();
        let second = create_scratch_folder(&parent, || uuid::Uuid::new_v4().to_string()).unwrap();
        assert_ne!(first, second);
        for project in [first, second] {
            let path = Path::new(&project);
            assert_eq!(
                path.parent(),
                Some(parent.canonicalize().unwrap().as_path())
            );
            assert!(
                path.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("Untitled project-")
            );
            assert_eq!(fs::read_dir(path).unwrap().count(), 0);
        }
    }

    #[test]
    fn scratch_projects_skip_collisions_and_fail_without_overwriting() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("Untitled project-folder")).unwrap();
        let file = root.path().join("Untitled project-file");
        fs::write(&file, b"keep").unwrap();
        let mut suffixes = ["folder", "file", "fresh"].into_iter();
        let created =
            create_scratch_folder(root.path(), || suffixes.next().unwrap().into()).unwrap();
        assert!(created.ends_with("Untitled project-fresh"));
        assert!(create_scratch_folder(root.path(), || "file".into()).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"keep");
        assert!(create_scratch_folder(&file, || "child".into()).is_err());
        assert!(!file.join("Untitled project-child").exists());
    }

    #[cfg(unix)]
    #[test]
    fn scratch_projects_never_adopt_existing_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("Untitled project-link"))
            .unwrap();
        std::os::unix::fs::symlink(
            root.path().join("missing"),
            root.path().join("Untitled project-broken"),
        )
        .unwrap();
        for suffix in ["link", "broken"] {
            assert!(create_scratch_folder(root.path(), || suffix.into()).is_err());
        }
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn folder_pages_default_to_host_home_and_page_sorted_directories() {
        let root = tempfile::tempdir().unwrap();
        for name in ["zebra", "alpha", "beta", ".hidden"] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        fs::write(root.path().join("file"), b"never returned").unwrap();
        let mut query = request(root.path());
        query.path = None;
        let first = browse(query.clone(), Some(root.path().into())).unwrap();
        assert_eq!(
            first.path,
            path_text(&root.path().canonicalize().unwrap()).unwrap()
        );
        assert_eq!(
            first
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        assert_eq!(first.next.as_deref(), Some("beta"));
        query.after = first.next;
        let last = browse(query, Some(root.path().into())).unwrap();
        assert_eq!(last.entries[0].name, "zebra");
        assert_eq!(last.next, None);
        let mut query = request(root.path());
        query.show_hidden = true;
        assert_eq!(browse(query, None).unwrap().entries[0].name, ".hidden");
        assert!(browse(ProjectFolderRequest::default(), None).is_err());
    }

    #[test]
    fn folder_pages_have_native_parent_and_reject_files_missing_and_relative_paths() {
        let root = tempfile::tempdir().unwrap();
        let empty = root.path().join("empty");
        fs::create_dir(&empty).unwrap();
        let page = browse(request(&empty), None).unwrap();
        assert!(page.entries.is_empty());
        assert!(page.next.is_none());
        assert_eq!(
            page.parent_path,
            Some(path_text(&root.path().canonicalize().unwrap()).unwrap())
        );
        let file = root.path().join("file");
        fs::write(&file, b"secret contents").unwrap();
        for path in [file, root.path().join("missing"), PathBuf::from("relative")] {
            assert!(browse(request(&path), None).is_err());
        }
        assert!(absolute_path("").is_err());
        assert!(absolute_path("/invalid\0path").is_err());
        assert!(absolute_path(&"/".repeat(MAX_PATH_BYTES + 1)).is_err());
        assert_eq!(
            folder_error(io::ErrorKind::PermissionDenied.into()),
            "Permission denied for this folder"
        );
    }

    #[test]
    fn folder_scan_and_page_count_are_bounded() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..105 {
            fs::create_dir(root.path().join(format!("folder-{index:03}"))).unwrap();
        }
        let mut query = request(root.path());
        query.limit = u16::MAX;
        let page = browse(query.clone(), None).unwrap();
        assert_eq!(page.entries.len(), MAX_PAGE_ENTRIES);
        assert_eq!(page.next.as_deref(), Some("folder-099"));
        assert!(
            scan_names(root.path(), &query, 2, 104)
                .unwrap_err()
                .contains("too many entries")
        );
        assert_eq!(scan_names(root.path(), &query, 2, 105).unwrap().len(), 3);
        query.after = Some("x".repeat(MAX_NAME_BYTES + 1));
        assert!(browse(query, None).unwrap_err().contains("cursor"));
    }

    #[test]
    fn folder_create_is_one_new_child_and_never_adopts_existing_targets() {
        let root = tempfile::tempdir().unwrap();
        let parent = path_text(root.path()).unwrap();
        let created = create(&parent, "new folder").unwrap();
        assert!(Path::new(&created).is_dir());
        assert_eq!(
            Path::new(&created).parent(),
            Some(root.path().canonicalize().unwrap().as_path())
        );
        assert!(
            create(&parent, "new folder")
                .unwrap_err()
                .contains("already exists")
        );
        fs::write(root.path().join("file"), b"untouched").unwrap();
        assert!(
            create(&parent, "file")
                .unwrap_err()
                .contains("already exists")
        );
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"untouched");
        for name in [
            "",
            " ",
            ".",
            "..",
            "../escape",
            "a/b",
            "a\\b",
            "/absolute",
            "a\0b",
            "line\nfeed",
        ] {
            assert!(create(&parent, name).is_err(), "accepted {name:?}");
        }
        assert!(create(&parent, &"x".repeat(MAX_NAME_BYTES + 1)).is_err());
        assert!(create("relative", "child").is_err());
        assert!(create(&path_text(&root.path().join("missing")).unwrap(), "child").is_err());
        assert!(!root.path().join("missing").exists());
    }

    #[cfg(unix)]
    #[test]
    fn folder_pages_follow_directory_links_and_do_not_lossily_alias_names() {
        use std::os::unix::{ffi::OsStringExt, fs::symlink};
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("link")).unwrap();
        symlink(root.path().join("missing"), root.path().join("broken")).unwrap();
        let invalid_name = root
            .path()
            .join(std::ffi::OsString::from_vec(vec![b'x', 0xff]));
        if let Err(error) = fs::create_dir(invalid_name) {
            // APFS refuses invalid Unicode names before an entry can exist.
            // Linux filesystems admit them, exercising the omission below.
            assert_eq!(error.raw_os_error(), Some(libc::EILSEQ));
        }
        let page = browse(request(root.path()), None).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].name, "link");
        let child = browse(request(Path::new(&page.entries[0].path)), None).unwrap();
        assert_eq!(
            child.path,
            path_text(&outside.path().canonicalize().unwrap()).unwrap()
        );
        assert!(
            create(&page.path, "link")
                .unwrap_err()
                .contains("already exists")
        );
        assert!(
            create(&page.path, "broken")
                .unwrap_err()
                .contains("already exists")
        );
        // A selected folder is not a sandbox; creation through an explicitly
        // chosen parent symlink follows the execution host's native semantics.
        let created = create(&page.entries[0].path, "child").unwrap();
        assert_eq!(
            Path::new(&created).parent(),
            Some(outside.path().canonicalize().unwrap().as_path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn folder_browse_reports_permission_failure_without_private_os_details() {
        use std::os::unix::fs::PermissionsExt;
        // Root bypasses directory permissions, so this condition has no
        // permission-denied behavior to exercise in a root-owned test runner.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let original = fs::metadata(root.path()).unwrap().permissions();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o0)).unwrap();
        let result = browse(request(root.path()), None);
        fs::set_permissions(root.path(), original).unwrap();
        assert_eq!(result.unwrap_err(), "Permission denied for this folder");
    }

    #[cfg(unix)]
    #[test]
    fn folder_pages_bound_serialized_bytes_and_resume_without_omitting_entries() {
        let root = tempfile::tempdir().unwrap();
        let mut deep = root.path().to_path_buf();
        // JSON escaping exercises the byte budget without exceeding macOS's
        // smaller native path limit. All names live in this disposable fixture.
        for _ in 0..8 {
            deep.push("\u{1}".repeat(90));
        }
        fs::create_dir_all(&deep).unwrap();
        for index in 0..100 {
            fs::create_dir(deep.join(format!("folder-{index:03}"))).unwrap();
        }
        let mut query = request(&deep);
        query.limit = 100;
        let first = browse(query.clone(), None).unwrap();
        assert!(first.entries.len() < 100);
        assert!(serde_json::to_vec(&first).unwrap().len() <= MAX_PAGE_BYTES);
        let mut names = first
            .entries
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        let mut next = first.next;
        while let Some(after) = next {
            query.after = Some(after);
            let page = browse(query.clone(), None).unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_PAGE_BYTES);
            names.extend(page.entries.into_iter().map(|entry| entry.name));
            next = page.next;
        }
        assert_eq!(
            names,
            (0..100)
                .map(|index| format!("folder-{index:03}"))
                .collect::<Vec<_>>()
        );
    }
}
