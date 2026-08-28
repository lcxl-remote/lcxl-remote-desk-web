//! Strict JSON prompts for input, complete model-turn, and image review.

use serde::Serialize;

use super::policy::POLITICS_POLICY;
use super::seam::{SafetyImage, SafetyInput, SafetyModelTurn};

pub const CONTENT_SAFETY_PROMPT_VERSION: &str = "content-safety-v1";

/// Provider-neutral prompt pair. Image bytes are attached separately by the
/// manager seam and therefore cannot accidentally enter text logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafetyPrompt {
    pub system: String,
    pub user: String,
}

fn system_prompt(allowed_stages: &str) -> String {
    format!(
        "You are a deterministic content-safety classifier for a remote-device AI product.\n\
         Policy version: {CONTENT_SAFETY_PROMPT_VERSION}.\n\
         Review sexual content (including minors), violence and graphic violence, \
         violent wrongdoing, hate, threatening harassment, self-harm and self-harm \
         instructions, illicit operational guidance, and politics.\n\
         Self-harm or self-harm-instruction content uses safe_redirect unless another \
         blocked category is also present. All other matched categories use block.\n\
         Politics boundary: {POLITICS_POLICY}\n\
         Legitimate defensive security, troubleshooting, incident response, and \
         forensic work are allowed when they do not provide disallowed real-world \
         harm, illicit execution, or political persuasion.\n\
         Return exactly one JSON object and no markdown or prose. Required keys are \
         decision, categories, stages, policy_version. additionalProperties are \
         forbidden. decision is allow, block, or safe_redirect. categories may contain \
         only sexual, sexual_minors, violence, graphic_violence, violent_wrongdoing, \
         hate, threatening_harassment, self_harm, self_harm_instructions, illicit, or \
         politics. stages may contain only {allowed_stages}. For allow, categories and \
         stages must both be empty. For block/safe_redirect, both must be non-empty. \
         policy_version must equal {CONTENT_SAFETY_PROMPT_VERSION}."
    )
}

fn json_user<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("safety prompt DTO is serializable")
}

pub fn build_input_prompt(request: &SafetyInput) -> SafetyPrompt {
    SafetyPrompt {
        system: system_prompt("input"),
        user: json_user(request),
    }
}

pub fn build_model_turn_prompt(request: &SafetyModelTurn) -> SafetyPrompt {
    SafetyPrompt {
        system: system_prompt("output or action"),
        user: json_user(request),
    }
}

pub fn build_image_prompt(request: &SafetyImage) -> SafetyPrompt {
    #[derive(Serialize)]
    struct ImageMetadata<'a> {
        surface: desk_agent_protocol::content_safety::ContentSafetySurface,
        mime_type: &'a str,
        original_allowed_intent: &'a str,
        image_attached_separately: bool,
    }

    SafetyPrompt {
        system: system_prompt("image"),
        user: json_user(&ImageMetadata {
            surface: request.surface,
            mime_type: &request.mime_type,
            original_allowed_intent: &request.original_allowed_intent,
            image_attached_separately: true,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ToolEffect;
    use desk_agent_protocol::content_safety::ContentSafetySurface;

    #[test]
    fn prompt_freezes_politics_technical_exception_and_strict_schema() {
        let prompt = build_input_prompt(&SafetyInput {
            surface: ContentSafetySurface::AssistantAnswer,
            text: "Why does this government site fail TLS?".into(),
            trusted_context: None,
        });
        assert!(prompt.system.contains("incidental technical object"));
        assert!(prompt.system.contains("additionalProperties are forbidden"));
        assert!(prompt.system.contains(CONTENT_SAFETY_PROMPT_VERSION));
        assert!(prompt.user.contains("government site"));
    }

    #[test]
    fn model_turn_prompt_contains_only_intent_output_and_normalized_actions() {
        let prompt = build_model_turn_prompt(&SafetyModelTurn {
            surface: ContentSafetySurface::TerminalCopilot,
            text: "check the service".into(),
            tool_calls: vec![super::super::seam::SafetyToolCall {
                name: "exec_command".into(),
                effect: ToolEffect::ReadOnly,
                canonical_arguments_json: r#"{"command":"systemctl status sshd"}"#.into(),
            }],
            original_allowed_intent: "diagnose ssh".into(),
        });
        assert!(prompt.user.contains("canonical_arguments_json"));
        assert!(!prompt.user.contains("tool_output"));
        assert!(prompt.system.contains("output or action"));
    }

    #[test]
    fn documentation_support_surface_uses_the_frozen_wire_token() {
        let prompt = build_input_prompt(&SafetyInput {
            surface: ContentSafetySurface::DocumentationSupport,
            text: "How do I configure unattended access?".into(),
            trusted_context: None,
        });
        assert!(
            prompt
                .user
                .contains("\"surface\":\"documentation_support\"")
        );
    }

    #[test]
    fn image_prompt_never_serializes_the_data_url() {
        let request = SafetyImage {
            surface: ContentSafetySurface::AssistantAnswer,
            image_data_url: "data:image/png;base64,SECRET_BYTES".into(),
            mime_type: "image/png".into(),
            original_allowed_intent: "inspect the error dialog".into(),
        };
        let prompt = build_image_prompt(&request);
        assert!(!prompt.user.contains("SECRET_BYTES"));
        assert!(prompt.user.contains("image_attached_separately"));
    }
}
