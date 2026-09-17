use super::*;

struct GrantTools {
    snapshot: RefCell<crate::grant_disclosure::GrantDisclosureSnapshot>,
    reads: std::cell::Cell<usize>,
}

#[async_trait(?Send)]
impl ToolSeam for GrantTools {
    async fn current_grant_disclosure(
        &self,
    ) -> Result<Option<crate::grant_disclosure::GrantDisclosureSnapshot>, AgentError> {
        Ok(Some(self.snapshot.borrow().clone()))
    }
    async fn confirm_and_exec(
        &self,
        call: &ToolCall,
        _: &crate::seam::ExecContext,
    ) -> Result<ExecOutcome, AgentError> {
        assert_eq!(call.name, "launch_application");
        self.reads.set(self.reads.get() + 1);
        self.snapshot.borrow_mut().grants[0].remaining_uses = 0;
        Ok(ExecOutcome::Executed {
            data_envelope: None,
            output: ToolRunOutput {
                content: "launch accepted by test seam".into(),
                image_data_url: None,
            },
            event_id: None,
        })
    }
    async fn run_read(&self, _: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        self.reads.set(self.reads.get() + 1);
        self.snapshot.borrow_mut().grants[0].remaining_uses = 0;
        Ok(ToolRunOutput {
            content: "immutable observation".into(),
            image_data_url: None,
        })
    }
}

#[tokio::test]
async fn approval_exposes_without_load_and_last_use_refreshes_next_request() {
    let (mut initial, providers, snapshot) = crate::grant_disclosure::tests::fixture();
    initial.latest_input_seq = 1;
    initial.focus_epoch.input_revision = 1;
    let scope = initial.scope_snapshot.clone();
    let target = providers
        .capability_for_tool("inspect_desktop_session")
        .unwrap()
        .registered_tool();
    let inventory = vec![crate::capability_availability::CapabilityAvailability {
        provider_id: snapshot.grants[0].provider_id.clone(),
        capability_id: snapshot.grants[0].capability_id.clone(),
        tool_name: target.name().into(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    let session = MemSession {
        inner: RefCell::new(Some(initial)),
        ..Default::default()
    };
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new([tool_use_args("read", target.name(), "{}"), answer("done")].into()),
        requests: requests.clone(),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let mut registry = vec![target];
    registry.extend(crate::capability_disclosure::capability_discovery_tool_registry());
    registry.extend(crate::permission_tools::permission_planning_tool_registry());
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    run_agent_turn(
        &deps,
        ClaimTurnParams {
            current_pdp_scope: scope,
            ..claim()
        },
        ChatMessage::text("u", ChatRole::User, "inspect the session"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    assert_eq!(tools.reads.get(), 1);
    let requests = requests.borrow();
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|t| t.name == "inspect_desktop_session")
    );
    assert!(
        !requests[1]
            .tools
            .iter()
            .any(|t| t.name == "inspect_desktop_session")
    );
    assert!(
        requests[1]
            .tools
            .iter()
            .any(|t| t.name == "request_permissions")
    );
    assert!(requests[1].messages[0].text.contains("exhausted"));
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.text.contains("immutable observation"))
    );
}

struct ExactGrantModel(ScriptModel);
#[async_trait(?Send)]
impl ModelSeam for ExactGrantModel {
    fn model_egress_policy(
        &self,
    ) -> Result<Option<crate::model_egress::ModelEgressPolicy>, AgentError> {
        Ok(Some(crate::model_egress::ModelEgressPolicy {
            destination: desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "test".into(),
                connection_revision: 1,
                model_id: "test".into(),
                profile_revision: 1,
            },
            selected_source_tools: Default::default(),
            export_authorization_id: "test".into(),
            now_unix_ms: 500,
            byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
            permission_resume: true,
        }))
    }
    async fn context_policy(
        &self,
        requirements: crate::model_capability::ModelRequirements,
    ) -> Result<crate::model_context::PinnedContextPolicy, AgentError> {
        self.0.context_policy(requirements).await
    }
    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
        self.0.call(request, sink).await
    }
}

