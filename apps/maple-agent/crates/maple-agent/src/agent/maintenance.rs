//! Host-local maintenance over durable new-store queues. No task owns this
//! worker: dropping the service or store ends it without retaining either.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::store::integrity::{IntegrityCancellation, IntegrityKind, IntegrityOutcome};
use crate::store::{AccountId, PreparedRebuild, RebuildProgress, Store, StoreError, StoreState};

use super::{
    AGENT_SERVICE_OPEN, AgentEventDispatcher, AgentServiceEvent, MapleAgentService, ServiceState,
    emit_agent_event, runtime::AgentRuntime,
};

const TICK_INTERVAL: Duration = Duration::from_secs(2);
const TASK_GC_STEPS: u16 = 4;
const ACCOUNT_GC_STEPS: u16 = 2;
const REBUILD_CANDIDATES: u16 = 16;
const WARNING_INTERVAL: Duration = Duration::from_secs(60);
const INTEGRITY_RETRY_INTERVAL: Duration = Duration::from_secs(60);
const STORAGE_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(10);
const OPTIMIZE_INTERVAL: Duration = Duration::from_secs(60 * 60);

type IntegritySlot = Mutex<Option<Arc<IntegrityCancellation>>>;

#[derive(Clone)]
struct IntegrityHooks {
    slot: Weak<IntegritySlot>,
    idle: Weak<tokio::sync::Notify>,
}

/// Clearing this slot means the checker has returned, including kill+wait for
/// an isolated child. Completed host shutdown waits for that exact event.
struct IntegrityRegistration {
    slot: Arc<IntegritySlot>,
    idle: Arc<tokio::sync::Notify>,
    cancellation: Arc<IntegrityCancellation>,
}

impl Drop for IntegrityRegistration {
    fn drop(&mut self) {
        self.cancellation.cancel();
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.cancellation))
        {
            *slot = None;
            self.idle.notify_waiters();
        }
    }
}

pub(super) fn start(service: &MapleAgentService) {
    let Some(store) = service.host.store.as_ref() else {
        return;
    };
    let Ok(executor) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let state = Arc::downgrade(&service.state);
    let store = Arc::downgrade(store);
    // A sink can own host resources in turn, so it must also remain weak while
    // sleeping. The broadcast sender itself retains no service/store handle.
    let sink = Arc::downgrade(&service.host.events.sink);
    let subscribers = service.host.events.subscribers.clone();
    let integrity = IntegrityHooks {
        slot: Arc::downgrade(&service.host.integrity_check),
        idle: Arc::downgrade(&service.host.integrity_idle),
    };
    executor.spawn(async move {
        let mut worker = Worker {
            integrity: Some(integrity),
            ..Worker::default()
        };
        let mut ticks = tokio::time::interval(TICK_INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticks.tick().await;
            let Some(state) = state.upgrade() else { break };
            let Some(store) = store.upgrade() else { break };
            if !admitted(&state) {
                worker.reset();
                continue;
            }
            if store.state() != StoreState::Open {
                worker.reset();
                continue;
            }
            // Start, stop, drain and account changes take this same lifecycle
            // gate. Maintenance skips contention; it never queues ahead of them.
            let Ok(_lifecycle) = state.lifecycle.try_lock() else {
                continue;
            };
            if !admitted(&state) {
                worker.reset();
                continue;
            }
            let runtime = match state.runtime.try_lock() {
                Ok(runtime) => runtime.clone(),
                Err(_) => continue,
            };
            drop(_lifecycle);
            let Some(sink) = sink.upgrade() else { break };
            let events = AgentEventDispatcher {
                sink,
                subscribers: subscribers.clone(),
            };
            let tick_state = Arc::clone(&state);
            // SQLite, source preparation and compression never occupy a Tokio
            // executor thread. Bulk preparation holds no service lifecycle
            // lock; every effect reacquires it and reads the current runtime.
            // No strong service/store/runtime handle survives this tick.
            match tokio::task::spawn_blocking(move || {
                worker.tick(&tick_state, &store, runtime.as_ref(), &events);
                worker
            })
            .await
            {
                Ok(next) => worker = next,
                Err(_) => {
                    log::warn!("Agent local-store maintenance worker stopped unexpectedly");
                    break;
                }
            }
        }
    });
}

