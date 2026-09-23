//! File selection by recorded results, shared by document and spreadsheet reads.
use super::*;

pub fn supports(name: &str) -> bool {
    matches!(
        name,
        "inspect_numbers_file"
            | "inspect_pages_file"
            | "inspect_keynote_file"
            | "inspect_powerpoint_file"
            | "inspect_word_file"
            | "inspect_excel_cell"
            | "inspect_spreadsheets"
            | "preview_spreadsheet_merge"
            | "preview_document"
            | "convert_document"
    )
}

pub fn multiple(name: &str) -> bool {
    matches!(name, "inspect_spreadsheets" | "preview_spreadsheet_merge")
}

pub fn selectors(call: &ToolCall) -> Result<Vec<ToolCall>, AgentError> {
    let value: serde_json::Value =
        serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
    let items = if multiple(&call.name) {
        value
            .get("file_sources")
            .and_then(|v| v.as_array())
            .cloned()
            .ok_or_else(unavailable)?
    } else {
        vec![
            serde_json::json!({"file_result_call_id": value.get("file_result_call_id"), "entry_name":value.get("entry_name")}),
        ]
    };
    if items.is_empty() || items.len() > 8 {
        return Err(unavailable());
    }
    items
        .into_iter()
        .map(|value| {
            let fields = value.as_object().ok_or_else(unavailable)?;
            if fields
                .keys()
                .any(|key| !matches!(key.as_str(), "file_result_call_id" | "entry_name"))
            {
                return Err(unavailable());
            }
            let selector = ToolCall {
                id: call.id.clone(),
                name: "read_text_file".into(),
                arguments_json: value.to_string(),
            };
            if read_result_id(&selector)?.is_none() {
                return Err(unavailable());
            }
            Ok(selector)
        })
        .collect()
}

pub fn without_selectors(call: &ToolCall) -> Result<ToolCall, AgentError> {
    let mut value: serde_json::Value =
        serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
    let map = value.as_object_mut().ok_or_else(unavailable)?;
    if multiple(&call.name) {
        map.remove("file_sources");
    } else {
        map.remove("file_result_call_id");
        map.remove("entry_name");
    }
    Ok(ToolCall {
        id: call.id.clone(),
        name: call.name.clone(),
        arguments_json: value.to_string(),
    })
}

pub fn add_schema(tool: &mut crate::registry::RegisteredTool) {
    if !supports(&tool.spec.name) {
        return;
    }
    let selector = serde_json::json!({"type":"object","additionalProperties":false,
        "properties":{"file_result_call_id":{"type":"string","minLength":1,"maxLength":256},
            "entry_name":{"type":"string","minLength":1,"maxLength":512}},"required":["file_result_call_id"]});
    let schema = &mut tool.spec.parameters_schema;
    let mut required = schema["required"].as_array().cloned().unwrap_or_default();
    if multiple(&tool.spec.name) {
        schema["properties"]["file_sources"] =
            serde_json::json!({"type":"array","minItems":1,"maxItems":8,"items":selector});
        required.push("file_sources".into());
    } else {
        for key in ["file_result_call_id", "entry_name"] {
            schema["properties"][key] = selector["properties"][key].clone();
        }
        required.push("file_result_call_id".into());
    }
    schema["required"] = required.into();
    tool.spec.description.push_str(" Select source files from a recorded inspect_files result in an approved conversation directory: supply its file_result_call_id and the exact entry_name. Multi-file reads use file_sources. Do not ask the user to attach files in File Manager.");
}
