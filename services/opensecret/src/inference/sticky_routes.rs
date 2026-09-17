//! Process-local, per-account route memory for Router v2.
//!
//! Provider prompt caches are warmed per account and per upstream route. When
//! Router v2 shifts an account to another provider of the same model, or Auto
//! selection shifts it to another approved model, later requests should keep
//! that route while it stays healthy and the account stays active. The memory
//! is enclave-local like the health state, bounded, and forgets an entry one
//! hour after the account's last accepted request so normal policy resumes.
//!
//! Entries are keyed by account, inference surface, and the caller's selector.
//! Chat Completions and Responses build different prompts and apply different
//! compatibility rules, so one surface never rewrites the other's memory.
//!
//! A remembered route is a preference, never a permit: health gates, plan
//! access, and the first-send probe claim still decide whether it is used.

use crate::bounded_ttl_cache::BoundedTtlCache;
use crate::inference::InferenceSurface;
use crate::provider_registry::ProviderId;
use std::num::NonZeroUsize;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// An entry returns to normal routing policy once the account has not had a
/// request accepted on it for this long.
pub(crate) const STICKY_ROUTE_IDLE_TTL: Duration = Duration::from_secs(60 * 60);
/// Bounded number of remembered (account, surface, selector) routes per enclave.
pub(crate) const STICKY_ROUTE_CAPACITY: usize = 50_000;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StickyRouteKey {
    account_uuid: Uuid,
    surface: InferenceSurface,
    selector: String,
}

/// The route an account most recently executed for one surface and selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StickyRoute {
    pub(crate) public_model_id: String,
    pub(crate) provider: ProviderId,
    pub(crate) provider_model_id: String,
}

pub(crate) struct StickyRouteMemory {
    entries: Mutex<BoundedTtlCache<StickyRouteKey, StickyRoute>>,
    idle_ttl: Duration,
}

impl std::fmt::Debug for StickyRouteMemory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StickyRouteMemory")
            .field("idle_ttl", &self.idle_ttl)
            .finish_non_exhaustive()
    }
}

impl Default for StickyRouteMemory {
    fn default() -> Self {
        Self::new(
            NonZeroUsize::new(STICKY_ROUTE_CAPACITY).expect("sticky route capacity is non-zero"),
            STICKY_ROUTE_IDLE_TTL,
        )
    }
}

