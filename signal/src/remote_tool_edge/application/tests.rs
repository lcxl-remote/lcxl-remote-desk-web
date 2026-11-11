use super::*;

#[tokio::test]
async fn application_catalog_passes_observation_gate_and_reaches_remote_dispatch() {
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    let registry = desk_diagnose_core::ai_assistant::ai_assistant_provider_registry();
    let capability = registry.capability_for_tool("list_applications").unwrap();
    let provider = registry
        .provider_for_capability(&capability.wire.capability_id)
        .unwrap();
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let grant = CapabilityGrant {
        schema_version: CAPABILITY_GRANT_SCHEMA_VERSION,
        grant_id: "catalog-read".into(),
        actor_id: "owner".into(),
        run_id: "run".into(),
        input_revision: 1,
        surface: ProductSurface::OssPersonalOwner,
        target_device_id: "device".into(),
        target_session_id: None,
        provider_id: provider.wire.provider_id.clone(),
        capability_id: capability.wire.capability_id.clone(),
        tool_name: "list_applications".into(),
        tool_schema_version: capability.wire.input_schema_version,
        effect: capability.wire.effect,
        risk_tier: CapabilityRiskTier::R1,
        resource_scope: vec!["target:current_device".into()],
        operation_scope: vec!["observe".into()],
        export_destinations: vec![],
        allowed_envelope_ids: vec![],
        use_policy: CapabilityGrantUsePolicy::Reusable,
        canonical_input_digest_sha256: None,
        issued_by: CapabilityGrantIssuer::UserDecision,
        issued_at_unix_ms: now,
        expires_at_unix_ms: now + 60_000,
        remaining_uses: 3,
        limits: CapabilityGrantLimits {
            max_bytes_per_call: capability.wire.limits.max_output_bytes,
            max_items_per_call: capability.wire.limits.max_objects,
            max_calls: 3,
        },
        policy_revision: 1,
        readiness_revision: 1,
        revoked_at_unix_ms: None,
        revoked_reason: None,
    };
    grant.validate().unwrap();
    let tools = SignalAiAssistantTools::new(
        db,
        registry,
        Arc::new(SharedConnectionMap::default()),
        Arc::new(SignalRemoteToolPendingStore::default()),
        "host".into(),
        "device".into(),
        "owner".into(),
        None,
        None,
        None,
        None,
        None,
        None,
        vec![],
        vec![],
        vec![],
        None,
        None,
        "find Chrome".into(),
        "run".into(),
        "turn".into(),
        1,
        1,
        vec![],
        30_000,
    );
    tools
        .bind_original_input(
            1,
            desk_diagnose_core::input_read_context::ReadContextSelection {
                external_issues: Vec::new(),
                external_issue_sources: Vec::new(),
                tool_names: vec!["list_applications".into()],
                expires_at: None,
                object_attachments: vec![],
                live_targets: vec![],
            },
            desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "test".into(),
                profile_revision: 1,
            },
            None,
        )
        .unwrap();
    // Exercise the production post-authorization path. An offline host must be
    // reached only after parsing, the observation gate, and envelope validation.
    // No native application enumeration or launch is performed by this test.
    for arguments in [
        r#"{"queries":["chrome","谷歌浏览器"],"limit":20}"#,
        r#"{"allow_unfiltered":true,"limit":1}"#,
    ] {
        let call = ToolCall {
            id: "catalog".into(),
            name: "list_applications".into(),
            arguments_json: arguments.into(),
        };
        let result = tools.invoke(&call, &grant, grant.expires_at_unix_ms).await;
        let Err(failure) = result else {
            panic!("offline target unexpectedly succeeded")
        };
        assert_eq!(
            failure.error.kind,
            AgentErrorKind::TargetOffline,
            "{:?}",
            failure.error
        );
        assert_eq!(failure.error.message, "target host is not connected");
    }
    let invalid = ToolCall {
        id: "invalid".into(),
        name: "list_applications".into(),
        arguments_json: "{}".into(),
    };
    let Err(failure) = tools
        .invoke(&invalid, &grant, grant.expires_at_unix_ms)
        .await
    else {
        panic!("missing search accepted")
    };
    assert_eq!(failure.error.kind, AgentErrorKind::InvalidInput);
}

/// A launch without its own approved request becomes a call-time proposal in
/// an interactive session; nothing is resolved or dispatched for it.
#[tokio::test]
async fn an_unapproved_launch_asks_for_approval_instead_of_failing() {
    use sea_orm::{ActiveModelTrait, Set};
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    let scope = desk_agent_protocol::AgentScope {
        granted: vec![],
        mode: desk_agent_protocol::ExecutionMode::ConfirmEachAction,
        expires_at: None,
        policy_name: None,
    };
    let mut session = desk_diagnose_core::session::PersistedAgentSession::new(
        "run",
        "owner",
        "device",
        desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
        scope.clone(),
        "1970-01-01T00:00:00Z",
    );
    session.surface = desk_diagnose_core::session::AgentSessionSurface::AiAssistant;
    session.begin_focus_epoch(1, Vec::new()).unwrap();
    session.input_revision = 1;
    let now = chrono::DateTime::from_timestamp_millis(1).unwrap();
    crate::entity::agent_session::ActiveModel {
        conversation_id: Set(session.conversation_id.clone()),
        actor_id: Set(session.actor_id.clone()),
        device_id: Set(session.device_id.clone()),
        conversation_schema_version: Set(Some(i32::from(
            desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
        ))),
        state_json: Set(session.encode_json_for_storage().unwrap()),
        version: Set(session.version),
        lease_token: Set(session.lease_token as i64),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();
    let tools = SignalAiAssistantTools::new(
        db,
        desk_diagnose_core::ai_assistant::ai_assistant_provider_registry(),
        Arc::new(SharedConnectionMap::default()),
        Arc::new(SignalRemoteToolPendingStore::default()),
        "host".into(),
        "device".into(),
        "owner".into(),
        None,
        None,
        None,
        None,
        None,
        None,
        vec![],
        vec![],
        vec![],
        None,
        None,
        "open Calculator".into(),
        "run".into(),
        "turn".into(),
        desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
        1,
        vec![],
        30_000,
    );
    let call = ToolCall {
        id: "launch".into(),
        name: desk_diagnose_core::application_launch::TOOL_NAME.into(),
        arguments_json:
            r#"{"target":{"kind":"macos_bundle","value":"/System/Applications/Calculator.app"}}"#
                .into(),
    };
    let ctx = ExecContext {
        assistant_turn_fence: None,
        conversation_id: "run".into(),
        turn_id: "turn".into(),
        tool_call_id: "launch".into(),
        actor_id: "owner".into(),
        policy_revision: desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
        scope,
        connection_id: None,
    };
    let outcome = tools
        .authorize_and_execute_semantic_action_inner(&call, &ctx)
        .await;
    assert!(
        matches!(
            outcome,
            Ok(ExecOutcome::GrantRequired {
                input_mismatch: false
            })
        ),
        "{outcome:?}"
    );
}
