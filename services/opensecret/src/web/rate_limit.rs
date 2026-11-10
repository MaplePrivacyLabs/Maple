//! Process-local request limits for the inner application routes.
//!
//! Limits run after the transport has authenticated and decrypted the logical
//! request, because that is the first point where the route and the account
//! are known: the edge sees only the encrypted carrier, and the enclave has no
//! trusted client address. A rejection is an ordinary application response, so
//! Transport V2 seals its status, `Retry-After` and body inside the reply.
//!
//! One [`Limiter`] is one purpose with one [`Policy`]. Its key is the account
//! the request acts on, whether or not the caller has proven it is theirs:
//!
//! - an authenticated route charges the verified caller through the
//!   [`enforce_user`] route layer;
//! - an unauthenticated route has no caller identity, so its handler charges
//!   the account the request targets, once resolved, with [`Limiter::check`].
//!
//! Key only on identities the server resolved: an account row, or a value
//! bound to one. A bucket per caller-chosen string would let one caller fill
//! the table and shut out every new key.
//!
//! State is per process. It resets on restart and each replica has its own
//! allowance, so this is a guardrail, not a global quota.
use crate::{models::users::User, ApiError};
use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
    Extension,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

const DEFAULT_MAX_KEYS: usize = 10_000;

const INFERENCE_WINDOW: Duration = Duration::from_secs(60);
const DEFAULT_INFERENCE_RPM: usize = 100;
const DEFAULT_INFERENCE_COOLDOWN_SECONDS: usize = 60;

const PASSWORD_RESET_REQUESTS_PER_HOUR: usize = 5;
const PASSWORD_RESET_ATTEMPTS_PER_REQUEST: usize = 5;
/// One reset request lives for 24 hours, so its attempt budget must not refill
/// inside that time.
const PASSWORD_RESET_REQUEST_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);
/// Reset limits are keyed by accounts anyone can name, so they hold more keys
/// than a limit only signed-in callers can reach.
const PASSWORD_RESET_MAX_KEYS: usize = 100_000;

const PASSWORD_RESET_REQUEST_POLICY: Policy = Policy {
    limit: PASSWORD_RESET_REQUESTS_PER_HOUR,
    window: Duration::from_secs(60 * 60),
    cooldown: None,
    max_keys: PASSWORD_RESET_MAX_KEYS,
};

const PASSWORD_RESET_ATTEMPT_POLICY: Policy = Policy {
    limit: PASSWORD_RESET_ATTEMPTS_PER_REQUEST,
    window: PASSWORD_RESET_REQUEST_LIFETIME,
    cooldown: None,
    // One key per reset request: at most the request limit per account per
    // hour, for a day.
    max_keys: PASSWORD_RESET_MAX_KEYS,
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Policy {
    /// Admissions allowed inside one rolling window.
    pub(crate) limit: usize,
    pub(crate) window: Duration,
    /// How long a key stays blocked after it reaches the limit. `None` blocks
    /// only until the oldest admission leaves the window.
    pub(crate) cooldown: Option<Duration>,
    /// Keys retained at once. A live key is never evicted to admit a new one.
    pub(crate) max_keys: usize,
}

/// What a limit is charged to. Deliberately not `Debug`: a scoped key is
/// derived from request secrets and must not reach a log.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) struct Key(KeyKind);

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum KeyKind {
    Account(Uuid),
    Scoped([u8; 32]),
}

impl Key {
    /// A user or platform account: the verified caller, or the account an
    /// unauthenticated request names.
    pub(crate) fn account(account: Uuid) -> Self {
        Self(KeyKind::Account(account))
    }

    /// An account narrowed by further facts the server matched to it, such as
    /// the secret of one of its reset requests. Parts are length-prefixed, so
    /// different splits of the same bytes are different keys.
    pub(crate) fn scoped(account: Uuid, parts: &[&[u8]]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"opensecret/rate-limit/scoped-key/v1\0");
        hasher.update(account.as_bytes());
        for part in parts {
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part);
        }
        Self(KeyKind::Scoped(hasher.finalize().into()))
    }
}

