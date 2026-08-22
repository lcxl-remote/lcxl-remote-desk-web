//! Fail-closed parser for safety-model verdicts.

use std::collections::HashSet;

use desk_agent_protocol::AgentError;
use desk_agent_protocol::content_safety::{
    ContentSafetyDecision, ContentSafetyStage, ContentSafetyVerdict,
};

use super::policy::aggregate_decision;
use super::prompt::CONTENT_SAFETY_PROMPT_VERSION;
use super::seam::content_safety_unavailable;

fn invalid_verdict() -> AgentError {
    content_safety_unavailable()
}

/// Closed, content-free reason for rejecting a provider verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyVerdictInvalidReason {
    InvalidJson,
    PolicyVersion,
    DuplicateCategory,
    DuplicateStage,
    StageNotAllowed,
    AllowNonempty,
    BlockedEmpty,
    DecisionMismatch,
}

impl SafetyVerdictInvalidReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid_json",
            Self::PolicyVersion => "policy_version",
            Self::DuplicateCategory => "duplicate_category",
            Self::DuplicateStage => "duplicate_stage",
            Self::StageNotAllowed => "stage_not_allowed",
            Self::AllowNonempty => "allow_nonempty",
            Self::BlockedEmpty => "blocked_empty",
            Self::DecisionMismatch => "decision_mismatch",
        }
    }
}

/// Parse a verdict while retaining only a closed diagnostic reason. Neither the
/// raw model output nor serde's input-derived error string escapes this helper.
pub fn parse_safety_verdict_detailed(
    raw: &str,
    allowed_stages: &[ContentSafetyStage],
) -> Result<ContentSafetyVerdict, SafetyVerdictInvalidReason> {
    let verdict: ContentSafetyVerdict =
        serde_json::from_str(raw).map_err(|_| SafetyVerdictInvalidReason::InvalidJson)?;

    if verdict.policy_version != CONTENT_SAFETY_PROMPT_VERSION {
        return Err(SafetyVerdictInvalidReason::PolicyVersion);
    }

    let unique_categories: HashSet<_> = verdict.categories.iter().copied().collect();
    if unique_categories.len() != verdict.categories.len() {
        return Err(SafetyVerdictInvalidReason::DuplicateCategory);
    }
    let unique_stages: HashSet<_> = verdict.stages.iter().copied().collect();
    if unique_stages.len() != verdict.stages.len() {
        return Err(SafetyVerdictInvalidReason::DuplicateStage);
    }

    if verdict
        .stages
        .iter()
        .any(|stage| !allowed_stages.contains(stage))
    {
        return Err(SafetyVerdictInvalidReason::StageNotAllowed);
    }

    match verdict.decision {
        ContentSafetyDecision::Allow => {
            if !verdict.categories.is_empty() || !verdict.stages.is_empty() {
                return Err(SafetyVerdictInvalidReason::AllowNonempty);
            }
        }
        ContentSafetyDecision::Block | ContentSafetyDecision::SafeRedirect => {
            if verdict.categories.is_empty() || verdict.stages.is_empty() {
                return Err(SafetyVerdictInvalidReason::BlockedEmpty);
            }
            if aggregate_decision(verdict.categories.iter().copied()) != verdict.decision {
                return Err(SafetyVerdictInvalidReason::DecisionMismatch);
            }
        }
    }

    Ok(verdict)
}

