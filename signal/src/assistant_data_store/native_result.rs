//! Native completion history is the final phase of the original writer. It
//! cannot execute an action, change settled usage, or authorize a model read.

use super::AssistantStoreError;
use crate::{
    config::{ConfigConnection, connection::DatabaseTransaction},
    entity::{agent_assistant_model_output as archived, agent_session},
};
use desk_agent_protocol::{
    ai_assistant::{
        source::{AssistantSourceKind, HistoryDependency, SourcePolicyClass, SourceUsePolicy},
        user_instructions::AssistantDataPolicy,
    },
    computer_use::ComputerActionCompleted,
};
use desk_diagnose_core::{
    action_result::{ActionResultOrigin, ActionResultReceipt},
    ai_assistant::{
        native_history::{
            NativeSendHistoryContext, NativeSendHistoryError, NativeSendHistoryProjection,
        },
        source::HistoryCapture,
    },
    native_ids::{self, Kind},
    seam::ToolRunOutput,
    session::{AgentSessionSurface, PersistedAgentSession},
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QuerySelect, TransactionTrait};

#[derive(Clone, Copy)]
pub(crate) struct NativeSendResult<'a> {
    pub origin: &'a ActionResultOrigin,
    pub receipt: &'a ActionResultReceipt,
    pub output: &'a ToolRunOutput,
    pub canonical_input: &'a str,
    pub completion: &'a ComputerActionCompleted,
    pub provider_device_id: &'a str,
    pub dispatched_at_unix_ms: u64,
}

struct PreparedCopy {
    actor: String,
    instance: String,
    audience: String,
    conversation: String,
    device: String,
    input_seq: u64,
    timezone: String,
    native_id: String,
    fields: NativeSendHistoryProjection,
    policy: AssistantDataPolicy,
    source_policy: SourceUsePolicy,
    dependencies: Vec<HistoryDependency>,
}

enum Prepared {
    Gap {
        actor: String,
        recorded_at: u64,
        policy: AssistantDataPolicy,
        reason: &'static str,
    },
    Copy(Box<PreparedCopy>),
}

/// Call after all original native result/outbox writes. The caller keeps the
/// actual ConfigContext SQLite writer fence; no source/native I/O may follow.
pub(crate) async fn capture_on(
    db: &DatabaseTransaction,
    native: NativeSendResult<'_>,
) -> Result<(), AssistantStoreError> {
    let source = db.begin().await?;
    let result = desk_diagnose_core::future::boxed(|| capture_prepared_on(&source, native)).await;
    match result {
        Ok(()) => {
            source.commit().await?;
            Ok(())
        }
        Err(error) => {
            source.rollback().await?;
            let reason = match error {
                AssistantStoreError::Backend(_) => "omitted_error",
                AssistantStoreError::Capacity => "omitted_capacity",
                _ => "omitted_policy",
            };
            fallback_gap_on(db, native, reason).await
        }
    }
}

async fn capture_prepared_on(
    db: &DatabaseTransaction,
    native: NativeSendResult<'_>,
) -> Result<(), AssistantStoreError> {
    let fence = &native.origin.turn_fence;
    let root = fence
        .delegation
        .as_ref()
        .map_or(fence.conversation_id.as_str(), |binding| {
            binding.root_conversation_id.as_str()
        });
    let scope = super::native_status::scope_on(db, &fence.actor_id, &fence.device_id, root).await?;
    let Some(scope) = scope else {
        return Ok(());
    };
    let status = scope.prepare_on(db).await?;
    let prepared = desk_diagnose_core::future::boxed(|| prepare_on(db, native)).await?;
    let status_capture = status.as_ref().map(|value| value.capture());
    let receipt_capture = match &prepared {
        Some(Prepared::Copy(copy)) => Some(copy.capture()),
        _ => None,
    };
    let captures = status_capture
        .iter()
        .chain(receipt_capture.iter())
        .collect::<Vec<_>>();
    let pending = super::history::staged::prepare_many_on(db, &captures, &scope.policy).await?;
    if let Some(status) = &status {
        status.validate_on(db).await?;
    }
    if let Some(Prepared::Gap {
        actor,
        recorded_at,
        policy,
        reason,
    }) = &prepared
    {
        super::history::record_scope_gap_on(db, actor, *recorded_at, policy, reason).await?;
    }
    if let Some(Prepared::Copy(copy)) = &prepared {
        let deadline = copy
            .dependencies
            .iter()
            .filter_map(|source| source.expires_at_unix_ms)
            .chain(copy.source_policy.content_hard_expires_at_unix_ms)
            .chain(
                copy.fields
                    .recorded_at_unix_ms()
                    .checked_add(u64::from(copy.policy.history_retention_days) * 86_400_000),
            )
            .min();
        if deadline.is_some_and(|until| until <= chrono::Utc::now().timestamp_millis() as u64) {
            return Err(AssistantStoreError::Inaccessible);
        }
    }
    pending.finalize_on().await?;
    Ok(())
}

