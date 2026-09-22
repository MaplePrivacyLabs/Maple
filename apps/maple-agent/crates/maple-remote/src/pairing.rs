//! One-time pairing codes and the host's pending pairing record.
//!
//! A code is 80 bits of randomness shown as sixteen Crockford base32
//! characters. It is the pre-shared key of one pairing handshake, valid for
//! five minutes and consumed by the first success. The host reads it from a
//! private file its `pair` command writes, so a running host needs no
//! restart to accept a new device.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::now_ms;

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Characters in a code: 16 × 5 bits = 80 bits.
pub const CODE_CHARS: usize = 16;
/// How long a published code stays valid.
pub const CODE_TTL: Duration = Duration::from_secs(5 * 60);
const PSK_DOMAIN: &[u8] = b"maple-pairing-v1";

#[derive(Clone, PartialEq, Eq)]
pub struct PairingCode(String);

/// The code is a secret; `Debug` never shows it.
impl std::fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingCode(..)")
    }
}

impl PairingCode {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 10];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let mut code = String::with_capacity(CODE_CHARS);
        let mut acc: u32 = 0;
        let mut bits = 0;
        for byte in bytes {
            acc = (acc << 8) | byte as u32;
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                code.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
            }
        }
        Self(code)
    }

    /// Accept what a person typed: any case, with or without dashes or
    /// spaces, and the usual Crockford confusables.
    pub fn parse(input: &str) -> Result<Self, String> {
        let mut code = String::with_capacity(CODE_CHARS);
        for ch in input.chars() {
            let ch = match ch.to_ascii_uppercase() {
                '-' | ' ' => continue,
                'O' => '0',
                'I' | 'L' => '1',
                other => other,
            };
            let byte = u8::try_from(ch).ok().filter(|byte| ALPHABET.contains(byte));
            let Some(byte) = byte else {
                return Err(format!("'{ch}' is not part of a pairing code"));
            };
            code.push(byte as char);
        }
        if code.chars().count() != CODE_CHARS {
            return Err(format!("a pairing code has {CODE_CHARS} characters"));
        }
        Ok(Self(code))
    }

    /// The code grouped for reading aloud.
    pub fn display(&self) -> String {
        self.0
            .as_bytes()
            .chunks(4)
            .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("-")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The pre-shared key for the pairing handshake.
    pub fn psk(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(PSK_DOMAIN);
        hasher.update(self.0.as_bytes());
        hasher.finalize().into()
    }
}

/// The code a host currently accepts, as stored on disk.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingPairing {
    pub code: String,
    pub created_ms: u64,
    pub expires_ms: u64,
}

/// The code is a secret; `Debug` shows only the validity window.
impl std::fmt::Debug for PendingPairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingPairing")
            .field("created_ms", &self.created_ms)
            .field("expires_ms", &self.expires_ms)
            .finish_non_exhaustive()
    }
}

impl PendingPairing {
    pub fn code(&self) -> Result<PairingCode, String> {
        PairingCode::parse(&self.code)
    }

    pub fn is_valid_at(&self, now_ms: u64) -> bool {
        now_ms < self.expires_ms
    }
}

/// The private file that holds the pending code.
pub struct PendingPairingStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl PendingPairingStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Publish a fresh code, replacing any pending one.
    pub fn publish(&self, code: &PairingCode) -> Result<PendingPairing, String> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = now_ms();
        let pending = PendingPairing {
            code: code.as_str().to_string(),
            created_ms: now,
            expires_ms: now + CODE_TTL.as_millis() as u64,
        };
        maple_agent::private_file::write_private_json(&self.path, &pending)
            .map_err(|error| format!("cannot write {}: {error}", self.path.display()))?;
        Ok(pending)
    }

    /// The pending code when one is valid. An expired record is removed.
    pub fn current(&self) -> Option<PendingPairing> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.read_valid()
    }

    /// The record on disk when it is still valid; an expired one is
    /// removed. Callers hold the lock.
    fn read_valid(&self) -> Option<PendingPairing> {
        let bytes = std::fs::read(&self.path).ok()?;
        let pending: PendingPairing = serde_json::from_slice(&bytes).ok()?;
        if pending.is_valid_at(now_ms()) {
            Some(pending)
        } else {
            let _ = std::fs::remove_file(&self.path);
            None
        }
    }

    /// Spend the pending code without checking which one it is: the
    /// operator withdrew it.
    pub fn consume(&self) {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = std::fs::remove_file(&self.path);
    }

    /// A pairing with `code` completed: spend the code if it is still the
    /// pending one. Fails when it was already spent or replaced, so of two
    /// pairings racing on one code exactly one succeeds.
    pub fn consume_if(&self, code: &PairingCode) -> Result<(), String> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stored = self
            .read_valid()
            .and_then(|pending| pending.code().ok())
            .filter(|pending| pending == code);
        if stored.is_none() {
            return Err("the pairing code was already used".to_string());
        }
        std::fs::remove_file(&self.path)
            .map_err(|error| format!("cannot spend the pairing code: {error}"))
    }
}

