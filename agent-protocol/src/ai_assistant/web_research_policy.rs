//! Bound on the bytes one public web page read may take; not a permission grant.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub const SCHEMA_VERSION: u16 = 1;
pub const DEFAULT_MAX_FETCH_BYTES: u32 = 128 * 1024;
pub const MIN_MAX_FETCH_BYTES: u32 = 16 * 1024;
/// A hard ceiling on the response body read for one page.
pub const MAX_MAX_FETCH_BYTES: u32 = 1024 * 1024;

pub fn valid_max_fetch_bytes(value: u32) -> bool {
    (MIN_MAX_FETCH_BYTES..=MAX_MAX_FETCH_BYTES).contains(&value)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebResearchPolicy {
    pub schema_version: u16,
    pub revision: u64,
    pub max_fetch_bytes: u32,
}

impl Default for WebResearchPolicy {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            revision: 0,
            max_fetch_bytes: DEFAULT_MAX_FETCH_BYTES,
        }
    }
}

impl WebResearchPolicy {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != SCHEMA_VERSION {
            return Err("unsupported web research policy schema");
        }
        if !valid_max_fetch_bytes(self.max_fetch_bytes) {
            return Err("web page read limit is out of range");
        }
        Ok(())
    }

    /// The next revision with a new limit.
    pub fn candidate(&self, max_fetch_bytes: u32) -> Result<Self, &'static str> {
        self.validate()?;
        let next = Self {
            schema_version: SCHEMA_VERSION,
            revision: self
                .revision
                .checked_add(1)
                .ok_or("web research policy revision exhausted")?,
            max_fetch_bytes,
        };
        next.validate()?;
        Ok(next)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateWebResearchPolicy {
    pub expected_revision: u64,
    pub max_fetch_bytes: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_is_bounded_and_each_update_advances_the_revision() {
        let current = WebResearchPolicy::default();
        assert_eq!(
            serde_json::to_value(&current).unwrap(),
            serde_json::json!({"schemaVersion": 1, "revision": 0, "maxFetchBytes": 131072})
        );
        let next = current.candidate(512 * 1024).unwrap();
        assert_eq!((next.revision, next.max_fetch_bytes), (1, 512 * 1024));
        assert!(current.candidate(MIN_MAX_FETCH_BYTES - 1).is_err());
        assert!(current.candidate(MAX_MAX_FETCH_BYTES + 1).is_err());
        assert!(current.candidate(MAX_MAX_FETCH_BYTES).is_ok());
    }
}
