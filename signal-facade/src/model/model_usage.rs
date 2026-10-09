//! Current public usage contract shared by signal and manager.

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ModelUsageQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub purpose: Option<String>,
    pub granularity: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsageItem {
    pub provider_id: String,
    pub model_id: String,
    pub model_name: String,
    pub purpose: String,
    pub hour_bucket: String,
    pub subject_tier: Option<String>,
    pub subject_user_id: Option<i32>,
    pub subject_org_id: Option<i32>,
    pub input_tokens: String,
    pub output_tokens: String,
    pub cache_read_tokens: String,
    pub cache_write_tokens: String,
    pub request_count: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModelUsageRange {
    pub from: String,
    pub to: String,
    pub granularity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModelUsageResult {
    pub items: Vec<ModelUsageItem>,
    pub range: ModelUsageRange,
    pub usage_source: String,
    pub partial: bool,
    pub available_from: Option<String>,
}
