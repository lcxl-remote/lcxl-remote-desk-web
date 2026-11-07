use super::*;

pub(super) struct GrantTools {
    pub(super) snapshot: RefCell<crate::grant_disclosure::GrantDisclosureSnapshot>,
    pub(super) reads: std::cell::Cell<usize>,
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
        assert!(matches!(
            call.name.as_str(),
            "launch_application" | "exec_command" | "create_text_file"
        ));
        self.reads.set(self.reads.get() + 1);
        self.snapshot
            .borrow_mut()
            .grants
            .iter_mut()
            .find(|grant| grant.tool_name == call.name)
            .unwrap()
            .remaining_uses = 0;
        Ok(ExecOutcome::Executed {
            data_envelope: None,
            output: ToolRunOutput {
                format: crate::seam::ToolOutputFormat::Text,
                content: "launch accepted by test seam".into(),
                image_data_url: None,
                document_preview: None,
            },
            event_id: None,
        })
    }
    async fn run_read(&self, _: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        self.reads.set(self.reads.get() + 1);
        self.snapshot.borrow_mut().grants[0].remaining_uses = 0;
        Ok(ToolRunOutput {
            format: crate::seam::ToolOutputFormat::Text,
            content: "immutable observation".into(),
            image_data_url: None,
            document_preview: None,
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
        turns: RefCell::new(
            [
                ModelTurn {
                    tool_calls: vec![
                        ToolCall {
                            id: "read".into(),
                            name: target.name().into(),
                            arguments_json: "{}".into(),
                        },
                        ToolCall {
                            id: "duplicate".into(),
                            name: target.name().into(),
                            arguments_json: "{}".into(),
                        },
                    ],
                    stop_reason: StopReason::ToolUse,
                    provider_meta: tool_meta(),
                    ..Default::default()
                },
                answer("done"),
            ]
            .into(),
        ),
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
    assert!(
        session
            .inner
            .borrow()
            .as_ref()
            .unwrap()
            .conversation
            .iter()
            .any(
                |message| message.tool_call_id.as_deref() == Some("duplicate")
                    && message.text.contains("grant_exhausted")
            )
    );
    let requests = requests.borrow();
    assert_eq!(requests[0].messages[0].text, requests[1].messages[0].text);
    for request in requests.iter() {
        let runtime = request
            .messages
            .iter()
            .position(crate::runtime_context::is_runtime)
            .unwrap();
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|message| crate::runtime_context::is_runtime(message))
                .count(),
            1
        );
        assert!(
            request.messages[..runtime]
                .iter()
                .any(|message| message.role != ChatRole::System)
        );
        assert!(
            !request.messages[0]
                .text
                .contains("capability_authorization")
        );
    }
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|t| t.name == "inspect_desktop_session")
    );
    assert!(
        requests[1]
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
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.text.contains("exhausted"))
    );
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
    fn model_output_token_limit(&self, request: &ModelRequest) -> Result<i64, AgentError> {
        Ok(request
            .caller_output_hard_cap
            .unwrap_or(128_000)
            .min(128_000))
    }

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
async fn permission_resume_keeps_scoped_prerequisites_and_validates_exact_grants() {
    use crate::dynamic_run::{
        GrantRequestItem, PERMISSION_REQUEST_SCHEMA_VERSION, PermissionRequest,
        PermissionRequestState,
    };
    use desk_agent_protocol::capability_grant::{CapabilityGrantUsePolicy, CapabilityRiskTier};
    use sha2::{Digest, Sha256};
    for blocked in [
        "none",
        "mixed",
        "expired",
        "revoked",
        "changed_input",
        "missing_grant",
        "offline",
        "scope",
        "mode",
    ] {
        let (mut initial, providers, mut snapshot) = crate::grant_disclosure::tests::fixture();
        let action_name = if blocked == "mixed" {
            "exec_command"
        } else {
            "launch_application"
        };
        let descriptor = providers.capability_for_tool(action_name).unwrap();
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
        let launch_exact = r#"{"args":[],"cwd":null,"run_as_admin":false,"target":{"kind":"executable","value":"C:\\app.exe"}}"#;
        let exact = if blocked == "mixed" {
            r#"{"command":"python hello.py","cwd":"C:/Users/Public","shell":"powershell","timeout_ms":60000}"#
        } else {
            launch_exact
        };
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
                selected_device_target: None,
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
        if blocked == "mixed" {
            crate::file_model_input::tests::approve_directory(&mut initial, "test-directory");
            let create = providers.capability_for_tool("create_text_file").unwrap();
            registry.push(create.registered_tool());
            scope.granted.push(create.required_capability);
            initial.scope_snapshot = scope.clone();
            snapshot.ready_capabilities.push(create.required_capability);
            let mut grant = snapshot.grants[0].clone();
            grant.grant_id = "create-grant".into();
            grant.provider_id = providers
                .provider_for_capability(&create.wire.capability_id)
                .unwrap()
                .wire
                .provider_id
                .clone();
            grant.capability_id = create.wire.capability_id.clone();
            grant.tool_name = create.wire.tool_name.clone();
            grant.tool_schema_version = create.wire.input_schema_version;
            grant.effect = create.wire.effect;
            grant.use_policy = CapabilityGrantUsePolicy::Reusable;
            grant.canonical_input_digest_sha256 = None;
            grant.resource_scope = vec!["directory:test".into()];
            grant.operation_scope = vec!["create_new_artifact".into()];
            grant.validate().unwrap();
            snapshot.grants.push(grant);
        }
        let mut inventory = vec![crate::capability_availability::CapabilityAvailability {
            provider_id: snapshot
                .grants
                .first()
                .map(|g| g.provider_id.clone())
                .unwrap_or_else(|| "application.launch".into()),
            capability_id: descriptor.wire.capability_id.clone(),
            tool_name: action_name.into(),
            compiled: true,
            enabled: true,
            connected: true,
            ready: true,
            reason: None,
        }];
        if blocked == "mixed" {
            let create = providers.capability_for_tool("create_text_file").unwrap();
            let mut available = inventory[0].clone();
            available.provider_id = snapshot.grants[1].provider_id.clone();
            available.capability_id = create.wire.capability_id.clone();
            available.tool_name = "create_text_file".into();
            inventory.push(available);
        }
        let session = MemSession {
            inner: RefCell::new(Some(initial)),
            ..Default::default()
        };
        let requests = Rc::new(RefCell::new(vec![]));
        let turns = if blocked == "mixed" {
            vec![
                tool_use_args(
                    "create",
                    "create_text_file",
                    r#"{"file_name":"hello.py","content_utf8":"print('hello world')"}"#,
                ),
                tool_use_args("launch", action_name, exact),
                answer("done"),
            ]
        } else if blocked == "none" {
            vec![tool_use_args("launch", action_name, exact), answer("done")]
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
        let exact_names = vec![action_name.into()];
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
            if blocked == "mixed" {
                2
            } else {
                usize::from(blocked == "none")
            },
            "{blocked}"
        );
        let requests = requests.borrow();
        assert_eq!(
            requests[0].tools.iter().any(|t| t.name == action_name),
            matches!(blocked, "none" | "mixed"),
            "{blocked}"
        );
        if blocked == "mixed" {
            assert!(
                requests[0]
                    .tools
                    .iter()
                    .any(|t| t.name == "create_text_file")
            );
            assert!(
                requests[0]
                    .messages
                    .iter()
                    .any(|m| m.text.contains("dependency order"))
            );
            assert!(
                requests[1]
                    .tools
                    .iter()
                    .any(|t| t.name == "create_text_file")
            );
            assert!(requests[1].tools.iter().any(|t| t.name == action_name));
            assert!(requests[2].tools.iter().any(|t| t.name == action_name));
        } else if blocked == "none" {
            assert!(requests[1].tools.iter().any(|t| t.name == action_name));
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

#[tokio::test]
async fn workbench_coordinator_uses_reads_and_rejects_forged_writes_and_permissions() {
    use crate::{native_ids, subagent::AgentRole};

    struct SelectedRead(RefCell<Vec<String>>);

    #[async_trait(?Send)]
    impl ToolSeam for SelectedRead {
        async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
            panic!("unexpected fixed-device read: {}", call.name)
        }

        async fn request_devices(
            &self,
            session: &PersistedAgentSession,
        ) -> Result<Vec<crate::request_devices::RequestDevice>, AgentError> {
            assert!(session.admits_assistant_device("device"));
            Ok(vec![crate::request_devices::RequestDevice {
                read_target: None,
                ready_provider_ids: vec![crate::ai_assistant::SYSTEM_INFO_CAPABILITY_ID.into()],
                approved_read_tools: Vec::new(),
                target_ref: "device".into(),
                name: None,
                platform: Some("macOS"),
                online: true,
                ready_capabilities: vec![Capability::SystemInfo, Capability::ShellExecConfirmed],
            }])
        }

        async fn run_request_device_read(
            &self,
            session: &PersistedAgentSession,
            call: &ToolCall,
        ) -> Result<crate::seam::ReadOutcome, AgentError> {
            let (target, _) = crate::request_devices::selected_read(call)?;
            assert!(session.admits_assistant_device(&target));
            self.0.borrow_mut().push(target);
            Ok(crate::seam::ReadOutcome::Completed {
                output: ToolRunOutput {
                    format: crate::seam::ToolOutputFormat::Text,
                    content: "macOS 15".into(),
                    image_data_url: None,
                    document_preview: None,
                },
                ok: true,
                data_envelope: None,
                event_id: None,
                background_task: None,
            })
        }
    }

    let providers = crate::ai_assistant::ai_assistant_provider_registry();
    let write = providers.capability_for_tool("browser_open_page").unwrap();
    let write_provider = providers
        .provider_for_capability(&write.wire.capability_id)
        .unwrap();
    let mut claim = claim();
    claim.device_id.clear();
    claim.current_pdp_scope.mode = ExecutionMode::ConfirmEachAction;
    claim
        .current_pdp_scope
        .granted
        .push(write.required_capability);
    let request_id = native_ids::stable(native_ids::Kind::AssistantMainRequest, &"message");
    let mut initial = PersistedAgentSession::new_workbench_coordinator(
        &claim.actor_id,
        "workbench",
        Some(&request_id),
        claim.policy_revision,
        claim.current_pdp_scope.clone(),
        &claim.now,
    )
    .unwrap();
    claim.conversation_id = initial.conversation_id.clone();
    initial.input_revision = 1;
    initial.latest_input_seq = 1;
    initial.focus_epoch.input_revision = 1;
    initial
        .bind_assistant_request_targets(vec!["device".into()])
        .unwrap();
    let candidates = [write.registered_tool()];
    let inventory = [crate::capability_availability::CapabilityAvailability {
        provider_id: write_provider.wire.provider_id.clone(),
        capability_id: write.wire.capability_id.clone(),
        tool_name: write.tool_spec.name.clone(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    let proposal = ToolCall {
        id: "proposal".into(),
        name: crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
        arguments_json: serde_json::json!({"items":[{
            "item_id": "open",
            "tool_name": "browser_open_page",
            "exact_input": {"target": {
                "origin": {"kind":"https","host_ascii":"app.slack.com","port":443},
                "url": "https://app.slack.com/"
            }},
            "suggested_ttl_seconds": 300,
            "suggested_max_uses": 1,
            "reason": "Open the page"
        }]})
        .to_string(),
    };

    // This is a valid native permission proposal and a requestable provider.
    let request = crate::permission_tools::build_permission_request(
        &proposal,
        &providers,
        "request".into(),
        1,
        claim.now.clone(),
    )
    .unwrap();
    validate_permission_request_availability(
        &AgentRole::Main,
        &[],
        &request,
        &providers,
        &inventory,
        &[],
        &candidates,
    )
    .unwrap();
    assert!(
        validate_permission_request_availability(
            &initial.agent_role,
            &[],
            &request,
            &providers,
            &inventory,
            &[],
            &candidates,
        )
        .is_err()
    );
    let mut invalid = initial.clone();
    invalid.permission_requests.push(request);
    assert!(
        PersistedAgentSession::decode_json(&serde_json::to_string(&invalid).unwrap())
            .unwrap_err()
            .to_string()
            .contains("invalid workbench coordination session")
    );

    let session = MemSession {
        inner: RefCell::new(Some(initial)),
        ..Default::default()
    };
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new(
            [
                ModelTurn {
                    tool_calls: vec![
                        ToolCall {
                            id: "write".into(),
                            name: "browser_open_page".into(),
                            arguments_json: "{}".into(),
                        },
                        proposal,
                        ToolCall {
                            id: "read".into(),
                            name: crate::request_devices::SYSTEM_INFO_TOOL.into(),
                            arguments_json: r#"{"target_ref":"device"}"#.into(),
                        },
                    ],
                    stop_reason: StopReason::ToolUse,
                    provider_meta: tool_meta(),
                    ..Default::default()
                },
                answer("inspection complete"),
            ]
            .into(),
        ),
        requests: requests.clone(),
    };
    let tools = SelectedRead(RefCell::new(Vec::new()));
    let mut registry = candidates.to_vec();
    registry.extend(crate::permission_tools::permission_planning_tool_registry());
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    deps.capability_permission_candidates = &candidates;
    run_agent_turn(
        &deps,
        claim,
        ChatMessage::text("u", ChatRole::User, "inspect the environment"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();

    let saved = session.inner.borrow();
    let saved = saved.as_ref().unwrap();
    assert_eq!(
        tools.0.borrow().as_slice(),
        &["device"],
        "{:?}",
        saved.conversation
    );
    assert!(saved.permission_requests.is_empty());
    for id in ["write", "proposal", "read"] {
        assert!(
            saved
                .conversation
                .iter()
                .any(|message| message.tool_call_id.as_deref() == Some(id))
        );
    }
    let requests = requests.borrow();
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == crate::request_devices::SYSTEM_INFO_TOOL)
    );
    assert!(
        !requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == "browser_open_page")
    );
    assert!(
        !requests[0]
            .messages
            .iter()
            .any(|message| message.text.contains("browser_open_page"))
    );
}
