use super::*;
use crate::worker::agent::linux_desktop::atspi as native;

pub(super) fn collect(
    application: &ObservedApplication,
    params: &UiInspectParams,
    selection: Option<&str>,
) -> Result<CollectedUiTree, AgentError> {
    native::collect(
        application.process_id,
        application.image_path.clone(),
        application.process_started_at,
        params.clone(),
        selection.map(str::to_owned),
    )
}

pub(crate) fn is_window(node: &super::super::CollectedUiNode) -> bool {
    matches!(node.role.as_str(), "frame" | "dialog" | "window")
}

pub(crate) fn adapter() -> (ComputerUseAdapterKind, &'static str) {
    (
        ComputerUseAdapterKind::LinuxAtspi,
        desk_diagnose_core::ai_assistant::LINUX_ATSPI_ADAPTER_VERSION,
    )
}

pub(crate) fn running_applications() -> Result<(Vec<ObservedApplication>, bool), AgentError> {
    native::running_applications().map(|apps| (apps, false))
}

pub(crate) fn application_display_metadata(pid: u32) -> Option<(String, ApplicationState)> {
    let _ = pid;
    None
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
    let _ = (pid, path, fingerprint);
    Err(super::super::error(
        desk_agent_protocol::AgentErrorKind::UnsupportedCapability,
        "Wayland independent window capture is unavailable",
        false,
    ))
}
