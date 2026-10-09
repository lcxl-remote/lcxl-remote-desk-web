//! Model-facing input assistance; protocol versions remain server-owned metadata.
use crate::chat::ToolSpec;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::OnceLock};

fn tools() -> &'static BTreeMap<String, ToolSpec> {
    static TOOLS: OnceLock<BTreeMap<String, ToolSpec>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        crate::ai_assistant::ai_assistant_provider_registry()
            .registered_tools()
            .into_iter()
            .chain(crate::permission_tools::permission_planning_tool_registry())
            .chain(crate::capability_disclosure::capability_discovery_tool_registry())
            .chain(crate::task_status_tools::task_status_tool_registry())
            .chain(crate::subagent::tools::registry())
            .chain(crate::goal_tools::registry())
            .chain(crate::goal_tools::open_registry())
            .chain(crate::directory_tools::registry())
            .chain(crate::schedule::proposal::registry())
            .chain(crate::conversation_history::conversation_history_tool_registry())
            .chain(crate::wait_tools::wait_tool_registry())
            .map(|tool| (tool.spec.name.clone(), tool.spec))
            .chain(
                crate::schedule::management_tools::specs()
                    .into_iter()
                    .map(|tool| (tool.name.clone(), tool)),
            )
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
        for (name, schema) in properties {
            if let Some(default) = schema.get("const").or_else(|| schema.get("default")) {
                object
                    .entry(name.clone())
                    .or_insert_with(|| default.clone());
            }
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
    if name == "request_permissions"
        && let Some(items) = input.get_mut("items").and_then(Value::as_array_mut)
    {
        for item in items {
            if let Some(name) = item["tool_name"].as_str().map(str::to_owned)
                && name != "request_permissions"
                && let Some(exact) = item.get_mut("exact_input")
            {
                fill_versions(&name, exact);
            }
        }
    }
    if let Some(tool) = tools().get(name) {
        fill(&tool.parameters_schema, input);
    }
    if name == "request_permissions"
        && let Some(items) = input.get_mut("items").and_then(Value::as_array_mut)
    {
        let mut used = items
            .iter()
            .filter_map(|item| item["item_id"].as_str().map(str::to_owned))
            .collect::<std::collections::BTreeSet<_>>();
        for (index, item) in items.iter_mut().enumerate() {
            if let Some(item) = item.as_object_mut()
                && !item.contains_key("item_id")
            {
                let mut id = format!("item_{}", index + 1);
                while used.contains(&id) {
                    id.push('_');
                }
                used.insert(id.clone());
                item.insert("item_id".into(), json!(id));
            }
        }
    }
    if let Some(recipients) = input
        .pointer_mut("/draft/recipients")
        .and_then(Value::as_array_mut)
    {
        for recipient in recipients {
            if let Some(recipient) = recipient.as_object_mut() {
                recipient.entry("display_name").or_insert(Value::Null);
            }
        }
    }
    if matches!(name, "prepare_outlook_draft" | "create_local_message_draft")
        && let Some(draft) = input.get_mut("draft").and_then(Value::as_object_mut)
    {
        draft
            .entry("attachment_labels")
            .or_insert_with(|| json!([]));
    }
}

pub(crate) fn project_defaults(tool: &mut ToolSpec) {
    fn remove(schema: &mut Value, field: &str) {
        if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            properties.remove(field);
        }
        if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|key| key != field);
        }
    }
    if tool.name == "inspect_office_selection" {
        remove(&mut tool.parameters_schema, "selection_only");
    }
    if let Some(recipient) = tool
        .parameters_schema
        .pointer_mut("/properties/draft/properties/recipients/items")
    {
        if let Some(required) = recipient.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|key| key != "display_name");
        }
        if tool.name == "prepare_gmail_draft" {
            remove(recipient, "role");
        }
    }
    if matches!(
        tool.name.as_str(),
        "prepare_outlook_draft" | "prepare_gmail_draft"
    ) && let Some(draft) = tool.parameters_schema.pointer_mut("/properties/draft")
    {
        remove(draft, "attachment_labels");
    }
    if tool.name == "create_local_message_draft"
        && let Some(required) = tool
            .parameters_schema
            .pointer_mut("/properties/draft/required")
            .and_then(Value::as_array_mut)
    {
        required.retain(|key| key != "attachment_labels");
    }
    if tool.name == "request_permissions"
        && let Some(required) = tool
            .parameters_schema
            .pointer_mut("/properties/items/items/required")
            .and_then(Value::as_array_mut)
    {
        required.retain(|key| key != "item_id");
    }
}

