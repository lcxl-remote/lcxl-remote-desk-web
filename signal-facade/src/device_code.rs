//! Shared policy for newly generated device dial codes.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub const DEFAULT_LENGTH: u16 = 8;
pub const MIN_LENGTH: u16 = 6;
pub const MAX_LENGTH: u16 = 64;
pub const MAX_ATTEMPTS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeviceCodeSettings {
    #[schema(minimum = 6, maximum = 64, default = 8)]
    pub length: u16,
    pub revision: u64,
}
impl Default for DeviceCodeSettings {
    fn default() -> Self {
        Self {
            length: DEFAULT_LENGTH,
            revision: 0,
        }
    }
}
impl DeviceCodeSettings {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(MIN_LENGTH..=MAX_LENGTH).contains(&self.length) {
            return Err("Device code length must be between 6 and 64");
        }
        Ok(())
    }
    pub fn candidate(&self, length: u16) -> Result<Self, &'static str> {
        let candidate = Self {
            length,
            revision: self
                .revision
                .checked_add(1)
                .ok_or("Device code revision exhausted")?,
        };
        candidate.validate()?;
        Ok(candidate)
    }
}
#[derive(Debug, Clone, Copy, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateDeviceCodeSettings {
    pub expected_revision: u64,
    #[schema(minimum = 6, maximum = 64)]
    pub length: u16,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generation_policy_defaults_bounds_and_revision() {
        let base = DeviceCodeSettings::default();
        assert_eq!(base.length, 8);
        for length in [6, 8, 10, 64] {
            let next = base.candidate(length).unwrap();
            assert_eq!(next.revision, 1);
            assert_eq!(next.length, length);
        }
        for length in [0, 5, 65, u16::MAX] {
            assert!(base.candidate(length).is_err());
        }
        assert!(
            DeviceCodeSettings {
                length: 8,
                revision: u64::MAX
            }
            .candidate(9)
            .is_err()
        );
    }
}
