//! Original native task rows for one personal root. No result bodies or authority
//! enter the retained snapshot; rows are reselected before the original save CAS.

use super::{AssistantStoreError, unsigned};
use crate::config::ConfigConnection;
use crate::entity::{
    agent_action_item as work, agent_exec_task as execution, agent_goal_run as goal,
    agent_schedule as timer, agent_schedule_run as scheduled, agent_subagent_run as child,
};
use desk_diagnose_core::{
    ai_assistant::native_history::{
        NativeHistorySnapshot, NativeHistoryStatus, NativeHistoryTaskKind,
        NativeStatusHistoryProjection,
    },
    session::PersistedAgentSession,
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Serialize, PartialEq, Eq)]
struct Rows {
    goals: Vec<goal::Model>,
    children: Vec<child::Model>,
    work: Vec<work::Model>,
    scheduled: Vec<scheduled::Model>,
    timers: Vec<timer::Model>,
    executions: Vec<execution::Model>,
}

pub(super) struct PreparedNativeStatus {
    rows: Box<Rows>,
    pub projection: NativeStatusHistoryProjection,
}

async fn rows_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    session: &PersistedAgentSession,
) -> Result<Box<Rows>, AssistantStoreError> {
    let page = u64::from(
        session
            .assistant_data
            .as_ref()
            .ok_or(AssistantStoreError::Inaccessible)?
            .limits
            .processing
            .native_status_rows,
    );
    let owner = session
        .actor_id
        .parse::<i32>()
        .map_err(|_| AssistantStoreError::Inaccessible)?;
    let goals = goal::Entity::find()
        .filter(goal::Column::ActorId.eq(&session.actor_id))
        .filter(goal::Column::ConversationId.eq(&session.conversation_id))
        .filter(goal::Column::DeviceId.eq(&session.device_id))
        .order_by_asc(goal::Column::GoalId)
        .limit(page)
        .all(db)
        .await?;
    let children = child::Entity::find()
        .filter(child::Column::ActorId.eq(&session.actor_id))
        .filter(child::Column::RootConversationId.eq(&session.conversation_id))
        .filter(child::Column::DeviceId.eq(&session.device_id))
        .order_by_asc(child::Column::TaskId)
        .limit(page)
        .all(db)
        .await?;
    let conversations = std::iter::once(session.conversation_id.clone())
        .chain(children.iter().map(|row| row.child_conversation_id.clone()))
        .collect::<Vec<_>>();
    let work = work::Entity::find()
        .filter(work::Column::ActorId.eq(&session.actor_id))
        .filter(work::Column::ConversationId.is_in(conversations.clone()))
        .order_by_asc(work::Column::Id)
        .limit(page)
        .all(db)
        .await?;
    let scheduled = scheduled::Entity::find()
        .filter(scheduled::Column::OwnerUserId.eq(owner))
        .filter(scheduled::Column::ConversationId.eq(&session.conversation_id))
        .order_by_asc(scheduled::Column::RunId)
        .limit(page)
        .all(db)
        .await?;
    let executions = execution::Entity::find()
        .filter(execution::Column::ConversationId.is_in(conversations))
        .order_by_asc(execution::Column::ExecRequestId)
        .limit(page)
        .all(db)
        .await?;
    let timers = timer::Entity::find()
        .filter(timer::Column::OwnerUserId.eq(owner))
        .filter(timer::Column::SourceConversationId.eq(&session.conversation_id))
        .filter(timer::Column::Kind.eq("conversation_resume"))
        .order_by_asc(timer::Column::Id)
        .limit(page)
        .all(db)
        .await?;
    Ok(Box::new(Rows {
        goals,
        children,
        work,
        scheduled,
        timers,
        executions,
    }))
}

