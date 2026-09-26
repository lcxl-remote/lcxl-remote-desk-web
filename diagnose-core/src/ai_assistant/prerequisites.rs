//! Device connectivity and a graphical desktop are independent prerequisites.
use super::*;

pub(super) fn requires_desktop(locality: ExecutionLocality, adapters: &[String]) -> bool {
    if locality == ExecutionLocality::Central {
        return false;
    }
    // Unknown adapters retain the desktop requirement until their contract is
    // explicitly classified. A mixed provider retains the stronger prerequisite.
    adapters.is_empty() || adapters.iter().any(|adapter| !headless(adapter))
}

fn headless(adapter: &str) -> bool {
    matches!(
        adapter,
        FILE_WORKSPACE_ADAPTER_ID
            | SPREADSHEET_FILE_ADAPTER_ID
            | FILE_ARTIFACT_ADAPTER_ID
            | TEXT_FILE_ADAPTER_ID
            | windows_text::ADAPTER_ID
            | linux::TEXT_ADAPTER_ID
            | SYSTEM_DIAGNOSTICS_ADAPTER_ID
            | SYSTEM_COMMAND_ADAPTER_ID
            | TERMINAL_OUTPUT_ADAPTER_ID
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_capabilities_still_require_the_device_but_not_a_graphical_desktop() {
        let registry = ai_assistant_provider_registry();
        for id in [
            FILE_METADATA_CAPABILITY_ID,
            FILE_CONTENT_CAPABILITY_ID,
            SPREADSHEET_FILE_CAPABILITY_ID,
            SPREADSHEET_MERGE_CAPABILITY_ID,
            FILE_ARTIFACT_CREATE_CAPABILITY_ID,
            TEXT_FILE_UPDATE_CAPABILITY_ID,
            TEXT_FILE_DELETE_CAPABILITY_ID,
            TERMINAL_OUTPUT_CAPABILITY_ID,
            SYSTEM_INFO_CAPABILITY_ID,
            SYSTEM_COMMAND_CAPABILITY_ID,
        ] {
            let prerequisites = &registry.capability(id).unwrap().wire.prerequisites;
            assert!(prerequisites.requires_edge_connection, "{id}");
            assert!(!prerequisites.requires_interactive_session, "{id}");
        }
        for id in [
            DESKTOP_SESSION_CAPABILITY_ID,
            DESKTOP_UI_CAPABILITY_ID,
            CURRENT_SCREEN_CAPABILITY_ID,
            BROWSER_OPEN_CAPABILITY_ID,
            linux::OUTPUT_CAPABILITY_ID,
        ] {
            assert!(
                registry
                    .capability(id)
                    .unwrap()
                    .wire
                    .prerequisites
                    .requires_interactive_session,
                "{id}"
            );
        }
    }

    #[test]
    fn unknown_or_mixed_adapters_cannot_inherit_headless_eligibility() {
        assert!(requires_desktop(ExecutionLocality::Edge, &[]));
        assert!(requires_desktop(
            ExecutionLocality::Edge,
            &["unknown".into()]
        ));
        assert!(requires_desktop(
            ExecutionLocality::Edge,
            &[
                FILE_WORKSPACE_ADAPTER_ID.into(),
                CURRENT_SCREEN_ADAPTER_ID.into()
            ]
        ));
        assert!(!requires_desktop(
            ExecutionLocality::Edge,
            &[
                TEXT_FILE_ADAPTER_ID.into(),
                windows_text::ADAPTER_ID.into(),
                linux::TEXT_ADAPTER_ID.into()
            ]
        ));
    }
}
