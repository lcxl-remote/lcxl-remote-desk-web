//! Administrative policy diagnostics, separate from persisted policy state.

use super::user_instructions::AssistantDataPolicy;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyIssueLevel {
    Error,
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyIssueCode {
    ValueOutOfRange,
    DefaultExceedsMaximum,
    PointAccountingUnavailable,
    DuplicateFactTypes,
    ResponseTooSmall,
    RelevancePageLimited,
    HistoryCandidatesLimited,
    NativeSummaryLimited,
    OwnerScanLimited,
    CurrentMemoriesLimited,
    RevisionMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyIssue {
    pub level: PolicyIssueLevel,
    pub code: PolicyIssueCode,
    pub fields: Vec<String>,
    pub message: String,
    pub effective_limit: Option<u64>,
}

impl PolicyIssue {
    pub fn error(code: PolicyIssueCode, fields: &[&str], message: impl Into<String>) -> Self {
        Self {
            level: PolicyIssueLevel::Error,
            code,
            fields: fields.iter().map(|field| (*field).into()).collect(),
            message: message.into(),
            effective_limit: None,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyAssessment {
    pub issues: Vec<PolicyIssue>,
}

impl PolicyAssessment {
    pub fn has_errors(&self) -> bool {
        self.issues
            .iter()
            .any(|issue| issue.level == PolicyIssueLevel::Error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyView {
    pub policy: AssistantDataPolicy,
    pub issues: Vec<PolicyIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum PolicyWriteData {
    Saved(PolicyView),
    Invalid(PolicyAssessment),
    Conflict { current_revision: u64 },
}

pub(crate) fn range(
    issues: &mut Vec<PolicyIssue>,
    field: &str,
    value: u64,
    minimum: u64,
    maximum: u64,
) {
    if !(minimum..=maximum).contains(&value) {
        issues.push(PolicyIssue::error(
            PolicyIssueCode::ValueOutOfRange,
            &[field],
            format!("Value must be between {minimum} and {maximum}"),
        ));
    }
}

pub(crate) fn result(issues: Vec<PolicyIssue>, message: &'static str) -> Result<(), &'static str> {
    if issues.is_empty() {
        Ok(())
    } else {
        Err(message)
    }
}

mod validation;
pub use validation::policy_errors;
pub(crate) use validation::{episode_errors, limits_errors, processing_errors};
