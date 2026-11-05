//! A coordinator's device configuration is independent of its active turn lease.

use std::collections::BTreeMap;

use desk_diagnose_core::dynamic_run::AssistantInputTarget;
use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession};
use sea_orm::QueryTrait;
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait, QueryFilter, QuerySelect, Set,
};

use crate::{AssistantStoreError, entity::agent_assistant_device_roster as roster};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRoster {
    pub revision: u64,
    pub members: BTreeMap<String, u64>,
}

pub fn configured_conversation_ids() -> sea_orm::sea_query::SelectStatement {
    roster::Entity::find()
        .select_only()
        .column(roster::Column::ConversationId)
        .into_query()
}

pub async fn delete(
    db: &impl ConnectionTrait,
    actor_id: &str,
    conversation_id: &str,
) -> Result<(), sea_orm::DbErr> {
    roster::Entity::delete_many()
        .filter(roster::Column::ConversationId.eq(conversation_id))
        .filter(roster::Column::ActorId.eq(actor_id))
        .exec(db)
        .await?;
    Ok(())
}

impl DeviceRoster {
    pub fn freeze(&self, targets: &mut [AssistantInputTarget]) -> Result<(), AssistantStoreError> {
        for target in targets {
            target.member_generation = Some(
                *self
                    .members
                    .get(&target.device_id)
                    .ok_or(AssistantStoreError::Inaccessible)?,
            );
        }
        Ok(())
    }
}

pub async fn permits_current_target(
    db: &impl ConnectionTrait,
    session: &desk_diagnose_core::session::PersistedAgentSession,
    device_id: &str,
) -> Result<bool, AssistantStoreError> {
    let Some(generation) = session.assistant_request_device_generation(device_id) else {
        return Ok(true);
    };
    let root = session.device_roster_conversation_id();
    Ok(load(db, &session.actor_id, root)
        .await?
        .is_some_and(|roster| roster.members.get(device_id) == Some(&generation)))
}

/// Project the live subset of the original request. Ordinary conversations have
/// no membership epochs and continue to use their request's explicit targets.
pub async fn current_request_devices(
    db: &impl ConnectionTrait,
    session: &PersistedAgentSession,
) -> Result<Vec<String>, AssistantStoreError> {
    let devices = session
        .assistant_request_devices()
        .ok_or(AssistantStoreError::Inaccessible)?;
    if devices
        .iter()
        .all(|id| session.assistant_request_device_generation(id).is_none())
    {
        return Ok(devices.to_vec());
    }
    let root = session.device_roster_conversation_id();
    let roster = load(db, &session.actor_id, root).await?;
    Ok(devices
        .iter()
        .filter(|id| {
            session
                .assistant_request_device_generation(id)
                .is_some_and(|generation| {
                    roster.as_ref().and_then(|roster| roster.members.get(*id)) == Some(&generation)
                })
        })
        .cloned()
        .collect())
}

/// A coordinator roster may be initialized before the first accepted input.
pub fn has_existing_input(
    state_json: &str,
    actor_id: &str,
    conversation_id: &str,
    version: i64,
) -> Result<bool, AssistantStoreError> {
    let state = PersistedAgentSession::decode_boxed_json(state_json)
        .map_err(|_| AssistantStoreError::Inaccessible)?;
    if state.surface != AgentSessionSurface::AiAssistant
        || !state.agent_role.is_main()
        || state.conversation_id != conversation_id
        || state.actor_id != actor_id
        || !state.device_id.is_empty()
        || state.version != version
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(state.input_revision != 0)
}

pub async fn load(
    db: &impl ConnectionTrait,
    actor_id: &str,
    conversation_id: &str,
) -> Result<Option<DeviceRoster>, AssistantStoreError> {
    decode(
        roster::Entity::find_by_id(conversation_id).one(db).await?,
        actor_id,
    )
}

