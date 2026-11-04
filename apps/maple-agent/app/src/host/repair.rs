//! Local-only integrity and explicit recovery adapter. Native mutation exposure
//! stays gated until G3. The host owns previews; a client never supplies a loss
//! report or a store key as recovery authority.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::HostFailure;
use maple_agent::store::integrity::{
    IntegrityCancellation, IntegrityKind, IntegrityReport, SALVAGE_CONFIRMATION, SalvagePreview,
    SalvageReport,
};
use maple_agent::store::keys::Password;
use maple_agent::store::{Store, StoreState};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const PAGE_SIZE: usize = 100;
const MAX_LOSS_ROWS: usize = 10_000;
const PREVIEW_TTL: Duration = Duration::from_secs(10 * 60);
const REPAIR_BUDGET: Duration = Duration::from_secs(5 * 60);

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        "repair_status"
            | "repair_progress"
            | "cancel_store_repair"
            | "authenticate_repair_password"
            | "check_store_integrity"
            | "preview_salvage"
            | "salvage_preview_page"
            | "salvage_store"
    )
}

/// These operations need the owner's serialized drain/admission path. Checking
/// can quarantine damage, even though it never edits authoritative user rows.
pub(super) fn changes_store(method: &str) -> bool {
    matches!(
        method,
        "authenticate_repair_password"
            | "check_store_integrity"
            | "preview_salvage"
            | "salvage_store"
    )
}

pub(super) fn requires_native_validation(method: &str) -> bool {
    matches!(
        method,
        "authenticate_repair_password"
            | "preview_salvage"
            | "salvage_preview_page"
            | "salvage_store"
    )
}

