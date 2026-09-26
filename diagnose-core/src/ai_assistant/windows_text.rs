//! Windows implementation identity for the shared recoverable text contract.
use super::*;
pub const ADAPTER_ID: &str = "file.text.windows.edge";

pub(super) fn adapter_for_os(os: &str) -> &'static str {
    if os.eq_ignore_ascii_case("windows") {
        ADAPTER_ID
    } else if os.eq_ignore_ascii_case("linux") {
        linux::TEXT_ADAPTER_ID
    } else {
        TEXT_FILE_ADAPTER_ID
    }
}

pub(super) fn adapter(providers: &ProviderRegistry) -> EdgeAdapterDescriptor {
    EdgeAdapterDescriptor {
        adapter_id: ADAPTER_ID.into(),
        adapter_version: TEXT_FILE_ADAPTER_VERSION.into(),
        capability_ids: vec![
            TEXT_FILE_UPDATE_CAPABILITY_ID.into(),
            TEXT_FILE_DELETE_CAPABILITY_ID.into(),
        ],
        limits: providers
            .capability(TEXT_FILE_UPDATE_CAPABILITY_ID)
            .expect("text mutation capability")
            .wire
            .limits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recoverable_text_uses_shared_tools_with_platform_specific_adapters() {
        let providers = ai_assistant_provider_registry();
        let windows = adapter(&providers);
        assert_eq!(windows.adapter_id, ADAPTER_ID);
        assert_eq!(windows.adapter_version, TEXT_FILE_ADAPTER_VERSION);
        for (id, tool) in [
            (TEXT_FILE_UPDATE_CAPABILITY_ID, "update_text_file"),
            (TEXT_FILE_DELETE_CAPABILITY_ID, "delete_text_file"),
        ] {
            let capability = providers.capability(id).unwrap();
            assert_eq!(capability.wire.tool_name, tool);
            assert_eq!(
                capability.adapter_ids,
                vec![
                    TEXT_FILE_ADAPTER_ID.to_owned(),
                    ADAPTER_ID.to_owned(),
                    linux::TEXT_ADAPTER_ID.to_owned()
                ]
            );
            assert_eq!(
                capability.wire.prerequisites.platforms,
                vec![
                    CapabilityPlatform::Windows,
                    CapabilityPlatform::Macos,
                    CapabilityPlatform::Linux
                ]
            );
            assert_eq!(
                capability.wire.execution_policy,
                ExecutionPolicy::DurableRequired
            );
            assert!(
                windows
                    .capability_ids
                    .iter()
                    .any(|candidate| candidate == id)
            );
        }
        assert_eq!(adapter_for_os("windows"), ADAPTER_ID);
        assert_eq!(adapter_for_os("Windows"), ADAPTER_ID);
        assert_eq!(adapter_for_os("macos"), TEXT_FILE_ADAPTER_ID);
    }
}
