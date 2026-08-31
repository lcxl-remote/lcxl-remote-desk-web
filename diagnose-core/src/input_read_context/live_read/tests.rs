use super::*;
use desk_agent_protocol::computer_use::{
    COMPUTER_USE_SCHEMA_VERSION, ComputerUseAdapterKind, ComputerUseAdapterRef,
    ComputerUseCapabilityReadiness, ComputerUseContextReference,
};

const TOOLS: [&str; 4] = [
    "inspect_live_document",
    "inspect_live_presentation",
    "inspect_live_spreadsheet",
    "inspect_office_selection",
];

fn readiness() -> ComputerUseReadiness {
    ComputerUseReadiness {
        schema_version: COMPUTER_USE_SCHEMA_VERSION,
        revision: 1,
        observed_at: "2026-08-31T00:00:00Z".into(),
        expires_at: "2026-08-31T00:01:00Z".into(),
        server_api_version: 1,
        os: "macos".into(),
        interactive_session_incarnation: "worker-1".into(),
        local_ceiling_revision: 1,
        capabilities: TOOLS
            .iter()
            .map(|name| ComputerUseCapabilityReadiness {
                capability: target_kind(name).unwrap().0,
                adapter: ComputerUseAdapterRef {
                    kind: ComputerUseAdapterKind::OfficeExcel,
                    version: "test".into(),
                },
                supported: true,
                ready: true,
                reason: None,
            })
            .collect(),
        context_references: TOOLS
            .iter()
            .map(|name| ComputerUseContextReference {
                capability: target_kind(name).unwrap().0,
                object_ref: ObjectRef {
                    token: format!("token-{name}"),
                    snapshot_id: "snapshot-1".into(),
                    object_kind: target_kind(name).unwrap().1,
                    expires_at: "2026-08-31T00:02:00Z".into(),
                },
            })
            .collect(),
    }
}

fn now() -> u64 {
    millis("2026-08-31T00:00:01Z").unwrap()
}

fn selection(names: &[&str]) -> ReadContextSelection {
    ReadContextSelection {
        tool_names: names.iter().map(|s| (*s).into()).collect(),
        expires_at: None,
        object_attachments: vec![],
        live_targets: vec![],
    }
}

fn captured() -> ReadContextSelection {
    let mut selection = selection(&TOOLS);
    selection.live_targets = capture(&selection, Some(&readiness()), now()).unwrap();
    selection
}

#[test]
fn captures_only_exposed_live_reads_and_never_batch_file_targets() {
    let mut input = selection(&["inspect_live_document", "inspect_selected_pages_with_iwork"]);
    input.live_targets = capture(&input, Some(&readiness()), now()).unwrap();
    assert_eq!(input.live_targets.len(), 1);
    assert_eq!(input.live_targets[0].tool_name, "inspect_live_document");
    assert_eq!(
        input.live_targets[0].object_ref,
        readiness().context_references[0].object_ref
    );
    assert!(
        capture(&selection(&["read_system_info"]), None, now())
            .unwrap()
            .is_empty()
    );
    assert!(capture(&selection(&["inspect_live_document"]), None, now()).is_err());
    assert_eq!(captured().live_targets.len(), 4);
}

#[test]
fn fresh_readiness_does_not_extend_the_original_deadline() {
    let selection = captured();
    let before = selection.clone();
    let mut current = readiness();
    current.revision += 1;
    current.expires_at = "2026-08-31T00:03:00Z".into();
    validate_current(&selection, Some(&current), now()).unwrap();
    let deadline = millis("2026-08-31T00:01:00Z").unwrap();
    assert_eq!(
        expiry(&selection, &selection.live_targets[0]).unwrap(),
        deadline
    );
    assert!(validate_current(&selection, Some(&current), deadline).is_err());
    let mut shorter = selection.clone();
    shorter.expires_at = Some("2026-08-31T00:00:30Z".into());
    assert!(target(&shorter, TOOLS[0], millis("2026-08-31T00:00:30Z").unwrap()).is_err());
    assert_eq!(selection, before);
}