/// Reject stale clients before the owner acquires an operation lock, drains
/// the runtime, closes handles, or creates recovery state.
pub(super) fn retired_operation_error(method: &str) -> Option<&'static str> {
    (method == "repair_store").then_some(
        "In-place local store repair is unavailable. The current store has been left unchanged.",
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Generation {
    store_id: Option<String>,
    setup_operation_id: Option<String>,
}

fn generation(store: &Store) -> Result<Generation, HostFailure> {
    let status = store.lock_status().map_err(HostFailure::from)?;
    Ok(Generation {
        store_id: status.store_id,
        setup_operation_id: status.setup_operation_id,
    })
}

struct StoredPreview {
    id: String,
    authority: SalvagePreview,
    generation: Generation,
    created: Instant,
    reviewed_pages: BTreeSet<usize>,
}

#[derive(Clone, Copy)]
enum Work {
    Check,
    Preview,
    Salvage,
    Authenticate,
}

#[derive(Clone)]
struct ActiveWork {
    id: String,
    work: Work,
    cancellation: IntegrityCancellation,
    baseline_run_id: u64,
    store: Arc<Store>,
}

#[derive(Default)]
struct RepairState {
    generation: Option<Generation>,
    authenticated: bool,
    preview: Option<StoredPreview>,
    diagnostic: Option<IntegrityReport>,
    active: Option<ActiveWork>,
}

#[derive(Default)]
pub(super) struct RepairHost {
    state: Arc<Mutex<RepairState>>,
}

struct Registration {
    state: Arc<Mutex<RepairState>>,
    id: String,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock()
            && state
                .active
                .as_ref()
                .is_some_and(|active| active.id == self.id)
        {
            state.active = None;
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Authentication<'a> {
    password: &'a str,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckRequest {
    kind: Option<IntegrityKind>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewPage<'a> {
    preview_id: &'a str,
    #[serde(default)]
    offset: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SavePreview<'a> {
    preview_id: &'a str,
    confirmation: &'a str,
}

fn decode<'a, T: Deserialize<'a>>(value: &'a Value) -> Result<T, HostFailure> {
    T::deserialize(value)
        .map_err(|_| HostFailure::new("repair-request", "Invalid local recovery request"))
}

fn encode(value: impl Serialize) -> Result<Value, HostFailure> {
    serde_json::to_value(value).map_err(|_| {
        HostFailure::new(
            "repair-response",
            "Could not encode local recovery response",
        )
    })
}

fn validate_token(value: &str) -> Result<(), HostFailure> {
    if value.len() != 36 || uuid::Uuid::parse_str(value).is_err() {
        return Err(HostFailure::new(
            "repair-preview",
            "Review a new recovery preview.",
        ));
    }
    Ok(())
}

fn require_damaged(store: &Store) -> Result<(), HostFailure> {
    if !matches!(store.state(), StoreState::Damaged { .. }) {
        return Err(HostFailure::new(
            "repair-damaged",
            "Recovery requires a damaged local store.",
        ));
    }
    Ok(())
}

fn synchronize_generation(state: &mut RepairState, current: Generation) {
    if state.generation.as_ref() != Some(&current) {
        state.generation = Some(current);
        state.authenticated = false;
        state.preview = None;
        state.diagnostic = None;
    }
    if state
        .preview
        .as_ref()
        .is_some_and(|preview| preview.created.elapsed() >= PREVIEW_TTL)
    {
        state.preview = None;
    }
}

impl RepairHost {
    fn start(
        &self,
        store: Arc<Store>,
        work: Work,
    ) -> Result<(Registration, IntegrityCancellation), HostFailure> {
        let current = generation(&store)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| HostFailure::new("repair-unavailable", "Local recovery is unavailable"))?;
        if state.active.is_some() {
            return Err(HostFailure::new(
                "repair-busy",
                "A local recovery operation is already running.",
            ));
        }
        synchronize_generation(&mut state, current);
        let id = uuid::Uuid::new_v4().to_string();
        let cancellation = IntegrityCancellation::default();
        state.active = Some(ActiveWork {
            id: id.clone(),
            work,
            cancellation: cancellation.clone(),
            baseline_run_id: store.store_progress().run_id,
            store,
        });
        Ok((
            Registration {
                state: Arc::clone(&self.state),
                id,
            },
            cancellation,
        ))
    }

    pub(super) fn cancel(&self) -> bool {
        let active = self
            .state
            .lock()
            .ok()
            .and_then(|state| state.active.clone());
        match active {
            Some(active) if matches!(active.work, Work::Check | Work::Preview) => {
                active.cancellation.cancel();
                true
            }
            Some(active) if matches!(active.work, Work::Salvage) => {
                active.store.cancel_store_operation()
            }
            _ => false,
        }
    }

    pub(super) fn progress(&self) -> Option<Value> {
        let active = self.state.lock().ok()?.active.clone()?;
        let (phase, completed, total, cancellable) = match active.work {
            Work::Check => ("checking".to_owned(), 0, 0, true),
            Work::Preview => ("previewing".to_owned(), 0, 0, true),
            Work::Authenticate => ("authenticating".to_owned(), 0, 0, false),
            Work::Salvage => {
                let progress =
                    active_salvage_progress(active.baseline_run_id, active.store.store_progress());
                let phase = serde_json::to_value(progress.phase)
                    .ok()?
                    .as_str()?
                    .to_owned();
                (
                    phase,
                    progress.completed_bytes,
                    progress.total_bytes,
                    progress.cancellable,
                )
            }
        };
        Some(serde_json::json!({
            "busy":true, "operation":"repair", "phase":phase, "completed":completed, "total":total,
            "completed_bytes":completed, "total_bytes":total, "remaining_ms":null,
            "cancellable":cancellable
        }))
    }

    fn status(&self, store: &Store) -> Result<Value, HostFailure> {
        let active = self
            .state
            .lock()
            .map_err(|_| HostFailure::new("repair-unavailable", "Local recovery is unavailable"))?
            .active
            .is_some();
        // A replacement may be between generation-qualified renames while its
        // worker is active. Do not clear authority from an intermediate view.
        if !active {
            let current = generation(store)?;
            let mut state = self.state.lock().map_err(|_| {
                HostFailure::new("repair-unavailable", "Local recovery is unavailable")
            })?;
            if state.active.is_none() {
                synchronize_generation(&mut state, current);
            }
        }
        let state = self
            .state
            .lock()
            .map_err(|_| HostFailure::new("repair-unavailable", "Local recovery is unavailable"))?;
        let mut value = serde_json::json!({
            "busy":state.active.is_some(),
            "authenticated":state.authenticated,
            "preview_id":state.preview.as_ref().map(|preview| preview.id.as_str()),
            "diagnostic":state.diagnostic,
        });
        drop(state);
        value["progress"] = self.progress().unwrap_or_else(idle_progress);
        Ok(value)
    }

    fn preview_page(
        &self,
        store: &Store,
        token: &str,
        offset: usize,
    ) -> Result<Value, HostFailure> {
        validate_token(token)?;
        require_damaged(store)?;
        let current = generation(store)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| HostFailure::new("repair-unavailable", "Local recovery is unavailable"))?;
        if state.active.is_some() {
            return Err(HostFailure::new(
                "repair-busy",
                "A local recovery operation is already running.",
            ));
        }
        synchronize_generation(&mut state, current.clone());
        let preview = state
            .preview
            .as_mut()
            .filter(|preview| preview.id == token && preview.generation == current)
            .ok_or_else(|| HostFailure::new("repair-preview", "Review a new recovery preview."))?;
        page(preview, offset)
    }

    fn take_preview(&self, store: &Store, token: &str) -> Result<SalvagePreview, HostFailure> {
        validate_token(token)?;
        require_damaged(store)?;
        let current = generation(store)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| HostFailure::new("repair-unavailable", "Local recovery is unavailable"))?;
        synchronize_generation(&mut state, current.clone());
        let preview = state
            .preview
            .as_ref()
            .filter(|preview| preview.id == token && preview.generation == current)
            .ok_or_else(|| HostFailure::new("repair-preview", "Review a new recovery preview."))?;
        if !review_is_complete(
            preview.authority.report().dropped_rows.len(),
            &preview.reviewed_pages,
        ) {
            return Err(HostFailure::new(
                "repair-review",
                "Review the recovery summary before saving.",
            ));
        }
        state
            .preview
            .take()
            .map(|preview| preview.authority)
            .ok_or(HostFailure::new(
                "repair-preview",
                "Review a new recovery preview.",
            ))
    }

    pub(super) async fn dispatch(
        &self,
        store: Arc<Store>,
        method: &str,
        input: &Value,
    ) -> Result<Value, HostFailure> {
        if requires_native_validation(method) && !cfg!(debug_assertions) {
            return Err(HostFailure::new(
                "repair-native",
                "Local recovery changes are unavailable until native validation is complete.",
            ));
        }
        match method {
            "repair_status" => self.status(&store),
            "repair_progress" => Ok(self.progress().unwrap_or_else(idle_progress)),
            "cancel_store_repair" => Ok(serde_json::json!(self.cancel())),
            "authenticate_repair_password" => {
                require_damaged(&store)?;
                let request: Authentication<'_> = decode(input)?;
                let password = Password::normalize(request.password).map_err(HostFailure::from)?;
                let (_active, _) = self.start(Arc::clone(&store), Work::Authenticate)?;
                self.state
                    .lock()
                    .map_err(|_| {
                        HostFailure::new("repair-unavailable", "Local recovery is unavailable")
                    })?
                    .preview = None;
                let worker = Arc::clone(&store);
                blocking(move || {
                    worker
                        .authenticate_repair_password(&password)
                        .map_err(HostFailure::from)
                })
                .await?;
                let current = generation(&store)?;
                let mut state = self.state.lock().map_err(|_| {
                    HostFailure::new("repair-unavailable", "Local recovery is unavailable")
                })?;
                synchronize_generation(&mut state, current);
                state.authenticated = true;
                Ok(serde_json::json!({"authenticated":true}))
            }
            "check_store_integrity" => {
                let request: CheckRequest = decode(input)?;
                let kind = request.kind.unwrap_or(IntegrityKind::Full);
                let (_active, cancellation) = self.start(Arc::clone(&store), Work::Check)?;
                let worker = Arc::clone(&store);
                let report = blocking(move || {
                    let budget = if kind == IntegrityKind::Quick {
                        Duration::from_millis(100)
                    } else {
                        REPAIR_BUDGET
                    };
                    let mut report = worker
                        .check_integrity(kind, budget, &cancellation)
                        .map_err(HostFailure::from)?;
                    if let Some(receipt) = &report.receipt {
                        report.recorded_last_integrity = worker
                            .record_integrity_clean(receipt, &cancellation)
                            .map_err(HostFailure::from)?;
                    }
                    Ok(report)
                })
                .await?;
                let current = generation(&store)?;
                let mut state = self.state.lock().map_err(|_| {
                    HostFailure::new("repair-unavailable", "Local recovery is unavailable")
                })?;
                synchronize_generation(&mut state, current);
                state.diagnostic = Some(report.clone());
                encode(report)
            }
            "preview_salvage" => {
                require_damaged(&store)?;
                let (_active, cancellation) = self.start(Arc::clone(&store), Work::Preview)?;
                self.state
                    .lock()
                    .map_err(|_| {
                        HostFailure::new("repair-unavailable", "Local recovery is unavailable")
                    })?
                    .preview = None;
                let worker = Arc::clone(&store);
                let authority = blocking(move || {
                    worker
                        .preview_salvage(REPAIR_BUDGET, &cancellation)
                        .map_err(HostFailure::from)
                })
                .await?;
                let current = generation(&store)?;
                if current.store_id.as_deref()
                    != Some(authority.report().source_generation.as_str())
                {
                    return Err(HostFailure::new(
                        "repair-preview",
                        "Review a new recovery preview.",
                    ));
                }
                if authority.report().dropped_rows.len() > MAX_LOSS_ROWS {
                    return Err(HostFailure::new(
                        "repair-large",
                        "The recovery loss report is too large to review safely.",
                    ));
                }
                let mut preview = StoredPreview {
                    id: uuid::Uuid::new_v4().to_string(),
                    authority,
                    generation: current.clone(),
                    created: Instant::now(),
                    reviewed_pages: BTreeSet::new(),
                };
                let response = page(&mut preview, 0)?;
                let mut state = self.state.lock().map_err(|_| {
                    HostFailure::new("repair-unavailable", "Local recovery is unavailable")
                })?;
                synchronize_generation(&mut state, current);
                state.preview = Some(preview);
                Ok(response)
            }
            "salvage_preview_page" => {
                let request: PreviewPage<'_> = decode(input)?;
                self.preview_page(&store, request.preview_id, request.offset)
            }
            "salvage_store" => {
                let request: SavePreview<'_> = decode(input)?;
                if request.confirmation != SALVAGE_CONFIRMATION {
                    return Err(HostFailure::new(
                        "repair-confirmation",
                        format!("Type {SALVAGE_CONFIRMATION} exactly to save readable data."),
                    ));
                }
                let (_active, _) = self.start(Arc::clone(&store), Work::Salvage)?;
                let preview = self.take_preview(&store, request.preview_id)?;
                let worker = Arc::clone(&store);
                let report = blocking(move || {
                    worker
                        .save_readable_data(&preview, SALVAGE_CONFIRMATION)
                        .map_err(HostFailure::from)
                })
                .await?;
                let current = generation(&store)?;
                let mut state = self.state.lock().map_err(|_| {
                    HostFailure::new("repair-unavailable", "Local recovery is unavailable")
                })?;
                synchronize_generation(&mut state, current);
                state.authenticated = false;
                state.preview = None;
                Ok(report_summary(&report))
            }
            _ => Err(HostFailure::new(
                "repair-operation",
                "Unknown local recovery operation",
            )),
        }
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, HostFailure> + Send + 'static,
) -> Result<T, HostFailure> {
    tokio::task::spawn_blocking(work).await.map_err(|_| {
        HostFailure::new("repair-worker", "The local recovery worker did not finish")
    })?
}

