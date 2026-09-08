use super::*;
use desk_agent_protocol::capability_grant::CapabilityGrantLimits;

fn scope() -> TaskPermissionScope {
    TaskPermissionScope {
        resources: vec!["device:1".into()],
        operations: vec!["observe".into()],
        export_destinations: vec![],
        limits: CapabilityGrantLimits {
            max_bytes_per_call: 100,
            max_items_per_call: 2,
            max_calls: 3,
        },
    }
}
fn contract() -> TaskContract {
    let automatic = scope();
    let mut ceiling = automatic.clone();
    ceiling.limits.max_bytes_per_call = 200;
    TaskContract {
        schema_version: 1,
        schedule_id: "schedule-1".into(),
        task_revision: 1,
        contract_revision: 1,
        target_device_id: "device-1".into(),
        prompt_sha256: "a".repeat(64),
        permissions: vec![TaskPermissionRule {
            rule_id: "observe".into(),
            provider_id: "device".into(),
            capability_id: "inspect".into(),
            tool_name: "inspect".into(),
            tool_schema_version: 1,
            effect: CapabilityEffect::ReadDevice,
            risk_tier: CapabilityRiskTier::R0,
            input: TaskInputConstraint::ScopedRead,
            automatic,
            approval_ceiling: ceiling,
        }],
        steps: vec![],
        exception_mode: TaskExceptionMode::RequestApproval,
        budget: TaskBudget {
            max_runs_per_utc_day: 24,
            max_calls_per_run: 10,
            max_model_tokens_per_run: 10000,
            max_runtime_seconds: 300,
        },
    }
}

#[test]
fn contracts_normalize_exact_json_and_reject_ambiguous_or_unbounded_rules() {
    let mut input = contract();
    input.permissions[0].input = TaskInputConstraint::Exact {
        canonical_json: "{\"z\":2, \"a\":{\"d\":3,\"b\":1}}".into(),
    };
    let validated = validate_contract(&input).unwrap();
    input.permissions[0].input = TaskInputConstraint::Exact {
        canonical_json: "{\"a\":{\"b\":1,\"d\":3},\"z\":2}".into(),
    };
    assert_eq!(
        validated.digest(),
        validate_contract(&input).unwrap().digest()
    );
    assert_eq!(
        validated.digest(),
        parse_contract(validated.canonical_json()).unwrap().digest()
    );
    input.permissions.push(input.permissions[0].clone());
    assert_eq!(
        validate_contract(&input).unwrap_err(),
        TaskContractError::ConflictingRules
    );
    input = contract();
    input.permissions[0].automatic.limits.max_calls = 4;
    assert_eq!(
        validate_contract(&input).unwrap_err(),
        TaskContractError::InvalidLimits
    );
    input = contract();
    input.permissions[0].effect = CapabilityEffect::ExecuteCommand;
    assert_eq!(
        validate_contract(&input).unwrap_err(),
        TaskContractError::InvalidInput
    );
    input.permissions[0].input = TaskInputConstraint::Exact {
        canonical_json: "{}".into(),
    };
    assert_eq!(
        validate_contract(&input).unwrap_err(),
        TaskContractError::InvalidSteps
    );
    let mut value = serde_json::to_value(contract()).unwrap();
    value["allow_all"] = Value::Bool(true);
    assert_eq!(
        parse_contract(&value.to_string()).unwrap_err(),
        TaskContractError::InvalidJson
    );
}

#[test]
fn boundary_classification_never_requests_approval_for_hard_violations() {
    let document = validate_contract(&contract()).unwrap();
    let input = serde_json::json!({});
    let resources = vec!["device:1".into()];
    let operations = vec!["observe".into()];
    let steps = std::collections::BTreeMap::new();
    let mut call = TaskCall {
        target_device_id: "device-1",
        provider_id: "device",
        capability_id: "inspect",
        tool_name: "inspect",
        tool_schema_version: 1,
        effect: CapabilityEffect::ReadDevice,
        risk_tier: CapabilityRiskTier::R0,
        input: &input,
        resources: &resources,
        operations: &operations,
        export_destinations: &[],
        byte_count: 100,
        item_count: 1,
        rule_call_count: 0,
        run_call_count: 0,
        step_id: None,
        step_states: &steps,
        message_destination: None,
        source_scopes: &[],
    };
    assert!(matches!(
        document.evaluate(&call),
        TaskDecision::Allowed { .. }
    ));
    call.byte_count = 150;
    assert!(matches!(
        document.evaluate(&call),
        TaskDecision::ApprovalRequired { .. }
    ));
    call.byte_count = 201;
    assert_eq!(
        document.evaluate(&call),
        TaskDecision::Denied(TaskDenyReason::HardBoundary)
    );
    call.byte_count = 150;
    call.target_device_id = "other-device";
    assert_eq!(
        document.evaluate(&call),
        TaskDecision::Denied(TaskDenyReason::Identity)
    );
    call.target_device_id = "device-1";
    call.run_call_count = 10;
    assert_eq!(
        document.evaluate(&call),
        TaskDecision::Denied(TaskDenyReason::Budget)
    );
    call.run_call_count = 0;
    let mut strict = contract();
    strict.exception_mode = TaskExceptionMode::Deny;
    assert_eq!(
        validate_contract(&strict).unwrap().evaluate(&call),
        TaskDecision::Denied(TaskDenyReason::ApprovalDisabled)
    );
}

fn message_contract() -> TaskContract {
    use desk_agent_protocol::{
        browser_control::BrowserOrigin,
        communication::{RecipientIdentity, RecipientKind},
    };
    let mut contract = contract();
    let rule = &mut contract.permissions[0];
    rule.effect = CapabilityEffect::SendExternal;
    rule.risk_tier = CapabilityRiskTier::R3;
    rule.input = TaskInputConstraint::GeneratedMessage {
        attachment_policy: None,
        max_subject_bytes: 100,
        max_body_bytes: 1000,
    };
    let destination = DestinationIdentity::EmailAccount {
        account_id: "account-1".into(),
    };
    rule.automatic.export_destinations = vec![destination.clone()];
    rule.approval_ceiling.export_destinations = vec![destination];
    contract.steps = vec![TaskFixedStep {
        step_id: "send-report".into(),
        rule_id: "observe".into(),
        depends_on: vec![],
        binding: TaskStepBinding::SendMessage {
            destination: TaskMessageDestination {
                channel: CommunicationChannel::Email,
                surface_kind: CommunicationSurfaceKind::ChromeExtension,
                scope: CommunicationSurfaceScope::WebOrigin {
                    origin: BrowserOrigin {
                        kind: BrowserOriginKind::Https,
                        host_ascii: GMAIL_WEB_HOST.into(),
                        port: 443,
                    },
                },
                adapter_id: "gmail-web".into(),
                adapter_version: "1".into(),
                profile_id: "profile-1".into(),
                account_id: "account-1".into(),
                recipients: vec![RecipientIdentity {
                    role: RecipientRole::To,
                    kind: RecipientKind::EmailMailbox,
                    stable_id: "recipient-1".into(),
                    canonical_address: "report@example.com".into(),
                    display_name: None,
                    display_warnings: vec![],
                    resolved_members: vec![],
                    member_snapshot_sha256: None,
                }],
            },
            allowed_source_scopes: vec!["device:1".into()],
        },
    }];
    refresh_message_scope(&mut contract);
    contract
}

fn refresh_message_scope(contract: &mut TaskContract) {
    let TaskStepBinding::SendMessage { destination, .. } = &contract.steps[0].binding else {
        unreachable!()
    };
    let scope = task_message_resource_scope(&contract.target_device_id, destination).unwrap();
    contract.permissions[0].automatic.resources = scope.clone();
    contract.permissions[0].approval_ceiling.resources = scope;
}

