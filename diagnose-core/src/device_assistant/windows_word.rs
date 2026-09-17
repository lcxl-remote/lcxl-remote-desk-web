//! Windows DOCX batch capabilities; no interactive Word or Live session.
use super::*;
pub const PROVIDER_ID: &str = "office.docx.batch";
pub const INSPECT_CAPABILITY_ID: &str = "office.docx.batch.inspect";
pub const PATCH_CAPABILITY_ID: &str = "office.docx.batch.patch";
pub const ADAPTER_ID: &str = "windows.office.docx.batch";
pub const INSPECT_TOOL: &str = "inspect_word_file";
pub const PATCH_TOOL: &str = "replace_word_copy_body";
pub(super) const READINESS_IDENTITIES: [(&str, &str, &str, &str); 1] = [(
    PROVIDER_ID,
    INSPECT_CAPABILITY_ID,
    ADAPTER_ID,
    desk_agent_protocol::computer_use::office_batch::DOCX_ADAPTER_VERSION,
)];
pub(super) const PATCH_READINESS_IDENTITIES: [(&str, &str, &str, &str); 1] = [(
    PROVIDER_ID,
    PATCH_CAPABILITY_ID,
    ADAPTER_ID,
    desk_agent_protocol::computer_use::office_batch::DOCX_ADAPTER_VERSION,
)];

