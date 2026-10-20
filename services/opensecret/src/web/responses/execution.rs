//! In-memory ownership for one live Responses execution.
//!
//! The database response status describes durable transcript state. It cannot
//! by itself prove that the main provider work has stopped. This registry
//! provides that process-local execution barrier. Usage publication,
//! conversation-title, and pre-persistence image-description work deliberately
//! retain their independent lifecycles.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
};
use tokio::sync::{watch, Notify};
use uuid::Uuid;

#[derive(Clone, Default)]
pub(crate) struct ResponseExecutionRegistry {
    inner: Arc<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    entries: Mutex<HashMap<Uuid, Arc<ResponseExecutionInner>>>,
}

struct ResponseExecutionInner {
    response_id: Uuid,
    user_id: Uuid,
    cancellation: watch::Sender<bool>,
    task_state: Mutex<ResponseExecutionTaskState>,
    task_finished: Notify,
    owner_finished: AtomicBool,
    registry: Weak<RegistryInner>,
}

struct ResponseExecutionTaskState {
    accepting_tasks: bool,
    active_tasks: usize,
}

#[derive(Clone)]
pub(crate) struct ResponseExecution {
    inner: Arc<ResponseExecutionInner>,
}

pub(crate) struct ResponseExecutionRegistration {
    execution: ResponseExecution,
}

pub(crate) struct ResponseExecutionTaskGuard {
    inner: Arc<ResponseExecutionInner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DuplicateResponseExecution;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClosedResponseExecution;

impl ResponseExecutionRegistry {
    pub(crate) fn register(
        &self,
        response_id: Uuid,
        user_id: Uuid,
    ) -> Result<ResponseExecutionRegistration, DuplicateResponseExecution> {
        let (cancellation, _) = watch::channel(false);
        let execution = Arc::new(ResponseExecutionInner {
            response_id,
            user_id,
            cancellation,
            task_state: Mutex::new(ResponseExecutionTaskState {
                accepting_tasks: true,
                active_tasks: 0,
            }),
            task_finished: Notify::new(),
            owner_finished: AtomicBool::new(false),
            registry: Arc::downgrade(&self.inner),
        });

        let mut entries = self
            .inner
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if entries.contains_key(&response_id) {
            return Err(DuplicateResponseExecution);
        }
        entries.insert(response_id, execution.clone());

        Ok(ResponseExecutionRegistration {
            execution: ResponseExecution { inner: execution },
        })
    }

    pub(crate) fn execution_for_user(
        &self,
        response_id: Uuid,
        user_id: Uuid,
    ) -> Option<ResponseExecution> {
        let entries = self
            .inner
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries
            .get(&response_id)
            .filter(|execution| execution.user_id == user_id)
            .cloned()
            .map(|inner| ResponseExecution { inner })
    }

    #[cfg(test)]
    fn contains(&self, response_id: Uuid) -> bool {
        self.inner
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&response_id)
    }
}

impl ResponseExecutionRegistration {
    pub(crate) fn execution(&self) -> ResponseExecution {
        self.execution.clone()
    }
}

impl Drop for ResponseExecutionRegistration {
    fn drop(&mut self) {
        let mut state = self
            .execution
            .inner
            .task_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.execution
            .inner
            .owner_finished
            .store(true, Ordering::Release);
        if state.active_tasks == 0 {
            // Seal the zero-child transition under the same lock used by
            // begin_task. A retained execution handle cannot race owner
            // completion and register work after observers saw quiescence.
            state.accepting_tasks = false;
        }
        drop(state);
        self.execution.inner.task_finished.notify_waiters();
        remove_if_finished(&self.execution.inner);
    }
}

impl ResponseExecution {
    pub(crate) fn response_id(&self) -> Uuid {
        self.inner.response_id
    }

    /// Acquire task ownership synchronously before spawning the child.
    pub(crate) fn begin_task(&self) -> Result<ResponseExecutionTaskGuard, ClosedResponseExecution> {
        let mut state = self
            .inner
            .task_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.accepting_tasks {
            return Err(ClosedResponseExecution);
        }
        state.active_tasks += 1;
        Ok(ResponseExecutionTaskGuard {
            inner: self.inner.clone(),
        })
    }

    /// Cancellation is sticky so children registered after the request raced
    /// with setup still observe it before doing provider or persistence work.
    pub(crate) fn cancel(&self) {
        self.inner
            .task_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .accepting_tasks = false;
        self.inner.cancellation.send_replace(true);
    }

    pub(crate) async fn cancelled(&self) {
        let mut cancellation = self.inner.cancellation.subscribe();
        if *cancellation.borrow() {
            return;
        }

        while cancellation.changed().await.is_ok() {
            if *cancellation.borrow() {
                return;
            }
        }

        // The sender lives as long as this execution, so channel closure is
        // equivalent to the execution owner going away.
    }

