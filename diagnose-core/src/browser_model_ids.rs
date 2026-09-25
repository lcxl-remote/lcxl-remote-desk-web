//! Model-facing browser IDs and derived fixed inputs. Durable results retain the
//! complete edge-issued references used for authorization and execution.

use crate::chat::{ChatMessage, ChatRole, ToolCall, ToolSpec};
use desk_agent_protocol::browser_control::{
    BrowserActionResult, BrowserElementRef, BrowserOrigin, BrowserOriginKind, BrowserPageRef,
};
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

pub(crate) fn supports(tool: &str) -> bool {
    matches!(
        tool,
        "browser_open_page"
            | "browser_navigate_page"
            | "browser_take_snapshot"
            | "browser_wait_for"
            | "browser_fill_form"
            | "browser_activate_element"
            | "prepare_gmail_draft"
            | "prepare_slack_message"
            | "send_gmail_message"
            | "send_slack_message"
            | "create_formula_workbook"
    )
}

fn browser_tool(tool: &str) -> bool {
    supports(tool) && tool != "create_formula_workbook" && tool != "browser_open_page"
}

fn element_fields(tool: &str) -> &'static [&'static str] {
    match tool {
        "browser_wait_for" | "browser_activate_element" => &["element"],
        "prepare_slack_message" => &["composer"],
        "prepare_gmail_draft" => &["to_field", "subject_field", "body_field"],
        "send_slack_message" => &["composer", "send_control"],
        "send_gmail_message" => &["to_field", "subject_field", "body_field", "send_control"],
        _ => &[],
    }
}

fn observed_id(value: &Value, field: &str) -> Result<String, AgentError> {
    let id = value
        .as_str()
        .ok_or_else(|| invalid(format!("{field} must be an observed ID string")))?;
    if id.is_empty() || id.len() > 256 {
        return Err(invalid(format!(
            "{field} must be an observed ID of at most 256 bytes"
        )));
    }
    Ok(id.to_owned())
}

fn origin_for_url(url: &str) -> Result<BrowserOrigin, AgentError> {
    let parsed = url::Url::parse(url).map_err(|_| invalid("invalid browser navigation URL"))?;
    let kind = match parsed.scheme() {
        "https" => BrowserOriginKind::Https,
        "http" if matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")) => {
            BrowserOriginKind::HttpLoopback
        }
        "http" => BrowserOriginKind::Http,
        _ => return Err(invalid("invalid browser navigation URL scheme")),
    };
    let origin = BrowserOrigin {
        kind,
        host_ascii: parsed
            .host_str()
            .ok_or_else(|| invalid("browser navigation URL must have a host"))?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase(),
        port: parsed
            .port_or_known_default()
            .ok_or_else(|| invalid("browser navigation URL must have a port"))?,
    };
    origin
        .validate()
        .map_err(|error| invalid(error.to_string()))?;
    // Keep the protocol's complete URL and origin validation authoritative.
    desk_agent_protocol::browser_control::BrowserNavigationTarget {
        url: url.to_owned(),
        origin: origin.clone(),
    }
    .validate()
    .map_err(|error| invalid(error.to_string()))?;
    Ok(origin)
}

fn remove_element_id(
    object: &mut serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<String>, AgentError> {
    let model = format!("{field}_id");
    if object.contains_key(field) && object.contains_key(&model) {
        return Err(invalid(format!("use either {model} or {field}, not both")));
    }
    object
        .remove(&model)
        .map(|value| observed_id(&value, &model))
        .transpose()
}

fn observed_browser_results(history: &[ChatMessage], now_ms: u64) -> Vec<BrowserActionResult> {
    history
        .iter()
        .rev()
        .filter(|message| matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput))
        .filter(|message| {
            message.data_envelope.as_ref().is_some_and(|envelope| {
                envelope
                    .retention
                    .expires_at_unix_ms
                    .is_none_or(|expiry| expiry > now_ms)
            })
        })
        .filter_map(crate::agent_loop::verified_browser_result)
        .collect()
}

