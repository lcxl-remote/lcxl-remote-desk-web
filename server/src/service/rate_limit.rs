//! Bounded in-process authentication rate limiting for the standalone server.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lru::LruCache;
use opentelemetry::{
    KeyValue, global,
    metrics::{Counter as MetricCounter, Gauge},
};

use super::client_ip::NetworkKey;

pub const MAX_BUCKETS_ENV: &str = "LRD_AUTH_RATE_LIMIT_MAX_BUCKETS";
pub const DEFAULT_MAX_BUCKETS: usize = 65_536;
pub const SECURITY_SMALL_CAPACITY: usize = 4_096;
const CAPACITY_SWEEP_INTERVAL_MS: u64 = 1_000;

pub const LOGIN_WINDOW_SEC: u64 = 900;
pub const LOGIN_MAX_FAILURES: u64 = 20;
pub const LOGIN_LOCK_BASE_SEC: u64 = 60;
pub const LOGIN_LOCK_MAX_SEC: u64 = 3_600;

pub const BOOTSTRAP_WINDOW_SEC: u64 = 600;
pub const BOOTSTRAP_MAX_FAILURES: u64 = 20;
pub const PROBE_WINDOW_SEC: u64 = 600;
pub const PROBE_MAX_ATTEMPTS: u64 = 60;
pub const REDEEM_WINDOW_SEC: u64 = 60;
pub const REDEEM_MAX_ATTEMPTS: u64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginFailureResult {
    Recorded,
    Locked { retry_after_sec: u64 },
    UntrackedCapacity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapAttempt {
    Allowed,
    Invalid,
    Limited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaDecision {
    Allowed,
    Limited,
}

trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }
}

#[derive(Debug, Clone)]
struct Counter {
    count: u64,
    expires_at_ms: u64,
}

#[derive(Debug, Clone)]
struct ActiveLock {
    failure: Counter,
    lock_until_ms: u64,
}

struct State {
    login_evictable: LruCache<NetworkKey, Counter>,
    login_active_locks: HashMap<NetworkKey, ActiveLock>,
    bootstrap: HashMap<NetworkKey, Counter>,
    probe: HashMap<NetworkKey, Counter>,
    redeem: HashMap<NetworkKey, Counter>,
    last_capacity_warn_ms: HashMap<&'static str, u64>,
    last_sweep_ms: HashMap<&'static str, u64>,
    #[cfg(test)]
    sweep_counts: HashMap<&'static str, u64>,
}

pub struct AuthRateLimiter {
    state: Mutex<State>,
    clock: Arc<dyn Clock>,
    metrics: AuthRateLimitMetrics,
    login_capacity: usize,
    bootstrap_capacity: usize,
    probe_capacity: usize,
    redeem_capacity: usize,
}

struct AuthRateLimitMetrics {
    evictions: MetricCounter<u64>,
    capacity_exhausted: MetricCounter<u64>,
    active_locks: Gauge<u64>,
    active_entries: Gauge<u64>,
}

impl AuthRateLimitMetrics {
    fn new() -> Self {
        let meter = global::meter("lcxl-remote-desk-server");
        Self {
            evictions: meter
                .u64_counter("auth_rate_limit_evictions_total")
                .with_description("Evictable authentication rate-limit entries removed")
                .build(),
            capacity_exhausted: meter
                .u64_counter("auth_rate_limit_capacity_exhausted_total")
                .with_description("Authentication rate-limit capacity exhaustion events")
                .build(),
            active_locks: meter
                .u64_gauge("auth_rate_limit_active_locks")
                .with_description("Currently active account-login locks")
                .build(),
            active_entries: meter
                .u64_gauge("auth_rate_limit_active_entries")
                .with_description("Currently active authentication rate-limit entries")
                .build(),
        }
    }

    fn record_eviction(&self, kind: &'static str) {
        self.evictions.add(1, &[KeyValue::new("kind", kind)]);
    }

    fn record_capacity_exhausted(&self, kind: &'static str, policy: &'static str) {
        self.capacity_exhausted.add(
            1,
            &[KeyValue::new("kind", kind), KeyValue::new("policy", policy)],
        );
    }

