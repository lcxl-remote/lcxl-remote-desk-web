//! Platform selection stays outside the shared UI observation flow.
use super::{
    AgentError, CollectedUiTree, ComputerUseAdapterKind, ObservedApplication, ResolvedObject,
    UiInspectParams,
};
use desk_agent_protocol::computer_use::ApplicationState;

#[cfg(windows)]
#[path = "ui_platform/windows.rs"]
mod implementation;
#[cfg(target_os = "macos")]
#[path = "ui_platform/macos.rs"]
mod implementation;
#[cfg(target_os = "linux")]
#[path = "ui_platform/linux.rs"]
mod implementation;
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
#[path = "ui_platform/unsupported.rs"]
mod implementation;

pub(super) use implementation::*;

pub(super) fn collect(
    application: &ObservedApplication,
    params: &UiInspectParams,
    selection: Option<&str>,
) -> Result<
    (
        CollectedUiTree,
        ComputerUseAdapterKind,
        &'static str,
        &'static str,
    ),
    AgentError,
> {
    let (kind, version) = adapter();
    Ok((
        implementation::collect(application, params, selection)?,
        kind,
        version,
        version,
    ))
}
