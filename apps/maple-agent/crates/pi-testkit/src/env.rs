//! Manually advanced time and deterministic randomness matching the recorder.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use pi_ai::env::{CancellationToken, PiEnv, RandomError, Sleep, SleepCancelled};
use tokio::sync::oneshot;

use crate::LocalTaskSet;

const RECORDER_SEED: u32 = 0x1234_5678;
type TimerKey = (u64, u64);

struct Timer {
    completion: oneshot::Sender<Result<(), SleepCancelled>>,
    signal: Option<CancellationToken>,
}

struct State {
    now_ms: i64,
    monotonic_ms: u64,
    timer_sequence: u64,
    timers: BTreeMap<TimerKey, Timer>,
    in_tick: bool,
    random_state: u32,
    uuid_sequence: u64,
    uuids: VecDeque<String>,
    random_numbers: VecDeque<f64>,
    random_bytes: VecDeque<u8>,
}

impl State {
    fn next_random(&mut self) -> u32 {
        // Match recorder/determinism.ts, including sharing this sequence
        // between Math.random and every byte of getRandomValues.
        self.random_state ^= self.random_state << 13;
        self.random_state ^= self.random_state >> 17;
        self.random_state ^= self.random_state << 5;
        self.random_state
    }

    fn move_to(&mut self, deadline: u64) {
        let elapsed = deadline
            .checked_sub(self.monotonic_ms)
            .expect("virtual monotonic time cannot move backward");
        self.now_ms = self
            .now_ms
            .checked_add(i64::try_from(elapsed).expect("virtual advance exceeds i64 milliseconds"))
            .expect("virtual wall clock overflow");
        self.monotonic_ms = deadline;
    }
}

/// An independent clock and random source. `new` uses the same seed as the
/// pinned TypeScript recorder; `with_seed` allows explicitly different streams.
///
/// Timers are registered eagerly and fire only during explicit advancement.
/// Use `advance_with` and `LocalTaskSet` to drain harness-owned continuations
/// between deadlines on Tokio's current-thread runtime. The clock cannot
/// determine that arbitrary external futures are quiescent.
#[derive(Clone)]
pub struct VirtualEnv {
    state: Arc<Mutex<State>>,
    advance_lock: Arc<tokio::sync::Mutex<()>>,
}

impl VirtualEnv {
    pub fn new(now_ms: i64) -> Self {
        Self::with_seed(now_ms, RECORDER_SEED)
    }

    pub fn with_seed(now_ms: i64, seed: u32) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                now_ms,
                monotonic_ms: 0,
                timer_sequence: 0,
                timers: BTreeMap::new(),
                in_tick: false,
                random_state: seed,
                uuid_sequence: 0,
                uuids: VecDeque::new(),
                random_numbers: VecDeque::new(),
                random_bytes: VecDeque::new(),
            })),
            advance_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Match `vi.setSystemTime`: changing wall time does not shorten or extend
    /// an already registered timer's remaining delay.
    pub fn set_now(&self, now_ms: i64) {
        self.state.lock().unwrap().now_ms = now_ms;
    }

    /// Advance through deadlines in order. Equal deadlines use registration
    /// order. Yield after each completion so current-thread continuations can
    /// register further timers before advancing to the next deadline.
    ///
    /// A continuation that itself yields may register its next timer after
    /// this call reaches its target. Use an explicit gate to finish such work
    /// before the next advance; this is not a scheduler-wide microtask drain.
    pub async fn advance(&self, ms: u64) {
        self.advance_inner(ms, None).await;
    }

    /// Advance while driving the test's owned callback futures to a checkpoint
    /// before moving past each deadline. A callback may yield any finite number
    /// of times before registering its next timer. External tasks and I/O still
    /// need explicit gates; only the supplied task set is driven.
    pub async fn advance_with(&self, ms: u64, tasks: &mut LocalTaskSet<'_>) {
        self.advance_inner(ms, Some(tasks)).await;
    }

    async fn advance_inner(&self, ms: u64, mut tasks: Option<&mut LocalTaskSet<'_>>) {
        let _advance = self.advance_lock.lock().await;
        if let Some(tasks) = tasks.as_deref_mut() {
            // Register initial callbacks before starting the tick, preserving
            // the zero-delay behavior of timers scheduled outside a tick.
            tasks.checkpoint().await;
        }
        let target = self
            .monotonic_ms()
            .checked_add(ms)
            .expect("virtual timer clock overflow");
        self.state.lock().unwrap().in_tick = true;
        let _tick = TickGuard(Arc::clone(&self.state));
        loop {
            let timer = {
                let mut state = self.state.lock().unwrap();
                let cancelled = state.timers.iter().find_map(|(key, timer)| {
                    timer
                        .signal
                        .as_ref()
                        .is_some_and(CancellationToken::is_cancelled)
                        .then_some(*key)
                });
                if let Some(key) = cancelled {
                    // Cancellation settles at the current instant; it must
                    // not move the clock to the cancelled timer's deadline.
                    state.timers.remove(&key)
                } else {
                    match state.timers.first_key_value().map(|(key, _)| *key) {
                        Some(key) if key.0 <= target => {
                            state.move_to(key.0);
                            state.timers.remove(&key)
                        }
                        _ => {
                            state.move_to(target);
                            None
                        }
                    }
                }
            };
            let Some(timer) = timer else {
                break;
            };
            let outcome = if timer
                .signal
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            {
                Err(SleepCancelled)
            } else {
                Ok(())
            };
            let _ = timer.completion.send(outcome);
            if let Some(tasks) = tasks.as_deref_mut() {
                tasks.checkpoint().await;
            } else {
                tokio::task::yield_now().await;
            }
        }
    }

    pub fn pending_timers(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .timers
            .values()
            .filter(|timer| {
                !timer
                    .signal
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled)
            })
            .count()
    }

    /// Scripted values are consumed before the deterministic fallback. Each
    /// script is independent: UUID generation does not consume PRNG state.
    pub fn push_uuid(&self, uuid: impl Into<String>) {
        self.state.lock().unwrap().uuids.push_back(uuid.into());
    }

    pub fn push_random_number(&self, value: f64) {
        assert!(
            (0.0..1.0).contains(&value),
            "Math.random values must be in [0, 1)"
        );
        self.state.lock().unwrap().random_numbers.push_back(value);
    }

    pub fn push_random_bytes(&self, bytes: impl IntoIterator<Item = u8>) {
        self.state.lock().unwrap().random_bytes.extend(bytes);
    }
}

