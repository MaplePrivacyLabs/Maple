use clru::CLruCache;
use std::collections::hash_map::RandomState;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

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
    /// recency order, so LRU order keeps matching expiry order and eviction
    /// stays oldest-first. An expired entry is removed instead of returned.
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
        assert_eq!(cache.get_live_at(&1, start + Duration::from_secs(4)), None);
        assert_eq!(
            cache.get_live_at(&4, start + Duration::from_secs(4)),
            Some(&"four")
        );
    }

    #[test]
    fn newest_survives_until_capacity_later_insertions() {
        let start = Instant::now();
        let mut cache = cache(4, Duration::from_secs(300));

        for key in 0..4 {
            cache.insert_evicting_at(key, key, start);
        }
        cache.insert_evicting_at(10, 10, start);
        for key in 11..14 {
            cache.insert_evicting_at(key, key, start);
            assert_eq!(cache.len(), 4);
        }
        assert_eq!(cache.get_live_at(&10, start), Some(&10));

        cache.insert_evicting_at(14, 14, start);
        assert_eq!(cache.get_live_at(&10, start), None);
        assert_eq!(cache.get_live_at(&14, start), Some(&14));
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
        assert_eq!(cache.get_live_at(&2, start + Duration::from_secs(4)), None);
        assert_eq!(
            cache.get_live_at(&1, start + Duration::from_secs(11)),
            Some(&"new")
        );
    }

    #[test]
    fn exact_ttl_boundary_is_expired() {
        let start = Instant::now();
        let mut cache = cache(2, Duration::from_secs(5));
        cache.insert_evicting_at(1, "one", start);
        assert_eq!(cache.get_live_at(&1, start + Duration::from_secs(5)), None);
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
            cache.get_live_at(&2, start + Duration::from_secs(5)),
            Some(&"live")
        );
        assert_eq!(
            cache.get_live_at(&3, start + Duration::from_secs(5)),
            Some(&"new")
        );
    }

    struct DropSpy(Arc<AtomicUsize>);

    impl Drop for DropSpy {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
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
            .get_live_at(&2, start + Duration::from_secs(5))
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
                cache
                    .write()
                    .await
                    .insert_evicting_at(key, key, Instant::now());
            }));
        }

        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(cache.read().await.len(), 32);
    }

    #[test]
    fn expiry_removal_and_extended_churn_remain_fixed_capacity() {
        let start = Instant::now();
        let ttl = Duration::from_secs(300);
        let mut cache = cache(128, ttl);

        // Every third key expires first; reading those leaves holes behind.
        for key in (0..128).step_by(3) {
            cache.insert_evicting_at(key, key, start);
        }
        for key in (0..128).filter(|key| key % 3 != 0) {
            cache.insert_evicting_at(key, key, start + Duration::from_secs(100));
        }
        for key in (0..128).step_by(3) {
            assert_eq!(cache.get_live_at(&key, start + ttl), None);
        }
        for key in 128..(128 * 12) {
            cache.insert_evicting_at(key, key, start + ttl);
            assert!(cache.len() <= 128);
        }

        assert_eq!(cache.len(), 128);
        assert_eq!(
            cache.get_live_at(&(128 * 12 - 1), start + ttl),
            Some(&(128 * 12 - 1))
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
    fn get_live_does_not_reorder_entries_so_the_oldest_is_still_evicted_first() {
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
        assert_eq!(
            cache.insert_evicting_at(4, 4u8, start + Duration::from_secs(30)),
            InsertOutcome::EvictedOldest
        );
        assert_eq!(cache.get_live_at(&1, start + Duration::from_secs(30)), None);
        assert_eq!(
            cache.get_live_at(&2, start + Duration::from_secs(30)),
            Some(&2u8)
        );
        assert_eq!(cache.len(), 3);
    }
}
