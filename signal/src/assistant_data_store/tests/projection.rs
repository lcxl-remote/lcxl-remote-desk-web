use desk_agent_protocol::{
    ai_assistant::source::SourceReadPurpose, data_lineage::DestinationIdentity,
};
use desk_diagnose_core::{
    ai_assistant::{
        read_projection::{ReadProjectionKind, ReadProjectionProof},
        seam::AssistantDataReadUse,
    },
    model_message_labels::model_bound_user_message,
    native_ids::{self, Kind},
};

// These fixtures exercise storage only. No prepared row is a physical send grant.
pub(super) fn proof(content: &str, revision: &str) -> ReadProjectionProof {
    let envelope = model_bound_user_message(
        format!("source-{revision}"),
        content.into(),
        DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
    )
    .unwrap()
    .data_envelope
    .unwrap();
    ReadProjectionProof {
        kind: ReadProjectionKind::CurrentMemories,
        actor_id: "1".into(),
        root_conversation_id: "root".into(),
        source_device_ref: "device".into(),
        input_revision: 1,
        actor_input_seq: 1,
        task_id: None,
        producer_message_id: None,
        producer_output_envelope_id: None,
        tool_call_id: None,
        tool_name: "current_memories".into(),
        logical_call_id: "preparation-1".into(),
        request_sha256: "a".repeat(64),
        usage: AssistantDataReadUse {
            decision_roots: Vec::new(),
            projection_id: native_ids::stable(Kind::AssistantReadProjection, &revision),
            source_envelope_id: envelope.envelope_id.clone(),
            purpose: SourceReadPurpose::ModelMemory,
            retrieval_episode_id: "episode".into(),
            scope_sha256: "b".repeat(64),
            policy_revision: 1,
            control_generation: 1,
            memory_state_generation: 1,
            roots: Vec::new(),
            history_dependencies: Vec::new(),
            memories: Vec::new(),
            expires_at_unix_ms: None,
        },
        source_envelope: envelope,
        snapshot_policy: None,
        created_at_unix_ms: 1000,
        unused_until_unix_ms: 1000
            + desk_agent_protocol::ai_assistant::limits::DEFAULT_UNUSED_PROJECTION_TTL_MS,
    }
}
