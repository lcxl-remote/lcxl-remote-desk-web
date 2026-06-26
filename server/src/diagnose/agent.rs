//! In-memory session bookkeeping and the read+exec tool registry helper.
//!
//! - [`InMemorySessionSeam`]: keeps agent sessions in process memory with a
//!   per-conversation atomic claim (one daemon process owns its sessions),
//!   implementing the shared [`desk_diagnose_core::seam::SessionSeam`].
//! - [`agent_tool_registry`]: the read-only tool set plus the mutating
//!   `exec_command` tool, each mapping onto one [`desk_diagnose_core`] tool.

use std::collections::HashMap;
use std::sync::Arc;

use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::registry::RegisteredTool;
use desk_diagnose_core::seam::{ClaimError, ClaimTurnParams, SessionSeam};
use desk_diagnose_core::session::{PersistedAgentSession, RecoveryVerdict};

/// The read-only tools plus the mutating exec tool.
pub fn agent_tool_registry() -> Vec<RegisteredTool> {
    let mut reg = desk_diagnose_core::read_tools::read_tool_registry();
    reg.extend(desk_diagnose_core::exec_tools::exec_tool_registry());
    reg
}

// ============================ Session seam (in-memory) ============================

/// Upper bound on concurrently cached conversations. When a *new* conversation
/// would push past this, the claim first TTL-sweeps, then evicts the
/// least-recently-accessed *settled* session; if every cached session is still
/// active, the new claim is refused (see [`InMemorySessionSeam::claim_turn`]).
const MAX_DIRECT_CONVERSATIONS: usize = 128;

/// Idle lifetime of a *settled* cached session. A settled session untouched for
/// this long is dropped on the next claim sweep. An active session is never
/// TTL-evicted regardless of age — a long approval/tool wait must not lose its
/// history (liveness, not last-access, gates eviction eligibility).
const SESSION_IDLE_TTL_MS: u64 = 30 * 60 * 1000;

/// Lease lifetime of an *active* turn. The owning loop renews it (on each save and
/// via the background heartbeat); if it lapses, the owner is presumed gone and the
/// next claim recovers the orphan in place. Kept well above the heartbeat interval
/// so a healthy long-running turn is never falsely reclaimed.
const LEASE_TTL_MS: u64 = 90 * 1000;

/// A coarse monotonic clock (milliseconds) for the in-memory cache's TTL / LRU
/// bookkeeping. Injected so tests drive eviction deterministically without
/// sleeping; production uses [`SystemClock`]. Kept distinct from the RFC3339
/// `now` strings that stamp domain `updated_at`: this is pure cache plumbing.
trait CacheClock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// Production clock: milliseconds elapsed on a monotonic [`std::time::Instant`]
/// since the seam was built (never runs backwards, unlike wall-clock time).
struct SystemClock {
    base: std::time::Instant,
}

impl SystemClock {
    fn new() -> Self {
        Self {
            base: std::time::Instant::now(),
        }
    }
}

impl CacheClock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.base.elapsed().as_millis() as u64
    }
}

/// A cached session plus its cache-management bookkeeping.
///
/// `last_access_ms` is deliberately separate from the session's domain
/// `updated_at` ("liveness"): `updated_at` records when the *session state* last
/// changed (CAS / audit meaning), whereas `last_access_ms` records when the
/// *cache entry* was last touched (claim or save) purely for LRU ordering and TTL
/// expiry. Keeping the two apart means cache policy never mutates persisted domain
/// state, and eviction *eligibility* keys off the turn machine's liveness
/// (`turn_state.is_active`) rather than off a timestamp — so a session that is
/// active but was last accessed long ago is never evicted as "stale".
struct CacheEntry {
    session: PersistedAgentSession,
    last_access_ms: u64,
    /// Lease expiry (monotonic ms) for an active turn — a liveness signal kept
    /// separate from `last_access_ms`: the background heartbeat extends this
    /// without counting as a user access, so a long-running turn is not mistaken
    /// for "recently accessed" (which would distort LRU). Meaningful only while the
    /// turn is active; an expired active entry is an orphan the next claim recovers.
    lease_deadline_ms: u64,
}