pub(super) async fn prepare_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    session: &PersistedAgentSession,
    policy_revision: u64,
) -> Result<Box<PreparedNativeStatus>, AssistantStoreError> {
    let rows = rows_on(db, session).await?;
    let data = session
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let mut items = Vec::new();
    let complete = [
        rows.goals.len(),
        rows.children.len(),
        rows.work.len(),
        rows.scheduled.len(),
        rows.timers.len(),
        rows.executions.len(),
    ]
    .iter()
    .all(|len| *len < data.limits.processing.native_status_rows as usize);
    for row in &rows.goals {
        crate::agent_goal_store::decode(row)?;
        items.push(NativeHistoryStatus {
            native_id: row.goal_id.clone(),
            kind: NativeHistoryTaskKind::Goal,
            state: row.status.clone(),
            revision: Some(unsigned(row.state_version)?),
            observed_at_unix_ms: unsigned(row.updated_at)?,
        });
    }
    for row in &rows.children {
        crate::agent_subagent_store::decode_run(row)?;
        items.push(NativeHistoryStatus {
            native_id: row.task_id.clone(),
            kind: NativeHistoryTaskKind::Subagent,
            state: row.state.clone(),
            revision: Some(unsigned(row.state_revision)?),
            observed_at_unix_ms: unsigned(row.updated_at)?,
        });
    }
    for row in &rows.work {
        let id = &row.action_request_id;
        items.push(NativeHistoryStatus {
            native_id: id.clone(),
            kind: NativeHistoryTaskKind::Work,
            state: row.status.clone(),
            revision: None,
            observed_at_unix_ms: unsigned(row.updated_at.timestamp_millis())?,
        });
    }
    for row in &rows.scheduled {
        items.push(NativeHistoryStatus {
            native_id: row.run_id.clone(),
            kind: NativeHistoryTaskKind::Schedule,
            state: row.status.clone(),
            revision: None,
            observed_at_unix_ms: unsigned(row.updated_at)?,
        });
    }
    for row in &rows.executions {
        if rows
            .work
            .iter()
            // Work can use a derived background call ID while execution keeps
            // the original foreground call ID. An original link or the exact
            // turn/call mapping identifies the same work; both complete rows
            // remain in the immutable source fingerprint.
            .any(|work| {
                work.conversation_id == row.conversation_id
                    && (work.exec_request_id.as_deref() == Some(row.exec_request_id.as_str())
                        || (work.action_request_id == row.exec_request_id
                            && work.action_request_id
                                == desk_diagnose_core::native_ids::background_action(
                                    &work.conversation_id,
                                    &work.turn_id,
                                    &row.tool_call_id,
                                )))
            })
        {
            continue;
        }
        items.push(NativeHistoryStatus {
            native_id: row.exec_request_id.clone(),
            kind: NativeHistoryTaskKind::Work,
            state: row.status.clone(),
            revision: None,
            observed_at_unix_ms: unsigned(row.updated_at.timestamp_millis())?,
        });
    }
    for row in &rows.timers {
        items.push(NativeHistoryStatus {
            native_id: row.schedule_id.clone(),
            kind: NativeHistoryTaskKind::Schedule,
            state: row.status.clone(),
            revision: Some(unsigned(row.revision)?),
            observed_at_unix_ms: unsigned(row.updated_at)?,
        });
    }
    // Work and occurrence rows have no native state revision. Absence remains
    // explicit; an attempt, clock value or fabricated counter is not a revision.
    let recorded_at = items
        .iter()
        .map(|item| item.observed_at_unix_ms)
        .max()
        .unwrap_or(data.current_input_recorded_at_unix_ms);
    let snapshot = NativeHistorySnapshot::bounded(
        items,
        complete,
        recorded_at,
        data.limits.processing.native_status_items as usize,
    )
    .map_err(AssistantStoreError::Invalid)?;
    let original_sha256 = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(rows.as_ref()).map_err(|_| AssistantStoreError::Inaccessible)?
        )
    );
    let projection = NativeStatusHistoryProjection::new(
        &session.actor_id,
        &data.source_device_ref,
        &session.conversation_id,
        &original_sha256,
        snapshot,
        policy_revision,
    )
    .map_err(AssistantStoreError::Invalid)?;
    Ok(Box::new(PreparedNativeStatus { rows, projection }))
}

impl PreparedNativeStatus {
    pub(super) async fn validate_on<C: ConnectionTrait + ConfigConnection>(
        &self,
        db: &C,
        session: &PersistedAgentSession,
    ) -> Result<(), AssistantStoreError> {
        if rows_on(db, session).await? != self.rows {
            return Err(AssistantStoreError::Inaccessible);
        }
        Ok(())
    }
}

mod scoped;
pub(crate) use scoped::{NativeStatusScope, ReclaimObservation, scope_on};
