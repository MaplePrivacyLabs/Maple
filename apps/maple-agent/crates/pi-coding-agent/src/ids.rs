use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use pi_ai::now_ms;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 64 unpredictable bits from the standard library's randomly keyed hasher.
pub(crate) fn random_u64() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.write_i64(now_ms());
    hasher.finish()
}

/// A short entry id (8 hex digits) that `taken` does not report as used.
pub fn new_entry_id(taken: impl Fn(&str) -> bool) -> String {
    loop {
        let id = format!("{:08x}", random_u64() as u32);
        if !taken(&id) {
            return id;
        }
    }
}

/// A time-ordered UUID (version 7), so session ids sort by creation time.
pub fn new_session_id() -> String {
    let millis = (now_ms() as u64) & 0xFFFF_FFFF_FFFF;
    let random = random_u64();
    let more = random_u64();
    format!(
        "{:08x}-{:04x}-7{:03x}-{:04x}-{:012x}",
        millis >> 16,
        millis & 0xFFFF,
        random & 0x0FFF,
        0x8000 | ((random >> 12) & 0x3FFF),
        more & 0xFFFF_FFFF_FFFF
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_version_7_uuids() {
        let id = new_session_id();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|part| part.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('7'));
        assert!(matches!(
            parts[3].chars().next(),
            Some('8' | '9' | 'a' | 'b')
        ));
        assert_ne!(id, new_session_id());
    }

    #[test]
    fn entry_ids_avoid_taken_ids() {
        let first = new_entry_id(|_| false);
        assert_eq!(first.len(), 8);
        let second = new_entry_id(|candidate| candidate == first);
        assert_ne!(first, second);
    }
}
