//! Resolve one frozen event without a browser or model call.

use super::*;
use desk_agent_protocol::data_lineage::DestinationIdentity;
use desk_assistant_data_store::{
    device_roster,
    entity::agent_issue_event,
    issue_events::routing::{self, FrozenIssueHeader, PendingEvent},
};
use desk_diagnose_core::{dynamic_run::AssistantInputTarget, external_tools::IssueAccess};
use futures_util::{StreamExt, stream};

pub async fn scan_routing_once(
    db: &DatabaseConnection,
    gate: &AiAssistantGate,
    after: i64,
) -> Result<i64, AgentError> {
    if !gate.is_enabled() {
        return Ok(0);
    }
    let rows = routing::candidates_on(db, after, BATCH_SIZE)
        .await
        .map_err(store)?;
    let next = if rows.len() == BATCH_SIZE as usize {
        rows.last().unwrap().id
    } else {
        0
    };
    let mut work =
        stream::iter(rows.into_iter().map(|row| route_event(db, gate, row))).buffer_unordered(4);
    while let Some(result) = work.next().await {
        if result.is_err() {
            log::debug!("[issue-source] event routing deferred");
        }
    }
    Ok(next)
}

pub(super) async fn route_event(
    db: &DatabaseConnection,
    gate: &AiAssistantGate,
    row: agent_issue_event::Model,
) -> Result<(), AgentError> {
    if row.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Ok(());
    }
    let Some(pending) = routing::pending_on(db, &row).await.map_err(store)? else {
        return Ok(());
    };
    let config = issue_connections::read(db)
        .await
        .map_err(|_| unavailable())?;
    let Some(connection) = config
        .connection_for_repository(&row.actor_id, &pending.current.repository)
        .cloned()
    else {
        return Ok(());
    };
    if pending.header.is_none() {
        let number = pending.comment.issue_number.ok_or_else(unavailable)?;
        let cloned = connection.clone();
        let client = tokio::task::spawn_blocking(move || cloned.client())
            .await
            .map_err(|_| unavailable())?
            .map_err(|_| unavailable())?;
        let snapshot = tokio::time::timeout(
            std::time::Duration::from_secs(90),
            client.read_issue_snapshot(&pending.current.repository, number),
        )
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?;
        let header = snapshot.map(|snapshot| FrozenIssueHeader {
            issue: IssueAccess {
                connection_ref: pending.current.repository.connection_ref.clone(),
                target: snapshot.target,
            },
            title: snapshot.title,
            body: snapshot.body,
        });
        persist_header(db, gate, &row, &connection, header.as_ref()).await?;
    }
    let destination = model_destination(db).await?;
    commit_route(db, gate, &row, &connection, destination.as_ref()).await
}

async fn persist_header(
    db: &DatabaseConnection,
    gate: &AiAssistantGate,
    hint: &agent_issue_event::Model,
    connection: &IssueConnectionConfig,
    header: Option<&FrozenIssueHeader>,
) -> Result<(), AgentError> {
    let txn = begin(db, hint).await?;
    let Some((pending, _)) = recheck_on(&txn, gate, hint, connection).await? else {
        return Ok(());
    };
    if pending.header.is_some() {
        return Ok(());
    }
    let now = chrono::Utc::now().timestamp_millis();
    match header {
        Some(header)
            if pending.original.allows_issue(&header.issue)
                && pending.current.allows_issue(&header.issue) =>
        {
            routing::freeze_header_on(&txn, &pending, header, now)
                .await
                .map_err(store)?;
        }
        Some(_) => routing::ignore_on(&txn, &pending, "outside_scope", now)
            .await
            .map_err(store)?,
        None => routing::ignore_on(&txn, &pending, "pull_request", now)
            .await
            .map_err(store)?,
    }
    txn.commit().await.map_err(|_| unavailable())
}

