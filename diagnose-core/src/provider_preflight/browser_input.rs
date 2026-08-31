//! Strict model-call decoding for the closed browser action vocabulary.

use super::error;
use crate::chat::ToolCall;
use desk_agent_protocol::browser_control::{
    BROWSER_CONTROL_SCHEMA_VERSION, BrowserAction, BrowserActionRequest, BrowserActivationClass,
    BrowserElementRef, BrowserFormField, BrowserMutationClass, BrowserNavigationTarget,
    BrowserPageRef, BrowserWaitState,
};
use desk_agent_protocol::communication::{GmailWebDraftHandoffInput, SlackWebDraftHandoffInput};
use desk_agent_protocol::{AgentError, AgentErrorKind};

pub fn browser_action_from_call(
    call: &ToolCall,
    server_call_id: &str,
) -> Result<BrowserActionRequest, AgentError> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OpenArgs {
        target: BrowserNavigationTarget,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct NavigateArgs {
        page: BrowserPageRef,
        target: BrowserNavigationTarget,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SnapshotArgs {
        page: BrowserPageRef,
        max_elements: u16,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct WaitArgs {
        page: BrowserPageRef,
        element: BrowserElementRef,
        state: BrowserWaitState,
        timeout_ms: u32,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct FillArgs {
        page: BrowserPageRef,
        fields: Vec<BrowserFormField>,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ActivateArgs {
        page: BrowserPageRef,
        element: BrowserElementRef,
    }
    let decode = |message: &str| {
        error(
            AgentErrorKind::InvalidInput,
            format!("invalid browser Provider input: {message}"),
            false,
            true,
        )
    };
    let action = match call.name.as_str() {
        "browser_open_page" => {
            let args: OpenArgs = serde_json::from_str(&call.arguments_json)
                .map_err(|error| decode(&error.to_string()))?;
            BrowserAction::OpenPage {
                target: args.target,
            }
        }
        "browser_navigate_page" => {
            let args: NavigateArgs = serde_json::from_str(&call.arguments_json)
                .map_err(|error| decode(&error.to_string()))?;
            BrowserAction::NavigatePage {
                page: args.page,
                target: args.target,
            }
        }
        "browser_take_snapshot" => {
            let args: SnapshotArgs = serde_json::from_str(&call.arguments_json)
                .map_err(|error| decode(&error.to_string()))?;
            BrowserAction::TakeSnapshot {
                page: args.page,
                max_elements: args.max_elements,
            }
        }
        "browser_wait_for" => {
            let args: WaitArgs = serde_json::from_str(&call.arguments_json)
                .map_err(|error| decode(&error.to_string()))?;
            BrowserAction::WaitFor {
                page: args.page,
                element: args.element,
                state: args.state,
                timeout_ms: args.timeout_ms,
            }
        }
        "browser_fill_form" => {
            let args: FillArgs = serde_json::from_str(&call.arguments_json)
                .map_err(|error| decode(&error.to_string()))?;
            BrowserAction::FillForm {
                page: args.page,
                fields: args.fields,
                mutation_class: BrowserMutationClass::InputFallback,
            }
        }
        "prepare_slack_web_message_handoff" => {
            let args: SlackWebDraftHandoffInput = serde_json::from_str(&call.arguments_json)
                .map_err(|error| decode(&error.to_string()))?;
            args.validate()
                .map_err(|error| decode(&error.to_string()))?;
            BrowserAction::FillForm {
                page: args.page,
                fields: vec![BrowserFormField {
                    element: args.composer,
                    value: args.body_plain_text,
                }],
                mutation_class: BrowserMutationClass::WriteExternalDraft,
            }
        }
        "prepare_gmail_web_draft_handoff" => {
            let args: GmailWebDraftHandoffInput = serde_json::from_str(&call.arguments_json)
                .map_err(|error| decode(&error.to_string()))?;
            args.validate()
                .map_err(|error| decode(&error.to_string()))?;
            let fields = vec![
                BrowserFormField {
                    element: args.to_field,
                    value: args.draft.recipients[0].address.clone(),
                },
                BrowserFormField {
                    element: args.subject_field,
                    value: args.draft.subject,
                },
                BrowserFormField {
                    element: args.body_field,
                    value: args.draft.body_plain_text,
                },
            ];
            match args.attachment {
                Some(attachment) => BrowserAction::FillFormAndUpload {
                    page: args.page,
                    fields,
                    upload_element: attachment.element,
                    file: attachment.artifact.file,
                    content: attachment.artifact.content,
                    file_name: attachment.artifact.file_name,
                    media_type: attachment.artifact.media_type,
                    size_bytes: attachment.artifact.size_bytes,
                    digest_sha256: attachment.artifact.digest_sha256,
                    mutation_class: BrowserMutationClass::WriteExternalDraft,
                },
                None => BrowserAction::FillForm {
                    page: args.page,
                    fields,
                    mutation_class: BrowserMutationClass::WriteExternalDraft,
                },
            }
        }
        "browser_activate_element" => {
            let args: ActivateArgs = serde_json::from_str(&call.arguments_json)
                .map_err(|error| decode(&error.to_string()))?;
            BrowserAction::ActivateElement {
                page: args.page,
                element: args.element,
                activation_class: BrowserActivationClass::InputFallback,
            }
        }
        _ => {
            return Err(error(
                AgentErrorKind::UnsupportedCapability,
                "browser Provider is not registered",
                false,
                true,
            ));
        }
    };
    let request = BrowserActionRequest {
        schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
        call_id: server_call_id.into(),
        action,
    };
    request.validate().map_err(|validation_error| {
        error(
            AgentErrorKind::InvalidInput,
            format!("invalid browser Provider input: {validation_error}"),
            false,
            true,
        )
    })?;
    Ok(request)
}
