//! Model-only batch surface, expanded into a sealed native action sequence.
use crate::chat::{ChatMessage, ToolCall, ToolSpec};
use desk_agent_protocol::{AgentError, AgentErrorKind, computer_use::ComputerActionStep};
use serde_json::{Value, json};

pub const MAX_STEPS: usize = 20;
pub fn supports(name: &str) -> bool {
    matches!(name, "execute_ui_actions" | "execute_background_inputs")
}
fn invalid(path: &str, detail: impl std::fmt::Display) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: format!(
            "Invalid batch at {path}: {detail}. Required UI format: {{\"application_id\":\"<app>\",\"steps\":[{{\"element_id\":\"<control>\",\"action\":{{\"kind\":\"invoke\"}}}}]}}. Background format: {{\"application_id\":\"<app>\",\"window_id\":\"<window>\",\"steps\":[{{\"action\":{{\"kind\":\"type_text\",\"text\":\"hello\"}}}}]}}. Use 1–20 steps. No step was executed."
        ),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

pub fn resolve(call: &ToolCall, history: &[ChatMessage], now: u64) -> Result<ToolCall, AgentError> {
    if call.arguments_json.len() > 64 * 1024 {
        return Err(invalid("$", "batch exceeds 64 KiB"));
    }
    let value: Value = serde_json::from_str(&call.arguments_json).map_err(|e| invalid("$", e))?;
    let obj = value
        .as_object()
        .ok_or_else(|| invalid("$", "expected object"))?;
    let background = call.name == "execute_background_inputs";
    for key in obj.keys() {
        if !matches!(key.as_str(), "application_id" | "steps")
            && !(background && key == "window_id")
        {
            return Err(invalid(key, "unexpected field"));
        }
    }
    if !value["application_id"].is_string() || (background && !value["window_id"].is_string()) {
        return Err(invalid(
            "$",
            "observed application_id and, for background input, window_id are required",
        ));
    }
    let steps = value["steps"]
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= MAX_STEPS)
        .ok_or_else(|| invalid("steps", "expected 1–20 steps"))?;
    let mut resolved = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let path = format!("steps[{}] (step {})", index, index + 1);
        let step = step
            .as_object()
            .ok_or_else(|| invalid(&path, "expected object"))?;
        if step
            .keys()
            .any(|k| k != "action" && !(k == "element_id" && !background))
            || !step.contains_key("action")
        {
            return Err(invalid(
                &path,
                "allowed fields: action; UI also requires element_id",
            ));
        }
        let mut args = Value::Object(step.clone());
        args["application_id"] = value["application_id"].clone();
        if background {
            args["window_id"] = value["window_id"].clone();
        }
        let child = crate::ui_model_ids::resolve_single_call(
            &ToolCall {
                arguments_json: args.to_string(),
                ..call.clone()
            },
            history,
            now,
        )
        .map_err(|e| invalid(&path, e.message))?;
        // Validate the entire sequence before any approval reservation or dispatch.
        if background {
            crate::provider_preflight::background_input_from_call(&child)
                .map_err(|e| invalid(&path, e.message))?;
        } else {
            crate::provider_preflight::ui_action_from_call(&child)
                .map_err(|e| invalid(&path, e.message))?;
        }
        resolved.push(serde_json::from_str::<Value>(&child.arguments_json).unwrap());
    }
    let mut first = resolved.remove(0);
    first["remaining_steps"] = json!(resolved);
    Ok(ToolCall {
        arguments_json: first.to_string(),
        ..call.clone()
    })
}

