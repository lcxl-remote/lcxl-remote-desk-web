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

/// Parse and validate one strict JSON verdict. Provider prose and parse details
/// are deliberately omitted from the returned error.
pub fn parse_safety_verdict(
    raw: &str,
    allowed_stages: &[ContentSafetyStage],
) -> Result<ContentSafetyVerdict, AgentError> {
    let verdict: ContentSafetyVerdict = serde_json::from_str(raw).map_err(|_| invalid_verdict())?;

    if verdict.policy_version != CONTENT_SAFETY_PROMPT_VERSION {
        return Err(invalid_verdict());
    }

    let unique_categories: HashSet<_> = verdict.categories.iter().copied().collect();
    let unique_stages: HashSet<_> = verdict.stages.iter().copied().collect();
    if unique_categories.len() != verdict.categories.len()
        || unique_stages.len() != verdict.stages.len()
    {
        return Err(invalid_verdict());
    }

    if verdict
        .stages
        .iter()
        .any(|stage| !allowed_stages.contains(stage))
    {
        return Err(invalid_verdict());
    }

    match verdict.decision {
        ContentSafetyDecision::Allow => {
            if !verdict.categories.is_empty() || !verdict.stages.is_empty() {
                return Err(invalid_verdict());
            }
        }
        ContentSafetyDecision::Block | ContentSafetyDecision::SafeRedirect => {
            if verdict.categories.is_empty() || verdict.stages.is_empty() {
                return Err(invalid_verdict());
            }
            if aggregate_decision(verdict.categories.iter().copied()) != verdict.decision {
                return Err(invalid_verdict());
            }
        }
    }

    Ok(verdict)
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
}
