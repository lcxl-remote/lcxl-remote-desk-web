//! Windows file-only Office inventory, separate from iWork and Office.js Live.
use super::*;
pub const PROVIDER_ID: &str = "office.pptx.batch";
pub const INSPECT_CAPABILITY_ID: &str = "office.pptx.batch.inspect";
pub const ADAPTER_ID: &str = "windows.office.pptx.batch";
pub const INSPECT_TOOL: &str = "inspect_powerpoint_file";
pub const PATCH_TOOL: &str = "patch_powerpoint_copy";
pub const PATCH_CAPABILITY_ID: &str = "office.pptx.batch.patch";
pub(super) const PATCH_READINESS_IDENTITIES: [(&str, &str, &str, &str); 1] = [(
    PROVIDER_ID,
    PATCH_CAPABILITY_ID,
    ADAPTER_ID,
    desk_agent_protocol::computer_use::office_batch::PPTX_ADAPTER_VERSION,
)];
pub(super) const READINESS_IDENTITIES: [(&str, &str, &str, &str); 1] = [(
    PROVIDER_ID,
    INSPECT_CAPABILITY_ID,
    ADAPTER_ID,
    desk_agent_protocol::computer_use::office_batch::PPTX_ADAPTER_VERSION,
)];

pub(super) fn provider() -> ProviderDescriptor {
    let mut descriptor = provider_for_tool(
        PROVIDER_ID,
        INSPECT_CAPABILITY_ID,
        "assistant.capability.powerpointBatchInspect",
        vec![ADAPTER_ID.into()],
        ExecutionLocality::Edge,
        CapabilityEffect::ReadFile,
        1,
        Vec::new(),
        vec![CapabilityDataCategory::FileContent],
        vec![AuthorizationResourceKind::FreshObjectReference],
        batch_inspect_tool(
            INSPECT_TOOL,
            "Read a bounded title and presenter-notes projection from exactly one owner-selected PPTX file snapshot. No Office application or Live session is opened. The model cannot nominate a path, source token, or interactive target.",
            Capability::PresentationLiveInspect,
        ),
    );
    descriptor = merge_provider_capabilities(
        descriptor,
        provider_for_tool(
            PROVIDER_ID,
            PATCH_CAPABILITY_ID,
            "assistant.capability.powerpointBatchPatch",
            vec![ADAPTER_ID.into()],
            ExecutionLocality::Edge,
            CapabilityEffect::MutateApplication,
            1,
            Vec::new(),
            vec![CapabilityDataCategory::FileContent],
            vec![AuthorizationResourceKind::FreshObjectReference],
            patch_tool(),
        ),
    );
    for capability in &mut descriptor.wire.capabilities {
        capability.prerequisites.platforms = vec![CapabilityPlatform::Windows];
    }
    for capability in &mut descriptor.capabilities {
        capability.wire.prerequisites.platforms = vec![CapabilityPlatform::Windows];
    }
    descriptor
}

pub(super) fn patch_tool() -> RegisteredTool {
    let mut tool = presentation_batch_patch_tool();
    tool.spec.name = PATCH_TOOL.into();
    tool.spec.description = "Apply one exact title or presenter-notes change to a fresh slide returned by inspect_powerpoint_file, then create a new PPTX in an owner-approved directory. Revalidate the selected source and read back the new file; never overwrite the source. No Office application, Live session, or native export is used.".into();
    tool.spec.parameters_schema["properties"]["output"] = batch_output_schema("pptx");
    tool
}

pub(super) fn adapter(providers: &ProviderRegistry) -> EdgeAdapterDescriptor {
    EdgeAdapterDescriptor {
        adapter_id: ADAPTER_ID.into(),
        adapter_version: desk_agent_protocol::computer_use::office_batch::PPTX_ADAPTER_VERSION
            .into(),
        capability_ids: vec![INSPECT_CAPABILITY_ID.into(), PATCH_CAPABILITY_ID.into()],
        limits: providers
            .capability(INSPECT_CAPABILITY_ID)
            .expect("static PPTX read exists")
            .wire
            .limits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pptx_readiness_projects_only_batch_and_preserves_disabled_state() {
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
                capability: Capability::PresentationLiveInspect,
                adapter: ComputerUseAdapterRef {
                    kind: ComputerUseAdapterKind::OfficePowerPoint,
                    version: office_batch::PPTX_ADAPTER_VERSION.into(),
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
        mutation.capabilities[0].capability = Capability::PresentationLivePatchConfirmed;
        let mutation_reports = provider_readiness_reports(&mutation).unwrap();
        assert_eq!(mutation_reports.len(), 1);
        assert_eq!(mutation_reports[0].capability_id, PATCH_CAPABILITY_ID);
        mutation.capabilities[0].adapter.version = "office-pptx-batch/v2".into();
        assert!(provider_readiness_reports(&mutation).is_err());
        readiness.capabilities[0].ready = false;
        readiness.capabilities[0].reason = Some(ComputerUseReadinessReason::DisabledByLocalCeiling);
        let blocked = provider_readiness_reports(&readiness).unwrap();
        assert!(!blocked[0].enabled && !blocked[0].ready);
        readiness.capabilities[0].adapter.version = "office-pptx-batch/v2".into();
        assert!(provider_readiness_reports(&readiness).is_err());
        readiness.capabilities[0].adapter = ComputerUseAdapterRef {
            kind: ComputerUseAdapterKind::IworkKeynote,
            version: IWORK_ADAPTER_VERSION.into(),
        };
        readiness.os = "macos".into();
        let iwork = provider_readiness_reports(&readiness).unwrap();
        assert_eq!(iwork.len(), 2);
        assert!(
            iwork
                .iter()
                .all(|report| report.provider_id == PRESENTATION_LIVE_PROVIDER_ID)
        );
    }
    #[test]
    fn powerpoint_registration_is_selected_file_only_and_windows_only() {
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
            Capability::PresentationLiveInspect
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
