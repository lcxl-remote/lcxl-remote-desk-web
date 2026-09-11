//! Model-facing desktop IDs. Full references remain in durable server history.

use crate::chat::{ChatMessage, ChatRole, ToolCall};
use desk_agent_protocol::computer_use::{ObjectKind, ObjectRef};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use serde_json::{Value, json};

fn invalid(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

fn fields(tool: &str) -> &'static [(&'static str, &'static str)] {
    match tool {
        "execute_ui_actions" => &[("application", "application_id"), ("target", "element_id")],
        "inspect_desktop_ui" => &[("root", "root_id")],
        "execute_background_inputs" => {
            &[("application", "application_id"), ("target", "window_id")]
        }
        "execute_confirmed_raw_input" => &[("target", "application_id")],
        "read_current_screen" => &[("window", "window_id")],
        _ => &[],
    }
}

pub fn needs_resolution(tool: &str) -> bool {
    !fields(tool).is_empty() || tool == "request_capability_grants"
}

fn references(value: &Value, result: &mut Vec<ObjectRef>) {
    match value {
        Value::Object(object) => {
            if object.contains_key("token") && object.contains_key("snapshot_id") {
                if let Ok(reference) = serde_json::from_value::<ObjectRef>(value.clone()) {
                    result.push(reference);
                }
            } else {
                for value in object.values() {
                    references(value, result);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                references(value, result);
            }
        }
        _ => {}
    }
}

fn observation_value(text: &str) -> Option<Value> {
    let mut value: Value = serde_json::from_str(text).ok()?;
    if value.pointer("/ReadContext/DesktopUiInspect").is_none()
        && value
            .pointer("/ReadContext/DesktopSessionInspect")
            .is_none()
    {
        return None;
    }
    crate::ui_model_output::expand_value(&mut value);
    Some(value)
}

/// Only observations in this conversation can supply references. A partial read
/// updates its own IDs, never the metadata of unrelated controls.
fn resolve(history: &[ChatMessage], id: &str, _now_ms: u64) -> Result<ObjectRef, AgentError> {
    if id.is_empty() || id.len() > 512 {
        return Err(invalid(
            "Object ID must be a nonempty observed ID (at most 512 bytes).",
        ));
    }
    for message in history.iter().rev().filter(|m| m.role == ChatRole::Tool) {
        let Some(call_id) = message.tool_call_id.as_deref() else {
            continue;
        };
        let observed_by = history
            .iter()
            .filter(|m| m.role == ChatRole::Assistant)
            .flat_map(|m| &m.tool_calls)
            .find(|call| call.id == call_id)
            .map(|call| call.name.as_str());
        if !matches!(
            observed_by,
            Some("inspect_desktop_ui" | "inspect_desktop_session")
        ) {
            continue;
        }
        let Some(output) = observation_value(&message.text) else {
            continue;
        };
        let mut observed = Vec::new();
        references(&output, &mut observed);
        if let Some(reference) = observed.into_iter().find(|r| r.token == id) {
            return Ok(reference);
        }
    }
    Err(invalid(
        "Object ID was not observed in this conversation. Read the desktop/UI to obtain an ID; do not invent IDs. No action was executed.",
    ))
}

/// Resolve model arguments before existing typed preflight and authorization.
/// Original model calls and their signed envelopes are never rewritten.
pub fn resolve_call(
    call: &ToolCall,
    history: &[ChatMessage],
    now_ms: u64,
) -> Result<ToolCall, AgentError> {
    if crate::application_batch::supports(&call.name) {
        return crate::application_batch::resolve(call, history, now_ms);
    }
    resolve_single_call(call, history, now_ms)
}

pub(crate) fn resolve_single_call(
    call: &ToolCall,
    history: &[ChatMessage],
    now_ms: u64,
) -> Result<ToolCall, AgentError> {
    if !needs_resolution(&call.name) {
        return Ok(call.clone());
    }
    let mut value: Value = serde_json::from_str(&call.arguments_json)
        .map_err(|_| invalid("Tool arguments must be a JSON object."))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid("Tool arguments must be a JSON object."))?;
    for (internal, model) in fields(&call.name) {
        if object.contains_key(*internal) {
            return Err(invalid(format!(
                "Use {model} with an observed ID, not {internal} or a full reference. The server supplies and validates reference metadata. No action was executed."
            )));
        }
        let Some(id) = object.remove(*model) else {
            continue;
        };
        if id.is_null() && call.name != "execute_ui_actions" {
            continue;
        }
        let id = id
            .as_str()
            .ok_or_else(|| invalid(format!("{model} must be an observed ID string.")))?;
        let reference = resolve(history, id, now_ms)?;
        let expected = match *model {
            "application_id" => Some(ObjectKind::Application),
            "element_id" => Some(ObjectKind::UiElement),
            "window_id" => Some(ObjectKind::Window),
            _ => None,
        };
        if expected.is_some_and(|kind| kind != reference.object_kind) {
            return Err(invalid(format!(
                "{model} identifies the wrong object kind. Use the corresponding observed application, control or window ID."
            )));
        }
        object.insert((*internal).into(), serde_json::to_value(reference).unwrap());
    }
    if call.name == "execute_ui_actions"
        && (!object.contains_key("application") || !object.contains_key("target"))
    {
        return Err(invalid(
            r#"Required shape: {"application_id":"<application ID>","element_id":"<control ID>","action":{"kind":"invoke"}}. set_value uses {"kind":"set_value","params":{"value":"text"}}. No action was executed."#,
        ));
    }
    if call.name == "execute_background_inputs" {
        if object.contains_key("geometry") {
            return Err(invalid(
                "The server supplies window geometry; do not provide geometry",
            ));
        }
        let window = object.get("target").cloned().unwrap_or(Value::Null);
        let mut geometry = Value::Null;
        for message in history
            .iter()
            .rev()
            .filter(|m| m.role == crate::chat::ChatRole::Tool)
        {
            let from_capture = message.tool_call_id.as_ref().is_some_and(|id| {
                history
                    .iter()
                    .filter(|m| m.role == ChatRole::Assistant)
                    .flat_map(|m| &m.tool_calls)
                    .any(|c| &c.id == id && c.name == "read_current_screen")
            });
            if !from_capture {
                continue;
            }
            let Ok(output) = serde_json::from_str::<Value>(&message.text) else {
                continue;
            };
            if let Some(frame) = output.pointer("/ReadContext/ScreenCaptureCurrent") {
                if frame.get("window") == Some(&window) {
                    geometry = frame.get("window_geometry").cloned().unwrap_or(Value::Null);
                    break;
                }
            }
        }
        object.insert("geometry".into(), geometry);
        if let Some(action) = object.get_mut("action").and_then(Value::as_object_mut) {
            if action.contains_key("element") {
                return Err(invalid(
                    "Use action.element_id, not a full element reference",
                ));
            }
            if let Some(id) = action.remove("element_id") {
                let reference = resolve(
                    history,
                    id.as_str()
                        .ok_or_else(|| invalid("element_id must be a string"))?,
                    now_ms,
                )?;
                if reference.object_kind != ObjectKind::UiElement {
                    return Err(invalid("element_id must identify a UI element"));
                }
                action.insert("element".into(), json!(reference));
            }
        }
    }
    if call.name == "request_capability_grants" {
        if let Some(items) = object.get_mut("items").and_then(Value::as_array_mut) {
            for item in items {
                if item["tool_name"] == "execute_confirmed_raw_input" {
                    if let Some(exact) = item.get_mut("exact_input") {
                        let nested = ToolCall {
                            id: call.id.clone(),
                            name: "execute_confirmed_raw_input".into(),
                            arguments_json: exact.to_string(),
                        };
                        *exact = serde_json::from_str(
                            &resolve_call(&nested, history, now_ms)?.arguments_json,
                        )
                        .unwrap();
                    }
                    continue;
                }
                if !matches!(
                    item["tool_name"].as_str(),
                    Some("execute_ui_actions" | "execute_background_inputs")
                ) {
                    continue;
                }
                let scope = item.get_mut("application_scope").and_then(Value::as_object_mut)
                    .ok_or_else(|| invalid(r#"Native UI permission requires application_scope: {"application_id":"<observed application ID>","actions":["invoke","set_value"]}. No approval card was created."#))?;
                if scope.contains_key("application") {
                    return Err(invalid(
                        "Use application_scope.application_id, not a full application reference. No approval card was created.",
                    ));
                }
                let id = scope.remove("application_id").ok_or_else(|| invalid("application_scope.application_id is required. No approval card was created."))?;
                let reference = resolve(
                    history,
                    id.as_str()
                        .ok_or_else(|| invalid("application_id must be a string."))?,
                    now_ms,
                )?;
                if reference.object_kind != ObjectKind::Application {
                    return Err(invalid(
                        "application_id must identify an observed application. No approval card was created.",
                    ));
                }
                scope.insert(
                    "application".into(),
                    serde_json::to_value(reference).unwrap(),
                );
            }
        }
    }
    Ok(ToolCall {
        arguments_json: value.to_string(),
        ..call.clone()
    })
}

fn project_arguments(tool: &str, value: &mut Value) {
    if crate::application_batch::supports(tool) && value.get("remaining_steps").is_some() {
        // Failed model calls are also replayed; only invert the trusted shape.
        let Some(tail) = value.get("remaining_steps").and_then(Value::as_array) else {
            return;
        };
        if tail.len() >= crate::application_batch::MAX_STEPS
            || !value.get("application").is_some_and(Value::is_object)
            || tail
                .iter()
                .any(|item| !item.is_object() || item.get("remaining_steps").is_some())
        {
            return;
        }
        let mut first = value.clone();
        let rest = first
            .as_object_mut()
            .unwrap()
            .remove("remaining_steps")
            .unwrap();
        let mut all = vec![first];
        all.extend(rest.as_array().unwrap().iter().cloned());
        for item in &mut all {
            project_arguments(tool, item);
        }
        let mut result = json!({"application_id":all[0]["application_id"],"steps":[]});
        if tool == "execute_background_inputs" {
            result["window_id"] = all[0]["window_id"].clone();
        }
        for item in &mut all {
            let obj = item.as_object_mut().unwrap();
            obj.remove("application_id");
            obj.remove("window_id");
        }
        result["steps"] = json!(all);
        *value = result;
        return;
    }
    if tool == "execute_background_inputs" {
        if let Some(object) = value.as_object_mut() {
            object.remove("geometry");
        }
        if let Some(action) = value.get_mut("action").and_then(Value::as_object_mut) {
            if let Some(element) = action.remove("element") {
                action.insert("element_id".into(), element["token"].clone());
            }
        }
    }

    if let Some(object) = value.as_object_mut() {
        for (internal, model) in fields(tool) {
            if let Some(reference) = object.remove(*internal) {
                object.insert(
                    (*model).into(),
                    reference.get("token").cloned().unwrap_or(Value::Null),
                );
            }
        }
        if tool == "request_capability_grants" {
            if let Some(items) = object.get_mut("items").and_then(Value::as_array_mut) {
                for item in items {
                    project_scope(item);
                    if item["tool_name"] == "execute_confirmed_raw_input" {
                        if let Some(exact) = item.get_mut("exact_input") {
                            project_arguments("execute_confirmed_raw_input", exact);
                        }
                    }
                }
            }
        }
    }
}

fn project_scope(value: &mut Value) {
    if let Some(scope) = value
        .get_mut("application_scope")
        .and_then(Value::as_object_mut)
    {
        if let Some(reference) = scope.remove("application") {
            scope.insert("application_id".into(), reference["token"].clone());
        }
    }
}

/// Compare a server-resolved call with the original ID-only model proposal.
/// All non-reference inputs, including the action, must still match exactly.
pub fn same_call_input(tool: &str, original: &str, resolved: &str) -> bool {
    let (Ok(left), Ok(mut right)) = (
        serde_json::from_str::<Value>(original),
        serde_json::from_str::<Value>(resolved),
    ) else {
        return false;
    };
    if left == right {
        return true;
    }
    project_arguments(tool, &mut right);
    left == right
}

fn hide_references(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if object.contains_key("token")
                && object.contains_key("snapshot_id")
                && object.contains_key("object_kind")
                && object.contains_key("expires_at")
            {
                let id = object["token"].clone();
                let kind = object["object_kind"].clone();
                *value = json!({"id":id,"kind":kind});
                return;
            }
            object.remove("snapshot_id");
            for value in object.values_mut() {
                hide_references(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                hide_references(value);
            }
        }
        _ => {}
    }
}

/// A reducing wire projection after the original messages passed model-egress
/// checks. Durable messages/envelopes remain unchanged and retain exact lineage.
pub fn project_request(request: &crate::seam::ModelRequest) -> crate::seam::ModelRequest {
    let mut request = request.clone();
    for message in &mut request.messages {
        if message.role == ChatRole::Tool {
            if let Some(mut value) = observation_value(&message.text) {
                crate::ui_model_output::add_window_discovery_hint(&mut value);
                hide_references(&mut value);
                // Omit default node fields without removing useful semantic IDs.
                if let Some(nodes) = value
                    .pointer_mut("/ReadContext/DesktopUiInspect/nodes")
                    .and_then(Value::as_array_mut)
                {
                    for node in nodes {
                        let fields = node.as_object_mut().unwrap();
                        fields.retain(|key, value| {
                            !value.is_null()
                                && !(key == "supported_actions"
                                    && value.as_array().is_some_and(Vec::is_empty))
                        });
                    }
                }
                message.text = value.to_string();
            }
        }
        for call in &mut message.tool_calls {
            if let Ok(mut value) = serde_json::from_str::<Value>(&call.arguments_json) {
                project_arguments(&call.name, &mut value);
                call.arguments_json = value.to_string();
            }
        }
        if message.role == ChatRole::System {
            let (start_tag, end_tag) =
                ("<capability_authorization>", "</capability_authorization>");
            if let Some(start) = message.text.find(start_tag).map(|i| i + start_tag.len()) {
                if let Some(end) = message.text[start..].find(end_tag).map(|i| i + start) {
                    if let Ok(mut entries) =
                        serde_json::from_str::<Vec<Value>>(&message.text[start..end])
                    {
                        for entry in &mut entries {
                            project_scope(entry);
                            if entry["tool_name"] == "execute_confirmed_raw_input" {
                                if let Some(exact) = entry.get_mut("approved_exact_input") {
                                    project_arguments("execute_confirmed_raw_input", exact);
                                }
                            }
                        }
                        message
                            .text
                            .replace_range(start..end, &serde_json::to_string(&entries).unwrap());
                    }
                }
            }
        }
    }
    for tool in &mut request.tools {
        project_tool(tool);
    }
    request
}

pub fn project_tool(tool: &mut crate::chat::ToolSpec) {
    if crate::application_batch::supports(&tool.name)
        && tool
            .parameters_schema
            .pointer("/properties/steps")
            .is_some()
    {
        return;
    }
    let schema = &mut tool.parameters_schema;
    for (internal, model) in fields(&tool.name) {
        if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            properties.remove(*internal);
            properties.insert((*model).into(), json!({"type":"string","minLength":1,"maxLength":512,"description":"Copy the observed object ID. The server resolves the reference and checks native object lifetime."}));
        }
        if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
            for field in required {
                if field == internal {
                    *field = json!(model);
                }
            }
        }
    }
    if tool.name == "request_capability_grants" {
        if let Some(scope) =
            schema.pointer_mut("/properties/items/items/properties/application_scope")
        {
            scope["properties"]
                .as_object_mut()
                .unwrap()
                .remove("application");
            scope["properties"]["application_id"] =
                json!({"type":"string","minLength":1,"maxLength":512});
            scope["required"] = json!(["application_id", "actions"]);
            scope["description"] = json!(
                "Native UI permission requires the observed application_id and needed actions. The server supplies all reference metadata. Do not supply exact_input."
            );
        }
    }
    match tool.name.as_str() {
        "inspect_desktop_ui" => tool.description = "Read UI using optional root_id (desktop session, application, window or control). For macOS app tasks, first search running apps using the session root and localized/English queries, then use the returned application ID as root_id to read controls or discover windows with queries=[窗口, window]. The application catalog does not inspect windows; missing window entries do not mean capture is unavailable. Use owner_selectable_windows[].object_ref.id as the screenshot window_id. If a complete app search has no match, launch through an authorized tool and search again; increasing UI depth cannot find a non-running app. Application entries expose application_state=foreground/background/hidden when known and omit matched_queries. Without root_id, observe the foreground application. Supply queries or element_id. For queries combine localized and English labels/native identifiers/control types, at most 16 alternatives (e.g. 日期, 时间, date, time, input). Or use a control root_id with element_only=true. Only explicitly use allow_unfiltered=true when targeted searches are insufficient. Use scope=menus for menus only. Returned object_ref contains only id and kind. The server validates IDs and reports invalidated objects; element_id can locate a known control. Reads require permission and never grant actions.".into(),
        "execute_confirmed_raw_input" => tool.description = "Execute one last-resort typed mouse/keyboard step using the observed foreground application_id. Requires an exact-input one-use grant for application_id, screen geometry and action. The server resolves the reference and checks native object lifetime and authorization. Do not provide reference metadata.".into(),
        "read_current_screen" => tool.description = "Capture the current display, or use window_id to capture a background macOS window. To obtain window_id: inspect_desktop_session -> inspect_desktop_ui(root_id=session ID, queries=[localized app name, English app name]) -> inspect_desktop_ui(root_id=returned application ID, queries=[窗口, window]) -> read_current_screen(window_id=owner_selectable_windows[].object_ref.id). The application catalog does not query windows; never infer capture is unsupported from missing window entries there. Do not pass an application ID as window_id. Requires screen capture authorization. The server resolves the window reference and checks native object lifetime. Minimized windows require restoration before capture.".into(),
        _ => {}
    }
    crate::application_batch::project_schema(tool);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ToolCallRef;
    #[test]
    fn screenshot_tools_explain_application_to_window_discovery() {
        let tools = crate::read_tools::read_tool_registry()
            .into_iter()
            .chain(crate::read_tools::device_assistant_read_tool_registry());
        let mut checked = std::collections::HashSet::new();
        for registered in tools {
            let mut tool = registered.spec;
            if matches!(
                tool.name.as_str(),
                "read_current_screen" | "inspect_desktop_ui"
            ) {
                project_tool(&mut tool);
                assert!(tool.description.contains("owner_selectable_windows"));
                assert!(tool.description.contains("window_id"));
                assert!(tool.description.contains("queries="));
                checked.insert(tool.name);
            }
        }
        assert_eq!(checked.len(), 2);
    }

    fn reference(id: &str, snapshot: &str, kind: &str, expiry: &str) -> Value {
        json!({"token":id,"snapshot_id":snapshot,"object_kind":kind,"expires_at":expiry})
    }
    fn observation(id: &str, snapshot: &str, expiry: &str) -> ChatMessage {
        ChatMessage::tool_result(format!("result-{snapshot}"), snapshot, json!({"ReadContext":{"DesktopUiInspect":{
            "snapshot_id":snapshot,"adapter":{"kind":"macos_accessibility","version":"test"},"nodes":[{
                "element_id":id,"object_ref":reference(id,snapshot,"ui_element",expiry),"role":"AXButton","name":"Date","value":null,"enabled":true,"is_protected":false,"supported_actions":["invoke"]
            }],"owner_selectable_windows":[],"truncated":false
        }}}).to_string())
    }
    fn history() -> Vec<ChatMessage> {
        let mut messages = vec![ChatMessage::tool_result("desktop-result", "desktop-call", json!({"ReadContext":{"DesktopSessionInspect":{
            "session":reference("session","desktop","desktop_session","2030-01-01T00:00:00Z"),"os":"macos","interactive_session_incarnation":"worker",
            "active_application":reference("calendar","desktop","application","2030-01-01T00:00:00Z"),"active_application_name":"Calendar"
        }}}).to_string()), observation("date", "four", "2030-01-01T00:00:00Z"), observation("title", "five", "2030-01-01T00:00:00Z")];
        let calls = messages
            .iter()
            .map(|m| ToolCallRef {
                id: m.tool_call_id.clone().unwrap(),
                name: if m.message_id == "desktop-result" {
                    "inspect_desktop_session"
                } else {
                    "inspect_desktop_ui"
                }
                .into(),
                arguments_json: "{}".into(),
            })
            .collect();
        messages.insert(0, ChatMessage::assistant_tool_calls("reads", "", calls));
        messages
    }
    fn call(tool: &str, mut input: Value) -> ToolCall {
        if crate::application_batch::supports(tool)
            && input.get("action").is_some()
            && input.get("application_id").is_some()
        {
            let mut step =
                json!({"action":input.as_object_mut().unwrap().remove("action").unwrap()});
            if let Some(element) = input.as_object_mut().unwrap().remove("element_id") {
                step["element_id"] = element;
            }
            input["steps"] = json!([step]);
        }
        ToolCall {
            id: "action".into(),
            name: tool.into(),
            arguments_json: input.to_string(),
        }
    }

    #[test]
    fn partial_read_does_not_replace_other_controls_reference() {
        let original = call(
            "execute_ui_actions",
            json!({"application_id":"calendar","element_id":"date","action":{"kind":"invoke"}}),
        );
        let resolved = resolve_call(&original, &history(), 1).unwrap();
        let input: Value = serde_json::from_str(&resolved.arguments_json).unwrap();
        assert_eq!(input["target"]["snapshot_id"], "four");
        assert_eq!(input["application"]["snapshot_id"], "desktop");
        assert!(same_call_input(
            &original.name,
            &original.arguments_json,
            &resolved.arguments_json
        ));
        let mut wrong = input.clone();
        wrong["action"] = json!({"kind":"focus"});
        assert!(!same_call_input(
            &original.name,
            &original.arguments_json,
            &wrong.to_string()
        ));
        let registry = crate::device_assistant::device_assistant_provider_registry();
        crate::provider_preflight::UiCallPreflight::build(
            &registry,
            desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
            &resolved,
            1,
        )
        .unwrap();
    }

    #[test]
    fn desktop_calls_accept_references_without_expiry() {
        let mut messages = history();
        for message in &mut messages {
            message.text = message.text.replace("2030-01-01T00:00:00Z", "");
        }
        let action = call(
            "execute_ui_actions",
            json!({"application_id":"calendar","element_id":"date","action":{"kind":"invoke"}}),
        );
        let resolved = resolve_call(&action, &messages, 2_000_000_000_000).unwrap();
        let registry = crate::device_assistant::device_assistant_provider_registry();
        crate::provider_preflight::UiCallPreflight::build(
            &registry,
            desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
            &resolved,
            2_000_000_000_000,
        )
        .unwrap();
    }

    #[test]
    fn ids_are_conversation_bound_without_a_clock_deadline() {
        let original = call(
            "inspect_desktop_ui",
            json!({"root_id":"date","element_only":true}),
        );
        assert!(
            resolve_call(&original, &[], 1)
                .unwrap_err()
                .message
                .contains("not observed")
        );
        assert!(resolve_call(&original, &history(), 2_000_000_000_000).is_ok());
        let forged = call(
            "inspect_desktop_ui",
            json!({"root":reference("date","five","ui_element","2099-01-01T00:00:00Z"),"element_only":true}),
        );
        assert!(
            resolve_call(&forged, &history(), 1)
                .unwrap_err()
                .message
                .contains("root_id")
        );
        let wrong_kind = call("read_current_screen", json!({"window_id":"date"}));
        assert!(
            resolve_call(&wrong_kind, &history(), 1)
                .unwrap_err()
                .message
                .contains("wrong object kind")
        );
        let unscoped = call("inspect_desktop_ui", json!({"queries":["Calendar"]}));
        assert_eq!(resolve_call(&unscoped, &[], 1).unwrap(), unscoped);
        let screen = call("read_current_screen", json!({}));
        assert_eq!(resolve_call(&screen, &[], 1).unwrap(), screen);
    }

    #[test]
    fn model_projection_hides_metadata_without_changing_durable_evidence() {
        let messages = history();
        let original = serde_json::to_string(&messages).unwrap();
        let mut request =
            crate::seam::ModelRequest::text_only(messages, crate::prompt::ResponseFormatSpec::None);
        request.tools = crate::device_assistant::device_assistant_tool_registry()
            .into_iter()
            .map(|t| t.spec)
            .collect();
        request.tools.extend(
            crate::permission_tools::permission_planning_tool_registry()
                .into_iter()
                .map(|t| t.spec),
        );
        let projected = project_request(&request);
        let text = serde_json::to_string(&projected.messages).unwrap();
        assert!(!text.contains("expires_at"));
        assert!(!text.contains("snapshot_id"));
        assert!(!text.contains("2030-01"));
        assert!(text.contains("calendar"));
        assert!(text.contains("date"));
        assert_eq!(serde_json::to_string(&request.messages).unwrap(), original);
        for name in [
            "inspect_desktop_ui",
            "execute_ui_actions",
            "read_current_screen",
        ] {
            let tool = projected.tools.iter().find(|t| t.name == name).unwrap();
            let schema = tool.parameters_schema.to_string();
            assert!(!schema.contains("expires_at"));
            assert!(!schema.contains("snapshot_id"));
        }
        let action = projected
            .tools
            .iter()
            .find(|t| t.name == "execute_ui_actions")
            .unwrap();
        assert_eq!(
            action.parameters_schema["required"],
            json!(["application_id", "steps"])
        );
        assert_eq!(
            serde_json::to_string(&project_request(&projected).messages).unwrap(),
            text
        );
    }

    #[test]
    fn window_capture_and_exact_fallback_use_server_resolved_ids() {
        let mut messages = history();
        let window = reference(
            "window",
            "window-snapshot",
            "window",
            "2030-01-01T00:00:00Z",
        );
        let mut value: Value = serde_json::from_str(&messages[2].text).unwrap();
        value["ReadContext"]["DesktopUiInspect"]["owner_selectable_windows"] =
            json!([{"object_ref":window}]);
        messages[2].text = value.to_string();
        let screenshot = resolve_call(
            &call("read_current_screen", json!({"window_id":"window"})),
            &messages,
            1,
        )
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&screenshot.arguments_json).unwrap()["window"],
            window
        );
        let exact = json!({"application_id":"calendar","action":{"screen":{"display":"1","width":100,"height":100,"dpi_x":96,"dpi_y":96},"step":{"kind":"key","params":{"key":"enter"}}}});
        let execution = resolve_call(
            &call("execute_confirmed_raw_input", exact.clone()),
            &messages,
            1,
        )
        .unwrap();
        let permission = resolve_call(
            &call(
                "request_capability_grants",
                json!({"items":[{"tool_name":"execute_confirmed_raw_input","exact_input":exact}]}),
            ),
            &messages,
            1,
        )
        .unwrap();
        let permission: Value = serde_json::from_str(&permission.arguments_json).unwrap();
        assert_eq!(
            permission["items"][0]["exact_input"],
            serde_json::from_str::<Value>(&execution.arguments_json).unwrap()
        );
        // A tool result from a different tool cannot register desktop IDs.
        messages[0]
            .tool_calls
            .iter_mut()
            .for_each(|c| c.name = "read_selected_text_file".into());
        assert!(
            resolve_call(
                &call("read_current_screen", json!({"window_id":"window"})),
                &messages,
                1
            )
            .unwrap_err()
            .message
            .contains("not observed")
        );
    }

    #[test]
    fn application_permission_resolves_on_server_and_keeps_approval_flow() {
        let original = call(
            "request_capability_grants",
            json!({"items":[{"item_id":"calendar","tool_name":"execute_ui_actions","application_scope":{"application_id":"calendar","actions":["invoke"]},"suggested_ttl_seconds":120,"suggested_max_uses":4,"reason":"Create requested event"}]}),
        );
        let resolved = resolve_call(&original, &history(), 1).unwrap();
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let mut request = crate::permission_tools::build_permission_request(
            &resolved,
            &registry,
            "request".into(),
            1,
            "2026-09-11T00:00:00Z".into(),
        )
        .unwrap();
        crate::application_ui::bind_request(&mut request, &history()).unwrap();
        assert_eq!(
            crate::application_ui::from_canonical(
                &request.items[0].tool_name,
                request.items[0].canonical_input_json.as_deref()
            )
            .unwrap()
            .application_name
            .as_deref(),
            Some("Calendar")
        );
        assert!(same_call_input(
            &original.name,
            &original.arguments_json,
            &resolved.arguments_json
        ));
    }
    #[test]
    fn background_ids_resolve_and_geometry_is_server_owned() {
        let mut messages = history();
        let window = reference("window", "native", "window", "2000-01-01T00:00:00Z");
        messages.push(ChatMessage::assistant_tool_calls(
            "windows",
            "",
            vec![ToolCallRef {
                id: "windows-call".into(),
                name: "inspect_desktop_ui".into(),
                arguments_json: "{}".into(),
            }],
        ));
        messages.push(ChatMessage::tool_result("windows-result","windows-call",json!({"ReadContext":{"DesktopUiInspect":{"owner_selectable_windows":[window.clone()]}}}).to_string()));
        let geometry = json!({"width_millipoints":600000,"height_millipoints":400000});
        messages.push(ChatMessage::assistant_tool_calls(
            "capture",
            "",
            vec![ToolCallRef {
                id: "capture-call".into(),
                name: "read_current_screen".into(),
                arguments_json: "{}".into(),
            }],
        ));
        messages.push(ChatMessage::tool_result("image","capture-call",json!({"ReadContext":{"ScreenCaptureCurrent":{"window":window,"window_geometry":geometry}}}).to_string()));
        let original = call(
            "execute_background_inputs",
            json!({"application_id":"calendar","window_id":"window","action":{"kind":"click","element_id":"date"}}),
        );
        let resolved = resolve_call(&original, &messages, 1).unwrap();
        let value: Value = serde_json::from_str(&resolved.arguments_json).unwrap();
        assert_eq!(value["target"], window);
        assert_eq!(value["geometry"], geometry);
        assert_eq!(value["action"]["element"]["token"], "date");
        assert!(same_call_input(
            &original.name,
            &original.arguments_json,
            &resolved.arguments_json
        ));
        let mut bad: Value = serde_json::from_str(&original.arguments_json).unwrap();
        bad["geometry"] = geometry;
        assert!(resolve_call(&call("execute_background_inputs", bad), &messages, 1).is_err());
        let mut tool = crate::background_input::tool().spec;
        project_tool(&mut tool);
        assert!(
            tool.parameters_schema["properties"]
                .get("window_id")
                .is_some()
        );
        assert!(
            tool.parameters_schema["properties"]
                .get("geometry")
                .is_none()
        );
    }
    #[test]
    fn batch_resolves_all_observed_steps_and_binds_every_operation() {
        let original = call(
            "execute_ui_actions",
            json!({"application_id":"calendar","steps":[{"element_id":"date","action":{"kind":"invoke"}},{"element_id":"title","action":{"kind":"set_value","params":{"value":"meeting"}}}]}),
        );
        let resolved = resolve_call(&original, &history(), 1).unwrap();
        assert!(same_call_input(
            &original.name,
            &original.arguments_json,
            &resolved.arguments_json
        ));
        let registry = crate::device_assistant::device_assistant_provider_registry();
        for surface in [
            desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
            desk_agent_protocol::capability_provider::ProductSurface::ManagerPersonalOwner,
        ] {
            let input =
                crate::provider_preflight::UiCallPreflight::build(&registry, surface, &resolved, 1)
                    .unwrap();
            assert_eq!(input.steps().len(), 2);
            assert_eq!(input.steps()[0].target.snapshot_id, "four");
            assert_eq!(input.steps()[1].target.snapshot_id, "five");
            assert_eq!(
                crate::application_batch::operation_scope(input.steps()),
                ["ui:invoke", "ui:set_value"]
            );
        }
        let mut broken: Value = serde_json::from_str(&original.arguments_json).unwrap();
        broken["steps"][1]["element_id"] = json!("unobserved");
        let error = resolve_call(&call("execute_ui_actions", broken), &history(), 1).unwrap_err();
        assert!(error.message.contains("step 2"));
        assert!(error.message.contains("No step was executed"));
    }
    #[test]
    fn batch_rejects_single_shape_nested_steps_and_excessive_length() {
        for input in [
            json!({"application_id":"calendar","action":{"kind":"invoke"},"element_id":"date"}),
            json!({"application_id":"calendar","steps":[]}),
            json!({"application_id":"calendar","steps":vec![json!({"element_id":"date","action":{"kind":"invoke"}});21]}),
            json!({"application_id":"calendar","steps":[{"element_id":"date","action":{"kind":"invoke"},"remaining_steps":[]}]}),
        ] {
            let direct = ToolCall {
                id: "bad".into(),
                name: "execute_ui_actions".into(),
                arguments_json: input.to_string(),
            };
            assert!(resolve_call(&direct, &history(), 1).is_err());
        }
    }
    #[test]
    fn malformed_model_batch_replay_cannot_panic_or_recurse() {
        for mut value in [
            json!({"remaining_steps":7}),
            json!({"application":{},"remaining_steps":[null]}),
            json!({"application":{},"remaining_steps":[{"remaining_steps":[{}]}]}),
        ] {
            project_arguments("execute_ui_actions", &mut value);
        }
    }
}