#[test]
fn changed_worker_object_snapshot_expiry_and_unready_reports_are_rejected() {
    let selection = captured();
    for variant in 0..9 {
        let mut current = readiness();
        match variant {
            0 => current.interactive_session_incarnation = "worker-2".into(),
            1 => current.context_references[0].object_ref.token = "other-document".into(),
            2 => current.context_references[0].object_ref.snapshot_id = "other-snapshot".into(),
            3 => {
                current.context_references[0].object_ref.expires_at = "2026-08-31T00:03:00Z".into()
            }
            4 => current.capabilities[0].ready = false,
            5 => current.capabilities[0].supported = false,
            6 => current.context_references.clear(),
            7 => current.expires_at = "2026-08-31T00:00:01Z".into(),
            8 => current.observed_at = "2026-08-31T00:00:32Z".into(),
            _ => unreachable!(),
        }
        assert!(
            validate_current(&selection, Some(&current), now()).is_err(),
            "variant {variant}"
        );
    }
    assert!(validate_current(&selection, None, now()).is_err());
}

#[test]
fn invalid_duplicate_unknown_or_unselected_targets_never_validate() {
    for variant in 0..8 {
        let mut selection = captured();
        match variant {
            0 => selection.live_targets.swap(0, 1),
            1 => selection
                .live_targets
                .push(selection.live_targets[0].clone()),
            2 => selection.live_targets[0].tool_name = "read_system_info".into(),
            3 => {
                selection.tool_names.remove(0);
            }
            4 => selection.live_targets[0].object_ref.object_kind = ObjectKind::File,
            5 => selection.live_targets[0].object_ref.token = " ".into(),
            6 => selection.live_targets[0].interactive_session_incarnation = "x".repeat(4097),
            7 => selection.live_targets[0].object_ref.expires_at = "invalid".into(),
            _ => unreachable!(),
        }
        assert!(selection.validate().is_err(), "variant {variant}");
    }
}

#[test]
fn legacy_json_round_trips_but_cannot_supply_a_missing_original_target() {
    let old = serde_json::json!({"tool_names":["inspect_live_document"], "expires_at":null});
    let selection: ReadContextSelection = serde_json::from_value(old.clone()).unwrap();
    selection.validate().unwrap();
    assert_eq!(serde_json::to_value(&selection).unwrap(), old);
    assert!(target(&selection, "inspect_live_document", now()).is_err());
    let current = captured();
    assert_eq!(
        serde_json::from_str::<ReadContextSelection>(&serde_json::to_string(&current).unwrap())
            .unwrap(),
        current
    );
    let mut corrupt = serde_json::to_value(current).unwrap();
    corrupt["live_targets"][0]["unexpected"] = true.into();
    assert!(serde_json::from_value::<ReadContextSelection>(corrupt).is_err());
}

#[test]
fn object_deadline_is_independent_of_readiness_deadline() {
    let mut current = readiness();
    current.context_references[0].object_ref.expires_at = "2026-08-31T00:00:20Z".into();
    let mut selection = selection(&[TOOLS[0]]);
    selection.live_targets = capture(&selection, Some(&current), now()).unwrap();
    let deadline = millis("2026-08-31T00:00:20Z").unwrap();
    validate_current(&selection, Some(&current), deadline - 1).unwrap();
    assert!(validate_current(&selection, Some(&current), deadline).is_err());
}

#[test]
fn accepted_clock_skew_is_preserved_but_original_targets_cannot_be_recaptured() {
    let mut current = readiness();
    current.observed_at = "2026-08-31T00:00:31Z".into();
    let mut selection = selection(&[TOOLS[0]]);
    selection.live_targets = capture(&selection, Some(&current), now()).unwrap();
    validate_current(&selection, Some(&current), now()).unwrap();
    assert!(capture(&selection, Some(&current), now()).is_err());
}

