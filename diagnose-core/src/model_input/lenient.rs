//! Lenient normalization of model-supplied tool input against the
//! model-visible schema. Stored model output is never rewritten; callers apply
//! this to a derived copy before validation, permission planning and dispatch.
use serde_json::Value;

/// A normalized tool call's original model arguments and the notice telling
/// the model which fields the server ignored or treated as omitted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NormalizedInput {
    pub original: String,
    pub notice: String,
}

/// Fields the normalization removed, reported back to the model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Normalized {
    /// Undeclared fields removed from a non-side-effecting tool input.
    pub ignored: Vec<String>,
    /// Optional, non-nullable fields whose `null` was treated as omitted.
    pub nulls: Vec<String>,
}

impl Normalized {
    pub fn is_empty(&self) -> bool {
        self.ignored.is_empty() && self.nulls.is_empty()
    }

    /// One model-facing sentence, empty when nothing changed.
    pub fn notice(&self) -> String {
        let mut parts = vec![];
        if !self.ignored.is_empty() {
            parts.push(format!(
                "Ignored unsupported input fields: {}.",
                self.ignored.join(", ")
            ));
        }
        if !self.nulls.is_empty() {
            parts.push(format!(
                "Null optional fields were treated as omitted: {}.",
                self.nulls.join(", ")
            ));
        }
        parts.join(" ")
    }
}

/// Fields this normalization never removes, at any level. Target and scope
/// selectors keep their strict meaning (omission can infer a target or widen a
/// scope); native reference names and server-owned metadata keep their explicit
/// validation errors instead of being silently dropped.
const PROTECTED_FIELDS: &[&str] = &[
    "target_ref",
    "window_id",
    "root_id",
    "element_id",
    "output_id",
    "display",
    "application_id",
    "directory_request_id",
    "application_scope",
    "application",
    "target",
    "root",
    "window",
    "element",
    "item_id",
    "resource_scope",
    "operation_scope",
];

pub fn normalize(schema: &Value, value: &mut Value, ignore_extra: bool) -> Normalized {
    let mut report = Normalized::default();
    walk(schema, value, "", ignore_extra, &mut report);
    report
}

