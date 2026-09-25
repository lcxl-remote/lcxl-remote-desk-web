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
    let fields = value.as_object().ok_or_else(unavailable)?;
    let uses_new = fields.contains_key("source") || fields.contains_key("sources");
    if multiple(&call.name) {
        if fields.contains_key("source")
            || fields.contains_key("file_result_call_id")
            || fields.contains_key("entry_name")
        {
            return Err(unavailable());
        }
    } else if fields.contains_key("sources") || fields.contains_key("file_sources") {
        return Err(unavailable());
    }
    if uses_new
        && ["file_result_call_id", "entry_name", "file_sources"]
            .iter()
            .any(|key| fields.contains_key(*key))
    {
        return Err(unavailable());
    }
    let items = if multiple(&call.name) {
        if uses_new {
            fields.get("sources")
        } else {
            fields.get("file_sources")
        }
        .and_then(serde_json::Value::as_array)
        .cloned()
        .ok_or_else(unavailable)?
    } else if uses_new {
        vec![fields.get("source").cloned().ok_or_else(unavailable)?]
    } else {
        vec![serde_json::json!({
            "file_result_call_id": fields.get("file_result_call_id"),
            "entry_name": fields.get("entry_name")
        })]
    };
    if items.is_empty() || items.len() > 8 {
        return Err(unavailable());
    }
    items
        .into_iter()
        .map(|value| {
            let fields = value.as_object().ok_or_else(unavailable)?;
            let selector = if uses_new {
                let kind = fields.get("kind").and_then(serde_json::Value::as_str);
                let id = fields.get("file_result_call_id").ok_or_else(unavailable)?;
                match kind {
                    Some("directory_entry")
                        if fields.len() == 3 && fields.contains_key("entry_name") =>
                    {
                        serde_json::json!({"file_result_call_id":id,"entry_name":fields["entry_name"]})
                    }
                    Some("artifact_result") if fields.len() == 2 => {
                        serde_json::json!({"file_result_call_id":id})
                    }
                    _ => return Err(unavailable()),
                }
            } else {
                if fields
                    .keys()
                    .any(|key| !matches!(key.as_str(), "file_result_call_id" | "entry_name"))
                {
                    return Err(unavailable());
                }
                value
            };
            let selector = ToolCall {
                id: call.id.clone(),
                name: "read_text_file".into(),
                arguments_json: selector.to_string(),
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
    for key in [
        "source",
        "sources",
        "file_sources",
        "file_result_call_id",
        "entry_name",
    ] {
        map.remove(key);
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
    let selector = serde_json::json!({"oneOf":[
        {"type":"object","additionalProperties":false,
            "properties":{"kind":{"const":"directory_entry"},
                "file_result_call_id":{"type":"string","minLength":1,"maxLength":256},
                "entry_name":{"type":"string","minLength":1,"maxLength":512}},
            "required":["kind","file_result_call_id","entry_name"]},
        {"type":"object","additionalProperties":false,
            "properties":{"kind":{"const":"artifact_result"},
                "file_result_call_id":{"type":"string","minLength":1,"maxLength":256}},
            "required":["kind","file_result_call_id"]}
    ]});
    let schema = &mut tool.spec.parameters_schema;
    let mut required = schema["required"].as_array().cloned().unwrap_or_default();
    if multiple(&tool.spec.name) {
        schema["properties"]["sources"] =
            serde_json::json!({"type":"array","minItems":1,"maxItems":8,"items":selector});
        required.push("sources".into());
    } else {
        schema["properties"]["source"] = selector;
        required.push("source".into());
    }
    schema["required"] = required.into();
    tool.spec.description.push_str(" Select each file from a recorded result in the approved conversation: use source (or sources for multi-file reads) with kind=directory_entry, file_result_call_id and exact entry_name for an inspect_files child; use kind=artifact_result with file_result_call_id for a verified create/read/update result. These are recorded result IDs, never paths or device tokens. Do not ask the user to attach files in File Manager.");
}