#[test]
fn generated_messages_cannot_replace_bound_destination_or_repeat_completed_steps() {
    let definition = message_contract();
    let validated = validate_contract(&definition).unwrap();
    let TaskStepBinding::SendMessage { destination, .. } = &definition.steps[0].binding else {
        unreachable!()
    };
    let sources = vec!["device:1".into()];
    let operations = vec!["observe".into()];
    let input = serde_json::json!({"subject":"Daily report", "body":"All devices healthy."});
    let ready = std::collections::BTreeMap::from([("send-report".into(), TaskStepStatus::Pending)]);
    let mut call = TaskCall {
        target_device_id: "device-1",
        provider_id: "device",
        capability_id: "inspect",
        tool_name: "inspect",
        tool_schema_version: 1,
        effect: CapabilityEffect::SendExternal,
        risk_tier: CapabilityRiskTier::R3,
        input: &input,
        resources: &definition.permissions[0].automatic.resources,
        operations: &operations,
        export_destinations: &definition.permissions[0].automatic.export_destinations,
        byte_count: 50,
        item_count: 1,
        rule_call_count: 0,
        run_call_count: 0,
        step_id: Some("send-report"),
        step_states: &ready,
        message_destination: Some(destination),
        source_scopes: &sources,
    };
    assert!(matches!(
        validated.evaluate(&call),
        TaskDecision::Allowed { .. }
    ));
    let injected = serde_json::json!({"subject":"Daily report", "body":"Safe", "recipient":"someone-else@example.com"});
    call.input = &injected;
    assert_eq!(
        validated.evaluate(&call),
        TaskDecision::Denied(TaskDenyReason::Input)
    );
    call.input = &input;
    let mut changed = destination.clone();
    changed.account_id = "other-account".into();
    call.message_destination = Some(&changed);
    assert_eq!(
        validated.evaluate(&call),
        TaskDecision::Denied(TaskDenyReason::Destination)
    );
    let mut changed_recipient = destination.clone();
    changed_recipient.recipients[0].stable_id = "other-recipient".into();
    call.message_destination = Some(&changed_recipient);
    assert_eq!(
        validated.evaluate(&call),
        TaskDecision::Denied(TaskDenyReason::Destination)
    );
    call.message_destination = Some(destination);
    let outside = vec!["private:unapproved".into()];
    call.source_scopes = &outside;
    assert_eq!(
        validated.evaluate(&call),
        TaskDecision::Denied(TaskDenyReason::SourceScope)
    );
    call.source_scopes = &sources;
    call.step_id = None;
    assert_eq!(
        validated.evaluate(&call),
        TaskDecision::Denied(TaskDenyReason::Step)
    );
    call.step_id = Some("send-report");
    let completed =
        std::collections::BTreeMap::from([("send-report".into(), TaskStepStatus::Succeeded)]);
    call.step_states = &completed;
    assert_eq!(
        validated.evaluate(&call),
        TaskDecision::Denied(TaskDenyReason::Step)
    );
    let unknown =
        std::collections::BTreeMap::from([("send-report".into(), TaskStepStatus::OutcomeUnknown)]);
    call.step_states = &unknown;
    assert_eq!(
        validated.evaluate(&call),
        TaskDecision::Denied(TaskDenyReason::Step)
    );
    let missing = std::collections::BTreeMap::new();
    call.step_states = &missing;
    assert_eq!(
        validated.evaluate(&call),
        TaskDecision::Denied(TaskDenyReason::Step)
    );
}

#[test]
fn malformed_step_graphs_and_destination_scope_expansion_are_rejected() {
    let mut document = message_contract();
    document.steps[0].depends_on = vec!["send-report".into()];
    assert_eq!(
        validate_contract(&document).unwrap_err(),
        TaskContractError::InvalidSteps
    );
    document.steps[0].depends_on = vec!["missing".into()];
    assert_eq!(
        validate_contract(&document).unwrap_err(),
        TaskContractError::InvalidSteps
    );
    document = message_contract();
    document.permissions[0]
        .approval_ceiling
        .export_destinations
        .push(DestinationIdentity::EmailAccount {
            account_id: "other-account".into(),
        });
    assert_eq!(
        validate_contract(&document).unwrap_err(),
        TaskContractError::InvalidDestination
    );
    document = message_contract();
    let TaskStepBinding::SendMessage { destination, .. } = &mut document.steps[0].binding else {
        unreachable!()
    };
    destination.surface_kind = CommunicationSurfaceKind::AssistiveUi;
    assert_eq!(
        validate_contract(&document).unwrap_err(),
        TaskContractError::InvalidDestination
    );
    let mut source = serde_json::to_value(contract()).unwrap();
    source["permissions"][0]["automatic"]["limits"]["allow_all"] = Value::Bool(true);
    assert_eq!(
        parse_contract(&source.to_string()).unwrap_err(),
        TaskContractError::InvalidJson
    );
}

#[test]
fn exact_rehearsal_matching_requires_identity_input_and_observed_scope() {
    use crate::provider_preflight::ObservedCapabilityAuthority;
    let mut input = contract();
    input.permissions[0].input = TaskInputConstraint::Exact {
        canonical_json: "{\"b\":2,\"a\":1}".into(),
    };
    let validated = validate_contract(&input).unwrap();
    let rule = &input.permissions[0];
    let observation = ObservedCapabilityAuthority {
        target_session_id: None,
        envelope_ids: vec![],
        content_digests_sha256: vec![],
        provider_id: rule.provider_id.clone(),
        capability_id: rule.capability_id.clone(),
        tool_name: rule.tool_name.clone(),
        tool_schema_version: rule.tool_schema_version,
        effect: rule.effect,
        risk_tier: rule.risk_tier,
        canonical_input_sha256: format!("{:x}", Sha256::digest(b"{\"a\":1,\"b\":2}")),
        resources: rule.automatic.resources.clone(),
        operations: rule.automatic.operations.clone(),
        export_destinations: vec![],
    };
    assert_eq!(
        validated
            .observed_exact_rule("device-1", &observation)
            .map(|rule| rule.rule_id.as_str()),
        Some("observe")
    );
    assert!(
        validated
            .observed_exact_rule("other-device", &observation)
            .is_none()
    );
    for mismatch in [
        "provider",
        "capability",
        "schema",
        "input",
        "resource",
        "operation",
        "empty",
    ] {
        let mut changed = observation.clone();
        match mismatch {
            "provider" => changed.provider_id = "other".into(),
            "capability" => changed.capability_id = "other".into(),
            "schema" => changed.tool_schema_version += 1,
            "input" => changed.canonical_input_sha256 = "b".repeat(64),
            "resource" => changed.resources.push("outside".into()),
            "operation" => changed.operations.push("write".into()),
            "empty" => changed.resources.clear(),
            _ => unreachable!(),
        }
        assert!(
            validated
                .observed_exact_rule("device-1", &changed)
                .is_none(),
            "{mismatch}"
        );
    }
    for expansion in ["resource", "operation"] {
        let mut expanded = input.clone();
        let rule = &mut expanded.permissions[0];
        match expansion {
            "resource" => {
                rule.automatic.resources.push("unobserved-resource".into());
                rule.approval_ceiling
                    .resources
                    .push("unobserved-resource".into());
            }
            "operation" => {
                rule.automatic
                    .operations
                    .push("unobserved-operation".into());
                rule.approval_ceiling
                    .operations
                    .push("unobserved-operation".into());
            }
            _ => unreachable!(),
        }
        assert!(
            validate_contract(&expanded)
                .unwrap()
                .observed_exact_rule("device-1", &observation)
                .is_none(),
            "unobserved {expansion} must not be covered"
        );
    }
    input.permissions[0].input = TaskInputConstraint::ScopedRead;
    assert!(
        validate_contract(&input)
            .unwrap()
            .observed_exact_rule("device-1", &observation)
            .is_none()
    );
    let scoped = validate_contract(&input).unwrap();
    assert_eq!(
        scoped
            .observed_scoped_read_rule("device-1", &observation)
            .map(|rule| rule.rule_id.as_str()),
        Some("observe")
    );
    let mut other_query = observation.clone();
    other_query.canonical_input_sha256 = "b".repeat(64);
    assert!(
        scoped
            .observed_scoped_read_rule("device-1", &other_query)
            .is_some()
    );
    assert!(
        scoped
            .observed_scoped_read_rule("other-device", &observation)
            .is_none()
    );
    for mismatch in [
        "provider",
        "tool",
        "schema",
        "resource",
        "operation",
        "empty",
        "hash",
    ] {
        let mut changed = observation.clone();
        match mismatch {
            "provider" => changed.provider_id = "other".into(),
            "tool" => changed.tool_name = "other".into(),
            "schema" => changed.tool_schema_version += 1,
            "resource" => changed.resources.push("outside".into()),
            "operation" => changed.operations.push("outside".into()),
            "empty" => changed.resources.clear(),
            "hash" => changed.canonical_input_sha256.clear(),
            _ => unreachable!(),
        }
        assert!(
            scoped
                .observed_scoped_read_rule("device-1", &changed)
                .is_none(),
            "{mismatch}"
        );
    }
    let mut expanded = input.clone();
    expanded.permissions[0]
        .automatic
        .resources
        .push("unobserved".into());
    expanded.permissions[0]
        .approval_ceiling
        .resources
        .push("unobserved".into());
    assert!(
        validate_contract(&expanded)
            .unwrap()
            .observed_scoped_read_rule("device-1", &observation)
            .is_none()
    );
}

