//! Explicit sources of time, timers, and randomness for the Pi port.
//!
//! TypeScript Pi reads these from ambient globals. Keeping them on an injected
//! object lets independent sessions and tests run without replacing globals.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use getrandom::Error as RandomError;
pub use tokio_util::sync::CancellationToken;

/// An aborted timer. Callers retain responsibility for Pi's context-specific
/// abort handling (for example, retry turns this into an aborted message).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SleepCancelled;

impl fmt::Display for SleepCancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Aborted")
    }
}

impl Error for SleepCancelled {}

pub type Sleep<'a> = Pin<Box<dyn Future<Output = Result<(), SleepCancelled>> + Send + 'a>>;

/// Host services corresponding to `Date.now`, timers, `randomUUID`,
/// `crypto.getRandomValues`, and `Math.random`.
///
/// UUIDv7 is deliberately not implemented here: Pi's own UUIDv7 algorithm must
/// consume `now_ms` and `fill_random` with its original sequence rules.
pub trait PiEnv: Send + Sync {
    /// Signed milliseconds since the Unix epoch, like `Date.now()`.
    fn now_ms(&self) -> i64;

    /// Elapsed milliseconds for deadlines, independent of wall-clock changes.
    fn monotonic_ms(&self) -> u64;

    fn fill_random(&self, destination: &mut [u8]) -> Result<(), RandomError>;

    /// A uniformly distributed number in `[0, 1)`. Its sequence is not shared
    /// across injected environments.
    fn math_random(&self) -> f64;

    /// Equivalent to Node's `crypto.randomUUID()`, separately injectable for
    /// the session entry-ID collision tests.
    fn random_uuid(&self) -> Result<String, RandomError> {
        let mut bytes = [0_u8; 16];
        self.fill_random(&mut bytes)?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
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

    /// Register a timer when called, rather than when the returned future is
    /// first polled. Dropping that future releases its timer registration.
    /// Fractional milliseconds are accepted because faux-provider pacing uses
    /// them. A missing signal leaves the timer uncancellable by a token.
    fn sleep<'a>(&'a self, ms: f64, signal: Option<&'a CancellationToken>) -> Sleep<'a>;
}

/// Production services. Each instance owns its monotonic-clock origin.
#[derive(Debug)]
pub struct SystemEnv {
    started: Instant,
}

impl SystemEnv {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Default for SystemEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl PiEnv for SystemEnv {
    fn now_ms(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => i64::try_from(duration.as_millis())
                .expect("system time exceeds the supported millisecond range"),
            Err(error) => -i64::try_from(error.duration().as_millis())
                .expect("system time precedes the supported millisecond range"),
        }
    }

    fn monotonic_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis())
            .expect("monotonic clock exceeds the supported millisecond range")
    }

    fn fill_random(&self, destination: &mut [u8]) -> Result<(), RandomError> {
        getrandom::fill(destination)
    }

    fn math_random(&self) -> f64 {
        let mut bytes = [0_u8; 8];
        // Math.random has no failure result. Never silently substitute a
        // constant or predictable fallback when the OS random source fails.
        self.fill_random(&mut bytes)
            .expect("operating system randomness is unavailable");
        ((u64::from_le_bytes(bytes) >> 11) as f64) / ((1_u64 << 53) as f64)
    }

    fn sleep<'a>(&'a self, ms: f64, signal: Option<&'a CancellationToken>) -> Sleep<'a> {
        if signal.is_some_and(CancellationToken::is_cancelled) {
            return Box::pin(async { Err(SleepCancelled) });
        }
        // Node truncates fractional delays and clamps out-of-range delays to
        // one millisecond. The virtual environment follows the recorder's fake
        // timers instead, including their explicitly advanced zero-delay timers.
        let delay_ms = if !ms.is_finite() || !(1.0..=2_147_483_647.0).contains(&ms) {
            1
        } else {
            ms.trunc() as u64
        };
        let timer = tokio::time::sleep(Duration::from_millis(delay_ms));
        let signal = signal.cloned();
        // A JavaScript timer promise settles even if nobody awaits it yet.
        // Polling the timer only from the returned future would let a later
        // cancellation replace a timeout that already completed.
        let task = tokio::spawn(async move {
            match signal {
                Some(signal) => tokio::select! {
                    biased;
                    () = signal.cancelled() => Err(SleepCancelled),
                    () = timer => Ok(()),
                },
                None => {
                    timer.await;
                    Ok(())
                }
            }
        });
        Box::pin(SystemSleep { task })
    }
}

struct SystemSleep {
    task: tokio::task::JoinHandle<Result<(), SleepCancelled>>,
}

impl Future for SystemSleep {
    type Output = Result<(), SleepCancelled>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.task)
            .poll(context)
            .map(|result| result.expect("system timer task unexpectedly failed"))
    }
}

impl Drop for SystemSleep {
    fn drop(&mut self) {
        self.task.abort();
    }
}
