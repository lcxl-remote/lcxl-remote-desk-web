use super::*;

struct SkillModel(ScriptModel);

#[async_trait(?Send)]
impl ModelSeam for SkillModel {
    fn model_egress_policy(
        &self,
    ) -> Result<Option<crate::model_egress::ModelEgressPolicy>, AgentError> {
        Ok(Some(crate::model_egress::ModelEgressPolicy {
            destination: desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
            selected_source_tools: Default::default(),
            export_authorization_id: "export".into(),
            now_unix_ms: 1,
            byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
            permission_resume: false,
        }))
    }
    fn model_output_token_limit(&self, request: &ModelRequest) -> Result<i64, AgentError> {
        self.0.model_output_token_limit(request)
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

#[tokio::test]
async fn native_skill_load_persists_first_party_text_and_only_projects_requested_reference() {
    let sessions = MemSession::default();
    let mut seed = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        scope(),
        "2026-11-05T00:00:00Z",
    );
    seed.surface = crate::session::AgentSessionSurface::AiAssistant;
    *sessions.inner.borrow_mut() = Some(seed);
    let mut load = tool_use("skill-body", crate::skills::LOAD_TOOL);
    load.tool_calls[0].arguments_json =
        serde_json::json!({"skill_ref":"issue-analysis"}).to_string();
    let mut reference = tool_use("skill-reference", crate::skills::LOAD_TOOL);
    reference.tool_calls[0].arguments_json =
        serde_json::json!({"skill_ref":"issue-analysis","resource":"references/checklist.md"})
            .to_string();
    let model = SkillModel(ScriptModel {
        turns: RefCell::new([load, reference, answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    });
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let clock = || "2026-11-05T00:00:00Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let result = run_agent_turn(
        &deps(&sessions, &model, &tools, &[], &clock),
        claim(),
        ChatMessage::text("user", ChatRole::User, "Analyze the selected issue"),
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(result, LoopOutcome::Answered("done".into()));
    assert!(tools.calls.borrow().is_empty());
    let requests = model.0.requests.borrow();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == crate::skills::LOAD_TOOL)
    );
    assert!(
        requests[0]
            .messages
            .iter()
            .any(|message| message.text.contains("BUNDLED SKILL CATALOGUE"))
    );
    assert!(
        !requests[0]
            .messages
            .iter()
            .any(|message| message.text.contains("# Issue analysis"))
    );
    assert!(requests[1].messages.iter().any(
        |message| message.role == ChatRole::System && message.text.contains("# Issue analysis")
    ));
    assert!(
        !requests[1]
            .messages
            .iter()
            .any(|message| message.role == ChatRole::UntrustedOutput
                && message.text.contains("# Investigation and reply checklist"))
    );
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|message| message.role == ChatRole::UntrustedOutput
                && message.text.contains("# Investigation and reply checklist"))
    );
    let saved = sessions.inner.borrow();
    let saved = saved.as_ref().unwrap();
    assert!(saved.skills.snapshot(saved.input_revision).is_some());
    assert!(
        !saved
            .conversation
            .iter()
            .any(|message| message.text.contains("# Issue analysis"))
    );
    assert_eq!(
        saved
            .conversation
            .iter()
            .filter(|message| message.role == ChatRole::Tool
                && message.text.contains("\"status\":\"loaded\""))
            .count(),
        2
    );
}

#[tokio::test]
async fn pure_action_preview_completes_without_a_device_executor_on_both_entries() {
    for device in ["", "device"] {
        let sessions = MemSession::default();
        let mut seed =
            PersistedAgentSession::new("conv", "actor", device, 1, scope(), "2026-11-05T00:00:00Z");
        seed.adopt_client_metadata(
            Some("client-conversation"),
            crate::session::AgentSessionSurface::AiAssistant,
        );
        seed.input_revision = 1;
        seed.begin_focus_epoch(1, Vec::new()).unwrap();
        seed.bind_assistant_input_targets(&[]).unwrap();
        let app = serde_json::json!({"token":"app","snapshot_id":"ui","object_kind":"application","expires_at":""});
        let target = serde_json::json!({"token":"button","snapshot_id":"ui","object_kind":"ui_element","expires_at":""});
        let node = |reference: serde_json::Value, role: &str| serde_json::json!({"object_ref":reference,"parent_index":null,"role":role,"name":"Calendar","value":null,"is_protected":false,"enabled":true,"supported_actions":["invoke"],"location":{"status":"unavailable"}});
        seed.conversation = crate::result_model_ids::tests::evidence(
            "inspect_desktop_ui",
            "ui-read",
            serde_json::json!({"queries":["Calendar"]}),
            serde_json::json!({"ReadContext":{"DesktopUiInspect":{"snapshot_id":"ui","adapter":{"kind":"macos_accessibility","version":crate::ai_assistant::MACOS_ACCESSIBILITY_ADAPTER_VERSION},"nodes":[node(app,"application"),node(target,"button")],"truncated":false}}}),
        );
        for message in &mut seed.conversation {
            if message.role == ChatRole::Assistant {
                message.replay_disposition = tool_meta().replay;
            }
            if let Some(label) = message.data_envelope.as_mut() {
                label.allowed_destinations = vec![
                    desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                        connection_id: "gateway".into(),
                        connection_revision: 1,
                        model_id: "model".into(),
                        profile_revision: 1,
                    },
                ];
            }
        }
        seed.ui_references.observe(&seed.conversation).unwrap();
        *sessions.inner.borrow_mut() = Some(seed);
        let mut preview = tool_use("preview", crate::ai_assistant::PREVIEW_COMPUTER_ACTION_TOOL);
        preview.tool_calls[0].arguments_json = serde_json::json!({"risk":"low","reversible":true,"data_egress":false,"actions":[{"target_id":"e1","action":{"adapter":"ui_in_application","action":{"application_id":"a1","action":{"kind":"invoke"}}},"before_summary":"Button observed","after_intent":"requested change","verification":"Read panel state"}]}).to_string();
        let model = SkillModel(ScriptModel {
            turns: RefCell::new([preview, answer("done")].into()),
            requests: Rc::new(RefCell::new(vec![])),
        });
        let tools = RecordingTools {
            calls: Rc::new(RefCell::new(vec![])),
            reply: "unused".into(),
        };
        let registry = crate::assistant_policy::targetless_web_internal_tools();
        let clock = || "2026-11-05T00:00:00Z".to_string();
        let mut params = claim();
        params.device_id = device.into();
        assert_eq!(
            run_agent_turn(
                &deps(&sessions, &model, &tools, &registry, &clock),
                params,
                ChatMessage::text("user", ChatRole::User, "Preview the requested change"),
                &mut NullTurnSink
            )
            .await
            .unwrap(),
            LoopOutcome::Answered("done".into())
        );
        assert!(tools.calls.borrow().is_empty());
        let saved = sessions.inner.borrow();
        let saved = saved.as_ref().unwrap();
        let result = saved
            .conversation
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some("preview"))
            .unwrap();
        assert_eq!(result.tool_ok, Some(true), "{}", result.text);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&result.text).unwrap()["actions"][0]["after_intent"],
            "requested change"
        );
        assert!(saved.permission_requests.is_empty());
    }
}
