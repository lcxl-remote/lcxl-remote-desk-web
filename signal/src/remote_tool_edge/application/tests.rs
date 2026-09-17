use super::*;

#[tokio::test]
async fn application_catalog_passes_observation_gate_and_reaches_remote_dispatch() {
    let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
    let registry = desk_diagnose_core::device_assistant::device_assistant_provider_registry();
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
        allowed_content_digests_sha256: vec![],
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
    let tools = SignalDeviceAssistantTools::new(
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