pub(crate) fn project_fixed_arguments(tool: &str, value: &mut Value) {
    if tool == "inspect_office_selection"
        && let Some(object) = value.as_object_mut()
    {
        object.remove("selection_only");
    }
    if tool == "prepare_gmail_draft"
        && let Some(draft) = value.get_mut("draft").and_then(Value::as_object_mut)
    {
        draft.remove("attachment_labels");
        if let Some(recipients) = draft.get_mut("recipients").and_then(Value::as_array_mut) {
            for recipient in recipients {
                if let Some(recipient) = recipient.as_object_mut() {
                    recipient.remove("role");
                }
            }
        }
    }
    if tool == "prepare_outlook_draft"
        && let Some(draft) = value.get_mut("draft").and_then(Value::as_object_mut)
    {
        draft.remove("attachment_labels");
    }
}

fn example(schema: &Value) -> Value {
    if let Some(value) = schema["examples"]
        .as_array()
        .and_then(|values| values.first())
    {
        return value.clone();
    }
    if let Some(v) = schema.get("const").or_else(|| schema.get("default")) {
        return v.clone();
    }
    if let Some(v) = schema["enum"].as_array().and_then(|v| v.first()) {
        return v.clone();
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(variants) = schema[key].as_array() {
            for variant in variants {
                let mut merged = schema.clone();
                merged.as_object_mut().unwrap().remove(key);
                merge_schema(&mut merged, variant);
                let sample = example(&merged);
                if check(schema, &sample, "$", 0).is_ok() {
                    return sample;
                }
            }
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
            (0..schema["minItems"]
                .as_u64()
                .unwrap_or(0)
                .min(schema["maxItems"].as_u64().unwrap_or(20))
                .min(20))
                .map(|_| example(&schema["items"]))
                .collect(),
        ),
        Some("integer" | "number") => schema
            .get("minimum")
            .cloned()
            .unwrap_or_else(|| json!(schema["maximum"].as_f64().unwrap_or(1.0).min(1.0) as i64)),
        Some("boolean") => json!(false),
        Some("null") => Value::Null,
        _ => string_example(schema),
    }
}

fn merge_schema(base: &mut Value, extra: &Value) {
    let Some(fields) = extra.as_object() else {
        return;
    };
    for (name, value) in fields {
        if name == "properties" {
            if !base[name].is_object() {
                base[name] = json!({});
            }
            if let Some(properties) = value.as_object() {
                for (key, child) in properties {
                    if base[name].get(key).is_some() {
                        merge_schema(&mut base[name][key], child);
                    } else {
                        base[name][key] = child.clone();
                    }
                }
            }
        } else if name == "required" {
            let mut required = base[name].as_array().cloned().unwrap_or_default();
            for key in value.as_array().into_iter().flatten() {
                if !required.contains(key) {
                    required.push(key.clone());
                }
            }
            base[name] = json!(required);
        } else {
            base[name] = value.clone();
        }
    }
}