fn walk(
    schema: &Value,
    value: &mut Value,
    path: &str,
    ignore_extra: bool,
    report: &mut Normalized,
) {
    match value {
        Value::Object(object) => {
            let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
                return;
            };
            let required = schema["required"]
                .as_array()
                .map(|fields| fields.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                .unwrap_or_default();
            // A variant declaring a property this level does not list may
            // legitimize that key; variants that only constrain listed fields
            // or name required fields cannot.
            let combined = ["oneOf", "anyOf"].iter().any(|key| {
                schema[key].as_array().is_some_and(|variants| {
                    variants.iter().any(|variant| {
                        variant["properties"].as_object().is_some_and(|declared| {
                            declared.keys().any(|name| !properties.contains_key(name))
                        })
                    })
                })
            });
            let keys = object.keys().cloned().collect::<Vec<_>>();
            for key in keys {
                if PROTECTED_FIELDS.contains(&key.as_str()) {
                    if let (Some(property), Some(child)) =
                        (properties.get(&key), object.get_mut(&key))
                    {
                        walk(property, child, &join(path, &key), ignore_extra, report);
                    }
                    continue;
                }
                let field = join(path, &key);
                match properties.get(&key) {
                    Some(property) => {
                        if object[&key].is_null()
                            && !required.contains(&key.as_str())
                            && !allows_null(property)
                        {
                            object.remove(&key);
                            report.nulls.push(field);
                        } else if let Some(child) = object.get_mut(&key) {
                            walk(property, child, &field, ignore_extra, report);
                        }
                    }
                    None if ignore_extra
                        && !combined
                        && schema["additionalProperties"] == Value::Bool(false) =>
                    {
                        object.remove(&key);
                        report.ignored.push(field);
                    }
                    None => {}
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter_mut().enumerate() {
                    walk(
                        item_schema,
                        item,
                        &format!("{path}[{index}]"),
                        ignore_extra,
                        report,
                    );
                }
            }
        }
        _ => {}
    }
}

/// Normalize the tool calls of one agent turn once, at the host, before the
/// output envelope binds them. Every later consumer sees only this copy.
pub fn normalize_model_turn(
    turn: &mut crate::chat::ModelTurn,
    request: &crate::seam::ModelRequest,
) {
    if request.use_case == crate::model_profile::ModelUseCase::Agent {
        normalize_turn_tools(turn, &request.tools);
    }
}

/// The tool definitions to normalize against, captured before a seam moves the
/// request into its provider call. Only agent turns are normalized.
pub fn agent_tools(request: &crate::seam::ModelRequest) -> Option<Vec<crate::chat::ToolSpec>> {
    (request.use_case == crate::model_profile::ModelUseCase::Agent && !request.tools.is_empty())
        .then(|| request.tools.clone())
}

pub fn normalize_turn_tools(turn: &mut crate::chat::ModelTurn, tools: &[crate::chat::ToolSpec]) {
    for call in &mut turn.tool_calls {
        let Some(spec) = tools.iter().find(|tool| tool.name == call.name) else {
            continue;
        };
        let Ok(mut value) = serde_json::from_str::<Value>(&call.arguments_json) else {
            continue;
        };
        let mut spec = spec.clone();
        crate::ui_model_ids::project_tool(&mut spec);
        let mut report = normalize(&spec.parameters_schema, &mut value, lenient(&call.name));
        if call.name == crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME
            && let Some(items) = value.get_mut("items").and_then(Value::as_array_mut)
        {
            let registry = crate::ai_assistant::ai_assistant_provider_registry_ref();
            for (index, item) in items.iter_mut().enumerate() {
                let Some(name) = item["tool_name"].as_str().map(str::to_owned) else {
                    continue;
                };
                let (Some(capability), Some(exact)) = (
                    registry.capability_for_tool(&name),
                    item.get_mut("exact_input"),
                ) else {
                    continue;
                };
                let mut target = capability.tool_spec.clone();
                crate::ui_model_ids::project_tool(&mut target);
                let nested = normalize(&target.parameters_schema, exact, lenient(&name));
                let prefix = format!("items[{index}].exact_input.");
                report.ignored.extend(
                    nested
                        .ignored
                        .into_iter()
                        .map(|field| format!("{prefix}{field}")),
                );
                report.nulls.extend(
                    nested
                        .nulls
                        .into_iter()
                        .map(|field| format!("{prefix}{field}")),
                );
            }
        }
        if report.is_empty() {
            continue;
        }
        let original = std::mem::replace(&mut call.arguments_json, value.to_string());
        turn.provider_meta.normalized_inputs.insert(
            call.id.clone(),
            NormalizedInput {
                original,
                notice: report.notice(),
            },
        );
    }
}

/// Undeclared fields are ignored only for tools whose effect cannot widen or
/// change a device, external or persistent outcome. Unknown tools stay strict.
fn lenient(name: &str) -> bool {
    use crate::registry::ToolEffect;
    use desk_agent_protocol::capability_provider::CapabilityEffect;
    if let Some(capability) =
        crate::ai_assistant::ai_assistant_provider_registry_ref().capability_for_tool(name)
    {
        let effect = capability.wire.effect;
        return !effect.is_side_effecting()
            && !matches!(
                effect,
                CapabilityEffect::ExportData | CapabilityEffect::CaptureScreen
            );
    }
    builtin_effects().get(name).is_some_and(|effect| {
        matches!(
            effect,
            ToolEffect::ReadOnly
                | ToolEffect::ActionPreview
                | ToolEffect::ExternalRead
                | ToolEffect::WaitTask
                | ToolEffect::RunProjection
                | ToolEffect::PermissionPlanning
                | ToolEffect::CapabilityDiscovery
                | ToolEffect::ConversationHistory
                | ToolEffect::AssistantHistory
                | ToolEffect::MemoryQuery
                | ToolEffect::ScheduleQuery
                | ToolEffect::SubAgentQuery
                | ToolEffect::SkillLoading
        )
    })
}

fn builtin_effects() -> &'static std::collections::BTreeMap<String, crate::registry::ToolEffect> {
    static EFFECTS: std::sync::OnceLock<
        std::collections::BTreeMap<String, crate::registry::ToolEffect>,
    > = std::sync::OnceLock::new();
    EFFECTS.get_or_init(|| {
        crate::permission_tools::permission_planning_tool_registry()
            .into_iter()
            .chain(crate::capability_disclosure::capability_discovery_tool_registry())
            .chain(crate::task_status_tools::task_status_tool_registry())
            .chain(crate::subagent::tools::registry())
            .chain(crate::goal_tools::registry())
            .chain(crate::goal_tools::open_registry())
            .chain(crate::directory_tools::registry())
            .chain(crate::schedule::proposal::registry())
            .chain(crate::conversation_history::conversation_history_tool_registry())
            .chain(crate::wait_tools::wait_tool_registry())
            .chain(crate::read_tools::read_tool_registry())
            .chain(crate::exec_tools::exec_tool_registry())
            .map(|tool| (tool.spec.name.clone(), tool.effect))
            .collect()
    })
}

