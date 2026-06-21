//! Agentic-loop session state: the two orthogonal state machines, the persisted
//! session, the authorization scope helpers, and the per-turn / lifetime counts.
//!
//! Two machines move independently (§5):
//! - [`TurnState`] tracks the conversational turn (idle → running → …). Only the
//!   turn machine gates whether a new turn may start.
//! - [`ExecutionState`] tracks an in-flight mutating execution and its
//!   reconciliation. A turn can finish while an execution is still unknown, and a
//!   late result resolves the execution without touching the turn machine.
//!
//! [`PersistedAgentSession`] is the whole thing serialized: the manager stores it
//! in Redis/DB and replays it across instances and restarts, so it is `serde` and
//! carries a `version` for optimistic concurrency. The Direct runtime keeps the
//! same struct in memory. The persistent **subject** (tenant / actor / device /
//! policy revision / scope) is validated on every follow-up turn and never
//! rebound on reconnect; the **turn routing** fields (connection / request /
//! turn id) are transient and rebind each turn.

use desk_agent_protocol::AgentScope;
use serde::{Deserialize, Serialize};

use crate::chat::TokenUsage;

/// The conversational turn machine. A new turn may be claimed only from
/// [`TurnState::Idle`]; `claim_turn` is the only `Idle → Running` transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    /// No turn running; a new turn may be claimed.
    #[default]
    Idle,
    /// A turn is executing the agent loop.
    Running,
    /// A mutating tool is waiting for user approval (set in the mutating path).
    AwaitingApproval,
    /// The current turn was cancelled by the operator.
    Cancelled,
    /// The current turn ended in an error.
    Failed,
}

impl TurnState {
    /// Whether the turn has reached a settled (terminal) state, so a fresh
    /// follow-up turn may be claimed while reusing the accumulated conversation
    /// history. [`Idle`] (a completed turn), [`Failed`] (an errored turn), and
    /// [`Cancelled`] are all settled.
    ///
    /// [`Idle`]: TurnState::Idle
    /// [`Failed`]: TurnState::Failed
    /// [`Cancelled`]: TurnState::Cancelled
    pub fn is_settled(self) -> bool {
        matches!(
            self,
            TurnState::Idle | TurnState::Failed | TurnState::Cancelled
        )
    }

    /// Whether a turn is in flight ([`Running`] or [`AwaitingApproval`]). An
    /// active turn is **not** directly claimable — it must first be safely
    /// settled (e.g. by lease recovery) before a follow-up turn can begin.
    ///
    /// [`Running`]: TurnState::Running
    /// [`AwaitingApproval`]: TurnState::AwaitingApproval
    pub fn is_active(self) -> bool {
        matches!(self, TurnState::Running | TurnState::AwaitingApproval)
    }

    /// Whether a fresh turn may be claimed from this state. A turn is claimable
    /// from any settled state (so follow-up questions continue the same session);
    /// an active turn ([`is_active`]) is not.
    ///
    /// [`is_active`]: TurnState::is_active
    pub fn can_claim(self) -> bool {
        self.is_settled()
    }
}

/// The execution-reconciliation machine, orthogonal to [`TurnState`].
///
/// Only the mutating path drives the non-`None` variants; a read-only turn leaves
/// this at [`ExecutionState::None`]. The `work_id` / `execution_id` /
/// `exec_request_id` fields identify the durable work item and the immutable
/// dispatch generation used for late-result fencing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ExecutionState {
    /// No execution in flight.
    #[default]
    None,
    /// A mutating tool was dispatched and its real result is awaited.
    Executing {
        work_id: i64,
        execution_id: String,
        exec_request_id: String,
    },
    /// The execution may have run but its outcome is unknown (cancel / timeout /
    /// crash after dispatch). `placeholder_message_id` anchors the placeholder
    /// tool result that keeps the conversation well-formed; a late real result
    /// replaces it in place (CAS by id) rather than appending.
    OutcomeUnknown {
        work_id: i64,
        execution_id: String,
        exec_request_id: String,
        placeholder_message_id: String,
        since: String,
    },
}

