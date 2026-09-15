use super::*;
use crate::worker::agent::windows_uia_observer as native;

pub(super) fn collect(
    application: &ObservedApplication,
    params: &UiInspectParams,
    selection: Option<&str>,
) -> Result<CollectedUiTree, AgentError> {
    native::collect_application_selection(application, params, selection)
}

pub(crate) fn is_window(node: &super::super::CollectedUiNode) -> bool {
    node.window_fingerprint.as_deref() == Some(node.fingerprint.as_str())
}

pub(crate) fn adapter() -> (ComputerUseAdapterKind, &'static str) {
    (ComputerUseAdapterKind::WindowsUia, "a4-windows-uia-read/v1")
}

pub(crate) fn running_applications() -> Result<(Vec<ObservedApplication>, bool), AgentError> {
    native::running_applications().map(|(apps, truncated)| {
        let mut identities = std::collections::HashSet::new();
        let applications = apps
            .into_iter()
            .filter_map(|app| {
                if !identities.insert((app.process_id, app.process_started_at)) {
                    return None;
                }
                let mut application = project(app);
                application.window_handle = 0;
                Some(application)
            })
            .collect();
        (applications, truncated)
    })
}

pub(crate) fn application_display_metadata(_: u32) -> Option<(String, ApplicationState)> {
    None
}

pub(crate) fn selected_application(
    target: Option<&ResolvedObject>,
) -> Result<Option<ObservedApplication>, AgentError> {
    let application = match target {
        Some(ResolvedObject::Application {
            window_handle,
            process_id,
            ..
        }) => {
            if *window_handle == 0 {
                native::application_by_process(*process_id)?
            } else {
                native::application_by_window(*window_handle)?
            }
        }
        Some(ResolvedObject::Window {
            process_id,
            image_path,
            fingerprint,
        })
        | Some(ResolvedObject::UiElement {
            process_id,
            image_path,
            fingerprint,
        }) => native::application_for_element(*process_id, image_path, fingerprint)?,
        _ => return Ok(None),
    };
    Ok(Some(project(application)))
}

pub(crate) fn validate_lifetime(application: &ObservedApplication) -> Result<(), AgentError> {
    if native::process_start(application.process_id) != application.process_started_at {
        return Err(super::super::error(
            desk_agent_protocol::AgentErrorKind::SessionUnavailable,
            "the selected application restarted during inspection",
            false,
        ));
    }
    Ok(())
}

fn project(app: native::WindowsForegroundApplication) -> ObservedApplication {
    ObservedApplication {
        window_handle: app.window_handle,
        process_id: app.process_id,
        image_path: app.image_path,
        process_started_at: Some(app.process_started_at),
    }
}

pub(crate) fn window_capture_target(
    pid: u32,
    path: &str,
    fingerprint: &str,
) -> Result<crate::worker::agent::collectors::screen_capture::WindowCaptureTarget, AgentError> {
    native::resolve_window_capture_target(pid, path, fingerprint)
}