#[test]
fn generated_send_binds_actual_payload_current_surface_and_run() {
    use desk_agent_protocol::{
        communication::{CommunicationPayload, CommunicationSurfaceRef, ImmutableBodySnapshot},
        data_lineage::ContentRef,
    };
    let target = TaskMessageTarget {
        task_device_id: "device-1",
        provider_device_id: "device-1",
    };
    let contract = validate_contract(&message_contract()).unwrap();
    let TaskStepBinding::SendMessage { destination, .. } = &contract.contract().steps[0].binding
    else {
        unreachable!()
    };
    let message = TaskGeneratedMessage {
        subject: "Report".into(),
        body: "Current report".into(),
    };
    let surface = CommunicationSurfaceRef {
        device_id: "device-1".into(),
        os_session_id: "current-session".into(),
        revision: 2,
        channel: destination.channel,
        kind: destination.surface_kind,
        scope: destination.scope.clone(),
        adapter_id: destination.adapter_id.clone(),
        adapter_version: destination.adapter_version.clone(),
        profile_id: destination.profile_id.clone(),
        account_id: destination.account_id.clone(),
    };
    let sha = format!("{:x}", Sha256::digest(message.body.as_bytes()));
    let payload = CommunicationPayload {
        surface: surface.clone(),
        recipients: destination.recipients.clone(),
        subject: message.subject.clone(),
        body: ImmutableBodySnapshot {
            content: ContentRef::ImmutableBlob {
                blob_id: "current-body".into(),
                sha256: sha.clone(),
                size_bytes: message.body.len() as u64,
                media_type: "text/plain".into(),
            },
            media_type: "text/plain".into(),
            size_bytes: message.body.len() as u64,
            digest_sha256: sha,
        },
        attachments: vec![],
    };
    let seal = |payload| {
        crate::communication::seal_send_payload("snapshot-1".into(), "run-1".into(), payload, 1000)
            .unwrap()
    };
    let snapshot = seal(payload.clone());
    {
        use crate::{
            communication_handoff::SentWebMessageEvidence,
            provider_preflight::ObservedCapabilityAuthority,
            schedule::source_graph::ResolvedTaskSources,
        };
        use desk_agent_protocol::communication::{SendOutcome, SendReceipt, SendReceiptEvidence};
        let rule = &contract.contract().permissions[0];
        let authority = ObservedCapabilityAuthority {
            target_session_id: None,
            envelope_ids: vec![],
            content_digests_sha256: vec![],
            provider_id: rule.provider_id.clone(),
            capability_id: rule.capability_id.clone(),
            tool_name: rule.tool_name.clone(),
            tool_schema_version: rule.tool_schema_version,
            effect: rule.effect,
            risk_tier: rule.risk_tier,
            canonical_input_sha256: "a".repeat(64),
            resources: vec!["original-ui-resource".into()],
            operations: rule.automatic.operations.clone(),
            export_destinations: rule.automatic.export_destinations.clone(),
        };
        let sent = SentWebMessageEvidence {
            snapshot: snapshot.clone(),
            subject: message.subject.clone(),
            body_plain_text: message.body.clone(),
            receipt: SendReceipt {
                schema_version: desk_agent_protocol::communication::COMMUNICATION_SCHEMA_VERSION,
                snapshot_id: snapshot.snapshot_id.clone(),
                snapshot_sha256: snapshot.canonical_payload_sha256.clone(),
                idempotency_key: crate::communication::send_idempotency_key(&snapshot).unwrap(),
                outcome: SendOutcome::Sent,
                provider_receipt_id: Some("remote-message".into()),
                evidence: SendReceiptEvidence::ProviderUiAcknowledgement,
                observed_at_unix_ms: 1001,
            },
        };
        // Draft construction must preserve the sealed recipient while allowing new text.
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let descriptor = registry
            .capability_for_tool("send_gmail_web_exact")
            .unwrap();
        let mut observed_send = authority.clone();
        observed_send.tool_name = "send_gmail_web_exact".into();
        observed_send.provider_id = registry
            .provider_for_capability(&descriptor.wire.capability_id)
            .unwrap()
            .wire
            .provider_id
            .clone();
        observed_send.capability_id = descriptor.wire.capability_id.clone();
        observed_send.tool_schema_version = descriptor.wire.input_schema_version;
        let mut candidate = contract.contract().clone();
        candidate.permissions.clear();
        candidate.steps.clear();
        draft::observed_rule(
            &mut candidate,
            &observed_send,
            None,
            Some(draft::ObservedMessage::Sent(&sent)),
            &registry,
        )
        .unwrap();
        let TaskStepBinding::SendMessage {
            destination: generated,
            ..
        } = &candidate.steps[0].binding
        else {
            panic!("send binding")
        };
        assert_eq!(generated.account_id, surface.account_id);
        assert_eq!(generated.recipients, snapshot.payload.recipients);
        assert!(matches!(
            candidate.permissions[0].input,
            TaskInputConstraint::GeneratedMessage { .. }
        ));
        let generated_contract = validate_contract(&candidate).unwrap();
        let generated_sources = ResolvedTaskSources {
            scopes: vec![crate::schedule::rehearsal::fixed_input::task_input_scope(
                &generated_contract,
            )],
            root_envelope_ids: vec!["fixed-input".into()],
        };
        assert!(
            generated_contract
                .observed_generated_message_step(
                    &target,
                    "run-1",
                    &observed_send,
                    &sent,
                    &generated_sources
                )
                .is_some()
        );
        let prepared = crate::communication_handoff::PreparedWebMessageEvidence {
            snapshot: sent.snapshot.clone(),
            subject: sent.subject.clone(),
            body_plain_text: sent.body_plain_text.clone(),
        };
        let preparation = registry
            .capability_for_tool("prepare_gmail_web_draft_handoff")
            .unwrap();
        let mut observed_preparation = observed_send.clone();
        observed_preparation.effect = CapabilityEffect::WriteExternalDraft;
        observed_preparation.tool_name = "prepare_gmail_web_draft_handoff".into();
        observed_preparation.provider_id = registry
            .provider_for_capability(&preparation.wire.capability_id)
            .unwrap()
            .wire
            .provider_id
            .clone();
        observed_preparation.capability_id = preparation.wire.capability_id.clone();
        observed_preparation.tool_schema_version = preparation.wire.input_schema_version;
        let mut preparation_contract = candidate.clone();
        preparation_contract.permissions.clear();
        preparation_contract.steps.clear();
        draft::observed_rule(
            &mut preparation_contract,
            &observed_preparation,
            None,
            Some(draft::ObservedMessage::Prepared(&prepared)),
            &registry,
        )
        .unwrap();
        let preparation_contract = validate_contract(&preparation_contract).unwrap();
        assert!(
            preparation_contract
                .observed_generated_draft_step(
                    &target,
                    "run-1",
                    &observed_preparation,
                    &prepared,
                    &generated_sources
                )
                .is_some()
        );
        assert!(
            preparation_contract
                .observed_generated_draft_step(
                    &target,
                    "run-1",
                    &observed_send,
                    &prepared,
                    &generated_sources
                )
                .is_none()
        );
        assert!(
            preparation_contract
                .observed_generated_message_step(
                    &target,
                    "run-1",
                    &observed_preparation,
                    &sent,
                    &generated_sources
                )
                .is_none()
        );
        assert!(
            generated_contract
                .observed_generated_draft_step(
                    &target,
                    "run-1",
                    &observed_send,
                    &prepared,
                    &generated_sources
                )
                .is_none()
        );
        let mut changed = prepared.clone();
        changed.body_plain_text.push_str("replacement");
        assert!(
            preparation_contract
                .observed_generated_draft_step(
                    &target,
                    "run-1",
                    &observed_preparation,
                    &changed,
                    &generated_sources
                )
                .is_none()
        );
        let sources = ResolvedTaskSources {
            scopes: vec!["device:1".into()],
            root_envelope_ids: vec!["original-read".into()],
        };
        assert_eq!(
            contract
                .observed_generated_message_step(&target, "run-1", &authority, &sent, &sources)
                .map(|step| step.step_id.as_str()),
            Some("send-report")
        );
        for case in 0..5 {
            let mut changed = sent.clone();
            match case {
                0 => changed.receipt.outcome = SendOutcome::OutcomeUnknown,
                1 => changed.receipt.snapshot_id = "another-snapshot".into(),
                2 => changed.receipt.observed_at_unix_ms = 999,
                3 => changed.body_plain_text.push_str("changed"),
                _ => {
                    changed.snapshot.payload.recipients[0].canonical_address =
                        "other@example.com".into()
                }
            }
            assert!(
                contract
                    .observed_generated_message_step(
                        &target, "run-1", &authority, &changed, &sources
                    )
                    .is_none()
            );
        }
        let mut extra = sources.clone();
        extra.scopes.push("device:other".into());
        assert!(
            contract
                .observed_generated_message_step(&target, "run-1", &authority, &sent, &extra)
                .is_none()
        );
        let mut wrong = authority.clone();
        wrong.provider_id = "another-provider".into();
        assert!(
            contract
                .observed_generated_message_step(&target, "run-1", &wrong, &sent, &sources)
                .is_none()
        );
        assert!(
            contract
                .observed_generated_message_step(&target, "other-run", &authority, &sent, &sources)
                .is_none()
        );
    }

    assert_eq!(
        contract.verify_generated_send_snapshot(
            &target,
            "run-1",
            "send-report",
            &snapshot,
            &surface,
            &message
        ),
        Ok(destination)
    );
    assert!(
        contract
            .verify_generated_send_snapshot(
                &target,
                "run-2",
                "send-report",
                &snapshot,
                &surface,
                &message
            )
            .is_err()
    );
    assert!(
        contract
            .verify_generated_send_snapshot(
                &target, "run-1", "missing", &snapshot, &surface, &message
            )
            .is_err()
    );
    let mut current = surface.clone();
    current.revision += 1;
    assert!(
        contract
            .verify_generated_send_snapshot(
                &target,
                "run-1",
                "send-report",
                &snapshot,
                &current,
                &message
            )
            .is_err()
    );
    // A new occurrence gets a different session/revision, payload and snapshot.
    // Only its freshly verified destination yields the same task-level scope.
    let mut next_payload = payload.clone();
    next_payload.surface.os_session_id = "next-session".into();
    next_payload.surface.revision += 1;
    let next_surface = next_payload.surface.clone();
    let next_snapshot = crate::communication::seal_send_payload(
        "next-snapshot".into(),
        "run-2".into(),
        next_payload,
        2000,
    )
    .unwrap();
    let verified_destination = contract
        .verify_generated_send_snapshot(
            &target,
            "run-2",
            "send-report",
            &next_snapshot,
            &next_surface,
            &message,
        )
        .unwrap();
    assert_eq!(
        task_message_resource_scope(target.task_device_id, verified_destination).unwrap(),
        contract.contract().permissions[0].automatic.resources,
    );
    assert_ne!(
        next_snapshot.canonical_payload_sha256,
        snapshot.canonical_payload_sha256
    );
    for mismatch in [
        "account",
        "profile",
        "adapter",
        "recipient",
        "subject",
        "body",
    ] {
        let mut changed = payload.clone();
        match mismatch {
            "account" => changed.surface.account_id = "other-account".into(),
            "profile" => changed.surface.profile_id = "other-profile".into(),
            "adapter" => changed.surface.adapter_version = "2".into(),
            "recipient" => changed.recipients[0].stable_id = "other-recipient".into(),
            "subject" => changed.subject = "Changed".into(),
            "body" => {
                changed.body.digest_sha256 = "a".repeat(64);
                let ContentRef::ImmutableBlob { sha256, .. } = &mut changed.body.content else {
                    unreachable!()
                };
                *sha256 = "a".repeat(64);
            }
            _ => unreachable!(),
        }
        let current = changed.surface.clone();
        let sealed = seal(changed);
        assert!(
            contract
                .verify_generated_send_snapshot(
                    &target,
                    "run-1",
                    "send-report",
                    &sealed,
                    &current,
                    &message
                )
                .is_err(),
            "{mismatch}"
        );
    }
    let mut corrupt = snapshot.clone();
    corrupt.canonical_payload_sha256 = "f".repeat(64);
    assert!(
        contract
            .verify_generated_send_snapshot(
                &target,
                "run-1",
                "send-report",
                &corrupt,
                &surface,
                &message
            )
            .is_err()
    );
    let mut mapped_definition = message_contract();
    mapped_definition.target_device_id = "manager-record-11".into();
    refresh_message_scope(&mut mapped_definition);
    let mapped = validate_contract(&mapped_definition).unwrap();
    let manager_target = TaskMessageTarget {
        task_device_id: "manager-record-11",
        provider_device_id: "device-1",
    };
    assert!(
        mapped
            .verify_generated_send_snapshot(
                &manager_target,
                "run-1",
                "send-report",
                &snapshot,
                &surface,
                &message
            )
            .is_ok()
    );
    let wrong_mapping = TaskMessageTarget {
        task_device_id: "manager-record-11",
        provider_device_id: "another-provider-device",
    };
    assert!(
        mapped
            .verify_generated_send_snapshot(
                &wrong_mapping,
                "run-1",
                "send-report",
                &snapshot,
                &surface,
                &message
            )
            .is_err()
    );
}

