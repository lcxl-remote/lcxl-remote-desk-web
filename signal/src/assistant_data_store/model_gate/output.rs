//! Completion checks share the native output-acceptance transaction.

use super::*;
use desk_diagnose_core::ai_assistant::native_export::AssistantNativeSourceSet;

pub(crate) struct PreparedOutput {
    set: Box<AssistantNativeSourceSet>,
    raw_set: String,
    row: agent_session::Model,
    session: Box<PersistedAgentSession>,
    policy: AssistantDataPolicy,
}

/// Call before taking the native output row lock. Owner controls use the same
/// actor/device/conversation order and serialize with the final source checks.
pub(crate) async fn prepare_on(
    db: &DatabaseTransaction,
    raw: Option<&str>,
) -> Result<Option<Box<PreparedOutput>>, AssistantStoreError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let set =
        Box::new(AssistantNativeSourceSet::decode(raw).map_err(AssistantStoreError::Invalid)?);
    if set.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Err(AssistantStoreError::Inaccessible);
    }
    super::super::ensure_runtime_on(db, &set.actor_id).await?;
    let config = db.config_read().await;
    let policy = config.assistant_data_policy.clone();
    let row = session_on(db, &set).await?;
    let session = Box::new(
        PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| AssistantStoreError::Invalid("invalid assistant completion session"))?,
    );
    validate_session(
        &row,
        &session,
        &set,
        chrono::Utc::now().timestamp_millis() as u64,
    )?;
    let binding = crate::assistant_data_store::source_device_ref(
        &config.metadata.instance,
        &session.device_id,
    );
    if set.source_device_ref != binding
        || session.assistant_data.as_ref().is_none_or(|data| {
            data.source_instance_id.as_deref() != Some(config.metadata.instance.as_str())
        })
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(Some(Box::new(PreparedOutput {
        set,
        raw_set: raw.into(),
        row,
        session,
        policy,
    })))
}

/// A provider already in flight may finish after its short send deadline. Its
/// actual sources, task and current holder must still permit accepting output.
pub(crate) fn validate_on<'a>(
    db: &'a DatabaseTransaction,
    prepared: Option<&'a PreparedOutput>,
    native: &'a native::Model,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Option<u64>, AssistantStoreError>> + Send + 'a>,
> {
    desk_diagnose_core::future::send_phase(|| async move {
        let Some(prepared) = prepared else {
            if native.assistant_source_set_json.is_some() {
                return Err(AssistantStoreError::Inaccessible);
            }
            return Ok(None);
        };
        let set = &prepared.set;
        if set.context.authorized_at_unix_ms != unsigned(native.authorized_at.timestamp_millis())? {
            return Err(AssistantStoreError::Inaccessible);
        }
        if set.context.policy_revision != prepared.policy.revision {
            return Err(AssistantStoreError::Inaccessible);
        }
        if native.assistant_source_set_json.as_deref() != Some(prepared.raw_set.as_str())
            || native.receipt_id != set.physical_call_id
            || native.context_conversation_id.as_deref()
                != Some(set.consumer_conversation_id.as_str())
            || native.state != crate::model_egress_store::STATE_DISPATCH_INTENT
            || native.completed_at.is_some()
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        let (audit, inputs, _) = crate::model_egress_store::receipt_inputs(native)?;
        if set.model_export_authorization_id != native.export_authorization_id
            || set.destination != audit.destination
            || set.input_lineage_sha256
                != input_lineage_sha256(&inputs).map_err(AssistantStoreError::Invalid)?
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        set.validate_input_lineage(&inputs)
            .map_err(AssistantStoreError::Invalid)?;
        let row = session_on(db, set).await?;
        if row.state_json != prepared.row.state_json
            || row.version != prepared.row.version
            || row.lease_token != prepared.row.lease_token
        {
            return Err(rejected("session_snapshot_changed"));
        }
        let now = chrono::Utc::now().timestamp_millis() as u64;
        validate_session(&row, &prepared.session, set, now)?;
        let mut deadline = unsigned(
            row.lease_deadline
                .ok_or(AssistantStoreError::Inaccessible)?
                .timestamp_millis(),
        )?;
        if let Some(until) = super::task_deadline_on(db, &prepared.session, now).await? {
            deadline = deadline.min(until);
        }

        if deadline <= chrono::Utc::now().timestamp_millis() as u64 {
            return Err(rejected("acceptance_deadline_elapsed"));
        }
        Ok(Some(deadline))
    })
}