#[test]
fn actual_read_binding_ignores_model_targets_and_preserves_original_source_and_deadline() {
    use crate::{
        chat::ToolCall,
        input_read_context::object_read::{ObjectReadBinding, requires_objects},
        seam::ToolRunOutput,
    };
    use desk_agent_protocol::{
        ContextKind, OperationInput, ReadContextInput, data_lineage::DestinationIdentity,
    };
    let selection = captured();
    let destination = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    let binding = ObjectReadBinding {
        original: &selection,
        destination: &destination,
        now_unix_ms: now(),
    };
    for name in TOOLS {
        assert!(requires_objects(name));
        let mut forged = target(&selection, name, now()).unwrap().object_ref.clone();
        forged.token = "model-supplied-other-target".into();
        let mut arguments = serde_json::json!({"max_bytes":1024});
        arguments[if name == "inspect_office_selection" {
            "document"
        } else {
            "target"
        }] = serde_json::to_value(forged).unwrap();
        let call = ToolCall {
            id: "read".into(),
            name: name.into(),
            arguments_json: arguments.to_string(),
        };
        let (_, mut operation) = crate::read_tools::build_read_operation(&call).unwrap();
        binding.bind(&call, &mut operation).unwrap();
        let reference = match operation {
            OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::OfficeDocumentInspect(p),
            }) => {
                assert!(p.selection_only);
                assert_eq!(p.max_bytes, 1024);
                p.document.unwrap()
            }
            OperationInput::ReadContext(ReadContextInput {
                kind:
                    ContextKind::SpreadsheetLiveInspect(p)
                    | ContextKind::DocumentLiveInspect(p)
                    | ContextKind::PresentationLiveInspect(p),
            }) => {
                assert_eq!(p.max_bytes, 1024);
                assert!(p.batch_file.is_none());
                p.target.unwrap()
            }
            _ => panic!("unexpected live operation"),
        };
        assert_eq!(
            reference,
            target(&selection, name, now()).unwrap().object_ref
        );
        let output = ToolRunOutput {
            content: "synthetic document content".into(),
            image_data_url: None,
        };
        let envelope = crate::model_message_labels::read_result_envelope(
            &crate::device_assistant::device_assistant_provider_registry(),
            &call,
            &output,
            crate::model_message_labels::ReadResultLabel {
                envelope_id: "result".into(),
                observation_id: "observation".into(),
                source_object_id: None,
                observed_at_unix_ms: now(),
            },
        )
        .unwrap();
        let labeled = binding.label(&call, &output, envelope.clone()).unwrap();
        assert_eq!(labeled.allowed_destinations, [destination.clone()]);
        assert_eq!(
            labeled.retention.expires_at_unix_ms,
            Some(binding.expiry(&call).unwrap())
        );
        assert!(
            labeled
                .provenance
                .source_object_id
                .as_ref()
                .unwrap()
                .starts_with("live-target:sha256:")
        );
        assert!(
            !serde_json::to_string(&labeled)
                .unwrap()
                .contains(&reference.token)
        );
        let oversized = ToolRunOutput {
            content: "x".repeat(1025),
            image_data_url: None,
        };
        assert!(binding.label(&call, &oversized, envelope.clone()).is_err());
        let image = ToolRunOutput {
            content: "".into(),
            image_data_url: Some("data:image/png;base64,x".into()),
        };
        assert!(binding.label(&call, &image, envelope.clone()).is_err());
        let mut changed = selection.clone();
        changed
            .live_targets
            .iter_mut()
            .find(|t| t.tool_name == name)
            .unwrap()
            .object_ref
            .token = "other".into();
        let changed_binding = ObjectReadBinding {
            original: &changed,
            ..binding
        };
        assert_ne!(
            changed_binding
                .label(&call, &output, envelope)
                .unwrap()
                .provenance
                .source_object_id,
            labeled.provenance.source_object_id
        );
    }
}

#[test]
fn legacy_live_reads_cannot_bypass_the_original_object_fence() {
    use crate::{chat::ToolCall, input_read_context::object_read::ObjectReadBinding};
    use desk_agent_protocol::data_lineage::DestinationIdentity;
    let legacy = selection(&TOOLS);
    let destination = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    let binding = ObjectReadBinding {
        original: &legacy,
        destination: &destination,
        now_unix_ms: now(),
    };
    for name in TOOLS {
        let call = ToolCall {
            id: "read".into(),
            name: name.into(),
            arguments_json: "{}".into(),
        };
        let (_, mut operation) = crate::read_tools::build_read_operation(&call).unwrap();
        assert!(binding.bind(&call, &mut operation).is_err());
        assert!(binding.expiry(&call).is_err());
    }
}

