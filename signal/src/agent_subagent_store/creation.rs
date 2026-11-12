//! Root-scoped, idempotent child creation and independent session persistence.
use super::*;
use crate::config::connection::DatabaseTransaction;
use desk_diagnose_core::{
    chat::ToolCall,
    session::TriggerOrigin,
    subagent::{
        DelegatedTaskBinding,
        creation::{CreationOrigin, TaskCreationEnvelope, first_dispatch_context, task_context},
        tools::{self, Operation, SpawnRequest},
    },
};
use sea_orm::ActiveModelTrait;
use sha2::{Digest, Sha256};

pub(crate) async fn parent_planning_on(
    txn: &DatabaseTransaction,
    held: &PersistedAgentSession,
    now_ms: i64,
) -> Result<Box<PersistedAgentSession>, DbErr> {
    let current = parent_on(txn, &held.conversation_id, &held.actor_id, &held.device_id).await?;
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&held.conversation_id))
        .one(txn)
        .await?
        .ok_or_else(|| invalid())?;
    if !held.agent_role.is_main()
        || !current.turn_state.is_active()
        || held.version != current.version
        || held.lease_token != current.lease_token
        || held.input_revision != current.input_revision
        || held.control_revision != current.control_revision
        || held.current_turn_id != current.current_turn_id
        || current.current_turn_id.is_none()
        || held.delegation_group_id != current.delegation_group_id
        || held.agent_role != current.agent_role
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
    #[cfg(test)]
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

        let history = desk_diagnose_core::future::boxed(|| {
            crate::assistant_data_store::native_scope_on(
                &txn,
                &held.actor_id,
                &held.device_id,
                &held.conversation_id,
            )
        })
        .await?;
        let result = spawn_on(&txn, held, call, request).await?;
        if let Some(history) = &history {
            desk_diagnose_core::future::boxed(|| history.capture_on(&txn)).await?;
        }
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
    let now = chrono::Utc::now();
    let current =
        desk_diagnose_core::future::boxed(|| parent_planning_on(txn, held, now.timestamp_millis()))
            .await?;
    create_request_on(txn, &current, SpawnOrigin::NativeTool(call), request, now).await
}