    fn record_state(&self, state: &State) {
        self.active_locks
            .record(state.login_active_locks.len() as u64, &[]);
        for (kind, value) in [
            ("login", state.login_evictable.len()),
            ("bootstrap", state.bootstrap.len()),
            ("probe", state.probe.len()),
            ("redeem", state.redeem.len()),
        ] {
            self.active_entries
                .record(value as u64, &[KeyValue::new("kind", kind)]);
        }
    }
}

impl AuthRateLimiter {
    pub fn from_env() -> Result<Self, String> {
        let capacity = match std::env::var(MAX_BUCKETS_ENV) {
            Ok(value) => parse_capacity(Some(&value))?,
            Err(std::env::VarError::NotPresent) => parse_capacity(None)?,
            Err(error) => return Err(format!("failed to read {MAX_BUCKETS_ENV}: {error}")),
        };
        Ok(Self::new(capacity))
    }

    pub fn new(login_and_redeem_capacity: usize) -> Self {
        Self::with_clock_and_capacities(
            Arc::new(SystemClock::default()),
            login_and_redeem_capacity,
            SECURITY_SMALL_CAPACITY,
            SECURITY_SMALL_CAPACITY,
            login_and_redeem_capacity,
        )
    }

    fn with_clock_and_capacities(
        clock: Arc<dyn Clock>,
        login_capacity: usize,
        bootstrap_capacity: usize,
        probe_capacity: usize,
        redeem_capacity: usize,
    ) -> Self {
        assert!(login_capacity > 0);
        assert!(bootstrap_capacity > 0);
        assert!(probe_capacity > 0);
        assert!(redeem_capacity > 0);
        Self {
            state: Mutex::new(State {
                login_evictable: LruCache::new(NonZeroUsize::new(login_capacity).unwrap()),
                login_active_locks: HashMap::new(),
                bootstrap: HashMap::new(),
                probe: HashMap::new(),
                redeem: HashMap::new(),
                last_capacity_warn_ms: HashMap::new(),
                last_sweep_ms: HashMap::new(),
                #[cfg(test)]
                sweep_counts: HashMap::new(),
            }),
            clock,
            metrics: AuthRateLimitMetrics::new(),
            login_capacity,
            bootstrap_capacity,
            probe_capacity,
            redeem_capacity,
        }
    }

    pub fn login_lock_ttl(&self, key: &NetworkKey) -> Option<u64> {
        let now = self.clock.now_ms();
        let mut state = self.lock_state();
        expire_login_lock_for_key(&mut state, key, now);
        let ttl = state
            .login_active_locks
            .get(key)
            .map(|active| ms_to_secs_ceil(active.lock_until_ms.saturating_sub(now)));
        self.metrics.record_state(&state);
        ttl
    }

    pub fn record_login_failure(&self, key: NetworkKey) -> LoginFailureResult {
        let now = self.clock.now_ms();
        let mut state = self.lock_state();
        expire_login_lock_for_key(&mut state, &key, now);
        if let Some(active) = state.login_active_locks.get(&key) {
            let result = LoginFailureResult::Locked {
                retry_after_sec: ms_to_secs_ceil(active.lock_until_ms.saturating_sub(now)),
            };
            self.metrics.record_state(&state);
            return result;
        }

        let mut counter = state.login_evictable.pop(&key).unwrap_or(Counter {
            count: 0,
            expires_at_ms: now + LOGIN_WINDOW_SEC * 1_000,
        });
        if now >= counter.expires_at_ms {
            counter.count = 0;
            counter.expires_at_ms = now + LOGIN_WINDOW_SEC * 1_000;
        }

        if counter.count == 0
            && state.login_active_locks.len() + state.login_evictable.len() >= self.login_capacity
        {
            if state.login_evictable.pop_lru().is_some() {
                self.metrics.record_eviction("login");
            } else {
                if sweep_due(&mut state, "login", now) {
                    expire_all_login_locks(&mut state, now);
                }
                if state.login_active_locks.len() + state.login_evictable.len()
                    >= self.login_capacity
                {
                    if state.login_evictable.pop_lru().is_some() {
                        self.metrics.record_eviction("login");
                    } else {
                        warn_capacity(&mut state, "login", now);
                        self.metrics
                            .record_capacity_exhausted("login", "availability_first");
                        self.metrics.record_state(&state);
                        return LoginFailureResult::UntrackedCapacity;
                    }
                }
            }
        }

        counter.count += 1;
        let result = if let Some(lock_ms) = lock_ttl_ms_for(counter.count) {
            state.login_active_locks.insert(
                key,
                ActiveLock {
                    failure: counter,
                    lock_until_ms: now + lock_ms,
                },
            );
            LoginFailureResult::Locked {
                retry_after_sec: ms_to_secs_ceil(lock_ms),
            }
        } else {
            state.login_evictable.put(key, counter);
            LoginFailureResult::Recorded
        };
        self.metrics.record_state(&state);
        result
    }

