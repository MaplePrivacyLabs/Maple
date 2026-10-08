//! Port of `packages/ai/src/utils/uuid.ts`.
//!
//! This is Pi's sequence-based UUIDv7 algorithm, not a generic UUIDv7 generator.

use crate::env::{PiEnv, RandomError};
use std::fmt;
use std::sync::Mutex;

const MAX_UUID_V7_TIMESTAMP: u64 = 0xffff_ffff_ffff;
const MAX_SEQUENCE: u64 = (1 << 41) - 1;

#[derive(Debug)]
pub enum UuidV7Error {
    InvalidTimestamp,
    SequenceExhausted,
    Random(RandomError),
}

impl fmt::Display for UuidV7Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTimestamp => write!(
                formatter,
                "UUIDv7 timestamp must be an integer between 0 and {MAX_UUID_V7_TIMESTAMP}"
            ),
            Self::SequenceExhausted => formatter.write_str("UUIDv7 generator sequence exhausted"),
            Self::Random(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for UuidV7Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Random(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct State {
    last_ordinary_timestamp: i64,
    sequence: Option<u64>,
}

/// Share one generator wherever Pi would share its loaded UUID module.
/// Isolating this state makes independent test environments reproducible.
#[derive(Debug)]
pub struct UuidV7Generator {
    state: Mutex<State>,
}

impl Default for UuidV7Generator {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                last_ordinary_timestamp: -1,
                sequence: None,
            }),
        }
    }
}

impl UuidV7Generator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Generate an ordered UUID; supplied timestamps are preserved for follower IDs.
    pub fn uuidv7(
        &self,
        env: &dyn PiEnv,
        timestamp_ms: Option<f64>,
    ) -> Result<String, UuidV7Error> {
        let requested_timestamp = timestamp_ms.unwrap_or_else(|| env.now_ms() as f64);
        if !requested_timestamp.is_finite()
            || requested_timestamp.fract() != 0.0
            || !(0.0..=MAX_UUID_V7_TIMESTAMP as f64).contains(&requested_timestamp)
        {
            return Err(UuidV7Error::InvalidTimestamp);
        }

        let mut state = self.state.lock().expect("UUIDv7 state mutex poisoned");
        let effective_timestamp = if timestamp_ms.is_none() {
            let timestamp = (requested_timestamp as i64).max(state.last_ordinary_timestamp);
            state.last_ordinary_timestamp = timestamp;
            timestamp as u64
        } else {
            requested_timestamp as u64
        };

        let mut bytes = [0_u8; 16];
        // Pi requests fresh randomness even when the sequence is exhausted, and
        // updates ordinary time before a potential randomness failure.
        env.fill_random(&mut bytes).map_err(UuidV7Error::Random)?;
        let sequence = match state.sequence {
            None => {
                (u64::from(bytes[1]) << 32)
                    | (u64::from(bytes[2]) << 24)
                    | (u64::from(bytes[3]) << 16)
                    | (u64::from(bytes[4]) << 8)
                    | u64::from(bytes[5])
            }
            Some(MAX_SEQUENCE) => return Err(UuidV7Error::SequenceExhausted),
            Some(sequence) => sequence + 1,
        };
        state.sequence = Some(sequence);

        for index in (0..=5).rev() {
            bytes[index] = (effective_timestamp >> ((5 - index) * 8)) as u8;
        }
        bytes[6] = 0x70 | ((sequence >> 37) & 0x0f) as u8;
        bytes[7] = ((sequence >> 29) & 0xff) as u8;
        bytes[8] = 0x80 | ((sequence >> 23) & 0x3f) as u8;
        bytes[9] = ((sequence >> 15) & 0xff) as u8;
        bytes[10] = ((sequence >> 7) & 0xff) as u8;
        bytes[11] = ((sequence & 0x7f) << 1) as u8 | (bytes[11] & 0x01);

        let hex = bytes.map(|byte| format!("{byte:02x}")).concat();
        Ok(format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        ))
    }

    /// Narrow conformance seam for the otherwise unreachable sequence boundary.
    #[cfg(feature = "test-internals")]
    pub fn set_sequence_for_test(&self, sequence: Option<u64>) {
        self.state
            .lock()
            .expect("UUIDv7 state mutex poisoned")
            .sequence = sequence;
    }
}
