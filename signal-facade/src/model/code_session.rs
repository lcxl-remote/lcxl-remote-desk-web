//! The open-source anonymous code-session identity, stored in the Actix session
//! cookie under a key distinct from the owner `CurrentUser`.
//!
//! A code-session is what an anonymous redeemer of an access-grant code holds: it
//! is **not** the single-account owner. Keeping it under its own session key means
//! it is never resolved by `get_current_user::<CurrentUser>()`, so it cannot pass
//! the owner-only guard and reach the full REST surface; only the
//! capability-scoped `enforce_device_scope` layer recognizes it. The cookie is
//! encrypted (actix-session `Private`), so the client can neither read nor forge
//! the server-minted principal.
//!
//! The cookie deliberately carries **no capability ceiling of its own**: the
//! authoritative ceiling lives in the grant record, which the REST guard looks up
//! fresh on every request. This binds the REST plane to the same TTL / revocation
//! as signaling and removes any chance of a stale cookie-cached ceiling diverging
//! from the live grant.

use serde::{Deserialize, Serialize};

/// Session key under which a redeemed code-session identity is stored. Distinct
/// from the owner user key so the two identities never alias.
pub const CODE_SESSION_KEY: &str = "code_session";

/// A capability-scoped redeemer identity minted at code redeem time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeSessionCookie {
    /// Server-minted, high-entropy principal id. It is the grant principal source
    /// (`code:{id}`) the RequestRemoteAccess authorizer authorizes against, and it
    /// cannot be a `user_id`, so it never aliases the single-account owner.
    pub code_session_id: String,
    /// The grant session minted for this redemption. The REST guard looks it up in
    /// the shared grant store on every request so a code-session's REST access is
    /// bound to the same freshness (TTL / revoke) as its signaling access — a
    /// revoked or expired grant severs the REST plane too, not just signaling. The
    /// live grant record is also the authoritative source of the capability
    /// ceiling.
    pub grant_session_id: String,
    /// The device connection this code authorized access to. Every request from
    /// this session — signaling RequestRemoteAccess and scoped REST alike — may only
    /// address this target.
    pub target_connection_id: String,
}