pub(crate) async fn archive_on<'a>(
    db: &'a DatabaseTransaction,
    prepared: Option<&PreparedOutput>,
    native: &native::Model,
    output: &desk_agent_protocol::data_lineage::DataEnvelope,
    completed_at_unix_ms: u64,
) -> Result<Option<Box<super::super::output_archive::PendingOutputArchive<'a>>>, AssistantStoreError>
{
    let Some(prepared) = prepared else {
        return Ok(None);
    };
    let (audit, inputs, input_snapshot) = crate::model_egress_store::receipt_inputs(native)?;
    let evidence = desk_diagnose_core::model_egress::NativeModelOutputEvidence::from_receipt(
        &native.receipt_id,
        &prepared.set.consumer_conversation_id,
        &native.export_authorization_id,
        unsigned(native.authorized_at.timestamp_millis())?,
        completed_at_unix_ms,
        &audit,
        &inputs,
        &input_snapshot,
        output,
    )
    .map_err(AssistantStoreError::Invalid)?;
    let sources = desk_diagnose_core::model_egress::NativeModelOutputSources {
        evidence,
        inputs,
        input_snapshot,
    };
    let input_policies =
        desk_diagnose_core::ai_assistant::source_registry::NativeInputPolicyRegistration::capture(
            &sources.input_snapshot,
            prepared.set.context.source_contract_revision,
            prepared.set.context.policy_revision,
        )
        .map_err(AssistantStoreError::Invalid)?;
    db.mark_phase("policies_captured");
    let input_policy_graph = super::super::output_archive::input_policy_graph_on(
        db,
        &prepared.set,
        &sources.input_snapshot,
        &input_policies,
    )
    .await?;
    db.mark_phase("policy_graph_built");
    let mut proof = desk_diagnose_core::ai_assistant::output_archive::ArchivedAssistantOutput {
        source_set: prepared.set.as_ref().clone(),
        input_revision: prepared.set.context.input_revision,
        actor_input_seq: prepared.set.context.actor_input_seq,
        input_recorded_at_unix_ms: prepared.set.context.input_recorded_at_unix_ms,
        statement_timezone: prepared.set.context.statement_timezone.clone(),
        sources,
        input_policies,
        input_policy_graph,
        output: output.clone(),
        retained_policy: None,
        retained_roots: Vec::new(),
    };
    if let Some(retained) =
        super::super::visible_history::native::resolve_policy_on(db, &proof, completed_at_unix_ms)
            .await?
    {
        proof.retained_policy = Some(retained.policy);
        proof.retained_roots = retained.read_roots;
    }
    db.mark_phase("policy_resolved");
    Ok(Some(
        super::super::output_archive::prepare_on(db, &proof, &prepared.policy).await?,
    ))
}

async fn session_on(
    db: &DatabaseTransaction,
    set: &AssistantNativeSourceSet,
) -> Result<agent_session::Model, AssistantStoreError> {
    agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&set.consumer_conversation_id))
        .filter(agent_session::Column::ActorId.eq(&set.actor_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)
}

fn validate_session(
    row: &agent_session::Model,
    session: &PersistedAgentSession,
    set: &AssistantNativeSourceSet,
    now: u64,
) -> Result<(), AssistantStoreError> {
    set.context.validate_session(session).map_err(|reason| {
        log::warn!(
            "[assistant-output] source check rejected: phase=session_context, reason={reason}"
        );
        AssistantStoreError::Invalid(reason)
    })?;
    let reason = if !retained_context_eligible(session) {
        Some("context_ineligible")
    } else if session.turn_state != TurnState::Running {
        Some("turn_not_running")
    } else if row.actor_id != set.actor_id || session.actor_id != set.actor_id {
        Some("actor_changed")
    } else if row.device_id != session.device_id
        || row.conversation_id != set.consumer_conversation_id
        || session.conversation_id != set.consumer_conversation_id
    {
        Some("subject_changed")
    } else if row.version != session.version {
        Some("session_version_changed")
    } else if unsigned(row.lease_token)? != session.lease_token {
        Some("lease_token_changed")
    } else if row
        .lease_deadline
        .is_none_or(|until| until.timestamp_millis() <= now as i64)
    {
        Some("lease_expired")
    } else if session
        .assistant_data
        .as_ref()
        .is_none_or(|data| data.temporary || data.source_device_ref != set.source_device_ref)
    {
        Some("assistant_scope_changed")
    } else {
        None
    };
    if let Some(reason) = reason {
        return Err(rejected(reason));
    }
    Ok(())
}

fn rejected(reason: &'static str) -> AssistantStoreError {
    log::warn!("[assistant-output] source check rejected: phase=acceptance, reason={reason}");
    AssistantStoreError::Inaccessible
}
