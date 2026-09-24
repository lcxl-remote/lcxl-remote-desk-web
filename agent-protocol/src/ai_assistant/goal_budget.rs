//! Platform-owned limits for long-running AI Assistant goals.

use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;

/// `None` disables only that goal budget. Transport, storage and permission
/// safety limits remain in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalBudgetLimits {
    #[serde(deserialize_with = "required_option")]
    #[schema(required = true)]
    pub active_time_ms: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    #[schema(required = true)]
    pub deadline_ms: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    #[schema(required = true)]
    pub model_tokens: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    #[schema(required = true)]
    pub model_calls: Option<u32>,
    #[serde(deserialize_with = "required_option")]
    #[schema(required = true)]
    pub tool_calls: Option<u32>,
    #[serde(deserialize_with = "required_option")]
    #[schema(required = true)]
    pub slices: Option<u32>,
    #[serde(deserialize_with = "required_option")]
    #[schema(required = true)]
    pub stalled_slices: Option<u32>,
}

fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalBudgetPolicy {
    pub schema_version: u16,
    pub revision: u64,
    pub limits: GoalBudgetLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateGoalBudgetPolicy {
    pub expected_revision: u64,
    pub limits: GoalBudgetLimits,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_budget_switch_must_be_explicit() {
        let mut limits = serde_json::to_value(GoalBudgetLimits {
            active_time_ms: None,
            deadline_ms: Some(86_400_000),
            model_tokens: Some(100_000),
            model_calls: Some(160),
            tool_calls: Some(200),
            slices: Some(20),
            stalled_slices: Some(3),
        })
        .unwrap();
        assert!(serde_json::from_value::<GoalBudgetLimits>(limits.clone()).is_ok());
        limits.as_object_mut().unwrap().remove("modelTokens");
        assert!(serde_json::from_value::<GoalBudgetLimits>(limits).is_err());
    }
}