fn admitted(state: &ServiceState) -> bool {
    state.admission.load(Ordering::Acquire) == AGENT_SERVICE_OPEN
        // Non-runnable does not mean unloaded: pending shutdown still owns
        // source entries, blob leases and account/session identity. All idle
        // effects yield until Retry or another completed shutdown saves them.
        && state
            .shutdown_pending
            .try_read()
            .is_ok_and(|pending| pending.is_none())
}

struct RebuildJob {
    account: AccountId,
    user_id: String,
    uuid: String,
    prepared: PreparedRebuild,
}

#[derive(Default)]
struct Worker {
    account_cursor: Option<AccountId>,
    old_epoch_cursors: HashMap<AccountId, i64>,
    rebuild: Option<RebuildJob>,
    orphan_pass_done: bool,
    last_warning: Option<Instant>,
    integrity: Option<IntegrityHooks>,
    last_integrity_attempt: Option<Instant>,
    last_storage_maintenance: Option<Instant>,
    last_optimize: Option<Instant>,
}

impl Worker {
    fn reset(&mut self) {
        let integrity = self.integrity.clone();
        *self = Self {
            integrity,
            ..Self::default()
        };
    }
    /// At most four deleted-task steps, two account steps, one orphan-blob
    /// step, one rebuild commit and one old-epoch step run in a tick. Every
    /// payload mutation uses the store's bounded idle-writer transactions.
    fn tick(
        &mut self,
        state: &ServiceState,
        store: &Arc<Store>,
        runtime: Option<&Arc<AgentRuntime>>,
        events: &AgentEventDispatcher,
    ) {
        if !admitted(state) || store.state() != StoreState::Open {
            return;
        }
        self.collect_deleted_tasks(state, store);
        if !admitted(state) {
            return;
        }
        self.collect_confirmed_accounts(state, store);
        if !admitted(state) {
            return;
        }
        if !self.orphan_pass_done {
            match while_service_open(state, || store.collect_orphan_blobs_step()) {
                Ok(progress) => self.orphan_pass_done = progress.done,
                Err(error) => self.report("orphan blob collection", error),
            }
        }
        if !admitted(state) {
            return;
        }

        let account = match store.accounts_for_maintenance(self.account_cursor, 1) {
            Ok(accounts) => accounts.first().copied(),
            Err(error) => {
                self.report("account discovery", error);
                None
            }
        };
        self.account_cursor = account;
        if let Some(account) = account {
            if self.rebuild.is_none() {
                self.select_rebuild(state, store, account);
            }
            self.collect_old_epochs(state, store, account);
        }
        if !admitted(state) {
            return;
        }
        self.advance_rebuild(state, store, runtime, events);
        if admitted(state) {
            self.maintain_storage_when_idle(state, store);
            self.check_integrity_when_idle(state, store, events);
        }
    }

    fn maintain_storage_when_idle(&mut self, state: &ServiceState, store: &Arc<Store>) {
        if self
            .last_storage_maintenance
            .is_some_and(|last| last.elapsed() < STORAGE_MAINTENANCE_INTERVAL)
        {
            return;
        }
        let optimize = self
            .last_optimize
            .is_none_or(|last| last.elapsed() >= OPTIMIZE_INTERVAL);
        match while_all_tasks_idle(state, || store.idle_storage_maintenance(optimize)) {
            Ok(()) => {
                let now = Instant::now();
                self.last_storage_maintenance = Some(now);
                if optimize {
                    self.last_optimize = Some(now);
                }
            }
            Err(StoreError::Busy) => {}
            Err(error) => {
                self.last_storage_maintenance = Some(Instant::now());
                self.report("physical storage maintenance", error);
            }
        }
    }

