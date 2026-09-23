//! Shared admission policy for the AI Assistant observation transport.
//! This is not a grant and does not authorize direct device access.
use desk_agent_protocol::{AgentError, AgentErrorKind, Capability};

/// All capabilities must explicitly opt into or out of this transport.
/// Do not add a wildcard: new enum variants must fail compilation here until
/// their routing is reviewed. Dedicated read providers are not generic reads.
pub fn require_device_observation(capability: Capability) -> Result<(), AgentError> {
    match capability {
        Capability::DesktopSessionInspect
        | Capability::ApplicationList
        | Capability::DesktopUiInspect
        | Capability::OfficeDocumentInspect
        | Capability::FileMetadataRead
        | Capability::FileContentRead
        | Capability::SpreadsheetFileInspect
        | Capability::SpreadsheetMergePreview
        | Capability::TerminalOutputRead
        | Capability::ScreenCaptureCurrent
        | Capability::SystemInfo
        | Capability::ProcessList
        | Capability::NetworkPorts
        | Capability::ServiceStatus
        | Capability::LogRecent
        | Capability::ContainerList
        | Capability::SpreadsheetLiveInspect
        | Capability::DocumentLiveInspect
        | Capability::PresentationLiveInspect => Ok(()),
        Capability::DocumentPreview => Ok(()),
        // These capabilities are unsupported here or use a dedicated path,
        // even when their operation is itself read-only.
        Capability::ContainerInspect
        | Capability::ContainerLogs
        | Capability::SpreadsheetWorkbookCreateConfirmed
        | Capability::SpreadsheetFormulaWorkbookCreateConfirmed
        | Capability::WordDocumentCreateConfirmed
        | Capability::WebResearchFetch
        | Capability::WebResearchSearch
        | Capability::AssistantActionPreview
        | Capability::ShellExecReadonly
        | Capability::ShellExecConfirmed
        | Capability::DesktopUiActionConfirmed
        | Capability::DesktopBackgroundInputConfirmed
        | Capability::DesktopInputFallbackConfirmed
        | Capability::OfficeExcelPatchConfirmed
        | Capability::OfficePowerPointPatchConfirmed
        | Capability::SpreadsheetLivePatchConfirmed
        | Capability::DocumentLivePatchConfirmed
        | Capability::PresentationLivePatchConfirmed
        | Capability::FilePatchConfirmed
        | Capability::FileCopyConfirmed
        | Capability::FileArtifactCreateConfirmed
        | Capability::CommunicationLocalDraftCreateConfirmed
        | Capability::CommunicationOutlookNewHandoffConfirmed
        | Capability::BrowserPageObserve
        | Capability::BrowserPageNavigateConfirmed
        | Capability::BrowserInputFallbackConfirmed
        | Capability::BrowserExternalDraftWriteConfirmed
        | Capability::BrowserExternalSendConfirmed
        | Capability::FileDeleteConfirmed
        | Capability::ApplicationLaunchConfirmed
        | Capability::DocumentConvertConfirmed => Err(AgentError {
            kind: AgentErrorKind::UnsupportedCapability,
            message: "AI Assistant may only invoke selected read-only observations".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_observations_do_not_admit_dedicated_execution_paths() {
        require_device_observation(Capability::ApplicationList).unwrap();
        require_device_observation(Capability::DesktopSessionInspect).unwrap();
        // Being read-only does not imply membership in the generic transport.
        for capability in [
            Capability::BrowserPageObserve,
            Capability::WebResearchFetch,
            Capability::ShellExecReadonly,
            Capability::ApplicationLaunchConfirmed,
            Capability::FileDeleteConfirmed,
        ] {
            assert_eq!(
                require_device_observation(capability).unwrap_err().kind,
                AgentErrorKind::UnsupportedCapability,
            );
        }
    }
}
