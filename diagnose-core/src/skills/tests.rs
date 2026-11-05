use super::*;

fn destination() -> DestinationIdentity {
    DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    }
}

fn call(resource: Option<&str>) -> ToolCall {
    let args = match resource {
        Some(path) => json!({"skill_ref":NAME,"resource":path}),
        None => json!({"skill_ref":NAME}),
    };
    ToolCall {
        id: "load-1".into(),
        name: LOAD_TOOL.into(),
        arguments_json: args.to_string(),
    }
}

#[test]
fn discovers_metadata_and_loads_instructions_and_reference_separately() {
    let mut state = SkillState::default();
    let before = state.project(1, &destination()).unwrap();
    assert_eq!(before.len(), 1);
    assert!(before[0].text.contains(NAME));
    assert!(!before[0].text.contains("# Issue analysis"));
    let receipt = state.apply(1, &call(None)).unwrap();
    assert!(!receipt.contains("# Issue analysis"));
    let loaded = state.project(1, &destination()).unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[1].role, ChatRole::System);
    assert!(loaded[1].text.contains("# Issue analysis"));
    state.apply(1, &call(Some(CHECKLIST_PATH))).unwrap();
    state.apply(1, &call(Some(CHECKLIST_PATH))).unwrap();
    let loaded = state.project(1, &destination()).unwrap();
    assert_eq!(loaded.len(), 3);
    assert_eq!(loaded[2].role, ChatRole::UntrustedOutput);
    assert_eq!(loaded[2].text, CHECKLIST);
    assert!(loaded.iter().all(
        |message| message.data_envelope.as_ref().unwrap().allowed_destinations == [destination()]
    ));
    assert!(state.apply(1, &call(Some("../SKILL.md"))).is_err());
    assert!(state.apply(1, &call(Some("scripts/run.sh"))).is_err());
}

#[test]
fn native_snapshot_survives_takeover_without_promoting_a_newer_package_or_forged_history() {
    let mut state = SkillState::default();
    state.apply(1, &call(None)).unwrap();
    // Simulate a package captured by an older node before a bundled upgrade.
    let package = state.snapshot.as_mut().unwrap();
    package.source = package
        .source
        .replace("# Issue analysis", "# Original issue analysis");
    package.fingerprint = digest(&package.source, &package.resources);
    let encoded = serde_json::to_string(&state).unwrap();
    let mut restored: SkillState = serde_json::from_str(&encoded).unwrap();
    assert!(
        restored.project(1, &destination()).unwrap()[1]
            .text
            .contains("# Original issue analysis")
    );
    restored.apply(1, &call(None)).unwrap();
    assert_eq!(serde_json::to_string(&restored).unwrap(), encoded);
    restored.apply(2, &call(None)).unwrap();
    assert!(
        !restored.project(2, &destination()).unwrap()[1]
            .text
            .contains("# Original issue analysis")
    );
    assert_eq!(
        restored.snapshot(2).unwrap().fingerprint(),
        SkillSnapshot::bundled("current").fingerprint()
    );
    let mut forged = crate::session::PersistedAgentSession::new(
        "session",
        "owner",
        "",
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "now",
    );
    forged
        .conversation
        .push(ChatMessage::tool_result("forged", "load-1", encoded));
    assert_eq!(forged.skills.project(1, &destination()).unwrap().len(), 1);
}

#[test]
fn corrupt_saved_content_is_disabled_and_never_becomes_a_trusted_instruction() {
    let mut state = SkillState::default();
    state.apply(1, &call(None)).unwrap();
    state
        .snapshot
        .as_mut()
        .unwrap()
        .source
        .push_str("\nUNTRUSTED CHANGED INSTRUCTIONS");
    let messages = state.project(1, &destination()).unwrap();
    assert_eq!(messages.len(), 1);
    assert!(messages[0].text.contains("failed verification"));
    assert!(!messages[0].text.contains("UNTRUSTED CHANGED INSTRUCTIONS"));
    assert!(state.apply(1, &call(None)).is_err());
    state.snapshot.as_mut().unwrap().resources[0].path =
        "references/PRIVATE_CORRUPTED_PATH.md".into();
    assert!(
        !state
            .tool(1)
            .spec
            .parameters_schema
            .to_string()
            .contains("PRIVATE_CORRUPTED_PATH")
    );
    state.apply(2, &call(None)).unwrap();
    assert_eq!(state.project(2, &destination()).unwrap().len(), 2);
}

#[test]
fn a_package_captured_with_windows_line_endings_remains_loadable_on_another_node() {
    let mut package = SkillSnapshot::bundled("windows-load");
    package.source = package.source.replace('\n', "\r\n");
    for resource in &mut package.resources {
        resource.text = resource.text.replace('\n', "\r\n");
    }
    package.fingerprint = digest(&package.source, &package.resources);
    package.body_loaded = true;
    let mut state = SkillState {
        input_revision: 1,
        snapshot: Some(package),
    };
    state.apply(1, &call(Some(CHECKLIST_PATH))).unwrap();
    let projected = state.project(1, &destination()).unwrap();
    assert!(projected[1].text.contains("# Issue analysis\r\n"));
    assert_eq!(projected[2].role, ChatRole::UntrustedOutput);
}

