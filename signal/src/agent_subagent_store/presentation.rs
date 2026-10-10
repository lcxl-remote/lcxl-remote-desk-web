//! Bounded task presentation read in the owner's coherent snapshot transaction.
use super::*;
use crate::entity::agent_subagent_inbox as inbox;
use desk_agent_protocol::ai_assistant::subagent::AiAssistantDelegationSnapshot;
use sea_orm::QueryTrait;

pub(crate) async fn presentation_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    session: &PersistedAgentSession,
) -> Result<AiAssistantDelegationSnapshot, DbErr> {
    if let Some(binding) = session.agent_role.binding() {
        let row = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(&binding.task_id))
            .filter(run_row::Column::ChildConversationId.eq(&session.conversation_id))
            .filter(run_row::Column::RootConversationId.eq(&binding.root_conversation_id))
            .filter(run_row::Column::ActorId.eq(&session.actor_id))
            .filter(run_row::Column::DeviceId.eq(&session.device_id))
            .one(db)
            .await?
            .ok_or_else(invalid)?;
        let run = decode_run(&row)?;
        run.validate_session(session).map_err(|_| invalid())?;
        return Ok(AiAssistantDelegationSnapshot {
            parent_session_id: Some(binding.root_conversation_id.clone()),
            task: Some(run.summary()),
            tasks: None,
            active_tasks: Vec::new(),
            attention_tasks: Vec::new(),
            attention_count: 0,
        });
    }
    let query = run_row::Entity::find()
        .filter(run_row::Column::RootConversationId.eq(&session.conversation_id))
        .filter(run_row::Column::ActorId.eq(&session.actor_id))
        .filter(run_row::Column::DeviceId.eq(&session.device_id));
    let total = query.clone().count(db).await?;
    let unfinished_query = query.clone().filter(run_row::Column::State.is_not_in([
        "completed",
        "failed",
        "cancelled",
    ]));
    let unfinished = unfinished_query.clone().count(db).await?;
    let active = unfinished_query
        .order_by_asc(run_row::Column::Id)
        .limit(desk_diagnose_core::subagent::SUBAGENT_ROOT_CAPACITY as u64 + 1)
        .all(db)
        .await?;
    if active.len() > desk_diagnose_core::subagent::SUBAGENT_ROOT_CAPACITY {
        return Err(invalid());
    }
    let active_tasks = active
        .iter()
        .map(|row| decode_run(row).map(|run| run.summary()))
        .collect::<Result<_, _>>()?;
    let unread = inbox::Entity::find()
        .select_only()
        .column(inbox::Column::TaskId)
        .filter(inbox::Column::RootConversationId.eq(&session.conversation_id))
        .filter(inbox::Column::ActorId.eq(&session.actor_id))
        .filter(inbox::Column::DeviceId.eq(&session.device_id))
        .filter(inbox::Column::UiReadAtMs.is_null())
        .filter(inbox::Column::EventKind.is_in([
            "waiting_approval",
            "completed",
            "failed",
            "cancelled",
        ]))
        .into_query();
    let attention = query
        .clone()
        .filter(run_row::Column::TaskId.in_subquery(unread))
        .filter(run_row::Column::State.is_in([
            "waiting_approval",
            "completed",
            "failed",
            "cancelled",
        ]));
    let attention_count = attention.clone().count(db).await?;
    let attention_rows = attention
        .order_by_asc(run_row::Column::Id)
        .limit(MAX_SUBAGENT_PAGE_SIZE as u64)
        .all(db)
        .await?;
    let attention_tasks = attention_rows
        .iter()
        .map(|row| decode_run(row).map(|run| run.summary()))
        .collect::<Result<_, _>>()?;
    let rows = query
        .order_by_desc(run_row::Column::Id)
        .limit(11)
        .all(db)
        .await?;
    let has_more = rows.len() > 10;
    let items = rows
        .iter()
        .take(10)
        .map(|row| decode_run(row).map(|run| run.summary()))
        .collect::<Result<_, _>>()?;
    let next_cursor = if has_more {
        rows.get(9).map(|row| row.id.to_string())
    } else {
        None
    };
    Ok(AiAssistantDelegationSnapshot {
        parent_session_id: None,
        task: None,
        active_tasks,
        attention_tasks,
        attention_count,
        tasks: Some(AiAssistantSubAgentPage {
            items,
            total,
            unfinished,
            next_cursor,
            has_more,
        }),
    })
}