    pub fn clear_login(&self, key: &NetworkKey) {
        let mut state = self.lock_state();
        state.login_evictable.pop(key);
        state.login_active_locks.remove(key);
        self.metrics.record_state(&state);
    }

    pub fn evaluate_bootstrap_attempt(
        &self,
        key: NetworkKey,
        expected: &[u8],
        provided: &[u8],
    ) -> BootstrapAttempt {
        let now = self.clock.now_ms();
        let mut state = self.lock_state();
        expire_counter_for_key(&mut state.bootstrap, &key, now);

        if !state.bootstrap.contains_key(&key) && state.bootstrap.len() >= self.bootstrap_capacity {
            if sweep_due(&mut state, "bootstrap", now) {
                remove_expired(&mut state.bootstrap, now);
            }
            if state.bootstrap.len() >= self.bootstrap_capacity {
                warn_capacity(&mut state, "bootstrap", now);
                self.metrics
                    .record_capacity_exhausted("bootstrap", "fail_closed");
                self.metrics.record_state(&state);
                return BootstrapAttempt::Limited;
            }
        }

        if state
            .bootstrap
            .get(&key)
            .is_some_and(|counter| counter.count >= BOOTSTRAP_MAX_FAILURES)
        {
            self.metrics.record_state(&state);
            return BootstrapAttempt::Limited;
        }

        if constant_time_eq(expected, provided) {
            state.bootstrap.remove(&key);
            self.metrics.record_state(&state);
            return BootstrapAttempt::Allowed;
        }

        let counter = state.bootstrap.entry(key).or_insert(Counter {
            count: 0,
            expires_at_ms: now + BOOTSTRAP_WINDOW_SEC * 1_000,
        });
        counter.count += 1;
        let result = if counter.count >= BOOTSTRAP_MAX_FAILURES {
            BootstrapAttempt::Limited
        } else {
            BootstrapAttempt::Invalid
        };
        self.metrics.record_state(&state);
        result
    }

    pub fn consume_probe(&self, key: NetworkKey) -> QuotaDecision {
        self.consume_security_quota(
            key,
            SecurityQuotaKind::Probe,
            PROBE_WINDOW_SEC,
            PROBE_MAX_ATTEMPTS,
        )
    }

    pub fn consume_redeem(&self, key: NetworkKey) -> QuotaDecision {
        self.consume_security_quota(
            key,
            SecurityQuotaKind::Redeem,
            REDEEM_WINDOW_SEC,
            REDEEM_MAX_ATTEMPTS,
        )
    }

