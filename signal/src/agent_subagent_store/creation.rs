//! Root-scoped, idempotent child creation and independent session persistence.
use super::*;
use desk_diagnose_core::{
    chat::ToolCall,
    session::TriggerOrigin,
    subagent::{
        DelegatedTaskBinding,
        creation::{TaskCreationEnvelope, task_context},
        tools::{self, Operation, SpawnRequest},
    },
};
use sea_orm::{ActiveModelTrait, DatabaseTransaction};
use sha2::{Digest, Sha256};

pub(crate) async fn parent_planning_on(
    txn: &DatabaseTransaction,
    held: &PersistedAgentSession,
    now_ms: i64,
) -> Result<PersistedAgentSession, DbErr> {
    let current = parent_on(txn, &held.conversation_id, &held.actor_id, &held.device_id).await?;
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&held.conversation_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    if !held.agent_role.is_main()
        || !current.turn_state.is_active()
        || held.version != current.version
        || held.lease_token != current.lease_token
        || held.input_revision != current.input_revision
        || held.control_revision != current.control_revision
        || held.current_turn_id != current.current_turn_id
        || current.current_turn_id.is_none()
        || held.delegation_group_id != current.delegation_group_id
        || held.trigger_origin != current.trigger_origin
        || held.subagent_result_only != current.subagent_result_only
        || row
            .lease_deadline
            .is_none_or(|deadline| deadline.timestamp_millis() < now_ms)
    {
        return Err(invalid());
    }
    Ok(current)
}

impl SubAgentStore {
    pub async fn spawn_for_turn(
        &self,
        held: &PersistedAgentSession,
        call: &ToolCall,
        request: &SpawnRequest,
    ) -> Result<AiAssistantSubAgentSummary, DbErr> {
        if tools::parse(held, call).map_err(|_| invalid())? != Operation::Spawn(request.clone()) {
            return Err(invalid());
        }
        let txn = crate::db::begin_write(&self.db, session_row::Entity).await?;

        let result = spawn_on(&txn, held, call, request).await?;
        txn.commit().await?;
        Ok(result)
    }
}