impl StickyRouteMemory {
    pub(crate) fn new(capacity: NonZeroUsize, idle_ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(BoundedTtlCache::new(capacity, idle_ttl)),
            idle_ttl,
        }
    }

    /// Returns the account's last route for `selector` on `surface` while the
    /// entry is live. Looking up neither extends the entry nor changes the
    /// eviction order; only an accepted request does.
    pub(crate) fn lookup(
        &self,
        account_uuid: Uuid,
        surface: InferenceSurface,
        selector: &str,
    ) -> Option<StickyRoute> {
        self.lookup_at(account_uuid, surface, selector, Instant::now())
    }

    /// Records the route a provider just accepted for the account and restarts
    /// its idle timer, replacing any earlier route for the same key. At
    /// capacity the least recently used account simply falls back to policy.
    pub(crate) fn record(
        &self,
        account_uuid: Uuid,
        surface: InferenceSurface,
        selector: &str,
        route: StickyRoute,
    ) {
        self.record_at(account_uuid, surface, selector, route, Instant::now());
    }

    pub(crate) fn lookup_at(
        &self,
        account_uuid: Uuid,
        surface: InferenceSurface,
        selector: &str,
        now: Instant,
    ) -> Option<StickyRoute> {
        let key = StickyRouteKey {
            account_uuid,
            surface,
            selector: selector.to_string(),
        };
        self.lock().get_live_at(&key, now).cloned()
    }

    pub(crate) fn record_at(
        &self,
        account_uuid: Uuid,
        surface: InferenceSurface,
        selector: &str,
        route: StickyRoute,
        now: Instant,
    ) {
        let key = StickyRouteKey {
            account_uuid,
            surface,
            selector: selector.to_string(),
        };
        self.lock().insert_evicting_at(key, route, now);
    }

    fn lock(&self) -> MutexGuard<'_, BoundedTtlCache<StickyRouteKey, StickyRoute>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_config::{AUTO_POWERFUL_MODEL_ID, AUTO_QUICK_MODEL_ID};

    const CHAT: InferenceSurface = InferenceSurface::ChatCompletions;
    const RESPONSES: InferenceSurface = InferenceSurface::Responses;

    fn route(model: &str, provider: ProviderId) -> StickyRoute {
        StickyRoute {
            public_model_id: model.to_string(),
            provider,
            provider_model_id: model.to_string(),
        }
    }

    #[test]
    fn remembers_routes_per_account_surface_and_selector() {
        let memory = StickyRouteMemory::default();
        let account = Uuid::from_u128(7);
        let other_account = Uuid::from_u128(8);
        let flash = route("glm-5-3-flash", ProviderId::Continuum);
        let kimi = route("kimi-k3", ProviderId::Tinfoil);
        let glm = route("glm-5-3", ProviderId::Continuum);

        memory.record(account, RESPONSES, AUTO_QUICK_MODEL_ID, flash.clone());
        memory.record(account, RESPONSES, AUTO_POWERFUL_MODEL_ID, kimi.clone());
        memory.record(account, CHAT, AUTO_POWERFUL_MODEL_ID, glm.clone());

        assert_eq!(
            memory.lookup(account, RESPONSES, AUTO_QUICK_MODEL_ID),
            Some(flash)
        );
        assert_eq!(
            memory.lookup(account, RESPONSES, AUTO_POWERFUL_MODEL_ID),
            Some(kimi)
        );
        // A Chat request never rewrites the Responses conversation's memory.
        assert_eq!(
            memory.lookup(account, CHAT, AUTO_POWERFUL_MODEL_ID),
            Some(glm)
        );
        assert_eq!(memory.lookup(account, CHAT, AUTO_QUICK_MODEL_ID), None);
        assert_eq!(memory.lookup(account, RESPONSES, "glm-5-3"), None);
        assert_eq!(
            memory.lookup(other_account, RESPONSES, AUTO_QUICK_MODEL_ID),
            None
        );
        assert_eq!(memory.len(), 3);
    }

    #[test]
    fn a_later_record_replaces_the_route_and_refreshes_the_idle_timer() {
        let memory = StickyRouteMemory::default();
        let account = Uuid::from_u128(9);
        let start = Instant::now();
        memory.record_at(
            account,
            CHAT,
            AUTO_QUICK_MODEL_ID,
            route("glm-5-3-flash", ProviderId::Tinfoil),
            start,
        );
        let refreshed_at = start + Duration::from_secs(50 * 60);
        memory.record_at(
            account,
            CHAT,
            AUTO_QUICK_MODEL_ID,
            route("glm-5-3-flash", ProviderId::Continuum),
            refreshed_at,
        );

        let just_after_first_expiry = start + STICKY_ROUTE_IDLE_TTL;
        assert_eq!(
            memory.lookup_at(account, CHAT, AUTO_QUICK_MODEL_ID, just_after_first_expiry),
            Some(route("glm-5-3-flash", ProviderId::Continuum))
        );
        assert_eq!(memory.len(), 1);
    }

    #[test]
    fn an_account_idle_for_one_hour_returns_to_policy() {
        let memory = StickyRouteMemory::default();
        let account = Uuid::from_u128(10);
        let start = Instant::now();
        memory.record_at(
            account,
            CHAT,
            AUTO_QUICK_MODEL_ID,
            route("glm-5-3-flash", ProviderId::Tinfoil),
            start,
        );

        let before_expiry = start + STICKY_ROUTE_IDLE_TTL - Duration::from_secs(1);
        assert!(memory
            .lookup_at(account, CHAT, AUTO_QUICK_MODEL_ID, before_expiry)
            .is_some());
        // A lookup is not a request and must not extend the idle timer.
        let at_expiry = start + STICKY_ROUTE_IDLE_TTL;
        assert_eq!(
            memory.lookup_at(account, CHAT, AUTO_QUICK_MODEL_ID, at_expiry),
            None
        );
        assert_eq!(memory.len(), 0, "expired entries are dropped on lookup");
    }

    #[test]
    fn capacity_is_fixed_and_evicts_the_account_with_the_oldest_accepted_request() {
        let memory = StickyRouteMemory::new(
            NonZeroUsize::new(2).expect("non-zero"),
            STICKY_ROUTE_IDLE_TTL,
        );
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let third = Uuid::from_u128(3);
        let flash = route("glm-5-3-flash", ProviderId::Tinfoil);

        memory.record(first, CHAT, AUTO_QUICK_MODEL_ID, flash.clone());
        memory.record(second, CHAT, AUTO_QUICK_MODEL_ID, flash.clone());
        // A lookup is not a request and does not protect the first account;
        // an accepted request does.
        assert!(memory.lookup(first, CHAT, AUTO_QUICK_MODEL_ID).is_some());
        memory.record(first, CHAT, AUTO_QUICK_MODEL_ID, flash.clone());
        memory.record(third, CHAT, AUTO_QUICK_MODEL_ID, flash.clone());

        assert_eq!(memory.len(), 2);
        assert_eq!(
            memory.lookup(first, CHAT, AUTO_QUICK_MODEL_ID),
            Some(flash.clone())
        );
        assert_eq!(memory.lookup(second, CHAT, AUTO_QUICK_MODEL_ID), None);
        assert_eq!(memory.lookup(third, CHAT, AUTO_QUICK_MODEL_ID), Some(flash));
    }

    #[test]
    fn default_memory_uses_the_one_hour_idle_policy() {
        let memory = StickyRouteMemory::default();
        assert_eq!(memory.idle_ttl, Duration::from_secs(3600));
        assert_eq!(STICKY_ROUTE_CAPACITY, 50_000);
    }
}