pub(super) fn inspect_tool() -> RegisteredTool {
    batch_inspect_tool(
        INSPECT_TOOL,
        "Read the bounded stored body text of exactly one owner-selected DOCX file. No Word application or Live session is opened. Fields are not evaluated. The model cannot nominate a path, source token, or interactive target.",
        Capability::DocumentLiveInspect,
    )
}
pub(super) fn patch_tool() -> RegisteredTool {
    let mut tool = document_batch_patch_tool();
    tool.spec.name = PATCH_TOOL.into();
    tool.spec.description = "Replace the body of a fresh document returned by inspect_word_file with exact bounded plain text and create a new DOCX in an owner-approved directory. Existing body formatting, tables and images are replaced; section settings and other package parts are preserved. Revalidate the source and read back the copy. Never overwrite the source. No Word application, Live session or native export is used.".into();
    tool.spec.parameters_schema["properties"]["output"] = batch_output_schema("docx");
    tool
}
pub(super) fn provider() -> ProviderDescriptor {
    let read = provider_for_tool(
        PROVIDER_ID,
        INSPECT_CAPABILITY_ID,
        "assistant.capability.wordBatchInspect",
        vec![ADAPTER_ID.into()],
        ExecutionLocality::Edge,
        CapabilityEffect::ReadFile,
        1,
        Vec::new(),
        vec![CapabilityDataCategory::FileContent],
        vec![AuthorizationResourceKind::FreshObjectReference],
        inspect_tool(),
    );
    let write = provider_for_tool(
        PROVIDER_ID,
        PATCH_CAPABILITY_ID,
        "assistant.capability.wordBatchPatch",
        vec![ADAPTER_ID.into()],
        ExecutionLocality::Edge,
        CapabilityEffect::MutateApplication,
        1,
        Vec::new(),
        vec![CapabilityDataCategory::FileContent],
        vec![AuthorizationResourceKind::FreshObjectReference],
        patch_tool(),
    );
    let mut descriptor = merge_provider_capabilities(read, write);
    for capability in &mut descriptor.wire.capabilities {
        capability.prerequisites.platforms = vec![CapabilityPlatform::Windows];
    }
    for capability in &mut descriptor.capabilities {
        capability.wire.prerequisites.platforms = vec![CapabilityPlatform::Windows];
    }
    descriptor
}
pub(super) fn adapter(providers: &ProviderRegistry) -> EdgeAdapterDescriptor {
    EdgeAdapterDescriptor {
        adapter_id: ADAPTER_ID.into(),
        adapter_version: desk_agent_protocol::computer_use::office_batch::DOCX_ADAPTER_VERSION
            .into(),
        capability_ids: vec![INSPECT_CAPABILITY_ID.into(), PATCH_CAPABILITY_ID.into()],
        limits: providers
            .capability(INSPECT_CAPABILITY_ID)
            .expect("static DOCX read exists")
            .wire
            .limits,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn docx_readiness_projects_only_batch_and_preserves_disabled_state() {
        use desk_agent_protocol::computer_use::{
            ComputerUseAdapterKind, ComputerUseAdapterRef, ComputerUseCapabilityReadiness,
            office_batch,
        };
        let mut readiness = ComputerUseReadiness {
            schema_version: 1,
            revision: 7,
            observed_at: "2026-09-14T00:00:00Z".into(),
            expires_at: "2026-09-14T00:01:00Z".into(),
            server_api_version: 1,
            os: "windows".into(),
            interactive_session_incarnation: "1:worker".into(),
            local_ceiling_revision: 3,
            context_references: vec![],
            capabilities: vec![ComputerUseCapabilityReadiness {
                capability: Capability::DocumentLiveInspect,
                adapter: ComputerUseAdapterRef {
                    kind: ComputerUseAdapterKind::OfficeWord,
                    version: office_batch::DOCX_ADAPTER_VERSION.into(),
                },
                supported: true,
                ready: true,
                reason: None,
            }],
        };
        let reports = provider_readiness_reports(&readiness).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].provider_id, PROVIDER_ID);
        assert_eq!(reports[0].capability_id, INSPECT_CAPABILITY_ID);
        assert_eq!(reports[0].adapter_id.as_deref(), Some(ADAPTER_ID));
        assert!(reports[0].ready);
        let mut mutation = readiness.clone();
        mutation.capabilities[0].capability = Capability::DocumentLivePatchConfirmed;
        let mutation_reports = provider_readiness_reports(&mutation).unwrap();
        assert_eq!(mutation_reports.len(), 1);
        assert_eq!(mutation_reports[0].capability_id, PATCH_CAPABILITY_ID);
        mutation.capabilities[0].adapter.version = "office-docx-batch/v2".into();
        assert!(provider_readiness_reports(&mutation).is_err());
        readiness.capabilities[0].ready = false;
        readiness.capabilities[0].reason = Some(ComputerUseReadinessReason::DisabledByLocalCeiling);
        let blocked = provider_readiness_reports(&readiness).unwrap();
        assert!(!blocked[0].enabled && !blocked[0].ready);
        readiness.capabilities[0].adapter.version = "office-docx-batch/v2".into();
        assert!(provider_readiness_reports(&readiness).is_err());
        readiness.capabilities[0].adapter = ComputerUseAdapterRef {
            kind: ComputerUseAdapterKind::IworkPages,
            version: IWORK_ADAPTER_VERSION.into(),
        };
        readiness.os = "macos".into();
        let iwork = provider_readiness_reports(&readiness).unwrap();
        assert_eq!(iwork.len(), 2);
        assert!(
            iwork
                .iter()
                .all(|report| report.provider_id == DOCUMENT_LIVE_PROVIDER_ID)
        );
    }
    #[test]
    fn word_registration_is_selected_file_only_and_windows_only() {
        let registry = device_assistant_provider_registry();
        let capability = registry.capability(INSPECT_CAPABILITY_ID).unwrap();
        assert_eq!(
            capability.wire.prerequisites.platforms,
            [CapabilityPlatform::Windows]
        );
        assert_eq!(capability.wire.effect, CapabilityEffect::ReadFile);
        assert!(is_selectable_context_capability_id(INSPECT_CAPABILITY_ID));
        assert_eq!(
            selected_context_capabilities(&[INSPECT_CAPABILITY_ID.into()]).unwrap()[0],
            Capability::DocumentLiveInspect
        );
        let spec = &capability.tool_spec.parameters_schema;
        for field in ["path", "source", "target", "batch_file"] {
            assert!(spec["properties"].get(field).is_none());
        }
        assert_eq!(spec["additionalProperties"], false);
        assert!(!crate::input_read_context::object_read::implicit_object_tool(INSPECT_TOOL, &[]));
        let _ = device_assistant_edge_adapter_registry();
    }
}
