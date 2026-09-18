use clru::CLruCache;
use std::collections::hash_map::RandomState;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};
use zeroize::Zeroize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InsertOutcome {
    Inserted,
    Replaced,
    ReclaimedExpired,
    EvictedOldest,
}

struct TimedEntry<V> {
    value: V,
    expires_at: Instant,
}

/// Values detached from the cache so their destructors and deallocation can
/// run after the caller releases the cache lock.
#[must_use = "keep retired values alive until after releasing the cache lock"]
pub(crate) struct RetiredValues<V> {
    entries: Vec<TimedEntry<V>>,
}

impl<V> RetiredValues<V> {
    pub(crate) fn removed_count(&self) -> usize {
        self.entries.len()
    }
}

/// A private, fixed-capacity TTL cache with oldest-first admission.
///
/// The wrapped cache uses `RandomState`, preallocates for its immutable capacity,
/// and deliberately exposes neither mutable iteration nor resizing. In
/// particular, do not add a call to `CLruCache::resize`: clru issue #67 tracks a
/// panic after resizing a cache with holes.
pub(crate) struct BoundedTtlCache<K, V> {
    entries: CLruCache<K, TimedEntry<V>, RandomState>,
    ttl: Duration,
}

impl<K, V> BoundedTtlCache<K, V>
where
    K: Eq + Hash,
{
    pub(crate) fn new(capacity: NonZeroUsize, ttl: Duration) -> Self {
        Self {
            // `with_memory` uses `RandomState` and reserves both the hash table
            // and dense entry storage. The capacity remains fixed for life.
            entries: CLruCache::with_memory(capacity, capacity.get()),
            ttl,
        }
    }

    /// Inserts a new value, always admitting it. At capacity, an expired oldest
    /// entry is reclaimed first; otherwise the oldest live entry is evicted.
    /// Duplicate keys retain the historical latest-request-wins behavior.
    pub(crate) fn insert_evicting(&mut self, key: K, value: V) -> InsertOutcome {
        self.insert_evicting_at(key, value, Instant::now())
    }

    pub(crate) fn insert_evicting_at(&mut self, key: K, value: V, now: Instant) -> InsertOutcome {
        let expires_at = now
            .checked_add(self.ttl)
            .expect("bounded cache TTL must fit in Instant");

        if let Some(replaced) = self.entries.pop(&key) {
            drop(replaced);
            let previous = self.entries.put(key, TimedEntry { value, expires_at });
            debug_assert!(previous.is_none());
            return InsertOutcome::Replaced;
        }

        let outcome = if self.entries.is_full() {
            let (_, oldest) = self
                .entries
                .pop_back()
                .expect("a full bounded cache must contain an oldest entry");
            let outcome = if oldest.expires_at <= now {
                InsertOutcome::ReclaimedExpired
            } else {
                InsertOutcome::EvictedOldest
            };
            drop(oldest);
            outcome
        } else {
            InsertOutcome::Inserted
        };

        let previous = self.entries.put(key, TimedEntry { value, expires_at });
        debug_assert!(previous.is_none());
        debug_assert!(self.entries.len() <= self.entries.capacity());
        outcome
    }

    /// Returns a live value without consuming it. Reads do not change the
    /// recency order, so LRU order keeps matching expiry order as
    /// [`Self::retire_expired_at`] requires. An expired entry is removed
    /// instead of returned.
    #[cfg(test)]
    pub(crate) fn get_live(&mut self, key: &K) -> Option<&V> {
        self.get_live_at(key, Instant::now())
    }

    pub(crate) fn get_live_at(&mut self, key: &K, now: Instant) -> Option<&V> {
        if self
            .entries
            .peek(key)
            .is_some_and(|entry| entry.expires_at <= now)
        {
            drop(self.entries.pop(key));
            return None;
        }
        self.entries.peek(key).map(|entry| &entry.value)
    }

    /// Removes a value exactly once and returns it only while its TTL is live.
    pub(crate) fn take_live(&mut self, key: &K) -> Option<V> {
        self.take_live_at(key, Instant::now())
    }

    fn take_live_at(&mut self, key: &K, now: Instant) -> Option<V> {
        let entry = self.entries.pop(key)?;
        if entry.expires_at <= now {
            drop(entry);
            None
        } else {
            Some(entry.value)
        }
    }

    /// Detaches the oldest expired values without scanning live entries.
    ///
    /// Insertion and replacement both move a value to MRU with a fresh fixed
    /// TTL, so expiry order matches LRU order. Each value is zeroized in place
    /// before removal so a Rust move cannot leave secret bytes in CLru's
    /// preallocated backing storage. Returning the zeroized values lets the
    /// caller release its lock before their destructors and deallocation run.
    pub(crate) fn retire_expired_at(&mut self, now: Instant) -> RetiredValues<V>
    where
        V: Zeroize,
    {
        let expired_count = self
            .entries
            .iter()
            .rev()
            .take_while(|(_, entry)| entry.expires_at <= now)
            .count();
        // Allocate once so secret-bearing values are not repeatedly relocated
        // as the retirement buffer grows.
        let mut entries = Vec::with_capacity(expired_count);
        for _ in 0..expired_count {
            self.entries
                .back_mut()
                .expect("counted oldest cache entry must remain present")
                .1
                .value
                .zeroize();
            let (_, entry) = self
                .entries
                .pop_back()
                .expect("peeked oldest cache entry must remain present");
            entries.push(entry);
        }
        RetiredValues { entries }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn cache<K, V>(capacity: usize, ttl: Duration) -> BoundedTtlCache<K, V>
    where
        K: Eq + Hash,
    {
        BoundedTtlCache::new(NonZeroUsize::new(capacity).unwrap(), ttl)
    }

    #[test]
    fn evicts_exactly_the_oldest_entry_and_always_admits_newest() {
        let start = Instant::now();
        let mut cache = cache(3, Duration::from_secs(300));

        assert_eq!(
            cache.insert_evicting_at(1, "one", start),
            InsertOutcome::Inserted
        );
        cache.insert_evicting_at(2, "two", start + Duration::from_secs(1));
        cache.insert_evicting_at(3, "three", start + Duration::from_secs(2));

        assert_eq!(
            cache.insert_evicting_at(4, "four", start + Duration::from_secs(3)),
            InsertOutcome::EvictedOldest
        );
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.take_live_at(&1, start + Duration::from_secs(4)), None);
        assert_eq!(
            cache.take_live_at(&4, start + Duration::from_secs(4)),
            Some("four")
        );
    }

    #[test]
    fn configured_pending_capacity_admits_the_next_entry_by_evicting_oldest() {
        let start = Instant::now();
        assert_eq!(crate::MAX_PENDING_ATTESTATIONS, 65_536);
        assert_eq!(crate::PENDING_ATTESTATION_TTL, Duration::from_secs(5 * 60));
        let capacity = crate::MAX_PENDING_ATTESTATIONS;
        let mut cache = cache(capacity, crate::PENDING_ATTESTATION_TTL);

        for key in 0..capacity {
            assert_eq!(
                cache.insert_evicting_at(key, (), start),
                InsertOutcome::Inserted
            );
        }
        assert_eq!(cache.len(), capacity);
        assert_eq!(
            cache.insert_evicting_at(capacity, (), start),
            InsertOutcome::EvictedOldest
        );
        assert_eq!(cache.len(), capacity);
        assert_eq!(cache.take_live_at(&0, start), None);
        assert_eq!(cache.take_live_at(&capacity, start), Some(()));
    }

    #[test]
    fn newest_survives_until_capacity_later_insertions() {
        let start = Instant::now();
        let mut cache = cache(4, Duration::from_secs(300));

        cache.insert_evicting_at(0, 0, start);
        for key in 1..4 {
            cache.insert_evicting_at(key, key, start);
        }
        assert_eq!(cache.take_live_at(&0, start), Some(0));

        cache.insert_evicting_at(10, 10, start);
        for key in 11..14 {
            cache.insert_evicting_at(key, key, start);
            assert_eq!(cache.len(), 4);
        }
        assert_eq!(cache.take_live_at(&10, start), Some(10));

        cache.insert_evicting_at(20, 20, start);
        for key in 21..25 {
            cache.insert_evicting_at(key, key, start);
        }
        assert_eq!(cache.take_live_at(&20, start), None);
        assert_eq!(cache.take_live_at(&24, start), Some(24));
    }

    #[test]
    fn duplicate_replaces_promotes_and_does_not_grow() {
        let start = Instant::now();
        let mut cache = cache(2, Duration::from_secs(10));

        cache.insert_evicting_at(1, "old", start);
        cache.insert_evicting_at(2, "two", start + Duration::from_secs(1));
        assert_eq!(
            cache.insert_evicting_at(1, "new", start + Duration::from_secs(2)),
            InsertOutcome::Replaced
        );
        assert_eq!(cache.len(), 2);

        cache.insert_evicting_at(3, "three", start + Duration::from_secs(3));
        assert_eq!(cache.take_live_at(&2, start + Duration::from_secs(4)), None);
        assert_eq!(
            cache.take_live_at(&1, start + Duration::from_secs(11)),
            Some("new")
        );
    }

    #[test]
    fn arbitrary_take_is_one_use() {
        let start = Instant::now();
        let mut cache = cache(3, Duration::from_secs(10));
        cache.insert_evicting_at(1, "one", start);
        cache.insert_evicting_at(2, "two", start);
        cache.insert_evicting_at(3, "three", start);

        assert_eq!(cache.take_live_at(&2, start), Some("two"));
        assert_eq!(cache.take_live_at(&2, start), None);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn exact_ttl_boundary_is_expired() {
        let start = Instant::now();
        let mut cache = cache(2, Duration::from_secs(5));
        cache.insert_evicting_at(1, "one", start);
        assert_eq!(cache.take_live_at(&1, start + Duration::from_secs(5)), None);
    }

    #[test]
    fn retires_untouched_expired_values_below_capacity_and_drops_outside_cache() {
        let start = Instant::now();
        let zeroizes = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let mut cache = cache(4, Duration::from_secs(5));

        let spy = || ZeroizingDropSpy {
            zeroizes: zeroizes.clone(),
            drops: drops.clone(),
        };
        cache.insert_evicting_at(1, spy(), start);
        cache.insert_evicting_at(2, spy(), start + Duration::from_secs(4));

        let retired = cache.retire_expired_at(start + Duration::from_secs(5));
        assert_eq!(retired.removed_count(), 1);
        assert_eq!(cache.len(), 1);
        assert_eq!(zeroizes.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        drop(retired);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(cache
            .take_live_at(&2, start + Duration::from_secs(5))
            .is_some());
    }

    #[test]
    fn expiry_retirement_preserves_survivor_order() {
        let start = Instant::now();
        let mut cache = cache(3, Duration::from_secs(5));
        cache.insert_evicting_at(1, 1_u8, start);
        cache.insert_evicting_at(2, 2_u8, start + Duration::from_secs(4));
        cache.insert_evicting_at(3, 3_u8, start + Duration::from_secs(4));

        drop(cache.retire_expired_at(start + Duration::from_secs(5)));
        cache.insert_evicting_at(4, 4_u8, start + Duration::from_secs(5));
        assert_eq!(
            cache.insert_evicting_at(5, 5_u8, start + Duration::from_secs(5)),
            InsertOutcome::EvictedOldest
        );

        assert_eq!(cache.take_live_at(&2, start + Duration::from_secs(5)), None);
        assert_eq!(
            cache.take_live_at(&3, start + Duration::from_secs(5)),
            Some(3)
        );
    }

    #[test]
    fn full_cache_reclaims_expired_oldest_before_live_entries() {
        let start = Instant::now();
        let mut cache = cache(2, Duration::from_secs(5));
        cache.insert_evicting_at(1, "expired", start);
        cache.insert_evicting_at(2, "live", start + Duration::from_secs(4));

        assert_eq!(
            cache.insert_evicting_at(3, "new", start + Duration::from_secs(5)),
            InsertOutcome::ReclaimedExpired
        );
        assert_eq!(
            cache.take_live_at(&2, start + Duration::from_secs(5)),
            Some("live")
        );
        assert_eq!(
            cache.take_live_at(&3, start + Duration::from_secs(5)),
            Some("new")
        );
    }

    struct DropSpy(Arc<AtomicUsize>);

    impl Drop for DropSpy {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct ZeroizingDropSpy {
        zeroizes: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    impl Zeroize for ZeroizingDropSpy {
        fn zeroize(&mut self) {
            self.zeroizes.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Drop for ZeroizingDropSpy {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn removed_values_drop_immediately_for_every_removal_path() {
        let start = Instant::now();
        let drops = Arc::new(AtomicUsize::new(0));
        let mut cache = cache(2, Duration::from_secs(5));

        cache.insert_evicting_at(1, DropSpy(drops.clone()), start);
        cache.insert_evicting_at(1, DropSpy(drops.clone()), start);
        assert_eq!(drops.load(Ordering::SeqCst), 1, "replacement");

        cache.insert_evicting_at(2, DropSpy(drops.clone()), start);
        cache.insert_evicting_at(3, DropSpy(drops.clone()), start);
        assert_eq!(drops.load(Ordering::SeqCst), 2, "capacity eviction");

        assert!(cache
            .take_live_at(&2, start + Duration::from_secs(5))
            .is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 3, "expiry");

        drop(cache);
        assert_eq!(drops.load(Ordering::SeqCst), 4, "cache destruction");
    }

    #[tokio::test]
    async fn actual_locking_pattern_never_exceeds_capacity() {
        let cache = Arc::new(RwLock::new(cache(32, Duration::from_secs(300))));
        let mut tasks = Vec::new();

        for key in 0..512 {
            let cache = cache.clone();
            tasks.push(tokio::spawn(async move {
                cache.write().await.insert_evicting(key, key);
            }));
        }

        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(cache.read().await.len(), 32);
    }

    #[tokio::test]
    async fn concurrent_take_has_exactly_one_winner() {
        let cache = Arc::new(RwLock::new(cache(1, Duration::from_secs(300))));
        cache.write().await.insert_evicting(1, "one-use");

        let mut tasks = Vec::new();
        for _ in 0..32 {
            let cache = cache.clone();
            tasks.push(tokio::spawn(
                async move { cache.write().await.take_live(&1) },
            ));
        }

        let mut winners = 0;
        for task in tasks {
            if task.await.unwrap() == Some("one-use") {
                winners += 1;
            }
        }
        assert_eq!(winners, 1);
        assert_eq!(cache.read().await.len(), 0);
    }

    #[test]
    fn arbitrary_removal_and_extended_churn_remain_fixed_capacity() {
        let start = Instant::now();
        let mut cache = cache(128, Duration::from_secs(300));

        for key in 0..128 {
            cache.insert_evicting_at(key, key, start);
        }
        for key in (0..128).step_by(3) {
            assert_eq!(cache.take_live_at(&key, start), Some(key));
        }
        for key in 128..(128 * 12) {
            cache.insert_evicting_at(key, key, start);
            assert!(cache.len() <= 128);
        }

        assert_eq!(cache.len(), 128);
        assert_eq!(
            cache.take_live_at(&(128 * 12 - 1), start),
            Some(128 * 12 - 1)
        );
    }

    #[test]
    fn get_live_returns_without_consuming_and_drops_expired_entries() {
        let start = Instant::now();
        let mut cache = cache(2, Duration::from_secs(60));
        cache.insert_evicting_at(1, "one", start);

        assert_eq!(
            cache.get_live_at(&1, start + Duration::from_secs(59)),
            Some(&"one")
        );
        assert_eq!(
            cache.get_live_at(&1, start + Duration::from_secs(59)),
            Some(&"one")
        );
        assert_eq!(cache.get_live_at(&1, start + Duration::from_secs(60)), None);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.get_live(&1), None);
    }

    #[test]
    fn get_live_does_not_reorder_entries_so_oldest_expiry_is_still_retired_first() {
        let start = Instant::now();
        let mut cache = cache(3, Duration::from_secs(60));
        cache.insert_evicting_at(1, 1u8, start);
        cache.insert_evicting_at(2, 2u8, start + Duration::from_secs(10));
        cache.insert_evicting_at(3, 3u8, start + Duration::from_secs(20));

        // Reading the oldest entry must not move it to the front.
        assert_eq!(
            cache.get_live_at(&1, start + Duration::from_secs(30)),
            Some(&1u8)
        );
        let retired = cache.retire_expired_at(start + Duration::from_secs(65));
        assert_eq!(retired.removed_count(), 1);
        assert_eq!(cache.get_live_at(&1, start + Duration::from_secs(65)), None);
        assert_eq!(
            cache.get_live_at(&2, start + Duration::from_secs(65)),
            Some(&2u8)
        );
        assert_eq!(cache.len(), 2);
    }
}