/// Root and the deterministic child control lock are held by the caller.
pub(crate) async fn spawn_on(
    txn: &DatabaseTransaction,
    held: &PersistedAgentSession,
    call: &ToolCall,
    request: &SpawnRequest,
) -> Result<AiAssistantSubAgentSummary, DbErr> {
    if tools::parse(held, call).map_err(|_| invalid())? != Operation::Spawn(request.clone()) {
        return Err(invalid());
    }
    let group_id = held.delegation_group_id.as_deref().ok_or_else(invalid)?;
    let creation_key = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                "subagent_creation",
                &held.conversation_id,
                group_id,
                &held.actor_id,
                &held.device_id,
                &call.id,
            ))
            .map_err(|_| invalid())?
        )
    );
    let arguments_sha256 = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(request).map_err(|_| invalid())?)
    );
    let child_id = format!("sa-{creation_key}");
    let task_id = format!("st-{creation_key}");
    let now = chrono::Utc::now();
    let current = parent_planning_on(txn, held, now.timestamp_millis()).await?;
    let original_row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(group_id))
        .filter(group_row::Column::RootConversationId.eq(&held.conversation_id))
        .filter(group_row::Column::ActorId.eq(&held.actor_id))
        .filter(group_row::Column::DeviceId.eq(&held.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let mut group = decode_group(&original_row)?;
    let creation = decode_creation(&original_row)?;
    if !group.can_interpret(held.input_revision, held.control_revision) {
        return Err(invalid());
    }
    super::scheduled_source::current_scheduled_source_on(txn, &original_row).await?;
    if let Some(existing) = run_row::Entity::find()
        .filter(run_row::Column::CreationKeySha256.eq(&creation_key))
        .one(txn)
        .await?
    {
        let run = decode_run(&existing)?;
        if existing.creation_arguments_sha256 != arguments_sha256
            || run.binding.task_id != task_id
            || run.child_conversation_id != child_id
            || run.binding.group_id != group_id
            || run.binding.root_conversation_id != held.conversation_id
            || run.actor_id != held.actor_id
            || run.device_id != held.device_id
        {
            return Err(invalid());
        }
        let context: TaskCreationEnvelope =
            serde_json::from_str(&existing.creation_envelope_json).map_err(|_| invalid())?;
        context.validate().map_err(|_| invalid())?;
        if context.source != creation {
            return Err(invalid());
        }
        return Ok(run.summary());
    }
    let unfinished = run_row::Entity::find()
        .filter(run_row::Column::RootConversationId.eq(&held.conversation_id))
        .filter(run_row::Column::ActorId.eq(&held.actor_id))
        .filter(run_row::Column::DeviceId.eq(&held.device_id))
        .filter(run_row::Column::State.is_not_in(["completed", "failed", "cancelled"]))
        .count(txn)
        .await?;
    let policy = crate::subagent_policy::read(txn).await?;
    if unfinished >= policy.limits.max_unfinished_per_root as u64 {
        return Err(DbErr::Custom(
            desk_diagnose_core::subagent::capacity_storage_message(
                policy.limits.max_unfinished_per_root,
            ),
        ));
    }
    let child_used = group
        .budget
        .child_charged
        .checked_add(group.budget.child_outstanding)
        .map_err(|_| invalid())?;
    let total_used = group
        .budget
        .charged
        .checked_add(group.budget.outstanding)
        .map_err(|_| invalid())?;
    if child_used.model_calls >= group.limits.child_ceiling().model_calls
        || child_used.tokens >= group.limits.child_ceiling().tokens
        || total_used.model_calls >= group.limits.total.model_calls
        || total_used.tokens >= group.limits.total.tokens
    {
        return Err(invalid());
    }
    group
        .admit_child(
            &task_id,
            request.required_for_completion,
            unfinished as usize,
            now.timestamp_millis(),
            policy.limits,
        )
        .map_err(|_| invalid())?;
    let context =
        task_context(creation, &current, call, &child_id, request).map_err(|_| invalid())?;
    let binding = DelegatedTaskBinding {
        root_conversation_id: held.conversation_id.clone(),
        group_id: group.group_id.clone(),
        task_id,
        source: group.source.clone(),
        objective: request.task.clone(),
        acceptance_criteria: request.acceptance_criteria.clone(),
        input_revision: 1,
        control_revision: 1,
        source_epoch: group.source_epoch,
        deadline_ms: group.limits.deadline_ms,
    };
    let base_scope = desk_agent_protocol::AgentScope {
        granted: desk_diagnose_core::ai_assistant::selected_context_capabilities(&[])
            .map_err(|_| invalid())?,
        mode: desk_agent_protocol::ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: Some("subagent-independent".into()),
    };
    let mut child = PersistedAgentSession::new_subagent(
        &child_id,
        &held.actor_id,
        &held.device_id,
        held.policy_revision,
        base_scope,
        binding.clone(),
        now.to_rfc3339(),
    )
    .map_err(|_| invalid())?;
    child
        .bind_delegated_owner_requirement(&context.source)
        .map_err(|_| invalid())?;
    child.response_locale = context.response_locale.clone();
    child.trigger_origin = TriggerOrigin::DelegatedTask;
    child.chain_id = child_id.clone();
    child.conversation.push(
        context
            .source
            .child_source_message(&child_id)
            .map_err(|_| invalid())?,
    );
    child.conversation.push(context.instruction.clone());
    // Only non-authorizing data labels cross the parent/child boundary.
    // Directory consent, capabilities, attachments, approvals and actions do not.
    let context_bytes = serde_json::to_vec(&child.conversation)
        .map_err(|_| invalid())?
        .len();
    if context_bytes as u64 > group.limits.max_context_bytes {
        return Err(invalid());
    }
    let run = SubAgentRun {
        child_conversation_id: child_id.clone(),
        actor_id: held.actor_id.clone(),
        device_id: held.device_id.clone(),
        name: request.name.clone(),
        binding,
        state: SubAgentState::Queued,
        state_revision: 1,
        source_paused: false,
        dependencies: Vec::new(),
        partial_report: None,
        terminal_report: None,
        failure_reason: None,
        report_corrections_used: 0,
        created_at: now.to_rfc3339(),
        updated_at: now.to_rfc3339(),
    };
    run.validate().map_err(|_| invalid())?;
    child.encode_json_for_storage().map_err(|_| invalid())?;
    replace_group_on(txn, &original_row, &group, now.timestamp_millis()).await?;
    run_row::ActiveModel {
        task_id: Set(run.binding.task_id.clone()),
        child_conversation_id: Set(child_id.clone()),
        root_conversation_id: Set(held.conversation_id.clone()),
        group_id: Set(group.group_id.clone()),
        actor_id: Set(held.actor_id.clone()),
        device_id: Set(held.device_id.clone()),
        creation_key_sha256: Set(creation_key),
        creation_arguments_sha256: Set(arguments_sha256),
        creation_envelope_json: Set(serde_json::to_string(&context).map_err(|_| invalid())?),
        result_envelope_json: Set(None),
        state: Set(run.state.as_str().into()),
        input_revision: Set(1),
        control_revision: Set(1),
        source_epoch: Set(i64::try_from(group.source_epoch).map_err(|_| invalid())?),
        state_revision: Set(1),
        state_json: Set(serde_json::to_string(&run).map_err(|_| invalid())?),
        next_attempt_at_ms: Set(Some(now.timestamp_millis())),
        deadline_ms: Set(run.binding.deadline_ms),
        created_at: Set(now.timestamp_millis()),
        updated_at: Set(now.timestamp_millis()),
        ..Default::default()
    }
    .insert(txn)
    .await?;
    session_row::ActiveModel {
        conversation_id: Set(child_id),
        actor_id: Set(held.actor_id.clone()),
        device_id: Set(held.device_id.clone()),
        state_json: Set(child.encode_json_for_storage().map_err(|_| invalid())?),
        version: Set(0),
        lease_token: Set(0),
        lease_deadline: Set(None),

        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(txn)
    .await?;
    append_state_event_on(txn, &group, &run, now.timestamp_millis()).await?;
    Ok(run.summary())
}
