//! Owner-facing workbench projections over the original queue and action facts.

use std::collections::{BTreeMap, BTreeSet};

use desk_agent_protocol::ai_assistant::workbench::{
    AiAssistantWorkbenchDelivery, AiAssistantWorkbenchTask, AiAssistantWorkbenchTasks,
};
use desk_diagnose_core::native_ids::{self, Kind};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    Select,
    sea_query::{Expr, Func},
};

use crate::{
    AssistantStoreError,
    entity::{
        agent_issue_event as event, agent_issue_reply as reply, agent_main_request as request,
        agent_subagent_run as run,
    },
    issue_authority, issue_reply,
    main_request::decode_receipt,
};

const PAGE_SIZE: usize = 20;

/// The native session stop already owns this round's control transaction.
pub async fn stop_issue_round_on(
    db: &impl ConnectionTrait,
    actor: &str,
    workbench: &str,
    request: &str,
    now: i64,
) -> Result<(), AssistantStoreError> {
    issue_authority::stop_request(db, actor, request, now).await?;
    issue_reply::lifecycle::cancel_logical_request_on(db, actor, workbench, request, now).await
}

/// The host validates the native coordinator before calling. Only exact native
/// work identities may associate worker state and external receipts with a row.
pub async fn list_on(
    db: &impl ConnectionTrait,
    actor: &str,
    workbench: &str,
    request_id: Option<&str>,
    before: Option<&str>,
) -> Result<AiAssistantWorkbenchTasks, AssistantStoreError> {
    use request::Column;
    let mut query = request::Entity::find()
        .select_only()
        .columns([
            Column::Id,
            Column::RequestId,
            Column::ClientMessageId,
            Column::State,
            Column::Failure,
            Column::InputRevision,
            Column::AcceptedAtMs,
            Column::UpdatedAtMs,
        ])
        .filter(Column::ActorId.eq(actor))
        .filter(Column::ConversationId.eq(workbench));
    if let Some(request_id) = request_id {
        if !native_ids::is(Kind::AssistantMainRequest, request_id) || before.is_some() {
            return Err(invalid());
        }
        query = query.filter(Column::RequestId.eq(request_id));
    } else if let Some(before) = before {
        let id = before
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(invalid)?;
        query = query.filter(Column::Id.lt(id));
    }
    type Row = (
        i64,
        String,
        String,
        String,
        Option<String>,
        Option<i64>,
        i64,
        i64,
    );
    let mut rows = query
        .order_by_desc(Column::Id)
        .limit(PAGE_SIZE as u64 + 1)
        .into_tuple::<Row>()
        .all(db)
        .await?;
    let next_before = if rows.len() > PAGE_SIZE {
        rows.pop();
        rows.last().map(|row| row.0.to_string())
    } else {
        None
    };
    if rows.is_empty() {
        return if request_id.is_some() {
            Err(AssistantStoreError::Inaccessible)
        } else {
            Ok(AiAssistantWorkbenchTasks {
                items: vec![],
                next_before,
            })
        };
    }

    let request_ids = rows.iter().map(|row| row.1.as_str()).collect::<Vec<_>>();
    let mut issue_numbers = issue_numbers_on(db, actor, workbench, request_ids).await?;
    let mut items = Vec::with_capacity(rows.len());
    for (_, id, client_id, state, failure, revision, accepted, updated) in rows {
        let execution_session_id = revision
            .filter(|revision| *revision > 0)
            .map(|_| native_ids::workbench_work_conversation(actor, workbench, &id));
        let issue_number = issue_numbers.remove(&id);
        items.push(AiAssistantWorkbenchTask {
            receipt: decode_receipt((id, client_id, state, failure, revision, accepted))?,
            execution_session_id,
            updated_at_ms: updated,
            issue_number,
            worker_states: vec![],
            deliveries: vec![],
        });
    }
    project_worker_states_on(db, actor, workbench, &mut items).await?;
    if request_id.is_some()
        && let Some(item) = items.first_mut()
    {
        item.deliveries = deliveries_on(db, actor, workbench, item).await?;
    }
    Ok(AiAssistantWorkbenchTasks { items, next_before })
}

async fn issue_numbers_on(
    db: &impl ConnectionTrait,
    actor: &str,
    workbench: &str,
    ids: Vec<&str>,
) -> Result<BTreeMap<String, i64>, AssistantStoreError> {
    use event::Column;
    Ok(event::Entity::find()
        .select_only()
        .columns([Column::RequestId, Column::IssueNumber])
        .filter(Column::ActorId.eq(actor))
        .filter(Column::ConversationId.eq(workbench))
        .filter(Column::RequestId.is_in(ids))
        .filter(Column::IssueNumber.is_not_null())
        .filter(Column::RetiredAtMs.is_null())
        .group_by(Column::RequestId)
        .group_by(Column::IssueNumber)
        .into_tuple::<(String, i64)>()
        .all(db)
        .await?
        .into_iter()
        .collect())
}

