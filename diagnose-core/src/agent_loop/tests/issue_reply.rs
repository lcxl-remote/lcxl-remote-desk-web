use super::delegation_budget::BudgetSessions;
use super::*;
use crate::external_tools::{IssueAccess, IssueTarget};
use crate::issue_reply::{
    PREPARE_TOOL_NAME, ReplyGeneration, ReplyGenerationInput, ReplyOrigin, ReplyPublication,
    ReplyToolReceipt,
};

fn access() -> IssueAccess {
    IssueAccess {
        connection_ref: "forge".into(),
        target: IssueTarget {
            platform: desk_agent_protocol::external_tools::IssuePlatform::Gitea,
            instance: "https://forge.test/".into(),
            account_id: 3,
            repository_id: 42,
            owner: "org".into(),
            repository: "project".into(),
            issue_number: 4,
            issue_id: 82,
            private: false,
            internal: false,
        },
    }
}

fn policy() -> crate::model_egress::ModelEgressPolicy {
    crate::model_egress::ModelEgressPolicy {
        destination: DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
        selected_source_tools: Default::default(),
        export_authorization_id: "ordinary-export".into(),
        now_unix_ms: 1,
        byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    }
}

struct PublishingModel {
    inner: ScriptModel,
}
#[async_trait(?Send)]
impl ModelSeam for PublishingModel {
    fn model_egress_policy(
        &self,
    ) -> Result<Option<crate::model_egress::ModelEgressPolicy>, AgentError> {
        Ok(Some(policy()))
    }
    fn model_output_token_limit(&self, req: &ModelRequest) -> Result<i64, AgentError> {
        self.inner.model_output_token_limit(req)
    }
    async fn context_policy(
        &self,
        req: crate::model_capability::ModelRequirements,
    ) -> Result<crate::model_context::PinnedContextPolicy, AgentError> {
        self.inner.context_policy(req).await
    }
    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
        if request.issue_reply.is_some() {
            assert!(request.delegation_call.is_some());
            assert_eq!(request.messages.len(), 2);
            assert!(
                request
                    .messages
                    .iter()
                    .all(|m| !m.text.contains("PRIVATE_OWNER_INPUT"))
            );
            assert!(request.tools.is_empty());
            assert!(request.assistant_data.is_none());
        }
        self.inner.call(request, sink).await
    }
}

struct PublishingTools {
    existing: bool,
    prepared: RefCell<usize>,
    frozen: RefCell<usize>,
}
#[async_trait(?Send)]
impl ToolSeam for PublishingTools {
    async fn issue_reply_access(
        &self,
        _: &PersistedAgentSession,
    ) -> Result<Vec<IssueAccess>, AgentError> {
        Ok(vec![access()])
    }
    async fn prepare_issue_reply(
        &self,
        session: &PersistedAgentSession,
        call: &ToolCall,
    ) -> Result<ReplyGeneration, AgentError> {
        let accesses = [access()];
        let issue = crate::issue_reply::resolve(&accesses, call)?.clone();
        *self.prepared.borrow_mut() += 1;
        let artifact_id = crate::issue_reply::artifact_id(
            &session.actor_id,
            &session.conversation_id,
            session.input_revision,
            &call.id,
        );
        if self.existing {
            return Ok(ReplyGeneration::Existing(ReplyToolReceipt {
                artifact_id,
                state: "ready".into(),
                reason: None,
            }));
        }
        let read = ToolCall {
            id: format!("{}-source", call.id),
            name: crate::external_tools::TOOL_NAME.into(),
            arguments_json:
                serde_json::json!({"tool_ref":issue.tool_ref(),"args":{"method":"get"}}).to_string(),
        };
        let content = "Authorized issue reproduction";
        let mut original =
            crate::input_read_context::ReadContextSelection::capture(&[], &session.scope_snapshot)
                .unwrap();
        original.external_issues.push(issue.clone());
        let label = crate::external_tools::label_issue_read(
            session,
            &original,
            &policy().destination,
            &read,
            &"a".repeat(64),
            content,
        )?;
        let mut material = ChatMessage::text("issue-material", ChatRole::UntrustedOutput, content);
        material.data_envelope = Some(label);
        Ok(ReplyGeneration::Generate(Box::new(ReplyGenerationInput {
            publication: ReplyPublication {
                origin: ReplyOrigin {
                    actor_id: session.actor_id.clone(),
                    root_conversation_id: session.conversation_id.clone(),
                    request_id: "request-a".into(),
                    execution_conversation_id: session.conversation_id.clone(),
                    tool_call_id: call.id.clone(),
                    input_revision: session.input_revision,
                    control_revision: session.control_revision,
                    authorization_id: "grant-a".into(),
                    authorization_revision: 1,
                },
                access: issue,
                comment_tool_signature: "b".repeat(64),
            },
            materials: vec![material],
        })))
    }
    async fn freeze_issue_reply(
        &self,
        session: &PersistedAgentSession,
        publication: ReplyPublication,
        turn: ModelTurn,
    ) -> Result<ReplyToolReceipt, AgentError> {
        *self.frozen.borrow_mut() += 1;
        assert_eq!(turn.text, "PUBLICATION_TEXT");
        assert_eq!(publication.origin.actor_id, session.actor_id);
        assert_eq!(publication.origin.input_revision, session.input_revision);
        assert!(
            session
                .unclosed_tool_call_ids()
                .iter()
                .any(|id| id == &publication.origin.tool_call_id)
        );
        Ok(ReplyToolReceipt {
            artifact_id: crate::issue_reply::artifact_id(
                &session.actor_id,
                &session.conversation_id,
                session.input_revision,
                &publication.origin.tool_call_id,
            ),
            state: "ready".into(),
            reason: None,
        })
    }
    async fn run_read(&self, _: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        panic!("Publication must not enter device reads")
    }
}

