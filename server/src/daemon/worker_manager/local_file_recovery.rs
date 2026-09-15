//! Local HTTP waiters; replies never enter the signaling output lane.
#[cfg(test)]
#[path = "local_file_recovery_tests.rs"]
mod tests;

use super::*;
use desk_agent_protocol::file_recovery::FileRecoveryFailure as Failure;
use desk_ipc_protocol::local_file_recovery::{
    LocalFileRecoveryCommand, LocalFileRecoveryOutcome, LocalFileRecoveryReply,
    LocalFileRecoveryRequest,
};

struct Pending {
    key: WorkerKey,
    incarnation: WorkerIncarnation,
    sid: String,
    session_id: u32,
    tx: oneshot::Sender<Result<LocalFileRecoveryOutcome, Failure>>,
}

#[derive(Clone, Default)]
pub(super) struct PendingRequests(Arc<StdMutex<HashMap<String, Pending>>>);

struct RemovePending {
    requests: PendingRequests,
    id: String,
}
impl Drop for RemovePending {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.requests.0.lock() {
            pending.remove(&self.id);
        }
    }
}

impl WorkerManager {
    pub(crate) async fn request_local_file_recovery(
        &self,
        user: &crate::windows_local_user::LocalUser,
        command: LocalFileRecoveryCommand,
    ) -> Result<LocalFileRecoveryOutcome, Failure> {
        command.validate().map_err(|_| Failure::InvalidRequest)?;
        if !user.is_alive() {
            return Err(Failure::IdentityChanged);
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        let cleanup = RemovePending {
            requests: self.local_recovery_requests.clone(),
            id: request_id.clone(),
        };
        let (tx, rx) = oneshot::channel();
        {
            let inner = self.inner.lock().await;
            let (key, worker) = inner
                .resident_workers
                .iter()
                .filter(|(key, worker)| {
                    key.desktop == DesktopTarget::WindowsDefault
                        && worker.session_id == user.session_id
                        && file_recovery_quota::windows_worker_user(worker).as_deref()
                            == Some(user.sid.as_str())
                })
                .max_by_key(|(_, worker)| worker.incarnation.0)
                .ok_or(Failure::WorkerUnavailable)?;
            if !user.is_alive() {
                return Err(Failure::IdentityChanged);
            }
            {
                let mut pending = self
                    .local_recovery_requests
                    .0
                    .lock()
                    .map_err(|_| Failure::StorageUnavailable)?;
                if pending.len() >= 8 {
                    return Err(Failure::Busy);
                }
                pending.insert(
                    request_id.clone(),
                    Pending {
                        key: key.clone(),
                        incarnation: worker.incarnation,
                        sid: user.sid.clone(),
                        session_id: user.session_id,
                        tx,
                    },
                );
            }
            worker
                .ipc_tx
                .send(ServiceToWorker::ManageLocalFileRecovery(
                    LocalFileRecoveryRequest {
                        request_id,
                        os_user: user.sid.clone(),
                        session_id: user.session_id,
                        deadline_unix_ms: (chrono::Utc::now().timestamp_millis().max(1) as u64)
                            .saturating_add(30_000),
                        command,
                    },
                ))
                .map_err(|_| Failure::WorkerUnavailable)?;
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
            .await
            .map_err(|_| Failure::WorkerUnavailable)?
            .map_err(|_| Failure::WorkerUnavailable)?;
        drop(cleanup);
        if !user.is_alive() {
            return Err(Failure::IdentityChanged);
        }
        result
    }

    pub(crate) async fn complete_local_file_recovery(
        &self,
        key: Option<&WorkerKey>,
        incarnation: WorkerIncarnation,
        reply: LocalFileRecoveryReply,
    ) {
        let inner = self.inner.lock().await;
        let Some(key) = key else { return };
        let Some(worker) = inner
            .resident_workers
            .get(key)
            .filter(|worker| worker.incarnation == incarnation)
        else {
            return;
        };
        let sid = file_recovery_quota::windows_worker_user(worker);
        let Ok(mut pending) = self.local_recovery_requests.0.lock() else {
            return;
        };
        let Some(entry) = pending.get(&reply.request_id) else {
            return;
        };
        if &entry.key != key
            || entry.incarnation != incarnation
            || worker.session_id != entry.session_id
            || sid.as_deref() != Some(entry.sid.as_str())
        {
            return;
        }
        if let Some(entry) = pending.remove(&reply.request_id) {
            let _ = entry.tx.send(reply.outcome);
        }
    }
}
