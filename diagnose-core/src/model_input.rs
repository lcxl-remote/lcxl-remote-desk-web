//! Model-facing input assistance; protocol versions remain server-owned metadata.
use crate::chat::ToolSpec;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::OnceLock};

fn tools() -> &'static BTreeMap<String, ToolSpec> {
    static TOOLS: OnceLock<BTreeMap<String, ToolSpec>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        crate::device_assistant::device_assistant_provider_registry()
            .registered_tools()
            .into_iter()
            .chain(crate::permission_tools::permission_planning_tool_registry())
            .chain(crate::capability_disclosure::capability_discovery_tool_registry())
            .map(|tool| (tool.spec.name.clone(), tool.spec))
            .collect()
    })
}

pub fn hide_versions(schema: &mut Value) {
    if let Some(object) = schema.as_object_mut() {
        let fixed = object
            .get("properties")
            .and_then(|v| v.get("schema_version"))
            .and_then(|v| v.get("const"))
            .is_some();
        if fixed {
            object
                .get_mut("properties")
                .and_then(Value::as_object_mut)
                .unwrap()
                .remove("schema_version");
            if let Some(required) = object.get_mut("required").and_then(Value::as_array_mut) {
                required.retain(|v| v != "schema_version");
            }
        }
        for value in object.values_mut() {
            hide_versions(value);
        }
    } else if let Some(array) = schema.as_array_mut() {
        for value in array {
            hide_versions(value);
        }
    }
}

fn fill(schema: &Value, input: &mut Value) {
    if let (Some(properties), Some(object)) =
        (schema["properties"].as_object(), input.as_object_mut())
    {
        if let Some(version) = properties
            .get("schema_version")
            .and_then(|v| v.get("const"))
        {
            object
                .entry("schema_version")
                .or_insert_with(|| version.clone());
        }
        for (key, value) in object {
            if let Some(child) = properties.get(key) {
                fill(child, value);
            }
        }
    }
    if let Some(items) = input.as_array_mut() {
        for value in items {
            fill(&schema["items"], value);
        }
    }
    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(variants) = schema[key].as_array() {
            for variant in variants {
                if variant["properties"]["kind"]["const"].is_null()
                    || variant["properties"]["kind"]["const"] == input["kind"]
                {
                    fill(variant, input);
                }
            }
        }
    }
}

pub fn fill_versions(name: &str, input: &mut Value) {
    if name == "request_capability_grants" {
        if let Some(items) = input.get_mut("items").and_then(Value::as_array_mut) {
            for item in items {
                if let Some(name) = item["tool_name"].as_str().map(str::to_owned) {
                    if name != "request_capability_grants" {
                        if let Some(exact) = item.get_mut("exact_input") {
                            fill_versions(&name, exact);
                        }
                    }
                }
            }
        }
    }
    if let Some(tool) = tools().get(name) {
        fill(&tool.parameters_schema, input);
    }
}

fn example(schema: &Value) -> Value {
    if let Some(v) = schema.get("const").or_else(|| schema.get("default")) {
        return v.clone();
    }
    if let Some(v) = schema["enum"].as_array().and_then(|v| v.first()) {
        return v.clone();
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(v) = schema[key].as_array().and_then(|v| v.first()) {
            let mut merged = schema.clone();
            merged.as_object_mut().unwrap().remove(key);
            if let Some(fields) = v.as_object() {
                for (name, child) in fields {
                    if name == "properties" {
                        if !merged[name].is_object() {
                            merged[name] = json!({});
                        }
                        if let Some(properties) = child.as_object() {
                            for (key, value) in properties {
                                merged[name][key] = value.clone();
                            }
                        }
                    } else if name == "required" {
                        let mut required = merged[name].as_array().cloned().unwrap_or_default();
                        if let Some(extra) = child.as_array() {
                            for item in extra {
                                if !required.contains(item) {
                                    required.push(item.clone());
                                }
                            }
                        }
                        merged[name] = json!(required);
                    } else {
                        merged[name] = child.clone();
                    }
                }
            }
            return example(&merged);
        }
    }
    match schema["type"].as_str().or_else(|| {
        schema["type"]
            .as_array()
            .and_then(|v| v.first())
            .and_then(Value::as_str)
    }) {
        Some("object") => {
            let mut object = serde_json::Map::new();
            if let Some(required) = schema["required"].as_array() {
                for key in required.iter().filter_map(Value::as_str) {
                    object.insert(key.into(), example(&schema["properties"][key]));
                }
            }
            Value::Object(object)
        }
        Some("array") => Value::Array(
            (0..schema["minItems"].as_u64().unwrap_or(1).min(20))
                .map(|_| example(&schema["items"]))
                .collect(),
        ),
        Some("integer" | "number") => schema.get("minimum").cloned().unwrap_or(json!(1)),
        Some("boolean") => json!(false),
        Some("null") => Value::Null,
        _ => json!("<value>"),
    }
}