impl ExecutionState {
    /// Whether a mutating tool may be exposed/started right now. While an outcome
    /// is unknown, only read-only follow-up is allowed (no new mutation).
    pub fn allows_new_mutation(&self) -> bool {
        matches!(self, ExecutionState::None)
    }
}

/// One agent conversation + session, in the shape the manager persists (and the
/// Direct runtime keeps in memory).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedAgentSession {
    pub conversation_id: String,
    pub conversation: Vec<crate::chat::ChatMessage>,

    // ---- Persistent security subject (validated each follow-up; never rebinds) ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    pub actor_id: String,
    pub device_id: String,
    /// Authorization anchor. A change is detected at turn boundaries (recompute)
    /// and mid-turn (narrow-only intersection); see [`Self::begin_turn`].
    pub policy_revision: i64,
    /// Scope snapshot captured at the start of the current turn — the baseline for
    /// the narrow-only mid-turn intersection (§5.3).
    pub turn_start_scope: AgentScope,
    /// Currently effective scope: at a turn boundary it equals the freshly
    /// computed PDP scope (may expand or narrow vs the last turn); during a turn
    /// it is `turn_start_scope ∩ latest` (narrow only).
    pub scope_snapshot: AgentScope,

    // ---- Transient turn routing (rebinds each turn / on reconnect) ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_control_connection_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_turn_id: Option<String>,

    // ---- The two orthogonal machines ----
    pub turn_state: TurnState,
    pub execution_state: ExecutionState,

    // ---- Counting: turn-level (circuit breaker) + lifetime (audit / budget) ----
    pub current_turn_steps: u32,
    pub current_turn_tokens: TokenUsage,
    pub lifetime_steps: u32,
    pub lifetime_tokens: TokenUsage,

    pub created_at: String,
    pub updated_at: String,
    /// Optimistic-concurrency version (DB CAS on the manager).
    pub version: i64,
}

/// Why a turn could not be claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnClaimError {
    /// A turn is already running (turn state is not `Idle`).
    Busy,
}

/// Why a follow-up turn was refused at the subject check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubjectMismatch {
    Tenant,
    Actor,
    Device,
}