/// Pairing attempts per source address. A failed pairing handshake is one
/// attempt; too many in the window lock that address out until the window
/// passes. Session handshakes never count, so a revoked device that keeps
/// reconnecting does not lock its address out of pairing again.
pub struct PairingLimiter {
    attempts: Mutex<HashMap<IpAddr, Vec<Instant>>>,
    max_attempts: usize,
    window: Duration,
}

/// Addresses remembered at once. Past this the address with the oldest
/// latest failure is forgotten, so a flood of sources cannot grow the map
/// without bound.
pub const MAX_TRACKED_ADDRESSES: usize = 1024;

impl Default for PairingLimiter {
    fn default() -> Self {
        Self::new(5, Duration::from_secs(10 * 60))
    }
}

impl PairingLimiter {
    pub fn new(max_attempts: usize, window: Duration) -> Self {
        Self {
            attempts: Mutex::new(HashMap::new()),
            max_attempts,
            window,
        }
    }

    /// Whether `ip` may try now.
    pub fn allows(&self, ip: IpAddr) -> bool {
        let mut attempts = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(recent) = attempts.get_mut(&ip) else {
            return true;
        };
        let now = Instant::now();
        recent.retain(|at| now.duration_since(*at) < self.window);
        if recent.is_empty() {
            attempts.remove(&ip);
            return true;
        }
        recent.len() < self.max_attempts
    }

    /// A pairing handshake from `ip` failed.
    pub fn record_failure(&self, ip: IpAddr) {
        let mut attempts = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        if !attempts.contains_key(&ip) {
            attempts.retain(|_, recent| {
                recent.retain(|at| now.duration_since(*at) < self.window);
                !recent.is_empty()
            });
            if attempts.len() >= MAX_TRACKED_ADDRESSES {
                let oldest = attempts
                    .iter()
                    .min_by_key(|(_, recent)| recent.iter().max().copied())
                    .map(|(ip, _)| *ip);
                if let Some(oldest) = oldest {
                    attempts.remove(&oldest);
                }
            }
        }
        attempts.entry(ip).or_default().push(now);
    }