impl From<Uuid> for Key {
    fn from(account: Uuid) -> Self {
        Self::account(account)
    }
}

struct Bucket {
    admitted: VecDeque<Instant>,
    blocked_until: Option<Instant>,
    retain_until: Instant,
}

pub(crate) struct Limiter {
    name: &'static str,
    policy: Policy,
    limited: fn(Duration) -> ApiError,
    buckets: Mutex<HashMap<Key, Bucket>>,
}

impl Limiter {
    pub(crate) fn new(name: &'static str, policy: Policy) -> Self {
        assert!(
            policy.limit > 0 && policy.max_keys > 0 && !policy.window.is_zero(),
            "rate limit policy {name} must admit at least one request"
        );
        Self {
            name,
            policy,
            limited: |retry_after| ApiError::RateLimited { retry_after },
            buckets: Mutex::default(),
        }
    }

    /// Replaces the generic 429 with the route family's own error shape.
    fn with_limited_response(mut self, limited: fn(Duration) -> ApiError) -> Self {
        self.limited = limited;
        self
    }

    /// Charges one admission to `key`, or says how long the key is blocked.
    pub(crate) fn check(&self, key: impl Into<Key>) -> Result<(), ApiError> {
        let mut buckets = self
            .buckets
            .lock()
            .map_err(|_| ApiError::InternalServerError)?;
        // Sample time under the lock so concurrent admissions stay ordered.
        self.admit(&mut buckets, key.into(), Instant::now())
    }

    fn admit(
        &self,
        buckets: &mut HashMap<Key, Bucket>,
        key: Key,
        now: Instant,
    ) -> Result<(), ApiError> {
        let Policy {
            limit,
            window,
            cooldown,
            max_keys,
        } = self.policy;
        if !buckets.contains_key(&key) && buckets.len() >= max_keys {
            // Reclaim only expired state. Evicting a live bucket would let new
            // keys reset another key's limit or active block.
            buckets.retain(|_, bucket| bucket.retain_until > now);
            if buckets.len() >= max_keys {
                return Err(ApiError::ServiceUnavailable);
            }
        }
        let bucket = buckets.entry(key).or_insert_with(|| Bucket {
            admitted: VecDeque::new(),
            blocked_until: None,
            retain_until: now,
        });
        if let Some(until) = bucket.blocked_until {
            if until > now {
                return Err(self.reject(until.duration_since(now)));
            }
            bucket.blocked_until = None;
        }
        while bucket
            .admitted
            .front()
            .is_some_and(|time| now.duration_since(*time) >= window)
        {
            bucket.admitted.pop_front();
        }
        if bucket.admitted.len() >= limit {
            // Rejections never move this deadline, so hammering a blocked key
            // cannot extend its block.
            let until = match cooldown {
                Some(cooldown) => now + cooldown,
                None => bucket
                    .admitted
                    .front()
                    .map_or(now + window, |oldest| *oldest + window),
            };
            bucket.blocked_until = Some(until);
            bucket.retain_until = bucket.retain_until.max(until);
            // The limiter name only: a key may identify an account or derive
            // from a secret.
            tracing::info!(limiter = self.name, "rate limit reached");
            return Err(self.reject(until.duration_since(now)));
        }
        bucket.admitted.push_back(now);
        bucket.retain_until = bucket.retain_until.max(now + window);
        Ok(())
    }

    fn reject(&self, remaining: Duration) -> ApiError {
        // Integer Retry-After must never tell the caller to retry before expiry.
        let seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0);
        (self.limited)(Duration::from_secs(seconds.max(1)))
    }
}

