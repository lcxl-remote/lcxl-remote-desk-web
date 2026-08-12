//! Access-grant session store: the shared seam behind the unified `AccessGrant`
//! model (device codes and temporary-support codes collapsed into one concept,
//! distinguished only by TTL).
//!
//! A redeemed code produces a **grant session**: a principal-bound, reusable
//! logical-session token (`grant_session_id`) written here. Every RequestRemoteAccess
//! that carries the token (the main control connection, the file-transfer second
//! connection, and any reconnect) does a **lookup-and-stamp** — a pure read of
//! the grant plus a principal / target / generation check — after which the
//! central signaling server stamps the grant's capability ceiling onto the
//! forwarded frame. It is deliberately *not* a one-shot claim: multiple
//! connections reuse the same token, which is what lets the file-transfer
//! connection inherit the same ceiling instead of escaping it.
//!
//! The trait lives here (a web-usable crate) rather than in `manager` so both
//! deployment targets can implement it (rule 13 / rule 22): the manager backs it
//! with Redis (shared across horizontally-scaled instances, rule 19) and the
//! open-source single-instance signal server backs it with the in-process
//! [`InProcessAccessGrantStore`]. Lookups are pure reads, so there is no
//! cross-instance write race to reconcile — Redis TTL / in-process expiry age the
//! session record out on its own.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::model::security_settings::SecuritySettings;

/// Length of a minted `grant_session_id`. The unambiguous 32-char alphabet
/// ([`desk_utils::string::CHARSET_ALPHANUM_UNAMBIGUOUS`]) yields ~5 bits/char, so
/// 32 chars ≈ 160 bits — an internal, non-user-typed high-entropy token.
pub const GRANT_SESSION_ID_LEN: usize = 32;

/// Bounds on a grant session record's TTL (seconds). The persistent authority is
/// the device-code row / support code; the `grant:{grant_session_id}` logical
/// session record only needs to cover the active session plus its auxiliary
/// (file-transfer / reconnect) window, then age out.
pub const MIN_GRANT_SESSION_TTL_SECS: u64 = 30;
pub const MAX_GRANT_SESSION_TTL_SECS: u64 = 60 * 60;
pub const DEFAULT_GRANT_SESSION_TTL_SECS: u64 = 10 * 60;

/// How many mint attempts before giving up on a `grant_session_id` collision.
/// Collisions are astronomically unlikely at ~160 bits; this only bounds a
/// pathological retry loop.
const MAX_MINT_ATTEMPTS: usize = 5;

/// Clamp a requested grant-session TTL into the allowed range.
pub fn clamp_grant_ttl_secs(requested: u64) -> u64 {
    requested.clamp(MIN_GRANT_SESSION_TTL_SECS, MAX_GRANT_SESSION_TTL_SECS)
}

/// The server-resolved principal a grant session is bound to. **Never**
/// browser-reported: on the manager it is the logged-in `user_id` (stringified);
/// on the open-source single-account signal server it is a server-minted,
/// high-entropy `code_session_id` carried in a private (encrypted) cookie. Two
/// grant sessions minted for different principals can never authorize each other
/// — a browser presenting someone else's `grant_session_id` is rejected because
/// the principal on the record does not match its own resolved identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GrantPrincipal(String);

impl GrantPrincipal {
    /// Build a principal from a manager account id.
    pub fn from_user_id(user_id: i32) -> Self {
        Self(format!("user:{user_id}"))
    }

    /// Build a principal from an open-source anonymous code-session id.
    pub fn from_code_session(code_session_id: impl Into<String>) -> Self {
        Self(format!("code:{}", code_session_id.into()))
    }

