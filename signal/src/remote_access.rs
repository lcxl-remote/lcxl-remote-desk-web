use std::collections::HashMap;
use std::sync::Arc;

use desk_signal_facade::model::connection::{ConnectionState, SharedConnectionMap};
use desk_signal_facade::model::remote_access::{
    PeerEvictionOutcome, RemoteAccessLockUpdatedData, RemotePeerTerminationResolvedData,
    TerminateRemotePeerData, UpdateRemoteAccessLockData,
};
use desk_signal_facade::model::signal::{RemoteDeskTypeEnum, SignalingModel, SignalingType};
use desk_signal_facade::service::{
    HostRemoteAccessController, RemoteAccessAdmissionAuthorizer, RemoteAccessAdmissionOutcome,
};
use desk_utils::error::DeskErrorCode;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set,
    TransactionTrait,
};
use tokio::sync::RwLock;

use crate::entity::{device_code, host_remote_access_state};

#[derive(Clone)]
pub struct SignalRemoteAccessControl {
    db: DatabaseConnection,
    connections: Arc<SharedConnectionMap>,
    browser_hosts: Arc<RwLock<HashMap<String, String>>>,
}

impl SignalRemoteAccessControl {
    pub fn new(db: DatabaseConnection, connections: Arc<SharedConnectionMap>) -> Self {
        Self {
            db,
            connections,
            browser_hosts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn is_client_locked(&self, client_id: &str) -> Result<bool, sea_orm::DbErr> {
        Ok(host_remote_access_state::Entity::find_by_id(client_id)
            .one(&self.db)
            .await?
            .is_some_and(|state| state.locked))
    }

    async fn apply_lock_request(
        &self,
        client_id: &str,
        request: &UpdateRemoteAccessLockData,
    ) -> Result<RemoteAccessLockUpdatedData, sea_orm::DbErr> {
        let txn = self.db.begin().await?;
        let code = device_code::Entity::find()
            .filter(device_code::Column::ClientId.eq(client_id))
            .one(&txn)
            .await?
            .ok_or_else(|| sea_orm::DbErr::RecordNotFound("device code not found".into()))?;
        let current = host_remote_access_state::Entity::find_by_id(client_id)
            .one(&txn)
            .await?;

        let requested_version = i64::try_from(request.state_version).unwrap_or(i64::MAX);
        let mut generation = code.generation;
        let mut generation_bumped = false;
        let mut committed = current.clone();
        let exact_replay = current.as_ref().is_some_and(|state| {
            requested_version == state.state_version
                && state.locked == request.locked
                && state.lock_id == request.lock_id
        });
        let can_unlock = current.as_ref().is_some_and(|state| {
            !request.locked
                && requested_version > state.state_version
                && state.locked
                && request.lock_id.is_some()
                && state.lock_id == request.lock_id
        });
        let can_initial_unlocked =
            current.is_none() && !request.locked && request.lock_id.is_none();
        let can_lock = request.locked
            && request
                .lock_id
                .as_deref()
                .is_some_and(|lock_id| !lock_id.trim().is_empty())
            && !current
                .as_ref()
                .is_some_and(|state| !state.locked && state.lock_id == request.lock_id)
            && current
                .as_ref()
                .is_none_or(|state| requested_version > state.state_version);

        if !exact_replay && (can_lock || can_unlock || can_initial_unlocked) {
            let now = chrono::Utc::now();
            if request.locked {
                let lock_id = request
                    .lock_id
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| sea_orm::DbErr::Custom("locked request needs lock_id".into()))?;
                let same_committed_lock = current
                    .as_ref()
                    .is_some_and(|state| state.locked && state.lock_id.as_deref() == Some(lock_id));
                if !same_committed_lock {
                    let mut code_update: device_code::ActiveModel = code.clone().into();
                    generation = generation.saturating_add(1);
                    generation_bumped = true;
                    code_update.generation = Set(generation);
                    code_update.updated_at = Set(now);
                    code_update.update(&txn).await?;
                }
            }

            let active = host_remote_access_state::ActiveModel {
                client_id: Set(client_id.to_string()),
                locked: Set(request.locked),
                state_version: Set(requested_version),
                lock_id: Set(if request.locked {
                    request.lock_id.clone()
                } else {
                    current.as_ref().and_then(|state| state.lock_id.clone())
                }),
                updated_at: Set(now),
            };
            if current.is_some() {
                active.update(&txn).await?;
            } else {
                active.insert(&txn).await?;
            }
            committed = host_remote_access_state::Entity::find_by_id(client_id)
                .one(&txn)
                .await?;
        }

        let state = committed.unwrap_or(host_remote_access_state::Model {
            client_id: client_id.to_string(),
            locked: false,
            state_version: 0,
            lock_id: None,
            updated_at: chrono::Utc::now(),
        });
        txn.commit().await?;
        if generation_bumped {
            log::info!(
                "authorization generation advanced: client_id={client_id}, reason=remote_access_lock"
            );
        }
        Ok(RemoteAccessLockUpdatedData {
            request_id: request.request_id.clone(),
            lock_id: state.lock_id,
            state_version: u64::try_from(state.state_version).unwrap_or(0),
            locked: state.locked,
            generation: i64::from(generation),
        })
    }

    async fn push<T: serde::Serialize>(
        source: &ConnectionState,
        request_id: &str,
        signaling_type: SignalingType,
        payload: &T,
    ) {
        let model = SignalingModel::new(
            request_id,
            signaling_type,
            None,
            Some(source.model.connection_id.clone()),
            serde_json::to_value(payload).ok(),
            None,
        );
        if let Ok(text) = serde_json::to_string(&model)
            && let Err(error) = source.session.write().await.text(text).await
        {
            log::warn!("failed to push {signaling_type:?} to host: {error}");
        }
    }
}

impl RemoteAccessAdmissionAuthorizer for SignalRemoteAccessControl {
    fn authorize<'a>(
        &'a self,
        source: &'a ConnectionState,
        connections: &'a SharedConnectionMap,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = RemoteAccessAdmissionOutcome> + Send + 'a>,
    > {
        Box::pin(async move {
            let Some(target_id) = model.to_connection_id.as_deref() else {
                return RemoteAccessAdmissionOutcome::Allow;
            };
            let target = connections.read().await.get(target_id).cloned();
            let Some(target) = target else {
                return RemoteAccessAdmissionOutcome::Allow;
            };
            if target.model.version_info.remote_desk_type != RemoteDeskTypeEnum::Server {
                return RemoteAccessAdmissionOutcome::Allow;
            }
            let Some(client_id) = target.model.version_info.client_id.as_deref() else {
                return RemoteAccessAdmissionOutcome::Reject {
                    code: DeskErrorCode::REMOTE_ACCESS_LOCKED,
                    message: "Target host lock state is unavailable".into(),
                };
            };
            match self.is_client_locked(client_id).await {
                Ok(false) => {
                    if model.signaling_type == SignalingType::RequestRemoteAccess {
                        self.browser_hosts
                            .write()
                            .await
                            .insert(source.model.connection_id.clone(), client_id.to_string());
                    }
                    RemoteAccessAdmissionOutcome::Allow
                }
                Ok(true) => RemoteAccessAdmissionOutcome::Reject {
                    code: DeskErrorCode::REMOTE_ACCESS_LOCKED,
                    message: "Remote access is locked by the host".into(),
                },
                Err(error) => {
                    log::error!("remote-access lock lookup failed: {error}");
                    RemoteAccessAdmissionOutcome::Reject {
                        code: DeskErrorCode::REMOTE_ACCESS_LOCKED,
                        message: "Target host lock state is unavailable".into(),
                    }
                }
            }
        })
    }
}

impl HostRemoteAccessController for SignalRemoteAccessControl {
    fn on_lock_request<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            if source.model.version_info.remote_desk_type != RemoteDeskTypeEnum::Server {
                log::warn!("non-host attempted UpdateRemoteAccessLockData");
                return;
            }
            let Some(client_id) = source.model.version_info.client_id.as_deref() else {
                return;
            };
            let Ok(request) = model.get_data::<UpdateRemoteAccessLockData>() else {
                return;
            };
            if request.request_id != model.request_id {
                log::warn!("remote-access lock request_id payload/frame mismatch");
                return;
            }
            match self.apply_lock_request(client_id, &request).await {
                Ok(ack) => {
                    Self::push(
                        source,
                        &model.request_id,
                        SignalingType::RemoteAccessLockUpdated,
                        &ack,
                    )
                    .await;
                }
                Err(error) => log::error!("remote-access mirror commit failed: {error}"),
            }
        })
    }

    fn on_terminate_peer_request<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            if source.model.version_info.remote_desk_type != RemoteDeskTypeEnum::Server {
                return;
            }
            let Ok(request) = model.get_data::<TerminateRemotePeerData>() else {
                return;
            };
            let owns_peer = self
                .browser_hosts
                .read()
                .await
                .get(&request.target_connection_id)
                .is_some_and(|host_client_id| {
                    source.model.version_info.client_id.as_deref() == Some(host_client_id.as_str())
                });
            let outcome = if owns_peer {
                let target = self
                    .connections
                    .write()
                    .await
                    .remove(&request.target_connection_id);
                if let Some(target) = target {
                    let session = target.session.read().await.clone();
                    let _ = session.close(None).await;
                    self.browser_hosts
                        .write()
                        .await
                        .remove(&request.target_connection_id);
                    PeerEvictionOutcome::Delivered
                } else {
                    PeerEvictionOutcome::Unavailable
                }
            } else {
                PeerEvictionOutcome::Unavailable
            };
            let ack = RemotePeerTerminationResolvedData {
                operation_id: request.operation_id,
                target_connection_id: request.target_connection_id,
                outcome,
            };
            Self::push(
                source,
                &model.request_id,
                SignalingType::RemotePeerTerminationResolved,
                &ack,
            )
            .await;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, Database, Schema};

    async fn setup() -> (DatabaseConnection, SignalRemoteAccessControl) {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(device_code::Entity))
            .await
            .unwrap();
        db.execute(&schema.create_table_from_entity(host_remote_access_state::Entity))
            .await
            .unwrap();
        device_code::ActiveModel {
            client_id: Set("host-1".into()),
            device_code: Set("ABC123".into()),
            capabilities: Set(None),
            generation: Set(0),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        let connections = Arc::new(SharedConnectionMap::new());
        let control = SignalRemoteAccessControl::new(db.clone(), connections);
        (db, control)
    }

    fn request(version: u64, lock_id: Option<&str>, locked: bool) -> UpdateRemoteAccessLockData {
        UpdateRemoteAccessLockData {
            request_id: format!("request-{version}"),
            lock_id: lock_id.map(str::to_string),
            state_version: version,
            locked,
        }
    }

    #[tokio::test]
    async fn lock_commit_bumps_generation_once_without_rotating_code() {
        let (db, control) = setup().await;
        let lock = request(2, Some("lock-a"), true);

        let first = control.apply_lock_request("host-1", &lock).await.unwrap();
        let replay = control.apply_lock_request("host-1", &lock).await.unwrap();

        assert_eq!(first.generation, 1);
        assert_eq!(replay.generation, 1);
        assert!(control.is_client_locked("host-1").await.unwrap());
        let code = device_code::Entity::find()
            .filter(device_code::Column::ClientId.eq("host-1"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(code.device_code, "ABC123");
        assert_eq!(code.generation, 1);
    }

    #[tokio::test]
    async fn unlock_requires_current_lock_id_and_never_rewinds_generation() {
        let (db, control) = setup().await;
        control
            .apply_lock_request("host-1", &request(2, Some("lock-a"), true))
            .await
            .unwrap();

        let wrong = control
            .apply_lock_request("host-1", &request(3, Some("lock-b"), false))
            .await
            .unwrap();
        assert!(wrong.locked);
        assert_eq!(wrong.state_version, 2);

        let unlocked = control
            .apply_lock_request("host-1", &request(3, Some("lock-a"), false))
            .await
            .unwrap();
        assert!(!unlocked.locked);
        assert_eq!(unlocked.state_version, 3);
        assert_eq!(unlocked.generation, 1);
        let code = device_code::Entity::find()
            .filter(device_code::Column::ClientId.eq("host-1"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(code.generation, 1);
    }

    #[tokio::test]
    async fn initial_unlock_with_a_forged_lock_id_is_not_committed() {
        let (_db, control) = setup().await;
        let ack = control
            .apply_lock_request("host-1", &request(9, Some("unknown-lock"), false))
            .await
            .unwrap();
        assert!(!ack.locked);
        assert_eq!(ack.state_version, 0);
        assert_eq!(ack.lock_id, None);
    }

    #[tokio::test]
    async fn same_lock_round_version_advance_does_not_bump_generation_again() {
        let (_db, control) = setup().await;
        control
            .apply_lock_request("host-1", &request(2, Some("lock-a"), true))
            .await
            .unwrap();
        let ack = control
            .apply_lock_request("host-1", &request(3, Some("lock-a"), true))
            .await
            .unwrap();
        assert!(ack.locked);
        assert_eq!(ack.state_version, 3);
        assert_eq!(ack.generation, 1);
    }

    #[tokio::test]
    async fn a_finished_lock_id_cannot_be_reused_for_a_new_round() {
        let (_db, control) = setup().await;
        control
            .apply_lock_request("host-1", &request(2, Some("lock-a"), true))
            .await
            .unwrap();
        control
            .apply_lock_request("host-1", &request(3, Some("lock-a"), false))
            .await
            .unwrap();

        let reused = control
            .apply_lock_request("host-1", &request(4, Some("lock-a"), true))
            .await
            .unwrap();
        assert!(!reused.locked);
        assert_eq!(reused.state_version, 3);

        let new_round = control
            .apply_lock_request("host-1", &request(4, Some("lock-b"), true))
            .await
            .unwrap();
        assert!(new_round.locked);
        let late_unlock = control
            .apply_lock_request("host-1", &request(5, Some("lock-a"), false))
            .await
            .unwrap();
        assert!(late_unlock.locked);
        assert_eq!(late_unlock.lock_id.as_deref(), Some("lock-b"));
    }
}
