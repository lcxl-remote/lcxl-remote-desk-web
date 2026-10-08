//! Model schema constraints matching existing parser combinations and byte budgets.
use crate::chat::ToolSpec;
use serde_json::{Value, json};

pub(crate) fn project(tool: &mut ToolSpec) {
    let schema = &mut tool.parameters_schema;
    match tool.name.as_str() {
        "request_permissions" => {
            let item = &mut schema["properties"]["items"]["items"];
            item["not"] = json!({"required":["application_scope","exact_input"]});
            item["allOf"] = json!([{
                "if":{"properties":{"tool_name":{"enum":["execute_ui_actions","send_background_input"]}},"required":["tool_name"]},
                "then":{"required":["application_scope"]},
                "else":{"not":{"required":["application_scope"]}}
            }]);
            item["properties"]["reason"]["x-maxUtf8Bytes"] =
                json!(crate::dynamic_run::MAX_PERMISSION_REASON_BYTES);
            item["properties"]["exact_input"]["description"] = json!(
                "Use the current model-facing target tool input, including observed object/result IDs. The server restores reference metadata, hashes and defaults before approval. Native UI permissions use application_scope instead. Obtain prerequisite reads before requesting a mutation that depends on their results."
            );
        }
        "read_current_screen" => schema["not"] = json!({"required":["display","window_id"]}),
        "read_text_file" => {
            schema["dependentRequired"] = json!({"entry_name":["file_result_call_id"]})
        }
        "inspect_files" => {
            schema["allOf"] = json!([
                {"if":{"required":["file_name"]},"then":{"properties":{"paginate":{"const":false}},"not":{"required":["cursor"]}}},
                {"if":{"required":["cursor"]},"then":{"required":["paginate"],"properties":{"paginate":{"const":true}}}},
                {"if":{"required":["paginate"],"properties":{"paginate":{"const":true}}},"then":{"properties":{"max_entries":{"minimum":2}}}}
            ])
        }
        "inspect_desktop_ui" => {
            schema["anyOf"] = json!([
                {"required":["queries"],"properties":{"queries":{"minItems":1}}},
                {"required":["element_id"]},
                {"required":["root_id","element_only"],"properties":{"element_only":{"const":true}}},
                {"required":["allow_unfiltered"],"properties":{"allow_unfiltered":{"const":true}}}
            ]);
            schema["allOf"] = json!([{"if":{"required":["element_only"],"properties":{"element_only":{"const":true}}},"then":{"properties":{"queries":{"maxItems":0}}}}]);
        }
        "list_applications" | "read_process_list" => {
            schema["anyOf"] = json!([
                {"required":["queries"],"properties":{"queries":{"minItems":1}}},
                {"required":["allow_unfiltered"],"properties":{"allow_unfiltered":{"const":true}}}
            ])
        }
        "read_conversation_attachment" => {
            schema["not"] = json!({"required":["queries"],"anyOf":[{"required":["start_line"]},{"required":["end_line"]}]});
            schema["allOf"] = json!([{"if":{"not":{"required":["queries"]}},"then":{"properties":{"ignore_case":{"const":false},"before_context":{"const":0},"after_context":{"const":0}}}}]);
        }
        "convert_document" => {
            schema["properties"]["conversion"]["allOf"] = json!([
                {"if":{"required":["kind"],"properties":{"kind":{"enum":["markdown_to_pdf","text_to_pdf","typst_to_pdf"]}}},"then":{"not":{"anyOf":[{"required":["pages"]},{"required":["page_markers"]}]}}}
            ])
        }
        "launch_application" => {
            schema["allOf"] = json!([{
                "if":{"required":["target"],"properties":{"target":{"required":["kind"],"properties":{"kind":{"enum":["macos_bundle","windows_app_id"]}}}}},
                "then":{"properties":{"cwd":{"type":"null"},"run_as_admin":{"const":false}}}
            }])
        }
        _ => {}
    }
    add_byte_budgets(schema, &tool.name);
}

fn add_byte_budgets(schema: &mut Value, tool: &str) {
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        for (name, field) in properties {
            let limit = match name.as_str() {
                "body_plain_text" | "content_utf8" | "before" | "after" => Some(65536),
                "subject" => Some(998),
                "address" | "display_name" => Some(512),
                "command" => Some(16384),
                "text" if matches!(tool, "send_raw_input" | "execute_wayland_output_input") => {
                    Some(512)
                }
                "text" | "value"
                    if matches!(
                        tool,
                        "execute_ui_actions"
                            | "send_background_input"
                            | "patch_live_spreadsheet_cell"
                            | "patch_numbers_copy"
                            | "patch_excel_copy"
                    ) =>
                {
                    Some(16384)
                }
                "formula"
                    if matches!(
                        tool,
                        "patch_live_spreadsheet_cell" | "patch_numbers_copy" | "patch_excel_copy"
                    ) =>
                {
                    Some(4096)
                }
                "text"
                    if matches!(
                        tool,
                        "replace_live_document_body"
                            | "replace_pages_copy_body"
                            | "replace_word_copy_body"
                            | "patch_live_presentation_slide"
                            | "patch_keynote_copy"
                            | "patch_powerpoint_copy"
                    ) =>
                {
                    Some(65536)
                }
                "file_name" | "output_name" => Some(200),
                "native_file_name" => Some(255),
                _ => None,
            };
            if let Some(limit) = limit
                && field.is_object()
            {
                field["x-maxUtf8Bytes"] = json!(limit);
                let description = field["description"].as_str().unwrap_or("");
                if !description.contains("UTF-8 bytes") {
                    field["description"] = json!(format!(
                        "{description} Maximum {limit} UTF-8 bytes (independent of Unicode character count)."
                    ));
                }
            }
        }
    }
    match schema {
        Value::Object(object) => {
            for child in object.values_mut() {
                add_byte_budgets(child, tool);
            }
        }
        Value::Array(array) => {
            for child in array {
                add_byte_budgets(child, tool);
            }
        }
        _ => {}
    }
}
