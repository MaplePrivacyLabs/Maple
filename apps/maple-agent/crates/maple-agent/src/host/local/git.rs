//! Git branch reporting for watched project roots.
//!
//! The host, not the client, owns the checkout, so it reads `HEAD` and
//! watches the git dir. Clients receive [`HostEvent::ProjectBranch`] when
//! a watch starts and whenever the branch may have changed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use notify::Watcher as _;

use crate::host::{HostEvent, HostEventHub};

/// The directory that holds `HEAD` for a checkout, or `None` when `root`
/// is not one. Supports worktrees, whose `.git` is a file that points at
/// the real git dir.
pub fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let pointer = std::fs::read_to_string(&dot_git).ok()?;
    let target = pointer.trim().strip_prefix("gitdir:")?.trim();
    let target = Path::new(target);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        root.join(target)
    })
}

/// Current git branch from a git dir, or the short commit id when HEAD
/// is detached. `None` when there is no readable `HEAD`.
pub fn git_branch(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        Some(reference) => Some(
            reference
                .strip_prefix("refs/heads/")
                .unwrap_or(reference)
                .to_string(),
        ),
        // Detached: a hex id. Anything else is a corrupt HEAD.
        None => head
            .get(..7)
            .filter(|id| id.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .map(str::to_string),
    }
}

/// The branch of `root`, resolving its git dir first.
pub fn branch_of(root: &Path) -> Option<String> {
    git_dir(root).as_deref().and_then(git_branch)
}

/// True when a watcher event means the branch may have changed: a semantic
/// change to a `HEAD` path (write, create, remove, or the rename pair of an
/// atomic replacement), or a rescan the backend requires. Access-only events
/// (open, read, close) are dropped: the branch read they would trigger emits
/// those same events again under Linux inotify, looping the watcher at full
/// CPU while idle (#945). Real writes still arrive as `Modify` on every
/// backend, so no true change is lost; `Any`/`Other` stay forwarded for
/// backends that cannot classify.
pub fn head_change_event(event: &notify::Event) -> bool {
    if event.need_rescan() {
        return true;
    }
    if matches!(event.kind, notify::EventKind::Access(_)) {
        return false;
    }
    event
        .paths
        .iter()
        .any(|path| path.file_name().is_some_and(|name| name == "HEAD"))
}

struct BranchWatch {
    /// Clients watching this root. The watcher lives while any remain.
    watchers: usize,
    /// `None` when the root is not a checkout or the watch could not start.
    /// A later `watch` of the same root tries again, so a folder that is
    /// initialised as a checkout while watched gets its watcher the next
    /// time a client asks for it; nothing polls for `.git` in between.
    watcher: Option<notify::RecommendedWatcher>,
}

/// One watcher per root, shared by every client that asked for it.
#[derive(Default)]
pub struct BranchWatchers {
    roots: Mutex<HashMap<String, BranchWatch>>,
}

impl BranchWatchers {
    /// Report the branch of `root` now, and keep reporting on change.
    /// Must run inside a Tokio runtime: the change reader is a task.
    pub fn watch(&self, root: String, events: Arc<HostEventHub>) {
        {
            // The lookup and the insert happen under one guard: two clients
            // watching the same root at once must share one watcher, not
            // have the second overwrite the first with a count of one.
            // `start_watcher` never awaits, so holding the lock is cheap.
            let mut roots = self
                .roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let watch = roots.entry(root.clone()).or_insert_with(|| BranchWatch {
                watchers: 0,
                watcher: None,
            });
            watch.watchers += 1;
            if watch.watcher.is_none() {
                watch.watcher = start_watcher(&root, Arc::clone(&events));
            }
        }
        // Every client wants the current branch, whether or not the watch
        // was already running.
        publish_branch(&events, root);
    }

