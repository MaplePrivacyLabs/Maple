//! Bounded route and outcome diagnostics for Transport V2.
//!
//! Every event uses the dedicated target in [`TARGET`] and a fixed set of
//! field names, so an operator can filter a narrow time window by stage, route
//! class and outcome without enabling broad `DEBUG` logging.
//!
//! The bounded vocabulary below is the whole logging surface. The request
//! target is reduced to one of the allowlisted [`Route`] classes; the query
//! string, dynamic path segments, headers, bodies, credentials, provider
//! payloads, user identifiers, session identifiers and transport request IDs
//! are never recorded. Before the envelope decrypts there is no trustworthy
//! route, so those events use [`Route::UNKNOWN`].
//!
//! There is deliberately no correlation key. The only identifiers on this path
//! (`X-Session-Id`, `X-OpenSecret-Routing-Key`, the client-chosen request ID)
//! are client-controlled, so joining an edge event to a gateway stage would
//! require trusting attacker-supplied input. See
//! `services/opensecret/docs/transport-v2-diagnostics.md`.

use std::{
    collections::HashMap,
    env,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::Instant,
};

/// Dedicated tracing target for Transport V2 diagnostics.
pub(crate) const TARGET: &str = "opensecret.transport_v2.diagnostics";

/// Allowlisted route classes. Matching only inspects a fixed prefix of the
/// path; everything after it is discarded. Unmatched targets collapse to
/// [`Route::OTHER`], so a caller cannot inject arbitrary text into `route`.
const ROUTE_CLASSES: &[(&[&str], &str)] = &[
    (&["health-check"], "health_check"),
    (&["ok"], "ok"),
    (&["attestation"], "attestation"),
    (&["key_exchange"], "key_exchange"),
    (&["protected"], "protected"),
    (&["login"], "login"),
    (&["logout"], "login"),
    (&["register"], "login"),
    (&["refresh"], "login"),
    (&["verify-email"], "login"),
    (&["password-reset"], "login"),
    (&["auth"], "auth"),
    (&["oauth"], "oauth"),
    (&["recovery"], "recovery"),
    (&["platform"], "platform"),
    (&["api", "v1"], "api_v1"),
    (&["v1", "chat", "completions"], "v1_chat_completions"),
    (&["v1", "completions"], "v1_completions"),
    (&["v1", "responses"], "v1_responses"),
    (&["v1", "conversations"], "v1_conversations"),
    (&["v1", "conversation-projects"], "v1_conversation_projects"),
    (&["v1", "instructions"], "v1_instructions"),
    (&["v1", "models"], "v1_models"),
    (&["v1", "embeddings"], "v1_embeddings"),
    (&["v1", "audio"], "v1_audio"),
    (&["v1", "systemone"], "v1_systemone"),
    (&["v1", "decisions"], "v1_decisions"),
    (&["v1", "web"], "v1_web"),
    (&["v1", "admin"], "v1_admin"),
    (&["v1", "users"], "v1_users"),
    (&["v1", "remote"], "v1_remote"),
];

const ROUTE_COUNT: usize = 2 + ROUTE_CLASSES.len();

/// A bounded route label: `unknown`, `other`, or an allowlisted class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Route(usize);

impl Route {
    pub(crate) const UNKNOWN: Self = Self(0);
    pub(crate) const OTHER: Self = Self(1);

    pub(crate) fn name(self) -> &'static str {
        if self.0 == Self::UNKNOWN.0 {
            "unknown"
        } else if self.0 == Self::OTHER.0 {
            "other"
        } else {
            ROUTE_CLASSES[self.0 - 2].1
        }
    }

    const fn index(self) -> usize {
        self.0
    }
}

/// Reduce a validated logical target to an allowlisted route class.
pub(crate) fn classify_target(target: &str) -> Route {
    let path = target.split('?').next().unwrap_or_default();
    let mut segments = ["", "", ""];
    let mut count = 0;
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        if count == segments.len() {
            break;
        }
        segments[count] = segment;
        count += 1;
    }
    for (index, (prefix, _)) in ROUTE_CLASSES.iter().enumerate() {
        if prefix.len() <= count
            && prefix
                .iter()
                .zip(segments.iter())
                .all(|(expected, actual)| expected == actual)
        {
            return Route(index + 2);
        }
    }
    Route::OTHER
}

/// Allowlisted method label; unknown tokens collapse to `other`.
pub(crate) fn classify_method(method: &str) -> &'static str {
    match method {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        _ => "other",
    }
}