/// Expand only server-resolved arguments; native ownership is checked on the edge.
pub fn actions(call: &ToolCall) -> Result<Vec<ComputerActionStep>, AgentError> {
    let mut first: Value =
        serde_json::from_str(&call.arguments_json).map_err(|e| invalid("$", e))?;
    let remaining = first
        .as_object_mut()
        .ok_or_else(|| invalid("$", "expected object"))?
        .remove("remaining_steps");
    let mut args = vec![first];
    if let Some(remaining) = remaining {
        let tail = remaining
            .as_array()
            .ok_or_else(|| invalid("remaining_steps", "expected array"))?;
        if tail.len() >= MAX_STEPS {
            return Err(invalid("steps", "too many steps"));
        }
        args.extend(tail.iter().cloned());
    }
    let app = args[0]["application"].clone();
    let window = args[0]["target"].clone();
    let mut output = Vec::new();
    for (index, args) in args.into_iter().enumerate() {
        if args.get("remaining_steps").is_some()
            || args["application"] != app
            || (call.name == "execute_background_inputs" && args["target"] != window)
        {
            return Err(invalid(
                &format!("steps[{index}]"),
                "all steps must use the same application and background window",
            ));
        }
        let child = ToolCall {
            arguments_json: args.to_string(),
            ..call.clone()
        };
        let (target, action) = if call.name == "execute_ui_actions" {
            let (target, action) = crate::provider_preflight::ui_action_from_call(&child)?;
            (
                target,
                desk_agent_protocol::computer_use::ComputerActionKind::UiInApplication {
                    application: serde_json::from_value(app.clone())
                        .map_err(|e| invalid("application", e))?,
                    action,
                },
            )
        } else if call.name == "execute_background_inputs" {
            let (target, input) = crate::provider_preflight::background_input_from_call(&child)?;
            (
                target,
                desk_agent_protocol::computer_use::ComputerActionKind::BackgroundInput {
                    application: serde_json::from_value(app.clone())
                        .map_err(|e| invalid("application", e))?,
                    input,
                    geometry: serde_json::from_value(args["geometry"].clone())
                        .map_err(|e| invalid("geometry", e))?,
                },
            )
        } else {
            return Err(invalid("tool", "not an application batch"));
        };
        output.push(ComputerActionStep {target,action,before_summary:format!("Observed target for step {}",index+1),after_intent:format!("Execute step {} in order",index+1),verification:"Report native API/event dispatch only. The assistant must read UI or screenshot to verify the application state.".into()});
    }
    Ok(output)
}

pub fn operation_scope(steps: &[ComputerActionStep]) -> Vec<String> {
    let mut scope = Vec::new();
    for step in steps {
        let operation = match &step.action {
            desk_agent_protocol::computer_use::ComputerActionKind::UiInApplication {
                action,
                ..
            } => crate::application_ui::operation(action),
            desk_agent_protocol::computer_use::ComputerActionKind::BackgroundInput {
                input,
                ..
            } => crate::application_ui::operation_kind(input.kind()),
            _ => unreachable!("validated application actions"),
        };
        if !scope.contains(&operation) {
            scope.push(operation);
        }
    }
    scope
}

pub fn project_schema(tool: &mut ToolSpec) {
    if !supports(&tool.name) {
        return;
    }
    let background = tool.name == "execute_background_inputs";
    let p = &tool.parameters_schema["properties"];
    let mut properties = json!({"application_id":p["application_id"],"steps":{"type":"array","minItems":1,"maxItems":MAX_STEPS,"items":{"type":"object","properties":{"action":p["action"]},"required":["action"],"additionalProperties":false}}});
    let mut required = vec!["application_id", "steps"];
    if background {
        properties["window_id"] = p["window_id"].clone();
        required.push("window_id");
    } else {
        properties["steps"]["items"]["properties"]["element_id"] = p["element_id"].clone();
        properties["steps"]["items"]["required"] = json!(["element_id", "action"]);
    }
    tool.parameters_schema = json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
    tool.description = format!(
        "Execute 1–20 {} steps in order in one approved application. Use steps even for one action; single-action tools are hidden. {} Reuse application_scope approval for ALL action kinds used. One batch consumes one grant use. The batch holds one writer lease; it is not a transaction and never rolls back. Stop on the first error, cancellation or unknown outcome; the result contains only the first failing step number/error, or the completed count. Earlier steps completed native dispatch and later steps did not run. A whole-batch preflight rejection means NO steps ran. No automatic retries or fixed delays between actions; wait only for the previous native API dispatch to return. If a later step depends on a new window, UI layout or asynchronous application result, end this batch, read UI/window screenshot and then construct a new batch. Do not invent future IDs. Dispatch success does not verify application state. Never ask the user to acknowledge a failed record. {}",
        if background {
            "background mouse/keyboard"
        } else {
            "semantic UI"
        },
        if background {
            "Prefer execute_ui_actions. Only use background input when semantic UI is impractical; combine with a current read_current_screen window_id screenshot. Target window must be the application's input window for keyboard; never activate the app or move the real cursor."
        } else {
            "Use observed element_id for each step. Prefer semantic UI over background input."
        },
        if background {
            "Mouse: action contains exactly one element_id or position {x,y} in original window screenshot pixels, origin top-left (0,0), x < screenshot width and y < screenshot height; these are pixels, not percentages. type_text supports Unicode. key_press uses named keys/modifiers. Scroll uses horizontal_pixels/vertical_pixels distances (e.g. vertical_pixels=-300 for 300 pixels down; -6 is only 6 pixels). TextEdit background Command+A is known ineffective; choose a different approach. Mouse support is experimental."
        } else {
            "set_value uses action {kind: set_value, params: {value: text}}; toggle uses params.desired. invoke/select/focus need only kind."
        }
    );
}