    /// Borrow the opaque principal string (for logging / equality only).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The record stored under `grant:{grant_session_id}`. Holds only the facts the
/// central lookup-and-stamp needs; the durable authority (device-code row /
/// support code) lives elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantSessionRecord {
    /// Who redeemed the code (server-resolved, see [`GrantPrincipal`]).
    pub principal: GrantPrincipal,
    /// The device (external `device_id` / target connection identity) this grant
    /// authorizes a session against.
    pub target_device: String,
    /// The per-code capability ceiling to stamp on a matching RequestRemoteAccess.
    /// `None` is reserved for owner/org full-control sessions, which do **not**
    /// go through a grant at all; a redeemed code always carries `Some(ceiling)`
    /// (a code with no explicit config is an all-`None` ceiling, i.e. every
    /// dimension prompts, never a wide-open `None`).
    pub access_ceiling: Option<SecuritySettings>,
    /// The code generation this grant was minted at. Regenerating the code bumps
    /// the live generation; a stamp is refused once the grant's generation no
    /// longer matches the code's current one (defense against a superseded code).
    pub generation: i64,
}

impl GrantSessionRecord {
    /// Lookup-and-stamp authorization check. Returns the ceiling to stamp on
    /// success (`Some(ceiling)`), or `None` to reject. Freshness (TTL) is
    /// enforced by the store's [`AccessGrantStore::lookup`]; this compares the
    /// server-resolved caller identity, the requested target, and the code's live
    /// generation. Kept as one shared helper so the manager and open-source paths
    /// authorize identically (rule 22).
    pub fn authorize(
        &self,
        principal: &GrantPrincipal,
        target_device: &str,
        current_generation: i64,
    ) -> Option<Option<SecuritySettings>> {
        let matches = &self.principal == principal
            && self.target_device == target_device
            && self.generation == current_generation;
        matches.then(|| self.access_ceiling.clone())
    }
}

/// A freshly minted grant session and when it expires (Unix seconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedGrant {
    pub grant_session_id: String,
    pub expires_at: i64,
}

/// A grant-store backend failure.
#[derive(Debug)]
pub struct AccessGrantError(pub String);

impl std::fmt::Display for AccessGrantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "access grant error: {}", self.0)
    }
}

impl std::error::Error for AccessGrantError {}

#[async_trait]
pub trait AccessGrantStore: Send + Sync {
    /// Mint a fresh `grant_session_id` bound to `record`, valid for `ttl_secs`
    /// (clamped into range). Retries on the astronomically-rare collision.
    async fn mint(
        &self,
        record: &GrantSessionRecord,
        ttl_secs: u64,
    ) -> Result<MintedGrant, AccessGrantError>;

    /// Resolve a `grant_session_id` to its record, or `None` if unknown /
    /// expired. A **pure read** — it never consumes the token, so the main, file
    /// and reconnect connections can each look it up within the TTL window.
    async fn lookup(
        &self,
        grant_session_id: &str,
    ) -> Result<Option<GrantSessionRecord>, AccessGrantError>;

    /// Drop a grant session immediately (explicit revoke / regen / end-support).
    /// Idempotent — revoking an unknown id is a no-op.
    async fn revoke(&self, grant_session_id: &str) -> Result<(), AccessGrantError>;
}

/// A monotonic-enough Unix-seconds clock, abstracted so tests drive expiry with a
/// logical clock instead of sleeping.
pub trait GrantClock: Send + Sync {
    fn now_unix(&self) -> i64;
}

/// Wall-clock implementation used in production.
#[derive(Default)]
pub struct RealClock;

impl GrantClock for RealClock {
    fn now_unix(&self) -> i64 {
        chrono::Utc::now().timestamp()
    }
}

struct Entry {
    record: GrantSessionRecord,
    expires_at: i64,
}

/// In-process [`AccessGrantStore`] for the single-instance open-source signal
/// server (and unit tests). Mirrors the Redis semantics: mint-if-absent, read
/// without consume, TTL honoured against the injected [`GrantClock`]. Being
/// single-instance, an in-process map is authoritative; a deployment that needs
/// cross-restart survival would swap in a SQLite-backed store behind the same
/// trait.
pub struct InProcessAccessGrantStore {
    grants: Mutex<HashMap<String, Entry>>,
    clock: Box<dyn GrantClock>,
}

