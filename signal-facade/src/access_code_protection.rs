//! Configuration shared by access-code redemption hosts.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AccessCodeProtectionPolicy {
    #[schema(minimum = 1, maximum = 86400)]
    pub request_window_sec: u32,
    #[schema(minimum = 1, maximum = 100000)]
    pub user_max_attempts: Option<u32>,
    #[schema(minimum = 1, maximum = 100000)]
    pub network_max_attempts: u32,
    pub backoff_enabled: bool,
    #[schema(minimum = 1, maximum = 86400)]
    pub failure_window_sec: u32,
    #[schema(minimum = 1, maximum = 100000)]
    pub max_failures: u32,
    #[schema(minimum = 1, maximum = 86400)]
    pub lock_base_sec: u32,
    #[schema(minimum = 1, maximum = 86400)]
    pub lock_max_sec: u32,
}
impl Default for AccessCodeProtectionPolicy {
    fn default() -> Self {
        Self {
            request_window_sec: 60,
            user_max_attempts: None,
            network_max_attempts: 5,
            backoff_enabled: true,
            failure_window_sec: 900,
            max_failures: 20,
            lock_base_sec: 60,
            lock_max_sec: 3600,
        }
    }
}
impl AccessCodeProtectionPolicy {
    pub fn manager_default() -> Self {
        Self {
            user_max_attempts: Some(20),
            network_max_attempts: 60,
            ..Self::default()
        }
    }
    pub fn validate(&self, require_user_limit: bool) -> Result<(), &'static str> {
        if self.user_max_attempts.is_some() != require_user_limit {
            return Err("Account limit is required only on Manager");
        }
        if [
            self.request_window_sec,
            self.failure_window_sec,
            self.lock_base_sec,
            self.lock_max_sec,
        ]
        .iter()
        .any(|v| !(1..=86400).contains(v))
        {
            return Err("Time intervals must be between 1 and 86400 seconds");
        }
        if [
            Some(self.network_max_attempts),
            Some(self.max_failures),
            self.user_max_attempts,
        ]
        .into_iter()
        .flatten()
        .any(|v| !(1..=100000).contains(&v))
        {
            return Err("Attempt limits must be between 1 and 100000");
        }
        if self.lock_base_sec > self.lock_max_sec {
            return Err("Maximum lock must not be shorter than initial lock");
        }
        Ok(())
    }
    pub fn lock_ttl_ms(&self, count: u64) -> Option<u64> {
        if !self.backoff_enabled || count < u64::from(self.max_failures) {
            return None;
        }
        let cap = u64::from(self.lock_max_sec) * 1000;
        let mut ttl = u64::from(self.lock_base_sec) * 1000;
        for _ in 0..count - u64::from(self.max_failures) {
            ttl = ttl.saturating_mul(2);
            if ttl >= cap {
                return Some(cap);
            }
        }
        Some(ttl.min(cap))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AccessCodeProtectionSettings {
    pub policy: AccessCodeProtectionPolicy,
    pub revision: u64,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateAccessCodeProtectionSettings {
    pub policy: AccessCodeProtectionPolicy,
    pub expected_revision: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_roles_defaults_and_capped_backoff() {
        let base = AccessCodeProtectionPolicy::default();
        assert!(base.validate(false).is_ok());
        assert!(base.validate(true).is_err());
        assert!(
            AccessCodeProtectionPolicy::manager_default()
                .validate(true)
                .is_ok()
        );
        for value in [0, 86401] {
            assert!(
                AccessCodeProtectionPolicy {
                    failure_window_sec: value,
                    ..base
                }
                .validate(false)
                .is_err()
            );
        }
        for value in [0, 100001] {
            assert!(
                AccessCodeProtectionPolicy {
                    max_failures: value,
                    ..base
                }
                .validate(false)
                .is_err()
            );
        }
        assert!(
            AccessCodeProtectionPolicy {
                lock_max_sec: 59,
                ..base
            }
            .validate(false)
            .is_err()
        );
        assert_eq!(base.lock_ttl_ms(19), None);
        assert_eq!(base.lock_ttl_ms(20), Some(60000));
        assert_eq!(base.lock_ttl_ms(21), Some(120000));
        assert_eq!(base.lock_ttl_ms(u64::MAX), Some(3600000));
        assert_eq!(
            AccessCodeProtectionPolicy {
                backoff_enabled: false,
                ..base
            }
            .lock_ttl_ms(100),
            None
        );
    }
}
