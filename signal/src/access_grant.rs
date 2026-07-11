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
