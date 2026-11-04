//! Single-instance coordination between identity registration and device-code edits.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[derive(Default)]
struct Slot {
    lock: Arc<AsyncMutex<()>>,
    users: usize,
}

#[derive(Default)]
pub(crate) struct DeviceCodeLifecycle {
    slots: Mutex<HashMap<String, Slot>>,
}

pub(crate) struct DeviceCodeGuard {
    owner: Arc<DeviceCodeLifecycle>,
    client_id: String,
    guard: Option<OwnedMutexGuard<()>>,
}

impl DeviceCodeLifecycle {
    pub(crate) async fn lock(self: &Arc<Self>, client_id: &str) -> DeviceCodeGuard {
        let lock = {
            let mut slots = self.slots.lock().unwrap();
            let slot = slots.entry(client_id.to_owned()).or_default();
            slot.users += 1;
            slot.lock.clone()
        };
        // The lease exists before awaiting, so cancelling a waiter also removes
        // its slot reference. The registry mutex never spans an await.
        let mut lease = DeviceCodeGuard {
            owner: self.clone(),
            client_id: client_id.to_owned(),
            guard: None,
        };
        lease.guard = Some(lock.lock_owned().await);
        lease
    }

    pub(crate) async fn prepare_registration<C: crate::config::ConfigConnection>(
        self: &Arc<Self>,
        db: &C,
        client_id: &str,
    ) -> Result<(DeviceCodeGuard, String), sea_orm::DbErr> {
        let guard = self.lock(client_id).await;
        let code = crate::device_code_store::get_or_create(db, client_id).await?;
        Ok((guard, code))
    }
}

impl Drop for DeviceCodeGuard {
    fn drop(&mut self) {
        // Unlock before removing the last lease. Waiting operations already
        // own a registry reference and therefore keep the same slot alive.
        drop(self.guard.take());
        let mut slots = self.owner.slots.lock().unwrap();
        let slot = slots.get_mut(&self.client_id).expect("live identity lease");
        slot.users -= 1;
        if slot.users == 0 {
            slots.remove(&self.client_id);
        }
    }
}

pub(crate) fn global() -> &'static Arc<DeviceCodeLifecycle> {
    static LIFECYCLE: OnceLock<Arc<DeviceCodeLifecycle>> = OnceLock::new();
    LIFECYCLE.get_or_init(Default::default)
}

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod performance;
#[cfg(test)]
mod tests;