async fn fallback_gap_on(
    db: &DatabaseTransaction,
    native: NativeSendResult<'_>,
    reason: &'static str,
) -> Result<(), AssistantStoreError> {
    let fence = &native.origin.turn_fence;
    if fence.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()
        || fence.delegation.is_some()
        || !matches!(
            native.origin.tool_name.as_str(),
            "send_gmail_message" | "send_slack_message"
        )
    {
        return Ok(());
    }
    let Some(row) = agent_session::Entity::find()
        .filter(agent_session::Column::ActorId.eq(&fence.actor_id))
        .filter(agent_session::Column::ConversationId.eq(&fence.conversation_id))
        .filter(agent_session::Column::DeviceId.eq(&fence.device_id))
        .one(db)
        .await?
    else {
        return Ok(());
    };
    let session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| AssistantStoreError::Inaccessible)?;
    let Some(data) = session
        .assistant_data
        .as_ref()
        .filter(|data| !data.temporary)
    else {
        return Ok(());
    };
    let config = db.config_read().await;
    if session.actor_id != fence.actor_id
        || session.device_id != fence.device_id
        || session.conversation_id != fence.conversation_id
        || session.surface != AgentSessionSurface::AiAssistant
        || !session.agent_role.is_main()
        || data.source_instance_id.as_deref() != Some(config.metadata.instance.as_str())
        || super::memory::source_device_on(db, &fence.actor_id, &data.source_device_ref)
            .await?
            .is_none()
    {
        return Ok(());
    }
    super::history::record_scope_gap_on(
        db,
        &fence.actor_id,
        native.receipt.received_at_unix_ms,
        &config.assistant_data_policy,
        reason,
    )
    .await
}