impl Default for VirtualEnv {
    fn default() -> Self {
        Self::new(0)
    }
}

impl PiEnv for VirtualEnv {
    fn now_ms(&self) -> i64 {
        self.state.lock().unwrap().now_ms
    }

    fn monotonic_ms(&self) -> u64 {
        self.state.lock().unwrap().monotonic_ms
    }

    fn fill_random(&self, destination: &mut [u8]) -> Result<(), RandomError> {
        let mut state = self.state.lock().unwrap();
        for byte in destination {
            *byte = state
                .random_bytes
                .pop_front()
                .unwrap_or_else(|| state.next_random() as u8);
        }
        Ok(())
    }

    fn math_random(&self) -> f64 {
        let mut state = self.state.lock().unwrap();
        state
            .random_numbers
            .pop_front()
            .unwrap_or_else(|| f64::from(state.next_random()) / 4_294_967_296.0)
    }

    fn random_uuid(&self) -> Result<String, RandomError> {
        let mut state = self.state.lock().unwrap();
        if let Some(uuid) = state.uuids.pop_front() {
            return Ok(uuid);
        }
        state.uuid_sequence = state
            .uuid_sequence
            .checked_add(1)
            .expect("virtual UUID sequence exhausted");
        Ok(format!(
            "{:08x}-0000-4000-8000-000000000000",
            state.uuid_sequence
        ))
    }

    fn sleep<'a>(&'a self, ms: f64, signal: Option<&'a CancellationToken>) -> Sleep<'a> {
        if signal.is_some_and(CancellationToken::is_cancelled) {
            return Box::pin(async { Err(SleepCancelled) });
        }
        // Sinon fake timers truncate fractional milliseconds and allow zero
        // outside a tick. During a tick, a new zero-delay timer fires 1ms later.
        let delay_ms = if !ms.is_finite() || ms < 0.0 {
            0
        } else if ms > 2_147_483_647.0 {
            1
        } else {
            ms.trunc() as u64
        };
        let (completion, receiver) = oneshot::channel();
        let key = {
            let mut state = self.state.lock().unwrap();
            let delay_ms = if delay_ms == 0 && state.in_tick {
                1
            } else {
                delay_ms
            };
            let deadline = state
                .monotonic_ms
                .checked_add(delay_ms)
                .expect("virtual timer deadline overflow");
            let key = (deadline, state.timer_sequence);
            state.timer_sequence = state
                .timer_sequence
                .checked_add(1)
                .expect("virtual timer sequence exhausted");
            state.timers.insert(
                key,
                Timer {
                    completion,
                    signal: signal.cloned(),
                },
            );
            key
        };
        Box::pin(VirtualSleep {
            state: Arc::clone(&self.state),
            key,
            receiver,
            cancelled: signal.map(|signal| {
                Box::pin(signal.cancelled()) as Pin<Box<dyn Future<Output = ()> + Send>>
            }),
        })
    }
}

struct TickGuard(Arc<Mutex<State>>);

impl Drop for TickGuard {
    fn drop(&mut self) {
        self.0.lock().unwrap().in_tick = false;
    }
}

struct VirtualSleep<'a> {
    state: Arc<Mutex<State>>,
    key: TimerKey,
    receiver: oneshot::Receiver<Result<(), SleepCancelled>>,
    cancelled: Option<Pin<Box<dyn Future<Output = ()> + Send + 'a>>>,
}

impl Future for VirtualSleep<'_> {
    type Output = Result<(), SleepCancelled>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        // An already-fired timer remains settled if the token is subsequently
        // cancelled, just as a resolved JavaScript Promise does.
        if let Poll::Ready(result) = Pin::new(&mut self.receiver).poll(context) {
            return Poll::Ready(result.expect("virtual timer sender unexpectedly dropped"));
        }
        if self
            .cancelled
            .as_mut()
            .is_some_and(|cancelled| cancelled.as_mut().poll(context).is_ready())
        {
            self.state.lock().unwrap().timers.remove(&self.key);
            return Poll::Ready(Err(SleepCancelled));
        }
        Poll::Pending
    }
}

impl Drop for VirtualSleep<'_> {
    fn drop(&mut self) {
        self.state.lock().unwrap().timers.remove(&self.key);
    }
}