impl Default for InProcessAccessGrantStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InProcessAccessGrantStore {
    pub fn new() -> Self {
        Self::with_clock(Box::new(RealClock))
    }

    pub fn with_clock(clock: Box<dyn GrantClock>) -> Self {
        Self {
            grants: Mutex::new(HashMap::new()),
            clock,
        }
    }

    /// Drop every expired entry (lazy GC on each mutating access, plus callable
    /// directly by a maintenance task if one is ever wired up).
    fn gc(&self, grants: &mut HashMap<String, Entry>) {
        let now = self.clock.now_unix();
        grants.retain(|_, e| e.expires_at > now);
    }
}

#[async_trait]
impl AccessGrantStore for InProcessAccessGrantStore {
    async fn mint(
        &self,
        record: &GrantSessionRecord,
        ttl_secs: u64,
    ) -> Result<MintedGrant, AccessGrantError> {
        let ttl_secs = clamp_grant_ttl_secs(ttl_secs);
        let expires_at = self.clock.now_unix() + ttl_secs as i64;
        let mut grants = self.grants.lock().unwrap();
        self.gc(&mut grants);
        for _ in 0..MAX_MINT_ATTEMPTS {
            let grant_session_id = desk_utils::string::generate_device_code(GRANT_SESSION_ID_LEN);
            if !grants.contains_key(&grant_session_id) {
                grants.insert(
                    grant_session_id.clone(),
                    Entry {
                        record: record.clone(),
                        expires_at,
                    },
                );
                return Ok(MintedGrant {
                    grant_session_id,
                    expires_at,
                });
            }
        }
        Err(AccessGrantError(
            "could not mint a unique grant session id after several attempts".to_string(),
        ))
    }

    async fn lookup(
        &self,
        grant_session_id: &str,
    ) -> Result<Option<GrantSessionRecord>, AccessGrantError> {
        let now = self.clock.now_unix();
        let mut grants = self.grants.lock().unwrap();
        match grants.get(grant_session_id) {
            Some(e) if e.expires_at > now => Ok(Some(e.record.clone())),
            Some(_) => {
                // Expired: drop it and report unknown, mirroring Redis TTL GC.
                grants.remove(grant_session_id);
                Ok(None)
            }
            None => Ok(None),
        }
    }