async fn prepare_on(
    db: &DatabaseTransaction,
    native: NativeSendResult<'_>,
) -> Result<Option<Prepared>, AssistantStoreError> {
    let origin = native.origin;
    if !matches!(
        origin.tool_name.as_str(),
        "send_gmail_message" | "send_slack_message"
    ) || origin.turn_fence.delegation.is_some()
    {
        return Ok(None);
    }
    let actor = &origin.turn_fence.actor_id;
    if actor != &crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Ok(None);
    }
    if !super::source_lifecycle::conversation_readable_on(
        db,
        actor,
        &origin.turn_fence.conversation_id,
        &origin.turn_fence.conversation_id,
    )
    .await?
    {
        return Ok(None);
    }
    let Some(row) = agent_session::Entity::find()
        .filter(agent_session::Column::ActorId.eq(actor))
        .filter(agent_session::Column::ConversationId.eq(&origin.turn_fence.conversation_id))
        .filter(agent_session::Column::DeviceId.eq(&origin.turn_fence.device_id))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    let session = Box::new(
        PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| AssistantStoreError::Inaccessible)?,
    );
    if session.actor_id != *actor
        || session.conversation_id != origin.turn_fence.conversation_id
        || session.device_id != origin.turn_fence.device_id
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    if session.surface != AgentSessionSurface::AiAssistant || !session.agent_role.is_main() {
        return Ok(None);
    }
    let Some(data) = session
        .assistant_data
        .as_ref()
        .filter(|data| !data.temporary)
    else {
        return Ok(None);
    };
    data.validate().map_err(AssistantStoreError::Invalid)?;
    let config = db.config_read().await;
    if data.source_instance_id.as_deref() != Some(config.metadata.instance.as_str())
        || super::memory::source_device_on(db, actor, &data.source_device_ref)
            .await?
            .is_none()
    {
        return Ok(None);
    }
    let policy = config.assistant_data_policy.clone();
    let gap = |reason| Prepared::Gap {
        actor: actor.clone(),
        recorded_at: native.receipt.received_at_unix_ms,
        policy: policy.clone(),
        reason,
    };
    let rows = archived::Entity::find()
        .filter(archived::Column::ActorId.eq(actor))
        .filter(archived::Column::ConversationId.eq(&origin.turn_fence.conversation_id))
        .filter(archived::Column::OutputEnvelopeId.is_in(origin.source_envelope_ids.clone()))
        .limit(2)
        .all(db)
        .await?;
    if rows.len() != 1 {
        return Ok(Some(gap("omitted_policy")));
    }
    let original = match super::output_archive::decode_row_on(
        db,
        rows.into_iter().next().expect("one original producer"),
    )
    .await
    {
        Ok(value) => value,
        Err(AssistantStoreError::Backend(error)) => return Err(error.into()),
        Err(_) => return Ok(Some(gap("omitted_policy"))),
    };
    let proof = &original.proof;
    if proof.source_set.source_device_ref != data.source_device_ref
        || proof.input_revision != origin.turn_fence.input_revision
        || proof.source_set.context.lease_token != origin.turn_fence.lease_token
        || proof.actor_input_seq > data.current_actor_input_seq
    {
        return Ok(Some(gap("omitted_policy")));
    }
    let fields = match NativeSendHistoryProjection::from_web_action(
        NativeSendHistoryContext {
            origin,
            result: native.receipt,
            output: native.output,
            provider_device_id: native.provider_device_id,
            dispatched_at_unix_ms: native.dispatched_at_unix_ms,
            original_source_contract_revision: proof.source_set.context.source_contract_revision,
            maximum_bytes: 32 * 1024,
        },
        native.canonical_input,
        native.completion,
    ) {
        Ok(Some(value)) => value,
        Ok(None) => return Ok(None),
        Err(NativeSendHistoryError::Capacity) => return Ok(Some(gap("omitted_capacity"))),
        Err(NativeSendHistoryError::InvalidOriginal) => return Ok(Some(gap("omitted_policy"))),
    };
    let mut known = proof
        .sources
        .input_snapshot
        .policy_sources()
        .into_iter()
        .map(|source| source.envelope_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    known.insert(&proof.output.envelope_id);
    if origin
        .source_envelope_ids
        .iter()
        .any(|source| !known.contains(source.as_str()))
    {
        return Ok(Some(gap("omitted_policy")));
    }
    let sources = super::visible_history::retained_native_sources(proof);
    let Some(sources) = sources else {
        return Ok(Some(gap("omitted_policy")));
    };
    let mut source_policy = sources.policy;
    source_policy.class = SourcePolicyClass::CommunicationReceipt;
    source_policy.independent_fact_allowed = false;
    let native_id = native_ids::stable(
        Kind::Message,
        &(
            "native-send-history-v1",
            &native.receipt.action,
            &native.receipt.envelope.envelope_id,
        ),
    );
    Ok(Some(Prepared::Copy(Box::new(PreparedCopy {
        actor: actor.clone(),
        instance: config.metadata.instance.clone(),
        audience: session.device_id.clone(),
        conversation: origin.turn_fence.conversation_id.clone(),
        device: data.source_device_ref.clone(),
        input_seq: proof.actor_input_seq,
        timezone: proof.statement_timezone.clone(),
        native_id,
        fields,
        policy,
        source_policy,
        dependencies: Vec::new(),
    }))))
}

impl PreparedCopy {
    fn capture(&self) -> HistoryCapture<'_> {
        HistoryCapture {
            native: Some(self.fields.metadata()),
            actor_id: &self.actor,
            device_ref: &self.device,
            directory: Some(
                desk_diagnose_core::ai_assistant::source::HistorySourceDirectory {
                    instance_id: &self.instance,
                    audience: &self.audience,
                    device_name: None,
                },
            ),
            conversation_id: &self.conversation,
            root_conversation_id: &self.conversation,
            native_id: &self.native_id,
            content: self.fields.content(),
            envelope: self.fields.original_envelope(),
            source_policy: &self.source_policy,
            dependencies: &self.dependencies,
            accepted_input_seq: self.input_seq,
            kind: AssistantSourceKind::Receipt,
            role: "tool",
            recorded_at_unix_ms: self.fields.recorded_at_unix_ms(),
            statement_timezone: &self.timezone,
            revision: 1,
        }
    }
}