pub(crate) fn resolve(
    call: &ToolCall,
    value: &mut Value,
    history: &[ChatMessage],
    now_ms: u64,
) -> Result<(), AgentError> {
    if !supports(&call.name) {
        return Ok(());
    }
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid("tool arguments must be an object"))?;
    if call.name == "create_formula_workbook" {
        match object.get("locale") {
            Some(locale) if locale != "en-US-a1" => return Err(invalid("locale must be en-US-a1")),
            Some(_) => {}
            None => {
                object.insert("locale".into(), json!("en-US-a1"));
            }
        }
        return Ok(());
    }
    if matches!(
        call.name.as_str(),
        "browser_open_page" | "browser_navigate_page"
    ) {
        let target = object
            .get_mut("target")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| invalid("browser target is required"))?;
        let url = target
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("browser target.url is required"))?;
        let derived = serde_json::to_value(origin_for_url(url)?).unwrap();
        match target.get("origin") {
            Some(origin) if origin != &derived => {
                return Err(invalid("browser target.origin does not match target.url"));
            }
            Some(_) => {}
            None => {
                target.insert("origin".into(), derived);
            }
        }
    }
    if call.name == "browser_wait_for" {
        match object.get("state") {
            Some(state) if state != "present" => {
                return Err(invalid("browser wait state must be present"));
            }
            Some(_) => {}
            None => {
                object.insert("state".into(), json!("present"));
            }
        }
    }
    if !browser_tool(&call.name) {
        return Ok(());
    }
    if object.contains_key("page") && object.contains_key("page_id") {
        return Err(invalid("use page_id or page, not both"));
    }
    let Some(page_id) = object.remove("page_id") else {
        return Ok(()); // Historical complete references remain readable.
    };
    let page_id = observed_id(&page_id, "page_id")?;
    let mut requested = Vec::new();
    for field in element_fields(&call.name) {
        if let Some(id) = remove_element_id(object, field)? {
            requested.push(((*field).to_owned(), id));
        }
    }
    if call.name == "browser_fill_form" {
        let fields = object
            .get_mut("fields")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| invalid("browser fields must be an array"))?;
        for (index, field) in fields.iter_mut().enumerate() {
            let item = field
                .as_object_mut()
                .ok_or_else(|| invalid("browser field must be an object"))?;
            if let Some(id) = remove_element_id(item, "element")? {
                requested.push((format!("fields[{index}].element"), id));
            }
        }
    }
    if call.name == "prepare_gmail_draft" {
        if let Some(Some(attachment)) = object.get_mut("attachment").map(Value::as_object_mut) {
            if let Some(id) = remove_element_id(attachment, "element")? {
                requested.push(("attachment.element".into(), id));
            }
        }
    }
    let results = observed_browser_results(history, now_ms);
    let latest = results
        .iter()
        .find(|result| result.page.page_id == page_id)
        .ok_or_else(|| invalid("page_id was not observed in a current verified browser result"))?;
    if results
        .iter()
        .any(|result| result.page.page_id == page_id && result.page.adapter != latest.page.adapter)
    {
        return Err(invalid(
            "page_id is ambiguous across browser profiles; refresh the page",
        ));
    }
    let result = if requested.is_empty() {
        latest
    } else {
        results.iter().find(|result| result.page == latest.page
            && result.snapshot.as_ref().is_some_and(|snapshot| requested.iter().all(|(_, id)| {
                snapshot.elements.iter().any(|element| element.element_id == *id)
            })))
            .ok_or_else(|| invalid("element_id was not observed with this current page; take a fresh browser snapshot"))?
    };
    object.insert("page".into(), serde_json::to_value(&result.page).unwrap());
    for (field, id) in requested {
        let element = result
            .snapshot
            .as_ref()
            .unwrap()
            .elements
            .iter()
            .find(|element| element.element_id == id)
            .unwrap();
        let encoded = serde_json::to_value(element).unwrap();
        if let Some(index) = field
            .strip_prefix("fields[")
            .and_then(|rest| rest.strip_suffix("].element"))
        {
            let index: usize = index.parse().unwrap();
            object["fields"].as_array_mut().unwrap()[index]["element"] = encoded;
        } else if field == "attachment.element" {
            object["attachment"]["element"] = encoded;
        } else {
            object.insert(field, encoded);
        }
    }
    Ok(())
}

fn replace_reference(object: &mut serde_json::Map<String, Value>, field: &str, id_key: &str) {
    if let Some(reference) = object.remove(field) {
        object.insert(
            id_key.into(),
            reference.get(id_key).cloned().unwrap_or(Value::Null),
        );
    }
}

