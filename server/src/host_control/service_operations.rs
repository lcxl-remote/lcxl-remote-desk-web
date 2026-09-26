//! Bounded, machine-local installation receipts; never replay an OS mutation.
use super::{ServiceOpKind, UpstreamSessionId};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};
use utoipa::ToSchema;

const DISPATCH_TIMEOUT: Duration = Duration::from_secs(30);
const RESULT_TIMEOUT: Duration = Duration::from_secs(600);
const RETENTION: Duration = Duration::from_secs(3600);
const CAPACITY: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServiceOperationState {
    Queued,
    Running,
    Succeeded,
    Cancelled,
    Failed,
    Unknown,
    /// The platform launcher accepted the request but cannot report completion.
    Submitted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServiceOperationError {
    AuthorizationNotGranted,
    MissingPkexec,
    LaunchFailed,
    InstallerFailed,
    Busy,
    Unsupported,
    ConnectionLost,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ServiceOperationStatus {
    pub operation_id: String,
    pub op: ServiceOpKind,
    pub state: ServiceOperationState,
    pub error: Option<ServiceOperationError>,
    pub exit_code: Option<i32>,
}

struct Record {
    status: ServiceOperationStatus,
    created: Instant,
    owner: Option<UpstreamSessionId>,
}

#[derive(Default)]
pub struct ServiceOperations(Mutex<HashMap<String, Record>>);

impl ServiceOperations {
    pub fn begin(&self, op: ServiceOpKind) -> Result<ServiceOperationStatus, &'static str> {
        self.begin_at(op, Instant::now())
    }

    fn begin_at(
        &self,
        op: ServiceOpKind,
        now: Instant,
    ) -> Result<ServiceOperationStatus, &'static str> {
        let mut records = self.0.lock().unwrap();
        records.retain(|_, record| now.duration_since(record.created) < RETENTION);
        for record in records.values_mut() {
            expire(record, now);
        }
        if records.values().any(|record| {
            matches!(
                record.status.state,
                ServiceOperationState::Queued | ServiceOperationState::Running
            )
        }) {
            return Err("A service operation is already in progress");
        }
        if records.len() >= CAPACITY {
            let oldest = records
                .iter()
                .min_by_key(|(_, r)| r.created)
                .map(|(id, _)| id.clone())
                .unwrap();
            records.remove(&oldest);
        }
        let status = ServiceOperationStatus {
            operation_id: uuid::Uuid::new_v4().to_string(),
            op,
            state: ServiceOperationState::Queued,
            error: None,
            exit_code: None,
        };
        records.insert(
            status.operation_id.clone(),
            Record {
                status: status.clone(),
                created: now,
                owner: None,
            },
        );
        Ok(status)
    }

    /// Called immediately before sending to one authenticated Tauri connection.
    pub fn claim(&self, id: &str, owner: UpstreamSessionId) -> bool {
        let mut records = self.0.lock().unwrap();
        let Some(record) = records.get_mut(id) else {
            return false;
        };
        expire(record, Instant::now());
        if record.status.state != ServiceOperationState::Queued || record.owner.is_some() {
            return false;
        }
        record.owner = Some(owner);
        record.status.state = ServiceOperationState::Running;
        true
    }

    pub fn get(&self, id: &str) -> Option<ServiceOperationStatus> {
        let mut records = self.0.lock().unwrap();
        let record = records.get_mut(id)?;
        if record.created.elapsed() >= RETENTION {
            return None;
        }
        expire(record, Instant::now());
        Some(record.status.clone())
    }

    pub fn complete(&self, owner: UpstreamSessionId, result: ServiceOperationStatus) -> bool {
        if matches!(
            result.state,
            ServiceOperationState::Queued | ServiceOperationState::Running
        ) {
            return false;
        }
        let mut records = self.0.lock().unwrap();
        let Some(record) = records.get_mut(&result.operation_id) else {
            return false;
        };
        if record.owner != Some(owner)
            || record.status.op != result.op
            || record.created.elapsed() >= RETENTION
            || !matches!(
                record.status.state,
                ServiceOperationState::Running | ServiceOperationState::Unknown
            )
        {
            return false;
        }
        record.status = result;
        true
    }

    pub fn dispatch_failed(&self, id: &str) {
        if let Some(record) = self.0.lock().unwrap().get_mut(id)
            && record.owner.is_none()
        {
            record.status.state = ServiceOperationState::Failed;
            record.status.error = Some(ServiceOperationError::ConnectionLost);
        }
    }

    pub fn disconnected(&self, owner: UpstreamSessionId) {
        for record in self.0.lock().unwrap().values_mut() {
            if record.owner == Some(owner) && record.status.state == ServiceOperationState::Running
            {
                record.status.state = ServiceOperationState::Unknown;
                record.status.error = Some(ServiceOperationError::ConnectionLost);
            }
        }
    }
}