    pub(crate) async fn wait_for_quiescence(&self) {
        loop {
            let finished = self.inner.task_finished.notified();
            tokio::pin!(finished);
            // Register this waiter before reading the counter so a child cannot
            // drop between the read and await and lose the wakeup.
            finished.as_mut().enable();
            let active_tasks = self
                .inner
                .task_state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .active_tasks;
            if active_tasks == 0 && self.inner.owner_finished.load(Ordering::Acquire) {
                return;
            }
            finished.await;
        }
    }
}

impl Drop for ResponseExecutionTaskGuard {
    fn drop(&mut self) {
        let mut state = self
            .inner
            .task_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(
            state.active_tasks > 0,
            "response execution task count underflow"
        );
        state.active_tasks = state.active_tasks.saturating_sub(1);
        if state.active_tasks == 0 && self.inner.owner_finished.load(Ordering::Acquire) {
            state.accepting_tasks = false;
        }
        drop(state);
        self.inner.task_finished.notify_waiters();
        remove_if_finished(&self.inner);
    }
}

fn remove_if_finished(execution: &Arc<ResponseExecutionInner>) {
    if !execution.owner_finished.load(Ordering::Acquire) {
        return;
    }

    let mut task_state = execution
        .task_state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if task_state.active_tasks != 0 {
        return;
    }
    // Seal natural completion before removing the map entry. A retained Arc
    // can no longer create work after observers saw a stable zero count.
    task_state.accepting_tasks = false;
    drop(task_state);

    let Some(registry) = execution.registry.upgrade() else {
        return;
    };
    let mut entries = registry
        .entries
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if entries
        .get(&execution.response_id)
        .is_some_and(|current| Arc::ptr_eq(current, execution))
    {
        entries.remove(&execution.response_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn cancellation_is_sticky_for_late_children() {
        let registry = ResponseExecutionRegistry::default();
        let response_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let registration = registry.register(response_id, user_id).unwrap();
        let execution = registration.execution();

        execution.cancel();

        timeout(Duration::from_millis(50), execution.cancelled())
            .await
            .expect("late child must observe prior cancellation");
    }

    #[tokio::test]
    async fn quiescence_waits_for_every_synchronously_registered_child() {
        let registry = ResponseExecutionRegistry::default();
        let registration = registry.register(Uuid::new_v4(), Uuid::new_v4()).unwrap();
        let execution = registration.execution();
        let first = execution.begin_task().unwrap();
        let second = execution.begin_task().unwrap();

        let waiter = tokio::spawn({
            let execution = execution.clone();
            async move { execution.wait_for_quiescence().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        drop(first);
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        drop(second);
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        drop(registration);
        timeout(Duration::from_millis(50), waiter)
            .await
            .expect("all child guards released")
            .expect("join quiescence waiter");
    }

    #[tokio::test]
    async fn zero_child_execution_does_not_quiesce_before_registration_finishes() {
        let registry = ResponseExecutionRegistry::default();
        let registration = registry.register(Uuid::new_v4(), Uuid::new_v4()).unwrap();
        let execution = registration.execution();

        let waiter = tokio::spawn({
            let execution = execution.clone();
            async move { execution.wait_for_quiescence().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        drop(registration);
        timeout(Duration::from_millis(50), waiter)
            .await
            .expect("owner completion releases quiescence waiter")
            .expect("join quiescence waiter");
        assert!(execution.begin_task().is_err());
    }

    #[test]
    fn lookup_is_user_scoped_and_duplicate_registration_is_rejected() {
        let registry = ResponseExecutionRegistry::default();
        let response_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let _registration = registry.register(response_id, user_id).unwrap();

        assert!(registry.execution_for_user(response_id, user_id).is_some());
        assert!(registry
            .execution_for_user(response_id, Uuid::new_v4())
            .is_none());
        assert!(registry.register(response_id, user_id).is_err());
    }

    #[test]
    fn registration_is_removed_only_after_owner_and_children_finish() {
        let registry = ResponseExecutionRegistry::default();
        let response_id = Uuid::new_v4();
        let registration = registry.register(response_id, Uuid::new_v4()).unwrap();
        let execution = registration.execution();
        let child = execution.begin_task().unwrap();

        drop(registration);
        assert!(registry.contains(response_id));

        drop(child);
        assert!(!registry.contains(response_id));
        assert!(execution.begin_task().is_err());
    }

    #[tokio::test]
    async fn cancellation_atomically_seals_late_task_admission() {
        let registry = ResponseExecutionRegistry::default();
        let registration = registry.register(Uuid::new_v4(), Uuid::new_v4()).unwrap();
        let execution = registration.execution();
        let existing_child = execution.begin_task().unwrap();

        execution.cancel();
        assert!(execution.begin_task().is_err());
        drop(registration);

        let waiter = tokio::spawn({
            let execution = execution.clone();
            async move { execution.wait_for_quiescence().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        drop(existing_child);
        timeout(Duration::from_millis(50), waiter)
            .await
            .expect("sealed execution reaches stable quiescence")
            .expect("join quiescence waiter");
        assert!(execution.begin_task().is_err());
    }
}
