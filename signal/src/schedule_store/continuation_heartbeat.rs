//! Runtime adapter for paired scheduled-run and session lease transactions.
use super::{
    ClaimedContinuation, ContinuationLease, FreshTaskLease, ScheduleStore, ScheduleStoreError,
};
use desk_diagnose_core::seam::{HeartbeatGuard, LeaseHeartbeat};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio_util::sync::CancellationToken;

pub struct ScheduleHeartbeat {
    store: ScheduleStore,
    fresh: bool,
    owner: i32,
    run_id: String,
    conversation_id: String,
    node_id: String,
    run_epoch: i64,
    session_token: u64,
    lease_seconds: u32,
    healthy: Arc<AtomicBool>,
    started: AtomicBool,
    cancel: CancellationToken,
}
impl ScheduleHeartbeat {
    /// Fresh tasks additionally require their current published parent on every tick.
    /// The cancellation token must also be used by the model/tool transport.
    pub async fn new_fresh(
        store: ScheduleStore,
        lease: FreshTaskLease<'_>,
        lease_seconds: u32,
        cancel: CancellationToken,
    ) -> Result<Self, ScheduleStoreError> {
        let identity = (
            lease.owner,
            lease.run_id.to_owned(),
            lease.node_id.to_owned(),
            lease.run_epoch,
            lease.session_token,
        );
        if cancel.is_cancelled()
            || !store.renew_fresh_task(lease, lease_seconds).await?
            || cancel.is_cancelled()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(Self {
            store,
            fresh: true,
            owner: identity.0,
            conversation_id: identity.1.clone(),
            run_id: identity.1,
            node_id: identity.2,
            run_epoch: identity.3,
            session_token: identity.4,
            lease_seconds,
            healthy: Arc::new(AtomicBool::new(true)),
            started: AtomicBool::new(false),
            cancel,
        })
    }
    /// Verify both current leases before returning a heartbeat to the model loop.
    /// Cancellation uses the SAME token supplied to the runtime's model/tool seams.
    pub async fn new(
        store: ScheduleStore,
        claimed: &ClaimedContinuation,
        node_id: String,
        lease_seconds: u32,
        cancel: CancellationToken,
    ) -> Result<Self, ScheduleStoreError> {
        if cancel.is_cancelled()
            || !store
                .renew_conversation_resume(
                    ContinuationLease {
                        owner: claimed.run.owner_user_id,
                        run_id: &claimed.run.run_id,
                        node_id: &node_id,
                        run_epoch: claimed.run.lease_epoch,
                        session_token: claimed.session.lease_token,
                    },
                    lease_seconds,
                )
                .await?
            || cancel.is_cancelled()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(Self {
            store,
            fresh: false,
            owner: claimed.run.owner_user_id,
            run_id: claimed.run.run_id.clone(),
            conversation_id: claimed.run.conversation_id.clone(),
            node_id,
            run_epoch: claimed.run.lease_epoch,
            session_token: claimed.session.lease_token,
            lease_seconds,
            healthy: Arc::new(AtomicBool::new(true)),
            started: AtomicBool::new(false),
            cancel,
        })
    }
}
struct Guard(Option<tokio::task::JoinHandle<()>>);
impl HeartbeatGuard for Guard {}
impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}
impl LeaseHeartbeat for ScheduleHeartbeat {
    fn start(&self, conversation_id: String, session_token: u64) -> Box<dyn HeartbeatGuard> {
        if conversation_id != self.conversation_id
            || session_token != self.session_token
            || self.started.swap(true, Ordering::AcqRel)
            || self.cancel.is_cancelled()
        {
            self.healthy.store(false, Ordering::Release);
            self.cancel.cancel();
            return Box::new(Guard(None));
        }
        let store = self.store.clone();
        let fresh = self.fresh;
        let owner = self.owner;
        let run_id = self.run_id.clone();
        let node_id = self.node_id.clone();
        let run_epoch = self.run_epoch;
        let lease_seconds = self.lease_seconds;
        let healthy = self.healthy.clone();
        let cancel = self.cancel.clone();
        let task = tokio::spawn(async move {
            let mut timer = tokio::time::interval(std::time::Duration::from_secs(u64::from(
                (lease_seconds / 3).min(10),
            )));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = timer.tick() => {
                        let renewed = store.renew_paired_lease(ContinuationLease { owner, run_id: &run_id, node_id: &node_id, run_epoch, session_token }, lease_seconds, fresh).await;
                        if !matches!(renewed, Ok(true)) {
                            healthy.store(false, Ordering::Release);
                            cancel.cancel();
                            break;
                        }
                    }
                }
            }
        });
        Box::new(Guard(Some(task)))
    }
    fn check_current(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + '_>> {
        Box::pin(async move {
            if !self.is_healthy() {
                return false;
            }
            let valid = self
                .store
                .renew_paired_lease(
                    ContinuationLease {
                        owner: self.owner,
                        run_id: &self.run_id,
                        node_id: &self.node_id,
                        run_epoch: self.run_epoch,
                        session_token: self.session_token,
                    },
                    self.lease_seconds,
                    self.fresh,
                )
                .await
                .unwrap_or(false);
            if !valid {
                self.healthy.store(false, Ordering::Release);
                self.cancel.cancel();
            }
            valid && self.is_healthy()
        })
    }
    fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire) && !self.cancel.is_cancelled()
    }
}