/// Real registry, scope projection, exact grant and permission-resume loop;
/// native dispatch is replaced by the recording seam, so no app is launched.
#[tokio::test]
async fn native_launch_permission_resume_uses_registered_scope_and_exact_grant() {
    use crate::dynamic_run::{
        GrantRequestItem, PERMISSION_REQUEST_SCHEMA_VERSION, PermissionRequest,
        PermissionRequestState,
    };
    use desk_agent_protocol::capability_grant::{CapabilityGrantUsePolicy, CapabilityRiskTier};
    use sha2::{Digest, Sha256};
    for blocked in [
        "none",
        "expired",
        "revoked",
        "changed_input",
        "missing_grant",
        "offline",
        "scope",
        "mode",
    ] {
        let (mut initial, providers, mut snapshot) = crate::grant_disclosure::tests::fixture();
        let descriptor = providers.capability_for_tool("launch_application").unwrap();
        let mut registry = vec![descriptor.registered_tool()];
        crate::tool_exposure::retain_candidates(&providers, &mut registry, &[]);
        let mut scope = AgentScope {
            granted: vec![],
            mode: ExecutionMode::ConfirmEachAction,
            expires_at: None,
            policy_name: None,
        };
        crate::tool_exposure::extend_candidate_scope(
            &providers,
            &registry,
            &[],
            &[],
            &mut scope.granted,
        );
        if blocked == "scope" {
            scope.granted.clear();
        }
        if blocked == "mode" {
            scope.mode = ExecutionMode::ReadOnly;
        }
        initial.scope_snapshot = scope.clone();
        initial.latest_input_seq = 1;
        initial.focus_epoch.input_revision = 1;
        initial.conversation.push(ChatMessage::text(
            "original",
            ChatRole::User,
            "open the approved app",
        ));
        let exact = r#"{"args":[],"cwd":null,"run_as_admin":false,"target":{"kind":"executable","value":"C:\\app.exe"}}"#;
        let digest = format!("{:x}", Sha256::digest(exact.as_bytes()));
        let grant = &mut snapshot.grants[0];
        grant.provider_id = providers
            .provider_for_capability(&descriptor.wire.capability_id)
            .unwrap()
            .wire
            .provider_id
            .clone();
        grant.capability_id = descriptor.wire.capability_id.clone();
        grant.tool_name = descriptor.wire.tool_name.clone();
        grant.effect = descriptor.wire.effect;
        grant.risk_tier = CapabilityRiskTier::R3;
        grant.use_policy = CapabilityGrantUsePolicy::OneShotExact;
        grant.canonical_input_digest_sha256 = Some(digest.clone());
        grant.resource_scope = vec![format!("application_input:sha256:{}", "a".repeat(64))];
        grant.operation_scope = vec!["launch_application".into()];
        grant.validate().unwrap();
        initial.permission_requests.push(PermissionRequest {
            schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
            request_id: "approved-launch".into(),
            input_revision: 1,
            state: PermissionRequestState::Approved,
            items: vec![GrantRequestItem {
                command_confirmation: None,
                launch_confirmation: None,
                item_id: "launch".into(),
                provider_id: grant.provider_id.clone(),
                tool_name: grant.tool_name.clone(),
                expected_effect: grant.effect,
                resource_scope: grant.resource_scope.clone(),
                operation_scope: grant.operation_scope.clone(),
                export_destinations: vec![],
                canonical_input_json: Some(exact.into()),
                canonical_input_digest_sha256: Some(digest),
                suggested_ttl_seconds: 300,
                suggested_max_uses: 1,
                reason: "open app".into(),
            }],
            created_at: "1970-01-01T00:00:00.100Z".into(),
        });
        snapshot.ready_capabilities = if blocked == "offline" {
            vec![]
        } else {
            vec![descriptor.required_capability]
        };
        if blocked == "expired" {
            snapshot.grants[0].expires_at_unix_ms = 400;
        }
        if blocked == "missing_grant" {
            snapshot.grants.clear();
        }
        if blocked == "revoked" {
            snapshot.grants[0].revoked_at_unix_ms = Some(400);
            snapshot.grants[0].revoked_reason = Some("owner".into());
        }
        if blocked == "changed_input" {
            initial.permission_requests[0].items[0].canonical_input_json = Some("{}".into());
        }
        let inventory = vec![crate::capability_availability::CapabilityAvailability {
            provider_id: "application.launch".into(),
            capability_id: descriptor.wire.capability_id.clone(),
            tool_name: "launch_application".into(),
            compiled: true,
            enabled: true,
            connected: true,
            ready: true,
            reason: None,
        }];
        let session = MemSession {
            inner: RefCell::new(Some(initial)),
            ..Default::default()
        };
        let requests = Rc::new(RefCell::new(vec![]));
        let turns = if blocked == "none" {
            vec![
                tool_use_args("launch", "launch_application", exact),
                answer("done"),
            ]
        } else {
            vec![answer("blocked")]
        };
        let model = ExactGrantModel(ScriptModel {
            turns: RefCell::new(turns.into()),
            requests: requests.clone(),
        });
        let tools = GrantTools {
            snapshot: RefCell::new(snapshot),
            reads: std::cell::Cell::new(0),
        };
        let clock = || "1970-01-01T00:00:00.500Z".to_string();
        let exact_names = vec!["launch_application".into()];
        let mut deps = deps(&session, &model, &tools, &registry, &clock);
        deps.provider_registry = Some(&providers);
        deps.capability_inventory = Some(&inventory);
        deps.permission_continuation_exact_tools = &exact_names;
        let mut claim = exec_claim();
        claim.current_pdp_scope = scope;
        claim.trigger_origin = crate::session::TriggerOrigin::PermissionDecision;
        resume_agent_turn_after_permission(
            &deps,
            claim,
            ChatMessage::text("decision", ChatRole::User, "approved"),
            &mut NullTurnSink,
        )
        .await
        .unwrap();
        assert_eq!(
            tools.reads.get(),
            usize::from(blocked == "none"),
            "{blocked}"
        );
        let requests = requests.borrow();
        assert_eq!(
            requests[0]
                .tools
                .iter()
                .any(|t| t.name == "launch_application"),
            blocked == "none",
            "{blocked}"
        );
        if blocked == "none" {
            assert!(
                !requests[1]
                    .tools
                    .iter()
                    .any(|t| t.name == "launch_application")
            );
        } else {
            let reason = match blocked {
                "scope" => "missing_scope_capability",
                "offline" => "capability_not_ready",
                "expired" | "revoked" | "changed_input" | "missing_grant" => {
                    "no_active_matching_grant"
                }
                "mode" => "execution_mode_disallows_effect",
                _ => unreachable!(),
            };
            assert!(
                requests[0].messages.iter().any(|m| m.text.contains(reason)),
                "{blocked}"
            );
        }
    }
}