impl PersistedAgentSession {
    /// Start a brand-new session bound to a subject, with a turn-boundary scope.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        conversation_id: impl Into<String>,
        tenant_id: Option<String>,
        actor_id: impl Into<String>,
        device_id: impl Into<String>,
        policy_revision: i64,
        scope: AgentScope,
        now: impl Into<String> + Clone,
    ) -> Self {
        Self {
            conversation_id: conversation_id.into(),
            conversation: Vec::new(),
            tenant_id,
            actor_id: actor_id.into(),
            device_id: device_id.into(),
            policy_revision,
            turn_start_scope: scope.clone(),
            scope_snapshot: scope,
            active_control_connection_id: None,
            current_request_id: None,
            current_turn_id: None,
            turn_state: TurnState::Idle,
            execution_state: ExecutionState::None,
            current_turn_steps: 0,
            current_turn_tokens: TokenUsage::default(),
            lifetime_steps: 0,
            lifetime_tokens: TokenUsage::default(),
            created_at: now.clone().into(),
            updated_at: now.into(),
            version: 0,
        }
    }

    /// Validate that a follow-up turn comes from the same subject. The connection
    /// id is **not** part of the subject (a reconnect must be able to continue).
    pub fn check_subject(
        &self,
        tenant_id: Option<&str>,
        actor_id: &str,
        device_id: &str,
    ) -> Result<(), SubjectMismatch> {
        if self.tenant_id.as_deref() != tenant_id {
            return Err(SubjectMismatch::Tenant);
        }
        if self.actor_id != actor_id {
            return Err(SubjectMismatch::Actor);
        }
        if self.device_id != device_id {
            return Err(SubjectMismatch::Device);
        }
        Ok(())
    }

    /// Claim the conversation for a new turn (a `settled → Running` transition).
    /// A turn may be claimed from any settled state ([`TurnState::can_claim`]), so
    /// a follow-up question continues the same conversation even after the prior
    /// turn finished or failed; the accumulated `conversation` history is left
    /// untouched. `execution_state` is also left as-is, so an unresolved
    /// `OutcomeUnknown` keeps the next turn read-only. On success: bind the turn
    /// routing, recompute the scope at the turn boundary (`current_pdp` may expand
    /// or narrow vs the last turn), and reset the turn-level counters. The caller's
    /// [`SessionSeam`] is responsible for doing this atomically (DB CAS / in-memory
    /// lock).
    ///
    /// [`SessionSeam`]: crate::seam::SessionSeam
    pub fn begin_turn(
        &mut self,
        turn_id: impl Into<String>,
        request_id: Option<String>,
        connection_id: Option<String>,
        policy_revision: i64,
        current_pdp_scope: AgentScope,
        now: impl Into<String>,
    ) -> Result<(), TurnClaimError> {
        if !self.turn_state.can_claim() {
            return Err(TurnClaimError::Busy);
        }
        self.turn_state = TurnState::Running;
        self.current_turn_id = Some(turn_id.into());
        self.current_request_id = request_id;
        self.active_control_connection_id = connection_id;
        // Turn boundary: adopt the freshly computed PDP scope (expand or narrow).
        self.policy_revision = policy_revision;
        self.turn_start_scope = current_pdp_scope.clone();
        self.scope_snapshot = current_pdp_scope;
        // Reset the turn-level circuit-breaker counters.
        self.current_turn_steps = 0;
        self.current_turn_tokens = TokenUsage::default();
        self.updated_at = now.into();
        Ok(())
    }

    /// Apply a mid-turn policy change: narrow the effective scope to
    /// `turn_start_scope ∩ latest` (a running turn may only lose authority, never
    /// gain it). Updates `policy_revision` to the observed latest.
    pub fn narrow_for_revision(&mut self, latest_revision: i64, latest_scope: &AgentScope) {
        self.scope_snapshot = narrow_scope(&self.turn_start_scope, latest_scope);
        self.policy_revision = latest_revision;
    }

    /// Record one model→tool step and its token usage against both the turn-level
    /// counters (circuit breaker) and the lifetime counters (audit / budget).
    pub fn record_step(&mut self, usage: TokenUsage) {
        self.current_turn_steps = self.current_turn_steps.saturating_add(1);
        self.lifetime_steps = self.lifetime_steps.saturating_add(1);
        add_usage(&mut self.current_turn_tokens, usage);
        add_usage(&mut self.lifetime_tokens, usage);
    }

    /// Whether the per-turn step budget is exhausted (circuit breaker).
    pub fn turn_step_budget_exhausted(&self) -> bool {
        self.current_turn_steps >= crate::MAX_STEPS_PER_TURN
    }

    /// Settle the turn machine at the end of a turn. Only the turn machine is
    /// touched — an in-flight [`ExecutionState`] is left for its own
    /// reconciliation. `now` updates the modified timestamp.
    pub fn finish_turn(&mut self, terminal: TurnState, now: impl Into<String>) {
        self.turn_state = terminal;
        self.updated_at = now.into();
    }

    /// Reconcile a late execution result against an unknown outcome (§6.2): if the
    /// execution machine is [`ExecutionState::OutcomeUnknown`] for `execution_id`,
    /// replace the placeholder tool-result message (matched by its `message_id`)
    /// text **in place** — never appending a second tool result for the same call —
    /// and clear the execution machine to [`ExecutionState::None`]. Returns whether
    /// a reconciliation happened (false if already resolved / a different
    /// execution / not unknown). The durable result is written first by the caller;
    /// this only mutates the conversation + execution state.
    pub fn reconcile_late_result(
        &mut self,
        execution_id: &str,
        result_text: impl Into<String>,
        now: impl Into<String>,
    ) -> bool {
        let placeholder_id = match &self.execution_state {
            ExecutionState::OutcomeUnknown {
                execution_id: current,
                placeholder_message_id,
                ..
            } if current == execution_id => placeholder_message_id.clone(),
            _ => return false,
        };
        if let Some(msg) = self
            .conversation
            .iter_mut()
            .find(|m| m.message_id == placeholder_id)
        {
            msg.text = result_text.into();
        }
        self.execution_state = ExecutionState::None;
        self.updated_at = now.into();
        true
    }
}