/// Parse and validate one strict JSON verdict. Provider prose and parse details
/// are deliberately omitted from the returned error.
pub fn parse_safety_verdict(
    raw: &str,
    allowed_stages: &[ContentSafetyStage],
) -> Result<ContentSafetyVerdict, AgentError> {
    parse_safety_verdict_detailed(raw, allowed_stages).map_err(|_| invalid_verdict())
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::AgentErrorKind;

    fn parse(raw: &str, stages: &[ContentSafetyStage]) -> Result<ContentSafetyVerdict, AgentError> {
        parse_safety_verdict(raw, stages)
    }

    #[test]
    fn accepts_consistent_allow_block_and_safe_redirect() {
        let allow = r#"{"decision":"allow","categories":[],"stages":[],"policy_version":"content-safety-v1"}"#;
        assert_eq!(
            parse(allow, &[ContentSafetyStage::Input]).unwrap().decision,
            ContentSafetyDecision::Allow
        );

        let block = r#"{"decision":"block","categories":["politics"],"stages":["input"],"policy_version":"content-safety-v1"}"#;
        assert_eq!(
            parse(block, &[ContentSafetyStage::Input]).unwrap().decision,
            ContentSafetyDecision::Block
        );

        let redirect = r#"{"decision":"safe_redirect","categories":["self_harm"],"stages":["output"],"policy_version":"content-safety-v1"}"#;
        assert_eq!(
            parse(redirect, &[ContentSafetyStage::Output])
                .unwrap()
                .decision,
            ContentSafetyDecision::SafeRedirect
        );
    }

    #[test]
    fn malformed_unknown_missing_duplicate_and_extra_content_fail_closed() {
        let cases = [
            "",
            "{}",
            r#"{"decision":"review","categories":[],"stages":[],"policy_version":"content-safety-v1"}"#,
            r#"{"decision":"allow","categories":[],"stages":[],"policy_version":"content-safety-v1","reason":"x"}"#,
            r#"{"decision":"allow","decision":"block","categories":[],"stages":[],"policy_version":"content-safety-v1"}"#,
            r#"{"decision":"allow","categories":[],"stages":[],"policy_version":"content-safety-v1"} trailing"#,
            r#"{"decision":"allow","categories":[],"stages":[],"policy_version":"other"}"#,
        ];
        for raw in cases {
            let error = parse(raw, &[ContentSafetyStage::Input]).unwrap_err();
            assert_eq!(error.kind, AgentErrorKind::ContentSafetyUnavailable);
            assert!(error.retryable);
        }
    }

    #[test]
    fn inconsistent_decision_duplicates_and_wrong_stage_fail_closed() {
        let cases = [
            r#"{"decision":"allow","categories":["politics"],"stages":["input"],"policy_version":"content-safety-v1"}"#,
            r#"{"decision":"safe_redirect","categories":["politics"],"stages":["input"],"policy_version":"content-safety-v1"}"#,
            r#"{"decision":"block","categories":["self_harm"],"stages":["input"],"policy_version":"content-safety-v1"}"#,
            r#"{"decision":"block","categories":["politics","politics"],"stages":["input"],"policy_version":"content-safety-v1"}"#,
            r#"{"decision":"block","categories":["politics"],"stages":["action","action"],"policy_version":"content-safety-v1"}"#,
            r#"{"decision":"block","categories":["politics"],"stages":["image"],"policy_version":"content-safety-v1"}"#,
        ];
        for raw in cases {
            assert!(
                parse(
                    raw,
                    &[ContentSafetyStage::Input, ContentSafetyStage::Action]
                )
                .is_err()
            );
        }
    }

    #[test]
    fn detailed_parser_returns_only_closed_reason_codes() {
        let cases = [
            ("not-json", SafetyVerdictInvalidReason::InvalidJson),
            (
                r#"{"decision":"allow","categories":[],"stages":[],"policy_version":"old"}"#,
                SafetyVerdictInvalidReason::PolicyVersion,
            ),
            (
                r#"{"decision":"block","categories":["politics","politics"],"stages":["input"],"policy_version":"content-safety-v1"}"#,
                SafetyVerdictInvalidReason::DuplicateCategory,
            ),
            (
                r#"{"decision":"block","categories":["politics"],"stages":["input","input"],"policy_version":"content-safety-v1"}"#,
                SafetyVerdictInvalidReason::DuplicateStage,
            ),
            (
                r#"{"decision":"block","categories":["politics"],"stages":["image"],"policy_version":"content-safety-v1"}"#,
                SafetyVerdictInvalidReason::StageNotAllowed,
            ),
            (
                r#"{"decision":"allow","categories":["politics"],"stages":["input"],"policy_version":"content-safety-v1"}"#,
                SafetyVerdictInvalidReason::AllowNonempty,
            ),
            (
                r#"{"decision":"block","categories":[],"stages":[],"policy_version":"content-safety-v1"}"#,
                SafetyVerdictInvalidReason::BlockedEmpty,
            ),
            (
                r#"{"decision":"safe_redirect","categories":["politics"],"stages":["input"],"policy_version":"content-safety-v1"}"#,
                SafetyVerdictInvalidReason::DecisionMismatch,
            ),
        ];
        for (raw, expected) in cases {
            let reason =
                parse_safety_verdict_detailed(raw, &[ContentSafetyStage::Input]).unwrap_err();
            assert_eq!(reason, expected);
            assert!(!reason.as_str().contains(raw));
        }
    }
}
