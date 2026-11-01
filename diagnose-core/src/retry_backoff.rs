//! Per-node retry pacing for background candidates that keep failing.
//!
//! This is only a scheduling hint. Losing it, or nodes disagreeing about it,
//! causes extra attempts at most: correctness stays with the durable version,
//! lease and claim checks of the attempt itself.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const INITIAL_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(300);
const PRUNE_ABOVE_ENTRIES: usize = 1024;

#[derive(Debug, Clone, Copy)]
struct Entry {
    failures: u32,
    retry_at: Instant,
}

#[derive(Debug, Default)]
pub struct RetryBackoff {
    entries: Mutex<HashMap<String, Entry>>,
}

impl RetryBackoff {
    /// Whether `key` may be attempted at `now`.
    pub fn is_ready(&self, key: &str, now: Instant) -> bool {
        self.lock()
            .get(key)
            .is_none_or(|entry| entry.retry_at <= now)
    }

    /// Record a failed attempt and return the delay before the next one.
    pub fn record_failure(&self, key: &str, now: Instant) -> Duration {
        let mut entries = self.lock();
        if entries.len() >= PRUNE_ABOVE_ENTRIES && !entries.contains_key(key) {
            entries.retain(|_, entry| entry.retry_at > now);
        }
        let failures = entries.get(key).map_or(0, |entry| entry.failures) + 1;
        let delay = INITIAL_DELAY
            .saturating_mul(2u32.saturating_pow(failures - 1))
            .min(MAX_DELAY);
        entries.insert(
            key.to_owned(),
            Entry {
                failures,
                retry_at: now + delay,
            },
        );
        delay
    }

    pub fn record_success(&self, key: &str) {
        self.lock().remove(key);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        // The map stays structurally valid if a holder panicked.
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_doubles_up_to_the_cap_and_blocks_until_due() {
        let backoff = RetryBackoff::default();
        let now = Instant::now();
        assert!(backoff.is_ready("a", now));
        assert_eq!(backoff.record_failure("a", now), Duration::from_secs(1));
        assert!(!backoff.is_ready("a", now));
        assert!(backoff.is_ready("a", now + Duration::from_secs(1)));
        assert_eq!(backoff.record_failure("a", now), Duration::from_secs(2));
        assert_eq!(backoff.record_failure("a", now), Duration::from_secs(4));
        for _ in 0..40 {
            backoff.record_failure("a", now);
        }
        assert_eq!(backoff.record_failure("a", now), MAX_DELAY);
    }

    #[test]
    fn success_clears_only_its_own_key() {
        let backoff = RetryBackoff::default();
        let now = Instant::now();
        backoff.record_failure("a", now);
        backoff.record_failure("b", now);
        backoff.record_success("a");
        assert!(backoff.is_ready("a", now));
        assert!(!backoff.is_ready("b", now));
        assert_eq!(backoff.record_failure("a", now), Duration::from_secs(1));
    }

    #[test]
    fn expired_entries_are_pruned_when_the_map_grows() {
        let backoff = RetryBackoff::default();
        let now = Instant::now();
        for index in 0..PRUNE_ABOVE_ENTRIES {
            backoff.record_failure(&index.to_string(), now);
        }
        backoff.record_failure("new", now + MAX_DELAY);
        assert_eq!(backoff.lock().len(), 1);
    }
}