#[test]
fn all_live_read_grants_use_original_references_and_deadlines_on_both_servers() {
    use crate::{
        capability_availability::CapabilityAvailability,
        chat::ToolCall,
        dynamic_run::{PermissionDecisionItem, PermissionItemDecision},
        permission_grant::{PermissionGrantIssuanceContext, build_permission_grants},
    };
    use desk_agent_protocol::{
        AgentScope, ExecutionMode, capability_provider::ProductSurface,
        data_lineage::DestinationIdentity,
    };
    let original = captured();
    let registry = crate::device_assistant::device_assistant_provider_registry();
    let destination = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    let mut session = crate::session::PersistedAgentSession::new(
        "run",
        "owner",
        "device",
        crate::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-08-31T00:00:00Z",
    );
    session.input_revision = 1;
    session.conversation.push(
        crate::model_message_labels::model_bound_user_message(
            "input".into(),
            "Read selected documents".into(),
            destination,
        )
        .unwrap(),
    );
    for name in TOOLS {
        let capability = registry.capability_for_tool(name).unwrap();
        let provider = registry
            .provider_for_capability(&capability.wire.capability_id)
            .unwrap();
        let request = crate::permission_tools::build_permission_request(&ToolCall {
            id:"request".into(),name:"request_capability_grants".into(),arguments_json:serde_json::json!({"items":[{
                "item_id":"read","provider_id":provider.wire.provider_id,"tool_name":name,"expected_effect":capability.wire.effect,
                "suggested_ttl_seconds":120,"suggested_max_uses":1,"reason":"Read original document"}]}).to_string(),
        },&registry,"request".into(),1,"2026-08-31T00:00:01Z".into()).unwrap();
        let decisions = [PermissionDecisionItem {
            item_id: "read".into(),
            decision: PermissionItemDecision::Approve {
                resource_scope: request.items[0].resource_scope.clone(),
                operation_scope: request.items[0].operation_scope.clone(),
                export_destinations: vec![],
                ttl_seconds: 120,
                max_uses: 1,
            },
        }];
        let inventory = [CapabilityAvailability {
            provider_id: provider.wire.provider_id.clone(),
            capability_id: capability.wire.capability_id.clone(),
            tool_name: name.into(),
            compiled: true,
            enabled: true,
            connected: true,
            ready: true,
            reason: None,
        }];
        let reference = target(&original, name, now()).unwrap().object_ref.clone();
        let mut ambient = reference.clone();
        ambient.token = "later-unselected-document".into();
        let refs = [reference.clone(), ambient.clone()];
        for surface in [
            ProductSurface::OssPersonalOwner,
            ProductSurface::ManagerPersonalOwner,
        ] {
            let context = PermissionGrantIssuanceContext {
                surface,
                registry: &registry,
                inventory: &inventory,
                readiness_revision: 1,
                now_unix_ms: now(),
                implicit_fresh_object_refs: &refs,
            };
            let grants =
                build_permission_grants(&session, &request, &decisions, &context, Some(&original))
                    .unwrap();
            assert_eq!(grants.len(), 1);
            assert_eq!(
                grants[0].resource_scope,
                crate::capability_grant::fresh_object_resource_scope(std::slice::from_ref(
                    &reference
                )),
                "{surface:?}/{name}"
            );
            assert_eq!(
                grants[0].expires_at_unix_ms,
                millis("2026-08-31T00:01:00Z").unwrap()
            );
            let absent = PermissionGrantIssuanceContext {
                implicit_fresh_object_refs: std::slice::from_ref(&ambient),
                ..context
            };
            assert!(
                build_permission_grants(&session, &request, &decisions, &absent, Some(&original))
                    .is_err()
            );
        }
    }
}
