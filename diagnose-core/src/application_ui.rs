//! Reusable application authority; native ownership is checked again at dispatch.

use desk_agent_protocol::computer_use::{
    ObjectKind, ObjectRef, UiApplicationScope, UiSemanticAction, UiSemanticActionKind,
};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionInput {
    pub application_scope: UiApplicationScope,
}

pub fn invalid() -> AgentError {
    AgentError { kind: AgentErrorKind::InvalidInput, message: "Application UI scope requires an observed application reference and 1–5 unique actions (invoke, select, focus, toggle, set_value). No permission or action was created.".into(), retryable: false, safe_for_model: true, error_code: None }
}

pub fn validate(scope: &UiApplicationScope) -> Result<(), AgentError> {
    let app = &scope.application;
    if app.object_kind != ObjectKind::Application
        || app.token.is_empty()
        || app.snapshot_id.is_empty()
        || chrono::DateTime::parse_from_rfc3339(&app.expires_at).is_err()
        || scope.actions.is_empty()
        || scope.actions.len() > 5
        || scope.actions.iter().enumerate().any(|(i, action)| {
            *action == UiSemanticActionKind::Scroll || scope.actions[..i].contains(action)
        })
    {
        return Err(invalid());
    }
    Ok(())
}

pub fn from_canonical(tool: &str, canonical: Option<&str>) -> Option<UiApplicationScope> {
    if tool != crate::device_assistant::EXECUTE_CONFIRMED_UI_ACTION_TOOL {
        return None;
    }
    let input: PermissionInput = serde_json::from_str(canonical?).ok()?;
    validate(&input.application_scope).ok()?;
    Some(input.application_scope)
}

pub fn resource(application: &ObjectRef) -> Vec<String> {
    vec![format!(
        "ui_application:sha256:{:x}",
        Sha256::digest(serde_json::to_vec(application).expect("reference serialization"))
    )]
}

pub fn operation(action: &UiSemanticAction) -> String {
    operation_kind(match action {
        UiSemanticAction::Invoke => UiSemanticActionKind::Invoke,
        UiSemanticAction::Select => UiSemanticActionKind::Select,
        UiSemanticAction::Focus => UiSemanticActionKind::Focus,
        UiSemanticAction::Toggle { .. } => UiSemanticActionKind::Toggle,
        UiSemanticAction::SetValue { .. } => UiSemanticActionKind::SetValue,
        UiSemanticAction::Scroll { .. } => UiSemanticActionKind::Scroll,
    })
}

pub fn operation_kind(action: UiSemanticActionKind) -> String {
    format!(
        "ui:{}",
        serde_json::to_value(action).unwrap().as_str().unwrap()
    )
}

