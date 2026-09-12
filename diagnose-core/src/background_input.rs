//! Model contract for application-scoped background input.
use crate::{
    chat::ToolSpec,
    registry::{RegisteredTool, ToolEffect},
};
use desk_agent_protocol::Capability;
use serde_json::json;

pub const GUIDANCE: &str = "Use execute_background_inputs with application_id, window_id and 1–20 steps. Prefer execute_ui_actions; use background input only when semantic UI is impractical, combining it with a current application-window screenshot. Steps are sequential, stop at first failure and never roll back or automatically retry. No fixed inter-step delay; if a step depends on asynchronous UI changes, end the batch and read UI before continuing. Reuse application_scope for all background actions in the batch. Native dispatch success is not application-state verification. Read the target UI/window screenshot afterward. Never activate the app or move the real cursor. TextEdit background Command+A is known ineffective; choose an alternative. Private mouse routing is experimental.";

pub fn tool() -> RegisteredTool {
    let object = |kind: &str| json!({"type":"object","properties":{"token":{"type":"string"},"snapshot_id":{"type":"string"},"object_kind":{"const":kind},"expires_at":{"type":"string"}},"required":["token","snapshot_id","object_kind","expires_at"],"additionalProperties":false});
    let position = json!({"type":"object","description":"Pixel coordinates in the original window screenshot, origin at top-left (0,0). Use x < screenshot width and y < screenshot height. Values are pixels, not percentages or normalized coordinates.","properties":{"x":{"type":"integer","minimum":0,"maximum":u32::MAX},"y":{"type":"integer","minimum":0,"maximum":u32::MAX}},"required":["x","y"],"additionalProperties":false});
    let mut actions = Vec::new();
    for kind in ["click", "double_click", "scroll"] {
        let mut properties = json!({"kind":{"const":kind},"position":position,"element_id":{"type":"string","description":"Observed control ID used to locate mouse coordinates, not an AX action"}});
        let mut required = vec!["kind"];
        if kind == "scroll" {
            properties["horizontal_pixels"] = json!({"type":"integer","minimum":-10000,"maximum":10000,"description":"Pixel distance, not wheel ticks or lines; zero means no horizontal scrolling"});
            properties["vertical_pixels"] = json!({"type":"integer","minimum":-10000,"maximum":10000,"description":"Pixel distance, NOT wheel ticks or lines. Positive up, negative down. For example -300 scrolls down 300 pixels; -6 moves only 6 pixels."});
            required.extend(["horizontal_pixels", "vertical_pixels"]);
        }
        actions.push(json!({"type":"object","properties":properties,"required":required,"oneOf":[{"required":["position"]},{"required":["element_id"]}],"additionalProperties":false}));
    }
    actions.push(json!({"type":"object","properties":{"kind":{"const":"type_text"},"text":{"type":"string","minLength":1,"maxLength":16384}},"required":["kind","text"],"additionalProperties":false}));
    actions.push(json!({"type":"object","properties":{"kind":{"const":"key_press"},"key":{"type":"string","description":"Letter/digit, Enter, Tab, Escape, Backspace, Delete, Space, ArrowLeft/Right/Up/Down, Home, End, PageUp/Down, F1–F12"},"modifiers":{"type":"array","maxItems":4,"uniqueItems":true,"items":{"enum":["Command","Control","Option","Shift"]}}},"required":["kind","key"],"additionalProperties":false}));
    RegisteredTool {
        spec: ToolSpec {
            name: "execute_background_inputs".into(),
            description: GUIDANCE.trim().into(),
            parameters_schema: json!({"type":"object","properties":{"application":object("application"),"target":object("window"),"action":{"oneOf":actions}},"required":["application","target","action"],"additionalProperties":false}),
        },
        required_capability: Capability::DesktopBackgroundInputConfirmed,
        effect: ToolEffect::Mutating,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pixel_coordinate_schema_preserves_scroll_distance_limits() {
        let schema = tool().spec.parameters_schema;
        let actions = &schema["properties"]["action"]["oneOf"];
        assert_eq!(
            actions[0]["properties"]["position"]["properties"]["x"]["maximum"],
            u32::MAX
        );
        assert!(
            actions[0]["properties"]["position"]["description"]
                .as_str()
                .unwrap()
                .contains("Pixel coordinates")
        );
        assert_eq!(
            actions[2]["properties"]["vertical_pixels"]["maximum"],
            10000
        );
        assert_eq!(
            actions[2]["properties"]["horizontal_pixels"]["minimum"],
            -10000
        );
    }
}