    /// Drop one client's interest in `root`. The watcher stops with the
    /// last one.
    pub fn unwatch(&self, root: &str) {
        let mut roots = self
            .roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(watch) = roots.get_mut(root) {
            watch.watchers = watch.watchers.saturating_sub(1);
            if watch.watchers == 0 {
                roots.remove(root);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn watched_roots(&self) -> Vec<String> {
        self.roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    /// `(clients, has a live watcher)` for `root`, when it is watched.
    #[cfg(test)]
    pub(crate) fn watch_state(&self, root: &str) -> Option<(usize, bool)> {
        self.roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(root)
            .map(|watch| (watch.watchers, watch.watcher.is_some()))
    }
}

/// Read the branch off the async workers and publish it.
fn publish_branch(events: &Arc<HostEventHub>, root: String) {
    let events = Arc::clone(events);
    tokio::task::spawn_blocking(move || {
        let branch = branch_of(Path::new(&root));
        events.publish(HostEvent::ProjectBranch {
            project_root: root,
            branch,
        });
    });
}

/// Watch the git dir of `root` and publish the branch when `HEAD`
/// changes. The watch is on the directory, not the file: git replaces
/// `HEAD` by rename, so a watch on the file itself is lost after the first
/// checkout. Non-recursive, so a busy `objects/` tree costs nothing.
/// Events arrive on the watcher's own thread and cross to a task through a
/// channel; a rebase or checkout touches `HEAD` several times in a row, so
/// one read per burst is enough.
fn start_watcher(root: &str, events: Arc<HostEventHub>) -> Option<notify::RecommendedWatcher> {
    let git_dir = git_dir(Path::new(root))?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher =
        match notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else { return };
            if head_change_event(&event) {
                tx.send(()).ok();
            }
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                log::debug!("branch watcher unavailable: {error}");
                return None;
            }
        };
    if let Err(error) = watcher.watch(&git_dir, notify::RecursiveMode::NonRecursive) {
        log::debug!("cannot watch {}: {error}", git_dir.display());
        return None;
    }
    let root = root.to_string();
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            while rx.try_recv().is_ok() {}
            let branch = {
                let root = root.clone();
                tokio::task::spawn_blocking(move || branch_of(Path::new(&root)))
                    .await
                    .unwrap_or(None)
            };
            events.publish(HostEvent::ProjectBranch {
                project_root: root.clone(),
                branch,
            });
        }
    });
    Some(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("maple-git-{tag}-{}", uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn uuid_like() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            ^ (std::process::id() as u128)
    }

    #[test]
    fn branch_reads_refs_detached_heads_and_worktree_pointers() {
        let root = temp_root("branch");
        let git = root.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(branch_of(&root).as_deref(), Some("feature/x"));

        std::fs::write(git.join("HEAD"), "0123abcdef0123abcdef\n").unwrap();
        assert_eq!(branch_of(&root).as_deref(), Some("0123abc"));

        std::fs::write(git.join("HEAD"), "garbage\n").unwrap();
        assert_eq!(branch_of(&root), None);

        let worktree = temp_root("worktree");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", git.display()),
        )
        .unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(branch_of(&worktree).as_deref(), Some("main"));

        let plain = temp_root("plain");
        assert_eq!(branch_of(&plain), None);
        for dir in [root, worktree, plain] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// Issue #945: the branch watcher must ignore access-only HEAD events
    /// (open, read, close). The branch read they trigger emits those same
    /// events again under Linux inotify, looping at ~200% CPU while idle.
    #[test]
    fn head_change_ignores_access_events() {
        use notify::EventKind;
        use notify::event::{
            AccessKind, AccessMode, CreateKind, DataChange, Flag, MetadataKind, ModifyKind,
            RemoveKind, RenameMode,
        };

        let head = PathBuf::from("/repo/.git/HEAD");
        let event = |kind: EventKind| notify::Event::new(kind).add_path(head.clone());

        // The read side of the loop, as emitted by Linux inotify.
        for kind in [
            EventKind::Access(AccessKind::Open(AccessMode::Read)),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            EventKind::Access(AccessKind::Any),
            EventKind::Access(AccessKind::Other),
        ] {
            assert!(!head_change_event(&event(kind)), "access {kind:?}");
        }

        // Real changes still refresh the label: in-place write, create,
        // remove, and the rename pair of an atomic replacement, plus the
        // unclassified kinds imprecise backends emit for real changes.
        for kind in [
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            EventKind::Create(CreateKind::File),
            EventKind::Create(CreateKind::Any),
            EventKind::Remove(RemoveKind::File),
            EventKind::Remove(RemoveKind::Any),
            EventKind::Any,
            EventKind::Other,
        ] {
            assert!(head_change_event(&event(kind)), "change {kind:?}");
        }

        // Unrelated paths never refresh, even with a change kind.
        let unrelated =
            notify::Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
                .add_path(PathBuf::from("/repo/.git/index"));
        assert!(!head_change_event(&unrelated));

        // A required rescan refreshes even without a HEAD path.
        let event = notify::Event::new(EventKind::Other).set_flag(Flag::Rescan);
        assert!(head_change_event(&event));
    }

    #[tokio::test]
    async fn watchers_are_shared_and_dropped_with_the_last_client() {
        let root = temp_root("shared");
        let git = root.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let hub = Arc::new(HostEventHub::default());
        let mut rx = hub.subscribe();
        let watchers = BranchWatchers::default();
        let path = root.to_string_lossy().to_string();
        watchers.watch(path.clone(), Arc::clone(&hub));
        watchers.watch(path.clone(), Arc::clone(&hub));
        assert_eq!(watchers.watched_roots(), vec![path.clone()]);
        assert_eq!(watchers.watch_state(&path), Some((2, true)));
        // Both watch calls report the branch.
        for _ in 0..2 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                event,
                HostEvent::ProjectBranch { branch: Some(ref branch), .. } if branch == "main"
            ));
        }
        watchers.unwatch(&path);
        assert_eq!(watchers.watch_state(&path), Some((1, true)));
        watchers.unwatch(&path);
        assert!(watchers.watched_roots().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    /// Two clients that start watching one root at the same moment share
    /// one watcher, and the first to leave does not take it with them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_watches_of_one_root_share_the_watcher() {
        let root = temp_root("concurrent");
        let git = root.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let hub = Arc::new(HostEventHub::default());
        let mut rx = hub.subscribe();
        let watchers = Arc::new(BranchWatchers::default());
        let path = root.to_string_lossy().to_string();
        let tasks: Vec<_> = (0..2)
            .map(|_| {
                let watchers = Arc::clone(&watchers);
                let hub = Arc::clone(&hub);
                let path = path.clone();
                tokio::spawn(async move { watchers.watch(path, hub) })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(watchers.watch_state(&path), Some((2, true)));
        watchers.unwatch(&path);
        assert_eq!(watchers.watch_state(&path), Some((1, true)));

        // The surviving watcher still reports a checkout.
        std::fs::write(git.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let event = tokio::time::timeout(remaining, rx.recv())
                .await
                .expect("branch change reported")
                .unwrap();
            if matches!(
                event,
                HostEvent::ProjectBranch { branch: Some(ref branch), .. } if branch == "feature"
            ) {
                break;
            }
        }
        watchers.unwatch(&path);
        assert!(watchers.watched_roots().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    /// A root that becomes a checkout after its first watch gets a watcher
    /// on the next watch instead of staying blind forever.
    #[tokio::test]
    async fn a_later_watch_attaches_a_watcher_once_the_root_is_a_checkout() {
        let root = temp_root("late");
        let hub = Arc::new(HostEventHub::default());
        let watchers = BranchWatchers::default();
        let path = root.to_string_lossy().to_string();
        watchers.watch(path.clone(), Arc::clone(&hub));
        assert_eq!(watchers.watch_state(&path), Some((1, false)));

        let git = root.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        watchers.watch(path.clone(), Arc::clone(&hub));
        assert_eq!(watchers.watch_state(&path), Some((2, true)));
        watchers.unwatch(&path);
        watchers.unwatch(&path);
        let _ = std::fs::remove_dir_all(root);
    }
}