/// Keeps agent sessions in process memory, keyed by conversation id. One daemon
/// process owns its sessions, so a single async mutex makes the whole claim —
/// TTL sweep, capacity eviction, and the turn claim — one atomic critical section.
pub struct InMemorySessionSeam {
    sessions: tokio::sync::Mutex<HashMap<String, CacheEntry>>,
    clock: Arc<dyn CacheClock>,
}

impl Default for InMemorySessionSeam {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemorySessionSeam {
    pub fn new() -> Self {
        Self::with_clock(Arc::new(SystemClock::new()))
    }

    fn with_clock(clock: Arc<dyn CacheClock>) -> Self {
        Self {
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            clock,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl SessionSeam for InMemorySessionSeam {
    async fn claim_turn(
        &self,
        params: ClaimTurnParams,
    ) -> Result<PersistedAgentSession, ClaimError> {
        let now_ms = self.clock.now_ms();
        let mut map = self.sessions.lock().await;

        // Lease recovery — an active entry whose lease has lapsed is an orphan (its
        // owning loop crashed or was aborted). Settle it in place so it no longer
        // blocks and a same-conversation follow-up can continue its history. The
        // in-memory runtime has no durable work item, so the verdict is
        // conservatively `InterruptedUnknown` (no fabricated execution identity);
        // `begin_turn` below rotates the lease token, fencing out the dead owner's
        // late saves.
        for entry in map.values_mut() {
            if entry.session.turn_state.is_active() && now_ms > entry.lease_deadline_ms {
                entry
                    .session
                    .recover_session(RecoveryVerdict::InterruptedUnknown, params.now.clone());
            }
        }

        // Rule 1 — TTL sweep: drop settled entries idle past the TTL. Active
        // entries are retained regardless of age (liveness gates this, not
        // last-access), so a long tool/approval wait never loses its history.
        map.retain(|_, e| {
            e.session.turn_state.is_active()
                || now_ms.saturating_sub(e.last_access_ms) < SESSION_IDLE_TTL_MS
        });

        // Only admitting a *new* conversation can grow the map; re-claiming an
        // existing one replaces in place and never triggers capacity eviction.
        let is_existing = map.contains_key(&params.conversation_id);
        if !is_existing {
            // Rules 2 & 3 — under capacity pressure, evict the least-recently-
            // accessed *settled* session; if none is settled (all active), refuse
            // the new claim. Busy is the loop's "try again shortly" signal: a
            // later claim, once the TTL sweep or a finishing turn frees a slot,
            // succeeds.
            while map.len() >= MAX_DIRECT_CONVERSATIONS {
                let lru_settled = map
                    .iter()
                    .filter(|(_, e)| e.session.turn_state.is_settled())
                    .min_by_key(|(_, e)| e.last_access_ms)
                    .map(|(k, _)| k.clone());
                match lru_settled {
                    Some(key) => {
                        map.remove(&key);
                    }
                    None => return Err(ClaimError::Busy),
                }
            }
        }

        let mut session = match map.get(&params.conversation_id) {
            Some(existing) => {
                existing
                    .session
                    .check_subject(&params.actor_id, &params.device_id)
                    .map_err(ClaimError::Subject)?;
                existing.session.clone()
            }
            None => PersistedAgentSession::new(
                params.conversation_id.clone(),
                params.actor_id.clone(),
                params.device_id.clone(),
                params.policy_revision,
                params.current_pdp_scope.clone(),
                params.now.clone(),
            ),
        };
        session
            .begin_turn(
                params.turn_id,
                params.request_id,
                params.connection_id,
                params.policy_revision,
                params.current_pdp_scope,
                params.now,
            )
            .map_err(|_| ClaimError::Busy)?;
        map.insert(
            session.conversation_id.clone(),
            CacheEntry {
                session: session.clone(),
                last_access_ms: now_ms,
                lease_deadline_ms: now_ms + LEASE_TTL_MS,
            },
        );
        Ok(session)
    }

    async fn save(&self, session: &mut PersistedAgentSession) -> Result<(), AgentError> {
        let now_ms = self.clock.now_ms();
        let mut map = self.sessions.lock().await;
        let entry = map.get_mut(&session.conversation_id).ok_or_else(|| {
            lease_lost(format!(
                "session {} vanished from the cache during save",
                session.conversation_id
            ))
        })?;
        // Fencing CAS: the held token must still be the current owner's (else the
        // lease was taken over) and the held version the latest (else a stale
        // snapshot). Either mismatch fails the save rather than overwriting.
        if entry.session.lease_token != session.lease_token {
            return Err(lease_lost(format!(
                "session {} lease was taken over (token mismatch)",
                session.conversation_id
            )));
        }
        if entry.session.version != session.version {
            return Err(lease_lost(format!(
                "session {} save lost the version CAS (concurrent writer)",
                session.conversation_id
            )));
        }
        session.version += 1;
        entry.session = session.clone();
        entry.last_access_ms = now_ms;
        // A save by the active owner is also proof of life: renew the lease.
        if session.turn_state.is_active() {
            entry.lease_deadline_ms = now_ms + LEASE_TTL_MS;
        }
        Ok(())
    }

    async fn heartbeat(
        &self,
        conversation_id: &str,
        lease_token: u64,
        _now: &str,
    ) -> Result<(), AgentError> {
        let now_ms = self.clock.now_ms();
        let mut map = self.sessions.lock().await;
        let entry = map
            .get_mut(conversation_id)
            .ok_or_else(|| lease_lost(format!("session {conversation_id} is gone")))?;
        // Renew only while the turn is active and the token still ours; never touch
        // the version (so a concurrent save is unaffected) or `last_access_ms` (so
        // renewal is not mistaken for a user access in the LRU ordering).
        if !entry.session.turn_state.is_active() {
            return Err(lease_lost(format!(
                "session {conversation_id} has settled; heartbeat stops"
            )));
        }
        if entry.session.lease_token != lease_token {
            return Err(lease_lost(format!(
                "session {conversation_id} lease was taken over (token mismatch)"
            )));
        }
        entry.lease_deadline_ms = now_ms + LEASE_TTL_MS;
        Ok(())
    }
}

/// A lease-lost / fencing error — surfaced when a revived old owner tries to write
/// after its lease was taken over. Server-internal (not safe for the model prompt);
/// the loop ends the turn rather than retrying.
fn lease_lost(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::SessionUnavailable,
        message: message.into(),
        retryable: false,
        safe_for_model: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{AgentScope, Capability, ExecutionMode};
    use desk_diagnose_core::chat::{ChatMessage, ChatRole};
    use desk_diagnose_core::session::TurnState;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Current time as an RFC3339 string.
    fn now_rfc3339() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    /// The combined registry exposes the read tools plus the exec tool.
    #[test]
    fn agent_registry_includes_exec_tool() {
        let names: Vec<_> = agent_tool_registry()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.contains(&"exec_command".to_string()));
        assert!(names.contains(&"read_system_info".to_string()));
    }

    // ---------------------------- Session cache lifecycle ----------------------------

    /// A clock the test drives by hand, so TTL / LRU eviction is deterministic
    /// without ever sleeping.
    struct FakeClock(AtomicU64);
    impl CacheClock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }
    impl FakeClock {
        fn new() -> Self {
            Self(AtomicU64::new(0))
        }
        fn set(&self, ms: u64) {
            self.0.store(ms, Ordering::SeqCst);
        }
    }

    fn claim_params(conv: &str) -> ClaimTurnParams {
        ClaimTurnParams {
            conversation_id: conv.into(),
            actor_id: "actor".into(),
            device_id: "device".into(),
            policy_revision: 1,
            current_pdp_scope: AgentScope {
                granted: vec![Capability::SystemInfo],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            turn_id: format!("turn-{conv}"),
            request_id: Some(format!("req-{conv}")),
            connection_id: None,
            now: now_rfc3339(),
        }
    }

    impl InMemorySessionSeam {
        async fn cache_len(&self) -> usize {
            self.sessions.lock().await.len()
        }
        async fn cache_has(&self, conv: &str) -> bool {
            self.sessions.lock().await.contains_key(conv)
        }
    }

    /// Claim a conversation and immediately settle it (Idle), so it sits in the
    /// cache as an evictable, history-bearing entry.
    async fn claim_settled(seam: &InMemorySessionSeam, conv: &str) {
        let mut s = seam.claim_turn(claim_params(conv)).await.expect("claim");
        s.finish_turn(TurnState::Idle, now_rfc3339());
        seam.save(&mut s).await.expect("save");
    }

    /// At capacity, admitting a new conversation evicts the least-recently-
    /// accessed *settled* session (the first one claimed), keeping the rest.
    #[tokio::test]
    async fn capacity_pressure_evicts_lru_settled() {
        let clock = Arc::new(FakeClock::new());
        let seam = InMemorySessionSeam::with_clock(clock.clone());
        // Fill to capacity with settled sessions, each at a distinct access time
        // so conv-0 is unambiguously the least-recently-accessed.
        for i in 0..MAX_DIRECT_CONVERSATIONS {
            clock.set(i as u64);
            claim_settled(&seam, &format!("conv-{i}")).await;
        }
        assert_eq!(seam.cache_len().await, MAX_DIRECT_CONVERSATIONS);

        // A new conversation evicts the oldest settled (conv-0) and stays bounded.
        clock.set(1_000);
        seam.claim_turn(claim_params("conv-new"))
            .await
            .expect("admit");
        assert_eq!(seam.cache_len().await, MAX_DIRECT_CONVERSATIONS);
        assert!(!seam.cache_has("conv-0").await, "LRU settled evicted");
        assert!(
            seam.cache_has("conv-new").await,
            "new conversation admitted"
        );
        assert!(
            seam.cache_has(&format!("conv-{}", MAX_DIRECT_CONVERSATIONS - 1))
                .await,
            "most-recent settled retained"
        );
    }

    /// Eviction eligibility is liveness, not age: an active session that is the
    /// globally least-recently-accessed entry survives capacity pressure, while
    /// the oldest *settled* session is evicted instead.
    #[tokio::test]
    async fn active_session_survives_capacity_over_older_settled() {
        let clock = Arc::new(FakeClock::new());
        let seam = InMemorySessionSeam::with_clock(clock.clone());
        // conv-active is the globally oldest entry, but stays Running (never saved
        // to a settled state).
        clock.set(0);
        seam.claim_turn(claim_params("conv-active"))
            .await
            .expect("claim active");
        // Fill the remaining slots with settled sessions at later access times.
        for i in 1..MAX_DIRECT_CONVERSATIONS {
            clock.set(i as u64);
            claim_settled(&seam, &format!("conv-{i}")).await;
        }
        assert_eq!(seam.cache_len().await, MAX_DIRECT_CONVERSATIONS);

        clock.set(1_000);
        seam.claim_turn(claim_params("conv-new"))
            .await
            .expect("admit");
        assert!(
            seam.cache_has("conv-active").await,
            "the active session survives despite being the oldest"
        );
        assert!(
            !seam.cache_has("conv-1").await,
            "the oldest settled session is evicted instead"
        );
    }

    /// When every cached session is active, a new conversation cannot be admitted
    /// and the claim is refused with Busy (the loop's "try again shortly" signal).
    #[tokio::test]
    async fn all_active_at_capacity_rejects_new_conversation() {
        let clock = Arc::new(FakeClock::new());
        let seam = InMemorySessionSeam::with_clock(clock.clone());
        for i in 0..MAX_DIRECT_CONVERSATIONS {
            clock.set(i as u64);
            // Claim without settling — every entry stays Running (active).
            seam.claim_turn(claim_params(&format!("conv-{i}")))
                .await
                .expect("claim");
        }
        clock.set(1_000);
        let err = seam.claim_turn(claim_params("conv-new")).await.unwrap_err();
        assert!(
            matches!(err, ClaimError::Busy),
            "all-active capacity is Busy"
        );
        assert!(!seam.cache_has("conv-new").await);
    }

    /// Re-claiming an *existing* (settled) conversation bypasses capacity
    /// eviction even when every other session is active — a follow-up question
    /// must always continue its own conversation.
    #[tokio::test]
    async fn existing_conversation_reclaims_at_capacity() {
        let clock = Arc::new(FakeClock::new());
        let seam = InMemorySessionSeam::with_clock(clock.clone());
        clock.set(0);
        claim_settled(&seam, "conv-keep").await;
        // Fill the rest with active sessions, hitting capacity exactly.
        for i in 1..MAX_DIRECT_CONVERSATIONS {
            clock.set(i as u64);
            seam.claim_turn(claim_params(&format!("conv-{i}")))
                .await
                .expect("claim");
        }
        assert_eq!(seam.cache_len().await, MAX_DIRECT_CONVERSATIONS);

        // Re-claiming the existing settled conversation succeeds without eviction.
        clock.set(1_000);
        seam.claim_turn(claim_params("conv-keep"))
            .await
            .expect("reclaim existing");
        assert_eq!(seam.cache_len().await, MAX_DIRECT_CONVERSATIONS);
        assert!(seam.cache_has("conv-keep").await);
    }

    /// A settled session idle past the TTL is swept on the next claim; an active
    /// session whose lease is kept alive (heartbeat) survives the same sweep —
    /// liveness, not age, is what protects an active entry.
    #[tokio::test]
    async fn ttl_sweep_drops_idle_settled_keeps_active() {
        let clock = Arc::new(FakeClock::new());
        let seam = InMemorySessionSeam::with_clock(clock.clone());
        clock.set(0);
        claim_settled(&seam, "conv-stale").await; // settled at t=0
        let busy = seam
            .claim_turn(claim_params("conv-busy"))
            .await
            .expect("claim"); // active at t=0

        // Advance past the idle TTL, but renew the active session's lease first so
        // it is a live owner, not a lapsed orphan, when the sweep runs.
        clock.set(SESSION_IDLE_TTL_MS + 1);
        seam.heartbeat("conv-busy", busy.lease_token, "t")
            .await
            .expect("renew lease");
        seam.claim_turn(claim_params("conv-fresh"))
            .await
            .expect("claim");

        assert!(!seam.cache_has("conv-stale").await, "idle settled swept");
        assert!(
            seam.cache_has("conv-busy").await,
            "active session with a live lease survives the TTL sweep"
        );
        assert!(seam.cache_has("conv-fresh").await);
    }

    /// A settled session accessed within the TTL is *not* swept; a `save` (an
    /// access) refreshes its last-access time so it survives an otherwise-expiring
    /// window.
    #[tokio::test]
    async fn save_refreshes_last_access_against_ttl() {
        let clock = Arc::new(FakeClock::new());
        let seam = InMemorySessionSeam::with_clock(clock.clone());
        clock.set(0);
        let mut s = seam
            .claim_turn(claim_params("conv-1"))
            .await
            .expect("claim");
        s.finish_turn(TurnState::Idle, now_rfc3339());

        // Just before the TTL would expire from t=0, touch it with a save.
        clock.set(SESSION_IDLE_TTL_MS - 1);
        seam.save(&mut s).await.expect("save refreshes access");

        // Advance to where the original t=0 access would have expired, but the
        // refreshed access (t=TTL-1) is still inside the window.
        clock.set(SESSION_IDLE_TTL_MS + 1);
        seam.claim_turn(claim_params("conv-2"))
            .await
            .expect("claim");
        assert!(
            seam.cache_has("conv-1").await,
            "save refreshed last-access, so the session is still live"
        );
    }

    // ------------------------- Lease fencing / heartbeat / recovery -------------------------

    /// A revived old owner whose lease was taken over cannot overwrite the new
    /// owner's work: after the lease lapses and a re-claim rotates the token, the
    /// old owner's `save` is rejected as lease-lost.
    #[tokio::test]
    async fn expired_owner_save_is_fenced_after_takeover() {
        let clock = Arc::new(FakeClock::new());
        let seam = InMemorySessionSeam::with_clock(clock.clone());
        clock.set(0);
        // Owner A claims and holds its session snapshot.
        let mut a = seam
            .claim_turn(claim_params("conv-1"))
            .await
            .expect("A claim");
        assert_eq!(a.lease_token, 1);

        // A's lease lapses; a later claim (B) recovers the orphan and rotates the
        // token (the recovered turn settled to Failed, which is re-claimable).
        clock.set(LEASE_TTL_MS + 1);
        let b = seam
            .claim_turn(claim_params("conv-1"))
            .await
            .expect("B reclaim");
        assert_eq!(b.lease_token, 2, "takeover rotated the fencing token");

        // A wakes and tries to persist its stale snapshot: fenced out (lease lost).
        a.conversation.push(ChatMessage::text(
            "late",
            ChatRole::Assistant,
            "stale write",
        ));
        let err = seam.save(&mut a).await.unwrap_err();
        assert_eq!(err.kind, AgentErrorKind::SessionUnavailable);
    }

    /// A heartbeat extends the lease only for the current token and never bumps the
    /// version, so a concurrent save by the owner still succeeds; a stale-token
    /// heartbeat is rejected.
    #[tokio::test]
    async fn heartbeat_renews_without_version_bump_and_fences_stale_token() {
        let clock = Arc::new(FakeClock::new());
        let seam = InMemorySessionSeam::with_clock(clock.clone());
        clock.set(0);
        let mut s = seam
            .claim_turn(claim_params("conv-1"))
            .await
            .expect("claim");
        let held_version = s.version;

        // Heartbeat renews the lease and does not touch the version: a subsequent
        // save by the owner (holding the same version) still passes its CAS.
        clock.set(10_000);
        seam.heartbeat("conv-1", s.lease_token, "t")
            .await
            .expect("renew");
        assert_eq!(
            s.version, held_version,
            "heartbeat left the held version intact"
        );
        seam.save(&mut s).await.expect("owner save still wins");

        // A heartbeat with a stale token is fenced.
        let err = seam
            .heartbeat("conv-1", s.lease_token + 99, "t")
            .await
            .unwrap_err();
        assert_eq!(err.kind, AgentErrorKind::SessionUnavailable);
    }

    /// Claiming a conversation whose active turn's lease has lapsed recovers the
    /// orphan in place: the prior history is preserved, the turn is re-claimable,
    /// and an interrupted mutating turn is barred from new mutation.
    #[tokio::test]
    async fn claim_recovers_expired_orphan_and_preserves_history() {
        let clock = Arc::new(FakeClock::new());
        let seam = InMemorySessionSeam::with_clock(clock.clone());
        clock.set(0);
        // First turn accumulates history then settles, so a follow-up re-claims it.
        let mut s = seam
            .claim_turn(claim_params("conv-1"))
            .await
            .expect("claim");
        s.conversation
            .push(ChatMessage::text("u1", ChatRole::User, "why slow?"));
        s.conversation
            .push(ChatMessage::text("a1", ChatRole::Assistant, "looking"));
        // Leave it Running (an active orphan) and let the lease lapse.
        seam.save(&mut s).await.expect("save running");

        clock.set(LEASE_TTL_MS + 1);
        let recovered = seam
            .claim_turn(claim_params("conv-1"))
            .await
            .expect("reclaim recovers the orphan");
        assert_eq!(recovered.turn_state, TurnState::Running, "new turn claimed");
        assert!(
            recovered.conversation.len() >= 2,
            "prior history preserved across recovery"
        );
        assert!(
            recovered.lease_token >= 2,
            "recovery + claim rotated the token"
        );
    }

    /// Once the turn settles, the heartbeat refuses (it only renews active turns),
    /// so a background renewer stops cleanly.
    #[tokio::test]
    async fn heartbeat_stops_after_turn_settles() {
        let clock = Arc::new(FakeClock::new());
        let seam = InMemorySessionSeam::with_clock(clock.clone());
        clock.set(0);
        let mut s = seam
            .claim_turn(claim_params("conv-1"))
            .await
            .expect("claim");
        seam.heartbeat("conv-1", s.lease_token, "t")
            .await
            .expect("renew while active");

        s.finish_turn(TurnState::Idle, now_rfc3339());
        seam.save(&mut s).await.expect("settle save");
        let err = seam
            .heartbeat("conv-1", s.lease_token, "t")
            .await
            .unwrap_err();
        assert_eq!(err.kind, AgentErrorKind::SessionUnavailable);
    }
}