fn string_example(schema: &Value) -> Value {
    if schema["format"] == "date-time" {
        return json!("2026-01-01T00:00:00Z");
    }
    let pattern = schema["pattern"].as_str().unwrap_or("");
    let mut value = if pattern.contains("[0-9a-f]{64}") {
        "0".repeat(64)
    } else if pattern.starts_with("^https://") {
        "https://example.invalid/".into()
    } else if pattern.starts_with("^Merged!") {
        "Merged!A1".into()
    } else if pattern.starts_with("^[A-Z]{1,3}") {
        "A1".into()
    } else if pattern == "^=" {
        "=SUM(A1:A2)".into()
    } else if pattern.contains("A-Za-z0-9._-") {
        ".txt".into()
    } else if let Some(extension) = [
        "draft.txt",
        "numbers",
        "pages",
        "key",
        "xlsx",
        "docx",
        "pptx",
    ]
    .into_iter()
    .find(|extension| {
        pattern
            .replace('\\', "")
            .contains(&format!(".{extension}$"))
    }) {
        format!("example.{extension}")
    } else {
        "<value>".into()
    };
    if let Some(max) = schema["maxLength"].as_u64() {
        value = value.chars().take(max as usize).collect();
    }
    while value.chars().count() < schema["minLength"].as_u64().unwrap_or(0) as usize {
        value.push('x');
    }
    json!(value)
}

