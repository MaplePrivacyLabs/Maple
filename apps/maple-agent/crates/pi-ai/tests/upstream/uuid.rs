//! Translated from `packages/ai/test/uuid.test.ts` at Pi v1.0.4.
use pi_ai::env::{CancellationToken, PiEnv, RandomError, Sleep};
use pi_ai::utils::uuid::{UuidV7Error, UuidV7Generator};
use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};

const TIMESTAMP: i64 = 0x0123_4567_89ab;

struct Env {
    now: AtomicI64,
    byte: AtomicU8,
}

impl Default for Env {
    fn default() -> Self {
        Self {
            now: AtomicI64::new(TIMESTAMP),
            byte: AtomicU8::new(0),
        }
    }
}

impl PiEnv for Env {
    fn now_ms(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
    fn monotonic_ms(&self) -> u64 {
        0
    }
    fn fill_random(&self, bytes: &mut [u8]) -> Result<(), RandomError> {
        bytes.fill(self.byte.fetch_add(1, Ordering::SeqCst).wrapping_add(1));
        Ok(())
    }
    fn math_random(&self) -> f64 {
        panic!("UUIDv7 must use crypto randomness")
    }
    fn sleep<'a>(&'a self, _: f64, _: Option<&'a CancellationToken>) -> Sleep<'a> {
        panic!("UUIDv7 must not sleep")
    }
}

fn parse_timestamp(uuid: &str) -> i64 {
    i64::from_str_radix(&uuid.replace('-', "")[..12], 16).unwrap()
}

mod uuidv7 {
    use super::*;

    #[test]
    fn generates_ordered_uuidv7s_while_preserving_follower_timestamps() {
        let env = Env::default();
        let generator = UuidV7Generator::new();
        let first = generator.uuidv7(&env, None).unwrap();
        let second = generator.uuidv7(&env, None).unwrap();
        env.now.store(TIMESTAMP - 1, Ordering::SeqCst);
        let after_rollback = generator.uuidv7(&env, None).unwrap();
        env.now.store(TIMESTAMP + 1, Ordering::SeqCst);
        let after_advance = generator.uuidv7(&env, None).unwrap();
        let ordinary_ids = vec![first, second, after_rollback, after_advance];
        let follower_timestamp = TIMESTAMP - 1_000;
        let followers = vec![
            generator
                .uuidv7(&env, Some(follower_timestamp as f64))
                .unwrap(),
            generator
                .uuidv7(&env, Some(follower_timestamp as f64))
                .unwrap(),
        ];
        let uuid_v7_re = regex::Regex::new(
            r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$",
        )
        .unwrap();
        for id in ordinary_ids.iter().chain(&followers) {
            assert!(uuid_v7_re.is_match(id));
        }
        let mut ordered = ordinary_ids.clone();
        ordered.sort();
        assert_eq!(ordinary_ids, ordered);
        assert_eq!(
            ordinary_ids.iter().collect::<HashSet<_>>().len(),
            ordinary_ids.len()
        );
        assert_eq!(
            ordinary_ids
                .iter()
                .map(|id| parse_timestamp(id))
                .collect::<Vec<_>>(),
            vec![TIMESTAMP, TIMESTAMP, TIMESTAMP, TIMESTAMP + 1]
        );
        assert_eq!(
            followers
                .iter()
                .map(|id| parse_timestamp(id))
                .collect::<Vec<_>>(),
            vec![follower_timestamp, follower_timestamp]
        );
        assert_eq!(
            followers.iter().collect::<HashSet<_>>().len(),
            followers.len()
        );
    }

    #[test]
    fn uses_fresh_randomness_for_every_uuid_tail() {
        let env = Env::default();
        let generator = UuidV7Generator::new();
        let tails: Vec<String> = (0..2)
            .map(|_| generator.uuidv7(&env, Some(TIMESTAMP as f64)).unwrap()[28..].into())
            .collect();
        assert_eq!(tails, ["01010101", "02020202"]);
    }

    #[test]
    fn accepts_timestamp_boundary_0() {
        assert_eq!(
            parse_timestamp(
                &UuidV7Generator::new()
                    .uuidv7(&Env::default(), Some(0.0))
                    .unwrap()
            ),
            0
        );
    }

    #[test]
    fn accepts_timestamp_boundary_281474976710655() {
        let timestamp = 2_i64.pow(48) - 1;
        assert_eq!(
            parse_timestamp(
                &UuidV7Generator::new()
                    .uuidv7(&Env::default(), Some(timestamp as f64))
                    .unwrap()
            ),
            timestamp
        );
    }

    fn rejects(timestamp: f64) {
        assert!(matches!(
            UuidV7Generator::new().uuidv7(&Env::default(), Some(timestamp)),
            Err(UuidV7Error::InvalidTimestamp)
        ));
    }

    #[test]
    fn rejects_invalid_timestamp_1() {
        rejects(-1.0);
    }
    #[test]
    fn rejects_invalid_timestamp_281474976710656() {
        rejects(2.0_f64.powi(48));
    }
    #[test]
    fn rejects_invalid_timestamp_1_5() {
        rejects(1.5);
    }
    #[test]
    fn rejects_invalid_timestamp_nan() {
        rejects(f64::NAN);
    }
    #[test]
    fn rejects_invalid_timestamp_infinity() {
        rejects(f64::INFINITY);
    }
}