fn allows_null(schema: &Value) -> bool {
    match &schema["type"] {
        Value::String(kind) => kind == "null",
        Value::Array(kinds) => kinds.iter().any(|kind| kind == "null"),
        // Untyped or combinator schemas keep null for the validator to judge.
        _ => true,
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.into()
    } else {
        format!("{path}.{key}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type":"object","additionalProperties":false,
            "required":["command"],
            "properties":{
                "command":{"type":"string"},
                "cwd":{"type":"string"},
                "clear":{"type":["string","null"]},
                "target_ref":{"type":"string"},
                "steps":{"type":"array","items":{
                    "type":"object","additionalProperties":false,
                    "properties":{"kind":{"type":"string"},"note":{"type":"string"}}
                }},
                "action":{"oneOf":[{"type":"object","properties":{"other":{"type":"integer"}}}],"type":"object",
                    "additionalProperties":false,"properties":{"kind":{"type":"string"}}}
            }
        })
    }

    #[test]
    fn optional_nulls_are_omitted_but_required_nullable_and_selection_nulls_stay() {
        let mut value = json!({"command":null,"cwd":null,"clear":null,"target_ref":null});
        let report = normalize(&schema(), &mut value, false);
        assert_eq!(
            value,
            json!({"command":null,"clear":null,"target_ref":null})
        );
        assert_eq!(report.nulls, ["cwd"]);
        assert!(report.ignored.is_empty());
    }

    #[test]
    fn protected_selectors_aliases_and_server_fields_are_never_removed() {
        let mut value = json!({
            "command":"pwd","window_id":null,"target_ref":"a","root":{"id":"x"},
            "item_id":"i","steps":[{"kind":"a","target_ref":"b"}]
        });
        let original = value.clone();
        let schema = json!({"type":"object","additionalProperties":false,
            "properties":{"command":{"type":"string"},"window_id":{"type":"string"},
                "steps":{"type":"array","items":{"type":"object","additionalProperties":false,
                    "properties":{"kind":{"type":"string"}}}}}});
        let report = normalize(&schema, &mut value, true);
        assert_eq!(value, original);
        assert!(report.is_empty());
    }

    #[test]
    fn extras_are_dropped_only_when_allowed_and_never_inside_combinators() {
        let input = json!({
            "command":"pwd","dry_run":true,
            "steps":[{"kind":"a","extra":1,"note":null}],
            "action":{"kind":"x","other":2}
        });
        let mut strict = input.clone();
        let report = normalize(&schema(), &mut strict, false);
        assert_eq!(strict["dry_run"], true);
        assert!(report.ignored.is_empty());
        assert_eq!(report.nulls, ["steps[0].note"]);

        let mut lenient = input;
        let report = normalize(&schema(), &mut lenient, true);
        assert_eq!(
            lenient,
            json!({"command":"pwd","steps":[{"kind":"a"}],"action":{"kind":"x","other":2}})
        );
        assert_eq!(report.ignored, ["dry_run", "steps[0].extra"]);
        assert_eq!(
            report.notice(),
            "Ignored unsupported input fields: dry_run, steps[0].extra. Null optional fields were treated as omitted: steps[0].note."
        );
    }
}

#[cfg(test)]
mod turn_tests {
    use super::*;
    use crate::chat::{ModelTurn, ToolCall};
    use crate::prompt::ResponseFormatSpec;
    use crate::seam::ModelRequest;
    use serde_json::json;

    fn request(names: &[&str]) -> ModelRequest {
        let registry = crate::ai_assistant::ai_assistant_provider_registry();
        let mut request = ModelRequest::text_only(vec![], ResponseFormatSpec::None);
        request.tools = names
            .iter()
            .map(|name| {
                if *name == crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME {
                    crate::permission_tools::permission_planning_tool_registry()
                        .into_iter()
                        .find(|tool| tool.spec.name == *name)
                        .unwrap()
                        .spec
                } else {
                    let mut tool = registry
                        .capability_for_tool(name)
                        .unwrap()
                        .registered_tool();
                    crate::request_devices::add_target_selector(&mut tool, false);
                    tool.spec
                }
            })
            .collect();
        request
    }