/// Add a [`TokenUsage`] delta into an accumulator, treating `None` as 0 and
/// preserving `Some` once either side reports a count.
fn add_usage(acc: &mut TokenUsage, delta: TokenUsage) {
    if let Some(d) = delta.input_tokens {
        acc.input_tokens = Some(acc.input_tokens.unwrap_or(0) + d);
    }
    if let Some(d) = delta.output_tokens {
        acc.output_tokens = Some(acc.output_tokens.unwrap_or(0) + d);
    }
}

/// Restrictiveness rank of an [`ExecutionMode`]: lower = less capable. The
/// declaration order is meaningfully increasing in capability, so the
/// intersection of two modes is the lower-ranked (more restrictive) one.
fn mode_rank(mode: desk_agent_protocol::ExecutionMode) -> u8 {
    use desk_agent_protocol::ExecutionMode::*;
    match mode {
        SuggestOnly => 0,
        ReadOnly => 1,
        ConfirmEachAction => 2,
        SessionApproved => 3,
        Automated => 4,
    }
}

/// Narrow `start` by `latest`: the intersection of two scopes (never broader than
/// `start`). Granted capabilities are the set intersection; the mode is the more
/// restrictive of the two; expiry is the earlier bound. Used for the narrow-only
/// mid-turn revision change (§5.3) — a running turn can only lose authority.
pub fn narrow_scope(start: &AgentScope, latest: &AgentScope) -> AgentScope {
    let granted: Vec<_> = start
        .granted
        .iter()
        .filter(|c| latest.granted.contains(c))
        .copied()
        .collect();
    let mode = if mode_rank(latest.mode) <= mode_rank(start.mode) {
        latest.mode
    } else {
        start.mode
    };
    let expires_at = match (&start.expires_at, &latest.expires_at) {
        (Some(a), Some(b)) => Some(if a <= b { a.clone() } else { b.clone() }),
        (Some(a), None) => Some(a.clone()),
        (None, b) => b.clone(),
    };
    AgentScope {
        granted,
        mode,
        expires_at,
        policy_name: latest
            .policy_name
            .clone()
            .or_else(|| start.policy_name.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{Capability, ExecutionMode};

    fn scope(granted: &[Capability], mode: ExecutionMode) -> AgentScope {
        AgentScope {
            granted: granted.to_vec(),
            mode,
            expires_at: None,
            policy_name: None,
        }
    }

    fn session() -> PersistedAgentSession {
        PersistedAgentSession::new(
            "conv-1",
            Some("tenant-a".into()),
            "actor-1",
            "device-1",
            7,
            scope(&[Capability::SystemInfo], ExecutionMode::ReadOnly),
            "2026-06-20T00:00:00Z",
        )
    }

    /// A turn can be claimed from a settled state; a second claim while Running is
    /// rejected; claiming resets the turn-level counters and adopts the new scope.
    #[test]
    fn begin_turn_from_settled_and_resets_counters() {
        let mut s = session();
        s.current_turn_steps = 5;
        s.lifetime_steps = 9;
        let new_scope = scope(
            &[Capability::SystemInfo, Capability::ProcessList],
            ExecutionMode::ReadOnly,
        );
        s.begin_turn(
            "turn-1",
            Some("req-1".into()),
            Some("conn-1".into()),
            8,
            new_scope.clone(),
            "2026-06-20T00:01:00Z",
        )
        .expect("claim from idle");
        assert_eq!(s.turn_state, TurnState::Running);
        assert_eq!(s.current_turn_steps, 0, "turn counter reset");
        assert_eq!(s.lifetime_steps, 9, "lifetime counter preserved");
        assert_eq!(s.policy_revision, 8);
        // Turn boundary adopts the new (here expanded) scope.
        assert_eq!(s.scope_snapshot, new_scope);
        assert_eq!(s.turn_start_scope, new_scope);

        // A second claim while Running is refused.
        assert_eq!(
            s.begin_turn("turn-2", None, None, 8, new_scope, "t"),
            Err(TurnClaimError::Busy)
        );
    }

    /// State classification: settled states ([`Idle`]/[`Failed`]/[`Cancelled`])
    /// are claimable and the complement of the active states
    /// ([`Running`]/[`AwaitingApproval`]).
    #[test]
    fn turn_state_settled_and_active_partition() {
        for st in [TurnState::Idle, TurnState::Failed, TurnState::Cancelled] {
            assert!(st.is_settled(), "{st:?} settled");
            assert!(!st.is_active(), "{st:?} not active");
            assert!(st.can_claim(), "{st:?} claimable");
        }
        for st in [TurnState::Running, TurnState::AwaitingApproval] {
            assert!(st.is_active(), "{st:?} active");
            assert!(!st.is_settled(), "{st:?} not settled");
            assert!(!st.can_claim(), "{st:?} not claimable");
        }
    }

    /// A follow-up turn can be claimed after the previous turn ended in `Failed`,
    /// and the accumulated conversation history is preserved across the re-claim
    /// (so the model sees the prior turns). An `AwaitingApproval`/`Running` session
    /// stays `Busy`.
    #[test]
    fn settled_session_reclaims_and_keeps_history() {
        use crate::chat::{ChatMessage, ChatRole};
        let mut s = session();
        // First turn runs, accumulates history, then fails.
        s.begin_turn("t1", None, None, 7, s.scope_snapshot.clone(), "t")
            .expect("first claim");
        s.conversation
            .push(ChatMessage::text("u1", ChatRole::User, "why is cpu high?"));
        s.conversation
            .push(ChatMessage::text("a1", ChatRole::Assistant, "checking..."));
        s.finish_turn(TurnState::Failed, "t");
        assert_eq!(s.turn_state, TurnState::Failed);

        // A second turn can be claimed from the failed state, history intact.
        s.begin_turn("t2", None, None, 7, s.scope_snapshot.clone(), "t")
            .expect("reclaim from failed");
        assert_eq!(s.turn_state, TurnState::Running);
        assert_eq!(s.conversation.len(), 2, "history preserved across reclaim");

        // While Running, a further claim is refused.
        assert_eq!(
            s.begin_turn("t3", None, None, 7, s.scope_snapshot.clone(), "t"),
            Err(TurnClaimError::Busy)
        );

        // AwaitingApproval is likewise not directly claimable.
        s.turn_state = TurnState::AwaitingApproval;
        assert_eq!(
            s.begin_turn("t4", None, None, 7, s.scope_snapshot.clone(), "t"),
            Err(TurnClaimError::Busy)
        );
    }

    /// `finish_turn` settles only the turn machine; the execution machine is left
    /// untouched (a late result reconciles it separately).
    #[test]
    fn finish_turn_leaves_execution_state() {
        let mut s = session();
        s.execution_state = ExecutionState::OutcomeUnknown {
            work_id: 1,
            execution_id: "e".into(),
            exec_request_id: "x".into(),
            placeholder_message_id: "p".into(),
            since: "t".into(),
        };
        s.begin_turn("t1", None, None, 7, s.scope_snapshot.clone(), "t")
            .unwrap();
        s.finish_turn(TurnState::Idle, "2026-06-20T00:02:00Z");
        assert_eq!(s.turn_state, TurnState::Idle);
        assert!(
            matches!(s.execution_state, ExecutionState::OutcomeUnknown { .. }),
            "execution state must survive finish_turn"
        );
    }

    /// The subject check ignores the connection id (reconnect can continue) but
    /// rejects a different tenant / actor / device.
    #[test]
    fn subject_check_ignores_connection_but_pins_identity() {
        let s = session();
        assert!(
            s.check_subject(Some("tenant-a"), "actor-1", "device-1")
                .is_ok()
        );
        assert_eq!(
            s.check_subject(Some("tenant-b"), "actor-1", "device-1"),
            Err(SubjectMismatch::Tenant)
        );
        assert_eq!(
            s.check_subject(Some("tenant-a"), "actor-9", "device-1"),
            Err(SubjectMismatch::Actor)
        );
        assert_eq!(
            s.check_subject(Some("tenant-a"), "actor-1", "device-9"),
            Err(SubjectMismatch::Device)
        );
    }

    /// A mid-turn revision change narrows only: capabilities shrink to the
    /// intersection and the mode drops to the more restrictive, never expanding
    /// past the turn-start scope.
    #[test]
    fn narrow_for_revision_only_shrinks() {
        let mut s = session();
        s.begin_turn(
            "t1",
            None,
            None,
            7,
            scope(
                &[Capability::SystemInfo, Capability::ProcessList],
                ExecutionMode::ConfirmEachAction,
            ),
            "t",
        )
        .unwrap();
        // Latest policy tries to add NetworkPorts (ignored — only intersection)
        // and is more restrictive on mode (ReadOnly < ConfirmEachAction → wins).
        s.narrow_for_revision(
            9,
            &scope(
                &[Capability::ProcessList, Capability::NetworkPorts],
                ExecutionMode::ReadOnly,
            ),
        );
        assert_eq!(s.scope_snapshot.granted, vec![Capability::ProcessList]);
        assert_eq!(s.scope_snapshot.mode, ExecutionMode::ReadOnly);
        assert_eq!(s.policy_revision, 9);
    }

    /// Steps and tokens accumulate against both turn-level and lifetime counters;
    /// the per-turn budget trips at `MAX_STEPS_PER_TURN`.
    #[test]
    fn step_counting_and_budget() {
        let mut s = session();
        for _ in 0..crate::MAX_STEPS_PER_TURN {
            assert!(!s.turn_step_budget_exhausted());
            s.record_step(TokenUsage {
                input_tokens: Some(10),
                output_tokens: Some(2),
            });
        }
        assert!(s.turn_step_budget_exhausted());
        assert_eq!(s.current_turn_steps, crate::MAX_STEPS_PER_TURN);
        assert_eq!(
            s.current_turn_tokens.input_tokens,
            Some(10 * crate::MAX_STEPS_PER_TURN as i64)
        );
        assert_eq!(
            s.lifetime_tokens.output_tokens,
            Some(2 * crate::MAX_STEPS_PER_TURN as i64)
        );
    }

    /// A late result reconciles an unknown outcome: it replaces the placeholder
    /// message text in place (no second tool result) and clears the execution
    /// machine; a mismatched execution id or an already-resolved state is a no-op.
    #[test]
    fn reconcile_late_result_replaces_placeholder_in_place() {
        let mut s = session();
        s.conversation.push(crate::chat::ChatMessage::tool_result(
            "ph-1",
            "call-1",
            "execution outcome unknown; the command may have executed; do not assume success",
        ));
        s.execution_state = ExecutionState::OutcomeUnknown {
            work_id: 7,
            execution_id: "exec-1".into(),
            exec_request_id: "req-1".into(),
            placeholder_message_id: "ph-1".into(),
            since: "2026-06-20T00:00:00Z".into(),
        };
        let conv_len = s.conversation.len();

        // A different execution id does not reconcile.
        assert!(!s.reconcile_late_result("other", "ran ok", "2026-06-20T00:01:00Z"));
        assert!(matches!(
            s.execution_state,
            ExecutionState::OutcomeUnknown { .. }
        ));

        // The matching id replaces the placeholder text in place and clears state.
        assert!(s.reconcile_late_result("exec-1", "exit_code=0", "2026-06-20T00:02:00Z"));
        assert_eq!(s.execution_state, ExecutionState::None);
        assert_eq!(
            s.conversation.len(),
            conv_len,
            "no second tool result appended"
        );
        let ph = s
            .conversation
            .iter()
            .find(|m| m.message_id == "ph-1")
            .unwrap();
        assert_eq!(ph.text, "exit_code=0");
        assert_eq!(ph.tool_call_id.as_deref(), Some("call-1"));

        // A second reconcile is a no-op (already resolved).
        assert!(!s.reconcile_late_result("exec-1", "again", "2026-06-20T00:03:00Z"));
    }

    /// The session round-trips through serde (manager persistence).
    #[test]
    fn session_round_trips() {
        let mut s = session();
        s.conversation.push(crate::chat::ChatMessage::text(
            "m0",
            crate::chat::ChatRole::User,
            "hi",
        ));
        let json = serde_json::to_string(&s).unwrap();
        let back: PersistedAgentSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }
}
