//! Provider-neutral content-safety seam and request contracts.

use async_trait::async_trait;
use desk_agent_protocol::content_safety::{
    ContentSafetyStage, ContentSafetySurface, ContentSafetyVerdict,
};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_utils::error::DeskErrorCode;
use serde::{Deserialize, Serialize};

use crate::registry::ToolEffect;

/// Input-stage request. `trusted_context` is server-generated and intentionally
/// excludes tool output and prior model prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyInput {
    pub surface: ContentSafetySurface,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_context: Option<String>,
}

/// A normalized proposed tool call. The runtime resolves `effect` from its
/// server-authoritative registry and canonicalizes JSON before constructing this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyToolCall {
    pub name: String,
    pub effect: ToolEffect,
    pub canonical_arguments_json: String,
}

/// One complete model turn, reviewed once for output and action policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyModelTurn {
    pub surface: ContentSafetySurface,
    pub text: String,
    #[serde(default)]
    pub tool_calls: Vec<SafetyToolCall>,
    pub original_allowed_intent: String,
}

/// An image review request. The data URL is transported as an image input by the
/// manager seam; prompt serialization must never embed or log it as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafetyImage {
    pub surface: ContentSafetySurface,
    pub image_data_url: String,
    pub mime_type: String,
    pub original_allowed_intent: String,
}

pub type SafetyVerdict = ContentSafetyVerdict;

/// Immutable content-safety snapshot frozen before a protected turn is claimed.
/// The manager resolves these fields once; the shared loop never reselects a
/// model or revision while the turn is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafetyContext {
    pub surface: ContentSafetySurface,
    pub original_allowed_intent: String,
    pub policy_revision: u64,
    pub safety_model_id: String,
    pub safety_prompt_version: String,
}

/// Classifies content without owning provider, persistence, or governance state.
#[async_trait(?Send)]
pub trait ContentSafetySeam {
    async fn check_input(&self, request: SafetyInput) -> Result<SafetyVerdict, AgentError>;

    async fn check_model_turn(&self, request: SafetyModelTurn)
    -> Result<SafetyVerdict, AgentError>;

    async fn check_image(&self, request: SafetyImage) -> Result<SafetyVerdict, AgentError>;
}

/// Typed, retryable fail-closed error for provider, timeout, parse, configuration,
/// capability, and policy-version failures.
pub fn content_safety_unavailable() -> AgentError {
    AgentError {
        kind: AgentErrorKind::ContentSafetyUnavailable,
        message: "Content safety review is temporarily unavailable.".into(),
        retryable: true,
        safe_for_model: false,
        error_code: Some(DeskErrorCode::AI_CONTENT_SAFETY_UNAVAILABLE.code()),
    }
}

/// Typed error for an image that the configured safety model cannot review.
pub fn content_safety_image_unsupported() -> AgentError {
    AgentError {
        kind: AgentErrorKind::ContentSafetyUnavailable,
        message: "The configured content safety model cannot review image input.".into(),
        retryable: true,
        safe_for_model: false,
        error_code: Some(DeskErrorCode::AI_CONTENT_SAFETY_IMAGE_UNSUPPORTED.code()),
    }
}

/// Typed non-retryable policy error. The wire message is deliberately generic;
/// category and provider rationale never cross the boundary.
pub fn content_blocked_error() -> AgentError {
    AgentError {
        kind: AgentErrorKind::ContentBlocked,
        message: "The content safety policy declined this request.".into(),
        retryable: false,
        safe_for_model: false,
        error_code: Some(DeskErrorCode::AI_CONTENT_BLOCKED.code()),
    }
}

/// Expected stage helper used by parser call sites.
pub const fn input_stage() -> ContentSafetyStage {
    ContentSafetyStage::Input
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_errors_have_fixed_retry_semantics_and_codes() {
        let unavailable = content_safety_unavailable();
        assert_eq!(unavailable.kind, AgentErrorKind::ContentSafetyUnavailable);
        assert!(unavailable.retryable);
        assert!(!unavailable.safe_for_model);
        assert_eq!(
            unavailable.error_code,
            Some(DeskErrorCode::AI_CONTENT_SAFETY_UNAVAILABLE.code())
        );

        let image_unsupported = content_safety_image_unsupported();
        assert_eq!(
            image_unsupported.error_code,
            Some(DeskErrorCode::AI_CONTENT_SAFETY_IMAGE_UNSUPPORTED.code())
        );
        assert!(image_unsupported.retryable);
        assert!(!image_unsupported.safe_for_model);

        let blocked = content_blocked_error();
        assert_eq!(blocked.kind, AgentErrorKind::ContentBlocked);
        assert!(!blocked.retryable);
        assert!(!blocked.safe_for_model);
        assert_eq!(
            blocked.error_code,
            Some(DeskErrorCode::AI_CONTENT_BLOCKED.code())
        );
    }
}

/// Closed runtime mode. OSS signal and an explicitly disabled manager pass
/// `Disabled`; a protected manager turn must carry the complete frozen seam and
/// context and cannot silently degrade to a no-op implementation.
pub enum ContentSafetyMode<'a> {
    Disabled,
    Enforced {
        seam: &'a dyn ContentSafetySeam,
        context: SafetyContext,
    },
}

impl ContentSafetyMode<'_> {
    pub const fn is_enforced(&self) -> bool {
        matches!(self, Self::Enforced { .. })
    }
}

/// Collapse arbitrary seam failures to fixed, non-provider-controlled wire
/// errors. The image-capability code remains distinct; all other details are
/// deliberately discarded.
pub fn normalize_safety_error(error: &AgentError) -> AgentError {
    if error.error_code == Some(DeskErrorCode::AI_CONTENT_SAFETY_IMAGE_UNSUPPORTED.code()) {
        return content_safety_image_unsupported();
    }
    content_safety_unavailable()
}
