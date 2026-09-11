//! Model contract for application-scoped background input.
use crate::{
    chat::ToolSpec,
    registry::{RegisteredTool, ToolEffect},
};
use desk_agent_protocol::Capability;
use serde_json::json;

pub const GUIDANCE: &str = "\nexecute_background_input delivers one mouse/keyboard action to a macOS application without activating it or moving the real cursor. Prefer semantic UI actions through execute_confirmed_ui_action. Use background input only when semantic UI cannot express the needed operation or is demonstrably impractical or unreliable. In that case, combine it with a current application-window screenshot from read_current_screen (window_id) to guide input, refreshing the screenshot when needed. Do not choose background input when a suitable semantic UI action can accomplish the task. Request application_scope with click/double_click/scroll/type_text/key_press actions, then reuse the grant. Pass application_id and window_id; mouse actions require exactly one observed element_id or position {x,y} normalized 0–1000 within the window screenshot. The server resolves geometry. Keyboard requires the target window to be the application's input window; a different application may remain foreground. Unicode text uses type_text; shortcuts use key_press and named modifiers. Read the UI or window screenshot after dispatch to verify the result. Dispatched is not semantic success. Background Command+A is known not to work in TextEdit on the tested macOS version; navigation/selection shortcuts may work. Never silently activate the app, repeat an ineffective input indefinitely, or ask the user to acknowledge a failed record. Mouse is experimental and its private window-routing API is checked at dispatch; unavailability of mouse does not imply keyboard is unavailable. Left-button click/double-click only. This tool is single-action, not a batch.\n";

pub fn tool() -> RegisteredTool {
    let object = |kind: &str| json!({"type":"object","properties":{"token":{"type":"string"},"snapshot_id":{"type":"string"},"object_kind":{"const":kind},"expires_at":{"type":"string"}},"required":["token","snapshot_id","object_kind","expires_at"],"additionalProperties":false});
    let position = json!({"type":"object","properties":{"x":{"type":"integer","minimum":0,"maximum":1000},"y":{"type":"integer","minimum":0,"maximum":1000}},"required":["x","y"],"additionalProperties":false});
    let mut actions = Vec::new();
    for kind in ["click", "double_click", "scroll"] {
        let mut properties = json!({"kind":{"const":kind},"position":position,"element_id":{"type":"string","description":"Observed control ID used to locate mouse coordinates, not an AX action"}});
        let mut required = vec!["kind"];
        if kind == "scroll" {
            properties["horizontal"] = json!({"type":"integer","minimum":-10000,"maximum":10000});
            properties["vertical"] = json!({"type":"integer","minimum":-10000,"maximum":10000,"description":"Pixels; positive up, negative down"});
            required.extend(["horizontal", "vertical"]);
        }
        actions.push(json!({"type":"object","properties":properties,"required":required,"oneOf":[{"required":["position"]},{"required":["element_id"]}],"additionalProperties":false}));
    }
    actions.push(json!({"type":"object","properties":{"kind":{"const":"type_text"},"text":{"type":"string","minLength":1,"maxLength":16384}},"required":["kind","text"],"additionalProperties":false}));
    actions.push(json!({"type":"object","properties":{"kind":{"const":"key_press"},"key":{"type":"string","description":"Letter/digit, Enter, Tab, Escape, Backspace, Delete, Space, ArrowLeft/Right/Up/Down, Home, End, PageUp/Down, F1–F12"},"modifiers":{"type":"array","maxItems":4,"uniqueItems":true,"items":{"enum":["Command","Control","Option","Shift"]}}},"required":["kind","key"],"additionalProperties":false}));
    RegisteredTool {
        spec: ToolSpec {
            name: "execute_background_input".into(),
            description: GUIDANCE.trim().into(),
            parameters_schema: json!({"type":"object","properties":{"application":object("application"),"target":object("window"),"action":{"oneOf":actions}},"required":["application","target","action"],"additionalProperties":false}),
        },
        required_capability: Capability::DesktopBackgroundInputConfirmed,
        effect: ToolEffect::Mutating,
    }
}