    fn check_integrity_when_idle(
        &mut self,
        state: &ServiceState,
        store: &Arc<Store>,
        events: &AgentEventDispatcher,
    ) {
        if self
            .last_integrity_attempt
            .is_some_and(|last| last.elapsed() < INTEGRITY_RETRY_INTERVAL)
        {
            return;
        }
        let Some(hooks) = &self.integrity else {
            return;
        };
        let (Some(slot), Some(idle)) = (hooks.slot.upgrade(), hooks.idle.upgrade()) else {
            return;
        };
        let due = match store.integrity_schedule(chrono::Utc::now().timestamp_millis(), false) {
            Ok(due) => due,
            Err(error) => {
                self.report("integrity scheduling", error);
                return;
            }
        };
        let kind = if due.quick {
            IntegrityKind::Quick
        } else if due.full {
            IntegrityKind::Full
        } else {
            return;
        };
        let now_ms = chrono::Utc::now().timestamp_millis();
        if kind == IntegrityKind::Full {
            match store.idle_integrity_retry_ready(now_ms) {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    self.report("integrity retry scheduling", error);
                    return;
                }
            }
        }
        let cancellation = Arc::new(IntegrityCancellation::default());
        let registration = match while_all_tasks_idle(state, || {
            if store.state() != StoreState::Open {
                return Err(StoreError::Busy);
            }
            let mut active = slot.lock().map_err(|_| StoreError::Busy)?;
            if active.is_some() {
                return Err(StoreError::Busy);
            }
            let budget = if kind == IntegrityKind::Quick {
                Duration::from_millis(100)
            } else {
                store
                    .begin_idle_integrity_attempt(now_ms)?
                    .ok_or(StoreError::Busy)?
            };
            *active = Some(Arc::clone(&cancellation));
            drop(active);
            Ok((
                IntegrityRegistration {
                    slot,
                    idle,
                    cancellation: Arc::clone(&cancellation),
                },
                budget,
            ))
        }) {
            Ok(registration) => registration,
            Err(StoreError::Busy) => return,
            Err(error) => {
                self.report("integrity admission", error);
                return;
            }
        };
        self.last_integrity_attempt = Some(Instant::now());
        let (registration, budget) = registration;
        // Admission/lifecycle guards have been released. New runs cancel this
        // token while acquiring their own run or external-agent launch lock.
        let report = store.check_integrity(kind, budget, &cancellation);
        match report {
            Ok(report) => {
                if report.outcome == IntegrityOutcome::Clean
                    && report.complete
                    && let Some(receipt) = report.receipt.as_ref()
                {
                    // Recheck idle under the same admission locks before
                    // persisting verification of this exact generation.
                    if let Err(error) = while_all_tasks_idle(state, || {
                        if cancellation.is_cancelled() {
                            return Err(StoreError::Busy);
                        }
                        store.record_integrity_clean(receipt, &cancellation)
                    }) {
                        self.report("integrity confirmation", error);
                    }
                }
                if matches!(store.state(), StoreState::Damaged { .. }) {
                    emit_agent_event(
                        events,
                        AgentServiceEvent::StoreStateChanged {
                            state: store.state(),
                        },
                    );
                }
            }
            Err(error) => self.report("integrity check", error),
        }
        // Drop only after the child checker has been reaped and final-record
        // admission has ended, so shutdown cannot release the owner early.
        drop(registration);
    }

    fn collect_deleted_tasks(&mut self, state: &ServiceState, store: &Arc<Store>) {
        let tasks = match store.deleted_tasks(TASK_GC_STEPS) {
            Ok(tasks) => tasks,
            Err(error) => {
                self.report("deleted-task discovery", error);
                return;
            }
        };
        for task in tasks {
            if !admitted(state) {
                break;
            }
            match while_task_idle(state, task.account, &task.uuid, || {
                // A crash can leave a hidden task's transient output behind.
                // Keep its UUID queued until exact owned-directory cleanup
                // succeeds. No SQLite transaction is held during filesystem
                // deletion, and run/launch admission prevents new writers.
                crate::store::spill::SpillRoot::new(store.paths())?.remove_task(&task.uuid)?;
                store.collect_task_step(task.account, &task.uuid)
            }) {
                Ok(_) => {}
                Err(StoreError::Busy) => continue,
                Err(error) => self.report("task collection", error),
            }
        }
    }

    fn collect_confirmed_accounts(&mut self, state: &ServiceState, store: &Arc<Store>) {
        let accounts = match store.confirmed_account_deletions(ACCOUNT_GC_STEPS) {
            Ok(accounts) => accounts,
            Err(error) => {
                self.report("confirmed-account discovery", error);
                return;
            }
        };
        for account in accounts {
            if !admitted(state) {
                break;
            }
            if let Err(error) = while_service_open(state, || store.collect_account_step(account)) {
                self.report("account collection", error);
            }
        }
    }

    fn select_rebuild(&mut self, state: &ServiceState, store: &Arc<Store>, account: AccountId) {
        let user_id = match store.account_user_id(account) {
            Ok(Some(user_id)) => user_id,
            Ok(None) => return,
            Err(error) => {
                self.report("timeline repair owner", error);
                return;
            }
        };
        let tasks = match store.timeline_rebuild_queue(account, REBUILD_CANDIDATES) {
            Ok(tasks) => tasks,
            Err(error) => {
                self.report("timeline repair discovery", error);
                return;
            }
        };
        for uuid in tasks {
            if !admitted(state) {
                return;
            }
            // Only the short enqueue write holds normal run admission and all
            // external-agent launch/state guards. Source parsing happens below.
            let ticket = match while_task_idle(state, account, &uuid, || {
                store.begin_timeline_rebuild(account, &uuid, false)
            }) {
                Ok(ticket) => ticket,
                Err(StoreError::Busy) => continue,
                Err(error) => {
                    self.report("timeline repair enqueue", error);
                    continue;
                }
            };
            match store.prepare_timeline_rebuild(&ticket) {
                Ok(prepared) => {
                    self.rebuild = Some(RebuildJob {
                        account,
                        user_id,
                        uuid,
                        prepared,
                    });
                    return;
                }
                Err(error) => self.report("timeline repair snapshot", error),
            }
            return;
        }
    }

    fn advance_rebuild(
        &mut self,
        state: &ServiceState,
        store: &Arc<Store>,
        runtime: Option<&Arc<AgentRuntime>>,
        events: &AgentEventDispatcher,
    ) {
        let Some(job) = self.rebuild.take() else {
            return;
        };
        if !admitted(state) {
            return;
        }
        // A snapshot is only a hint to avoid wasted parsing. The final mutation
        // below obtains a fresh current-runtime admission guard in all cases.
        if let Some(runtime) = runtime.filter(|runtime| runtime.store.account() == job.account)
            && runtime
                .maintenance_while_idle(&job.uuid, || Ok(()))
                .is_err()
        {
            self.rebuild = Some(job);
            return;
        }
        // Prepared chunks contain encoded SQL bindings only. Revision checks in
        // the final transaction reject any source changes made during parsing.
        let chunk = match store.prepare_timeline_chunk(&job.prepared) {
            Ok(chunk) => chunk,
            Err(StoreError::Busy) => {
                self.rebuild = Some(job);
                return;
            }
            Err(error) => {
                self.report("timeline repair preparation", error);
                return;
            }
        };
        match while_task_idle(state, job.account, &job.uuid, || {
            store.commit_timeline_chunk(&chunk, false)
        }) {
            Ok(RebuildProgress::Pending { .. }) | Err(StoreError::Busy) => self.rebuild = Some(job),
            Ok(RebuildProgress::NeedsSnapshot) => {}
            Ok(RebuildProgress::Replaced { epoch }) => emit_agent_event(
                events,
                AgentServiceEvent::HistoryReplaced {
                    user_id: job.user_id,
                    session_id: job.uuid,
                    rows_epoch: epoch,
                },
            ),
            Err(error) => self.report("timeline repair commit", error),
        }
    }

    fn collect_old_epochs(&mut self, state: &ServiceState, store: &Arc<Store>, account: AccountId) {
        let cursor = self.old_epoch_cursors.get(&account).copied();
        let candidate = match store.old_timeline_queue(account, 1, cursor) {
            Ok(rows) => rows.into_iter().next(),
            Err(error) => {
                self.report("obsolete timeline discovery", error);
                return;
            }
        };
        let Some((key, uuid)) = candidate else {
            self.old_epoch_cursors.remove(&account);
            return;
        };
        match while_task_idle(state, account, &uuid, || {
            store.collect_old_timeline_chunk(account, &uuid)
        }) {
            Ok(_) => {
                self.old_epoch_cursors.insert(account, key);
            }
            Err(StoreError::Busy) => {
                self.old_epoch_cursors.insert(account, key);
            }
            Err(error) => {
                self.old_epoch_cursors.insert(account, key);
                self.report("obsolete timeline collection", error);
            }
        }
    }

    fn report(&mut self, action: &'static str, error: StoreError) {
        if matches!(
            error,
            StoreError::Busy
                | StoreError::Conflict
                | StoreError::NotFound
                | StoreError::EpochChanged
                | StoreError::Locked
                | StoreError::NeedsSetup
        ) {
            return;
        }
        let now = Instant::now();
        if self
            .last_warning
            .is_some_and(|last| now.duration_since(last) < WARNING_INTERVAL)
        {
            return;
        }
        self.last_warning = Some(now);
        // StoreError deliberately excludes SQL and stored values. Candidate ids
        // and source payloads never enter maintenance diagnostics.
        log::warn!("Agent local-store {action} could not finish: {error}");
    }
}

