//! Shared text/vision provider-validation probe.

use crate::chat::{ChatMessage, ChatRole};
use crate::model_capability::ModelCapabilities;

const VISION_MARKER: &str = "LCXL7F";
const VISION_PROBE_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAASgAAABICAYAAABFhGj3AAAB8ElEQVR42u3dMY7DMAxFQd7/0vINZBcm8CnNA9LblDjbBNlakhRaGYEkQEkSoCQBSpIAJQlQkgQoSfoJqKrafsYP4OX9uj/p85/+/NPvR/f8458fUIACFKAA5QICClCAAhSgAAUoQAEKUIACFKAABShAAQpQgAIUoADlAgIKUIACFGADADl9/v5ANL8/oFxgQDlfQAEKUIACFKAABShAAQpQLrD5AwpQFgRQgAIUoFxgQDlfQAEKUIACFKAsyJr/RU/nc/f8AWUBzB9QgAIUoAAFKEBZEEA5H0ABygKYP6AAZUEA5XwABSgLYP6AAhSgAAUoQAHKggDK/NN/kA5QFgRQ5g8oF8T7mz+gAGVBAGX+gAKUBTF/QAHKggDK/AEFKAsCKPMHlAvi/c0fUICyIIAy3+P3F1CAMn9AAcoBAQpQng9QgAKU+wUoQFkg8wcUoCwIoMwXUIAClPkDClAOCFCA8nyAsiD+cSegAOUCAcr8AQUoCwIo9wtQgAIUoAAFKAcEKEC5/4CyIIACFKBcIECZP6AAZUEA5X4B6tMLjv9BrPDnn/5Fy9vvD6AABShAAQpQgAIUoAAFKEABClCAAhSgAAUoQAEKUIACFKAABShAAQpQgAIUoDKAkiRASRKgJAFKkgAlCVCSBChJApSk8B7YCriL5EqlXgAAAABJRU5ErkJggg==";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeExpectation {
    pub message: ChatMessage,
    pub max_output_tokens: u32,
    pub required_marker: Option<&'static str>,
}

impl ProbeExpectation {
    pub fn validated_capabilities(&self) -> Vec<String> {
        let mut capabilities = vec!["text".to_string()];
        if self.required_marker.is_some() {
            capabilities.push("image_input".to_string());
        }
        capabilities
    }
}

pub fn provider_probe_request(capabilities: ModelCapabilities) -> ProbeExpectation {
    if capabilities.image_input {
        ProbeExpectation {
            message: ChatMessage::text(
                "provider-vision-probe",
                ChatRole::User,
                "Read the exact six ASCII characters shown in the attached image and reply with those characters first. Do not guess from this instruction.",
            )
            .with_image(format!(
                "data:image/png;base64,{VISION_PROBE_PNG_BASE64}"
            )),
            max_output_tokens: 64,
            required_marker: Some(VISION_MARKER),
        }
    } else {
        ProbeExpectation {
            message: ChatMessage::text(
                "provider-text-probe",
                ChatRole::User,
                "Reply with the single word: pong",
            ),
            max_output_tokens: 16,
            required_marker: None,
        }
    }
}

pub fn verify_probe_response(expectation: &ProbeExpectation, content: &str) -> Result<(), String> {
    let content = content.trim();
    if content.is_empty() {
        return Err("provider returned an empty probe response".to_string());
    }
    match expectation.required_marker {
        Some(marker) if !content.to_ascii_uppercase().starts_with(marker) => {
            Err("provider response did not prove image access".to_string())
        }
        None if !content.eq_ignore_ascii_case("pong") => {
            Err("provider response did not match the fixed text probe token".to_string())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_probe_carries_owned_png_and_requires_marker() {
        let probe = provider_probe_request(ModelCapabilities { image_input: true });
        assert!(
            probe
                .message
                .image_data_url
                .as_deref()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert_eq!(probe.max_output_tokens, 64);
        assert!(verify_probe_response(&probe, "LCXL7F").is_ok());
        assert!(verify_probe_response(&probe, "I can see an image").is_err());
        assert_eq!(probe.validated_capabilities(), vec!["text", "image_input"]);
    }

    #[test]
    fn text_probe_only_requires_nonempty_response() {
        let probe = provider_probe_request(ModelCapabilities::default());
        assert!(probe.message.image_data_url.is_none());
        assert_eq!(probe.max_output_tokens, 16);
        assert!(verify_probe_response(&probe, " pong ").is_ok());
        assert!(verify_probe_response(&probe, "arbitrary provider prose").is_err());
        assert!(verify_probe_response(&probe, " ").is_err());
        assert_eq!(probe.validated_capabilities(), vec!["text"]);
    }
}