fn active_salvage_progress(
    baseline_run_id: u64,
    progress: maple_agent::store::lifecycle::StoreProgress,
) -> maple_agent::store::lifecycle::StoreProgress {
    if progress.run_id != baseline_run_id {
        progress
    } else {
        // The worker has not started a new export. Its prior terminal counters
        // do not describe authentication, preview validation, or this recovery.
        maple_agent::store::lifecycle::StoreProgress {
            phase: maple_agent::store::lifecycle::OperationPhase::Preparing,
            ..Default::default()
        }
    }
}

fn idle_progress() -> Value {
    serde_json::json!({"busy":false,"phase":"idle","completed":0,"total":0,"cancellable":false})
}

fn report_summary(report: &SalvageReport) -> Value {
    serde_json::json!({
        "source_generation":report.source_generation,
        "replacement_generation":report.replacement_generation,
        "retained_tasks":report.retained_tasks,"dropped_tasks":report.dropped_tasks,
        "retained_rows":report.retained_rows,"dropped_rows_count":report.dropped_rows.len(),
        "tasks_to_rebuild":report.tasks_to_rebuild,
    })
}

fn page(preview: &mut StoredPreview, offset: usize) -> Result<Value, HostFailure> {
    let report = preview.authority.report();
    let len = report.dropped_rows.len();
    let range = page_bounds(len, offset)?;
    let end = range.end;
    let mut value = report_summary(report);
    value["preview_id"] = Value::String(preview.id.clone());
    value["dropped_rows"] = encode(&report.dropped_rows[range])?;
    value["next_offset"] = if end < len {
        serde_json::json!(end)
    } else {
        Value::Null
    };
    preview.reviewed_pages.insert(offset / PAGE_SIZE);
    Ok(value)
}

