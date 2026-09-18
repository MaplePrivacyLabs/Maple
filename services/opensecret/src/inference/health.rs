//! Enclave-local health classification for inference routes.
//!
//! The state has a fixed registry-derived topology and is deliberately local to
//! one enclave. Active routing reads coherent snapshots, and expired circuits
//! are protected by fenced half-open probe leases so concurrent requests cannot
//! create a probe herd.

use super::{AttemptFailureKind, AttemptTerminal, RouteKey};
use crate::provider_registry::{
    FailoverPolicy, ProviderId, ProviderRegistry, RateLimitScope, PROVIDER_REGISTRY,
};
use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub(crate) const SHADOW_HEALTH_POLICY_VERSION: &str = "routing-v2-health-shadow-v1";

// The identifier retains its original shadow-era name for telemetry continuity;
// these thresholds now drive active circuit and capacity decisions.
const ROUTE_FAILURE_WINDOW: Duration = Duration::from_secs(60);
const ROUTE_FAILURES_TO_OPEN: u8 = 3;
const ROUTE_OPEN_COOLDOWN: Duration = Duration::from_secs(30);
// A missing or zero upstream hint must not turn an active circuit into a hot
// retry loop. Thirty seconds matches the route-health cooldown while remaining
// bounded well below the one-hour maximum accepted from providers.
pub(crate) const MIN_CAPACITY_COOLDOWN: Duration = Duration::from_secs(30);
const MAX_CAPACITY_COOLDOWN: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShadowObservationMode {
    Update,
    TelemetryOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum CapacityPoolKey {
    ProviderModel(RouteKey),
    ProviderAccount(ProviderId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShadowDisposition {
    Healthy,
    Watch {
        consecutive_failures: u8,
    },
    WouldOpen {
        remaining: Duration,
    },
    WouldProbe,
    /// A half-open provider attempt is live. `retry_after` is only a bounded
    /// recovery hint for callers; the lease itself has no timer and remains
    /// live until its attempt terminalizes or its guard is dropped.
    ProbeInFlight {
        retry_after: Duration,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShadowRouteSnapshot {
    pub(crate) route_health: ShadowDisposition,
    pub(crate) deployment_capacity: ShadowDisposition,
    pub(crate) rate_limit_capacity: ShadowDisposition,
    pub(crate) effective: ShadowDisposition,
}

impl ShadowRouteSnapshot {
    pub(crate) fn for_failover(self, policy: FailoverPolicy) -> ShadowDisposition {
        match policy {
            FailoverPolicy::AllGates => self.effective,
            FailoverPolicy::CapacityGates => {
                strongest_disposition([self.deployment_capacity, self.rate_limit_capacity])
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShadowSignal {
    Completed,
    CapacityRejected {
        status: u16,
    },
    RouteFailure {
        kind: AttemptFailureKind,
        status: Option<u16>,
    },
    Neutral {
        kind: AttemptFailureKind,
        status: Option<u16>,
    },
    UnknownRoute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShadowObservationReport {
    pub(crate) policy_version: &'static str,
    pub(crate) mode: ShadowObservationMode,
    pub(crate) route: RouteKey,
    pub(crate) signal: ShadowSignal,
    pub(crate) capacity_pool: Option<CapacityPoolKey>,
    pub(crate) snapshot: Option<ShadowRouteSnapshot>,
    pub(crate) mutated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeRejectionReason {
    CircuitOpen,
    ProbeInFlight,
    UnknownRoute,
}

/// Result of atomically checking every health gate for a selected route.
///
/// `Ready(None)` means no gate needs a probe. `Ready(Some(_))` owns every
/// expired-open gate that must be probed as one composite decision. Rejections
/// are replay-safe because this check happens before a provider attempt starts.
#[derive(Debug)]
pub(crate) enum ProbeClaimResult {
    Ready(Option<ProbeLease>),
    Rejected {
        reason: ProbeRejectionReason,
        retry_after: Duration,
    },
}

#[derive(Debug, Clone, Copy)]
struct ShadowHealthPolicy {
    route_failure_window: Duration,
    route_failures_to_open: u8,
    route_open_cooldown: Duration,
    minimum_capacity_cooldown: Duration,
    max_capacity_cooldown: Duration,
}

impl Default for ShadowHealthPolicy {
    fn default() -> Self {
        Self {
            route_failure_window: ROUTE_FAILURE_WINDOW,
            route_failures_to_open: ROUTE_FAILURES_TO_OPEN,
            route_open_cooldown: ROUTE_OPEN_COOLDOWN,
            minimum_capacity_cooldown: MIN_CAPACITY_COOLDOWN,
            max_capacity_cooldown: MAX_CAPACITY_COOLDOWN,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveProbe {
    lease_id: ProbeLeaseId,
    generation_at_claim: u64,
}

#[derive(Debug, Default)]
struct RouteHealthState {
    failure_times: VecDeque<Instant>,
    open_until: Option<Instant>,
    generation: u64,
    active_probe: Option<ActiveProbe>,
}

#[derive(Debug, Default)]
struct CapacityState {
    open_until: Option<Instant>,
    generation: u64,
    active_probe: Option<ActiveProbe>,
}

#[derive(Debug)]
struct ShadowHealthInner {
    route_health: HashMap<RouteKey, RouteHealthState>,
    capacity: HashMap<CapacityPoolKey, CapacityState>,
    next_probe_lease_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ProbeLeaseId(u64);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ProbeGateKey {
    RouteHealth(RouteKey),
    Capacity(CapacityPoolKey),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProbeGateClaim {
    gate: ProbeGateKey,
    generation_at_claim: u64,
}

/// Non-cloneable ownership of a composite half-open probe.
///
/// Dropping an unresolved lease releases only matching fenced gates. It never
/// heals a circuit, and a stale lease cannot release a newer probe.
pub(crate) struct ProbeLease {
    inner: Arc<Mutex<ShadowHealthInner>>,
    route: RouteKey,
    lease_id: ProbeLeaseId,
    claims: Option<Box<[ProbeGateClaim]>>,
}

impl std::fmt::Debug for ProbeLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProbeLease")
            .field("route", &self.route)
            .field("lease_id", &self.lease_id)
            .field(
                "claim_count",
                &self.claims.as_ref().map_or(0, |claims| claims.len()),
            )
            .finish()
    }
}

impl ProbeLease {
    fn take_matching_claims(
        &mut self,
        owner: &Arc<Mutex<ShadowHealthInner>>,
        route: &RouteKey,
    ) -> Option<(ProbeLeaseId, Box<[ProbeGateClaim]>)> {
        if !Arc::ptr_eq(&self.inner, owner) || &self.route != route {
            return None;
        }
        self.claims.take().map(|claims| (self.lease_id, claims))
    }

    #[cfg(test)]
    fn claim_count(&self) -> usize {
        self.claims.as_ref().map_or(0, |claims| claims.len())
    }
}

impl Drop for ProbeLease {
    fn drop(&mut self) {
        let Some(claims) = self.claims.take() else {
            return;
        };
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        release_probe_claims(&mut inner, self.lease_id, &claims);
    }
}

/// Fixed-topology, enclave-local health state.
///
/// Every key is pre-populated from the static provider registry. Observations
/// for unknown routes are reported but can never grow the maps.
#[derive(Debug)]
pub(crate) struct ShadowHealthState {
    policy: ShadowHealthPolicy,
    rate_limit_pools: HashMap<RouteKey, CapacityPoolKey>,
    failover_policies: HashMap<RouteKey, FailoverPolicy>,
    inner: Arc<Mutex<ShadowHealthInner>>,
    #[cfg(test)]
    observation_count: AtomicUsize,
}

impl Default for ShadowHealthState {
    fn default() -> Self {
        Self::new(&PROVIDER_REGISTRY)
    }
}

impl ShadowHealthState {
    pub(crate) fn new(registry: &'static ProviderRegistry) -> Self {
        Self::new_with_policy(registry, ShadowHealthPolicy::default())
    }

    fn new_with_policy(registry: &'static ProviderRegistry, policy: ShadowHealthPolicy) -> Self {
        let mut route_health = HashMap::new();
        let mut capacity = HashMap::new();
        let mut rate_limit_pools = HashMap::new();
        let mut failover_policies = HashMap::new();

        for model in registry.completion_models() {
            for route in model.routes {
                let route_key = RouteKey {
                    provider: route.provider,
                    provider_model_id: route.provider_model_id.to_string(),
                };
                let deployment_pool = CapacityPoolKey::ProviderModel(route_key.clone());
                let rate_limit_pool = match route.rate_limit_scope {
                    RateLimitScope::ProviderModel => deployment_pool.clone(),
                    RateLimitScope::ProviderAccount => {
                        CapacityPoolKey::ProviderAccount(route.provider)
                    }
                };

                route_health.entry(route_key.clone()).or_default();
                capacity.entry(deployment_pool).or_default();
                capacity.entry(rate_limit_pool.clone()).or_default();
                rate_limit_pools
                    .entry(route_key.clone())
                    .or_insert(rate_limit_pool);
                failover_policies.entry(route_key).or_insert(model.failover);
            }
        }

        Self {
            policy,
            rate_limit_pools,
            failover_policies,
            inner: Arc::new(Mutex::new(ShadowHealthInner {
                route_health,
                capacity,
                next_probe_lease_id: 0,
            })),
            #[cfg(test)]
            observation_count: AtomicUsize::new(0),
        }
    }

    pub(crate) fn observe_terminal(
        &self,
        terminal: &AttemptTerminal,
        mode: ShadowObservationMode,
    ) -> ShadowObservationReport {
        let mut inner = self.lock();
        let now = Instant::now();
        self.observe_terminal_locked(&mut inner, terminal, mode, None, now)
    }

    /// Records the terminal result of an attempt that may own a half-open
    /// probe. The terminal health mutation and matching lease resolution occur
    /// while holding one state lock.
    ///
    /// Telemetry-only recovered sub-attempts must use [`Self::observe_terminal`]
    /// and leave the lease on the final attempt guard.
    pub(crate) fn observe_terminal_with_probe(
        &self,
        terminal: &AttemptTerminal,
        mode: ShadowObservationMode,
        mut probe: Option<ProbeLease>,
    ) -> ShadowObservationReport {
        debug_assert!(
            mode == ShadowObservationMode::Update || probe.is_none(),
            "telemetry-only observations must not own a probe lease"
        );
        let mut inner = self.lock();
        let now = Instant::now();
        self.observe_terminal_locked(&mut inner, terminal, mode, probe.as_mut(), now)
    }

    pub(crate) fn snapshot(&self, route: &RouteKey) -> Option<ShadowRouteSnapshot> {
        let inner = self.lock();
        self.snapshot_locked(&inner, route, Instant::now())
    }

    /// Returns a point-in-time snapshot for every requested route while holding
    /// the state lock once. Active selection must not combine observations from
    /// different instants when deciding whether candidate routes are open.
    pub(crate) fn snapshot_routes(&self, routes: &[RouteKey]) -> Option<Vec<ShadowRouteSnapshot>> {
        let inner = self.lock();
        let now = Instant::now();
        routes
            .iter()
            .map(|route| self.snapshot_locked(&inner, route, now))
            .collect()
    }

    /// Point-in-time snapshots for several route groups under one lock. Each
    /// group resolves independently, so a route unknown to the health state
    /// voids only its own group rather than every candidate.
    pub(crate) fn snapshot_route_groups(
        &self,
        groups: &[Vec<RouteKey>],
    ) -> Vec<Option<Vec<ShadowRouteSnapshot>>> {
        let inner = self.lock();
        let now = Instant::now();
        groups
            .iter()
            .map(|routes| {
                routes
                    .iter()
                    .map(|route| self.snapshot_locked(&inner, route, now))
                    .collect()
            })
            .collect()
    }

    /// Atomically checks the route-health gate, deployment-capacity gate, and
    /// registry-declared rate-limit gate for one already-selected route.
    pub(crate) fn try_claim_probe(&self, route: &RouteKey) -> ProbeClaimResult {
        let mut inner = self.lock();
        self.try_claim_probe_locked(&mut inner, route, Instant::now())
    }

    fn try_claim_probe_locked(
        &self,
        inner: &mut ShadowHealthInner,
        route: &RouteKey,
        now: Instant,
    ) -> ProbeClaimResult {
        let Some(rate_limit_pool) = self.rate_limit_pools.get(route) else {
            return ProbeClaimResult::Rejected {
                reason: ProbeRejectionReason::UnknownRoute,
                retry_after: self.policy.minimum_capacity_cooldown,
            };
        };

        let deployment_pool = CapacityPoolKey::ProviderModel(route.clone());
        let failover = self
            .failover_policies
            .get(route)
            .copied()
            .unwrap_or(FailoverPolicy::AllGates);
        let mut gates = Vec::with_capacity(3);
        if failover == FailoverPolicy::AllGates {
            gates.push(ProbeGateKey::RouteHealth(route.clone()));
        }
        gates.push(ProbeGateKey::Capacity(deployment_pool.clone()));
        if rate_limit_pool != &deployment_pool {
            gates.push(ProbeGateKey::Capacity(rate_limit_pool.clone()));
        }

        let mut expired = Vec::with_capacity(gates.len());
        let mut open_remaining = None;
        let mut probe_in_flight = false;
        for gate in &gates {
            let Some(status) = probe_gate_status(inner, gate, now) else {
                return ProbeClaimResult::Rejected {
                    reason: ProbeRejectionReason::UnknownRoute,
                    retry_after: self.policy.minimum_capacity_cooldown,
                };
            };
            match status {
                ProbeGateStatus::Healthy => {}
                ProbeGateStatus::ExpiredOpen => expired.push(gate.clone()),
                ProbeGateStatus::StillOpen { remaining } => {
                    open_remaining = Some(
                        open_remaining
                            .map_or(remaining, |current: Duration| current.max(remaining)),
                    );
                }
                ProbeGateStatus::ProbeInFlight => probe_in_flight = true,
            }
        }

        if probe_in_flight {
            return ProbeClaimResult::Rejected {
                reason: ProbeRejectionReason::ProbeInFlight,
                retry_after: open_remaining
                    .unwrap_or(self.policy.minimum_capacity_cooldown)
                    .max(self.policy.minimum_capacity_cooldown),
            };
        }
        if let Some(retry_after) = open_remaining {
            return ProbeClaimResult::Rejected {
                reason: ProbeRejectionReason::CircuitOpen,
                retry_after,
            };
        }
        if expired.is_empty() {
            return ProbeClaimResult::Ready(None);
        }

        let lease_id = next_probe_lease_id(inner);
        let mut claims = Vec::with_capacity(expired.len());
        for gate in expired {
            let generation_at_claim = claim_probe_gate(inner, &gate, lease_id)
                .expect("validated probe gate must remain claimable under the state lock");
            claims.push(ProbeGateClaim {
                gate,
                generation_at_claim,
            });
        }

        ProbeClaimResult::Ready(Some(ProbeLease {
            inner: Arc::clone(&self.inner),
            route: route.clone(),
            lease_id,
            claims: Some(claims.into_boxed_slice()),
        }))
    }

    fn observe_terminal_locked(
        &self,
        inner: &mut ShadowHealthInner,
        terminal: &AttemptTerminal,
        mode: ShadowObservationMode,
        probe: Option<&mut ProbeLease>,
        now: Instant,
    ) -> ShadowObservationReport {
        #[cfg(test)]
        self.observation_count.fetch_add(1, Ordering::Relaxed);

        let route = terminal.attempt().route.route_key();
        let Some(rate_limit_pool) = self.rate_limit_pools.get(&route).cloned() else {
            return ShadowObservationReport {
                policy_version: SHADOW_HEALTH_POLICY_VERSION,
                mode,
                route,
                signal: ShadowSignal::UnknownRoute,
                capacity_pool: None,
                snapshot: None,
                mutated: false,
            };
        };

        let mut signal = ShadowSignal::Completed;
        let mut capacity_pool = None;
        let mut mutation = Mutation::Completed;

        if let AttemptTerminal::Failed { failure, .. } = terminal {
            let status = failure.status;
            if failure.kind == AttemptFailureKind::CapacityRejected {
                match status {
                    Some(429) => {
                        signal = ShadowSignal::CapacityRejected { status: 429 };
                        capacity_pool = Some(rate_limit_pool.clone());
                        mutation = Mutation::Capacity {
                            pool: rate_limit_pool.clone(),
                            retry_after: failure.retry_after,
                        };
                    }
                    Some(status @ (503 | 529)) => {
                        let pool = CapacityPoolKey::ProviderModel(route.clone());
                        signal = ShadowSignal::CapacityRejected { status };
                        capacity_pool = Some(pool.clone());
                        mutation = Mutation::Capacity {
                            pool,
                            retry_after: failure.retry_after,
                        };
                    }
                    _ => {
                        signal = ShadowSignal::Neutral {
                            kind: failure.kind,
                            status,
                        };
                        mutation = Mutation::None;
                    }
                }
            } else if failure.kind == AttemptFailureKind::HttpStatus {
                if status.is_some_and(|status| (500..=599).contains(&status)) {
                    signal = ShadowSignal::RouteFailure {
                        kind: failure.kind,
                        status,
                    };
                    mutation = Mutation::RouteFailure;
                } else {
                    signal = ShadowSignal::Neutral {
                        kind: failure.kind,
                        status,
                    };
                    mutation = Mutation::None;
                }
            } else if is_route_health_failure(failure.kind) {
                signal = ShadowSignal::RouteFailure {
                    kind: failure.kind,
                    status,
                };
                mutation = Mutation::RouteFailure;
            } else {
                signal = ShadowSignal::Neutral {
                    kind: failure.kind,
                    status,
                };
                mutation = Mutation::None;
            }
        }

        let mut mutated = mode == ShadowObservationMode::Update && mutation != Mutation::None;
        if mode == ShadowObservationMode::Update {
            let probe_claims =
                probe.and_then(|probe| probe.take_matching_claims(&self.inner, &route));
            if let Some((lease_id, claims)) = probe_claims {
                match mutation.clone() {
                    Mutation::Completed => {
                        resolve_probe_success(inner, lease_id, &claims);
                        if let Some(state) = inner.route_health.get_mut(&route) {
                            apply_route_success(state, now);
                        }
                    }
                    Mutation::None => {
                        release_probe_claims(inner, lease_id, &claims);
                    }
                    mutation => {
                        self.apply_mutation(inner, &route, mutation, now);
                        release_probe_claims(inner, lease_id, &claims);
                    }
                }
                mutated = true;
            } else if mutated {
                self.apply_mutation(inner, &route, mutation, now);
            }
        }

        ShadowObservationReport {
            policy_version: SHADOW_HEALTH_POLICY_VERSION,
            mode,
            route: route.clone(),
            signal,
            capacity_pool,
            snapshot: self.snapshot_locked(inner, &route, now),
            mutated,
        }
    }

    fn apply_mutation(
        &self,
        inner: &mut ShadowHealthInner,
        route: &RouteKey,
        mutation: Mutation,
        now: Instant,
    ) {
        match mutation {
            Mutation::Completed => {
                if let Some(state) = inner.route_health.get_mut(route) {
                    apply_route_success(state, now);
                }
                // Ordinary success carries no authority to heal an opened
                // capacity gate; only a matching probe lease can do that.
            }
            Mutation::Capacity { pool, retry_after } => {
                if let Some(state) = inner.capacity.get_mut(&pool) {
                    let cooldown = retry_after
                        .unwrap_or(self.policy.minimum_capacity_cooldown)
                        .max(self.policy.minimum_capacity_cooldown)
                        .min(self.policy.max_capacity_cooldown);
                    advance_generation(&mut state.generation);
                    extend_open_until(&mut state.open_until, now, cooldown);
                }
            }
            Mutation::RouteFailure => {
                if let Some(state) = inner.route_health.get_mut(route) {
                    apply_route_failure(state, self.policy, now);
                }
            }
            Mutation::None => {}
        }
    }

    fn snapshot_locked(
        &self,
        inner: &ShadowHealthInner,
        route: &RouteKey,
        now: Instant,
    ) -> Option<ShadowRouteSnapshot> {
        let route_health = route_health_disposition(
            inner.route_health.get(route)?,
            self.policy.route_failure_window,
            self.policy.minimum_capacity_cooldown,
            now,
        );
        let deployment_pool = CapacityPoolKey::ProviderModel(route.clone());
        let deployment_capacity = capacity_disposition(
            inner.capacity.get(&deployment_pool)?,
            self.policy.minimum_capacity_cooldown,
            now,
        );
        let rate_limit_pool = self.rate_limit_pools.get(route)?;
        let rate_limit_capacity = capacity_disposition(
            inner.capacity.get(rate_limit_pool)?,
            self.policy.minimum_capacity_cooldown,
            now,
        );
        let effective =
            strongest_disposition([route_health, deployment_capacity, rate_limit_capacity]);

        Some(ShadowRouteSnapshot {
            route_health,
            deployment_capacity,
            rate_limit_capacity,
            effective,
        })
    }

    fn lock(&self) -> MutexGuard<'_, ShadowHealthInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(test)]
    fn with_policy(policy: ShadowHealthPolicy) -> Self {
        Self::new_with_policy(&PROVIDER_REGISTRY, policy)
    }

    #[cfg(test)]
    fn observe_terminal_at(
        &self,
        terminal: &AttemptTerminal,
        mode: ShadowObservationMode,
        now: Instant,
    ) -> ShadowObservationReport {
        let mut inner = self.lock();
        self.observe_terminal_locked(&mut inner, terminal, mode, None, now)
    }

    #[cfg(test)]
    fn observe_terminal_with_probe_at(
        &self,
        terminal: &AttemptTerminal,
        mode: ShadowObservationMode,
        mut probe: Option<ProbeLease>,
        now: Instant,
    ) -> ShadowObservationReport {
        let mut inner = self.lock();
        self.observe_terminal_locked(&mut inner, terminal, mode, probe.as_mut(), now)
    }

    #[cfg(test)]
    pub(crate) fn try_claim_probe_at(&self, route: &RouteKey, now: Instant) -> ProbeClaimResult {
        let mut inner = self.lock();
        self.try_claim_probe_locked(&mut inner, route, now)
    }

    #[cfg(test)]
    fn snapshot_at(&self, route: &RouteKey, now: Instant) -> Option<ShadowRouteSnapshot> {
        let inner = self.lock();
        self.snapshot_locked(&inner, route, now)
    }

    #[cfg(test)]
    fn cardinality(&self) -> (usize, usize) {
        let inner = self.lock();
        (inner.route_health.len(), inner.capacity.len())
    }

    #[cfg(test)]
    pub(crate) fn observation_count(&self) -> usize {
        self.observation_count.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeGateStatus {
    Healthy,
    StillOpen { remaining: Duration },
    ExpiredOpen,
    ProbeInFlight,
}

fn probe_gate_status(
    inner: &ShadowHealthInner,
    gate: &ProbeGateKey,
    now: Instant,
) -> Option<ProbeGateStatus> {
    let (open_until, active_probe) = match gate {
        ProbeGateKey::RouteHealth(route) => {
            let state = inner.route_health.get(route)?;
            (state.open_until, state.active_probe)
        }
        ProbeGateKey::Capacity(pool) => {
            let state = inner.capacity.get(pool)?;
            (state.open_until, state.active_probe)
        }
    };

    if active_probe.is_some() {
        return Some(ProbeGateStatus::ProbeInFlight);
    }
    match open_until {
        Some(until) if now < until => Some(ProbeGateStatus::StillOpen {
            remaining: until.duration_since(now),
        }),
        Some(_) => Some(ProbeGateStatus::ExpiredOpen),
        None => Some(ProbeGateStatus::Healthy),
    }
}

fn next_probe_lease_id(inner: &mut ShadowHealthInner) -> ProbeLeaseId {
    loop {
        inner.next_probe_lease_id = inner.next_probe_lease_id.wrapping_add(1);
        if inner.next_probe_lease_id == 0 {
            continue;
        }
        let candidate = ProbeLeaseId(inner.next_probe_lease_id);
        let in_use = inner.route_health.values().any(|state| {
            state
                .active_probe
                .is_some_and(|probe| probe.lease_id == candidate)
        }) || inner.capacity.values().any(|state| {
            state
                .active_probe
                .is_some_and(|probe| probe.lease_id == candidate)
        });
        if !in_use {
            return candidate;
        }
    }
}

fn claim_probe_gate(
    inner: &mut ShadowHealthInner,
    gate: &ProbeGateKey,
    lease_id: ProbeLeaseId,
) -> Option<u64> {
    let (generation, active_probe) = match gate {
        ProbeGateKey::RouteHealth(route) => {
            let state = inner.route_health.get_mut(route)?;
            (&mut state.generation, &mut state.active_probe)
        }
        ProbeGateKey::Capacity(pool) => {
            let state = inner.capacity.get_mut(pool)?;
            (&mut state.generation, &mut state.active_probe)
        }
    };
    debug_assert!(active_probe.is_none());
    advance_generation(generation);
    *active_probe = Some(ActiveProbe {
        lease_id,
        generation_at_claim: *generation,
    });
    Some(*generation)
}

fn release_probe_claims(
    inner: &mut ShadowHealthInner,
    lease_id: ProbeLeaseId,
    claims: &[ProbeGateClaim],
) {
    for claim in claims {
        match &claim.gate {
            ProbeGateKey::RouteHealth(route) => {
                if let Some(state) = inner.route_health.get_mut(route) {
                    if active_probe_matches(state.active_probe, lease_id, claim.generation_at_claim)
                    {
                        state.active_probe = None;
                    }
                }
            }
            ProbeGateKey::Capacity(pool) => {
                if let Some(state) = inner.capacity.get_mut(pool) {
                    if active_probe_matches(state.active_probe, lease_id, claim.generation_at_claim)
                    {
                        state.active_probe = None;
                    }
                }
            }
        }
    }
}

fn resolve_probe_success(
    inner: &mut ShadowHealthInner,
    lease_id: ProbeLeaseId,
    claims: &[ProbeGateClaim],
) {
    for claim in claims {
        match &claim.gate {
            ProbeGateKey::RouteHealth(route) => {
                if let Some(state) = inner.route_health.get_mut(route) {
                    let matching_active = active_probe_matches(
                        state.active_probe,
                        lease_id,
                        claim.generation_at_claim,
                    );
                    let matches = matching_active && state.generation == claim.generation_at_claim;
                    if matches {
                        state.failure_times.clear();
                        state.open_until = None;
                        state.active_probe = None;
                        advance_generation(&mut state.generation);
                    } else if matching_active {
                        state.active_probe = None;
                    }
                }
            }
            ProbeGateKey::Capacity(pool) => {
                if let Some(state) = inner.capacity.get_mut(pool) {
                    let matching_active = active_probe_matches(
                        state.active_probe,
                        lease_id,
                        claim.generation_at_claim,
                    );
                    let matches = matching_active && state.generation == claim.generation_at_claim;
                    if matches {
                        state.open_until = None;
                        state.active_probe = None;
                        advance_generation(&mut state.generation);
                    } else if matching_active {
                        state.active_probe = None;
                    }
                }
            }
        }
    }
}

fn active_probe_matches(
    active_probe: Option<ActiveProbe>,
    lease_id: ProbeLeaseId,
    generation_at_claim: u64,
) -> bool {
    active_probe.is_some_and(|active| {
        active.lease_id == lease_id && active.generation_at_claim == generation_at_claim
    })
}

fn advance_generation(generation: &mut u64) {
    *generation = generation.wrapping_add(1);
    if *generation == 0 {
        *generation = 1;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mutation {
    Completed,
    Capacity {
        pool: CapacityPoolKey,
        retry_after: Option<Duration>,
    },
    RouteFailure,
    None,
}

fn is_route_health_failure(kind: AttemptFailureKind) -> bool {
    match kind {
        AttemptFailureKind::Connect
        | AttemptFailureKind::Transport
        | AttemptFailureKind::ResponseStartTimeout
        | AttemptFailureKind::ResponseBody
        | AttemptFailureKind::InvalidResponse
        | AttemptFailureKind::UpstreamResponseError
        | AttemptFailureKind::UpstreamStreamError
        | AttemptFailureKind::StreamTimeout
        | AttemptFailureKind::UnexpectedEof => true,
        AttemptFailureKind::ProviderUnavailable
        | AttemptFailureKind::RequestBuild
        | AttemptFailureKind::HttpStatus
        | AttemptFailureKind::CapacityRejected
        | AttemptFailureKind::ConsumerDropped => false,
    }
}

fn apply_route_failure(state: &mut RouteHealthState, policy: ShadowHealthPolicy, now: Instant) {
    advance_generation(&mut state.generation);
    // Any failure while an opened route is awaiting or running its half-open
    // probe reopens the circuit from this failure time. The probe deliberately
    // has no timeout, so the original failure window may already have elapsed.
    if state.open_until.is_some() {
        extend_open_until(&mut state.open_until, now, policy.route_open_cooldown);
        return;
    }

    state.failure_times.retain(|failure_at| {
        now.checked_duration_since(*failure_at)
            .is_none_or(|age| age <= policy.route_failure_window)
    });
    state.failure_times.push_back(now);

    // The queue never needs more entries than the threshold. Keeping only the
    // bounded recent tail makes the fixed-topology state safe even if a future
    // shadow policy raises its observation volume.
    let threshold = usize::from(policy.route_failures_to_open.max(1));
    while state.failure_times.len() > threshold {
        state.failure_times.pop_front();
    }

    if state.failure_times.len() >= threshold {
        extend_open_until(&mut state.open_until, now, policy.route_open_cooldown);
    } else {
        state.open_until = None;
    }
}

fn apply_route_success(state: &mut RouteHealthState, _now: Instant) {
    // Once a route has opened, only the matching fenced half-open probe may
    // heal it. A late success from an attempt admitted before the open must not
    // bypass probe ownership at or after the cooldown boundary.
    if state.open_until.is_some() {
        return;
    }
    if !state.failure_times.is_empty() {
        advance_generation(&mut state.generation);
    }
    state.failure_times.clear();
}

fn extend_open_until(open_until: &mut Option<Instant>, now: Instant, cooldown: Duration) {
    let candidate = now.checked_add(cooldown).unwrap_or(now);
    *open_until = Some(open_until.map_or(candidate, |current| current.max(candidate)));
}

fn route_health_disposition(
    state: &RouteHealthState,
    failure_window: Duration,
    probe_retry_after: Duration,
    now: Instant,
) -> ShadowDisposition {
    if state.active_probe.is_some() {
        return ShadowDisposition::ProbeInFlight {
            retry_after: probe_retry_after,
        };
    }
    if let Some(until) = state.open_until {
        return if now < until {
            ShadowDisposition::WouldOpen {
                remaining: until.duration_since(now),
            }
        } else {
            ShadowDisposition::WouldProbe
        };
    }

    let current_failures = state
        .failure_times
        .iter()
        .filter(|failure_at| {
            now.checked_duration_since(**failure_at)
                .is_none_or(|age| age <= failure_window)
        })
        .count();
    if current_failures > 0 {
        ShadowDisposition::Watch {
            consecutive_failures: u8::try_from(current_failures).unwrap_or(u8::MAX),
        }
    } else {
        ShadowDisposition::Healthy
    }
}

fn capacity_disposition(
    state: &CapacityState,
    probe_retry_after: Duration,
    now: Instant,
) -> ShadowDisposition {
    if state.active_probe.is_some() {
        return ShadowDisposition::ProbeInFlight {
            retry_after: probe_retry_after,
        };
    }
    match state.open_until {
        Some(until) if now < until => ShadowDisposition::WouldOpen {
            remaining: until.duration_since(now),
        },
        Some(_) => ShadowDisposition::WouldProbe,
        None => ShadowDisposition::Healthy,
    }
}

fn strongest_disposition(
    dispositions: impl IntoIterator<Item = ShadowDisposition>,
) -> ShadowDisposition {
    dispositions
        .into_iter()
        .fold(ShadowDisposition::Healthy, stronger_disposition)
}

fn stronger_disposition(left: ShadowDisposition, right: ShadowDisposition) -> ShadowDisposition {
    match (left, right) {
        (
            ShadowDisposition::ProbeInFlight {
                retry_after: left_retry_after,
            },
            ShadowDisposition::ProbeInFlight {
                retry_after: right_retry_after,
            },
        ) => ShadowDisposition::ProbeInFlight {
            retry_after: left_retry_after.max(right_retry_after),
        },
        (
            ShadowDisposition::ProbeInFlight { retry_after },
            ShadowDisposition::WouldOpen { remaining },
        )
        | (
            ShadowDisposition::WouldOpen { remaining },
            ShadowDisposition::ProbeInFlight { retry_after },
        ) => ShadowDisposition::ProbeInFlight {
            retry_after: retry_after.max(remaining),
        },
        (left @ ShadowDisposition::ProbeInFlight { .. }, _) => left,
        (_, right @ ShadowDisposition::ProbeInFlight { .. }) => right,
        (
            ShadowDisposition::WouldOpen {
                remaining: left_remaining,
            },
            ShadowDisposition::WouldOpen {
                remaining: right_remaining,
            },
        ) => ShadowDisposition::WouldOpen {
            remaining: left_remaining.max(right_remaining),
        },
        (left @ ShadowDisposition::WouldOpen { .. }, _) => left,
        (_, right @ ShadowDisposition::WouldOpen { .. }) => right,
        (ShadowDisposition::WouldProbe, _) | (_, ShadowDisposition::WouldProbe) => {
            ShadowDisposition::WouldProbe
        }
        (
            ShadowDisposition::Watch {
                consecutive_failures: left_failures,
            },
            ShadowDisposition::Watch {
                consecutive_failures: right_failures,
            },
        ) => ShadowDisposition::Watch {
            consecutive_failures: left_failures.max(right_failures),
        },
        (left @ ShadowDisposition::Watch { .. }, _) => left,
        (_, right @ ShadowDisposition::Watch { .. }) => right,
        (ShadowDisposition::Healthy, ShadowDisposition::Healthy) => ShadowDisposition::Healthy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::{
        AttemptFailure, AttemptStage, CompletionEvidence, InferenceExecution, ReplaySafety,
        RouteIdentity,
    };
    use crate::provider_registry::RouteSelectionSource;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn route(provider: ProviderId, public_model: &str, provider_model: &str) -> RouteIdentity {
        RouteIdentity::new(
            provider,
            public_model,
            provider_model,
            public_model,
            RouteSelectionSource::StaticSplit,
            None,
        )
    }

    fn completed(route: RouteIdentity) -> AttemptTerminal {
        AttemptTerminal::Completed {
            attempt: InferenceExecution {
                request_id: super::super::InferenceRequestId::new(),
                execution_id: super::super::InferenceExecutionId::new(),
            }
            .begin_attempt(route),
            evidence: CompletionEvidence::ProviderDone,
        }
    }

    fn failed(
        route: RouteIdentity,
        kind: AttemptFailureKind,
        status: Option<u16>,
        retry_after: Option<Duration>,
    ) -> AttemptTerminal {
        let mut failure = AttemptFailure::new(
            kind,
            AttemptStage::AwaitingResponse,
            ReplaySafety::NotProvenPreAcceptance,
        );
        failure.status = status;
        failure.retry_after = retry_after;
        AttemptTerminal::Failed {
            attempt: InferenceExecution {
                request_id: super::super::InferenceRequestId::new(),
                execution_id: super::super::InferenceExecutionId::new(),
            }
            .begin_attempt(route),
            failure,
        }
    }

    fn test_policy() -> ShadowHealthPolicy {
        ShadowHealthPolicy {
            route_failure_window: Duration::from_secs(10),
            route_failures_to_open: 3,
            route_open_cooldown: Duration::from_secs(5),
            minimum_capacity_cooldown: Duration::from_secs(4),
            max_capacity_cooldown: Duration::from_secs(10),
        }
    }

    fn expect_probe(result: ProbeClaimResult) -> ProbeLease {
        match result {
            ProbeClaimResult::Ready(Some(lease)) => lease,
            other => panic!("expected probe lease, got {other:?}"),
        }
    }

    fn open_route_at(state: &ShadowHealthState, route: &RouteIdentity, now: Instant) {
        for _ in 0..3 {
            state.observe_terminal_at(
                &failed(route.clone(), AttemptFailureKind::StreamTimeout, None, None),
                ShadowObservationMode::Update,
                now,
            );
        }
    }

    #[test]
    fn classifier_is_exhaustive_and_only_provider_faults_penalize_health() {
        let state = ShadowHealthState::with_policy(test_policy());
        let now = Instant::now();
        let kimi = route(ProviderId::Tinfoil, "kimi-k3", "kimi-k3");

        let route_failures = [
            AttemptFailureKind::Connect,
            AttemptFailureKind::Transport,
            AttemptFailureKind::ResponseStartTimeout,
            AttemptFailureKind::ResponseBody,
            AttemptFailureKind::InvalidResponse,
            AttemptFailureKind::UpstreamResponseError,
            AttemptFailureKind::UpstreamStreamError,
            AttemptFailureKind::StreamTimeout,
            AttemptFailureKind::UnexpectedEof,
        ];
        for kind in route_failures {
            let report = state.observe_terminal_at(
                &failed(kimi.clone(), kind, None, None),
                ShadowObservationMode::TelemetryOnly,
                now,
            );
            assert!(matches!(
                report.signal,
                ShadowSignal::RouteFailure {
                    kind: observed,
                    status: None
                } if observed == kind
            ));
            assert!(!report.mutated);
        }

        for kind in [
            AttemptFailureKind::ProviderUnavailable,
            AttemptFailureKind::RequestBuild,
            AttemptFailureKind::ConsumerDropped,
        ] {
            let report = state.observe_terminal_at(
                &failed(kimi.clone(), kind, None, None),
                ShadowObservationMode::Update,
                now,
            );
            assert!(matches!(report.signal, ShadowSignal::Neutral { .. }));
            assert!(!report.mutated);
        }

        let server = state.observe_terminal_at(
            &failed(
                kimi.clone(),
                AttemptFailureKind::HttpStatus,
                Some(500),
                None,
            ),
            ShadowObservationMode::TelemetryOnly,
            now,
        );
        assert!(matches!(server.signal, ShadowSignal::RouteFailure { .. }));

        let caller = state.observe_terminal_at(
            &failed(
                kimi.clone(),
                AttemptFailureKind::HttpStatus,
                Some(400),
                None,
            ),
            ShadowObservationMode::Update,
            now,
        );
        assert!(matches!(caller.signal, ShadowSignal::Neutral { .. }));
        assert!(!caller.mutated);

        let malformed_capacity = state.observe_terminal_at(
            &failed(kimi, AttemptFailureKind::CapacityRejected, Some(500), None),
            ShadowObservationMode::Update,
            now,
        );
        assert!(matches!(
            malformed_capacity.signal,
            ShadowSignal::Neutral { .. }
        ));
        assert!(!malformed_capacity.mutated);
    }

    #[test]
    fn provider_specific_rate_limit_scopes_and_deployment_capacity_are_isolated() {
        let state = ShadowHealthState::with_policy(test_policy());
        let now = Instant::now();
        let k3 = route(ProviderId::Tinfoil, "kimi-k3", "kimi-k3");
        let quick = route(ProviderId::Tinfoil, "gpt-oss-120b", "gpt-oss-120b");
        let k2 = route(ProviderId::Continuum, "glm-5-3-flash", "glm-5.3-flash");
        let glm_continuum = route(ProviderId::Continuum, "glm-5-3", "glm-5.3");
        let glm_tinfoil = route(ProviderId::Tinfoil, "glm-5-3", "glm-5-3");
        let glm_flash = route(ProviderId::Tinfoil, "glm-5-3-flash", "glm-5-3-flash");

        let tinfoil = state.observe_terminal_at(
            &failed(
                k3.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(429),
                Some(Duration::from_secs(5)),
            ),
            ShadowObservationMode::Update,
            now,
        );
        assert_eq!(
            tinfoil.capacity_pool,
            Some(CapacityPoolKey::ProviderModel(k3.route_key()))
        );
        assert!(matches!(
            state.snapshot_at(&k3.route_key(), now).unwrap().effective,
            ShadowDisposition::WouldOpen { .. }
        ));
        assert_eq!(
            state
                .snapshot_at(&quick.route_key(), now)
                .unwrap()
                .effective,
            ShadowDisposition::Healthy
        );

        let continuum = state.observe_terminal_at(
            &failed(
                k2.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(429),
                Some(Duration::from_secs(5)),
            ),
            ShadowObservationMode::Update,
            now,
        );
        assert_eq!(
            continuum.capacity_pool,
            Some(CapacityPoolKey::ProviderAccount(ProviderId::Continuum))
        );
        assert!(matches!(
            state
                .snapshot_at(&glm_continuum.route_key(), now)
                .unwrap()
                .effective,
            ShadowDisposition::WouldOpen { .. }
        ));
        assert_eq!(
            state
                .snapshot_at(&glm_tinfoil.route_key(), now)
                .unwrap()
                .effective,
            ShadowDisposition::Healthy
        );
        assert_eq!(
            state
                .snapshot_at(&glm_flash.route_key(), now)
                .unwrap()
                .effective,
            ShadowDisposition::Healthy
        );

        let tinfoil_glm = state.observe_terminal_at(
            &failed(
                glm_tinfoil.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(429),
                Some(Duration::from_secs(5)),
            ),
            ShadowObservationMode::Update,
            now,
        );
        assert_eq!(
            tinfoil_glm.capacity_pool,
            Some(CapacityPoolKey::ProviderModel(glm_tinfoil.route_key()))
        );
        assert!(matches!(
            state
                .snapshot_at(&glm_tinfoil.route_key(), now)
                .unwrap()
                .effective,
            ShadowDisposition::WouldOpen { .. }
        ));
        assert_eq!(
            state
                .snapshot_at(&glm_flash.route_key(), now)
                .unwrap()
                .effective,
            ShadowDisposition::Healthy
        );

        for status in [503, 529] {
            let deployment_state = ShadowHealthState::with_policy(test_policy());
            deployment_state.observe_terminal_at(
                &failed(
                    k2.clone(),
                    AttemptFailureKind::CapacityRejected,
                    Some(status),
                    Some(Duration::from_secs(5)),
                ),
                ShadowObservationMode::Update,
                now,
            );
            assert!(matches!(
                deployment_state
                    .snapshot_at(&k2.route_key(), now)
                    .unwrap()
                    .effective,
                ShadowDisposition::WouldOpen { .. }
            ));
            assert_eq!(
                deployment_state
                    .snapshot_at(&glm_continuum.route_key(), now)
                    .unwrap()
                    .effective,
                ShadowDisposition::Healthy
            );

            deployment_state.observe_terminal_at(
                &failed(
                    glm_tinfoil.clone(),
                    AttemptFailureKind::CapacityRejected,
                    Some(status),
                    Some(Duration::from_secs(5)),
                ),
                ShadowObservationMode::Update,
                now,
            );
            assert!(matches!(
                deployment_state
                    .snapshot_at(&glm_tinfoil.route_key(), now)
                    .unwrap()
                    .effective,
                ShadowDisposition::WouldOpen { .. }
            ));
            assert_eq!(
                deployment_state
                    .snapshot_at(&glm_flash.route_key(), now)
                    .unwrap()
                    .effective,
                ShadowDisposition::Healthy
            );
        }
    }

    #[test]
    fn capacity_cooldowns_are_bounded_and_success_cannot_clear_them_early() {
        let state = ShadowHealthState::with_policy(test_policy());
        let start = Instant::now();
        let k3 = route(ProviderId::Tinfoil, "kimi-k3", "kimi-k3");
        let key = k3.route_key();

        state.observe_terminal_at(
            &failed(
                k3.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(429),
                None,
            ),
            ShadowObservationMode::Update,
            start,
        );
        assert_eq!(
            state.snapshot_at(&key, start).unwrap().effective,
            ShadowDisposition::WouldOpen {
                remaining: Duration::from_secs(4)
            }
        );

        state.observe_terminal_at(
            &completed(k3.clone()),
            ShadowObservationMode::Update,
            start + Duration::from_secs(1),
        );
        assert!(matches!(
            state
                .snapshot_at(&key, start + Duration::from_secs(1))
                .unwrap()
                .effective,
            ShadowDisposition::WouldOpen { .. }
        ));
        assert_eq!(
            state
                .snapshot_at(&key, start + Duration::from_secs(4))
                .unwrap()
                .effective,
            ShadowDisposition::WouldProbe
        );

        let lease = expect_probe(state.try_claim_probe_at(&key, start + Duration::from_secs(4)));
        state.observe_terminal_with_probe_at(
            &completed(k3.clone()),
            ShadowObservationMode::Update,
            Some(lease),
            start + Duration::from_secs(4),
        );
        assert_eq!(
            state
                .snapshot_at(&key, start + Duration::from_secs(4))
                .unwrap()
                .effective,
            ShadowDisposition::Healthy
        );

        state.observe_terminal_at(
            &failed(
                k3.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(429),
                Some(Duration::ZERO),
            ),
            ShadowObservationMode::Update,
            start + Duration::from_secs(10),
        );
        assert_eq!(
            state
                .snapshot_at(&key, start + Duration::from_secs(10))
                .unwrap()
                .effective,
            ShadowDisposition::WouldOpen {
                remaining: Duration::from_secs(4)
            }
        );
        assert_eq!(
            state
                .snapshot_at(&key, start + Duration::from_secs(14))
                .unwrap()
                .effective,
            ShadowDisposition::WouldProbe
        );

        state.observe_terminal_at(
            &failed(
                k3.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(429),
                Some(Duration::from_secs(100)),
            ),
            ShadowObservationMode::Update,
            start + Duration::from_secs(20),
        );
        assert_eq!(
            state
                .snapshot_at(&key, start + Duration::from_secs(20))
                .unwrap()
                .effective,
            ShadowDisposition::WouldOpen {
                remaining: Duration::from_secs(10)
            }
        );
    }

    #[test]
    fn route_snapshots_share_one_point_in_time_and_fail_closed_for_unknown_routes() {
        let state = ShadowHealthState::with_policy(test_policy());
        let glm_tinfoil = route(ProviderId::Tinfoil, "glm-5-3", "glm-5-3").route_key();
        let glm_continuum = route(ProviderId::Continuum, "glm-5-3", "glm-5.3").route_key();

        let snapshots = state
            .snapshot_routes(&[glm_tinfoil.clone(), glm_continuum.clone()])
            .expect("registered GLM routes");
        assert_eq!(snapshots.len(), 2);
        assert!(snapshots
            .iter()
            .all(|snapshot| snapshot.effective == ShadowDisposition::Healthy));

        let unknown = RouteKey {
            provider: ProviderId::Tinfoil,
            provider_model_id: "not-registered".to_string(),
        };
        assert!(state
            .snapshot_routes(&[glm_tinfoil, unknown, glm_continuum])
            .is_none());
    }

    #[test]
    fn route_health_uses_a_window_threshold_and_terminal_success_reset() {
        let state = ShadowHealthState::with_policy(test_policy());
        let start = Instant::now();
        let k3 = route(ProviderId::Tinfoil, "kimi-k3", "kimi-k3");
        let key = k3.route_key();
        let fault = || failed(k3.clone(), AttemptFailureKind::StreamTimeout, None, None);

        state.observe_terminal_at(&fault(), ShadowObservationMode::Update, start);
        assert_eq!(
            state.snapshot_at(&key, start).unwrap().route_health,
            ShadowDisposition::Watch {
                consecutive_failures: 1
            }
        );

        state.observe_terminal_at(
            &completed(k3.clone()),
            ShadowObservationMode::Update,
            start + Duration::from_secs(1),
        );
        assert_eq!(
            state
                .snapshot_at(&key, start + Duration::from_secs(1))
                .unwrap()
                .route_health,
            ShadowDisposition::Healthy
        );

        for offset in [2, 3, 4] {
            state.observe_terminal_at(
                &fault(),
                ShadowObservationMode::Update,
                start + Duration::from_secs(offset),
            );
        }
        assert_eq!(
            state
                .snapshot_at(&key, start + Duration::from_secs(4))
                .unwrap()
                .route_health,
            ShadowDisposition::WouldOpen {
                remaining: Duration::from_secs(5)
            }
        );

        state.observe_terminal_at(
            &completed(k3.clone()),
            ShadowObservationMode::Update,
            start + Duration::from_secs(5),
        );
        assert!(matches!(
            state
                .snapshot_at(&key, start + Duration::from_secs(5))
                .unwrap()
                .route_health,
            ShadowDisposition::WouldOpen { .. }
        ));
        assert_eq!(
            state
                .snapshot_at(&key, start + Duration::from_secs(9))
                .unwrap()
                .route_health,
            ShadowDisposition::WouldProbe
        );

        let lease = expect_probe(state.try_claim_probe_at(&key, start + Duration::from_secs(9)));
        state.observe_terminal_with_probe_at(
            &completed(k3.clone()),
            ShadowObservationMode::Update,
            Some(lease),
            start + Duration::from_secs(9),
        );
        assert_eq!(
            state
                .snapshot_at(&key, start + Duration::from_secs(9))
                .unwrap()
                .route_health,
            ShadowDisposition::Healthy
        );

        state.observe_terminal_at(
            &fault(),
            ShadowObservationMode::Update,
            start + Duration::from_secs(30),
        );
        assert_eq!(
            state
                .snapshot_at(&key, start + Duration::from_secs(30))
                .unwrap()
                .route_health,
            ShadowDisposition::Watch {
                consecutive_failures: 1
            }
        );
    }

    #[test]
    fn route_health_threshold_uses_one_bounded_rolling_window() {
        let state = ShadowHealthState::with_policy(test_policy());
        let start = Instant::now();
        let k3 = route(ProviderId::Tinfoil, "kimi-k3", "kimi-k3");
        let key = k3.route_key();
        let fault = || failed(k3.clone(), AttemptFailureKind::StreamTimeout, None, None);

        for offset in [0, 9, 18] {
            state.observe_terminal_at(
                &fault(),
                ShadowObservationMode::Update,
                start + Duration::from_secs(offset),
            );
        }
        assert_eq!(
            state
                .snapshot_at(&key, start + Duration::from_secs(18))
                .unwrap()
                .route_health,
            ShadowDisposition::Watch {
                consecutive_failures: 2
            }
        );

        state.observe_terminal_at(
            &fault(),
            ShadowObservationMode::Update,
            start + Duration::from_secs(19),
        );
        assert!(matches!(
            state
                .snapshot_at(&key, start + Duration::from_secs(19))
                .unwrap()
                .route_health,
            ShadowDisposition::WouldOpen { .. }
        ));
    }

    #[test]
    fn recovered_transport_attempts_are_telemetry_only() {
        let state = ShadowHealthState::with_policy(test_policy());
        let now = Instant::now();
        let k3 = route(ProviderId::Tinfoil, "kimi-k3", "kimi-k3");
        let key = k3.route_key();

        let recovered = state.observe_terminal_at(
            &failed(k3.clone(), AttemptFailureKind::Connect, None, None),
            ShadowObservationMode::TelemetryOnly,
            now,
        );
        assert!(matches!(
            recovered.signal,
            ShadowSignal::RouteFailure {
                kind: AttemptFailureKind::Connect,
                ..
            }
        ));
        assert!(!recovered.mutated);

        state.observe_terminal_at(
            &completed(k3),
            ShadowObservationMode::Update,
            now + Duration::from_millis(1),
        );
        assert_eq!(
            state
                .snapshot_at(&key, now + Duration::from_millis(1))
                .unwrap()
                .effective,
            ShadowDisposition::Healthy
        );
    }

    #[test]
    fn half_open_probe_claim_is_atomic_with_one_winner_at_the_boundary() {
        let state = Arc::new(ShadowHealthState::with_policy(test_policy()));
        let start = Instant::now();
        let k3 = route(ProviderId::Tinfoil, "kimi-k3", "kimi-k3");
        let key = k3.route_key();
        state.observe_terminal_at(
            &failed(
                k3.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(429),
                Some(Duration::from_secs(4)),
            ),
            ShadowObservationMode::Update,
            start,
        );

        let boundary = start + Duration::from_secs(4);
        match state.try_claim_probe_at(&key, boundary - Duration::from_nanos(1)) {
            ProbeClaimResult::Rejected {
                reason: ProbeRejectionReason::CircuitOpen,
                retry_after,
            } => assert_eq!(retry_after, Duration::from_nanos(1)),
            other => panic!("circuit must remain closed immediately before boundary: {other:?}"),
        }
        let barrier = Arc::new(Barrier::new(33));
        let late_success = {
            let state = Arc::clone(&state);
            let barrier = Arc::clone(&barrier);
            let terminal = completed(k3);
            thread::spawn(move || {
                barrier.wait();
                state.observe_terminal_at(&terminal, ShadowObservationMode::Update, boundary)
            })
        };
        let threads = (0..32)
            .map(|_| {
                let state = Arc::clone(&state);
                let key = key.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    state.try_claim_probe_at(&key, boundary)
                })
            })
            .collect::<Vec<_>>();

        let report = late_success.join().expect("late success thread");
        assert_eq!(report.signal, ShadowSignal::Completed);

        let mut leases = Vec::new();
        let mut rejections = 0;
        for thread in threads {
            match thread.join().expect("probe claim thread") {
                ProbeClaimResult::Ready(Some(lease)) => leases.push(lease),
                ProbeClaimResult::Rejected {
                    reason: ProbeRejectionReason::ProbeInFlight,
                    retry_after,
                } => {
                    rejections += 1;
                    assert_eq!(retry_after, Duration::from_secs(4));
                }
                other => panic!("unexpected concurrent claim result: {other:?}"),
            }
        }
        assert_eq!(leases.len(), 1);
        assert_eq!(rejections, 31);
        assert_eq!(
            state.snapshot_at(&key, boundary).unwrap().effective,
            ShadowDisposition::ProbeInFlight {
                retry_after: Duration::from_secs(4)
            }
        );
        assert_eq!(
            state
                .snapshot_at(&key, boundary + Duration::from_secs(60 * 60))
                .unwrap()
                .effective,
            ShadowDisposition::ProbeInFlight {
                retry_after: Duration::from_secs(4)
            }
        );

        drop(leases);
        assert_eq!(
            state.snapshot_at(&key, boundary).unwrap().effective,
            ShadowDisposition::WouldProbe
        );
    }

    #[test]
    fn shared_continuum_account_gate_allows_one_probe_across_models() {
        let state = Arc::new(ShadowHealthState::with_policy(test_policy()));
        let start = Instant::now();
        let k2 = route(ProviderId::Continuum, "glm-5-3-flash", "glm-5.3-flash");
        let glm = route(ProviderId::Continuum, "glm-5-3", "glm-5.3");
        state.observe_terminal_at(
            &failed(
                k2.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(429),
                Some(Duration::from_secs(4)),
            ),
            ShadowObservationMode::Update,
            start,
        );

        let boundary = start + Duration::from_secs(4);
        let barrier = Arc::new(Barrier::new(2));
        let threads = [k2.route_key(), glm.route_key()]
            .into_iter()
            .map(|key| {
                let state = Arc::clone(&state);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    state.try_claim_probe_at(&key, boundary)
                })
            })
            .collect::<Vec<_>>();

        let mut leases = Vec::new();
        let mut rejected = 0;
        for thread in threads {
            match thread.join().expect("shared account probe thread") {
                ProbeClaimResult::Ready(Some(lease)) => leases.push(lease),
                ProbeClaimResult::Rejected {
                    reason: ProbeRejectionReason::ProbeInFlight,
                    ..
                } => rejected += 1,
                other => panic!("unexpected shared account claim result: {other:?}"),
            }
        }
        assert_eq!(leases.len(), 1);
        assert_eq!(rejected, 1);
        assert!(matches!(
            state
                .snapshot_at(&k2.route_key(), boundary)
                .unwrap()
                .rate_limit_capacity,
            ShadowDisposition::ProbeInFlight { .. }
        ));
        assert!(matches!(
            state
                .snapshot_at(&glm.route_key(), boundary)
                .unwrap()
                .rate_limit_capacity,
            ShadowDisposition::ProbeInFlight { .. }
        ));
    }

    #[test]
    fn tinfoil_glm_models_probe_independently() {
        let state = ShadowHealthState::with_policy(test_policy());
        let start = Instant::now();
        let base = route(ProviderId::Tinfoil, "glm-5-3", "glm-5-3");
        let flash = route(ProviderId::Tinfoil, "glm-5-3-flash", "glm-5-3-flash");

        for route in [&base, &flash] {
            state.observe_terminal_at(
                &failed(
                    route.clone(),
                    AttemptFailureKind::CapacityRejected,
                    Some(429),
                    Some(Duration::from_secs(4)),
                ),
                ShadowObservationMode::Update,
                start,
            );
        }

        let boundary = start + Duration::from_secs(4);
        let base_probe = expect_probe(state.try_claim_probe_at(&base.route_key(), boundary));
        let flash_probe = expect_probe(state.try_claim_probe_at(&flash.route_key(), boundary));

        assert_eq!(base_probe.claim_count(), 1);
        assert_eq!(flash_probe.claim_count(), 1);
        assert!(matches!(
            state
                .snapshot_at(&base.route_key(), boundary)
                .unwrap()
                .rate_limit_capacity,
            ShadowDisposition::ProbeInFlight { .. }
        ));
        assert!(matches!(
            state
                .snapshot_at(&flash.route_key(), boundary)
                .unwrap()
                .rate_limit_capacity,
            ShadowDisposition::ProbeInFlight { .. }
        ));
    }

    #[test]
    fn glm_flash_probe_skips_the_route_health_gate() {
        let state = ShadowHealthState::with_policy(test_policy());
        let now = Instant::now();
        let flash = route(ProviderId::Tinfoil, "glm-5-3-flash", "glm-5-3-flash");
        let glm = route(ProviderId::Tinfoil, "glm-5-3", "glm-5-3");
        open_route_at(&state, &flash, now);
        open_route_at(&state, &glm, now);

        match state.try_claim_probe_at(&flash.route_key(), now) {
            ProbeClaimResult::Ready(None) => {}
            other => panic!("Flash should ignore route-health circuits, got {other:?}"),
        }
        match state.try_claim_probe_at(&glm.route_key(), now) {
            ProbeClaimResult::Rejected {
                reason: ProbeRejectionReason::CircuitOpen,
                ..
            } => {}
            other => panic!("GLM 5.3 should still fence route-health, got {other:?}"),
        }
    }

    #[test]
    fn composite_probe_claims_are_all_or_none_and_deduplicate_capacity_gates() {
        let start = Instant::now();
        let k2 = route(ProviderId::Continuum, "glm-5-3", "glm-5.3");

        let composite = ShadowHealthState::with_policy(test_policy());
        open_route_at(&composite, &k2, start);
        for status in [503, 429] {
            composite.observe_terminal_at(
                &failed(
                    k2.clone(),
                    AttemptFailureKind::CapacityRejected,
                    Some(status),
                    Some(Duration::from_secs(5)),
                ),
                ShadowObservationMode::Update,
                start,
            );
        }
        let boundary = start + Duration::from_secs(5);
        let composite_lease = expect_probe(composite.try_claim_probe_at(&k2.route_key(), boundary));
        assert_eq!(composite_lease.claim_count(), 3);
        let snapshot = composite.snapshot_at(&k2.route_key(), boundary).unwrap();
        assert!(matches!(
            snapshot.route_health,
            ShadowDisposition::ProbeInFlight { .. }
        ));
        assert!(matches!(
            snapshot.deployment_capacity,
            ShadowDisposition::ProbeInFlight { .. }
        ));
        assert!(matches!(
            snapshot.rate_limit_capacity,
            ShadowDisposition::ProbeInFlight { .. }
        ));
        drop(composite_lease);

        let tinfoil = ShadowHealthState::with_policy(test_policy());
        let k3 = route(ProviderId::Tinfoil, "kimi-k3", "kimi-k3");
        open_route_at(&tinfoil, &k3, start);
        tinfoil.observe_terminal_at(
            &failed(
                k3.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(429),
                Some(Duration::from_secs(5)),
            ),
            ShadowObservationMode::Update,
            start,
        );
        let deduplicated = expect_probe(tinfoil.try_claim_probe_at(&k3.route_key(), boundary));
        assert_eq!(deduplicated.claim_count(), 2);
        drop(deduplicated);

        let blocked = ShadowHealthState::with_policy(test_policy());
        open_route_at(&blocked, &k2, start);
        blocked.observe_terminal_at(
            &failed(
                k2.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(503),
                Some(Duration::from_secs(6)),
            ),
            ShadowObservationMode::Update,
            start,
        );
        match blocked.try_claim_probe_at(&k2.route_key(), boundary) {
            ProbeClaimResult::Rejected {
                reason: ProbeRejectionReason::CircuitOpen,
                retry_after,
            } => assert_eq!(retry_after, Duration::from_secs(1)),
            other => panic!("expected all-or-none rejection, got {other:?}"),
        }
        let snapshot = blocked.snapshot_at(&k2.route_key(), boundary).unwrap();
        assert_eq!(snapshot.route_health, ShadowDisposition::WouldProbe);
        assert_eq!(
            snapshot.deployment_capacity,
            ShadowDisposition::WouldOpen {
                remaining: Duration::from_secs(1)
            }
        );
    }

    #[test]
    fn successful_probe_heals_claimed_gates_and_resets_route_watch() {
        let start = Instant::now();
        let k2 = route(ProviderId::Continuum, "glm-5-3", "glm-5.3");
        let state = ShadowHealthState::with_policy(test_policy());
        open_route_at(&state, &k2, start);
        for status in [503, 429] {
            state.observe_terminal_at(
                &failed(
                    k2.clone(),
                    AttemptFailureKind::CapacityRejected,
                    Some(status),
                    Some(Duration::from_secs(5)),
                ),
                ShadowObservationMode::Update,
                start,
            );
        }
        let boundary = start + Duration::from_secs(5);
        let lease = expect_probe(state.try_claim_probe_at(&k2.route_key(), boundary));
        state.observe_terminal_with_probe_at(
            &completed(k2.clone()),
            ShadowObservationMode::Update,
            Some(lease),
            boundary,
        );
        assert_eq!(
            state
                .snapshot_at(&k2.route_key(), boundary)
                .unwrap()
                .effective,
            ShadowDisposition::Healthy
        );

        let only_account_open = ShadowHealthState::with_policy(test_policy());
        only_account_open.observe_terminal_at(
            &failed(k2.clone(), AttemptFailureKind::StreamTimeout, None, None),
            ShadowObservationMode::Update,
            start,
        );
        only_account_open.observe_terminal_at(
            &failed(
                k2.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(429),
                Some(Duration::from_secs(4)),
            ),
            ShadowObservationMode::Update,
            start,
        );
        let lease = expect_probe(
            only_account_open.try_claim_probe_at(&k2.route_key(), start + Duration::from_secs(4)),
        );
        assert_eq!(lease.claim_count(), 1);
        only_account_open.observe_terminal_with_probe_at(
            &completed(k2.clone()),
            ShadowObservationMode::Update,
            Some(lease),
            start + Duration::from_secs(4),
        );
        assert_eq!(
            only_account_open
                .snapshot_at(&k2.route_key(), start + Duration::from_secs(4))
                .unwrap()
                .route_health,
            ShadowDisposition::Healthy
        );
    }

    #[test]
    fn unleased_success_cannot_heal_an_owned_route_gate() {
        let state = ShadowHealthState::with_policy(test_policy());
        let start = Instant::now();
        let k3 = route(ProviderId::Tinfoil, "kimi-k3", "kimi-k3");
        let key = k3.route_key();
        open_route_at(&state, &k3, start);

        let boundary = start + Duration::from_secs(5);
        let lease = expect_probe(state.try_claim_probe_at(&key, boundary));
        state.observe_terminal_at(
            &completed(k3.clone()),
            ShadowObservationMode::Update,
            boundary,
        );
        assert!(matches!(
            state.snapshot_at(&key, boundary).unwrap().route_health,
            ShadowDisposition::ProbeInFlight { .. }
        ));

        state.observe_terminal_with_probe_at(
            &completed(k3),
            ShadowObservationMode::Update,
            Some(lease),
            boundary,
        );
        assert_eq!(
            state.snapshot_at(&key, boundary).unwrap().route_health,
            ShadowDisposition::Healthy
        );
    }

    #[test]
    fn failed_probes_reopen_only_the_classified_route_or_capacity_gate() {
        let start = Instant::now();
        let k3 = route(ProviderId::Tinfoil, "kimi-k3", "kimi-k3");
        let route_failure = ShadowHealthState::with_policy(test_policy());
        open_route_at(&route_failure, &k3, start);
        let boundary = start + Duration::from_secs(5);
        let lease = expect_probe(route_failure.try_claim_probe_at(&k3.route_key(), boundary));
        let late_terminal = boundary + Duration::from_secs(11);
        route_failure.observe_terminal_with_probe_at(
            &failed(k3.clone(), AttemptFailureKind::StreamTimeout, None, None),
            ShadowObservationMode::Update,
            Some(lease),
            late_terminal,
        );
        assert_eq!(
            route_failure
                .snapshot_at(&k3.route_key(), late_terminal)
                .unwrap()
                .route_health,
            ShadowDisposition::WouldOpen {
                remaining: Duration::from_secs(5)
            }
        );

        let k2 = route(ProviderId::Continuum, "glm-5-3-flash", "glm-5.3-flash");
        let glm = route(ProviderId::Continuum, "glm-5-3", "glm-5.3");
        let account_limit = ShadowHealthState::with_policy(test_policy());
        account_limit.observe_terminal_at(
            &failed(
                k2.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(429),
                Some(Duration::from_secs(4)),
            ),
            ShadowObservationMode::Update,
            start,
        );
        let account_boundary = start + Duration::from_secs(4);
        let lease =
            expect_probe(account_limit.try_claim_probe_at(&k2.route_key(), account_boundary));
        account_limit.observe_terminal_with_probe_at(
            &failed(
                k2.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(429),
                None,
            ),
            ShadowObservationMode::Update,
            Some(lease),
            account_boundary,
        );
        assert_eq!(
            account_limit
                .snapshot_at(&glm.route_key(), account_boundary)
                .unwrap()
                .rate_limit_capacity,
            ShadowDisposition::WouldOpen {
                remaining: Duration::from_secs(4)
            }
        );

        let deployment_limit = ShadowHealthState::with_policy(test_policy());
        deployment_limit.observe_terminal_at(
            &failed(
                k2.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(503),
                Some(Duration::from_secs(4)),
            ),
            ShadowObservationMode::Update,
            start,
        );
        let lease =
            expect_probe(deployment_limit.try_claim_probe_at(&k2.route_key(), account_boundary));
        deployment_limit.observe_terminal_with_probe_at(
            &failed(
                k2.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(503),
                None,
            ),
            ShadowObservationMode::Update,
            Some(lease),
            account_boundary,
        );
        assert_eq!(
            deployment_limit
                .snapshot_at(&k2.route_key(), account_boundary)
                .unwrap()
                .deployment_capacity,
            ShadowDisposition::WouldOpen {
                remaining: Duration::from_secs(4)
            }
        );
        assert_eq!(
            deployment_limit
                .snapshot_at(&glm.route_key(), account_boundary)
                .unwrap()
                .effective,
            ShadowDisposition::Healthy
        );
    }

    #[test]
    fn neutral_terminal_and_raii_drop_release_without_healing() {
        let state = ShadowHealthState::with_policy(test_policy());
        let start = Instant::now();
        let k3 = route(ProviderId::Tinfoil, "kimi-k3", "kimi-k3");
        let key = k3.route_key();
        state.observe_terminal_at(
            &failed(
                k3.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(429),
                Some(Duration::from_secs(4)),
            ),
            ShadowObservationMode::Update,
            start,
        );
        let boundary = start + Duration::from_secs(4);
        let lease = expect_probe(state.try_claim_probe_at(&key, boundary));
        state.observe_terminal_with_probe_at(
            &failed(k3.clone(), AttemptFailureKind::HttpStatus, Some(400), None),
            ShadowObservationMode::Update,
            Some(lease),
            boundary,
        );
        assert_eq!(
            state.snapshot_at(&key, boundary).unwrap().effective,
            ShadowDisposition::WouldProbe
        );

        let lease = expect_probe(state.try_claim_probe_at(&key, boundary));
        state.observe_terminal_at(
            &failed(k3, AttemptFailureKind::Connect, None, None),
            ShadowObservationMode::TelemetryOnly,
            boundary + Duration::from_secs(30),
        );
        assert!(matches!(
            state
                .snapshot_at(&key, boundary + Duration::from_secs(30))
                .unwrap()
                .effective,
            ShadowDisposition::ProbeInFlight { .. }
        ));
        drop(lease);
        assert_eq!(
            state.snapshot_at(&key, boundary).unwrap().effective,
            ShadowDisposition::WouldProbe
        );
    }

    #[test]
    fn stale_or_duplicate_lease_cannot_release_a_newer_probe() {
        let state = ShadowHealthState::with_policy(test_policy());
        let start = Instant::now();
        let k3 = route(ProviderId::Tinfoil, "kimi-k3", "kimi-k3");
        let key = k3.route_key();
        state.observe_terminal_at(
            &failed(
                k3,
                AttemptFailureKind::CapacityRejected,
                Some(429),
                Some(Duration::from_secs(4)),
            ),
            ShadowObservationMode::Update,
            start,
        );
        let boundary = start + Duration::from_secs(4);
        let first = expect_probe(state.try_claim_probe_at(&key, boundary));
        let stale_one = ProbeLease {
            inner: Arc::clone(&first.inner),
            route: first.route.clone(),
            lease_id: first.lease_id,
            claims: first.claims.clone(),
        };
        let stale_two = ProbeLease {
            inner: Arc::clone(&first.inner),
            route: first.route.clone(),
            lease_id: first.lease_id,
            claims: first.claims.clone(),
        };
        drop(first);

        // Force an ID reuse to prove the gate-local generation, not merely the
        // monotonic ID, fences stale/double release.
        {
            let mut inner = state.lock();
            inner.next_probe_lease_id = stale_one.lease_id.0.wrapping_sub(1);
        }

        let newer = expect_probe(state.try_claim_probe_at(&key, boundary));
        assert_eq!(newer.lease_id, stale_one.lease_id);
        assert!(matches!(
            state.snapshot_at(&key, boundary).unwrap().effective,
            ShadowDisposition::ProbeInFlight { .. }
        ));
        drop(stale_one);
        drop(stale_two);
        assert!(matches!(
            state.snapshot_at(&key, boundary).unwrap().effective,
            ShadowDisposition::ProbeInFlight { .. }
        ));
        drop(newer);
        assert_eq!(
            state.snapshot_at(&key, boundary).unwrap().effective,
            ShadowDisposition::WouldProbe
        );
    }

    #[test]
    fn unrelated_success_cannot_heal_a_gate_with_a_live_probe() {
        let state = ShadowHealthState::with_policy(test_policy());
        let start = Instant::now();
        let k3 = route(ProviderId::Tinfoil, "kimi-k3", "kimi-k3");
        let key = k3.route_key();
        state.observe_terminal_at(
            &failed(
                k3.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(429),
                Some(Duration::from_secs(4)),
            ),
            ShadowObservationMode::Update,
            start,
        );
        let boundary = start + Duration::from_secs(4);
        let lease = expect_probe(state.try_claim_probe_at(&key, boundary));

        state.observe_terminal_at(
            &completed(k3.clone()),
            ShadowObservationMode::Update,
            boundary,
        );
        assert!(matches!(
            state.snapshot_at(&key, boundary).unwrap().effective,
            ShadowDisposition::ProbeInFlight { .. }
        ));

        state.observe_terminal_with_probe_at(
            &completed(k3.clone()),
            ShadowObservationMode::Update,
            Some(lease),
            boundary,
        );
        assert_eq!(
            state.snapshot_at(&key, boundary).unwrap().effective,
            ShadowDisposition::Healthy
        );

        let reopened_at = boundary + Duration::from_secs(10);
        state.observe_terminal_at(
            &failed(
                k3.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(429),
                Some(Duration::from_secs(4)),
            ),
            ShadowObservationMode::Update,
            reopened_at,
        );
        let second_boundary = reopened_at + Duration::from_secs(4);
        let lease = expect_probe(state.try_claim_probe_at(&key, second_boundary));

        // A distinct provider attempt can reopen a gate while its half-open
        // probe is still live. Its generation fences the older probe success.
        state.observe_terminal_at(
            &failed(
                k3.clone(),
                AttemptFailureKind::CapacityRejected,
                Some(429),
                None,
            ),
            ShadowObservationMode::Update,
            second_boundary,
        );
        state.observe_terminal_with_probe_at(
            &completed(k3),
            ShadowObservationMode::Update,
            Some(lease),
            second_boundary,
        );
        assert_eq!(
            state.snapshot_at(&key, second_boundary).unwrap().effective,
            ShadowDisposition::WouldOpen {
                remaining: Duration::from_secs(4)
            }
        );
    }

    #[test]
    fn registry_topology_bounds_state_under_concurrent_observation() {
        let state = Arc::new(ShadowHealthState::with_policy(test_policy()));
        let cardinality = state.cardinality();
        let terminal = completed(route(ProviderId::Tinfoil, "kimi-k3", "kimi-k3"));

        let threads = (0..32)
            .map(|_| {
                let state = Arc::clone(&state);
                let terminal = terminal.clone();
                thread::spawn(move || {
                    state.observe_terminal(&terminal, ShadowObservationMode::Update)
                })
            })
            .collect::<Vec<_>>();

        for thread in threads {
            let report = thread.join().expect("observation thread");
            assert!(report.mutated);
        }
        assert_eq!(state.cardinality(), cardinality);

        let unknown = completed(route(ProviderId::Tinfoil, "unknown", "unknown"));
        let report = state.observe_terminal(&unknown, ShadowObservationMode::Update);
        assert_eq!(report.signal, ShadowSignal::UnknownRoute);
        assert!(!report.mutated);

        for _ in 0..128 {
            match state.try_claim_probe(&unknown.attempt().route.route_key()) {
                ProbeClaimResult::Rejected {
                    reason: ProbeRejectionReason::UnknownRoute,
                    retry_after,
                } => assert_eq!(retry_after, Duration::from_secs(4)),
                other => panic!("unknown route must fail closed, got {other:?}"),
            }
        }
        assert_eq!(state.cardinality(), cardinality);
    }
}