const STAGE_COUNT: usize = 6;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Stage {
    Admission,
    EnvelopeDecode,
    InnerDispatch,
    LogicalResponse,
    EncryptedResponse,
    StreamTerminal,
}

impl Stage {
    pub(crate) const ALL: [Stage; STAGE_COUNT] = [
        Stage::Admission,
        Stage::EnvelopeDecode,
        Stage::InnerDispatch,
        Stage::LogicalResponse,
        Stage::EncryptedResponse,
        Stage::StreamTerminal,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Stage::Admission => "admission",
            Stage::EnvelopeDecode => "envelope_decode",
            Stage::InnerDispatch => "inner_dispatch",
            Stage::LogicalResponse => "logical_response",
            Stage::EncryptedResponse => "encrypted_response",
            Stage::StreamTerminal => "stream_terminal",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

const OUTCOME_COUNT: usize = 12;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Outcome {
    Admitted,
    Rejected,
    Decoded,
    Dispatched,
    Success,
    ClientError,
    ServerError,
    Started,
    StartFailed,
    Ended,
    Failed,
    Disconnected,
}

impl Outcome {
    pub(crate) const ALL: [Outcome; OUTCOME_COUNT] = [
        Outcome::Admitted,
        Outcome::Rejected,
        Outcome::Decoded,
        Outcome::Dispatched,
        Outcome::Success,
        Outcome::ClientError,
        Outcome::ServerError,
        Outcome::Started,
        Outcome::StartFailed,
        Outcome::Ended,
        Outcome::Failed,
        Outcome::Disconnected,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Outcome::Admitted => "admitted",
            Outcome::Rejected => "rejected",
            Outcome::Decoded => "decoded",
            Outcome::Dispatched => "dispatched",
            Outcome::Success => "success",
            Outcome::ClientError => "client_error",
            Outcome::ServerError => "server_error",
            Outcome::Started => "started",
            Outcome::StartFailed => "start_failed",
            Outcome::Ended => "ended",
            Outcome::Failed => "failed",
            Outcome::Disconnected => "disconnected",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }

    /// Exceptional outcomes are always candidates for a log line, bounded by
    /// the volume budget. Everything else is a per-request success event and is
    /// sampled.
    const fn is_exceptional(self) -> bool {
        matches!(
            self,
            Outcome::Rejected
                | Outcome::ClientError
                | Outcome::ServerError
                | Outcome::StartFailed
                | Outcome::Failed
                | Outcome::Disconnected
        )
    }

    const fn level(self) -> tracing::Level {
        match self {
            Outcome::ServerError
            | Outcome::StartFailed
            | Outcome::Failed
            | Outcome::Disconnected => tracing::Level::WARN,
            _ => tracing::Level::INFO,
        }
    }
}

const CELLS: usize = STAGE_COUNT * ROUTE_COUNT * OUTCOME_COUNT;

fn cell(stage: Stage, route: Route, outcome: Outcome) -> usize {
    (stage.index() * ROUTE_COUNT + route.index()) * OUTCOME_COUNT + outcome.index()
}

struct Counters {
    cells: Box<[AtomicU64]>,
}

impl Counters {
    fn new() -> Self {
        Self {
            cells: (0..CELLS).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    fn add(&self, index: usize) {
        self.cells[index].fetch_add(1, Ordering::Relaxed);
    }

    fn take(&self, index: usize) -> u64 {
        self.cells[index].swap(0, Ordering::Relaxed)
    }

    fn get(&self, index: usize) -> u64 {
        self.cells[index].load(Ordering::Relaxed)
    }
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

#[derive(Clone, Copy, Debug)]
struct Volume {
    sample_permille: u64,
    exceptional_burst: f64,
    exceptional_per_minute: f64,
}

impl Volume {
    fn from_env() -> Self {
        let sample_permille = env_u64("TRANSPORT_V2_DIAGNOSTICS_SAMPLE_PERMILLE", 10).min(1000);
        let exceptional_burst =
            env_u64("TRANSPORT_V2_DIAGNOSTICS_EXCEPTIONAL_BURST", 20).min(10_000) as f64;
        let exceptional_per_minute =
            env_u64("TRANSPORT_V2_DIAGNOSTICS_EXCEPTIONAL_PER_MINUTE", 60).min(60_000) as f64;
        Self {
            sample_permille,
            exceptional_burst,
            exceptional_per_minute,
        }
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

/// Per-request diagnostic context once a route is known.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RequestDiagnostics {
    route: Route,
    method: &'static str,
    inner_status: Option<u16>,
}

impl RequestDiagnostics {
    pub(crate) const fn unknown() -> Self {
        Self {
            route: Route::UNKNOWN,
            method: "other",
            inner_status: None,
        }
    }

    pub(crate) fn from_envelope(method: &str, target: &str) -> Self {
        Self {
            route: classify_target(target),
            method: classify_method(method),
            inner_status: None,
        }
    }

    pub(crate) const fn with_inner_status(mut self, status: u16) -> Self {
        self.inner_status = Some(status);
        self
    }

    pub(crate) const fn route(self) -> Route {
        self.route
    }

    pub(crate) const fn method(self) -> &'static str {
        self.method
    }

    pub(crate) const fn inner_status(self) -> Option<u16> {
        self.inner_status
    }
}

/// Bounded, sampling diagnostics for one gateway instance.
pub(crate) struct Diagnostics {
    volume: Volume,
    sampled: AtomicU64,
    window: Counters,
    total: Counters,
    suppressed: Counters,
    buckets: Mutex<HashMap<usize, Bucket>>,
}

impl Diagnostics {
    pub(crate) fn from_env() -> Self {
        Self::with_volume(Volume::from_env())
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::with_volume(Volume {
            sample_permille: 1000,
            exceptional_burst: 1000.0,
            exceptional_per_minute: 1000.0,
        })
    }

    fn with_volume(volume: Volume) -> Self {
        Self {
            volume,
            sampled: AtomicU64::new(0),
            window: Counters::new(),
            total: Counters::new(),
            suppressed: Counters::new(),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Record one bounded stage outcome.
    ///
    /// Aggregate counts advance for every event. A per-request line is written
    /// only when the outcome survives its volume policy: exceptional outcomes
    /// pass a per-key token bucket, success outcomes are sampled. Suppressed
    /// events stay visible in the aggregate counts.
    pub(crate) fn record(
        &self,
        stage: Stage,
        request: RequestDiagnostics,
        outcome: Outcome,
        reason: Option<&'static str>,
        outer_status: Option<u16>,
    ) {
        let index = cell(stage, request.route, outcome);
        self.window.add(index);
        self.total.add(index);

        let emit = if outcome.is_exceptional() {
            self.allow_exceptional(index)
        } else {
            self.should_sample()
        };
        if !emit {
            self.suppressed.add(index);
            return;
        }

        emit_event(
            outcome.level(),
            stage,
            request,
            outcome,
            reason,
            outer_status,
        );
    }

    fn should_sample(&self) -> bool {
        if self.volume.sample_permille == 0 {
            return false;
        }
        let position = self.sampled.fetch_add(1, Ordering::Relaxed);
        position % 1000 < self.volume.sample_permille
    }

    fn allow_exceptional(&self, index: usize) -> bool {
        if self.volume.exceptional_burst < 1.0 {
            return false;
        }
        let refill_per_second = self.volume.exceptional_per_minute / 60.0;
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        let bucket = buckets.entry(index).or_insert(Bucket {
            tokens: self.volume.exceptional_burst,
            last: now,
        });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens =
            (bucket.tokens + elapsed * refill_per_second).min(self.volume.exceptional_burst);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Emit the bounded window of aggregate counts, one line per non-zero
    /// `(stage, route, outcome)` cell. Cardinality is fixed by the enums, so the
    /// worst case is `STAGE_COUNT * ROUTE_COUNT * OUTCOME_COUNT` lines per
    /// window and nothing is dropped.
    pub(crate) fn emit_aggregate(&self) {
        for stage in Stage::ALL {
            for route_index in 0..ROUTE_COUNT {
                let route = Route(route_index);
                for outcome in Outcome::ALL {
                    let index = cell(stage, route, outcome);
                    let window = self.window.take(index);
                    let suppressed = self.suppressed.take(index);
                    if window == 0 && suppressed == 0 {
                        continue;
                    }
                    tracing::info!(
                        target: TARGET,
                        kind = "aggregate",
                        stage = stage.as_str(),
                        route = route.name(),
                        outcome = outcome.as_str(),
                        window,
                        total = self.total.get(index),
                        suppressed,
                        "transport-v2 diagnostics aggregate"
                    );
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn count(&self, stage: Stage, route: Route, outcome: Outcome) -> u64 {
        self.total.get(cell(stage, route, outcome))
    }

    #[cfg(test)]
    pub(crate) fn suppressed(&self, stage: Stage, route: Route, outcome: Outcome) -> u64 {
        self.suppressed.get(cell(stage, route, outcome))
    }
}

fn emit_event(
    level: tracing::Level,
    stage: Stage,
    request: RequestDiagnostics,
    outcome: Outcome,
    reason: Option<&'static str>,
    outer_status: Option<u16>,
) {
    if level == tracing::Level::WARN {
        tracing::warn!(
            target: TARGET,
            kind = "request",
            stage = stage.as_str(),
            route = request.route.name(),
            method = request.method,
            outcome = outcome.as_str(),
            reason = reason,
            inner_status = request.inner_status,
            outer_status = outer_status,
            "transport-v2 request diagnostic"
        );
    } else {
        tracing::info!(
            target: TARGET,
            kind = "request",
            stage = stage.as_str(),
            route = request.route.name(),
            method = request.method,
            outcome = outcome.as_str(),
            reason = reason,
            inner_status = request.inner_status,
            outer_status = outer_status,
            "transport-v2 request diagnostic"
        );
    }
}

/// Emits a terminal stream outcome exactly once. If the response body stream is
/// dropped before its terminal record, the drop reports a client disconnect
/// instead of silently losing the request.
pub(crate) struct TerminalGuard {
    diagnostics: std::sync::Arc<Diagnostics>,
    request: RequestDiagnostics,
    finished: bool,
}

impl TerminalGuard {
    pub(crate) fn new(
        diagnostics: std::sync::Arc<Diagnostics>,
        request: RequestDiagnostics,
    ) -> Self {
        Self {
            diagnostics,
            request,
            finished: false,
        }
    }

    pub(crate) fn finish(&mut self, outcome: Outcome, reason: Option<&'static str>) {
        self.diagnostics
            .record(Stage::StreamTerminal, self.request, outcome, reason, None);
        self.finished = true;
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.diagnostics.record(
                Stage::StreamTerminal,
                self.request,
                Outcome::Disconnected,
                Some("response_stream_dropped"),
                None,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::{layer::Context, prelude::*, Layer};

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

    impl<S: tracing::Subscriber> Layer<S> for Captured {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            if event.metadata().target() != TARGET {
                return;
            }
            let mut visitor = Fields::default();
            event.record(&mut visitor);
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(visitor.values);
        }
    }

    #[derive(Default)]
    struct Fields {
        values: BTreeMap<String, String>,
    }

    impl Visit for Fields {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.values
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            self.values
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_i64(&mut self, field: &Field, value: i64) {
            self.values
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.values
                .insert(field.name().to_string(), format!("{value:?}"));
        }
    }

    fn capture() -> (Captured, tracing::subscriber::DefaultGuard) {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        (captured, guard)
    }

    fn volume(sample_permille: u64, burst: f64, per_minute: f64) -> Volume {
        Volume {
            sample_permille,
            exceptional_burst: burst,
            exceptional_per_minute: per_minute,
        }
    }

    #[test]
    fn classification_drops_query_and_dynamic_segments() {
        let route = classify_target("/v1/conversations/secret-id/items/another?token=leak");
        assert_eq!(route.name(), "v1_conversations");
        let request = RequestDiagnostics::from_envelope("POST", "/protected/user/deadbeef");
        assert_eq!(request.route().name(), "protected");
        assert_eq!(request.method(), "POST");
    }

    #[test]
    fn unknown_targets_are_other_and_methods_are_bounded() {
        assert_eq!(classify_target("/nonsense/path").name(), "other");
        assert_eq!(classify_target("/v1/chat").name(), "other");
        assert_eq!(classify_method("TRACE"), "other");
        assert_eq!(classify_method("get"), "other");
        assert_eq!(Route::UNKNOWN.name(), "unknown");
    }

    #[test]
    fn malformed_and_unauthenticated_events_keep_unknown_route() {
        let (captured, _guard) = capture();
        let diagnostics = Diagnostics::with_volume(volume(1000, 20.0, 60.0));
        diagnostics.record(
            Stage::Admission,
            RequestDiagnostics::unknown(),
            Outcome::Rejected,
            Some("request_decryption_failed"),
            Some(400),
        );
        let events = captured.0.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["route"], "unknown");
        assert_eq!(events[0]["stage"], "admission");
        assert_eq!(events[0]["outer_status"], "400");
        assert!(!events[0].contains_key("inner_status"));
    }

    #[test]
    fn dynamic_segments_never_reach_event_fields() {
        let (captured, _guard) = capture();
        let diagnostics = Diagnostics::with_volume(volume(1000, 20.0, 60.0));
        let request =
            RequestDiagnostics::from_envelope("POST", "/v1/conversations/private-id?q=secret");
        diagnostics.record(
            Stage::InnerDispatch,
            request,
            Outcome::Dispatched,
            None,
            None,
        );
        let events = captured.0.lock().unwrap();
        let serialized = format!("{:?}", events[0]);
        assert!(!serialized.contains("private-id"));
        assert!(!serialized.contains("secret"));
        assert_eq!(events[0]["route"], "v1_conversations");
        assert_eq!(events[0]["method"], "POST");
    }

    #[test]
    fn logical_and_outer_failures_are_distinct() {
        let (captured, _guard) = capture();
        let diagnostics = Diagnostics::with_volume(volume(1000, 20.0, 60.0));
        let request = RequestDiagnostics::from_envelope("GET", "/v1/models").with_inner_status(502);
        diagnostics.record(
            Stage::LogicalResponse,
            request,
            Outcome::ServerError,
            None,
            None,
        );
        diagnostics.record(
            Stage::Admission,
            RequestDiagnostics::unknown(),
            Outcome::Rejected,
            Some("unavailable"),
            Some(502),
        );
        let events = captured.0.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["stage"], "logical_response");
        assert_eq!(events[0]["inner_status"], "502");
        assert!(!events[0].contains_key("outer_status"));
        assert_eq!(events[1]["stage"], "admission");
        assert_eq!(events[1]["outer_status"], "502");
        assert!(!events[1].contains_key("inner_status"));
    }

    #[test]
    fn success_events_are_sampled_and_suppression_is_counted() {
        let (captured, _guard) = capture();
        let diagnostics = Diagnostics::with_volume(volume(0, 20.0, 60.0));
        let request = RequestDiagnostics::from_envelope("GET", "/v1/models");
        for _ in 0..5 {
            diagnostics.record(
                Stage::LogicalResponse,
                request,
                Outcome::Success,
                None,
                None,
            );
        }
        assert!(captured.0.lock().unwrap().is_empty());
        let route = classify_target("/v1/models");
        assert_eq!(
            diagnostics.suppressed(Stage::LogicalResponse, route, Outcome::Success),
            5
        );
        assert_eq!(
            diagnostics.count(Stage::LogicalResponse, route, Outcome::Success),
            5
        );
    }

    #[test]
    fn exceptional_events_pass_a_token_bucket_and_count_suppression() {
        let (captured, _guard) = capture();
        let diagnostics = Diagnostics::with_volume(volume(1000, 2.0, 0.0));
        let request = RequestDiagnostics::unknown();
        for _ in 0..5 {
            diagnostics.record(
                Stage::Admission,
                request,
                Outcome::Rejected,
                Some("bad_request"),
                Some(400),
            );
        }
        assert_eq!(captured.0.lock().unwrap().len(), 2);
        assert_eq!(
            diagnostics.suppressed(Stage::Admission, Route::UNKNOWN, Outcome::Rejected),
            3
        );
    }

    #[test]
    fn aggregate_emits_counts_and_keeps_totals() {
        let (captured, _guard) = capture();
        let diagnostics = Diagnostics::with_volume(volume(0, 20.0, 60.0));
        let request = RequestDiagnostics::from_envelope("GET", "/v1/models");
        diagnostics.record(
            Stage::LogicalResponse,
            request,
            Outcome::Success,
            None,
            None,
        );
        diagnostics.emit_aggregate();
        let events = captured.0.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["kind"], "aggregate");
        assert_eq!(events[0]["window"], "1");
        assert_eq!(events[0]["total"], "1");
        assert_eq!(events[0]["suppressed"], "1");
    }

    #[test]
    fn terminal_guard_reports_disconnect_only_once() {
        let (captured, _guard) = capture();
        let diagnostics = Arc::new(Diagnostics::with_volume(volume(1000, 20.0, 60.0)));
        let request = RequestDiagnostics::from_envelope("GET", "/v1/models").with_inner_status(200);
        {
            let mut guard = TerminalGuard::new(Arc::clone(&diagnostics), request);
            guard.finish(Outcome::Ended, None);
        }
        assert_eq!(captured.0.lock().unwrap().len(), 1);
        drop(TerminalGuard::new(diagnostics, request));
        assert_eq!(captured.0.lock().unwrap().len(), 2);
        assert_eq!(captured.0.lock().unwrap()[1]["outcome"], "disconnected");
    }
}
