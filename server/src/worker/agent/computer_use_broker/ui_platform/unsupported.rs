use super::*;

pub(super) fn collect(
    _: &ObservedApplication,
    _: &UiInspectParams,
    _: Option<&str>,
) -> Result<CollectedUiTree, AgentError> {
    Err(super::super::error(
        desk_agent_protocol::AgentErrorKind::UnsupportedPlatform,
        "semantic desktop UI inspection is unavailable on this platform",
        false,
    ))
}

pub(crate) fn is_window(_: &super::super::CollectedUiNode) -> bool {
    false
}

pub(crate) fn adapter() -> (ComputerUseAdapterKind, &'static str) {
    (ComputerUseAdapterKind::WindowsUia, "unavailable")
}
pub(crate) fn running_applications() -> Result<(Vec<ObservedApplication>, bool), AgentError> {
    Err(super::super::error(
        desk_agent_protocol::AgentErrorKind::UnsupportedPlatform,
        "application inspection is unavailable on this platform",
        false,
    ))
}
pub(crate) fn application_display_metadata(_: u32) -> Option<(String, ApplicationState)> {
    None
}
pub(crate) fn selected_application(
    _: Option<&ResolvedObject>,
) -> Result<Option<ObservedApplication>, AgentError> {
    Ok(None)
}
pub(crate) fn validate_lifetime(_: &ObservedApplication) -> Result<(), AgentError> {
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
        "independent window capture is unavailable on this platform",
        false,
    ))
}
