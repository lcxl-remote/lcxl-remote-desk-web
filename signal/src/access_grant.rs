//! Process-global access-grant store for the single-account open-source signal.
//!
//! The open-source signal server is single-instance, so an in-process store is
//! authoritative — the multi-instance shared-storage requirement is a manager
//! concern; this server never runs multi-instance. The unified redeem endpoint
//! mints a grant here, and the RequestRemote authorizer looks it up when stamping
//! a code-session's capability ceiling. Both share this one instance so a minted
//! grant is visible to the stamp path.

use desk_signal_facade::grant::InProcessAccessGrantStore;
use std::sync::{Arc, OnceLock};

/// The single shared grant store for this process. Minting (redeem) and
/// lookup-and-stamp (RequestRemote authorizer) must resolve to the same instance.
pub fn global_access_grant_store() -> Arc<InProcessAccessGrantStore> {
    static STORE: OnceLock<Arc<InProcessAccessGrantStore>> = OnceLock::new();
    STORE
        .get_or_init(|| Arc::new(InProcessAccessGrantStore::new()))
        .clone()
}

/// The live code generation for the device registered under `client_id`, or `None`
/// if no such device code exists (or the signal DB is not initialized in this
/// process, e.g. a pure desk-server mode). Mirrors the RequestRemote authorizer's
/// [`crate::request_remote_authorizer::DbDeviceGenerationLookup`] so a code
/// session's REST access is bound to the same generation freshness as its
/// signaling: a regenerated (bumped) or deleted device code makes the check fail,
/// invalidating a grant minted at the superseded generation.
pub async fn current_device_generation(client_id: &str) -> Option<i64> {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let db = crate::db::try_get_db()?;
    crate::entity::device_code::Entity::find()
        .filter(crate::entity::device_code::Column::ClientId.eq(client_id))
        .one(db)
        .await
        .ok()
        .flatten()
        .map(|row| row.generation as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_store_is_a_single_shared_instance() {
        let a = global_access_grant_store();
        let b = global_access_grant_store();
        assert!(Arc::ptr_eq(&a, &b));
    }
}
