//! Retry + circuit breaker for outbound rate-limited calls (RL-U3-13 / LM-66,
//! ADR-0008 enforcement point).
//!
//! Wraps an async fallible operation that may fail with `RateLimited` (HTTP 429)
//! or `Overloaded` (HTTP 529). On those errors the limiter retries with
//! exponential backoff (base 1s, doubling, capped at 60s, ±20% jitter). If
//! `breaker_threshold` rate-limit failures land within `breaker_window` the
//! circuit opens for `breaker_open_duration` and the next call fails fast with
//! `CircuitOpen` instead of hitting the upstream. After the cool-down a single
//! probe is admitted in `HalfOpen`; a successful probe closes the circuit, a
//! failed probe re-opens it.
//!
//! `Permanent` errors propagate immediately and do not affect the circuit.
//!
//! Time and randomness are abstracted via the `Clock` and `Jitter` traits so
//! tests can advance time deterministically without `tokio::time::pause`
//! (which would also pause the actual sleep, defeating the purpose).

// Production wiring (`execute_task` tool) lands in U4; the public surface is
// fully covered by tests and is intentionally unused inside the daemon today.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use rand::Rng;
use tokio::time::Instant;

#[derive(Clone, Debug)]
pub struct RateLimitConfig {
    pub base_backoff: Duration,
    pub max_backoff: Duration,
    /// `0.20` = ±20% uniformly distributed.
    pub jitter_pct: f64,
    pub max_retries: usize,
    pub breaker_threshold: usize,
    pub breaker_window: Duration,
    pub breaker_open_duration: Duration,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            base_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            jitter_pct: 0.20,
            max_retries: 8,
            breaker_threshold: 5,
            breaker_window: Duration::from_secs(60),
            breaker_open_duration: Duration::from_secs(120),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

impl fmt::Display for CircuitState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CircuitState::Closed => f.write_str("closed"),
            CircuitState::Open => f.write_str("open"),
            CircuitState::HalfOpen => f.write_str("half_open"),
        }
    }
}

#[derive(Debug)]
pub enum RateLimitError {
    RateLimited,
    Overloaded,
    CircuitOpen,
    Permanent(String),
}

impl fmt::Display for RateLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RateLimitError::RateLimited => f.write_str("rate limited (429)"),
            RateLimitError::Overloaded => f.write_str("upstream overloaded (529)"),
            RateLimitError::CircuitOpen => f.write_str("circuit open — fail fast"),
            RateLimitError::Permanent(m) => write!(f, "permanent: {m}"),
        }
    }
}

impl std::error::Error for RateLimitError {}

#[derive(Debug, Clone)]
pub struct RunMetrics {
    pub retry_count: usize,
    pub total_backoff: Duration,
    pub final_state: CircuitState,
}

pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

pub trait Jitter: Send + Sync {
    /// Return a uniform sample in `[-1.0, 1.0]`.
    fn sample(&self) -> f64;
}

pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

pub struct ThreadRng;
impl Jitter for ThreadRng {
    fn sample(&self) -> f64 {
        rand::thread_rng().gen_range(-1.0..=1.0)
    }
}

pub struct RateLimiter {
    config: RateLimitConfig,
    state: Mutex<State>,
    clock: Box<dyn Clock>,
    jitter: Box<dyn Jitter>,
}