#[test]
fn fresh_task_sessions_have_independent_identity_and_no_rehearsal_state() {
    use crate::schedule::fresh_session::{FreshSessionInput, initial_session};
    use crate::{chat::ChatRole, session::TriggerOrigin};
    use desk_agent_protocol::{AgentScope, ExecutionMode};
    let prompt = "Send the daily report";
    let mut definition = contract();
    definition.prompt_sha256 = format!("{:x}", Sha256::digest(prompt.as_bytes()));
    let definition = validate_contract(&definition).unwrap();
    let make = |run, text| {
        initial_session(
            &definition,
            FreshSessionInput {
                run_id: run,
                actor_id: "1",
                prompt: text,
                locale: Some("zh-CN"),
                policy_revision: 1,
                scope: AgentScope {
                    granted: vec![],
                    mode: ExecutionMode::SuggestOnly,
                    expires_at: None,
                    policy_name: None,
                },
                now: "2026-09-06T06:00:00Z",
            },
        )
    };
    let first = make("schedule-run-first", prompt).unwrap();
    let second = make("schedule-run-second", prompt).unwrap();
    assert_ne!(first.conversation_id, second.conversation_id);
    assert_ne!(first.client_conversation_id, second.client_conversation_id);
    assert_ne!(first.current_turn_id, second.current_turn_id);
    assert_ne!(
        first.conversation[0].message_id,
        second.conversation[0].message_id
    );
    for session in [first, second] {
        assert_eq!(session.trigger_origin, TriggerOrigin::ScheduledTask);
        assert_eq!(session.conversation.len(), 1);
        assert_eq!(session.conversation[0].role, ChatRole::User);
        assert_eq!(session.conversation[0].text.as_str(), prompt);
        assert_eq!(session.input_revision, 1);
        assert_eq!(session.latest_input_seq, 1);
        assert_eq!(session.handled_input_seq, 0);
        assert_eq!(
            session.client_conversation_id,
            Some(format!(
                "task_{:x}",
                Sha256::digest(session.conversation_id.as_bytes())
            ))
        );
        assert!(session.active_control_connection_id.is_none());
        assert!(session.permission_requests.is_empty());
        assert!(session.context_attachments.is_empty());
        assert!(session.scope_snapshot.granted.is_empty());
        assert_eq!(session.automation_turns_used, 0);
    }
    assert!(make("rehearsal-old", prompt).is_err());
    assert!(make("schedule-run-first", "Changed task").is_err());
}

#[test]
fn generated_message_contract_requires_stable_destination_scope() {
    let definition = message_contract();
    assert!(validate_contract(&definition).is_ok());
    let TaskStepBinding::SendMessage { destination, .. } = &definition.steps[0].binding else {
        unreachable!()
    };
    let original = task_message_resource_scope("device-1", destination).unwrap();
    assert_eq!(original, definition.permissions[0].automatic.resources);
    assert_ne!(
        original,
        task_message_resource_scope("other-device", destination).unwrap()
    );
    assert!(task_message_resource_scope("", destination).is_err());
    for account_id in [
        crate::device_assistant::GMAIL_WEB_CURRENT_PROFILE_ACCOUNT_ID,
        crate::device_assistant::SLACK_WEB_CURRENT_PROFILE_ACCOUNT_ID,
        crate::device_assistant::OUTLOOK_NEW_UNVERIFIED_ACCOUNT_ID,
    ] {
        let mut unverified = destination.clone();
        unverified.account_id = account_id.into();
        assert!(task_message_resource_scope("device-1", &unverified).is_err());
    }

    for case in 0..5 {
        let mut changed = destination.clone();
        match case {
            0 => changed.account_id = "other-account".into(),
            1 => changed.profile_id = "other-profile".into(),
            2 => changed.adapter_version = "2".into(),
            3 => changed.recipients[0].stable_id = "other-recipient".into(),
            _ => changed.recipients[0].canonical_address = "other@example.com".into(),
        }
        assert_ne!(
            original,
            task_message_resource_scope("device-1", &changed).unwrap()
        );
        let mut stale_scope = definition.clone();
        let TaskStepBinding::SendMessage { destination, .. } = &mut stale_scope.steps[0].binding
        else {
            unreachable!()
        };
        *destination = changed;
        assert!(validate_contract(&stale_scope).is_err());
    }
    for scope in [
        vec!["selected:sha256:temporary-page".into()],
        vec!["device:1".into()],
        vec![],
    ] {
        let mut bad = definition.clone();
        bad.permissions[0].automatic.resources = scope.clone();
        bad.permissions[0].approval_ceiling.resources = scope;
        assert!(validate_contract(&bad).is_err());
    }
    let mut widened = definition;
    widened.permissions[0]
        .approval_ceiling
        .resources
        .push("other-recipient".into());
    assert!(validate_contract(&widened).is_err());
}