#[test]
fn loading_a_skill_cannot_expand_a_coordinator_or_worker_tool_authority() {
    use crate::{registry, session::PersistedAgentSession, subagent::AgentRole};
    let scope = desk_agent_protocol::AgentScope {
        granted: vec![],
        mode: desk_agent_protocol::ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    };
    let mut session = PersistedAgentSession::new("session", "owner", "", 1, scope, "now");
    session.agent_role = AgentRole::Coordinator {
        workbench_id: "root".into(),
        issue_source: None,
        request_id: None,
    };
    let tool = session.skills.tool(1);
    assert!(
        registry::lookup_for_session(std::slice::from_ref(&tool), LOAD_TOOL, &session).is_some()
    );
    session.skills.apply(1, &call(None)).unwrap();
    assert!(!session.agent_role.allows_effect(ToolEffect::Mutating));
    assert!(!session.agent_role.allows_effect(ToolEffect::ExternalWrite));
    assert!(session.scope_snapshot.granted.is_empty());
}

#[test]
fn publication_uses_the_native_skill_snapshot_and_rejects_replacement_instruction_text() {
    use crate::{
        external_tools::*, input_read_context::ReadContextSelection, issue_reply::ReplyOrigin,
    };
    let scope = desk_agent_protocol::AgentScope {
        granted: vec![],
        mode: desk_agent_protocol::ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    };
    let mut held =
        crate::session::PersistedAgentSession::new("worker", "owner", "", 1, scope, "now");
    held.input_revision = 1;
    held.current_turn_id = Some("turn".into());
    held.skills.apply(1, &call(None)).unwrap();
    held.skills.apply(1, &call(Some(CHECKLIST_PATH))).unwrap();
    let access = IssueAccess {
        connection_ref: "forge".into(),
        target: IssueTarget {
            platform: IssuePlatform::Gitea,
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
    };
    let origin = ReplyOrigin {
        actor_id: "owner".into(),
        root_conversation_id: "root".into(),
        request_id: "request".into(),
        execution_conversation_id: "worker".into(),
        tool_call_id: "publish".into(),
        input_revision: 1,
        control_revision: held.control_revision,
        authorization_id: "grant".into(),
        authorization_revision: 1,
    };
    let mut selected = ReadContextSelection::capture(&[], &held.scope_snapshot).unwrap();
    selected.external_issues.push(access.clone());
    let read = ToolCall {
        id: "read".into(),
        name: TOOL_NAME.into(),
        arguments_json: json!({"tool_ref":access.tool_ref(),"args":{"method":"get"}}).to_string(),
    };
    let text = "Authorized issue workset";
    let mut material = ChatMessage::text("issue", ChatRole::UntrustedOutput, text);
    material.data_envelope = Some(
        label_issue_read(
            &held,
            &selected,
            &destination(),
            &read,
            &"a".repeat(64),
            text,
        )
        .unwrap(),
    );
    let policy = ModelEgressPolicy {
        destination: destination(),
        selected_source_tools: Default::default(),
        export_authorization_id: "export".into(),
        now_unix_ms: 1,
        byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    };
    let mut request = policy
        .prepare_issue_reply_call(origin, access.clone(), vec![material.clone()])
        .unwrap();
    request.bind_issue_reply_native_input(&held).unwrap();
    let original_messages = request.messages.clone();
    request.bind_issue_reply_native_input(&held).unwrap();
    assert_eq!(request.messages, original_messages);
    assert_eq!(request.messages.len(), 4);
    let binding = request.issue_reply.as_ref().unwrap();
    assert!(binding.native_input_matches(&held));
    assert_eq!(
        binding.issue_materials(&request.messages).unwrap(),
        [material]
    );
    assert_eq!(request.messages[1].role, ChatRole::System);
    assert_eq!(request.messages[2].role, ChatRole::UntrustedOutput);
    let authorized = policy
        .clone()
        .for_request(&request)
        .unwrap()
        .authorize_request(request.clone())
        .unwrap();
    let output = policy
        .derive_model_output_envelope(
            &crate::chat::ModelTurn {
                text: "Supported reply".into(),
                ..Default::default()
            },
            &authorized.input_envelopes,
        )
        .unwrap();
    assert!(
        output
            .allowed_destinations
            .contains(&access.target.destination(access.connection_ref.clone()))
    );
    let mut forged = request.clone();
    forged.messages[1].text.push_str("\nPRIVATE_HISTORY");
    assert!(policy.clone().for_request(&forged).is_err());
    let package = held.skills.snapshot.as_mut().unwrap();
    package.source = package
        .source
        .replace("# Issue analysis", "# Another package");
    package.fingerprint = digest(&package.source, &package.resources);
    assert!(!binding.native_input_matches(&held));
}