/// Check structural constraints before typed parsing can collapse a useful error.
pub fn validate_format(name: &str, input: &Value) -> Result<(), String> {
    let Some(tool) = tools().get(name) else {
        return Ok(());
    };
    check(&tool.parameters_schema, input, "$", 0).map_err(|reason| describe_error(name, &reason))
}
pub fn validate_format_with_schema(
    name: &str,
    schema: &Value,
    input: &Value,
) -> Result<(), String> {
    check(schema, input, "$", 0).map_err(|reason| describe_error(name, &reason))
}
fn check(schema: &Value, value: &Value, path: &str, depth: usize) -> Result<(), String> {
    if depth > 32 {
        return Err(format!(
            "Invalid input at {path}: nesting exceeds 32 levels"
        ));
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(variants) = schema[key].as_array() {
            let errors: Vec<_> = variants
                .iter()
                .filter_map(|s| check(s, value, path, depth + 1).err())
                .collect();
            if errors.len() == variants.len() {
                return Err(format!(
                    "Invalid input at {path}: no allowed variant matches: {}",
                    errors.join("; ")
                ));
            }
        }
    }
    let matches_type = |kind: &str| match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    };
    let valid = match &schema["type"] {
        Value::String(kind) => matches_type(kind),
        Value::Array(kinds) => kinds.iter().filter_map(Value::as_str).any(matches_type),
        _ => true,
    };
    if !valid {
        return Err(format!(
            "Invalid input at {path}: expected type {}",
            schema["type"]
        ));
    }
    if let Some(expected) = schema.get("const") {
        if expected != value {
            return Err(format!("Invalid input at {path}: expected {expected}"));
        }
    }
    if let Some(allowed) = schema["enum"].as_array() {
        if !allowed.contains(value) {
            return Err(format!(
                "Invalid input at {path}: allowed values {}",
                schema["enum"]
            ));
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema["required"].as_array() {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    return Err(format!(
                        "Invalid input at {path}.{key}: missing required field"
                    ));
                }
            }
        }
        for (key, value) in object {
            if let Some(child) = schema["properties"].get(key) {
                check(child, value, &format!("{path}.{key}"), depth + 1)?;
            } else if schema["additionalProperties"] == false {
                return Err(format!("Invalid input at {path}.{key}: unknown field"));
            }
        }
    }
    if let Some(array) = value.as_array() {
        for (i, value) in array.iter().enumerate() {
            check(&schema["items"], value, &format!("{path}[{i}]"), depth + 1)?;
        }
    }
    for (min, max, n) in [
        (
            "minLength",
            "maxLength",
            value.as_str().map(|v| v.chars().count() as f64),
        ),
        (
            "minItems",
            "maxItems",
            value.as_array().map(|v| v.len() as f64),
        ),
        ("minimum", "maximum", value.as_f64()),
    ] {
        if let Some(n) = n {
            if schema[min].as_f64().is_some_and(|m| n < m)
                || schema[max].as_f64().is_some_and(|m| n > m)
            {
                return Err(format!(
                    "Invalid input at {path}: bounds {min}={}, {max}={}",
                    schema[min], schema[max]
                ));
            }
        }
    }
    Ok(())
}