#[test]
fn fixed_task_input_is_verified_and_scoped_to_the_task_requirement() {
    use crate::{
        model_message_labels::model_bound_user_message,
        schedule::rehearsal::fixed_input::{fixed_task_input_source, task_input_scope},
        session::PersistedAgentSession,
    };
    let mut definition = contract();
    definition.prompt_sha256 = format!("{:x}", Sha256::digest(b"fixed requirement"));
    let contract = validate_contract(&definition).unwrap();
    let mut session = PersistedAgentSession::new(
        "conversation",
        "1",
        "device-1",
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-09-07T00:00:00Z",
    );
    session.conversation.push(
        model_bound_user_message(
            "input".into(),
            "fixed requirement".into(),
            DestinationIdentity::Model {
                connection_id: "model-provider".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap(),
    );
    let (node, binding) = fixed_task_input_source(&session, "input", &contract).unwrap();
    assert!(node.source_envelope_ids.is_empty());
    assert_eq!(
        binding.authority,
        crate::schedule::source_graph::TaskSourceAuthority::Scopes(vec![task_input_scope(
            &contract
        )])
    );
    let mut other = definition.clone();
    other.task_revision += 1;
    assert_ne!(
        task_input_scope(&contract),
        task_input_scope(&validate_contract(&other).unwrap())
    );
    other = definition.clone();
    other.schedule_id = "other-task".into();
    assert_ne!(
        task_input_scope(&contract),
        task_input_scope(&validate_contract(&other).unwrap())
    );
    let mut changed = session.clone();
    changed.conversation[0].text.push_str(" changed");
    assert!(fixed_task_input_source(&changed, "input", &contract).is_err());
    changed = session.clone();
    changed.device_id = "other-device".into();
    assert!(fixed_task_input_source(&changed, "input", &contract).is_err());
    changed = session.clone();
    changed.conversation[0]
        .data_envelope
        .as_mut()
        .unwrap()
        .provenance
        .source_envelope_ids
        .push("unproved-source".into());
    assert!(fixed_task_input_source(&changed, "input", &contract).is_err());
    changed = session.clone();
    changed.conversation.push(session.conversation[0].clone());
    assert!(fixed_task_input_source(&changed, "input", &contract).is_err());
}

#[test]
fn rehearsal_graph_requires_each_original_read_even_when_it_has_known_parents() {
    use crate::{
        chat::{ChatMessage, ChatRole, ModelTurn, StopReason, ToolCall},
        model_egress::{ModelEgressPolicy, ModelInputLineage, project_model_input_lineage},
        model_message_labels::{ReadResultLabel, model_bound_user_message, read_result_envelope},
        schedule::{
            rehearsal::{
                fixed_input::task_input_scope,
                read_sources::RehearsalToolSource,
                sources::{RehearsalModelCall, resolve_rehearsal_model_sources},
            },
            source_graph::{TaskSourceAuthority, TaskSourceBinding},
        },
        seam::{ModelRequest, ToolRunOutput},
        session::PersistedAgentSession,
    };
    let mut definition = contract();
    definition.prompt_sha256 = format!("{:x}", Sha256::digest(b"fixed requirement"));
    let contract = validate_contract(&definition).unwrap();
    let destination = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    let user = model_bound_user_message(
        "input".into(),
        "fixed requirement".into(),
        destination.clone(),
    )
    .unwrap();
    let call = ToolCall {
        id: "read-call".into(),
        name: "read_system_info".into(),
        arguments_json: "{}".into(),
    };
    let out = ToolRunOutput {
        content: "system report".into(),
        image_data_url: None,
    };
    let mut label = read_result_envelope(
        &crate::device_assistant::device_assistant_provider_registry(),
        &call,
        &out,
        ReadResultLabel {
            envelope_id: "read-source".into(),
            observation_id: "read-observation".into(),
            source_object_id: None,
            observed_at_unix_ms: 100,
        },
    )
    .unwrap();
    label
        .provenance
        .source_envelope_ids
        .push(user.data_envelope.as_ref().unwrap().envelope_id.clone());
    // This fixture starts from an already authorized original input label.
    label.allowed_destinations = vec![destination.clone()];
    let read = RehearsalToolSource {
        lineage: ModelInputLineage {
            envelope_id: label.envelope_id.clone(),
            digest_sha256: label.digest_sha256.clone(),
            source_provider_id: label.provenance.source_provider_id.clone(),
            source_tool_name: label.provenance.source_tool_name.clone(),
            source_envelope_ids: label.provenance.source_envelope_ids.clone(),
            public_system_prompt: false,
        },
        authority: TaskSourceBinding {
            envelope_id: label.envelope_id.clone(),
            digest_sha256: label.digest_sha256.clone(),
            authority: TaskSourceAuthority::Scopes(vec!["device:1".into()]),
        },
    };
    let mut tool = ChatMessage::tool_result("read-result", &call.id, &out.content);
    tool.data_envelope = Some(label);
    let policy = ModelEgressPolicy {
        destination,
        selected_source_tools: [call.name].into_iter().collect(),
        export_authorization_id: "original-export".into(),
        now_unix_ms: 100,
        byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    };
    let authorized = policy
        .authorize_request(ModelRequest::text_only(
            vec![
                ChatMessage::text("system", ChatRole::System, "public instructions"),
                user.clone(),
                tool.clone(),
            ],
            crate::prompt::ResponseFormatSpec::None,
        ))
        .unwrap();
    let turn = ModelTurn {
        text: "summary".into(),
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    };
    let mut answer = ChatMessage::text("answer", ChatRole::Assistant, &turn.text);
    answer.data_envelope = Some(
        policy
            .derive_model_output_envelope(&turn, &authorized.input_envelopes)
            .unwrap(),
    );
    let mut session = PersistedAgentSession::new(
        "conversation",
        "1",
        "device-1",
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-09-07T00:00:00Z",
    );
    session.conversation = vec![user, tool, answer];
    let calls = vec![RehearsalModelCall {
        output_message_id: "answer".into(),
        export_authorization_id: policy.export_authorization_id.clone(),
        inputs: project_model_input_lineage(&authorized.audit, &authorized.input_envelopes)
            .unwrap(),
    }];
    assert!(
        resolve_rehearsal_model_sources(&session, "input", &contract, &[], &calls, &[], "answer")
            .is_err()
    );
    let reads = vec![read];
    let proof = resolve_rehearsal_model_sources(
        &session,
        "input",
        &contract,
        &reads,
        &calls,
        &[],
        "answer",
    )
    .unwrap();
    assert_eq!(
        proof.scopes,
        vec!["device:1".to_string(), task_input_scope(&contract)]
    );
    session.conversation[1]
        .data_envelope
        .as_mut()
        .unwrap()
        .allowed_destinations
        .clear();
    let exported = policy
        .authorize_request(ModelRequest::text_only(
            vec![
                ChatMessage::text("system", ChatRole::System, "public instructions"),
                session.conversation[0].clone(),
                session.conversation[1].clone(),
            ],
            crate::prompt::ResponseFormatSpec::None,
        ))
        .unwrap();
    session.conversation[2].data_envelope = Some(
        policy
            .derive_model_output_envelope(&turn, &exported.input_envelopes)
            .unwrap(),
    );
    let mut exported_calls = vec![RehearsalModelCall {
        output_message_id: "answer".into(),
        export_authorization_id: policy.export_authorization_id.clone(),
        inputs: project_model_input_lineage(&exported.audit, &exported.input_envelopes).unwrap(),
    }];
    assert_eq!(
        resolve_rehearsal_model_sources(
            &session,
            "input",
            &contract,
            &reads,
            &exported_calls,
            &[],
            "answer"
        )
        .unwrap()
        .scopes,
        proof.scopes
    );
    exported_calls[0].export_authorization_id = "wrong-export-identity".into();
    assert!(
        resolve_rehearsal_model_sources(
            &session,
            "input",
            &contract,
            &reads,
            &exported_calls,
            &[],
            "answer"
        )
        .is_err()
    );
}

#[test]
fn compressed_answer_preserves_original_read_scope() {
    use crate::{
        chat::{ChatMessage, ChatRole, ModelTurn, StopReason, ToolCall},
        model_context::*,
        model_egress::{ModelEgressPolicy, project_model_input_lineage},
        model_message_labels::{ReadResultLabel, model_bound_user_message, read_result_envelope},
        prompt::ResponseFormatSpec,
        schedule::{
            rehearsal::{
                fixed_input::task_input_scope, read_sources::RehearsalToolSource, sources::*,
            },
            source_graph::*,
        },
        seam::{ModelRequest, ToolRunOutput},
        session::PersistedAgentSession,
    };
    let mut definition = contract();
    let prompt = "x".repeat(16000);
    definition.prompt_sha256 = format!("{:x}", Sha256::digest(prompt.as_bytes()));
    let contract = validate_contract(&definition).unwrap();
    let policy = ModelEgressPolicy {
        destination: DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
        selected_source_tools: ["read_system_info".into()].into_iter().collect(),
        export_authorization_id: "original-export".into(),
        now_unix_ms: 100,
        byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    };
    let user = model_bound_user_message("input".into(), prompt, policy.destination.clone())
        .unwrap()
        .with_turn_id("first");
    let tool_call = ToolCall {
        id: "read-call".into(),
        name: "read_system_info".into(),
        arguments_json: "{}".into(),
    };
    let projected = policy
        .authorize_request(ModelRequest::text_only(
            vec![user.clone()],
            ResponseFormatSpec::None,
        ))
        .unwrap();
    let proposal_turn = ModelTurn {
        tool_calls: vec![tool_call.clone()],
        stop_reason: StopReason::ToolUse,
        ..Default::default()
    };
    let mut proposal = ChatMessage::assistant_tool_calls("proposal", "", vec![tool_call.to_ref()])
        .with_turn_id("first");
    proposal.data_envelope = Some(
        policy
            .derive_model_output_envelope(&proposal_turn, &projected.input_envelopes)
            .unwrap(),
    );
    let mut calls = vec![RehearsalModelCall {
        output_message_id: proposal.message_id.clone(),
        export_authorization_id: policy.export_authorization_id.clone(),
        inputs: project_model_input_lineage(&projected.audit, &projected.input_envelopes).unwrap(),
    }];
    let out = ToolRunOutput {
        content: "r".repeat(1000),
        image_data_url: None,
    };
    let label = read_result_envelope(
        &crate::device_assistant::device_assistant_provider_registry(),
        &tool_call,
        &out,
        ReadResultLabel {
            envelope_id: "original-read".into(),
            observation_id: "observation".into(),
            source_object_id: None,
            observed_at_unix_ms: 100,
        },
    )
    .unwrap();
    let read = RehearsalToolSource {
        lineage: crate::model_egress::ModelInputLineage {
            envelope_id: label.envelope_id.clone(),
            digest_sha256: label.digest_sha256.clone(),
            source_provider_id: label.provenance.source_provider_id.clone(),
            source_tool_name: label.provenance.source_tool_name.clone(),
            source_envelope_ids: label.provenance.source_envelope_ids.clone(),
            public_system_prompt: false,
        },
        authority: TaskSourceBinding {
            envelope_id: label.envelope_id.clone(),
            digest_sha256: label.digest_sha256.clone(),
            authority: TaskSourceAuthority::Scopes(vec!["device:1".into()]),
        },
    };
    let mut result =
        ChatMessage::tool_result("read-result", &tool_call.id, &out.content).with_turn_id("first");
    result.data_envelope = Some(label);
    let mut session = PersistedAgentSession::new(
        "conversation",
        "1",
        &definition.target_device_id,
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-09-07T00:00:00Z",
    );
    session.conversation = vec![user, proposal, result];
    let context = PinnedContextPolicy::checkpoint_summary(
        crate::replay::SourceContextKey::derive(
            crate::model_profile::WireProtocol::OpenAiChatCompletions,
            "gateway",
            "model",
            "test",
        ),
        1,
        crate::MIN_MODEL_CONTEXT_BYTES * 5,
        1,
    )
    .unwrap();
    let ContextBuildPlan::NeedsCompression(plan) = plan_model_context(
        &session.conversation,
        &session.model_context_state,
        &context,
        &ContextProtectionSet::default(),
        7,
    )
    .unwrap() else {
        panic!("compression expected");
    };
    let input = authorize_compression_input(&policy, &plan, &session.conversation).unwrap();
    let projected = policy
        .authorize_request(ModelRequest::text_only(
            input.messages.clone(),
            ResponseFormatSpec::None,
        ))
        .unwrap();
    let mut summary_turn = ModelTurn {
        text: r#"{"goals":[{"text":"Summarized request","source_message_ids":["input"]}]}"#.into(),
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    };
    summary_turn.provider_meta.data_envelope = Some(
        policy
            .derive_model_output_envelope(&summary_turn, &projected.input_envelopes)
            .unwrap(),
    );
    let provenance = CompressorProvenanceV1::for_call(
        &context,
        "a".repeat(64),
        "b".repeat(64),
        1,
        "c".repeat(64),
        "2026-09-07T00:00:00Z",
        "first",
    );
    let mut summary =
        parse_validated_context_summary(&summary_turn.text, &plan, provenance).unwrap();
    bind_context_summary_lineage(&policy, &mut summary, &summary_turn, &input).unwrap();
    let (next, view) = apply_validated_checkpoint(
        &plan,
        summary,
        &session.conversation,
        &session.model_context_state,
        7,
    )
    .unwrap();
    session.model_context_state = next;
    let compressions = vec![RehearsalCompressionCall {
        trace: retained_rehearsal_compressions(&session).unwrap().remove(0),
        inputs: project_model_input_lineage(&projected.audit, &projected.input_envelopes).unwrap(),
    }];
    // The answer uses only the summary. Its upstream continuation lens still
    // contains the original read and must preserve that resource scope.
    let projected = policy
        .authorize_request(ModelRequest::text_only(
            vec![view.messages[0].clone()],
            ResponseFormatSpec::None,
        ))
        .unwrap();
    let turn = ModelTurn {
        text: "Final answer".into(),
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    };
    let mut answer =
        ChatMessage::text("answer", ChatRole::Assistant, &turn.text).with_turn_id("first");
    answer.data_envelope = Some(
        policy
            .derive_model_output_envelope(&turn, &projected.input_envelopes)
            .unwrap(),
    );
    session.conversation.push(answer);
    calls.push(RehearsalModelCall {
        output_message_id: "answer".into(),
        export_authorization_id: policy.export_authorization_id.clone(),
        inputs: project_model_input_lineage(&projected.audit, &projected.input_envelopes).unwrap(),
    });
    let reads = vec![read];
    let proof = resolve_rehearsal_model_sources(
        &session,
        "input",
        &contract,
        &reads,
        &calls,
        &compressions,
        "answer",
    )
    .unwrap();
    let mut expected = vec!["device:1".to_owned(), task_input_scope(&contract)];
    expected.sort();
    assert_eq!(proof.scopes, expected);
    assert!(
        resolve_rehearsal_model_sources(
            &session,
            "input",
            &contract,
            &[],
            &calls,
            &compressions,
            "answer"
        )
        .is_err()
    );
    assert!(
        resolve_rehearsal_model_sources(
            &session,
            "input",
            &contract,
            &reads,
            &calls,
            &[],
            "answer"
        )
        .is_err()
    );
    // A retained checkpoint does not authorize substituting another model
    // operation's receipt, even when the summary and its parents are identical.
    let mut foreign = compressions.clone();
    foreign[0].trace.export_authorization_id = "another-run-export".into();
    assert!(
        build_rehearsal_source_graph(&session, "input", &contract, &reads, &calls, &foreign,)
            .is_err()
    );
    let mut duplicate = compressions.clone();
    duplicate.push(compressions[0].clone());
    assert!(
        build_rehearsal_source_graph(&session, "input", &contract, &reads, &calls, &duplicate,)
            .is_err()
    );
    let mut wrong = compressions.clone();
    wrong[0].inputs.clear();
    assert!(
        resolve_rehearsal_model_sources(
            &session, "input", &contract, &reads, &calls, &wrong, "answer"
        )
        .is_err()
    );
}

#[test]
fn publication_catalog_rechecks_current_identity_capability_and_limits() {
    use crate::provider_registry::ProviderRegistryBuilder;
    use desk_agent_protocol::capability_provider::ProductSurface;
    let registry = crate::device_assistant::device_assistant_provider_registry();
    let capability = registry.capability_for_tool("read_system_info").unwrap();
    let provider = registry
        .provider_for_capability(&capability.wire.capability_id)
        .unwrap();
    let mut input = contract();
    let rule = &mut input.permissions[0];
    rule.provider_id = provider.wire.provider_id.clone();
    rule.capability_id = capability.wire.capability_id.clone();
    rule.tool_name = capability.wire.tool_name.clone();
    rule.tool_schema_version = capability.wire.input_schema_version;
    rule.effect = capability.wire.effect;
    rule.risk_tier = crate::capability_risk::classify_provider_descriptor_floor(
        rule.effect,
        &capability.wire.data_policy,
    );
    rule.automatic.limits.max_items_per_call = 1;
    rule.approval_ceiling.limits.max_items_per_call = 1;
    let allowed = [capability.required_capability];
    let checked = validate_contract(&input).unwrap();
    for surface in [
        ProductSurface::OssPersonalOwner,
        ProductSurface::ManagerPersonalOwner,
    ] {
        checked
            .validate_current_catalog(&registry, surface, &allowed)
            .unwrap();
        assert_eq!(
            checked.validate_current_catalog(&registry, surface, &[]),
            Err(TaskCatalogError::Unavailable)
        );
        assert_eq!(
            checked.validate_current_catalog(
                &ProviderRegistryBuilder::new().build().unwrap(),
                surface,
                &allowed
            ),
            Err(TaskCatalogError::Unavailable)
        );
    }
    let surface = ProductSurface::ManagerPersonalOwner;
    for field in [0, 1, 2] {
        let mut changed = input.clone();
        match field {
            0 => changed.permissions[0].provider_id = "other-provider".into(),
            1 => changed.permissions[0].capability_id = "other-capability".into(),
            _ => changed.permissions[0].tool_schema_version += 1,
        }
        assert_eq!(
            validate_contract(&changed)
                .unwrap()
                .validate_current_catalog(&registry, surface, &allowed),
            Err(TaskCatalogError::Identity)
        );
    }
    let mut changed = input.clone();
    changed.permissions[0]
        .approval_ceiling
        .limits
        .max_bytes_per_call = capability.wire.limits.max_input_bytes + 1;
    assert_eq!(
        validate_contract(&changed)
            .unwrap()
            .validate_current_catalog(&registry, surface, &allowed),
        Err(TaskCatalogError::Limits)
    );
    let browser = registry.capability_for_tool("browser_wait_for").unwrap();
    let mut understated = input.clone();
    let rule = &mut understated.permissions[0];
    rule.provider_id = registry
        .provider_for_capability(&browser.wire.capability_id)
        .unwrap()
        .wire
        .provider_id
        .clone();
    rule.capability_id = browser.wire.capability_id.clone();
    rule.tool_name = browser.wire.tool_name.clone();
    rule.effect = browser.wire.effect;
    rule.tool_schema_version = browser.wire.input_schema_version;
    rule.risk_tier = CapabilityRiskTier::R0;
    assert_eq!(
        validate_contract(&understated)
            .unwrap()
            .validate_current_catalog(&registry, surface, &[browser.required_capability]),
        Err(TaskCatalogError::Risk)
    );
    let mut narrowed = provider.clone();
    for cap in &mut narrowed.capabilities {
        cap.wire.surfaces = vec![ProductSurface::OssPersonalOwner];
    }
    narrowed.wire.capabilities = narrowed
        .capabilities
        .iter()
        .map(|cap| cap.wire.clone())
        .collect();
    let restricted = ProviderRegistryBuilder::new()
        .register(narrowed)
        .build()
        .unwrap();
    assert_eq!(
        checked.validate_current_catalog(&restricted, surface, &allowed),
        Err(TaskCatalogError::Unavailable)
    );
}

#[test]
fn attachment_policy_is_validated_and_bound_to_contract_digest() {
    let mut candidate = message_contract();
    let before = validate_contract(&candidate).unwrap();
    let original = serde_json::to_value(before.contract()).unwrap();
    let limits = TaskAttachmentLimits {
        max_count: 1,
        max_bytes_per_attachment: 1024,
        max_total_bytes: 1024,
        media_types: vec!["text/plain".into()],
    };
    if let TaskInputConstraint::GeneratedMessage {
        attachment_policy, ..
    } = &mut candidate.permissions[0].input
    {
        *attachment_policy = Some(TaskAttachmentPolicy {
            automatic: limits.clone(),
            approval_ceiling: limits,
        });
    }
    let with_attachment = validate_contract(&candidate).unwrap();
    let updated = serde_json::to_value(with_attachment.contract()).unwrap();
    assert_ne!(canonical(original).unwrap(), canonical(updated).unwrap());
    if let TaskInputConstraint::GeneratedMessage {
        attachment_policy: Some(policy),
        ..
    } = &mut candidate.permissions[0].input
    {
        policy.automatic.max_total_bytes = 2048;
    }
    assert!(matches!(
        validate_contract(&candidate),
        Err(TaskContractError::InvalidLimits)
    ));
}

#[test]
fn step_attachment_checks_sources_before_returning_bounded_decision() {
    use super::attachment::TaskAttachmentDecision;
    use crate::model_egress::ModelInputLineage;
    use crate::schedule::source_graph::{
        TaskSourceAuthority, TaskSourceBinding, attachment::TaskAttachmentReceipt,
    };
    use desk_agent_protocol::{
        communication::ImmutableAttachmentSnapshot, data_lineage::ContentRef,
    };
    let mut candidate = message_contract();
    candidate.exception_mode = TaskExceptionMode::RequestApproval;
    let limit = |bytes| TaskAttachmentLimits {
        max_count: 1,
        max_bytes_per_attachment: bytes,
        max_total_bytes: bytes,
        media_types: vec!["text/plain".into()],
    };
    if let TaskInputConstraint::GeneratedMessage {
        attachment_policy, ..
    } = &mut candidate.permissions[0].input
    {
        *attachment_policy = Some(TaskAttachmentPolicy {
            automatic: limit(2),
            approval_ceiling: limit(8),
        });
    }
    let file = ImmutableAttachmentSnapshot {
        content: ContentRef::Artifact {
            artifact_id: "artifact".into(),
            sha256: "a".repeat(64),
            size_bytes: 4,
            media_type: "text/plain".into(),
        },
        file_name: "report.txt".into(),
        media_type: "text/plain".into(),
        size_bytes: 4,
        digest_sha256: "a".repeat(64),
    };
    let node = ModelInputLineage {
        public_system_prompt: false,
        envelope_id: "artifact".into(),
        digest_sha256: "a".repeat(64),
        source_provider_id: "device".into(),
        source_tool_name: "read".into(),
        source_envelope_ids: vec![],
    };
    let root = TaskSourceBinding {
        envelope_id: "artifact".into(),
        digest_sha256: "a".repeat(64),
        authority: TaskSourceAuthority::Scopes(vec!["device:1".into()]),
    };
    let receipt_digest = "a".repeat(64);
    let receipt = TaskAttachmentReceipt {
        run_id: "run",
        envelope_id: "artifact",
        receipt_digest_sha256: &receipt_digest,
        attachment: &file,
    };
    let evaluate = |contract: &ValidatedTaskContract, root: &TaskSourceBinding| {
        contract.evaluate_step_attachments(
            "run",
            "send-report",
            std::slice::from_ref(&file),
            std::slice::from_ref(&receipt),
            std::slice::from_ref(&node),
            std::slice::from_ref(root),
        )
    };
    let contract = validate_contract(&candidate).unwrap();
    assert_eq!(
        evaluate(&contract, &root).unwrap().decision,
        TaskAttachmentDecision::RequiresApproval
    );
    assert!(contract.attachment_approval_scope("send-report", std::slice::from_ref(&file)));
    assert!(!contract.attachment_approval_scope("unknown-step", std::slice::from_ref(&file)));
    let evidence = evaluate(&contract, &root).unwrap();
    assert!(evidence.matches(&contract, "run", "send-report", std::slice::from_ref(&file)));
    assert!(!evidence.matches(
        &contract,
        "other-run",
        "send-report",
        std::slice::from_ref(&file)
    ));
    assert!(!evidence.matches(&contract, "run", "other-step", std::slice::from_ref(&file)));
    let mut replaced = file.clone();
    replaced.file_name = "replaced.txt".into();
    assert!(!evidence.matches(&contract, "run", "send-report", &[replaced]));
    let mut changed_contract = candidate.clone();
    changed_contract.budget.max_runtime_seconds -= 1;
    assert!(!evidence.matches(
        &validate_contract(&changed_contract).unwrap(),
        "run",
        "send-report",
        std::slice::from_ref(&file)
    ));
    let mut outside = root.clone();
    outside.authority = TaskSourceAuthority::Scopes(vec!["device:other".into()]);
    assert_eq!(
        evaluate(&contract, &outside),
        Err(TaskContractError::InvalidScope)
    );
    candidate.exception_mode = TaskExceptionMode::Deny;
    assert_eq!(
        evaluate(&validate_contract(&candidate).unwrap(), &root)
            .unwrap()
            .decision,
        TaskAttachmentDecision::Denied
    );
    assert!(
        !validate_contract(&candidate)
            .unwrap()
            .attachment_approval_scope("send-report", std::slice::from_ref(&file))
    );
}

#[test]
fn slack_generated_draft_binds_verified_account_page_and_destination() {
    use desk_agent_protocol::communication::{
        RecipientIdentity, RecipientKind, SlackWebDraftHandoffInput,
    };
    let input: SlackWebDraftHandoffInput = serde_json::from_value(serde_json::json!({
        "schema_version": desk_agent_protocol::communication::COMMUNICATION_SCHEMA_VERSION,
        "page": {
            "schema_version": 1,
            "adapter": { "engine": "chrome_extension", "device_id": "edge-1", "os_session_id": "os-1",
                "browser_major_version": 140, "browser_version": "140.0", "adapter_id": "extension",
                "adapter_version": "1", "profile_incarnation": "profile-1", "connection_revision": 1 },
            "page_id": "page-1", "page_incarnation": "incarnation-1",
            "origin": { "kind": "https", "host_ascii": "app.slack.com", "port": 443 },
            "document_revision": 1, "url_sha256": "a".repeat(64), "observed_at_unix_ms": 100,
            "account_id": "slack-web:T123:U456"
        },
        "composer": { "page_id": "page-1", "page_incarnation": "incarnation-1", "document_revision": 1,
            "element_id": "composer-1", "role": "textbox", "accessible_name": "reports",
            "value": null, "element_revision": 1 },
        "body_plain_text": "Today's report"
    })).unwrap();
    input.validate().unwrap();
    let mut definition = message_contract();
    definition.permissions[0].effect = CapabilityEffect::WriteExternalDraft;
    definition.permissions[0].tool_name = "prepare_slack_web_message_handoff".into();
    let TaskStepBinding::SendMessage { destination, .. } = &mut definition.steps[0].binding else {
        unreachable!()
    };
    destination.channel = CommunicationChannel::Chat;
    destination.scope = CommunicationSurfaceScope::WebOrigin {
        origin: input.page.origin.clone(),
    };
    destination.adapter_id = crate::device_assistant::SLACK_WEB_ADAPTER_ID.into();
    destination.adapter_version = crate::device_assistant::SLACK_WEB_ADAPTER_VERSION.into();
    destination.account_id = input.page.account_id.clone().unwrap();
    destination.recipients = vec![RecipientIdentity {
        role: RecipientRole::ChatDestination,
        kind: RecipientKind::ChatChannel,
        stable_id: format!(
            "slack-destination-{:x}",
            Sha256::digest(b"profile-1:reports")
        ),
        canonical_address: "reports".into(),
        display_name: None,
        display_warnings: vec![],
        resolved_members: vec![],
        member_snapshot_sha256: None,
    }];
    let export = DestinationIdentity::ChatAccount {
        account_id: destination.account_id.clone(),
    };
    definition.permissions[0].automatic.export_destinations = vec![export.clone()];
    definition.permissions[0]
        .approval_ceiling
        .export_destinations = vec![export];
    refresh_message_scope(&mut definition);
    let contract = validate_contract(&definition).unwrap();
    let target = TaskMessageTarget {
        task_device_id: &definition.target_device_id,
        provider_device_id: "edge-1",
    };
    assert!(
        contract
            .verify_generated_slack_draft(&target, "send-report", &input, &input.page)
            .is_ok()
    );
    let mut changed = input.clone();
    changed.composer.accessible_name = "another-channel".into();
    assert!(
        contract
            .verify_generated_slack_draft(&target, "send-report", &changed, &changed.page)
            .is_err()
    );
    changed = input.clone();
    changed.page.account_id = None;
    assert!(
        contract
            .verify_generated_slack_draft(&target, "send-report", &changed, &changed.page)
            .is_err()
    );
    changed.page.account_id =
        Some(crate::device_assistant::SLACK_WEB_CURRENT_PROFILE_ACCOUNT_ID.into());
    assert!(
        contract
            .verify_generated_slack_draft(&target, "send-report", &changed, &changed.page)
            .is_err()
    );
    let mut stale_page = input.page.clone();
    stale_page.observed_at_unix_ms += 1;
    assert!(
        contract
            .verify_generated_slack_draft(&target, "send-report", &input, &stale_page)
            .is_err()
    );
    definition.permissions[0].effect = CapabilityEffect::SendExternal;
    let send_contract = validate_contract(&definition).unwrap();
    assert!(
        send_contract
            .verify_generated_slack_draft(&target, "send-report", &input, &input.page)
            .is_err()
    );
}

#[test]
fn automatic_approval_candidate_keeps_exact_input_scope_and_bounded_lifetime() {
    use crate::{
        capability_grant::CapabilityGrantCall,
        chat::{ChatMessage, ChatRole, ToolCall},
        session::{PersistedAgentSession, TriggerOrigin},
    };
    let registry = crate::device_assistant::device_assistant_provider_registry();
    let tool_name = "inspect_desktop_session";
    let capability = registry.capability_for_tool(tool_name).unwrap();
    let provider = registry
        .provider_for_capability(&capability.wire.capability_id)
        .unwrap();
    let original = ToolCall {
        id: "inspect-1".into(),
        name: tool_name.into(),
        arguments_json: "{}".into(),
    };
    let planning = ToolCall {
        id: "planning".into(),
        name: crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
        arguments_json: serde_json::json!({"items":[{
            "item_id":"inspect-1", "provider_id":provider.wire.provider_id, "tool_name":tool_name,
            "expected_effect":capability.wire.effect, "resource_scope":["target:device"],
            "exact_input":{}, "suggested_ttl_seconds":60, "suggested_max_uses":1, "reason":"Inspect"
        }]})
        .to_string(),
    };
    let normalized = crate::permission_tools::build_permission_request(
        &planning,
        &registry,
        "normalized".into(),
        1,
        "2026-09-07T00:00:00Z".into(),
    )
    .unwrap();
    let item = &normalized.items[0];
    let mut definition = contract();
    let rule = &mut definition.permissions[0];
    rule.provider_id = provider.wire.provider_id.clone();
    rule.capability_id = capability.wire.capability_id.clone();
    rule.tool_name = tool_name.into();
    rule.tool_schema_version = capability.wire.input_schema_version;
    rule.effect = capability.wire.effect;
    for scope in [&mut rule.automatic, &mut rule.approval_ceiling] {
        scope.resources = item.resource_scope.clone();
        scope.operations = item.operation_scope.clone();
        scope.export_destinations = item.export_destinations.clone();
    }
    let contract = validate_contract(&definition).unwrap();
    let mut session = PersistedAgentSession::new(
        "run",
        "owner",
        &definition.target_device_id,
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-09-07T00:00:00Z",
    );
    session.input_revision = 1;
    session.trigger_origin = TriggerOrigin::ScheduledTask;
    let mut proposal = ChatMessage::text("proposal", ChatRole::Assistant, "");
    proposal.tool_calls.push(original.to_ref());
    session.conversation.push(proposal);
    let call = CapabilityGrantCall {
        actor_id: "owner",
        run_id: "run",
        input_revision: 1,
        surface: desk_agent_protocol::capability_provider::ProductSurface::ManagerPersonalOwner,
        target_device_id: &definition.target_device_id,
        target_session_id: None,
        provider_id: &provider.wire.provider_id,
        capability_id: &capability.wire.capability_id,
        tool_name,
        tool_schema_version: capability.wire.input_schema_version,
        effect: capability.wire.effect,
        risk_tier: definition.permissions[0].risk_tier,
        resource_scope: &item.resource_scope,
        operation_scope: &item.operation_scope,
        export_destinations: &item.export_destinations,
        envelope_ids: &[],
        content_digests_sha256: &[],
        canonical_input_digest_sha256: item.canonical_input_digest_sha256.as_deref().unwrap(),
        byte_count: 2,
        item_count: 1,
        policy_revision: 1,
        readiness_revision: 1,
        now_unix_ms: 1000,
    };
    let create = |until| {
        exception::request_for_original_call(
            &contract,
            &session,
            &original,
            &call,
            &registry,
            "edge",
            until,
            "1970-01-01T00:00:01Z".into(),
        )
    };
    let short = create(62000).unwrap();
    assert_eq!(short.items[0].suggested_ttl_seconds, 61);
    assert_eq!(short.items[0].suggested_max_uses, 1);
    assert_eq!(
        short.items[0].canonical_input_json,
        item.canonical_input_json
    );
    assert_eq!(short.items[0].resource_scope, item.resource_scope);
    let long = create(999000).unwrap();
    assert_eq!(long.items[0].suggested_ttl_seconds, 300);
    assert_eq!(short.request_id, long.request_id);
    assert!(create(1999).is_err());
    let oversized = CapabilityGrantCall {
        byte_count: 201,
        ..call.clone()
    };
    assert!(
        exception::request_for_original_call(
            &contract,
            &session,
            &original,
            &oversized,
            &registry,
            "edge",
            62000,
            "1970-01-01T00:00:01Z".into()
        )
        .is_err()
    );
    assert!(
        exception::request_for_original_call(
            &contract,
            &session,
            &original,
            &call,
            &registry,
            "edge",
            62000,
            "1970-01-01T00:00:00Z".into()
        )
        .is_err()
    );
}

#[test]
fn generated_text_artifact_contract_requires_safe_name_local_effect_and_sources() {
    let mut definition = contract();
    let resources =
        artifact::directory_resource_scope(&definition.target_device_id, "/reports").unwrap();
    definition.permissions[0].automatic.resources = resources.clone();
    definition.permissions[0].approval_ceiling.resources = resources;
    definition.permissions[0].effect = CapabilityEffect::WriteArtifact;
    definition.permissions[0].tool_name = "create_text_artifact_in_selected_directory".into();
    definition.permissions[0].input = TaskInputConstraint::GeneratedTextArtifact {
        file_name: "report.txt".into(),
        max_content_bytes: 65_536,
    };
    definition.steps = vec![TaskFixedStep {
        step_id: "artifact".into(),
        rule_id: "observe".into(),
        depends_on: vec![],
        binding: TaskStepBinding::ProduceTextArtifact {
            canonical_directory: "/reports".into(),
            allowed_source_scopes: vec!["device:1".into()],
        },
    }];
    assert!(validate_contract(&definition).is_ok());
    let mut wrong = definition.clone();
    wrong.permissions[0].input = TaskInputConstraint::GeneratedTextArtifact {
        file_name: "../report.txt".into(),
        max_content_bytes: 100,
    };
    assert!(validate_contract(&wrong).is_err());
    wrong = definition.clone();
    wrong.steps[0].binding = TaskStepBinding::ProduceTextArtifact {
        canonical_directory: "/reports".into(),
        allowed_source_scopes: vec![],
    };
    assert!(validate_contract(&wrong).is_err());
    wrong = definition.clone();
    wrong.permissions[0].effect = CapabilityEffect::SendExternal;
    assert!(validate_contract(&wrong).is_err());
    wrong = definition;
    wrong.permissions[0].input = TaskInputConstraint::GeneratedTextArtifact {
        file_name: "report.txt".into(),
        max_content_bytes: 65_537,
    };
    assert!(validate_contract(&wrong).is_err());
}
