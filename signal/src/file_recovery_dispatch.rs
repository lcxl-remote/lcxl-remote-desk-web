//! Persistent cleanup intents are retried when their authenticated device reconnects.
use crate::{
    control_authorizer::SINGLE_ACCOUNT_USER_ID,
    file_recovery_cleanup_store::FileRecoveryCleanupStore,
    file_recovery_scope_store::FileRecoveryScopeStore,
};
use actix_web::web;
use desk_agent_protocol::file_recovery::*;
use desk_signal_facade::{
    model::{auth_context::AuthKind, connection::SharedConnectionMap, signal::RemoteDeskTypeEnum},
    service::file_recovery::{RecoveryRequestError, request_authorized},
};
use std::{sync::OnceLock, time::Duration};
fn wakeup() -> &'static tokio::sync::Notify {
    static WAKE: OnceLock<tokio::sync::Notify> = OnceLock::new();
    WAKE.get_or_init(tokio::sync::Notify::new)
}
pub fn notify() {
    wakeup().notify_one();
}
pub async fn run(db: sea_orm::DatabaseConnection, connections: web::Data<SharedConnectionMap>) {
    let store = FileRecoveryCleanupStore::new(db.clone());
    let scopes = FileRecoveryScopeStore::new(db.clone());
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! { _ = tick.tick() => (), _ = wakeup().notified() => () }
        let targets: Vec<_> = connections
            .read()
            .await
            .iter()
            .filter_map(|(connection, target)| {
                (target.auth_context.auth_kind == AuthKind::TokenAuth
                    && target.auth_context.remote_desk_type == RemoteDeskTypeEnum::Server)
                    .then(|| {
                        target
                            .model
                            .version_info
                            .client_id
                            .as_ref()
                            .map(|device| (connection.clone(), device.clone()))
                    })
                    .flatten()
            })
            .collect();
        for (connection, device) in targets {
            let now = chrono::Utc::now().timestamp_millis();
            let job = match store
                .claim_for_device(&device, &SINGLE_ACCOUNT_USER_ID.to_string(), now)
                .await
            {
                Ok(Some(job)) => job,
                Ok(None) => continue,
                Err(_) => {
                    log::warn!("File recovery cleanup queue unavailable; retrying");
                    continue;
                }
            };
            let pending_scopes = match scopes
                .pending(&job.actor_id, &job.device_id, &job.conversation_id)
                .await
            {
                Ok(scopes) => scopes,
                Err(_) => {
                    let _ = store
                        .retry(
                            &job,
                            "cleanup_pending",
                            chrono::Utc::now().timestamp_millis(),
                        )
                        .await;
                    continue;
                }
            };
            // One original namespace per lease keeps the RPC bounded. Other users
            // remain pending until their authenticated worker is available.
            let original_scope = if pending_scopes.is_empty() {
                None
            } else {
                // An offline original user must not starve other available users.
                pending_scopes.get(job.attempts.unsigned_abs() as usize % pending_scopes.len())
            };
            let result = request_authorized(
                &connections,
                &connection,
                SINGLE_ACCOUNT_USER_ID,
                None,
                FileRecoveryRequest {
                    expected_authority: original_scope.map(|scope| scope.authority.clone()),
                    expected_os_user: original_scope.map(|scope| scope.os_user.clone()),
                    command: FileRecoveryCommand::DeleteConversation {
                        conversation_id: job.conversation_id.clone(),
                    },
                },
            )
            .await;
            let now = chrono::Utc::now().timestamp_millis();
            let saved = match result {
                Ok(FileRecoveryReply {
                    outcome: FileRecoveryOutcome::Deleted { complete: true },
                    authority,
                    os_user,
                }) => {
                    let acknowledged = match original_scope {
                        Some(scope) => {
                            scopes
                                .acknowledge_cleanup(scope, &authority, &os_user, now)
                                .await
                        }
                        None => Ok(true),
                    };
                    match acknowledged {
                        Ok(true) if pending_scopes.len() <= 1 => {
                            store
                                .complete(
                                    &job.conversation_id,
                                    job.lease_id.as_deref().unwrap(),
                                    now,
                                )
                                .await
                        }
                        Ok(true) => store.retry(&job, "cleanup_pending", now).await,
                        Ok(false) => store.retry(&job, "identity_changed", now).await,
                        Err(_) => store.retry(&job, "cleanup_pending", now).await,
                    }
                }
                other => {
                    let category = match other {
                        Ok(FileRecoveryReply {
                            outcome:
                                FileRecoveryOutcome::Unavailable {
                                    reason: FileRecoveryFailure::IdentityChanged,
                                },
                            ..
                        }) => "identity_changed",
                        Err(RecoveryRequestError::Timeout) => "timeout",
                        Err(RecoveryRequestError::Offline) => "offline",
                        _ => "cleanup_pending",
                    };
                    store.retry(&job, category, now).await
                }
            };
            if saved.is_err() {
                log::warn!(
                    "File recovery cleanup acknowledgment could not be saved; durable lease will retry"
                );
            }
        }
    }
}
