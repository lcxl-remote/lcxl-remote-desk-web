//! Provider-neutral model capability and request requirement contracts.

use desk_agent_protocol::Capability;

use crate::chat::ChatMessage;
use crate::registry::RegisteredTool;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub image_input: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelRequirements {
    pub image_input: bool,
}

impl ModelRequirements {
    pub const TEXT_ONLY: Self = Self { image_input: false };
    pub const IMAGE_INPUT: Self = Self { image_input: true };

    pub fn for_messages(messages: &[ChatMessage]) -> Self {
        Self {
            image_input: messages
                .iter()
                .any(|message| message.image_data_url.is_some()),
        }
    }
}

impl ModelCapabilities {
    pub fn satisfies(self, requirements: ModelRequirements) -> bool {
        !requirements.image_input || self.image_input
    }
}

/// Remove tools that can produce a visual attachment when the selected model is
/// text-only. The same filtered registry must be used both for advertising and
/// returned-call lookup.
pub fn filter_model_compatible_tools(
    registry: &[RegisteredTool],
    capabilities: ModelCapabilities,
) -> Vec<RegisteredTool> {
    registry
        .iter()
        .filter(|tool| {
            capabilities.image_input || tool.required_capability != Capability::ScreenCaptureCurrent
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChatMessage, ChatRole};
    use crate::read_tools::read_tool_registry;

    #[test]
    fn requirements_follow_actual_message_images() {
        let text = vec![ChatMessage::text("m1", ChatRole::User, "hello")];
        assert_eq!(
            ModelRequirements::for_messages(&text),
            ModelRequirements::TEXT_ONLY
        );
        let visual = vec![
            ChatMessage::text("m2", ChatRole::User, "screen")
                .with_image("data:image/jpeg;base64,AQID"),
        ];
        assert_eq!(
            ModelRequirements::for_messages(&visual),
            ModelRequirements::IMAGE_INPUT
        );
    }

    #[test]
    fn capabilities_satisfy_only_supported_requirements() {
        assert!(ModelCapabilities::default().satisfies(ModelRequirements::TEXT_ONLY));
        assert!(!ModelCapabilities::default().satisfies(ModelRequirements::IMAGE_INPUT));
        assert!(ModelCapabilities { image_input: true }.satisfies(ModelRequirements::IMAGE_INPUT));
    }

    #[test]
    fn text_model_filters_only_visual_read_tool() {
        let all = read_tool_registry();
        let filtered = filter_model_compatible_tools(&all, ModelCapabilities::default());
        assert!(all.iter().any(|tool| tool.name() == "read_current_screen"));
        assert!(
            !filtered
                .iter()
                .any(|tool| tool.name() == "read_current_screen")
        );
        assert!(
            filtered
                .iter()
                .any(|tool| tool.name() == "read_system_info")
        );
    }
}
