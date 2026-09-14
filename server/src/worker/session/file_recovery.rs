//! Each interactive worker maintains its OS user's private backup store.
//! The store lock serializes this with actions and other workers/processes.
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(super) struct Maintenance(tokio::task::JoinHandle<()>);
impl Drop for Maintenance {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) fn start(data_root: PathBuf, quota: super::QuotaClient) -> Maintenance {
    Maintenance(tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(60));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut previous_unknown = None;
        loop {
            timer.tick().await;
            let root = data_root.clone();
            let quota = quota.clone();
            let result = tokio::task::spawn_blocking(move || -> std::io::Result<Option<usize>> {
                let vault = desk_file_recovery::Vault::open(&root)?;
                let Some(mut locked) = vault.try_lock()? else {
                    return Ok(None);
                };
                locked.observe_system_clock()?;
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(std::io::Error::other)?
                    .as_millis() as u64;
                let (policy, _, _) = quota.policy()?;
                if locked.policy() != &policy { locked.set_policy(policy)?; }
                let unknown = locked.recover_interrupted(now)?.len();
                quota.reconcile_settlements(&mut locked);
                for record in locked.pending_quota_cleanup().into_iter().take(32) {
                    match quota.release(&record) {
                        Ok(()) => locked.acknowledge_quota_cleanup(&record.scope, &record.id)?,
                        Err(error) => tracing::warn!(error_kind = ?error.kind(), "File recovery quota cleanup unconfirmed; retaining durable retry record"),
                    }
                }
                for namespace in locked.pending_quota_namespaces().into_iter().take(32) {
                    match quota.release_namespace(&namespace, locked.namespace_epoch(&namespace)?) {
                        Ok(()) => locked.acknowledge_quota_namespace(&namespace)?,
                        Err(error) => tracing::warn!(error_kind = ?error.kind(), "File recovery namespace quota cleanup unconfirmed; retaining retry record"),
                    }
                }
                #[cfg(unix)]
                locked.maintain_epoch_indexes(&unsafe { libc::geteuid() }.to_string(), &mut quota.clone(), 64)?;
                Ok(Some(unknown))
            })
            .await;
            match result {
                Ok(Ok(Some(unknown))) => {
                    if unknown > 0 && previous_unknown != Some(unknown) {
                        tracing::warn!(
                            unknown_operations = unknown,
                            "File recovery retained operations with missing commit results; mutations will not be repeated"
                        );
                    }
                    previous_unknown = Some(unknown);
                }
                Ok(Ok(None)) => (),
                Ok(Err(error)) => tracing::warn!(error_kind = ?error.kind(),
                    clock_changed = error.get_ref().is_some_and(|error| error.is::<desk_file_recovery::CleanupClockError>()),
                    "File recovery maintenance failed; pending materials will be retried"),
                Err(_) => {
                    tracing::warn!("File recovery maintenance task failed; retrying next interval")
                }
            }
        }
    }))
}