fn expire(record: &mut Record, now: Instant) {
    let age = now.duration_since(record.created);
    let timed_out = match record.status.state {
        ServiceOperationState::Queued => age >= DISPATCH_TIMEOUT,
        ServiceOperationState::Running => age >= RESULT_TIMEOUT,
        _ => false,
    };
    if timed_out {
        record.status.state = ServiceOperationState::Unknown;
        record.status.error = Some(ServiceOperationError::TimedOut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_windows_claim_an_operation_exactly_once() {
        let store = std::sync::Arc::new(ServiceOperations::default());
        let status = store.begin(ServiceOpKind::Install).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|owner| {
                let store = store.clone();
                let barrier = barrier.clone();
                let id = status.operation_id.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.claim(&id, owner)
                })
            })
            .collect();
        let winners = threads
            .into_iter()
            .filter_map(|t| t.join().ok())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
    }

    #[test]
    fn running_timeout_is_unknown_and_late_receipt_is_still_correlated() {
        let store = ServiceOperations::default();
        let mut status = store.begin(ServiceOpKind::Install).unwrap();
        assert!(store.claim(&status.operation_id, 1));
        store
            .0
            .lock()
            .unwrap()
            .get_mut(&status.operation_id)
            .unwrap()
            .created = Instant::now() - RESULT_TIMEOUT;
        assert_eq!(
            store.get(&status.operation_id).unwrap().state,
            ServiceOperationState::Unknown
        );
        assert!(!store.claim(&status.operation_id, 2));
        status.state = ServiceOperationState::Succeeded;
        assert!(store.complete(1, status.clone()));
        assert_eq!(store.get(&status.operation_id), Some(status));
    }

    #[test]
    fn one_recipient_and_matching_receipt_only() {
        let store = ServiceOperations::default();
        let mut status = store.begin(ServiceOpKind::Install).unwrap();
        assert!(store.begin(ServiceOpKind::Uninstall).is_err());
        assert!(!store.complete(1, status.clone()));
        assert!(store.claim(&status.operation_id, 1));
        assert!(!store.claim(&status.operation_id, 2));
        status.state = ServiceOperationState::Succeeded;
        assert!(!store.complete(2, status.clone()));
        let mut wrong = status.clone();
        wrong.op = ServiceOpKind::Uninstall;
        assert!(!store.complete(1, wrong));
        assert!(store.complete(1, status.clone()));
        assert!(!store.complete(1, status));
        assert!(store.begin(ServiceOpKind::Uninstall).is_ok());
    }

    #[test]
    fn disconnect_never_replays_and_late_result_cannot_finish_new_operation() {
        let store = ServiceOperations::default();
        let mut old = store.begin(ServiceOpKind::Install).unwrap();
        assert!(store.claim(&old.operation_id, 7));
        store.disconnected(7);
        assert_eq!(
            store.get(&old.operation_id).unwrap().state,
            ServiceOperationState::Unknown
        );
        assert!(!store.claim(&old.operation_id, 8));
        let new = store.begin(ServiceOpKind::Install).unwrap();
        old.state = ServiceOperationState::Succeeded;
        assert!(!store.complete(8, old.clone()));
        assert!(store.complete(7, old));
        assert_eq!(
            store.get(&new.operation_id).unwrap().state,
            ServiceOperationState::Queued
        );
    }

    #[test]
    fn expired_requests_are_not_dispatched_and_history_is_bounded() {
        let store = ServiceOperations::default();
        let now = Instant::now();
        let expired = store
            .begin_at(ServiceOpKind::Install, now - DISPATCH_TIMEOUT)
            .unwrap();
        assert!(!store.claim(&expired.operation_id, 1));
        assert_eq!(
            store.get(&expired.operation_id).unwrap().error,
            Some(ServiceOperationError::TimedOut)
        );
        for _ in 0..CAPACITY + 3 {
            let item = store.begin(ServiceOpKind::Install).unwrap();
            store.dispatch_failed(&item.operation_id);
        }
        assert_eq!(store.0.lock().unwrap().len(), CAPACITY);
        assert!(store.get(&expired.operation_id).is_none());
    }
}
