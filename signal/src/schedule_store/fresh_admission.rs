//! Single-account runtime checks around atomic fresh occurrence admission.
use super::{ScheduleStore, ScheduleStoreError, entity};
use crate::{
    control_authorizer::SINGLE_ACCOUNT_USER_ID, device_assistant_gate::DeviceAssistantGate,
    entity::agent_schedule_run,
};
use desk_agent_protocol::AgentScope;
use desk_diagnose_core::session::PersistedAgentSession;
use desk_signal_facade::model::{
    auth_context::AuthKind, connection::SharedConnectionMap, signal::RemoteDeskTypeEnum,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, TransactionTrait};

/// Scope and policy must come from current server-side projection.
pub struct FreshTaskClaim<'a> {
    pub owner: i32,
    pub run_id: &'a str,
    pub node_id: &'a str,
    pub lease_seconds: u32,
    pub policy_revision: i64,
    pub scope: AgentScope,
}

pub struct ClaimedFreshTask {
    pub target_connection_id: String,
    pub session: PersistedAgentSession,
}

async fn target(
    connections: &SharedConnectionMap,
    device: &str,
) -> Result<String, ScheduleStoreError> {
    let map = connections.read().await;
    let mut targets = map.values().filter(|peer| {
        peer.auth_context.auth_kind == AuthKind::TokenAuth
            && peer.auth_context.remote_desk_type == RemoteDeskTypeEnum::Server
            && peer.model.version_info.client_id.as_deref() == Some(device)
    });
    let first = targets.next().ok_or(ScheduleStoreError::NotFound)?;
    if targets.next().is_some() {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(first.model.connection_id.clone())
}

impl ScheduleStore {
    /// Runtime checks are repeated after DB waits. Model/tool dispatch must still
    /// check current connectivity, readiness, policy and parent authority itself.
    pub async fn claim_fresh_task(
        &self,
        connections: &SharedConnectionMap,
        gate: &DeviceAssistantGate,
        input: FreshTaskClaim<'_>,
    ) -> Result<ClaimedFreshTask, ScheduleStoreError> {
        let settings = gate.snapshot();
        if input.owner != SINGLE_ACCOUNT_USER_ID || !settings.enabled {
            return Err(ScheduleStoreError::NotFound);
        }
        let txn = self.db.begin().await?;
        let pending = agent_schedule_run::Entity::find()
            .filter(agent_schedule_run::Column::OwnerUserId.eq(input.owner))
            .filter(agent_schedule_run::Column::RunId.eq(input.run_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let task = entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(input.owner))
            .filter(entity::Column::ScheduleId.eq(&pending.schedule_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let target_connection_id = target(connections, &task.target_device_id).await?;
        if pending.status == "awaiting_permission" {
            let request_id = pending
                .result_ref
                .as_deref()
                .and_then(|value| {
                    value
                        .strip_prefix("permission:")
                        .or_else(|| value.strip_prefix("directory:"))
                })
                .ok_or(ScheduleStoreError::Conflict)?;
            let session = Self::claim_fresh_approval_on(&txn, input, request_id).await?;
            if target(connections, &task.target_device_id).await? != target_connection_id
                || gate.snapshot() != settings
            {
                return Err(ScheduleStoreError::Conflict);
            }
            txn.commit().await?;
            return Ok(ClaimedFreshTask {
                target_connection_id,
                session,
            });
        }
        let work =
            Self::claim_queued_on(&txn, input.run_id, input.node_id, input.lease_seconds).await?;
        let authority = Self::lock_run_authority(
            &txn,
            input.owner,
            &task.target_device_id,
            input.run_id,
            input.node_id,
            work.lease_epoch,
        )
        .await?;
        let session =
            Self::insert_fresh_session_on(&txn, &authority, input.policy_revision, input.scope)
                .await?;
        if target(connections, &task.target_device_id).await? != target_connection_id
            || gate.snapshot() != settings
        {
            return Err(ScheduleStoreError::Conflict);
        }
        txn.commit().await?;
        Ok(ClaimedFreshTask {
            target_connection_id,
            session,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::publication::tests::{Verifier, fixture_on};
    use super::*;
    use crate::entity::{agent_session, agent_task_budget_reservation};
    use desk_agent_protocol::{ExecutionMode, device_assistant::DeviceAssistantSettings};
    use desk_signal_facade::model::{
        auth_context::AuthContext,
        connection::{ConnectionModel, ConnectionState},
        version::VersionInfo,
    };
    use sea_orm::{ConnectionTrait, Database, Schema};

    mod model_gateway;

    async fn peer(id: &str, device: &str) -> ConnectionState {
        let request = actix_web::test::TestRequest::get()
            .insert_header(("upgrade", "websocket"))
            .insert_header(("connection", "upgrade"))
            .insert_header(("sec-websocket-version", "13"))
            .insert_header(("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="))
            .to_http_request();
        let payload = <actix_web::web::Payload as actix_web::FromRequest>::from_request(
            &request,
            &mut actix_web::dev::Payload::None,
        )
        .await
        .unwrap();
        let (_, socket, _) = actix_ws::handle(&request, payload).unwrap();
        ConnectionState {
            model: ConnectionModel {
                connection_id: id.into(),
                ip: None,
                version_info: VersionInfo::new(
                    1,
                    1,
                    "fixture".into(),
                    RemoteDeskTypeEnum::Server,
                    None,
                    Some(device.into()),
                ),
                device_id: None,
                owner_node_id: None,
            },
            session: std::sync::Arc::new(tokio::sync::RwLock::new(socket)),
            terminal_connection_ids: Default::default(),
            request_callback_map: Default::default(),
            device_code: None,
            auth_context: AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Server),
        }
    }

    #[actix_web::test]
    async fn fresh_admission_requires_one_live_token_device_and_rolls_back_denials() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let (store, task, _, publication) = fixture_on(db).await;
        store
            .db
            .execute(
                &Schema::new(store.db.get_database_backend())
                    .create_table_from_entity(agent_session::Entity),
            )
            .await
            .unwrap();
        store
            .publish_task(1, &publication, &Verifier(true))
            .await
            .unwrap();
        let queued = store
            .enqueue_manual(1, &task.schedule_id, "admission")
            .await
            .unwrap();
        let before = store.read(1, &task.schedule_id).await.unwrap();
        let connections = SharedConnectionMap::new();
        let gate = DeviceAssistantGate::default();
        for failure in [
            "disabled",
            "offline",
            "cookie",
            "duplicate",
            "owner",
            "policy",
            "none",
        ] {
            connections.write().await.clear();
            gate.replace(DeviceAssistantSettings {
                revision: 1,
                enabled: failure != "disabled",
            });
            if failure != "offline" {
                let mut node = peer("node", &task.target_device_id).await;
                if failure == "cookie" {
                    node.auth_context = AuthContext::cookie(1, RemoteDeskTypeEnum::Server);
                }
                connections.write().await.insert("node".into(), node);
            }
            if failure == "duplicate" {
                connections
                    .write()
                    .await
                    .insert("other".into(), peer("other", &task.target_device_id).await);
            }
            let result = store
                .claim_fresh_task(
                    &connections,
                    &gate,
                    FreshTaskClaim {
                        owner: if failure == "owner" { 2 } else { 1 },
                        run_id: &queued.run_id,
                        node_id: "scheduler",
                        lease_seconds: 90,
                        policy_revision: if failure == "policy" { 2 } else { 1 },
                        scope: AgentScope {
                            granted: vec![],
                            mode: ExecutionMode::SuggestOnly,
                            expires_at: None,
                            policy_name: None,
                        },
                    },
                )
                .await;
            if failure == "none" {
                let claimed = result.unwrap();
                assert_eq!(claimed.target_connection_id, "node");
                assert_eq!(claimed.session.conversation_id, queued.run_id);
                assert_eq!(
                    agent_session::Entity::find()
                        .all(&store.db)
                        .await
                        .unwrap()
                        .len(),
                    1
                );
                assert_eq!(
                    agent_task_budget_reservation::Entity::find()
                        .all(&store.db)
                        .await
                        .unwrap()
                        .len(),
                    1
                );
            } else {
                assert!(result.is_err(), "{failure}");
                assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before);
                assert_eq!(
                    agent_schedule_run::Entity::find_by_id(queued.id)
                        .one(&store.db)
                        .await
                        .unwrap()
                        .unwrap(),
                    queued
                );
                assert!(
                    agent_session::Entity::find()
                        .all(&store.db)
                        .await
                        .unwrap()
                        .is_empty()
                );
                assert!(
                    agent_task_budget_reservation::Entity::find()
                        .all(&store.db)
                        .await
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }
}
