//! One change at a time per file: `edit` and `write` calls on the same file run in the
//! order they arrived, while calls on different files run in parallel.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

type Queues = Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>;

fn queues() -> &'static Queues {
    static QUEUES: OnceLock<Queues> = OnceLock::new();
    QUEUES.get_or_init(Default::default)
}

/// The file's real path, so two names for one file share a queue; the path as given
/// when it does not exist yet.
fn queue_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Run `change` once every earlier change to the same file has finished.
pub async fn with_file_mutation_queue<T>(path: &Path, change: impl Future<Output = T>) -> T {
    let queue = {
        let mut queues = queues().lock().unwrap_or_else(|error| error.into_inner());
        queues.retain(|_, queue| queue.strong_count() > 0);
        let key = queue_key(path);
        match queues.get(&key).and_then(Weak::upgrade) {
            Some(queue) => queue,
            None => {
                let queue = Arc::new(tokio::sync::Mutex::new(()));
                queues.insert(key, Arc::downgrade(&queue));
                queue
            }
        }
    };
    // Tokio's mutex is fair, so waiting changes run in arrival order.
    let _turn = queue.lock().await;
    change.await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn changes_to_one_file_run_one_at_a_time_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for index in 0..5 {
            let (path, log) = (path.clone(), log.clone());
            tasks.push(tokio::spawn(async move {
                with_file_mutation_queue(&path, async {
                    log.lock().unwrap().push(format!("start {index}"));
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    log.lock().unwrap().push(format!("end {index}"));
                })
                .await
            }));
            // Let each task reach the queue before the next starts.
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        for task in tasks {
            task.await.unwrap();
        }
        let log = log.lock().unwrap();
        for pair in log.chunks(2) {
            let started = pair[0].strip_prefix("start ").unwrap();
            assert_eq!(pair[1], format!("end {started}"));
        }
    }

    #[tokio::test]
    async fn different_files_do_not_wait_for_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("a.txt");
        let second = dir.path().join("b.txt");
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        let held = tokio::spawn({
            let first = first.clone();
            async move { with_file_mutation_queue(&first, async { wait.await.ok() }).await }
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        tokio::time::timeout(
            Duration::from_secs(1),
            with_file_mutation_queue(&second, async {}),
        )
        .await
        .expect("another file's change does not wait");
        release.send(()).unwrap();
        held.await.unwrap();
    }
}
