//! Independent reviewer execution policy, separate from connection settings.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRuntimeSettings {
    pub revision: String,
    pub format_retry_count: u32,
}
impl Default for ApprovalRuntimeSettings {
    fn default() -> Self {
        Self {
            revision: "1".into(),
            format_retry_count: 1,
        }
    }
}
impl ApprovalRuntimeSettings {
    pub fn validate(&self) -> Result<(), &'static str> {
        let revision = self
            .revision
            .parse::<u64>()
            .map_err(|_| "invalid approval runtime revision")?;
        if revision == 0 || revision.to_string() != self.revision {
            return Err("invalid approval runtime revision");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_is_one_additional_call_and_zero_disables_correction() {
        let mut value = ApprovalRuntimeSettings::default();
        assert_eq!(value.format_retry_count, 1);
        value.format_retry_count = 0;
        assert!(value.validate().is_ok());
        assert!(
            serde_json::from_str::<ApprovalRuntimeSettings>(
                r#"{"revision":"1","format_retry_count":-1}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ApprovalRuntimeSettings>(
                r#"{"revision":"1","format_retry_count":1.5}"#
            )
            .is_err()
        );
        value.revision = "01".into();
        assert!(value.validate().is_err());
    }
}