/// Validate the compact native receipt without asserting semantic UI success.
pub fn completion_receipt(
    completed: &desk_agent_protocol::computer_use::ComputerActionCompleted,
    expected_steps: Option<usize>,
) -> Result<Option<(bool, String)>, AgentError> {
    use desk_agent_protocol::computer_use::ComputerActionResultClass as Class;
    let Some(message) = completed.message.as_deref() else {
        return Ok(None);
    };
    let Ok(value) = serde_json::from_str::<Value>(message) else {
        return Ok(None);
    };
    let Some(failed) = compact_failed(&value) else {
        return Ok(None);
    };
    let number = value[if failed {
        "failed_step_number"
    } else {
        "completed_steps"
    }]
    .as_u64()
    .unwrap_or(0) as usize;
    if message.len() > 4096
        || !completed.facts.is_empty()
        || completed.output.is_some()
        || value["application_state_verified"] != false
        || number == 0
        || number > MAX_STEPS
        || expected_steps.is_some_and(|n| if failed { number > n } else { number != n })
        || if failed {
            completed.result != Class::Failed
                || !value["error"]["message"].is_string()
                || !matches!(
                    value["effect"].as_str(),
                    Some("no_effect" | "may_have_effect")
                )
        } else {
            completed.result != Class::ChangedButUnverified
        }
    {
        return Err(invalid("receipt", "invalid native batch receipt"));
    }
    Ok(Some((failed, message.into())))
}

pub fn compact_failed(value: &Value) -> Option<bool> {
    if value["application_state_verified"] != false {
        return None;
    }
    match value["status"].as_str() {
        Some("completed")
            if value["completed_steps"]
                .as_u64()
                .is_some_and(|n| n > 0 && n <= MAX_STEPS as u64) =>
        {
            Some(false)
        }
        Some("stopped_on_error")
            if value["failed_step_number"]
                .as_u64()
                .is_some_and(|n| n > 0 && n <= MAX_STEPS as u64)
                && value["error"]["message"].is_string() =>
        {
            Some(true)
        }
        _ => None,
    }
}

#[cfg(test)]
mod receipt_tests {
    use super::*;
    use desk_agent_protocol::computer_use::*;
    #[test]
    fn compact_completion_checks_counts_class_and_has_no_step_list() {
        let mut completed=ComputerActionCompleted { work_id:"1".into(),action_request_id:"action".into(),execution_generation:"generation".into(),result:ComputerActionResultClass::ChangedButUnverified,facts:vec![],message:Some(json!({"status":"completed","completed_steps":2,"application_state_verified":false}).to_string()),output:None };
        assert_eq!(
            completion_receipt(&completed, Some(2)).unwrap().unwrap().0,
            false
        );
        assert!(completion_receipt(&completed, Some(3)).is_err());
        completed.result = ComputerActionResultClass::Failed;
        assert!(completion_receipt(&completed, Some(2)).is_err());
        completed.message=Some(json!({"status":"stopped_on_error","failed_step_number":2,"effect":"may_have_effect","application_state_verified":false,"error":{"message":"AX rejected"}}).to_string());
        assert_eq!(
            completion_receipt(&completed, Some(2)).unwrap().unwrap().0,
            true
        );
        assert!(completion_receipt(&completed, Some(1)).is_err());
    }
}