async fn project_worker_states_on(
    db: &impl ConnectionTrait,
    actor: &str,
    workbench: &str,
    items: &mut [AiAssistantWorkbenchTask],
) -> Result<(), AssistantStoreError> {
    use run::Column;
    let roots = items
        .iter()
        .filter_map(|item| item.execution_session_id.as_deref())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        return Ok(());
    }
    let rows = run::Entity::find()
        .select_only()
        .columns([Column::RootConversationId, Column::State])
        .column_as(
            Expr::from(Func::max(Expr::col(Column::UpdatedAt))),
            "latest_updated_at",
        )
        .filter(Column::ActorId.eq(actor))
        .filter(Column::WorkbenchId.eq(workbench))
        .filter(Column::RootConversationId.is_in(roots))
        .group_by(Column::RootConversationId)
        .group_by(Column::State)
        .into_tuple::<(String, String, i64)>()
        .all(db)
        .await?;
    let mut by_root = BTreeMap::<String, (BTreeSet<String>, i64)>::new();
    for (root, state, updated) in rows {
        let (states, latest) = by_root.entry(root).or_default();
        states.insert(state);
        *latest = (*latest).max(updated);
    }
    for item in items {
        if let Some(root) = item.execution_session_id.as_deref()
            && let Some((states, latest)) = by_root.remove(root)
        {
            item.updated_at_ms = item.updated_at_ms.max(latest);
            item.worker_states = states.into_iter().collect();
        }
    }
    Ok(())
}

async fn deliveries_on(
    db: &impl ConnectionTrait,
    actor: &str,
    workbench: &str,
    item: &AiAssistantWorkbenchTask,
) -> Result<Vec<AiAssistantWorkbenchDelivery>, AssistantStoreError> {
    use reply::Column;
    // The original publication stores its native request identity even if the
    // Worker or result body has already been retired.
    let rows = reply::Entity::find()
        .filter(Column::ActorId.eq(actor))
        .filter(Column::RootConversationId.eq(workbench))
        .filter(Column::RequestId.eq(&item.receipt.request_id))
        .order_by_desc(Column::Id)
        .limit(PAGE_SIZE as u64)
        .all(db)
        .await?;
    rows.into_iter()
        .map(|row| {
            let target = issue_reply::receipt_key(&row)?.access.target;
            Ok(AiAssistantWorkbenchDelivery {
                artifact_id: row.artifact_id,
                state: row.state,
                reason: row.reason,
                updated_at_ms: row.updated_at_ms,
                comment_id: row.comment_id,
                instance: target.instance,
                owner: target.owner,
                repository: target.repository,
                issue_number: target.issue_number,
            })
        })
        .collect()
}

/// Only results requiring a person are shown outside the task. Pending
/// reconciliation, opt-outs, and source/policy decisions stay in the task.
fn needs_delivery_attention(now: i64) -> Select<reply::Entity> {
    use reply::Column;
    use sea_orm::sea_query::ExprTrait;
    reply::Entity::find()
        .filter(Column::RetiredAtMs.is_null())
        .filter(
            Condition::any()
                .add(Column::ContentExpiresAtMs.is_null())
                .add(Column::ContentExpiresAtMs.gt(now)),
        )
        .filter(
            Condition::any()
                .add(
                    Column::State
                        .eq("internal")
                        .and(Column::Reason.eq("connection_unavailable")),
                )
                .add(
                    Column::State
                        .eq("unknown")
                        .and(Column::Reason.eq("artifact_invalid")),
                ),
        )
}

pub async fn delivery_attention_on(
    db: &impl ConnectionTrait,
    actor: &str,
    now: i64,
) -> Result<Vec<(String, String, String, i64)>, sea_orm::DbErr> {
    use reply::Column;
    needs_delivery_attention(now)
        .select_only()
        .columns([
            Column::RootConversationId,
            Column::RequestId,
            Column::ArtifactId,
            Column::UpdatedAtMs,
        ])
        .filter(Column::ActorId.eq(actor))
        .into_tuple()
        .all(db)
        .await
}

pub async fn delivery_attention_owners_on(
    db: &impl ConnectionTrait,
    now: i64,
) -> Result<Vec<String>, sea_orm::DbErr> {
    needs_delivery_attention(now)
        .select_only()
        .column(reply::Column::ActorId)
        .distinct()
        .into_tuple()
        .all(db)
        .await
}

fn invalid() -> AssistantStoreError {
    AssistantStoreError::Invalid("Invalid workbench task selector")
}