/// Check structural constraints before typed parsing can collapse a useful error.
pub fn validate_format(name: &str, input: &Value) -> Result<(), String> {
    let Some(tool) = tools().get(name) else {
        return Ok(());
    };
    check(&tool.parameters_schema, input, "$", 0)
        .map_err(|reason| describe_error(name, &reason.message))
}
pub fn validate_format_with_schema(
    name: &str,
    schema: &Value,
    input: &Value,
) -> Result<(), String> {
    validate_format_reported(name, schema, input).map_err(|reason| reason.message)
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatError {
    pub message: String,
    pub issue: crate::model_observability::InputIssue,
    pub schema_path: Option<String>,
}

impl FormatError {
    fn new(
        issue: crate::model_observability::InputIssue,
        schema_path: &str,
        message: String,
    ) -> Self {
        Self {
            message,
            issue,
            schema_path: (schema_path.len() <= 192).then(|| schema_path.to_string()),
        }
    }
}

pub fn validate_format_reported(
    name: &str,
    schema: &Value,
    input: &Value,
) -> Result<(), FormatError> {
    check(schema, input, "$", 0).map_err(|mut error| {
        error.message = describe_error_with_schema(name, schema, &error.message);
        error
    })
}

fn check(schema: &Value, value: &Value, path: &str, depth: usize) -> Result<(), FormatError> {
    check_at(schema, value, path, "$", depth)
}

fn check_at(
    schema: &Value,
    value: &Value,
    path: &str,
    schema_path: &str,
    depth: usize,
) -> Result<(), FormatError> {
    use crate::model_observability::InputIssue as Issue;
    if schema == &Value::Bool(false) {
        return Err(FormatError::new(
            Issue::Combination,
            schema_path,
            format!("Invalid input at {path}: value is forbidden"),
        ));
    }
    if depth > 32 {
        return Err(FormatError::new(
            Issue::Length,
            schema_path,
            format!("Invalid input at {path}: nesting exceeds 32 levels"),
        ));
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(variants) = schema[key].as_array() {
            let errors: Vec<_> = variants
                .iter()
                .filter_map(|s| check_at(s, value, path, schema_path, depth + 1).err())
                .collect();
            let matches = variants.len() - errors.len();
            if matches == 0 || (key == "oneOf" && matches != 1) {
                return Err(FormatError::new(
                    Issue::Combination,
                    schema_path,
                    format!(
                        "Invalid input at {path}: {key} requires {} matching variant(s), observed {matches}: {}",
                        if key == "oneOf" {
                            "exactly one"
                        } else {
                            "at least one"
                        },
                        errors
                            .iter()
                            .map(|error| error.message.as_str())
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                ));
            }
        }
    }
    if let Some(variants) = schema["allOf"].as_array() {
        for variant in variants {
            check_at(variant, value, path, schema_path, depth + 1)?;
        }
    }
    if let Some(negated) = schema.get("not")
        && check_at(negated, value, path, schema_path, depth + 1).is_ok()
    {
        return Err(FormatError::new(
            Issue::Combination,
            schema_path,
            format!("Invalid input at {path}: forbidden parameter combination"),
        ));
    }
    if let Some(condition) = schema.get("if") {
        let branch = if check_at(condition, value, path, schema_path, depth + 1).is_ok() {
            "then"
        } else {
            "else"
        };
        if let Some(branch) = schema.get(branch) {
            check_at(branch, value, path, schema_path, depth + 1)?;
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
        return Err(FormatError::new(
            Issue::Type,
            schema_path,
            format!("Invalid input at {path}: expected type {}", schema["type"]),
        ));
    }
    if let Some(expected) = schema.get("const")
        && expected != value
    {
        return Err(FormatError::new(
            Issue::Enum,
            schema_path,
            format!("Invalid input at {path}: expected {expected}"),
        ));
    }
    if let Some(allowed) = schema["enum"].as_array()
        && !allowed.contains(value)
    {
        return Err(FormatError::new(
            Issue::Enum,
            schema_path,
            format!("Invalid input at {path}: allowed values {}", schema["enum"]),
        ));
    }
    if let Some(object) = value.as_object() {
        if let Some(dependencies) = schema["dependentRequired"].as_object() {
            for (key, required) in dependencies {
                if object.contains_key(key) {
                    for field in required
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        if !object.contains_key(field) {
                            return Err(FormatError::new(
                                Issue::MissingField,
                                &format!("{schema_path}.{field}"),
                                format!("Invalid input at {path}.{field}: required with {key}"),
                            ));
                        }
                    }
                }
            }
        }
        if let Some(required) = schema["required"].as_array() {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    return Err(FormatError::new(
                        Issue::MissingField,
                        &format!("{schema_path}.{key}"),
                        format!("Invalid input at {path}.{key}: missing required field"),
                    ));
                }
            }
        }
        for (key, value) in object {
            if let Some(child) = schema["properties"].get(key) {
                check_at(
                    child,
                    value,
                    &format!("{path}.{key}"),
                    &format!("{schema_path}.{key}"),
                    depth + 1,
                )?;
            } else if schema["additionalProperties"] == false {
                return Err(FormatError::new(
                    Issue::UnknownField,
                    &format!("{schema_path}.*"),
                    format!("Invalid input at {path}.{key}: unknown field"),
                ));
            } else if schema["additionalProperties"].is_object() {
                check_at(
                    &schema["additionalProperties"],
                    value,
                    &format!("{path}.{key}"),
                    &format!("{schema_path}.*"),
                    depth + 1,
                )?;
            }
        }
    }
    if let Some(array) = value.as_array() {
        if schema["uniqueItems"] == true {
            for (i, item) in array.iter().enumerate() {
                if array[..i].contains(item) {
                    return Err(FormatError::new(
                        Issue::Combination,
                        schema_path,
                        format!("Invalid input at {path}[{i}]: duplicate array item"),
                    ));
                }
            }
        }
        for (i, value) in array.iter().enumerate() {
            check_at(
                &schema["items"],
                value,
                &format!("{path}[{i}]"),
                &format!("{schema_path}[]"),
                depth + 1,
            )?;
        }
    }
    if let Some(text) = value.as_str() {
        if schema["x-maxUtf8Bytes"]
            .as_u64()
            .is_some_and(|limit| text.len() as u64 > limit)
        {
            return Err(FormatError::new(
                Issue::Length,
                schema_path,
                format!(
                    "Invalid input at {path}: maximum {} UTF-8 bytes, observed {}",
                    schema["x-maxUtf8Bytes"],
                    text.len()
                ),
            ));
        }
        if let Some(pattern) = schema["pattern"].as_str() {
            let regex = regex::Regex::new(pattern).map_err(|_| {
                FormatError::new(
                    Issue::SchemaUnavailable,
                    schema_path,
                    format!("Invalid schema pattern at {path}"),
                )
            })?;
            if !regex.is_match(text) {
                return Err(FormatError::new(
                    Issue::Pattern,
                    schema_path,
                    format!("Invalid input at {path}: expected pattern {pattern}"),
                ));
            }
        }
        if schema["format"] == "date-time" && chrono::DateTime::parse_from_rfc3339(text).is_err() {
            return Err(FormatError::new(
                Issue::Pattern,
                schema_path,
                format!("Invalid input at {path}: expected RFC3339 date-time"),
            ));
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
        if let Some(n) = n
            && (schema[min].as_f64().is_some_and(|m| n < m)
                || schema[max].as_f64().is_some_and(|m| n > m))
        {
            return Err(FormatError::new(
                Issue::Length,
                schema_path,
                format!(
                    "Invalid input at {path}: bounds {min}={}, {max}={}",
                    schema[min], schema[max]
                ),
            ));
        }
    }
    Ok(())
}

pub fn describe_error(name: &str, reason: &str) -> String {
    let Some(tool) = tools().get(name) else {
        return format!("tool error: {reason}");
    };
    let mut tool = tool.clone();
    crate::ui_model_ids::project_tool(&mut tool);
    describe_error_with_schema(name, &tool.parameters_schema, reason)
}

pub fn describe_error_with_schema(name: &str, schema: &Value, reason: &str) -> String {
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

    let mut schema = schema.clone();
    hide_versions(&mut schema);
    let sample = example(&schema);
    format!(
        "tool error: {reason}. Tool: {name}. Required fields: {}. Structural example (replace placeholders with observed IDs and intended values): {sample}. Load the target Provider tool with describe_tools for field constraints before requesting permission (built-in conversation tools must not be loaded); correct the arguments rather than repeating them or asking the user to supply the format.",
        schema.get("required").unwrap_or(&Value::Null)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reported_errors_expose_schema_paths_without_dynamic_property_values() {
        use crate::model_observability::InputIssue;
        let schema = json!({"type":"object", "properties":{"items":{"type":"array", "items":{"type":"object", "properties":{"count":{"type":"integer"}}, "additionalProperties":false}}}});
        let error = validate_format_reported(
            "test",
            &schema,
            &json!({"items":[{"secret-user-key":"secret-value"}]}),
        )
        .unwrap_err();
        assert_eq!(error.issue, InputIssue::UnknownField);
        assert_eq!(error.schema_path.as_deref(), Some("$.items[].*"));
        let error = validate_format_reported(
            "test",
            &schema,
            &json!({"items":[{"count":"secret-value"}]}),
        )
        .unwrap_err();
        assert_eq!(error.issue, InputIssue::Type);
        assert_eq!(error.schema_path.as_deref(), Some("$.items[].count"));
        assert!(!error.schema_path.unwrap().contains("secret"));
    }

    #[test]
    fn unavailable_server_pattern_is_not_a_model_input_error() {
        let error = validate_format_reported(
            "test",
            &json!({"type":"string", "pattern":"["}),
            &json!("value"),
        )
        .unwrap_err();
        assert_eq!(
            error.issue,
            crate::model_observability::InputIssue::SchemaUnavailable
        );
    }
    #[test]
    fn every_model_schema_has_a_valid_structural_example_and_projection_is_stable() {
        let mut failures = Vec::new();
        for original in tools().values() {
            let mut tool = original.clone();
            crate::ui_model_ids::project_tool(&mut tool);
            let sample = example(&tool.parameters_schema);
            if let Err(error) = check(&tool.parameters_schema, &sample, "$", 0) {
                failures.push(format!("{}: {error:?}: {sample}", tool.name));
            }
            let mut again = tool.clone();
            crate::ui_model_ids::project_tool(&mut again);
            assert_eq!(
                again.parameters_schema, tool.parameters_schema,
                "{} schema projection changed twice",
                tool.name
            );
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn constraints_enforce_patterns_dates_unique_arrays_and_exclusive_combinations() {
        for (schema, good, bad) in [
            (
                json!({"type":"string","pattern":"^[0-9a-f]{64}$"}),
                json!("a".repeat(64)),
                json!("not-a-hash"),
            ),
            (
                json!({"type":"string","format":"date-time"}),
                json!("2026-10-07T01:02:03-07:00"),
                json!("tomorrow"),
            ),
            (
                json!({"type":"array","uniqueItems":true,"items":{"type":"string"}}),
                json!(["a", "b"]),
                json!(["a", "a"]),
            ),
            (
                json!({"oneOf":[{"type":"integer"},{"minimum":1}]}),
                json!(0),
                json!(1),
            ),
            (
                json!({"type":"object","dependentRequired":{"a":["b"]}}),
                json!({"a":1,"b":2}),
                json!({"a":1}),
            ),
            (
                json!({"not":{"required":["a","b"]}}),
                json!({"a":1}),
                json!({"a":1,"b":2}),
            ),
            (
                json!({"allOf":[{"minimum":1},{"maximum":2}]}),
                json!(1),
                json!(3),
            ),
            (
                json!({"if":{"required":["a"]},"then":{"required":["b"]},"else":{"required":["c"]}}),
                json!({"a":1,"b":2}),
                json!({"a":1}),
            ),
            (
                json!({"type":"object","additionalProperties":{"type":"integer"}}),
                json!({"a":1}),
                json!({"a":"bad"}),
            ),
        ] {
            assert!(check(&schema, &good, "$", 0).is_ok());
            assert!(check(&schema, &bad, "$", 0).is_err());
        }
    }

    fn model_schema(name: &str) -> Value {
        let mut spec = tools().get(name).unwrap().clone();
        crate::ui_model_ids::project_tool(&mut spec);
        spec.parameters_schema
    }

    #[test]
    fn audited_input_defects_and_parameter_combinations_are_rejected_early() {
        let cases = [
            (
                "read_current_screen",
                json!({"display":"one","window_id":"window"}),
            ),
            (
                "inspect_desktop_ui",
                json!({"element_id":"control","element_only":true,"queries":["wrong"]}),
            ),
            ("inspect_files", json!({"cursor":"cursor","paginate":false})),
            ("read_text_file", json!({"entry_name":"notes.txt"})),
            (
                "request_permissions",
                json!({"items":[{"tool_name":"execute_ui_actions","reason":"action","application_scope":{"application_id":"app","actions":["invoke"]},"exact_input":{}}]}),
            ),
            (
                "request_scheduled_task",
                json!({"kind":"fresh_task","title":"test","prompt":"test","rule":{"kind":"after_confirmation","delay_seconds":10},"time_input":{"timezone":"UTC","reference_date":"2026-10-07","local_time":"08:00:00","rule":{"kind":"once"}}}),
            ),
            (
                "wait_subagents",
                json!({"task_ids":["same","same"],"mode":"all_terminal"}),
            ),
            (
                "create_word_report",
                json!({"preview_id":"p","file_name":"report.docx","title":"Report","web_source_ids":["source_id"]}),
            ),
            (
                "convert_document",
                json!({"source":{"kind":"artifact_result","file_result_call_id":"result"},"output_name":"report.pdf","conversion":{"kind":"text_to_pdf","pages":[{"start":1,"end":2}]}}),
            ),
            (
                "launch_application",
                json!({"target":{"kind":"macos_bundle","value":"/Applications/Test.app"},"cwd":"/tmp","run_as_admin":true}),
            ),
            (
                "read_conversation_attachment",
                json!({"attachment_id":"part","queries":["test"],"start_line":1}),
            ),
        ];
        for (name, input) in cases {
            assert!(
                check(&model_schema(name), &input, "$", 0).is_err(),
                "{name}: {input}"
            );
        }
        assert!(check(&model_schema("read_text_file"), &json!({}), "$", 0).is_ok());
        assert!(check(&model_schema("inspect_live_document"), &json!({}), "$", 0).is_ok());
        assert_eq!(
            model_schema("inspect_live_document")["properties"]["max_bytes"]["default"],
            32768
        );
        assert!(
            check(
                &model_schema("create_word_report"),
                &json!({"preview_id":"p","file_name":"报告.docx","title":"报告"}),
                "$",
                0
            )
            .is_ok()
        );
    }

    #[test]
    fn natural_language_limits_count_unicode_and_native_content_keeps_a_byte_limit() {
        let call = crate::chat::ToolCall {
            id: "goal".into(),
            name: crate::goal_tools::REQUEST_GOAL_TOOL_NAME.into(),
            arguments_json: json!({"goal_text":"目标😀".repeat(3000)}).to_string(),
        };
        validate_format_with_schema(
            &call.name,
            &model_schema(&call.name),
            &serde_json::from_str(&call.arguments_json).unwrap(),
        )
        .unwrap();
        crate::goal_tools::parse_open(&call).unwrap();
        crate::subagent::role::validate_task(&"任务😀".repeat(2000), &["验收😀".repeat(300)])
            .unwrap();
        let schema = json!({"type":"string","maxLength":4,"x-maxUtf8Bytes":8});
        assert!(check(&schema, &json!("中文"), "$", 0).is_ok());
        assert!(check(&schema, &json!("中文😀"), "$", 0).is_err());
    }

    #[test]
    fn error_examples_use_current_shell_and_maximum_runtime() {
        let mut tool = tools()["exec_command"].clone();
        crate::ui_model_ids::project_tool(&mut tool);
        tool.parameters_schema["properties"]["shell"]["enum"] = json!(["zsh"]);
        tool.parameters_schema["properties"]["timeout_ms"]["maximum"] = json!(50);
        tool.parameters_schema["properties"]["timeout_ms"]
            .as_object_mut()
            .unwrap()
            .remove("default");
        let text = describe_error_with_schema(
            &tool.name,
            &tool.parameters_schema,
            "missing required shell",
        );
        assert!(text.contains("\"shell\":\"zsh\""));
        assert!(!text.contains("\"shell\":\"bash\""));
        assert!(
            check(
                &tool.parameters_schema,
                &example(&tool.parameters_schema),
                "$",
                0
            )
            .is_ok()
        );
        let outlook = model_schema("prepare_outlook_draft");
        assert!(
            outlook
                .pointer("/properties/draft/properties/attachment_labels")
                .is_none()
        );
        let input = example(&outlook);
        assert!(check(&outlook, &input, "$", 0).is_ok());
    }
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
        fill_versions("prepare_outlook_draft", &mut input);
        assert_eq!(
            input["draft"]["schema_version"],
            desk_agent_protocol::communication::COMMUNICATION_SCHEMA_VERSION
        );
        let mut command = json!({"shell":"bash","command":"pwd","timeout_ms":10000});
        fill_versions("exec_command", &mut command);
        assert_eq!(command["schema_version"], 1);
        let grant = json!({"items":[{"tool_name":"exec_command","exact_input":{"shell":"bash","command":"pwd","timeout_ms":10000}}]});
        let mut resolved = grant.clone();
        fill_versions("request_permissions", &mut resolved);
        assert_eq!(resolved["items"][0]["exact_input"]["schema_version"], 1);
        assert!(crate::ui_model_ids::same_call_input(
            "request_permissions",
            &grant.to_string(),
            &resolved.to_string()
        ));
    }
    #[test]
    fn command_without_version_preserves_exact_approval_identity() {
        let input = json!({"shell":"bash","command":"pwd","timeout_ms":10000});
        let original = input.to_string();
        let canonical =
            crate::permission_tools::canonical_tool_permission_input_json("exec_command", input)
                .unwrap();
        let policy = crate::command_confirmation::test_policy();
        let confirmation = policy.prepare(&canonical, 1).unwrap();
        policy.revalidate(&confirmation, &canonical, 1).unwrap();
        assert!(crate::ui_model_ids::same_call_input(
            "exec_command",
            &original,
            &canonical
        ));
        assert!(!crate::ui_model_ids::same_call_input(
            "exec_command",
            &original,
            &canonical.replace("pwd", "date")
        ));
        let error = validate_format("exec_command", &json!({"schema_version":1,"command":"pwd"}))
            .unwrap_err();
        assert!(error.contains("$.shell"));
        assert!(error.contains("Structural example"));
    }
    #[test]
    fn input_error_explains_required_fields_and_example_without_versions() {
        let text = describe_error("exec_command", "missing field `shell`");
        assert!(text.contains("missing field `shell`"));
        assert!(text.contains("timeout_ms"));
        assert!(text.contains("\"command\":\"pwd\""));
        assert!(!text.contains("schema_version"));
    }
}
