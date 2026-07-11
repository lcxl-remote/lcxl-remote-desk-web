//! The open-source anonymous code-session identity, stored in the Actix session
//! cookie under a key distinct from the owner `CurrentUser`.
//!
//! A code-session is what an anonymous redeemer of an access-grant code holds: it
//! is **not** the single-account owner. Keeping it under its own session key means
//! it is never resolved by `get_current_user::<CurrentUser>()`, so it cannot pass
//! the owner-only `reject_anonymous_users` guard and reach the full REST surface;
//! only the capability-scoped `enforce_device_scope` layer recognizes it. The
//! cookie is encrypted (actix-session `Private`), so the client can neither read
//! nor forge the server-minted principal.

use crate::model::security_settings::SecuritySettings;
use serde::{Deserialize, Serialize};

/// Session key under which a redeemed code-session identity is stored. Distinct
/// from the owner user key so the two identities never alias.
pub const CODE_SESSION_KEY: &str = "code_session";

/// A capability-scoped redeemer identity minted at code redeem time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeSessionCookie {
    /// Server-minted, high-entropy principal id. It is the grant principal source
    /// (`code:{id}`) the RequestRemote authorizer authorizes against, and it
    /// cannot be a `user_id`, so it never aliases the single-account owner.
    pub code_session_id: String,
    /// The device connection this code authorized access to. Every request from
    /// this session — signaling RequestRemote and scoped REST alike — may only
    /// address this target.
    pub target_connection_id: String,
    /// The redeemed code's capability ceiling. On the REST plane it is met with
    /// the host global settings to gate the capability-carrier endpoints; on the
    /// signaling plane the same ceiling is carried by the grant record.
    pub access_ceiling: SecuritySettings,
}