/// Every limit this service enforces. Add a field and its policy here, then
/// mount [`enforce_user`] on the route or call [`Limiter::check`] in the handler.
#[derive(Clone)]
pub(crate) struct RateLimits {
    /// Authenticated inference POSTs. One account shares one bucket across
    /// every inference route, keys and sessions.
    pub(crate) inference: Arc<Limiter>,
    /// Password-reset emails, per targeted user account. This also caps how
    /// many reset codes can be live for one account.
    pub(crate) password_reset_request: Arc<Limiter>,
    /// Confirmation attempts against one user reset request, keyed by the
    /// account and that request's secret. Only the party that started a
    /// request holds its secret, so nobody else can spend its budget.
    pub(crate) password_reset_attempt: Arc<Limiter>,
    /// The same two limits for platform (developer) accounts.
    pub(crate) platform_password_reset_request: Arc<Limiter>,
    pub(crate) platform_password_reset_attempt: Arc<Limiter>,
}

impl RateLimits {
    pub(crate) fn from_env() -> Result<Self, String> {
        let rpm = setting("INFERENCE_RATE_LIMIT_RPM", DEFAULT_INFERENCE_RPM, 1_000)?;
        let cooldown = setting(
            "INFERENCE_RATE_LIMIT_COOLDOWN_SECONDS",
            DEFAULT_INFERENCE_COOLDOWN_SECONDS,
            3_600,
        )?;
        Ok(Self::with_inference(
            rpm,
            Duration::from_secs(cooldown as u64),
        ))
    }

    fn with_inference(rpm: usize, cooldown: Duration) -> Self {
        Self {
            inference: Arc::new(
                Limiter::new(
                    "inference",
                    Policy {
                        limit: rpm,
                        window: INFERENCE_WINDOW,
                        cooldown: Some(cooldown),
                        max_keys: DEFAULT_MAX_KEYS,
                    },
                )
                .with_limited_response(|retry_after| {
                    ApiError::InferenceRateLimited { retry_after }
                }),
            ),
            password_reset_request: Arc::new(Limiter::new(
                "password_reset_request",
                PASSWORD_RESET_REQUEST_POLICY,
            )),
            password_reset_attempt: Arc::new(Limiter::new(
                "password_reset_attempt",
                PASSWORD_RESET_ATTEMPT_POLICY,
            )),
            platform_password_reset_request: Arc::new(Limiter::new(
                "platform_password_reset_request",
                PASSWORD_RESET_REQUEST_POLICY,
            )),
            platform_password_reset_attempt: Arc::new(Limiter::new(
                "platform_password_reset_attempt",
                PASSWORD_RESET_ATTEMPT_POLICY,
            )),
        }
    }
}

impl Default for RateLimits {
    fn default() -> Self {
        Self::with_inference(
            DEFAULT_INFERENCE_RPM,
            Duration::from_secs(DEFAULT_INFERENCE_COOLDOWN_SECONDS as u64),
        )
    }
}