    fn consume_security_quota(
        &self,
        key: NetworkKey,
        kind: SecurityQuotaKind,
        window_sec: u64,
        max: u64,
    ) -> QuotaDecision {
        let now = self.clock.now_ms();
        let mut state = self.lock_state();
        let capacity = match kind {
            SecurityQuotaKind::Probe => self.probe_capacity,
            SecurityQuotaKind::Redeem => self.redeem_capacity,
        };
        let label = kind.label();
        {
            let map = match kind {
                SecurityQuotaKind::Probe => &mut state.probe,
                SecurityQuotaKind::Redeem => &mut state.redeem,
            };
            expire_counter_for_key(map, &key, now);
        }
        let needs_capacity = match kind {
            SecurityQuotaKind::Probe => {
                !state.probe.contains_key(&key) && state.probe.len() >= capacity
            }
            SecurityQuotaKind::Redeem => {
                !state.redeem.contains_key(&key) && state.redeem.len() >= capacity
            }
        };
        if needs_capacity && sweep_due(&mut state, label, now) {
            let map = match kind {
                SecurityQuotaKind::Probe => &mut state.probe,
                SecurityQuotaKind::Redeem => &mut state.redeem,
            };
            remove_expired(map, now);
        }
        let (decision, capacity_exhausted) = {
            let map = match kind {
                SecurityQuotaKind::Probe => &mut state.probe,
                SecurityQuotaKind::Redeem => &mut state.redeem,
            };
            if let Some(counter) = map.get_mut(&key) {
                if counter.count >= max {
                    (QuotaDecision::Limited, false)
                } else {
                    counter.count += 1;
                    (QuotaDecision::Allowed, false)
                }
            } else if map.len() >= capacity {
                (QuotaDecision::Limited, true)
            } else {
                map.insert(
                    key,
                    Counter {
                        count: 1,
                        expires_at_ms: now + window_sec * 1_000,
                    },
                );
                (QuotaDecision::Allowed, false)
            }
        };
        if capacity_exhausted {
            warn_capacity(&mut state, label, now);
            self.metrics.record_capacity_exhausted(label, "fail_closed");
        }
        self.metrics.record_state(&state);
        decision
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    pub fn probe_count(&self, key: &NetworkKey) -> u64 {
        let now = self.clock.now_ms();
        let mut state = self.lock_state();
        expire_counter_for_key(&mut state.probe, key, now);
        state.probe.get(key).map_or(0, |counter| counter.count)
    }

    #[cfg(test)]
    fn sweep_count(&self, kind: &'static str) -> u64 {
        self.lock_state()
            .sweep_counts
            .get(kind)
            .copied()
            .unwrap_or(0)
    }
}

#[derive(Clone, Copy)]
enum SecurityQuotaKind {
    Probe,
    Redeem,
}

impl SecurityQuotaKind {
    fn label(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Redeem => "redeem",
        }
    }
}

fn expire_login_lock_for_key(state: &mut State, key: &NetworkKey, now: u64) {
    let expired = state
        .login_active_locks
        .get(key)
        .is_some_and(|active| active.lock_until_ms <= now);
    if !expired {
        return;
    }
    if let Some(active) = state.login_active_locks.remove(key)
        && active.failure.expires_at_ms > now
    {
        state.login_evictable.put(*key, active.failure);
    }
}

fn expire_all_login_locks(state: &mut State, now: u64) {
    let expired: Vec<_> = state
        .login_active_locks
        .iter()
        .filter_map(|(key, active)| (active.lock_until_ms <= now).then_some(*key))
        .collect();
    for key in expired {
        expire_login_lock_for_key(state, &key, now);
    }
}

fn expire_counter_for_key(map: &mut HashMap<NetworkKey, Counter>, key: &NetworkKey, now: u64) {
    if map
        .get(key)
        .is_some_and(|counter| counter.expires_at_ms <= now)
    {
        map.remove(key);
    }
}

fn remove_expired(map: &mut HashMap<NetworkKey, Counter>, now: u64) {
    map.retain(|_, counter| counter.expires_at_ms > now);
}

fn sweep_due(state: &mut State, kind: &'static str, now: u64) -> bool {
    let due = state
        .last_sweep_ms
        .get(kind)
        .is_none_or(|last| now.saturating_sub(*last) >= CAPACITY_SWEEP_INTERVAL_MS);
    if due {
        state.last_sweep_ms.insert(kind, now);
        #[cfg(test)]
        {
            *state.sweep_counts.entry(kind).or_default() += 1;
        }
    }
    due
}

fn parse_capacity(value: Option<&str>) -> Result<usize, String> {
    let capacity = match value {
        Some(value) => value
            .trim()
            .parse::<usize>()
            .map_err(|_| format!("{MAX_BUCKETS_ENV} must be a positive integer"))?,
        None => DEFAULT_MAX_BUCKETS,
    };
    if capacity == 0 {
        return Err(format!("{MAX_BUCKETS_ENV} must be greater than zero"));
    }
    Ok(capacity)
}