/// Resolve the application label from prior immutable observations before review.
pub fn bind_request(
    request: &mut crate::dynamic_run::PermissionRequest,
    messages: &[crate::chat::ChatMessage],
) -> Result<(), AgentError> {
    for item in &mut request.items {
        let Some(mut scope) = from_canonical(&item.tool_name, item.canonical_input_json.as_deref())
        else {
            continue;
        };
        let name = messages
            .iter()
            .rev()
            .filter(|m| m.role == crate::chat::ChatRole::Tool)
            .find_map(|message| {
                let output = crate::ui_model_output::deserialize(&message.text).ok()?;
                match output {
                    desk_agent_protocol::OperationOutput::ReadContext(
                        desk_agent_protocol::ReadContextOutput::DesktopUiInspect(ui),
                    ) => ui
                        .nodes
                        .into_iter()
                        .find(|node| node.object_ref == scope.application)
                        .and_then(|node| node.name),
                    desk_agent_protocol::OperationOutput::ReadContext(
                        desk_agent_protocol::ReadContextOutput::DesktopSessionInspect(desktop),
                    ) if desktop.active_application.as_ref() == Some(&scope.application) => {
                        desktop.active_application_name
                    }
                    _ => None,
                }
            })
            .ok_or_else(invalid)?;
        scope.application_name = Some(name);
        let expiry = chrono::DateTime::parse_from_rfc3339(&scope.application.expires_at)
            .map_err(|_| invalid())?;
        let created =
            chrono::DateTime::parse_from_rfc3339(&request.created_at).map_err(|_| invalid())?;
        let remaining = (expiry - created).num_seconds();
        if remaining < 1 {
            return Err(invalid());
        }
        item.suggested_ttl_seconds = item.suggested_ttl_seconds.min(remaining as u32);
        let canonical = crate::permission_tools::canonical_permission_input_json(
            serde_json::to_value(PermissionInput {
                application_scope: scope,
            })
            .unwrap(),
        )
        .map_err(|_| invalid())?;
        item.canonical_input_digest_sha256 =
            Some(format!("{:x}", Sha256::digest(canonical.as_bytes())));
        item.canonical_input_json = Some(canonical);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chat::{ChatMessage, ToolCall},
        permission_tools::*,
    };
    use serde_json::json;

    #[test]
    fn scope_review_uses_observed_application_name_and_bounded_expiry() {
        let app = json!({"token":"calendar","snapshot_id":"apps","object_kind":"application","expires_at":"2026-09-11T03:10:00Z"});
        let call = ToolCall { id:"request".into(),name:REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),arguments_json:json!({"items":[{"item_id":"app","tool_name":"execute_confirmed_ui_action","reason":"Add meeting","suggested_ttl_seconds":900,"suggested_max_uses":8,"application_scope":{"application":app,"application_name":"invented label","actions":["invoke","set_value"]}}]}).to_string() };
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let mut request = build_permission_request(
            &call,
            &registry,
            "request-app".into(),
            1,
            "2026-09-11T03:09:00Z".into(),
        )
        .unwrap();
        assert!(bind_request(&mut request, &[]).is_err());
        let observation = ChatMessage::tool_result("result","inspect",json!({"ReadContext":{"DesktopSessionInspect":{"session":{"token":"session","snapshot_id":"apps","object_kind":"desktop_session","expires_at":"2026-09-11T03:10:00Z"},"os":"macos","interactive_session_incarnation":"session","active_application":app,"active_application_name":"Calendar"}}}).to_string());
        bind_request(&mut request, &[observation]).unwrap();
        assert_eq!(request.items[0].suggested_ttl_seconds, 60);
        let scope = from_canonical(
            &request.items[0].tool_name,
            request.items[0].canonical_input_json.as_deref(),
        )
        .unwrap();
        assert_eq!(scope.application_name.as_deref(), Some("Calendar"));
        request.validate().unwrap();
        let mut duplicated = scope.clone();
        duplicated.actions.push(duplicated.actions[0]);
        assert!(validate(&duplicated).is_err());
        duplicated.actions = vec![UiSemanticActionKind::Scroll];
        assert!(validate(&duplicated).is_err());
    }

    #[test]
    fn exact_approval_cannot_accidentally_use_application_scoped_call_arguments() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let application = json!({"token":"app","snapshot_id":"apps","object_kind":"application","expires_at":"2026-09-11T03:10:00Z"});
        let target = json!({"token":"control","snapshot_id":"ui","object_kind":"ui_element","expires_at":"2026-09-11T03:10:00Z"});
        let call = ToolCall {
            id: "request".into(),
            name: REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
            arguments_json: json!({"items":[{"item_id":"click","tool_name":"execute_confirmed_ui_action","reason":"Click","suggested_ttl_seconds":30,"suggested_max_uses":1,"exact_input":{"application":application,"target":target,"action":{"kind":"invoke"}}}]}).to_string(),
        };
        let error = build_permission_request(
            &call,
            &registry,
            "request".into(),
            1,
            "2026-09-11T03:09:00Z".into(),
        )
        .unwrap_err();
        assert!(error.message.contains("requires application_scope"));
        assert!(error.message.contains("never exact_input"));
        assert!(
            error
                .message
                .contains("no request or approval card was created")
        );
    }

    #[test]
    fn invalid_batch_reports_no_card_and_prerequisite_recovery() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let call = ToolCall { id:"batch".into(),name:REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),arguments_json:json!({"items":[{"item_id":"read","tool_name":"inspect_desktop_ui","reason":"Inspect","suggested_ttl_seconds":300,"suggested_max_uses":8},{"item_id":"click","tool_name":"execute_confirmed_ui_action","reason":"Click unknown target","suggested_ttl_seconds":300,"suggested_max_uses":1}]}).to_string() };
        let error = build_permission_request(
            &call,
            &registry,
            "request".into(),
            1,
            "2026-09-11T03:09:00Z".into(),
        )
        .unwrap_err();
        assert!(error.message.contains("item_id=click"));
        assert!(
            error
                .message
                .contains("no request or approval card was created")
        );
        assert!(
            error
                .message
                .contains("first request only the prerequisite read permission")
        );
        assert!(error.message.contains("object_kind"));
    }
}