fn while_service_open<T>(
    state: &ServiceState,
    action: impl FnOnce() -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let _lifecycle = state.lifecycle.try_lock().map_err(|_| StoreError::Busy)?;
    if !admitted(state) {
        return Err(StoreError::Busy);
    }
    action()
}

pub(super) fn while_task_idle<T>(
    state: &ServiceState,
    account: AccountId,
    uuid: &str,
    action: impl FnOnce() -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    while_service_open(state, || {
        let runtime = state
            .runtime
            .try_lock()
            .map_err(|_| StoreError::Busy)?
            .clone();
        match runtime.filter(|runtime| runtime.store.account() == account) {
            Some(runtime) => runtime
                .maintenance_while_idle(uuid, || Ok(action()))
                .map_err(|_| StoreError::Busy)?,
            None => action(),
        }
    })
}

fn while_all_tasks_idle<T>(
    state: &ServiceState,
    action: impl FnOnce() -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    while_service_open(state, || {
        let runtime = state
            .runtime
            .try_lock()
            .map_err(|_| StoreError::Busy)?
            .clone();
        match runtime {
            Some(runtime) => runtime
                .maintenance_while_all_idle(|| Ok(action()))
                .map_err(|_| StoreError::Busy)?,
            None => action(),
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use pi_ai::{Content, Message, UserMessage};
    use pi_coding_agent::SessionMessage;

    use super::super::{
        AGENT_SERVICE_DRAINING, AgentEventSink, AgentPathLayout, MapleAgentHostResources,
    };
    use super::*;
    use crate::store::{NewTask, SessionLoad, StorePaths, TaskKind};

    const TASK: &str = "00000000-0000-7000-8000-000000000001";

    #[derive(Default)]
    struct Events(Mutex<Vec<AgentServiceEvent>>);
    impl AgentEventSink for Events {
        fn emit(&self, event: &AgentServiceEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    fn fixture() -> (
        tempfile::TempDir,
        Arc<Store>,
        MapleAgentService,
        Arc<Events>,
        AccountId,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(StorePaths::new(directory.path().join("local"))).unwrap());
        let account = store.upsert_account("maintenance@example.test", 1).unwrap();
        let events = Arc::new(Events::default());
        let paths = AgentPathLayout::from_app_roots(
            directory.path().join("config"),
            directory.path().join("local"),
        );
        let service = MapleAgentService::new(
            MapleAgentHostResources::new(paths, events.clone(), String::new())
                .with_store(Arc::clone(&store)),
        );
        (directory, store, service, events, account)
    }

    fn create_task(store: &Arc<Store>, account: AccountId) {
        store
            .create_task(
                account,
                &NewTask {
                    uuid: TASK.into(),
                    project_root: if cfg!(windows) {
                        "C:/project"
                    } else {
                        "/project"
                    }
                    .into(),
                    kind: TaskKind::Desktop,
                    title: "Maintenance".into(),
                    provisional: false,
                    model: None,
                    created_ms: 1,
                },
            )
            .unwrap();
    }

    #[test]
    fn interrupted_confirmed_deletion_finishes_but_pending_attempt_does_not_collect() {
        let (_directory, store, service, _events, account) = fixture();
        create_task(&store, account);
        store
            .begin_account_deletion(account, "00000000-0000-4000-8000-000000000001", 2)
            .unwrap();
        let mut worker = Worker::default();
        worker.tick(&service.state, &store, None, &service.host.events);
        assert!(store.task(account, TASK).unwrap().is_some());
        assert!(
            store
                .account_id("maintenance@example.test")
                .unwrap()
                .is_some()
        );
        store
            .confirm_account_deletion(account, "00000000-0000-4000-8000-000000000001")
            .unwrap();
        // This fresh worker represents a host restart after server confirmation.
        let mut restarted = Worker::default();
        for _ in 0..12 {
            restarted.tick(&service.state, &store, None, &service.host.events);
        }
        assert!(store.deleted_tasks(10).unwrap().is_empty());
        assert!(
            store
                .account_id("maintenance@example.test")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn repair_emits_replaced_after_flip_then_collects_only_the_obsolete_epoch() {
        let (_directory, store, service, events, account) = fixture();
        create_task(&store, account);
        let mut manager = store
            .load_session(account, TASK, SessionLoad::Context)
            .unwrap()
            .into_manager("/project");
        manager.append_message(SessionMessage::Llm(Message::User(UserMessage {
            content: vec![Content::text("Keep authoritative history")],
            timestamp: 10,
        })));
        assert!(manager.take_persist_error().is_none());
        let before = store.task(account, TASK).unwrap().unwrap().rows_epoch;
        store.begin_timeline_rebuild(account, TASK, false).unwrap();
        let mut worker = Worker::default();
        for _ in 0..4 {
            worker.tick(&service.state, &store, None, &service.host.events);
        }
        let after = store.task(account, TASK).unwrap().unwrap().rows_epoch;
        assert!(after > before);
        assert!(events.0.lock().unwrap().iter().any(|event| matches!(event,
            AgentServiceEvent::HistoryReplaced { user_id, session_id, rows_epoch } if user_id == "maintenance@example.test" && session_id == TASK && *rows_epoch == after)));
        assert!(
            store
                .old_timeline_queue(account, 10, None)
                .unwrap()
                .is_empty()
        );
        let reopened = store
            .load_session(account, TASK, SessionLoad::Full)
            .unwrap()
            .into_manager("/project");
        assert_eq!(reopened.branch().len(), 1);
    }

    #[test]
    fn draining_admission_prevents_collection_and_repair_enqueue() {
        let (_directory, store, service, _events, account) = fixture();
        create_task(&store, account);
        store.mark_task_deleted(account, TASK, 2).unwrap();
        service
            .state
            .admission
            .store(AGENT_SERVICE_DRAINING, Ordering::Release);
        Worker::default().tick(&service.state, &store, None, &service.host.events);
        assert_eq!(store.deleted_tasks(10).unwrap().len(), 1);
        assert_eq!(
            while_service_open(&service.state, || Ok(())),
            Err(StoreError::Busy)
        );
    }

    #[test]
    fn resumed_task_collection_clears_only_its_new_spill_directory() {
        let (_directory, store, service, _events, account) = fixture();
        create_task(&store, account);
        let spills = crate::store::spill::SpillRoot::new(store.paths()).unwrap();
        let task_dir = spills.task_dir(TASK).unwrap();
        std::fs::write(task_dir.join("output"), b"transient task output").unwrap();
        let other = "00000000-0000-7000-8000-000000000002";
        let other_dir = spills.task_dir(other).unwrap();
        std::fs::write(other_dir.join("output"), b"another task").unwrap();
        store.mark_task_deleted(account, TASK, 2).unwrap();
        Worker::default().tick(&service.state, &store, None, &service.host.events);
        assert!(!task_dir.exists());
        assert_eq!(
            std::fs::read(other_dir.join("output")).unwrap(),
            b"another task"
        );
        spills.cleanup().unwrap();
        std::fs::remove_dir(spills.path().parent().unwrap()).unwrap();
    }

    #[test]
    fn completed_shutdown_waits_for_integrity_checker_reap() {
        // Construct outside Tokio so the ordinary periodic worker does not
        // contend with this explicit child-lifetime simulation.
        let (_directory, _store, service, _events, _account) = fixture();
        let cancellation = Arc::new(IntegrityCancellation::default());
        *service.host.integrity_check.lock().unwrap() = Some(Arc::clone(&cancellation));
        let checker = IntegrityRegistration {
            slot: Arc::clone(&service.host.integrity_check),
            idle: Arc::clone(&service.host.integrity_idle),
            cancellation: Arc::clone(&cancellation),
        };
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move {
                let stopping = tokio::spawn(async move { service.shutdown().await });
                tokio::time::timeout(Duration::from_secs(5), async {
                    while !cancellation.is_cancelled() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert!(cancellation.is_cancelled());
                assert!(
                    !stopping.is_finished(),
                    "cancel alone cannot release host ownership"
                );
                drop(checker); // The checker has now returned after kill+wait.
                stopping.await.unwrap().unwrap();
            });
    }

    #[test]
    fn lifecycle_contention_does_not_register_an_integrity_child() {
        let (_directory, store, service, _events, _account) = fixture();
        let mut worker = Worker {
            integrity: Some(IntegrityHooks {
                slot: Arc::downgrade(&service.host.integrity_check),
                idle: Arc::downgrade(&service.host.integrity_idle),
            }),
            ..Worker::default()
        };
        let _busy = service.state.lifecycle.try_lock().unwrap();
        worker.check_integrity_when_idle(&service.state, &store, &service.host.events);
        assert!(service.host.integrity_check.lock().unwrap().is_none());
        assert!(worker.last_integrity_attempt.is_none());
        worker.reset();
        assert!(
            worker.integrity.is_some(),
            "temporary draining must preserve scheduling hooks"
        );
    }

    #[test]
    fn lifecycle_contention_yields_without_calling_the_mutation() {
        let (_directory, _store, service, _events, _account) = fixture();
        let _lifecycle = service.state.lifecycle.try_lock().unwrap();
        assert_eq!(
            while_service_open(&service.state, || panic!("must yield")),
            Err::<(), _>(StoreError::Busy)
        );
    }

    #[tokio::test]
    async fn pending_shutdown_defers_task_collection_until_source_history_is_saved() {
        use super::super::AgentRunTerminal;
        use super::super::tests::{Harness, finished};

        let harness = Harness::new().await;
        let runtime = harness.runtime().await;
        let task = harness.create_task().await;
        let deleted = harness.create_task().await;
        harness.faux.push_text("Saved before maintenance waits.");
        let mut run = harness.send(&task, "Retain this task").await;
        assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
        let session = runtime.loaded_session(&task).await.unwrap();
        let store = harness.service.host.store.as_ref().unwrap();
        let account = runtime.store.account();
        harness.service.begin_draining();
        store.mark_task_deleted(account, &deleted, 2).unwrap();
        store.close_for_shutdown().unwrap();
        session.set_session_name("Pending source history").await;
        assert!(harness.service.shutdown().await.is_err());
        store.initialize_plain().unwrap();
        harness.service.resume_admission();

        // Run the real worker with an empty runnable-runtime snapshot. Its
        // durable queue must remain intact while the pending owner is retained.
        Worker::default().tick(
            &harness.service.state,
            store,
            None,
            &harness.service.host.events,
        );
        assert!(
            store
                .deleted_tasks(10)
                .unwrap()
                .iter()
                .any(|row| row.uuid == deleted)
        );
        assert_eq!(
            while_task_idle(&harness.service.state, account, &task, || panic!(
                "pending source must block mutation"
            )),
            Err::<(), _>(StoreError::Busy)
        );
        assert_eq!(
            while_all_tasks_idle(&harness.service.state, || panic!(
                "pending source must block idle effects"
            )),
            Err::<(), _>(StoreError::Busy)
        );
        assert_eq!(
            while_service_open(&harness.service.state, || panic!(
                "pending source must block account/orphan GC"
            )),
            Err::<(), _>(StoreError::Busy)
        );
        assert!(session.has_unpersisted());

        harness.service.flush_persistence().await.unwrap();
        assert!(!session.has_unpersisted());
        let mut worker = Worker::default();
        for _ in 0..8 {
            worker.tick(
                &harness.service.state,
                store,
                None,
                &harness.service.host.events,
            );
        }
        assert!(
            !store
                .deleted_tasks(10)
                .unwrap()
                .iter()
                .any(|row| row.uuid == deleted)
        );
        assert!(store.task(account, &task).unwrap().is_some());
        harness.service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn sleeping_worker_retains_neither_the_service_nor_its_store() {
        let (_directory, store, service, events, _account) = fixture();
        let weak_state = Arc::downgrade(&service.state);
        let weak_store = Arc::downgrade(&store);
        start(&service);
        drop(service);
        drop(store);
        drop(events);
        tokio::task::yield_now().await;
        assert!(weak_state.upgrade().is_none());
        assert!(weak_store.upgrade().is_none());
    }
}