fn warn_capacity(state: &mut State, kind: &'static str, now: u64) {
    let last = state.last_capacity_warn_ms.entry(kind).or_default();
    if now.saturating_sub(*last) >= 60_000 || *last == 0 {
        log::warn!("Authentication rate-limit capacity exhausted: kind={kind}");
        *last = now.max(1);
    }
}

pub fn lock_ttl_ms_for(count: u64) -> Option<u64> {
    if count < LOGIN_MAX_FAILURES {
        return None;
    }
    let mut ttl = LOGIN_LOCK_BASE_SEC * 1_000;
    let cap = LOGIN_LOCK_MAX_SEC * 1_000;
    for _ in 0..count - LOGIN_MAX_FAILURES {
        ttl = ttl.saturating_mul(2);
        if ttl >= cap {
            return Some(cap);
        }
    }
    Some(ttl.min(cap))
}

fn ms_to_secs_ceil(ms: u64) -> u64 {
    ms.div_ceil(1_000)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in left.iter().zip(right) {
        diff |= left ^ right;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Barrier, Mutex as TestMutex};

    #[derive(Default)]
    struct ManualClock(AtomicU64);

    impl Clock for ManualClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl ManualClock {
        fn advance(&self, millis: u64) {
            self.0.fetch_add(millis, Ordering::SeqCst);
        }
    }

    fn key(value: u8) -> NetworkKey {
        match IpAddr::V4(Ipv4Addr::new(192, 0, 2, value)) {
            IpAddr::V4(ip) => NetworkKey::V4(ip),
            IpAddr::V6(_) => unreachable!(),
        }
    }

    fn limiter(clock: Arc<ManualClock>, capacity: usize) -> AuthRateLimiter {
        AuthRateLimiter::with_clock_and_capacities(clock, capacity, 2, 2, 2)
    }

    #[test]
    fn twentieth_login_failure_locks_and_success_can_clear() {
        let clock = Arc::new(ManualClock::default());
        let limiter = limiter(clock, 8);
        for _ in 0..LOGIN_MAX_FAILURES - 1 {
            assert_eq!(
                limiter.record_login_failure(key(1)),
                LoginFailureResult::Recorded
            );
        }
        assert_eq!(
            limiter.record_login_failure(key(1)),
            LoginFailureResult::Locked {
                retry_after_sec: LOGIN_LOCK_BASE_SEC
            }
        );
        assert_eq!(limiter.login_lock_ttl(&key(1)), Some(LOGIN_LOCK_BASE_SEC));
        limiter.clear_login(&key(1));
        assert_eq!(limiter.login_lock_ttl(&key(1)), None);
    }

    #[test]
    fn fixed_failure_window_does_not_refresh() {
        let clock = Arc::new(ManualClock::default());
        let limiter = limiter(clock.clone(), 8);
        limiter.record_login_failure(key(1));
        clock.advance((LOGIN_WINDOW_SEC - 1) * 1_000);
        limiter.record_login_failure(key(1));
        clock.advance(1_001);
        assert_eq!(
            limiter.record_login_failure(key(1)),
            LoginFailureResult::Recorded
        );
    }

    #[test]
    fn active_locks_are_pinned_and_new_login_keys_fail_open() {
        let clock = Arc::new(ManualClock::default());
        let limiter = limiter(clock.clone(), 1);
        for _ in 0..LOGIN_MAX_FAILURES {
            limiter.record_login_failure(key(1));
        }
        assert_eq!(
            limiter.record_login_failure(key(2)),
            LoginFailureResult::UntrackedCapacity
        );
        assert!(limiter.login_lock_ttl(&key(1)).is_some());
        assert_eq!(limiter.sweep_count("login"), 1);
        assert_eq!(
            limiter.record_login_failure(key(3)),
            LoginFailureResult::UntrackedCapacity
        );
        assert_eq!(limiter.sweep_count("login"), 1);
        clock.advance(CAPACITY_SWEEP_INTERVAL_MS);
        assert_eq!(
            limiter.record_login_failure(key(4)),
            LoginFailureResult::UntrackedCapacity
        );
        assert_eq!(limiter.sweep_count("login"), 2);
    }

    #[test]
    fn capacity_sweep_reclaims_an_expired_login_lock() {
        let clock = Arc::new(ManualClock::default());
        let limiter = limiter(clock.clone(), 1);
        for _ in 0..LOGIN_MAX_FAILURES {
            limiter.record_login_failure(key(1));
        }
        clock.advance((LOGIN_LOCK_BASE_SEC + 1) * 1_000);
        assert_eq!(
            limiter.record_login_failure(key(2)),
            LoginFailureResult::Recorded
        );
        assert_eq!(limiter.login_lock_ttl(&key(1)), None);
    }

    #[test]
    fn bootstrap_counts_only_failures_and_is_atomic_at_threshold() {
        let clock = Arc::new(ManualClock::default());
        let limiter = limiter(clock, 8);
        for _ in 0..BOOTSTRAP_MAX_FAILURES - 1 {
            assert_eq!(
                limiter.evaluate_bootstrap_attempt(key(1), b"secret", b"wrong"),
                BootstrapAttempt::Invalid
            );
        }
        assert_eq!(
            limiter.evaluate_bootstrap_attempt(key(1), b"secret", b"secret"),
            BootstrapAttempt::Allowed
        );
        for _ in 0..BOOTSTRAP_MAX_FAILURES - 1 {
            limiter.evaluate_bootstrap_attempt(key(1), b"secret", b"wrong");
        }
        assert_eq!(
            limiter.evaluate_bootstrap_attempt(key(1), b"secret", b"wrong"),
            BootstrapAttempt::Limited
        );
        assert_eq!(
            limiter.evaluate_bootstrap_attempt(key(1), b"secret", b"secret"),
            BootstrapAttempt::Limited
        );
    }

    #[test]
    fn concurrent_bootstrap_failures_share_one_atomic_budget() {
        let limiter = Arc::new(limiter(Arc::new(ManualClock::default()), 64));
        let barrier = Arc::new(Barrier::new(32));
        let decisions = Arc::new(TestMutex::new(Vec::new()));
        std::thread::scope(|scope| {
            for _ in 0..32 {
                let limiter = Arc::clone(&limiter);
                let barrier = Arc::clone(&barrier);
                let decisions = Arc::clone(&decisions);
                scope.spawn(move || {
                    barrier.wait();
                    let decision = limiter.evaluate_bootstrap_attempt(key(1), b"secret", b"wrong");
                    decisions.lock().unwrap().push(decision);
                });
            }
        });
        let decisions = decisions.lock().unwrap();
        assert_eq!(
            decisions
                .iter()
                .filter(|decision| **decision == BootstrapAttempt::Invalid)
                .count(),
            (BOOTSTRAP_MAX_FAILURES - 1) as usize
        );
        assert_eq!(
            decisions
                .iter()
                .filter(|decision| **decision == BootstrapAttempt::Limited)
                .count(),
            32 - (BOOTSTRAP_MAX_FAILURES - 1) as usize
        );
    }

    #[test]
    fn bootstrap_capacity_is_fail_closed_before_correct_comparison() {
        let clock = Arc::new(ManualClock::default());
        let limiter = limiter(clock, 8);
        assert_eq!(
            limiter.evaluate_bootstrap_attempt(key(1), b"secret", b"wrong"),
            BootstrapAttempt::Invalid
        );
        assert_eq!(
            limiter.evaluate_bootstrap_attempt(key(2), b"secret", b"wrong"),
            BootstrapAttempt::Invalid
        );
        assert_eq!(
            limiter.evaluate_bootstrap_attempt(key(3), b"secret", b"secret"),
            BootstrapAttempt::Limited
        );
        assert_eq!(limiter.sweep_count("bootstrap"), 1);
        assert_eq!(
            limiter.evaluate_bootstrap_attempt(key(4), b"secret", b"secret"),
            BootstrapAttempt::Limited
        );
        assert_eq!(limiter.sweep_count("bootstrap"), 1);
    }

    #[test]
    fn capacity_sweep_reclaims_expired_security_entries() {
        let clock = Arc::new(ManualClock::default());
        let limiter = limiter(clock.clone(), 8);
        for existing in [key(1), key(2)] {
            assert_eq!(
                limiter.evaluate_bootstrap_attempt(existing, b"secret", b"wrong"),
                BootstrapAttempt::Invalid
            );
            assert_eq!(limiter.consume_probe(existing), QuotaDecision::Allowed);
            assert_eq!(limiter.consume_redeem(existing), QuotaDecision::Allowed);
        }
        clock.advance((BOOTSTRAP_WINDOW_SEC + 1) * 1_000);
        assert_eq!(
            limiter.evaluate_bootstrap_attempt(key(3), b"secret", b"secret"),
            BootstrapAttempt::Allowed
        );
        assert_eq!(limiter.consume_probe(key(3)), QuotaDecision::Allowed);
        assert_eq!(limiter.consume_redeem(key(3)), QuotaDecision::Allowed);
    }

    #[test]
    fn probe_and_redeem_quotas_are_independent_and_expire() {
        let clock = Arc::new(ManualClock::default());
        let limiter = limiter(clock.clone(), 8);
        for _ in 0..PROBE_MAX_ATTEMPTS {
            assert_eq!(limiter.consume_probe(key(1)), QuotaDecision::Allowed);
        }
        assert_eq!(limiter.consume_probe(key(1)), QuotaDecision::Limited);
        for _ in 0..REDEEM_MAX_ATTEMPTS {
            assert_eq!(limiter.consume_redeem(key(1)), QuotaDecision::Allowed);
        }
        assert_eq!(limiter.consume_redeem(key(1)), QuotaDecision::Limited);
        clock.advance((PROBE_WINDOW_SEC + 1) * 1_000);
        assert_eq!(limiter.consume_probe(key(1)), QuotaDecision::Allowed);
        assert_eq!(limiter.consume_redeem(key(1)), QuotaDecision::Allowed);
    }

    #[test]
    fn probe_and_redeem_capacity_exhaustion_is_fail_closed() {
        let limiter = limiter(Arc::new(ManualClock::default()), 8);
        for existing in [key(1), key(2)] {
            assert_eq!(limiter.consume_probe(existing), QuotaDecision::Allowed);
            assert_eq!(limiter.consume_redeem(existing), QuotaDecision::Allowed);
        }
        assert_eq!(limiter.consume_probe(key(3)), QuotaDecision::Limited);
        assert_eq!(limiter.consume_redeem(key(3)), QuotaDecision::Limited);
        assert_eq!(limiter.sweep_count("probe"), 1);
        assert_eq!(limiter.sweep_count("redeem"), 1);
        assert_eq!(limiter.consume_probe(key(4)), QuotaDecision::Limited);
        assert_eq!(limiter.consume_redeem(key(4)), QuotaDecision::Limited);
        assert_eq!(limiter.sweep_count("probe"), 1);
        assert_eq!(limiter.sweep_count("redeem"), 1);
    }

    #[test]
    fn exponential_curve_matches_manager_prior_art() {
        assert_eq!(lock_ttl_ms_for(19), None);
        assert_eq!(lock_ttl_ms_for(20), Some(60_000));
        assert_eq!(lock_ttl_ms_for(21), Some(120_000));
        assert_eq!(lock_ttl_ms_for(22), Some(240_000));
        assert_eq!(lock_ttl_ms_for(26), Some(3_600_000));
        assert_eq!(lock_ttl_ms_for(100), Some(3_600_000));
    }

    #[test]
    fn capacity_configuration_rejects_invalid_and_zero_values() {
        assert_eq!(parse_capacity(None), Ok(DEFAULT_MAX_BUCKETS));
        assert_eq!(parse_capacity(Some(" 128 ")), Ok(128));
        assert!(parse_capacity(Some("not-a-number")).is_err());
        assert!(parse_capacity(Some("")).is_err());
        assert!(parse_capacity(Some("0")).is_err());
    }
}