fn setting(name: &str, default: usize, maximum: usize) -> Result<usize, String> {
    match std::env::var(name) {
        Ok(value) => parse_setting(&value, maximum)
            .ok_or_else(|| format!("{name} must be an integer between 1 and {maximum}")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(_) => Err(format!("{name} must be valid Unicode")),
    }
}

fn parse_setting(value: &str, maximum: usize) -> Option<usize> {
    value
        .parse::<usize>()
        .ok()
        .filter(|value| (1..=maximum).contains(value))
}

/// Route layer for authenticated routes: charges the verified caller. Mount it
/// inside authentication and transport/body decoding, so a request counts only
/// once it is well formed and attributed, and before the handler can persist
/// anything or contact a provider.
pub(crate) async fn enforce_user(
    State(limiter): State<Arc<Limiter>>,
    Extension(user): Extension<User>,
    request: Request,
    next: Next,
) -> Response {
    match limiter.check(user.uuid) {
        Ok(()) => next.run(request).await,
        Err(error) => error.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inference(rpm: usize, cooldown: Duration) -> Arc<Limiter> {
        RateLimits::with_inference(rpm, cooldown).inference
    }

    fn rolling(limit: usize, window: Duration, max_keys: usize) -> Limiter {
        Limiter::new(
            "test",
            Policy {
                limit,
                window,
                cooldown: None,
                max_keys,
            },
        )
    }

    fn check_at(limiter: &Limiter, key: impl Into<Key>, now: Instant) -> Result<(), ApiError> {
        limiter.admit(&mut limiter.buckets.lock().unwrap(), key.into(), now)
    }

    fn inference_retry_seconds(result: Result<(), ApiError>) -> u64 {
        match result {
            Err(ApiError::InferenceRateLimited { retry_after }) => retry_after.as_secs(),
            other => panic!("expected the inference rate limit, got {other:?}"),
        }
    }

    fn retry_seconds(result: Result<(), ApiError>) -> u64 {
        match result {
            Err(ApiError::RateLimited { retry_after }) => retry_after.as_secs(),
            other => panic!("expected the generic rate limit, got {other:?}"),
        }
    }

    #[test]
    fn inference_default_allows_100_then_cooldown_is_fixed_and_rounded_up() {
        let limiter = RateLimits::default().inference;
        let account = Uuid::new_v4();
        let now = Instant::now();
        for _ in 0..100 {
            check_at(&limiter, account, now).unwrap();
        }
        assert_eq!(
            inference_retry_seconds(check_at(&limiter, account, now)),
            60
        );
        assert_eq!(
            inference_retry_seconds(check_at(
                &limiter,
                account,
                now + Duration::from_millis(500)
            )),
            60
        );
        assert_eq!(
            inference_retry_seconds(check_at(&limiter, account, now + Duration::from_secs(59))),
            1
        );
        // Rejected requests did not extend the original deadline.
        check_at(&limiter, account, now + INFERENCE_WINDOW).unwrap();
    }

    #[test]
    fn rolling_window_does_not_reset_at_a_calendar_boundary() {
        let limiter = inference(2, INFERENCE_WINDOW);
        let account = Uuid::new_v4();
        let now = Instant::now();
        check_at(&limiter, account, now).unwrap();
        check_at(&limiter, account, now + Duration::from_secs(59)).unwrap();
        // Exactly 60 seconds removes just the first admission.
        check_at(&limiter, account, now + INFERENCE_WINDOW).unwrap();
        assert_eq!(
            inference_retry_seconds(check_at(&limiter, account, now + INFERENCE_WINDOW)),
            60
        );
    }

    #[test]
    fn without_a_cooldown_a_key_is_blocked_only_until_its_oldest_admission_expires() {
        let hour = Duration::from_secs(3_600);
        let limiter = rolling(2, hour, 16);
        let account = Uuid::new_v4();
        let now = Instant::now();
        check_at(&limiter, account, now).unwrap();
        check_at(&limiter, account, now + Duration::from_secs(600)).unwrap();
        assert_eq!(
            retry_seconds(check_at(&limiter, account, now + Duration::from_secs(900))),
            2_700
        );
        // Further rejections report the same deadline rather than moving it.
        assert_eq!(
            retry_seconds(check_at(
                &limiter,
                account,
                now + Duration::from_secs(3_599)
            )),
            1
        );
        // One slot frees when the first admission leaves the window; the
        // second admission still holds the other.
        check_at(&limiter, account, now + hour).unwrap();
        assert_eq!(retry_seconds(check_at(&limiter, account, now + hour)), 600);
    }

    #[test]
    fn keys_limiters_and_enclave_instances_are_independent() {
        let limits = RateLimits::with_inference(1, INFERENCE_WINDOW);
        let account = Uuid::new_v4();
        limits.inference.check(account).unwrap();
        assert_eq!(inference_retry_seconds(limits.inference.check(account)), 60);
        limits.inference.check(Uuid::new_v4()).unwrap();
        // The same account has a separate allowance for a separate purpose.
        limits
            .platform_password_reset_request
            .check(account)
            .unwrap();
        // Another process starts with its own state.
        RateLimits::default().inference.check(account).unwrap();
    }

    #[test]
    fn scoped_keys_separate_one_account_and_are_not_the_account_key() {
        let limiter = rolling(1, INFERENCE_WINDOW, 16);
        let account = Uuid::new_v4();
        let other_account = Uuid::new_v4();
        limiter.check(account).unwrap();
        limiter.check(Key::scoped(account, &[b"first"])).unwrap();
        limiter.check(Key::scoped(account, &[b"second"])).unwrap();
        limiter
            .check(Key::scoped(other_account, &[b"first"]))
            .unwrap();
        // Length prefixes keep different splits of the same bytes apart.
        limiter.check(Key::scoped(account, &[b"ab", b"c"])).unwrap();
        limiter.check(Key::scoped(account, &[b"a", b"bc"])).unwrap();
        limiter.check(Key::scoped(account, &[])).unwrap();

        assert_eq!(retry_seconds(limiter.check(account)), 60);
        assert_eq!(
            retry_seconds(limiter.check(Key::scoped(account, &[b"first"]))),
            60
        );
    }

    #[test]
    fn capacity_reclaims_expired_state_without_evicting_live_blocks() {
        let mut limiter = Limiter::new(
            "test",
            Policy {
                limit: 1,
                window: INFERENCE_WINDOW,
                cooldown: Some(INFERENCE_WINDOW),
                max_keys: 1,
            },
        );
        limiter.limited = |retry_after| ApiError::InferenceRateLimited { retry_after };
        let account = Uuid::new_v4();
        let another = Uuid::new_v4();
        let now = Instant::now();
        check_at(&limiter, account, now).unwrap();
        assert_eq!(
            inference_retry_seconds(check_at(&limiter, account, now + Duration::from_secs(30))),
            60
        );
        assert!(matches!(
            check_at(&limiter, another, now + INFERENCE_WINDOW),
            Err(ApiError::ServiceUnavailable)
        ));
        assert_eq!(
            inference_retry_seconds(check_at(&limiter, account, now + INFERENCE_WINDOW)),
            30
        );
        check_at(&limiter, another, now + Duration::from_secs(90)).unwrap();
        assert_eq!(limiter.buckets.lock().unwrap().len(), 1);
    }

    #[test]
    fn concurrent_requests_cannot_over_admit() {
        let limiter = RateLimits::default().inference;
        let account = Uuid::new_v4();
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let limiter = limiter.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    (0..10).filter(|_| limiter.check(account).is_ok()).count()
                })
            })
            .collect();
        assert_eq!(
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .sum::<usize>(),
            100
        );
        assert_eq!(
            limiter.buckets.lock().unwrap()[&Key::account(account)]
                .admitted
                .len(),
            100
        );
    }

    #[test]
    fn invalid_configuration_cannot_disable_or_unbound_the_limiter() {
        for invalid in ["", "0", "-1", "1001", "garbage", "999999999999999999999999"] {
            assert_eq!(parse_setting(invalid, 1000), None);
        }
        assert_eq!(parse_setting("100", 1000), Some(100));
        assert_eq!(parse_setting("1000", 1000), Some(1000));
    }

    #[test]
    fn password_reset_budgets_match_the_request_lifetime() {
        let limits = RateLimits::default();
        let account = Uuid::new_v4();
        let now = Instant::now();

        for (requests, attempts) in [
            (
                &limits.password_reset_request,
                &limits.password_reset_attempt,
            ),
            (
                &limits.platform_password_reset_request,
                &limits.platform_password_reset_attempt,
            ),
        ] {
            for _ in 0..5 {
                check_at(requests, account, now).unwrap();
            }
            assert_eq!(retry_seconds(check_at(requests, account, now)), 3_600);

            // A request's attempts never refill while it can still be
            // completed.
            let request = Key::scoped(account, &[b"reset-secret-hash"]);
            for _ in 0..5 {
                check_at(attempts, request, now).unwrap();
            }
            let last_moment = now + PASSWORD_RESET_REQUEST_LIFETIME - Duration::from_secs(1);
            assert_eq!(retry_seconds(check_at(attempts, request, last_moment)), 1);
        }
    }
}