/// Postgres callers use the same conversation lock before initializing a roster
/// or freezing targets. An existing row also fences writers from other nodes.
pub async fn lock(
    txn: &impl ConnectionTrait,
    actor_id: &str,
    conversation_id: &str,
) -> Result<Option<DeviceRoster>, AssistantStoreError> {
    let query = roster::Entity::find_by_id(conversation_id);
    let query = if txn.get_database_backend() == DatabaseBackend::Postgres {
        query.lock_exclusive()
    } else {
        query
    };
    decode(query.one(txn).await?, actor_id)
}

fn decode(
    row: Option<roster::Model>,
    actor_id: &str,
) -> Result<Option<DeviceRoster>, AssistantStoreError> {
    let Some(row) = row else { return Ok(None) };
    if row.actor_id != actor_id {
        return Err(AssistantStoreError::Inaccessible);
    }
    let invalid = || AssistantStoreError::Invalid("Invalid assistant device roster");
    let revision = u64::try_from(row.revision).map_err(|_| invalid())?;
    let members: BTreeMap<String, u64> =
        serde_json::from_str(&row.members_json).map_err(|_| invalid())?;
    desk_agent_protocol::ai_assistant::validate_target_refs(members.keys().map(String::as_str))
        .map_err(|_| invalid())?;
    if revision == 0
        || members
            .values()
            .any(|generation| *generation == 0 || *generation > revision)
    {
        return Err(invalid());
    }
    Ok(Some(DeviceRoster { revision, members }))
}

/// The host validates the coordinator and its device ownership before calling this
/// function. All ids are native, resolved under the host's owner/target locks.
pub async fn replace(
    txn: &impl ConnectionTrait,
    actor_id: &str,
    conversation_id: &str,
    expected_revision: u64,
    device_ids: Vec<String>,
) -> Result<DeviceRoster, AssistantStoreError> {
    desk_agent_protocol::ai_assistant::validate_target_refs(device_ids.iter().map(String::as_str))
        .map_err(AssistantStoreError::Invalid)?;
    let previous = lock(txn, actor_id, conversation_id).await?;
    let current_revision = previous.as_ref().map_or(0, |value| value.revision);
    if expected_revision != current_revision {
        return Err(AssistantStoreError::Stale { current_revision });
    }
    if let Some(previous) = &previous
        && previous.members.len() == device_ids.len()
        && device_ids
            .iter()
            .all(|id| previous.members.contains_key(id))
    {
        return Ok(previous.clone());
    }
    let revision = current_revision
        .checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or(AssistantStoreError::Capacity)?;
    let members = device_ids
        .into_iter()
        .map(|id| {
            let generation = previous
                .as_ref()
                .and_then(|roster| roster.members.get(&id))
                .copied()
                .unwrap_or(revision);
            (id, generation)
        })
        .collect::<BTreeMap<_, _>>();
    let members_json = serde_json::to_string(&members)
        .map_err(|_| AssistantStoreError::Invalid("Could not encode assistant device roster"))?;
    let written = if previous.is_none() {
        roster::Entity::insert(roster::ActiveModel {
            conversation_id: Set(conversation_id.into()),
            actor_id: Set(actor_id.into()),
            revision: Set(revision as i64),
            members_json: Set(members_json),
        })
        .on_conflict(
            OnConflict::column(roster::Column::ConversationId)
                .do_nothing()
                .to_owned(),
        )
        .exec_without_returning(txn)
        .await?
            == 1
    } else {
        roster::Entity::update_many()
            .col_expr(roster::Column::Revision, Expr::value(revision as i64))
            .col_expr(roster::Column::MembersJson, Expr::value(members_json))
            .filter(roster::Column::ConversationId.eq(conversation_id))
            .filter(roster::Column::ActorId.eq(actor_id))
            .filter(roster::Column::Revision.eq(current_revision as i64))
            .exec(txn)
            .await?
            .rows_affected
            == 1
    };
    if !written {
        let current_revision = load(txn, actor_id, conversation_id)
            .await?
            .map_or(0, |value| value.revision);
        return Err(AssistantStoreError::Stale { current_revision });
    }
    Ok(DeviceRoster { revision, members })
}