pub fn describe_error(name: &str, reason: &str) -> String {
    if reason.contains("Structural example") {
        return reason.into();
    }
    let lower = reason.to_ascii_lowercase();
    if ![
        "invalid",
        "missing",
        "expected",
        "required",
        "unknown field",
        "parse",
        "format",
    ]
    .iter()
    .any(|word| lower.contains(word))
    {
        return format!("tool error: {reason}");
    }

    let Some(tool) = tools().get(name) else {
        return format!("tool error: {reason}");
    };
    let mut tool = tool.clone();
    crate::ui_model_ids::project_tool(&mut tool);
    let schema = &tool.parameters_schema;
    let sample = if name == crate::command_confirmation::COMMAND_TOOL {
        json!({"shell":"bash","command":"pwd","timeout_ms":10000})
    } else {
        example(schema)
    };
    format!(
        "tool error: {reason}. Tool: {name}. Required fields: {}. Structural example (replace placeholders with observed IDs and intended values): {sample}. Load the target Provider tool with load_capability_details for field constraints before requesting permission (built-in conversation tools must not be loaded); correct the arguments rather than repeating them or asking the user to supply the format.",
        schema.get("required").unwrap_or(&Value::Null)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_fixed_versions_are_hidden_and_nested_versions_are_restored() {
        for tool in tools().values() {
            let mut schema = tool.parameters_schema.clone();
            hide_versions(&mut schema);
            fn check(v: &Value) {
                if let Some(p) = v.get("properties") {
                    assert!(p.get("schema_version").is_none());
                }
                if let Some(o) = v.as_object() {
                    for v in o.values() {
                        check(v);
                    }
                }
                if let Some(a) = v.as_array() {
                    for v in a {
                        check(v);
                    }
                }
            }
            check(&schema);
        }
        let mut input = json!({"draft":{"recipients":[],"subject":"test","body_plain_text":"test","attachment_labels":[]}});
        fill_versions("prepare_outlook_new_draft_handoff", &mut input);
        assert_eq!(
            input["draft"]["schema_version"],
            desk_agent_protocol::communication::COMMUNICATION_SCHEMA_VERSION
        );
        let mut command = json!({"shell":"bash","command":"pwd","timeout_ms":10000});
        fill_versions("execute_confirmed_command", &mut command);
        assert_eq!(command["schema_version"], 1);
        let grant = json!({"items":[{"tool_name":"execute_confirmed_command","exact_input":{"shell":"bash","command":"pwd","timeout_ms":10000}}]});
        let mut resolved = grant.clone();
        fill_versions("request_capability_grants", &mut resolved);
        assert_eq!(resolved["items"][0]["exact_input"]["schema_version"], 1);
        assert!(crate::ui_model_ids::same_call_input(
            "request_capability_grants",
            &grant.to_string(),
            &resolved.to_string()
        ));
    }
    #[test]
    fn command_without_version_preserves_exact_approval_identity() {
        let input = json!({"shell":"bash","command":"pwd","timeout_ms":10000});
        let original = input.to_string();
        let canonical = crate::permission_tools::canonical_tool_permission_input_json(
            "execute_confirmed_command",
            input,
        )
        .unwrap();
        let policy = crate::command_confirmation::test_policy();
        let confirmation = policy.prepare(&canonical, 1).unwrap();
        policy.revalidate(&confirmation, &canonical, 1).unwrap();
        assert!(crate::ui_model_ids::same_call_input(
            "execute_confirmed_command",
            &original,
            &canonical
        ));
        assert!(!crate::ui_model_ids::same_call_input(
            "execute_confirmed_command",
            &original,
            &canonical.replace("pwd", "date")
        ));
        let error = validate_format(
            "execute_confirmed_command",
            &json!({"schema_version":1,"command":"pwd"}),
        )
        .unwrap_err();
        assert!(error.contains("$.shell"));
        assert!(error.contains("Structural example"));
    }
    #[test]
    fn input_error_explains_required_fields_and_example_without_versions() {
        let text = describe_error("execute_confirmed_command", "missing field `shell`");
        assert!(text.contains("missing field `shell`"));
        assert!(text.contains("timeout_ms"));
        assert!(text.contains("\"command\":\"pwd\""));
        assert!(!text.contains("schema_version"));
    }
}
