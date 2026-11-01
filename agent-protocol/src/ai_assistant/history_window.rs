//! Owner-only retained fragments; these are never execution-session snapshots.
use super::source::{AssistantSourceKind, AssistantSourceResolution, validate_source_id};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum AssistantHistoryWindowView {
    #[default]
    Context,
    Latest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AssistantHistoryWindowRequest {
    pub source_id: String,
    pub revision: u64,
    #[serde(default)]
    pub view: AssistantHistoryWindowView,
    pub cursor: Option<String>,
}
impl AssistantHistoryWindowRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_source_id(&self.source_id)?;
        if self.revision == 0
            || self.revision > i64::MAX as u64
            || self
                .cursor
                .as_ref()
                .is_some_and(|v| v.is_empty() || v.len() > 2048)
        {
            return Err("invalid retained history window");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AssistantHistoryFragment {
    pub source: AssistantSourceResolution,
    pub role: String,
    pub kind: AssistantSourceKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AssistantHistoryWindow {
    pub conversation_id: String,
    pub anchor_source_id: String,
    pub anchor_revision: u64,
    pub observed_through_generation: u64,
    pub fragments: Vec<AssistantHistoryFragment>,
    pub previous_cursor: Option<String>,
    pub next_cursor: Option<String>,
    pub partial: bool,
}