fn page_bounds(total: usize, offset: usize) -> Result<std::ops::Range<usize>, HostFailure> {
    if total > MAX_LOSS_ROWS
        || offset > total
        || !offset.is_multiple_of(PAGE_SIZE)
        || (offset == total && offset != 0)
    {
        return Err(HostFailure::new(
            "repair-page",
            "Invalid recovery loss report page.",
        ));
    }
    Ok(offset..offset.saturating_add(PAGE_SIZE).min(total))
}

fn review_is_complete(total: usize, reviewed: &BTreeSet<usize>) -> bool {
    // The complete drop counts and first detail page must have been delivered.
    // Additional details are optional; large loss reports do not require a
    // user to click through thousands of individual row locators.
    total <= MAX_LOSS_ROWS && reviewed.contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_recovery_preview_has_semantics_at_validation_source() {
        let failure = validate_token("not-a-preview").unwrap_err();
        assert_eq!(failure.code, "repair-preview");
        assert_eq!(failure.message, "Review a new recovery preview.");
    }

    #[test]
    fn recovery_method_classification_keeps_diagnostics_and_experimental_salvage_separate() {
        for method in ["repair_status", "repair_progress", "cancel_store_repair"] {
            assert!(handles(method), "{method}");
            assert!(!changes_store(method), "{method}");
            assert!(!requires_native_validation(method), "{method}");
        }
        assert!(handles("check_store_integrity"));
        assert!(changes_store("check_store_integrity"));
        assert!(!requires_native_validation("check_store_integrity"));
        for method in [
            "authenticate_repair_password",
            "preview_salvage",
            "salvage_store",
        ] {
            assert!(handles(method), "{method}");
            assert!(changes_store(method), "{method}");
            assert!(requires_native_validation(method), "{method}");
        }
        // Paging an experimental preview is also debug-only, although it is
        // a read which must not use the owner's drain path.
        assert!(handles("salvage_preview_page"));
        assert!(!changes_store("salvage_preview_page"));
        assert!(requires_native_validation("salvage_preview_page"));
        for method in ["repair_store", "damaged_copies", "delete_damaged_copy"] {
            assert!(!handles(method), "{method}");
            assert!(!changes_store(method), "{method}");
        }
        assert!(!handles("remote_repair_store"));
        assert!(retired_operation_error("repair_store").is_some());
        assert!(retired_operation_error("check_store_integrity").is_none());
    }

    #[test]
    fn salvage_progress_does_not_reuse_a_previous_export_snapshot() {
        use maple_agent::store::lifecycle::{OperationPhase, StoreProgress};
        let previous = StoreProgress {
            run_id: 7,
            phase: OperationPhase::Complete,
            completed_bytes: 4096,
            total_bytes: 4096,
            remaining_ms: Some(0),
            cancellable: false,
        };
        assert_eq!(
            active_salvage_progress(7, previous),
            StoreProgress {
                phase: OperationPhase::Preparing,
                ..Default::default()
            }
        );
        let current = StoreProgress {
            run_id: 8,
            phase: OperationPhase::Exporting,
            completed_bytes: 1024,
            total_bytes: 8192,
            remaining_ms: Some(100),
            cancellable: true,
        };
        assert_eq!(active_salvage_progress(7, current.clone()), current);
    }

    #[test]
    fn decoded_requests_cannot_supply_a_loss_report_or_extra_authority() {
        assert!(
            decode::<SavePreview<'_>>(&serde_json::json!({
                "preview_id":uuid::Uuid::new_v4().to_string(), "confirmation":SALVAGE_CONFIRMATION,
                "report":{"dropped_rows":[]}
            }))
            .is_err()
        );
        assert!(validate_token("unbounded-client-supplied-token").is_err());
        assert!(validate_token(&uuid::Uuid::new_v4().to_string()).is_ok());
    }

    #[test]
    fn replacement_generation_invalidates_every_cached_authority() {
        let first = Generation {
            store_id: Some("first".into()),
            setup_operation_id: None,
        };
        let mut state = RepairState::default();
        synchronize_generation(&mut state, first.clone());
        state.authenticated = true;
        synchronize_generation(&mut state, first);
        assert!(state.authenticated);
        synchronize_generation(
            &mut state,
            Generation {
                store_id: Some("second".into()),
                setup_operation_id: None,
            },
        );
        assert!(!state.authenticated);
        assert!(state.preview.is_none());
        assert!(state.diagnostic.is_none());
    }

    #[test]
    fn idle_progress_does_not_promise_a_percentage_or_cancellation() {
        let progress = idle_progress();
        assert_eq!(progress["completed"], 0);
        assert_eq!(progress["total"], 0);
        assert_eq!(progress["cancellable"], false);
    }

    #[test]
    fn recovery_review_pages_are_bounded_and_only_the_summary_is_required() {
        assert_eq!(page_bounds(201, 0).unwrap(), 0..100);
        assert_eq!(page_bounds(201, 100).unwrap(), 100..200);
        assert_eq!(page_bounds(201, 200).unwrap(), 200..201);
        assert!(page_bounds(201, 1).is_err());
        assert!(page_bounds(200, 200).is_err());
        assert!(page_bounds(MAX_LOSS_ROWS + 1, 0).is_err());
        assert_eq!(page_bounds(0, 0).unwrap(), 0..0);
        let mut reviewed = BTreeSet::from([2, 500]);
        assert!(!review_is_complete(201, &reviewed));
        reviewed.insert(0);
        assert!(review_is_complete(201, &reviewed));
        assert!(!review_is_complete(0, &BTreeSet::new()));
        assert!(review_is_complete(0, &BTreeSet::from([0])));
    }
}