    /// Addresses with a failure still inside the window.
    pub fn tracked_addresses(&self) -> usize {
        self.attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_have_sixteen_characters_and_parse_leniently() {
        let code = PairingCode::generate();
        assert_eq!(code.as_str().len(), CODE_CHARS);
        assert!(code.as_str().bytes().all(|b| ALPHABET.contains(&b)));
        let shown = code.display();
        assert_eq!(shown.len(), CODE_CHARS + 3);
        assert_eq!(PairingCode::parse(&shown).unwrap(), code);
        assert_eq!(
            PairingCode::parse(&shown.to_lowercase().replace('-', " ")).unwrap(),
            code
        );
        assert_eq!(
            PairingCode::parse("oOiIlL1100AAAAAA").unwrap().as_str(),
            "0011111100AAAAAA"
        );
        assert!(PairingCode::parse("TOO-SHORT").is_err());
        assert!(
            PairingCode::parse("UUUUUUUUUUUUUUUU").is_err(),
            "U is not in the alphabet"
        );
        assert_ne!(code.psk(), PairingCode::generate().psk());
        assert_eq!(code.psk(), PairingCode::parse(&shown).unwrap().psk());
    }

    #[test]
    fn non_ascii_input_is_refused_and_debug_hides_the_code() {
        // U+0150 truncates to 0x50, 'P', which is in the alphabet.
        let error = PairingCode::parse("\u{150}000000000000000").unwrap_err();
        assert!(error.contains("not part of"), "{error}");
        // Sixteen characters, one of them multi-byte: not a length error.
        let error = PairingCode::parse("000000000000000\u{e9}").unwrap_err();
        assert!(error.contains("not part of"), "{error}");
        assert!(PairingCode::parse("0000000000000000").is_ok());

        let code = PairingCode::generate();
        let shown = format!("{code:?}");
        assert!(!shown.contains(code.as_str()), "{shown}");
        let pending = PendingPairing {
            code: code.as_str().to_string(),
            created_ms: 1,
            expires_ms: 2,
        };
        let shown = format!("{pending:?}");
        assert!(!shown.contains(code.as_str()), "{shown}");
        assert!(shown.contains("expires_ms"), "{shown}");
    }

    #[test]
    fn a_code_is_spent_only_by_the_pairing_that_used_it() {
        let dir = std::env::temp_dir().join(format!("maple-pairing-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = PendingPairingStore::new(dir.join("pending.json"));
        let code = PairingCode::generate();
        let other = PairingCode::generate();
        assert!(store.consume_if(&code).is_err(), "nothing pending");
        store.publish(&code).unwrap();
        assert!(store.consume_if(&other).is_err(), "a different code");
        assert!(store.current().is_some(), "the pending code survives");
        store.consume_if(&code).unwrap();
        assert!(store.current().is_none());
        assert!(
            store.consume_if(&code).is_err(),
            "the second pairing on one code loses"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pending_codes_expire_and_are_consumed() {
        let dir = std::env::temp_dir().join(format!("maple-pairing-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = PendingPairingStore::new(dir.join("pending.json"));
        assert!(store.current().is_none());
        let code = PairingCode::generate();
        let pending = store.publish(&code).unwrap();
        assert_eq!(store.current().unwrap().code, code.as_str());
        assert!(pending.is_valid_at(pending.created_ms));
        assert!(!pending.is_valid_at(pending.expires_ms));
        store.consume();
        assert!(store.current().is_none());
        let mut expired = store.publish(&code).unwrap();
        expired.expires_ms = 0;
        maple_agent::private_file::write_private_json(store.path(), &expired).unwrap();
        assert!(store.current().is_none());
        assert!(!store.path().exists(), "an expired record is removed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_limiter_locks_an_address_out_after_repeated_failures() {
        let limiter = PairingLimiter::new(2, Duration::from_secs(60));
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let other: IpAddr = "10.0.0.2".parse().unwrap();
        assert!(limiter.allows(ip));
        limiter.record_failure(ip);
        assert!(limiter.allows(ip));
        limiter.record_failure(ip);
        assert!(!limiter.allows(ip));
        assert!(limiter.allows(other));
        assert_eq!(limiter.tracked_addresses(), 1, "asking never adds an entry");
    }

    #[test]
    fn the_limiter_forgets_quiet_addresses_and_caps_how_many_it_tracks() {
        let limiter = PairingLimiter::new(2, Duration::from_millis(1));
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        limiter.record_failure(ip);
        limiter.record_failure(ip);
        assert!(!limiter.allows(ip));
        std::thread::sleep(Duration::from_millis(5));
        assert!(limiter.allows(ip), "the window passed");
        assert_eq!(limiter.tracked_addresses(), 0, "an empty entry is pruned");

        let limiter = PairingLimiter::new(2, Duration::from_secs(60));
        let first: IpAddr = "10.1.0.0".parse().unwrap();
        limiter.record_failure(first);
        limiter.record_failure(first);
        assert!(!limiter.allows(first));
        for index in 1..=MAX_TRACKED_ADDRESSES as u32 {
            let ip = IpAddr::from(std::net::Ipv4Addr::from(0x0a01_0000 + index));
            limiter.record_failure(ip);
        }
        assert_eq!(limiter.tracked_addresses(), MAX_TRACKED_ADDRESSES);
        assert!(
            limiter.allows(first),
            "the address with the oldest failure was forgotten"
        );
    }
}
