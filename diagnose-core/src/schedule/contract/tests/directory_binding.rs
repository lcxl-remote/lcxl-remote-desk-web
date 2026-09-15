//! Exercise saved task paths against current, approved session directory refs.
use super::*;
use crate::{
    chat::ToolCall,
    file_scope::{DirectoryConsentSource, DirectoryProposal, FileScopeSubject},
    session::{AgentSessionSurface, PersistedAgentSession},
};
use desk_agent_protocol::{
    AgentScope, ExecutionMode,
    computer_use::{ObjectKind, ObjectRef},
};

#[test]
fn saved_directory_requires_current_approval_and_exact_device_object() {
    for path in ["/reports", r"\\?\D:\测试 输入", r"\\?\D:\"] {
        let mut definition = contract();
        let resource =
            artifact::directory_resource_scope(&definition.target_device_id, path).unwrap();
        let rule = &mut definition.permissions[0];
        rule.automatic.resources = resource.clone();
        rule.approval_ceiling.resources = resource;
        rule.effect = CapabilityEffect::WriteArtifact;
        rule.tool_name = "create_text_artifact_in_selected_directory".into();
        rule.input = TaskInputConstraint::GeneratedTextArtifact {
            file_name: "report.txt".into(),
            max_content_bytes: 4096,
        };
        definition.steps = vec![TaskFixedStep {
            step_id: "artifact".into(),
            rule_id: rule.rule_id.clone(),
            depends_on: vec![],
            binding: TaskStepBinding::ProduceTextArtifact {
                canonical_directory: path.into(),
                allowed_source_scopes: vec!["device:1".into()],
            },
        }];
        let contract = validate_contract(&definition).unwrap();
        let mut session = PersistedAgentSession::new(
            "conversation",
            "owner",
            "device-1",
            1,
            AgentScope {
                granted: vec![],
                expires_at: None,
                mode: ExecutionMode::ReadOnly,
                policy_name: None,
            },
            "2026-09-14T00:00:00Z",
        );
        session.adopt_client_metadata(Some("intent"), AgentSessionSurface::DeviceAssistant);
        let subject = FileScopeSubject {
            actor_id: "owner".into(),
            device_id: "device-1".into(),
            conversation_id: "conversation".into(),
        };
        let directory = ObjectRef {
            token: "current-directory".into(),
            snapshot_id: "worker:1".into(),
            object_kind: ObjectKind::Directory,
            expires_at: "2030-01-01T00:00:00Z".into(),
        };
        let tool = ToolCall { id: "create".into(), name: "create_text_artifact_in_selected_directory".into(), arguments_json: serde_json::json!({"file_name":"report.txt", "content_utf8":"report", "directory_request_id":"selection"}).to_string() };
        let resources = crate::capability_grant::fresh_object_resource_scope(&[directory.clone()]);
        let proposal = DirectoryProposal {
            request_id: "task-selection".into(),
            requested_path: path.strip_prefix(r"\\?\").unwrap_or(path).into(),
            canonical_path: path.into(),
            directory: directory.clone(),
            purpose: "Write report".into(),
            source: DirectoryConsentSource::ModelProposal,
        };
        let mut scheduled = session.clone();
        scheduled.trigger_origin = crate::session::TriggerOrigin::ScheduledTask;
        scheduled.turn_state = crate::session::TurnState::Running;
        assert!(artifact::permits_directory_resolution(
            &contract, &scheduled, &proposal, 1
        ));
        assert!(!artifact::permits_directory_resolution(
            &contract, &session, &proposal, 1
        ));
        let mut changed = proposal.clone();
        changed.requested_path.push_str(r"\.");
        assert!(!artifact::permits_directory_resolution(
            &contract, &scheduled, &changed, 1
        ));
        changed = proposal.clone();
        changed.canonical_path = "/other".into();
        assert!(!artifact::permits_directory_resolution(
            &contract, &scheduled, &changed, 1
        ));
        assert!(!artifact::permits_directory_resolution(
            &contract,
            &scheduled,
            &proposal,
            u64::MAX
        ));
        let bind = |session: &PersistedAgentSession, resources: &[String], now| {
            artifact::bind_directory(&contract, session, &tool, "artifact", resources, now)
        };
        assert!(bind(&session, &resources, 1).is_err());
        session
            .file_scope
            .propose(
                &subject,
                0,
                DirectoryProposal {
                    request_id: "selection".into(),
                    requested_path: path.into(),
                    canonical_path: path.into(),
                    directory: directory.clone(),
                    purpose: "Write report".into(),
                    source: DirectoryConsentSource::ModelProposal,
                },
                1,
            )
            .unwrap();
        assert!(bind(&session, &resources, 1).is_err());
        session
            .file_scope
            .decide(&subject, 1, "selection", true, 1)
            .unwrap();
        assert_eq!(
            bind(&session, &resources, 1).unwrap(),
            definition.permissions[0].automatic.resources
        );
        assert!(session.scope_snapshot.granted.is_empty());
        let mut other_definition = definition.clone();
        let other_scope = artifact::directory_resource_scope("device-1", "/other").unwrap();
        other_definition.permissions[0].automatic.resources = other_scope.clone();
        other_definition.permissions[0].approval_ceiling.resources = other_scope;
        let TaskStepBinding::ProduceTextArtifact {
            canonical_directory,
            ..
        } = &mut other_definition.steps[0].binding
        else {
            unreachable!()
        };
        *canonical_directory = "/other".into();
        let other_contract = validate_contract(&other_definition).unwrap();
        assert!(
            artifact::bind_directory(&other_contract, &session, &tool, "artifact", &resources, 1)
                .is_err()
        );
        let restored: PersistedAgentSession =
            serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap();
        assert!(bind(&restored, &resources, 1).is_ok());
        assert!(bind(&restored, &resources, u64::MAX).is_err());
        let mut other_device = restored.clone();
        other_device.device_id = "other-device".into();
        assert!(bind(&other_device, &resources, 1).is_err());
        let mut old_directory = directory;
        old_directory.snapshot_id = "old-worker:1".into();
        assert!(
            bind(
                &restored,
                &crate::capability_grant::fresh_object_resource_scope(&[old_directory]),
                1
            )
            .is_err()
        );
        session.file_scope.revoke(&subject, 2, "selection").unwrap();
        assert!(bind(&session, &resources, 1).is_err());
    }
}