#[tokio::test]
async fn publication_uses_original_physical_budget_and_replay_skips_generation() {
    for (existing, denied) in [(false, false), (true, false), (false, true)] {
        let sessions = BudgetSessions {
            deny_publication: denied,
            ..Default::default()
        };
        let mut first = tool_use("publish-1", PREPARE_TOOL_NAME);
        first.tool_calls[0].arguments_json =
            serde_json::json!({"tool_ref":access().tool_ref()}).to_string();
        let turns = if existing || denied {
            vec![first, answer("DONE")]
        } else {
            let mut publication = answer("PUBLICATION_TEXT");
            publication.usage = crate::chat::TokenUsage {
                input_tokens: Some(7),
                output_tokens: Some(3),
                ..Default::default()
            };
            vec![first, publication, answer("DONE")]
        };
        let model = PublishingModel {
            inner: ScriptModel {
                turns: RefCell::new(turns.into()),
                requests: Rc::new(RefCell::new(vec![])),
            },
        };
        let tools = PublishingTools {
            existing,
            prepared: RefCell::new(0),
            frozen: RefCell::new(0),
        };
        let clock = || "2026-11-05T00:00:00Z".to_string();
        let mut deps = deps(&sessions.sessions, &model, &tools, &[], &clock);
        deps.session_seam = &sessions;
        let output = Rc::new(RefCell::new(String::new()));
        let mut sink = Collector(output.clone());
        let mut input = claim();
        input.current_pdp_scope.granted.clear();
        input.current_pdp_scope.mode = ExecutionMode::ReadOnly;
        let result = run_agent_turn(
            &deps,
            input,
            ChatMessage::text("u", ChatRole::User, "PRIVATE_OWNER_INPUT"),
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(result, LoopOutcome::Answered("DONE".into()));
        assert_eq!(*tools.prepared.borrow(), 1);
        assert_eq!(*tools.frozen.borrow(), usize::from(!existing && !denied));
        assert!(!output.borrow().contains("PUBLICATION_TEXT"));
        let requests = model.inner.requests.borrow();
        assert_eq!(requests.len(), if existing || denied { 2 } else { 3 });
        assert!(
            requests[0]
                .tools
                .iter()
                .any(|t| t.name == PREPARE_TOOL_NAME)
        );
        assert_eq!(
            requests.iter().filter(|r| r.issue_reply.is_some()).count(),
            usize::from(!existing && !denied)
        );
        let reservations = sessions.reservations.borrow();
        assert_eq!(
            reservations
                .iter()
                .filter(|r| r.kind == crate::subagent::reservation::DelegationCallKind::Model)
                .count(),
            requests.len()
        );
        assert_eq!(sessions.settlements.borrow().len(), reservations.len());
        let final_session = sessions.sessions.inner.borrow();
        let final_session = final_session.as_ref().unwrap();
        assert_eq!(final_session.current_turn_steps, 2);
        assert_eq!(final_session.input_revision, 1);
        if !existing && !denied {
            assert_eq!(final_session.lifetime_tokens.input_tokens, Some(7));
            assert_eq!(final_session.lifetime_tokens.output_tokens, Some(3));
            let settlement = sessions.settlements.borrow();
            let (_, actual) = settlement
                .iter()
                .find(|(id, _)| {
                    reservations.iter().any(|r| {
                        &r.reservation_id == id && r.logical_call_id.starts_with("issue-reply:")
                    })
                })
                .unwrap();
            let actual = actual.unwrap();
            assert_eq!(actual.input_tokens, 7);
            assert_eq!(actual.output_tokens, 3);
            assert_eq!(actual.model_calls, 1);
        }

        assert_eq!(
            requests
                .last()
                .unwrap()
                .messages
                .iter()
                .filter(|m| m.role == ChatRole::Tool)
                .count(),
            1
        );
        assert!(
            requests
                .last()
                .unwrap()
                .messages
                .iter()
                .any(|m| m.role == ChatRole::Tool
                    && if denied {
                        m.text.contains("budget")
                    } else {
                        m.text.contains("ready")
                    })
        );
    }
}

#[test]
fn publication_exposure_and_arguments_use_the_same_role_boundary() {
    let access = access();
    let tool = crate::issue_reply::tool(std::slice::from_ref(&access))
        .unwrap()
        .unwrap();
    assert_eq!(tool.effect, ToolEffect::ExternalWrite);
    assert!(tool.required_device_capability.is_none());
    let mut session = PersistedAgentSession::new("root", "owner", "", 1, scope(), "now");
    session.scope_snapshot.granted.clear();
    session.scope_snapshot.mode = ExecutionMode::ReadOnly;
    assert!(
        crate::registry::lookup_for_session(
            std::slice::from_ref(&tool),
            PREPARE_TOOL_NAME,
            &session
        )
        .is_some()
    );
    session.agent_role = crate::subagent::role::AgentRole::Coordinator {
        workbench_id: "workbench".into(),
        request_id: None,
    };
    assert!(
        crate::registry::lookup_for_session(
            std::slice::from_ref(&tool),
            PREPARE_TOOL_NAME,
            &session
        )
        .is_none()
    );
    assert!(crate::registry::exposed_for_session(std::slice::from_ref(&tool), &session).is_empty());
    for args in [
        serde_json::json!({"tool_ref":"wrong"}),
        serde_json::json!({"tool_ref":access.tool_ref(),"body":"Injected text"}),
        serde_json::json!({"tool_ref":access.tool_ref(),"target":"other-issue"}),
    ] {
        let call = ToolCall {
            id: "publish-1".into(),
            name: PREPARE_TOOL_NAME.into(),
            arguments_json: args.to_string(),
        };
        assert!(crate::issue_reply::resolve(std::slice::from_ref(&access), &call).is_err());
    }
}

#[tokio::test]
async fn incomplete_generation_and_content_rejection_never_freeze_a_comment() {
    use desk_agent_protocol::content_safety::{ContentSafetyDecision, ContentSafetyStage};
    for case in 0..4 {
        let sessions = BudgetSessions::default();
        let mut first = tool_use("publish-1", PREPARE_TOOL_NAME);
        first.tool_calls[0].arguments_json =
            serde_json::json!({"tool_ref":access().tool_ref()}).to_string();
        let mut reply = answer("PUBLICATION_TEXT");
        match case {
            0 => reply.stop_reason = StopReason::MaxTokens,
            1 => reply = tool_use("unexpected", "write"),
            2 => reply.text.clear(),
            _ => {}
        }
        let model = PublishingModel {
            inner: ScriptModel {
                turns: RefCell::new(vec![first, reply, answer("DONE")].into()),
                requests: Rc::new(RefCell::new(vec![])),
            },
        };
        let tools = PublishingTools {
            existing: false,
            prepared: RefCell::new(0),
            frozen: RefCell::new(0),
        };
        let safety = SafetyScript {
            model_turn_results: RefCell::new(
                [
                    Ok(safety_verdict(
                        ContentSafetyDecision::Allow,
                        ContentSafetyStage::Output,
                    )),
                    Ok(safety_verdict(
                        ContentSafetyDecision::Block,
                        ContentSafetyStage::Output,
                    )),
                ]
                .into(),
            ),
            ..Default::default()
        };
        let clock = || "2026-11-05T00:00:00Z".to_string();
        let mut deps = deps(&sessions.sessions, &model, &tools, &[], &clock);
        deps.session_seam = &sessions;
        if case == 3 {
            deps.content_safety = crate::content_safety::ContentSafetyMode::Enforced {
                seam: &safety,
                context: safety_context(),
            };
        }
        let result = run_agent_turn(
            &deps,
            claim(),
            ChatMessage::text("u", ChatRole::User, "PRIVATE_OWNER_INPUT"),
            &mut NullTurnSink,
        )
        .await;
        if case == 3 {
            assert_eq!(
                result.unwrap_err().kind,
                desk_agent_protocol::AgentErrorKind::ContentBlocked
            );
            let reviews = safety.model_turn_requests.borrow();
            assert_eq!(reviews.len(), 2);
            assert_eq!(reviews[1].text, "PUBLICATION_TEXT");
        } else {
            assert_eq!(result.unwrap(), LoopOutcome::Answered("DONE".into()));
        }
        assert_eq!(*tools.frozen.borrow(), 0);
        assert_eq!(
            sessions.settlements.borrow().len(),
            sessions.reservations.borrow().len()
        );
    }
}

#[tokio::test]
async fn forged_coordinator_publication_never_reaches_the_generation_port() {
    let sessions = BudgetSessions::default();
    let mut input = claim();
    input.device_id.clear();
    let mut initial = PersistedAgentSession::new_workbench_coordinator(
        &input.actor_id,
        "workbench",
        None,
        input.policy_revision,
        input.current_pdp_scope.clone(),
        &input.now,
    )
    .unwrap();
    initial.input_revision = 1;
    initial.latest_input_seq = 1;
    initial.focus_epoch.input_revision = 1;
    input.conversation_id = initial.conversation_id.clone();
    *sessions.sessions.inner.borrow_mut() = Some(initial);
    let mut forged = tool_use("publish-1", PREPARE_TOOL_NAME);
    forged.tool_calls[0].arguments_json =
        serde_json::json!({"tool_ref":access().tool_ref()}).to_string();
    let model = PublishingModel {
        inner: ScriptModel {
            turns: RefCell::new(vec![forged, answer("DONE")].into()),
            requests: Rc::new(RefCell::new(vec![])),
        },
    };
    let tools = PublishingTools {
        existing: false,
        prepared: RefCell::new(0),
        frozen: RefCell::new(0),
    };
    let clock = || "2026-11-05T00:00:00Z".to_string();
    let mut deps = deps(&sessions.sessions, &model, &tools, &[], &clock);
    deps.session_seam = &sessions;
    assert_eq!(
        run_agent_turn(
            &deps,
            input,
            ChatMessage::text("u", ChatRole::User, "PRIVATE_OWNER_INPUT"),
            &mut NullTurnSink
        )
        .await
        .unwrap(),
        LoopOutcome::Answered("DONE".into())
    );
    assert_eq!(*tools.prepared.borrow(), 0);
    assert_eq!(*tools.frozen.borrow(), 0);
    assert!(model.inner.requests.borrow().iter().all(|request| {
        request
            .tools
            .iter()
            .all(|tool| tool.name != PREPARE_TOOL_NAME)
            && request.issue_reply.is_none()
    }));
}

#[tokio::test]
async fn a_worker_cannot_generate_without_its_durable_original_budget() {
    let sessions = MemSession::default();
    let model = PublishingModel {
        inner: ScriptModel {
            turns: RefCell::new(Vec::new().into()),
            requests: Rc::new(RefCell::new(vec![])),
        },
    };
    let tools = PublishingTools {
        existing: false,
        prepared: RefCell::new(0),
        frozen: RefCell::new(0),
    };
    let clock = || "2026-11-05T00:00:00Z".to_string();
    let deps = deps(&sessions, &model, &tools, &[], &clock);
    let mut session = PersistedAgentSession::new("worker", "owner", "", 1, scope(), "now");
    session.input_revision = 1;
    session.current_turn_id = Some("worker-turn".into());
    session.turn_state = TurnState::Running;
    session.agent_role = crate::subagent::AgentRole::SubAgent {
        binding: Box::new(crate::subagent::DelegatedTaskBinding {
            root_conversation_id: "root".into(),
            group_id: "group".into(),
            task_id: "task".into(),
            source: crate::subagent::DelegationSource::UserInput { input_revision: 1 },
            targets: vec![],
            objective: "Investigate issue".into(),
            acceptance_criteria: vec!["Report findings".into()],
            input_revision: 1,
            control_revision: 0,
            source_epoch: 1,
            deadline_ms: i64::MAX,
        }),
    };
    let call = ToolCall {
        id: "publish-1".into(),
        name: PREPARE_TOOL_NAME.into(),
        arguments_json: serde_json::json!({"tool_ref":access().tool_ref()}).to_string(),
    };
    let error = super::super::issue_reply::run(&deps, &mut session, &call)
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "durable child call accounting is unavailable"
    );
    assert!(model.inner.requests.borrow().is_empty());
    assert_eq!(*tools.frozen.borrow(), 0);
}