struct State {
    failures: VecDeque<Instant>,
    circuit: CircuitState,
    open_since: Option<Instant>,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self::with_deps(config, Box::new(SystemClock), Box::new(ThreadRng))
    }

    pub fn with_deps(
        config: RateLimitConfig,
        clock: Box<dyn Clock>,
        jitter: Box<dyn Jitter>,
    ) -> Self {
        Self {
            config,
            state: Mutex::new(State {
                failures: VecDeque::new(),
                circuit: CircuitState::Closed,
                open_since: None,
            }),
            clock,
            jitter,
        }
    }

    pub fn circuit_state(&self) -> CircuitState {
        self.state.lock().unwrap().circuit
    }

    pub fn config(&self) -> &RateLimitConfig {
        &self.config
    }

    /// Run `op` under the retry + circuit policy. Returns the operation
    /// result alongside metrics describing the run.
    ///
    /// `sleeper` is invoked between retries with the computed backoff. In
    /// production this is `tokio::time::sleep`; tests pass a mock that
    /// advances a `MockClock` instead.
    pub async fn execute<F, Fut, T, S, SF>(
        &self,
        mut op: F,
        mut sleeper: S,
    ) -> (Result<T, RateLimitError>, RunMetrics)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, RateLimitError>>,
        S: FnMut(Duration) -> SF,
        SF: Future<Output = ()>,
    {
        let mut retry_count = 0usize;
        let mut total_backoff = Duration::ZERO;

        loop {
            // 1. Probe circuit.
            if self.is_circuit_blocking() {
                return (
                    Err(RateLimitError::CircuitOpen),
                    RunMetrics {
                        retry_count,
                        total_backoff,
                        final_state: CircuitState::Open,
                    },
                );
            }

            // 2. Run.
            match op().await {
                Ok(v) => {
                    self.record_success();
                    return (
                        Ok(v),
                        RunMetrics {
                            retry_count,
                            total_backoff,
                            final_state: self.circuit_state(),
                        },
                    );
                }
                Err(RateLimitError::Permanent(m)) => {
                    return (
                        Err(RateLimitError::Permanent(m)),
                        RunMetrics {
                            retry_count,
                            total_backoff,
                            final_state: self.circuit_state(),
                        },
                    );
                }
                Err(e @ (RateLimitError::RateLimited | RateLimitError::Overloaded)) => {
                    self.record_failure();
                    if retry_count >= self.config.max_retries {
                        return (
                            Err(e),
                            RunMetrics {
                                retry_count,
                                total_backoff,
                                final_state: self.circuit_state(),
                            },
                        );
                    }
                    // If recording the failure tripped the breaker, fail
                    // fast without burning another backoff cycle.
                    if self.circuit_state() == CircuitState::Open {
                        return (
                            Err(RateLimitError::CircuitOpen),
                            RunMetrics {
                                retry_count,
                                total_backoff,
                                final_state: CircuitState::Open,
                            },
                        );
                    }
                    let backoff = self.compute_backoff(retry_count);
                    total_backoff += backoff;
                    retry_count += 1;
                    sleeper(backoff).await;
                }
                Err(RateLimitError::CircuitOpen) => {
                    // The op shouldn't return CircuitOpen itself; treat it
                    // as a permanent surprise rather than retrying.
                    return (
                        Err(RateLimitError::Permanent(
                            "operation returned CircuitOpen".to_string(),
                        )),
                        RunMetrics {
                            retry_count,
                            total_backoff,
                            final_state: self.circuit_state(),
                        },
                    );
                }
            }
        }
    }

    /// Computes the backoff for a given attempt index (0-based) without
    /// consuming any retry budget. Public for diagnostics + tests.
    pub fn compute_backoff(&self, attempt: usize) -> Duration {
        let exp = u32::try_from(attempt).unwrap_or(u32::MAX);
        let factor: u64 = 1u64 << exp.min(63);
        let scaled = self
            .config
            .base_backoff
            .checked_mul(factor.try_into().unwrap_or(u32::MAX))
            .unwrap_or(self.config.max_backoff);
        let capped = scaled.min(self.config.max_backoff);
        let jitter_range = capped.as_secs_f64() * self.config.jitter_pct;
        let jitter = self.jitter.sample() * jitter_range;
        let total = (capped.as_secs_f64() + jitter).max(0.0);
        Duration::from_secs_f64(total)
    }

    fn is_circuit_blocking(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.circuit == CircuitState::Open {
            let elapsed = state
                .open_since
                .map(|s| self.clock.now().saturating_duration_since(s))
                .unwrap_or(Duration::ZERO);
            if elapsed >= self.config.breaker_open_duration {
                state.circuit = CircuitState::HalfOpen;
                state.open_since = None;
                return false;
            }
            return true;
        }
        false
    }

    fn record_failure(&self) {
        let mut state = self.state.lock().unwrap();
        let now = self.clock.now();
        let window = self.config.breaker_window;
        while let Some(&front) = state.failures.front() {
            if now.saturating_duration_since(front) > window {
                state.failures.pop_front();
            } else {
                break;
            }
        }
        state.failures.push_back(now);

        if state.circuit == CircuitState::HalfOpen {
            state.circuit = CircuitState::Open;
            state.open_since = Some(now);
            state.failures.clear();
            tracing::warn!("rate_limiter circuit re-opened after half-open probe failure");
            return;
        }

        if state.failures.len() >= self.config.breaker_threshold {
            state.circuit = CircuitState::Open;
            state.open_since = Some(now);
            let n = state.failures.len();
            state.failures.clear();
            tracing::warn!(
                threshold = self.config.breaker_threshold,
                observed = n,
                "rate_limiter circuit opened"
            );
        }
    }

    fn record_success(&self) {
        let mut state = self.state.lock().unwrap();
        state.failures.clear();
        if state.circuit == CircuitState::HalfOpen {
            state.circuit = CircuitState::Closed;
            state.open_since = None;
            tracing::info!("rate_limiter circuit closed after successful probe");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Mock clock: tests advance time without sleeping the runtime.
    struct MockClock {
        current: Mutex<Instant>,
    }
    impl MockClock {
        fn new(start: Instant) -> Arc<Self> {
            Arc::new(Self {
                current: Mutex::new(start),
            })
        }
        fn advance(&self, by: Duration) {
            let mut c = self.current.lock().unwrap();
            *c += by;
        }
    }
    impl Clock for Arc<MockClock> {
        fn now(&self) -> Instant {
            *self.current.lock().unwrap()
        }
    }

    /// Deterministic jitter (always +1.0) so tests can compare exact backoff
    /// upper bounds.
    struct UpperJitter;
    impl Jitter for UpperJitter {
        fn sample(&self) -> f64 {
            1.0
        }
    }

    /// Zero jitter — backoff is exactly the exponential value.
    struct NoJitter;
    impl Jitter for NoJitter {
        fn sample(&self) -> f64 {
            0.0
        }
    }

    fn fast_config() -> RateLimitConfig {
        RateLimitConfig {
            base_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(640),
            jitter_pct: 0.0,
            max_retries: 5,
            breaker_threshold: 5,
            breaker_window: Duration::from_secs(60),
            breaker_open_duration: Duration::from_secs(120),
        }
    }

    fn limiter_with(config: RateLimitConfig, clock: Arc<MockClock>) -> RateLimiter {
        RateLimiter::with_deps(config, Box::new(clock), Box::new(NoJitter))
    }

    fn mock_sleeper(
        clock: Arc<MockClock>,
    ) -> impl FnMut(Duration) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> {
        move |d| {
            let c = clock.clone();
            Box::pin(async move {
                c.advance(d);
            })
        }
    }

    #[tokio::test]
    async fn succeeds_on_first_try_no_retries() {
        let clock = MockClock::new(Instant::now());
        let limiter = limiter_with(fast_config(), clock.clone());
        let (result, metrics) = limiter
            .execute(
                || async { Ok::<_, RateLimitError>(42) },
                mock_sleeper(clock.clone()),
            )
            .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(metrics.retry_count, 0);
        assert_eq!(metrics.total_backoff, Duration::ZERO);
        assert_eq!(metrics.final_state, CircuitState::Closed);
    }

    #[tokio::test]
    async fn retries_on_429_with_exponential_backoff() {
        let clock = MockClock::new(Instant::now());
        let cfg = fast_config();
        let limiter = limiter_with(cfg.clone(), clock.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_inner = calls.clone();
        let (result, metrics) = limiter
            .execute(
                move || {
                    let n = calls_inner.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if n < 3 {
                            Err(RateLimitError::RateLimited)
                        } else {
                            Ok::<_, RateLimitError>("ok")
                        }
                    }
                },
                mock_sleeper(clock.clone()),
            )
            .await;
        assert_eq!(result.unwrap(), "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(metrics.retry_count, 3);
        // 10ms + 20ms + 40ms = 70ms with no jitter.
        assert_eq!(metrics.total_backoff, Duration::from_millis(70));
    }

    #[tokio::test]
    async fn backoff_is_capped_at_max_backoff() {
        let cfg = RateLimitConfig {
            base_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            jitter_pct: 0.0,
            ..fast_config()
        };
        let clock = MockClock::new(Instant::now());
        let limiter = limiter_with(cfg, clock);
        // attempt 0 → 1s, 1 → 2s, ..., 5 → 32s, 6 → 60s (capped from 64s),
        // 10 → 60s (massively capped).
        assert_eq!(limiter.compute_backoff(0), Duration::from_secs(1));
        assert_eq!(limiter.compute_backoff(5), Duration::from_secs(32));
        assert_eq!(limiter.compute_backoff(6), Duration::from_secs(60));
        assert_eq!(limiter.compute_backoff(10), Duration::from_secs(60));
        assert_eq!(limiter.compute_backoff(63), Duration::from_secs(60));
        assert_eq!(limiter.compute_backoff(usize::MAX), Duration::from_secs(60));
    }

    #[tokio::test]
    async fn jitter_stays_within_pct_band() {
        let cfg = RateLimitConfig {
            base_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            jitter_pct: 0.20,
            ..fast_config()
        };
        let clock = MockClock::new(Instant::now());
        let limiter = RateLimiter::with_deps(cfg, Box::new(clock), Box::new(UpperJitter));
        // Upper-jitter pushes backoff to base + 20%.
        assert_eq!(limiter.compute_backoff(0), Duration::from_secs_f64(1.20));
        assert_eq!(limiter.compute_backoff(2), Duration::from_secs_f64(4.80));

        let cfg_low = RateLimitConfig {
            base_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            jitter_pct: 0.20,
            ..fast_config()
        };
        struct LowerJitter;
        impl Jitter for LowerJitter {
            fn sample(&self) -> f64 {
                -1.0
            }
        }
        let clock = MockClock::new(Instant::now());
        let limiter_low = RateLimiter::with_deps(cfg_low, Box::new(clock), Box::new(LowerJitter));
        assert_eq!(
            limiter_low.compute_backoff(0),
            Duration::from_secs_f64(0.80)
        );
    }

    #[tokio::test]
    async fn circuit_opens_after_threshold_consecutive_failures() {
        let clock = MockClock::new(Instant::now());
        let cfg = RateLimitConfig {
            max_retries: 100, // never give up by retry budget
            ..fast_config()
        };
        let limiter = limiter_with(cfg, clock.clone());
        let (result, metrics) = limiter
            .execute(
                || async { Err::<(), _>(RateLimitError::RateLimited) },
                mock_sleeper(clock.clone()),
            )
            .await;
        assert!(matches!(result, Err(RateLimitError::CircuitOpen)));
        assert_eq!(metrics.final_state, CircuitState::Open);
        assert_eq!(limiter.circuit_state(), CircuitState::Open);
        // 5 failures = breaker_threshold; the 5th failure trips it.
        assert!(metrics.retry_count >= 4 && metrics.retry_count < 100);
    }

    #[tokio::test]
    async fn circuit_open_blocks_subsequent_calls_fast() {
        let clock = MockClock::new(Instant::now());
        let cfg = RateLimitConfig {
            max_retries: 100,
            ..fast_config()
        };
        let limiter = limiter_with(cfg, clock.clone());
        let _ = limiter
            .execute(
                || async { Err::<(), _>(RateLimitError::RateLimited) },
                mock_sleeper(clock.clone()),
            )
            .await;
        assert_eq!(limiter.circuit_state(), CircuitState::Open);

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_inner = calls.clone();
        let (result, metrics) = limiter
            .execute(
                move || {
                    calls_inner.fetch_add(1, Ordering::SeqCst);
                    async { Ok::<_, RateLimitError>(()) }
                },
                mock_sleeper(clock.clone()),
            )
            .await;
        assert!(matches!(result, Err(RateLimitError::CircuitOpen)));
        // Op was never invoked because circuit was open.
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(metrics.retry_count, 0);
    }

    #[tokio::test]
    async fn circuit_transitions_to_half_open_after_cool_down() {
        let clock = MockClock::new(Instant::now());
        let cfg = RateLimitConfig {
            max_retries: 100,
            breaker_open_duration: Duration::from_secs(120),
            ..fast_config()
        };
        let limiter = limiter_with(cfg, clock.clone());
        let _ = limiter
            .execute(
                || async { Err::<(), _>(RateLimitError::RateLimited) },
                mock_sleeper(clock.clone()),
            )
            .await;
        assert_eq!(limiter.circuit_state(), CircuitState::Open);

        // Advance past the cool-down.
        clock.advance(Duration::from_secs(121));

        let (result, metrics) = limiter
            .execute(
                || async { Ok::<_, RateLimitError>("probe") },
                mock_sleeper(clock.clone()),
            )
            .await;
        assert_eq!(result.unwrap(), "probe");
        // Successful probe → closed.
        assert_eq!(metrics.final_state, CircuitState::Closed);
        assert_eq!(limiter.circuit_state(), CircuitState::Closed);
    }

    #[tokio::test]
    async fn half_open_failure_re_opens_circuit() {
        let clock = MockClock::new(Instant::now());
        let cfg = RateLimitConfig {
            max_retries: 100,
            breaker_open_duration: Duration::from_secs(120),
            ..fast_config()
        };
        let limiter = limiter_with(cfg, clock.clone());
        let _ = limiter
            .execute(
                || async { Err::<(), _>(RateLimitError::RateLimited) },
                mock_sleeper(clock.clone()),
            )
            .await;
        clock.advance(Duration::from_secs(121));
        // Half-open probe fails → circuit must re-open immediately.
        let (result, _metrics) = limiter
            .execute(
                || async { Err::<(), _>(RateLimitError::RateLimited) },
                mock_sleeper(clock.clone()),
            )
            .await;
        assert!(matches!(result, Err(RateLimitError::CircuitOpen)));
        assert_eq!(limiter.circuit_state(), CircuitState::Open);
    }

    #[tokio::test]
    async fn permanent_error_propagates_without_retry() {
        let clock = MockClock::new(Instant::now());
        let limiter = limiter_with(fast_config(), clock.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_inner = calls.clone();
        let (result, metrics) = limiter
            .execute(
                move || {
                    calls_inner.fetch_add(1, Ordering::SeqCst);
                    async { Err::<(), _>(RateLimitError::Permanent("bad input".into())) }
                },
                mock_sleeper(clock.clone()),
            )
            .await;
        assert!(matches!(result, Err(RateLimitError::Permanent(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(metrics.retry_count, 0);
        assert_eq!(metrics.final_state, CircuitState::Closed);
    }

    #[tokio::test]
    async fn overloaded_529_retries_like_429() {
        let clock = MockClock::new(Instant::now());
        let limiter = limiter_with(fast_config(), clock.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_inner = calls.clone();
        let (result, metrics) = limiter
            .execute(
                move || {
                    let n = calls_inner.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if n < 1 {
                            Err(RateLimitError::Overloaded)
                        } else {
                            Ok::<_, RateLimitError>("ok")
                        }
                    }
                },
                mock_sleeper(clock.clone()),
            )
            .await;
        assert_eq!(result.unwrap(), "ok");
        assert_eq!(metrics.retry_count, 1);
        assert_eq!(metrics.total_backoff, Duration::from_millis(10));
    }

    #[tokio::test]
    async fn failure_window_expires_old_failures() {
        let clock = MockClock::new(Instant::now());
        let cfg = RateLimitConfig {
            max_retries: 100,
            breaker_threshold: 3,
            breaker_window: Duration::from_secs(10),
            ..fast_config()
        };
        let limiter = limiter_with(cfg, clock.clone());
        // Record 2 failures, then advance past the window, then record 2 more.
        // Circuit should still be closed because the old 2 expired.
        for _ in 0..2 {
            limiter.record_failure();
        }
        clock.advance(Duration::from_secs(11));
        for _ in 0..2 {
            limiter.record_failure();
        }
        assert_eq!(limiter.circuit_state(), CircuitState::Closed);
        // One more inside the window crosses the threshold.
        limiter.record_failure();
        assert_eq!(limiter.circuit_state(), CircuitState::Open);
    }

    #[tokio::test]
    async fn max_retries_returns_last_error_with_metrics() {
        let clock = MockClock::new(Instant::now());
        let cfg = RateLimitConfig {
            max_retries: 2,
            // Disable circuit so we exit via retry budget, not breaker.
            breaker_threshold: 999,
            ..fast_config()
        };
        let limiter = limiter_with(cfg, clock.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_inner = calls.clone();
        let (result, metrics) = limiter
            .execute(
                move || {
                    calls_inner.fetch_add(1, Ordering::SeqCst);
                    async { Err::<(), _>(RateLimitError::RateLimited) }
                },
                mock_sleeper(clock.clone()),
            )
            .await;
        assert!(matches!(result, Err(RateLimitError::RateLimited)));
        // 1 initial + 2 retries = 3 total invocations.
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(metrics.retry_count, 2);
        // 10ms + 20ms = 30ms.
        assert_eq!(metrics.total_backoff, Duration::from_millis(30));
    }

    #[tokio::test]
    async fn metrics_total_backoff_is_zero_on_first_success() {
        let clock = MockClock::new(Instant::now());
        let limiter = limiter_with(fast_config(), clock.clone());
        let (_, metrics) = limiter
            .execute(
                || async { Ok::<_, RateLimitError>(()) },
                mock_sleeper(clock.clone()),
            )
            .await;
        assert_eq!(metrics.total_backoff, Duration::ZERO);
        assert_eq!(metrics.retry_count, 0);
    }
}