#[derive(Clone, Copy)]
enum SpawnOrigin<'a> {
    NativeTool(&'a ToolCall),
    FirstDispatch(&'a str),
}

/// A staged route becomes a decision only with the native input and Worker.
/// The caller holds the authenticated owner, workbench and native work locks.
pub(crate) async fn first_dispatch_on(
    txn: &DatabaseTransaction,
    work: &mut PersistedAgentSession,
    queued: &desk_assistant_data_store::entity::agent_main_request::Model,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<String>, DbErr> {
    use desk_assistant_data_store::main_request::dispatch::FirstDispatch;
    let Some(request) = FirstDispatch::request_from(queued, work).map_err(|_| invalid())? else {
        return Ok(None);
    };
    if let Some(worker_id) = &request.worker_id
        && !worker_exists_on(txn, work, worker_id).await?
    {
        // An expired Worker identity does not discard the original owner input.
        return Ok(None);
    }
    let result = desk_diagnose_core::future::boxed(|| {
        create_request_on(
            txn,
            work,
            SpawnOrigin::FirstDispatch(&queued.request_id),
            &request,
            now,
        )
    })
    .await?;
    work.handled_input_seq = work.latest_input_seq;
    Ok(Some(result.task_id))
}

async fn worker_exists_on(
    txn: &DatabaseTransaction,
    parent: &PersistedAgentSession,
    worker_id: &str,
) -> Result<bool, DbErr> {
    Ok(desk_assistant_data_store::worker_request::in_scope(
        &parent.actor_id,
        &parent.device_id,
        &parent.conversation_id,
        parent.agent_role.workbench_id(),
    )
    .filter(run_row::Column::WorkerId.eq(worker_id))
    .select_only()
    .column(run_row::Column::Id)
    .into_tuple::<i64>()
    .one(txn)
    .await?
    .is_some())
}

fn requested_worker_id(request: &SpawnRequest, origin: &CreationOrigin<'_>) -> String {
    request
        .worker_id
        .clone()
        .unwrap_or_else(|| origin.default_worker_id())
}

async fn create_request_on(
    txn: &DatabaseTransaction,
    held: &PersistedAgentSession,
    origin: SpawnOrigin<'_>,
    request: &SpawnRequest,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<AiAssistantSubAgentSummary, DbErr> {
    let (kind, intent) = match origin {
        SpawnOrigin::NativeTool(call) => (CreationOrigin::NATIVE_TOOL, call.id.as_str()),
        SpawnOrigin::FirstDispatch(request_id) => (CreationOrigin::FIRST_DISPATCH, request_id),
    };
    let group_id = held
        .delegation_group_id
        .as_deref()
        .ok_or_else(|| invalid())?;
    let creation_origin = CreationOrigin {
        kind,
        root_conversation_id: &held.conversation_id,
        group_id,
        actor_id: &held.actor_id,
        device_id: &held.device_id,
        intent,
    };
    let arguments_sha256 = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(request).map_err(|_| invalid())?)
    );
    let child_id = creation_origin.child_conversation_id();
    let task_id = creation_origin.task_id();
    let original_row = desk_diagnose_core::future::boxed(|| async {
        group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(group_id))
            .filter(group_row::Column::RootConversationId.eq(&held.conversation_id))
            .filter(group_row::Column::ActorId.eq(&held.actor_id))
            .filter(group_row::Column::DeviceId.eq(&held.device_id))
            .one(txn)
            .await
    })
    .await?
    .ok_or_else(|| invalid())?;
    let mut group = boxed_group(&original_row)?;
    let creation = decode_creation(&original_row)?;
    if !group.can_interpret(held.input_revision, held.control_revision)
        || group.source.workbench_id() != held.agent_role.workbench_id()
    {
        return Err(invalid());
    }
    desk_diagnose_core::future::boxed(|| async {
        super::scheduled_source::current_scheduled_source_on(txn, &original_row).await
    })
    .await?;
    if let Some(existing) = desk_diagnose_core::future::boxed(|| async {
        run_row::Entity::find()
            .filter(run_row::Column::RootConversationId.eq(&held.conversation_id))
            .filter(run_row::Column::GroupId.eq(group_id))
            .filter(run_row::Column::OriginKind.eq(kind))
            .filter(run_row::Column::OriginRef.eq(intent))
            .one(txn)
            .await
    })
    .await?
    {
        let run = decode_run(&existing)?;
        if existing.creation_arguments_sha256 != arguments_sha256
            || run.binding.task_id != task_id
            || (held.agent_role.issue_source().is_none()
                && run.worker_id != requested_worker_id(request, &creation_origin))
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
    let issue_source = held.agent_role.issue_source();
    let preferred = if let Some(source) = issue_source {
        crate::issue_connections::check_work_on(
            txn,
            held,
            held.assistant_request_targets()
                .is_some_and(|targets| !targets.is_empty()),
            now.timestamp_millis(),
        )
        .await
        .map_err(|_| invalid())?;
        desk_assistant_data_store::issue_events::routing::preferred_worker_on(
            txn,
            &held.actor_id,
            source,
        )
        .await
        .map_err(|_| invalid())?
    } else {
        None
    };
    let worker_id = preferred
        .clone()
        .unwrap_or_else(|| requested_worker_id(request, &creation_origin));
    // An existing Worker continues in its session; a new one gets its own.
    let reused_session = desk_diagnose_core::future::boxed(|| {
        desk_assistant_data_store::worker_request::worker_session_on(
            txn,
            desk_assistant_data_store::worker_request::in_scope(
                &held.actor_id,
                &held.device_id,
                &held.conversation_id,
                group.source.workbench_id(),
            ),
            &worker_id,
            &group.source,
        )
    })
    .await?;
    let child_id = reused_session.clone().unwrap_or(child_id);
    if matches!(origin, SpawnOrigin::NativeTool(_))
        && preferred.is_none()
        && let Some(worker_id) = &request.worker_id
        && !worker_exists_on(txn, held, worker_id).await?
    {
        return Err(invalid());
    }
    desk_diagnose_core::future::send_phase(|| async {
        let unfinished = desk_diagnose_core::future::boxed(|| {
            desk_assistant_data_store::worker_request::unfinished_in_scope(
                &held.actor_id,
                &held.device_id,
                &held.conversation_id,
                group.source.workbench_id(),
            )
            .count(txn)
        })
        .await?;
        let policy =
            desk_diagnose_core::future::boxed(|| async { crate::subagent_policy::read(txn).await })
                .await?;
        let mut admission_limits = policy.limits;
        if group.source.workbench_id().is_some() {
            admission_limits.max_unfinished_per_root =
                desk_assistant_data_store::main_request::PENDING_PER_CONVERSATION as u32;
        }
        if unfinished >= u64::from(admission_limits.max_unfinished_per_root) {
            return Err(DbErr::Custom(
                desk_diagnose_core::subagent::capacity_storage_message(
                    admission_limits.max_unfinished_per_root,
                ),
            ));
        }
        group = current_group_on(txn, &original_row).await?;
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
        if !group.limits.child_ceiling().has_model_capacity(child_used)
            || !group.limits.total.has_model_capacity(total_used)
        {
            return Err(invalid());
        }
        group
            .admit_child(
                &task_id,
                request.required_for_completion,
                unfinished as usize,
                now.timestamp_millis(),
                admission_limits,
            )
            .map_err(|_| invalid())?;
        let targets = creation.request_targets(held).map_err(|_| invalid())?;
        if !targets.is_empty() {
            let live = desk_diagnose_core::future::boxed(|| {
                desk_assistant_data_store::device_roster::current_request_devices(txn, held)
            })
            .await
            .map_err(|_| invalid())?;
            if targets
                .iter()
                .any(|target| !live.contains(&target.device_id))
            {
                return Err(invalid());
            }
        }
        let context = match origin {
            SpawnOrigin::NativeTool(call) => task_context(creation, held, call, &task_id, request),
            SpawnOrigin::FirstDispatch(_) => {
                first_dispatch_context(creation, held, &task_id, request)
            }
        }
        .map_err(|_| invalid())?;
        let binding = DelegatedTaskBinding {
            targets,
            root_conversation_id: held.conversation_id.clone(),
            group_id: group.group_id.clone(),
            task_id,
            source: group.source.clone(),
            objective: request.task.clone(),
            acceptance_criteria: request.acceptance_criteria.clone(),
            input_revision: 1,
            control_revision: 1,
            source_epoch: group.source_epoch,
            deadline_ms: desk_diagnose_core::subagent::creation::child_deadline_ms(
                group.limits.deadline_ms,
                now.timestamp_millis(),
                policy.limits.max_task_duration_minutes,
            )
            .map_err(|_| invalid())?,
        };
        let base_scope = desk_agent_protocol::AgentScope {
            granted: desk_diagnose_core::ai_assistant::selected_context_capabilities(&[])
                .map_err(|_| invalid())?,
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: Some("subagent-independent".into()),
        };
        let child = if reused_session.is_none() {
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
            child.inherit_direct_approval_origin(held);
            child
                .bind_delegated_owner_requirement(&context.source)
                .map_err(|_| invalid())?;
            desk_diagnose_core::ai_assistant::delegated_retrieval::DelegatedAssistantSeed::bind(
                context.source.assistant_seed.as_deref(),
                &mut child,
            )
            .map_err(|_| invalid())?;
            child.response_locale = context.response_locale.clone();
            child.trigger_origin = TriggerOrigin::DelegatedTask;
            child.chain_id = child_id.clone();
            child.conversation.push(
                context
                    .source
                    .child_source_message(&binding.task_id)
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
            Some(child)
        } else {
            None
        };
        let run = SubAgentRun {
            worker_id: worker_id.clone(),
            child_conversation_id: child_id.clone(),
            actor_id: held.actor_id.clone(),
            device_id: held.device_id.clone(),
            name: request.name.clone(),
            binding,
            state: SubAgentState::Queued,
            state_revision: 1,
            source_paused: false,
            dependencies: Vec::new(),
            pending_question_id: None,
            partial_report: None,
            terminal_report: None,
            failure_reason: None,
            report_corrections_used: 0,
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
        };
        run.validate().map_err(|_| invalid())?;
        if let Some(child) = &child {
            child.encode_json_for_storage().map_err(|_| invalid())?;
        }
        desk_diagnose_core::future::boxed(|| async {
            replace_group_on(txn, &original_row, &group, now.timestamp_millis()).await
        })
        .await?;
        desk_diagnose_core::future::boxed(|| async {
            run_row::ActiveModel {
            task_id: Set(run.binding.task_id.clone()),
            child_conversation_id: Set(child_id.clone()),
            worker_id: Set(worker_id),
            workbench_id: Set(group.source.workbench_id().map(str::to_owned)),
            root_conversation_id: Set(held.conversation_id.clone()),
            group_id: Set(group.group_id.clone()),
            actor_id: Set(held.actor_id.clone()),
            device_id: Set(held.device_id.clone()),
            origin_kind: Set(kind.into()),
            origin_ref: Set(intent.into()),
            creation_arguments_sha256: Set(arguments_sha256),
            creation_envelope_json: Set(serde_json::to_string(&context).map_err(|_| invalid())?),
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
        .await
        })
        .await?;
        if let Some(child) = child {
            desk_diagnose_core::future::boxed(|| async {
                session_row::ActiveModel {
                    conversation_id: Set(child_id),
                    actor_id: Set(held.actor_id.clone()),
                    device_id: Set(held.device_id.clone()),
                    conversation_schema_version: Set(Some(i32::from(
                        desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
                    ))),
                    state_json: Set(child.encode_json_for_storage().map_err(|_| invalid())?),
                    version: Set(0),
                    lease_token: Set(0),
                    lease_deadline: Set(None),

                    created_at: Set(now),
                    updated_at: Set(now),
                    ..Default::default()
                }
                .insert(txn)
                .await
            })
            .await?;
        }
        desk_diagnose_core::future::boxed(|| async {
            append_state_event_on(txn, &group, &run, now.timestamp_millis()).await
        })
        .await?;
        if let Some(source) = issue_source {
            desk_assistant_data_store::issue_events::routing::record_first_worker_on(
                txn,
                &held.actor_id,
                source,
                &run.worker_id,
            )
            .await
            .map_err(|_| invalid())?;
        }
        Ok(run.summary())
    })
    .await
}