    async fn revoke(&self, grant_session_id: &str) -> Result<(), AccessGrantError> {
        self.grants.lock().unwrap().remove(grant_session_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    /// A logical clock a test advances explicitly, so TTL expiry needs no sleep.
    #[derive(Default)]
    struct ManualClock(AtomicI64);

    impl ManualClock {
        fn advance(&self, secs: i64) {
            self.0.fetch_add(secs, Ordering::SeqCst);
        }
    }

    impl GrantClock for ManualClock {
        fn now_unix(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn ceiling() -> SecuritySettings {
        // A code that permits terminal but leaves the rest unset (prompt).
        SecuritySettings {
            allow_terminal: Some(true),
            ..SecuritySettings {
                allow_remote_control: None,
                allow_clipboard_sync: None,
                allow_private_screen: None,
                allow_whiteboard: None,
                allow_terminal: None,
                allow_file_browse: None,
                allow_file_delete: None,
                allow_file_transfer: None,
                approval_timeout: None,
            }
        }
    }

    fn record() -> GrantSessionRecord {
        GrantSessionRecord {
            principal: GrantPrincipal::from_user_id(7),
            target_device: "device-abc".to_string(),
            access_ceiling: Some(ceiling()),
            generation: 3,
        }
    }

    #[tokio::test]
    async fn mint_then_lookup_returns_record() {
        let store = InProcessAccessGrantStore::new();
        let minted = store.mint(&record(), 300).await.unwrap();
        assert_eq!(minted.grant_session_id.len(), GRANT_SESSION_ID_LEN);
        assert_eq!(
            store.lookup(&minted.grant_session_id).await.unwrap(),
            Some(record())
        );
    }

    #[tokio::test]
    async fn lookup_does_not_consume_the_token() {
        let store = InProcessAccessGrantStore::new();
        let minted = store.mint(&record(), 300).await.unwrap();
        // The main, file-transfer and reconnect connections all look it up.
        assert!(
            store
                .lookup(&minted.grant_session_id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .lookup(&minted.grant_session_id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .lookup(&minted.grant_session_id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn unknown_token_is_none() {
        let store = InProcessAccessGrantStore::new();
        assert_eq!(
            store
                .lookup("NOSUCHGRANTSESSION00000000000000")
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn expired_token_is_none() {
        let clock = Arc::new(ManualClock::default());
        let store = InProcessAccessGrantStore::with_clock(Box::new(ClockHandle(clock.clone())));
        let minted = store.mint(&record(), 60).await.unwrap();
        clock.advance(61);
        assert_eq!(store.lookup(&minted.grant_session_id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn revoke_drops_the_token() {
        let store = InProcessAccessGrantStore::new();
        let minted = store.mint(&record(), 300).await.unwrap();
        store.revoke(&minted.grant_session_id).await.unwrap();
        assert_eq!(store.lookup(&minted.grant_session_id).await.unwrap(), None);
        // Revoking an unknown id is a no-op.
        store
            .revoke("NOSUCHGRANTSESSION00000000000000")
            .await
            .unwrap();
    }

    #[test]
    fn ttl_is_clamped_into_range() {
        assert_eq!(clamp_grant_ttl_secs(1), MIN_GRANT_SESSION_TTL_SECS);
        assert_eq!(clamp_grant_ttl_secs(u64::MAX), MAX_GRANT_SESSION_TTL_SECS);
        assert_eq!(clamp_grant_ttl_secs(120), 120);
    }

    #[test]
    fn authorize_stamps_ceiling_for_matching_principal_target_generation() {
        let r = record();
        // Correct principal + target + generation ⇒ stamp the ceiling.
        assert_eq!(
            r.authorize(&GrantPrincipal::from_user_id(7), "device-abc", 3),
            Some(Some(ceiling()))
        );
    }

    #[test]
    fn authorize_rejects_wrong_principal() {
        let r = record();
        // A browser presenting this grant_session_id but resolved to a different
        // identity is rejected (defense against token borrowing / fanout).
        assert_eq!(
            r.authorize(&GrantPrincipal::from_user_id(8), "device-abc", 3),
            None
        );
    }

    #[test]
    fn authorize_rejects_wrong_target() {
        let r = record();
        assert_eq!(
            r.authorize(&GrantPrincipal::from_user_id(7), "device-xyz", 3),
            None
        );
    }

    #[test]
    fn authorize_rejects_superseded_generation() {
        let r = record();
        // The code was regenerated (live generation is now 4) ⇒ the old grant is
        // no longer honored.
        assert_eq!(
            r.authorize(&GrantPrincipal::from_user_id(7), "device-abc", 4),
            None
        );
    }

    #[test]
    fn manager_and_code_session_principals_are_distinct() {
        // A `user:7` grant and a `code:7` grant must never collide, so the
        // stringified namespaces cannot alias.
        assert_ne!(
            GrantPrincipal::from_user_id(7),
            GrantPrincipal::from_code_session("7")
        );
    }

    /// Adapter so a test can share one `ManualClock` between the driver and the
    /// store (the store takes ownership of its `Box<dyn GrantClock>`).
    struct ClockHandle(Arc<ManualClock>);
    impl GrantClock for ClockHandle {
        fn now_unix(&self) -> i64 {
            self.0.now_unix()
        }
    }
}