pub(crate) fn project_arguments(tool: &str, value: &mut Value) {
    if !supports(tool) {
        return;
    }
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if tool == "create_formula_workbook" {
        object.remove("locale");
        return;
    }
    if matches!(tool, "browser_open_page" | "browser_navigate_page") {
        if let Some(target) = object.get_mut("target").and_then(Value::as_object_mut) {
            target.remove("origin");
        }
    }
    if tool == "browser_wait_for" {
        object.remove("state");
    }
    if !browser_tool(tool) {
        return;
    }
    replace_reference(object, "page", "page_id");
    for field in element_fields(tool) {
        replace_reference(object, field, &format!("{field}_id"));
    }
    if tool == "browser_fill_form" {
        if let Some(fields) = object.get_mut("fields").and_then(Value::as_array_mut) {
            for field in fields {
                if let Some(item) = field.as_object_mut() {
                    replace_reference(item, "element", "element_id");
                }
            }
        }
    }
    if tool == "prepare_gmail_draft" {
        if let Some(attachment) = object.get_mut("attachment").and_then(Value::as_object_mut) {
            replace_reference(attachment, "element", "element_id");
        }
    }
}

fn replace_schema_reference(object: &mut Value, field: &str, id_key: &str) {
    let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) else {
        return;
    };
    if properties.remove(field).is_none() {
        return;
    }
    properties.insert(id_key.into(), json!({"type":"string","minLength":1,"maxLength":256,
        "description":"Copy an ID from a current verified browser observation; the server restores and validates the full reference."}));
    if let Some(required) = object.get_mut("required").and_then(Value::as_array_mut) {
        for item in required {
            if item == field {
                *item = json!(id_key);
            }
        }
    }
}

pub(crate) fn project_tool(tool: &mut ToolSpec) {
    if !supports(&tool.name) {
        return;
    }
    let schema = &mut tool.parameters_schema;
    if tool.name == "create_formula_workbook" {
        schema["properties"]
            .as_object_mut()
            .unwrap()
            .remove("locale");
        schema["required"]
            .as_array_mut()
            .unwrap()
            .retain(|item| item != "locale");
        return;
    }
    if matches!(
        tool.name.as_str(),
        "browser_open_page" | "browser_navigate_page"
    ) {
        if let Some(target) = schema.pointer_mut("/properties/target") {
            target["properties"]
                .as_object_mut()
                .unwrap()
                .remove("origin");
            target["required"]
                .as_array_mut()
                .unwrap()
                .retain(|item| item != "origin");
        }
    }
    if tool.name == "browser_wait_for" {
        schema["properties"]
            .as_object_mut()
            .unwrap()
            .remove("state");
        schema["required"]
            .as_array_mut()
            .unwrap()
            .retain(|item| item != "state");
    }
    if !browser_tool(&tool.name) {
        return;
    }
    replace_schema_reference(schema, "page", "page_id");
    for field in element_fields(&tool.name) {
        replace_schema_reference(schema, field, &format!("{field}_id"));
    }
    if tool.name == "browser_fill_form" {
        if let Some(field) = schema.pointer_mut("/properties/fields/items") {
            replace_schema_reference(field, "element", "element_id");
        }
    }
    if tool.name == "prepare_gmail_draft" {
        if let Some(attachment) = schema.pointer_mut("/properties/attachment") {
            replace_schema_reference(attachment, "element", "element_id");
        }
    }
}

pub(crate) fn page_projection(page: &BrowserPageRef) -> Value {
    json!({"page_id":page.page_id,"origin":page.origin,"account_id":page.account_id})
}

pub(crate) fn element_projection(element: &BrowserElementRef) -> Value {
    json!({"element_id":element.element_id,"role":element.role,
        "accessible_name":element.accessible_name,"value":element.value})
}