    fn turn(calls: &[(&str, Value)]) -> ModelTurn {
        ModelTurn {
            tool_calls: calls
                .iter()
                .enumerate()
                .map(|(index, (name, input))| ToolCall {
                    id: format!("call-{index}"),
                    name: (*name).into(),
                    arguments_json: input.to_string(),
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn read_tools_drop_extras_side_effecting_tools_keep_them_and_selectors_survive() {
        let request = request(&["inspect_desktop_ui", "update_text_file"]);
        let mut turn = turn(&[
            (
                "inspect_desktop_ui",
                json!({"target_ref":"a","queries":["Calculator"],"verbose":true,"max_depth":null}),
            ),
            (
                "update_text_file",
                json!({"file_version_id":"v1","replacement":"x","dry_run":true}),
            ),
        ]);
        let original_read = turn.tool_calls[0].arguments_json.clone();
        let original_update = turn.tool_calls[1].arguments_json.clone();
        normalize_model_turn(&mut turn, &request);
        assert_eq!(
            turn.provider_meta.normalized_inputs["call-0"].original,
            original_read
        );
        let read: Value = serde_json::from_str(&turn.tool_calls[0].arguments_json).unwrap();
        assert_eq!(read, json!({"target_ref":"a","queries":["Calculator"]}));
        assert_eq!(
            turn.provider_meta.normalized_inputs["call-0"].notice,
            "Ignored unsupported input fields: verbose. Null optional fields were treated as omitted: max_depth."
        );
        assert_eq!(turn.tool_calls[1].arguments_json, original_update);
        assert!(!turn.provider_meta.normalized_inputs.contains_key("call-1"));
    }

    #[test]
    fn permission_exact_input_nulls_are_omitted_and_match_the_later_call() {
        let request = request(&[crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME]);
        let exact =
            json!({"shell":"bash","command":"pwd","timeout_ms":1000,"cwd":null,"io_mode":null});
        let mut turn = turn(&[(
            crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME,
            json!({"items":[{"tool_name":"exec_command","reason":"Run","exact_input":exact}]}),
        )]);
        normalize_model_turn(&mut turn, &request);
        let value: Value = serde_json::from_str(&turn.tool_calls[0].arguments_json).unwrap();
        let normalized = value["items"][0]["exact_input"].clone();
        // cwd declares null (its default); io_mode does not accept null.
        assert_eq!(
            normalized,
            json!({"shell":"bash","command":"pwd","timeout_ms":1000,"cwd":null})
        );
        assert_eq!(
            turn.provider_meta.normalized_inputs["call-0"].notice,
            "Null optional fields were treated as omitted: items[0].exact_input.io_mode."
        );
        let later_call = json!({"shell":"bash","command":"pwd","timeout_ms":1000});
        assert_eq!(
            crate::permission_tools::canonical_tool_permission_input_json(
                "exec_command",
                normalized
            )
            .unwrap(),
            crate::permission_tools::canonical_tool_permission_input_json(
                "exec_command",
                later_call
            )
            .unwrap()
        );
    }

    #[test]
    fn unchanged_unparsable_unknown_and_non_agent_calls_keep_their_bytes() {
        let request = request(&["inspect_desktop_ui"]);
        let inputs = [
            ("inspect_desktop_ui", "{\"queries\": [\"a\"]}".to_string()),
            ("inspect_desktop_ui", "{not json".to_string()),
            ("unknown_tool", "{\"x\":null}".to_string()),
        ];
        let mut turn = ModelTurn {
            tool_calls: inputs
                .iter()
                .enumerate()
                .map(|(index, (name, input))| ToolCall {
                    id: format!("call-{index}"),
                    name: (*name).into(),
                    arguments_json: input.clone(),
                })
                .collect(),
            ..Default::default()
        };
        normalize_model_turn(&mut turn, &request);
        for (call, (_, input)) in turn.tool_calls.iter().zip(&inputs) {
            assert_eq!(&call.arguments_json, input);
        }
        assert!(turn.provider_meta.normalized_inputs.is_empty());

        let mut review = request.clone();
        review.use_case = crate::model_profile::ModelUseCase::Approval;
        let mut turn = turn_with_extra();
        let before = turn.clone();
        normalize_model_turn(&mut turn, &review);
        assert_eq!(turn, before);
    }

    fn turn_with_extra() -> ModelTurn {
        turn(&[(
            "inspect_desktop_ui",
            json!({"queries":["a"],"verbose":true}),
        )])
    }

    #[test]
    fn notices_stay_outside_the_bytes_bound_by_the_output_envelope() {
        let mut turn = turn_with_extra();
        normalize_model_turn(&mut turn, &request(&["inspect_desktop_ui"]));
        assert!(!turn.provider_meta.normalized_inputs.is_empty());
        let mut message = crate::chat::ChatMessage::assistant_tool_calls(
            "m",
            turn.text.clone(),
            turn.tool_calls.iter().map(|call| call.to_ref()).collect(),
        );
        let bound = crate::model_egress::message_content_bytes(&message).unwrap();
        message.normalized_inputs = turn.provider_meta.normalized_inputs.clone();
        assert_eq!(
            crate::model_egress::message_content_bytes(&message).unwrap(),
            bound
        );
        assert_eq!(
            crate::model_egress::model_turn_content_bytes(&turn).unwrap(),
            bound
        );
    }
}