pub(super) async fn commit_route(
    db: &DatabaseConnection,
    gate: &AiAssistantGate,
    hint: &agent_issue_event::Model,
    connection: &IssueConnectionConfig,
    destination: Option<&DestinationIdentity>,
) -> Result<(), AgentError> {
    let txn = begin(db, hint).await?;
    let Some((pending, root)) = recheck_on(&txn, gate, hint, connection).await? else {
        return Ok(());
    };
    if pending.header.is_none() {
        return Ok(());
    }
    let now = chrono::Utc::now().timestamp_millis();
    if let Some(previous) = routing::latest_round_on(&txn, &pending, now)
        .await
        .map_err(store)?
        && open_round_on(&txn, &previous).await?
    {
        if routing::attach_on(&txn, &pending, &previous, now)
            .await
            .map_err(store)?
        {
            txn.commit().await.map_err(|_| unavailable())?;
        }
        return Ok(());
    }
    if routing::release_attachment_on(&txn, &pending, now)
        .await
        .map_err(store)?
    {
        return txn.commit().await.map_err(|_| unavailable());
    }
    let Some(destination) = destination else {
        return Ok(());
    };
    let targets = if pending.can_use_devices() {
        targets_on(&txn, &root).await?
    } else {
        vec![]
    };
    let minutes = crate::subagent_policy::read(&txn)
        .await
        .map_err(|_| unavailable())?
        .limits
        .max_task_duration_minutes;
    let deadline = now.saturating_add(i64::from(minutes) * 60_000);
    routing::enqueue_on(&txn, &pending, &root, &targets, destination, now, deadline)
        .await
        .map_err(store)?;
    txn.commit().await.map_err(|_| unavailable())
}

async fn recheck_on(
    txn: &DatabaseTransaction,
    gate: &AiAssistantGate,
    hint: &agent_issue_event::Model,
    connection: &IssueConnectionConfig,
) -> Result<Option<(PendingEvent, Box<PersistedAgentSession>)>, AgentError> {
    if !gate.is_enabled() {
        return Ok(None);
    }
    let Some(pending) = routing::pending_on(txn, hint).await.map_err(store)? else {
        return Ok(None);
    };
    let config = issue_connections::read(txn)
        .await
        .map_err(|_| unavailable())?;
    if config.connection_for_repository(&hint.actor_id, &pending.current.repository)
        != Some(connection)
    {
        return Ok(None);
    }
    let Some(root) = coordinator_on(txn, &pending.current).await? else {
        return Ok(None);
    };
    Ok(Some((pending, root)))
}

async fn open_round_on(
    txn: &DatabaseTransaction,
    previous: &desk_diagnose_core::issue_authority::IssueAdmission,
) -> Result<bool, AgentError> {
    if routing::has_open_work_on(
        txn,
        &previous.subscription.actor_id,
        &previous.subscription.conversation_id,
        &previous.request_id,
    )
    .await
    .map_err(store)?
    {
        return Ok(true);
    }
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ActorId.eq(&previous.subscription.actor_id))
        .filter(agent_session::Column::ConversationId.eq(&previous.execution_root_id))
        .one(txn)
        .await
        .map_err(|_| unavailable())?;
    let Some(row) = row else {
        return Ok(false);
    };
    let work =
        PersistedAgentSession::decode_boxed_json(&row.state_json).map_err(|_| unavailable())?;
    if work.version != row.version
        || work.actor_id != previous.subscription.actor_id
        || work.conversation_id != previous.execution_root_id
        || work.agent_role.workbench_request_id() != Some(previous.request_id.as_str())
    {
        return Err(unavailable());
    }
    if work.main_stopped {
        return Ok(false);
    }
    crate::agent_run_event_store::main_requests::slot_held(txn, &work).await
}

async fn begin(
    db: &DatabaseConnection,
    _hint: &agent_issue_event::Model,
) -> Result<DatabaseTransaction, AgentError> {
    crate::db::begin_write(db, agent_issue_event::Entity)
        .await
        .map_err(|_| unavailable())
}

async fn model_destination(
    db: &DatabaseConnection,
) -> Result<Option<DestinationIdentity>, AgentError> {
    let model = crate::model_provider::load(db)
        .await
        .map_err(|_| unavailable())?;
    Ok(model.destination_identity().ok())
}

async fn targets_on(
    txn: &DatabaseTransaction,
    root: &PersistedAgentSession,
) -> Result<Vec<AssistantInputTarget>, AgentError> {
    let Some(roster) = device_roster::load(txn, &root.actor_id, &root.conversation_id)
        .await
        .map_err(store)?
    else {
        return Ok(vec![]);
    };
    let references = roster.members.keys().cloned().collect::<Vec<_>>();
    let allowed =
        crate::agent_run_event_store::eligible_request_device_ids(txn, &references).await?;
    let mut targets = references
        .into_iter()
        .filter(|reference| allowed.contains(reference))
        .map(|reference| AssistantInputTarget {
            device_id: reference.clone(),
            reference,
            member_generation: None,
        })
        .collect::<Vec<_>>();
    roster.freeze(&mut targets).map_err(store)?;
    Ok(targets)
}
