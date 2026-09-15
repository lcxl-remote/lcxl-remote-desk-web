use super::*;
use crate::worker::agent::macos_accessibility_observer as native;

pub(super) fn collect(
    application: &ObservedApplication,
    params: &UiInspectParams,
    selection: Option<&str>,
) -> Result<CollectedUiTree, AgentError> {
    native::collect_application_selection(
        application.process_id,
        &application.image_path,
        params.max_depth,
        params.max_nodes,
        params.max_bytes,
        params.scope,
        selection,
        params.query.as_ref(),
        params.element_only,
    )
}

pub(crate) fn is_window(node: &super::super::CollectedUiNode) -> bool {
    node.role == "AXWindow" || node.role.starts_with("AXWindow/")
}

pub(crate) fn adapter() -> (ComputerUseAdapterKind, &'static str) {
    (
        ComputerUseAdapterKind::MacosAccessibility,
        "macos-accessibility-read/v1",
    )
}

pub(crate) fn running_applications() -> Result<(Vec<ObservedApplication>, bool), AgentError> {
    native::running_applications().map(|apps| (apps, false))
}

pub(crate) fn application_display_metadata(pid: u32) -> Option<(String, ApplicationState)> {
    native::application_display_metadata(pid)
}

pub(crate) fn selected_application(
    target: Option<&ResolvedObject>,
) -> Result<Option<ObservedApplication>, AgentError> {
    match target {
        Some(ResolvedObject::Application { process_id, .. })
        | Some(ResolvedObject::Window { process_id, .. })
        | Some(ResolvedObject::UiElement { process_id, .. }) => {
            native::application_by_pid(*process_id).map(Some)
        }
        _ => Ok(None),
    }
}

pub(crate) fn validate_lifetime(application: &ObservedApplication) -> Result<(), AgentError> {
    if native::application_by_pid(application.process_id)?.process_started_at
        != application.process_started_at
    {
        return Err(super::super::error(
            desk_agent_protocol::AgentErrorKind::SessionUnavailable,
            "the selected application restarted during inspection",
            true,
        ));
    }
    Ok(())
}

pub(crate) fn window_capture_target(
    pid: u32,
    path: &str,
    fingerprint: &str,
) -> Result<crate::worker::agent::collectors::screen_capture::WindowCaptureTarget, AgentError> {
    native::resolve_window_capture_target(pid, path, fingerprint)
}