pub(crate) fn project_result_message(message: &mut ChatMessage) {
    if crate::agent_loop::verified_browser_result(message).is_none() {
        return;
    }
    let Ok(mut value) = serde_json::from_str::<Value>(&message.text) else {
        return;
    };
    let result = if value.pointer("/output/kind") == Some(&json!("browser")) {
        &mut value["output"]["value"]
    } else {
        &mut value
    };
    if let Some(page) = result.get_mut("page")
        && let Ok(reference) = serde_json::from_value::<BrowserPageRef>(page.clone())
    {
        *page = page_projection(&reference);
    }
    if let Some(snapshot) = result.get_mut("snapshot").and_then(Value::as_object_mut) {
        if let Some(page) = snapshot.get_mut("page")
            && let Ok(reference) = serde_json::from_value::<BrowserPageRef>(page.clone())
        {
            *page = page_projection(&reference);
        }
        if let Some(elements) = snapshot.get_mut("elements").and_then(Value::as_array_mut) {
            for element in elements {
                if let Ok(reference) = serde_json::from_value::<BrowserElementRef>(element.clone())
                {
                    *element = element_projection(&reference);
                }
            }
        }
    }
    message.text = value.to_string();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, value: Value) -> ToolCall {
        ToolCall {
            id: "model-call".into(),
            name: name.into(),
            arguments_json: value.to_string(),
        }
    }

    #[test]
    fn fixed_inputs_are_derived_before_exact_authorization() {
        let open = call(
            "browser_open_page",
            json!({"target":{"url":"https://mail.google.com/mail/u/0/"}}),
        );
        let resolved = crate::ui_model_ids::resolve_call(&open, &[], 1).unwrap();
        let value: Value = serde_json::from_str(&resolved.arguments_json).unwrap();
        assert_eq!(
            value["target"]["origin"],
            json!({"kind":"https","host_ascii":"mail.google.com","port":443})
        );
        assert!(crate::ui_model_ids::same_call_input(
            &open.name,
            &open.arguments_json,
            &resolved.arguments_json
        ));
        let forged = call(
            "browser_open_page",
            json!({"target":{"url":"https://mail.google.com/",
            "origin":{"kind":"https","host_ascii":"app.slack.com","port":443}}}),
        );
        assert!(crate::ui_model_ids::resolve_call(&forged, &[], 1).is_err());
        let formula = call(
            "create_formula_workbook",
            json!({"preview_id":"p","file_name":"r.xlsx",
            "target_cell":"Merged!D2","formula":"=A2+B2"}),
        );
        let resolved = crate::ui_model_ids::resolve_call(&formula, &[], 1).unwrap();
        let value: Value = serde_json::from_str(&resolved.arguments_json).unwrap();
        assert_eq!(value["locale"], "en-US-a1");
        assert!(crate::ui_model_ids::same_call_input(
            &formula.name,
            &formula.arguments_json,
            &resolved.arguments_json
        ));
        let wrong = call("create_formula_workbook", json!({"locale":"fr-FR"}));
        assert!(crate::ui_model_ids::resolve_call(&wrong, &[], 1).is_err());
        let permission = call(
            "request_permissions",
            json!({"items":[{"tool_name":"browser_open_page",
            "exact_input":{"target":{"url":"https://mail.google.com/"}}}]}),
        );
        let resolved = crate::ui_model_ids::resolve_call(&permission, &[], 1).unwrap();
        let value: Value = serde_json::from_str(&resolved.arguments_json).unwrap();
        assert_eq!(
            value["items"][0]["exact_input"]["target"]["origin"]["host_ascii"],
            "mail.google.com"
        );
    }

    #[test]
    fn model_schemas_use_short_ids_and_omit_derived_fields() {
        let registry = crate::ai_assistant::ai_assistant_provider_registry();
        for name in [
            "browser_navigate_page",
            "browser_take_snapshot",
            "browser_wait_for",
            "browser_fill_form",
            "browser_activate_element",
            "prepare_gmail_draft",
            "prepare_slack_message",
            "send_gmail_message",
            "send_slack_message",
        ] {
            let mut spec = registry
                .capability_for_tool(name)
                .unwrap()
                .registered_tool()
                .spec;
            crate::ui_model_ids::project_tool(&mut spec);
            assert!(
                spec.parameters_schema
                    .pointer("/properties/page_id")
                    .is_some(),
                "{name}"
            );
            assert!(
                spec.parameters_schema.pointer("/properties/page").is_none(),
                "{name}"
            );
        }
        let mut wait = registry
            .capability_for_tool("browser_wait_for")
            .unwrap()
            .registered_tool()
            .spec;
        crate::ui_model_ids::project_tool(&mut wait);
        assert!(
            wait.parameters_schema
                .pointer("/properties/element_id")
                .is_some()
        );
        assert!(
            wait.parameters_schema
                .pointer("/properties/state")
                .is_none()
        );
        let mut open = registry
            .capability_for_tool("browser_open_page")
            .unwrap()
            .registered_tool()
            .spec;
        crate::ui_model_ids::project_tool(&mut open);
        assert!(
            open.parameters_schema
                .pointer("/properties/target/properties/origin")
                .is_none()
        );
        let mut formula = registry
            .capability_for_tool("create_formula_workbook")
            .unwrap()
            .registered_tool()
            .spec;
        crate::ui_model_ids::project_tool(&mut formula);
        assert!(
            formula
                .parameters_schema
                .pointer("/properties/locale")
                .is_none()
        );
    }
}
