//! Route recovery management by a daemon-verified account, never by a claimed PID.
use super::*;
impl WorkerManager {
    pub(super) async fn send_windows_file_recovery_request(
        &self,
        user: &str,
        payload: desk_ipc_protocol::message::FileRecoveryRequestPayload,
    ) -> Result<(), String> {
        let inner = self.inner.lock().await;
        let worker = inner
            .resident_workers
            .iter()
            .filter(|(key, worker)| {
                key.desktop == DesktopTarget::WindowsDefault
                    && file_recovery_quota::windows_worker_user(worker).as_deref() == Some(user)
            })
            .max_by_key(|(_, worker)| worker.incarnation.0)
            .map(|(_, worker)| worker)
            .ok_or_else(|| "original recovery user worker unavailable".to_owned())?;
        worker
            .ipc_tx
            .send(ServiceToWorker::ManageFileRecovery(payload))
            .map_err(|_| "original recovery user worker disconnected".to_owned())
    }
}
