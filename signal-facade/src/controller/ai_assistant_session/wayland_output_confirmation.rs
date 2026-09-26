//! Owner-visible exact output action. This projection grants no authority.
use desk_agent_protocol::computer_use::{RawInputScreenContext, RawInputStep};
use desk_diagnose_core::{ai_assistant::linux, chat::ToolCall, dynamic_run::GrantRequestItem};
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WaylandOutputConfirmationDto {
    pub screen: RawInputScreenContext,
    pub step: RawInputStep,
    pub whole_output: bool,
    pub one_shot: bool,
}

pub(super) fn project(item: &GrantRequestItem) -> Option<WaylandOutputConfirmationDto> {
    if item.tool_name != linux::OUTPUT_TOOL
        || item.provider_id != linux::OUTPUT_PROVIDER_ID
        || item.suggested_max_uses != 1
        || item.validate().is_err()
    {
        return None;
    }
    let call = ToolCall {
        id: item.item_id.clone(),
        name: item.tool_name.clone(),
        arguments_json: item.canonical_input_json.clone()?,
    };
    let (target, action) =
        desk_diagnose_core::provider_preflight::wayland_output_input_from_call(&call).ok()?;
    if item.resource_scope
        != desk_diagnose_core::provider_preflight::wayland_output::output_resource_scope(&target)
        || item.operation_scope != ["wayland_output_input:exact_step"]
    {
        return None;
    }
    Some(WaylandOutputConfirmationDto {
        screen: action.screen,
        step: action.step,
        whole_output: true,
        one_shot: true,
    })
}
