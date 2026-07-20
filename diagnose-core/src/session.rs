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
//! same struct in memory. The persistent **subject** (actor / device /
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
    /// A turn was interrupted (crash / lease takeover) while a mutating tool was
    /// outstanding, but with **no recoverable execution identity** — the runtime
    /// could not prove whether the command ran or even reached dispatch. Unlike
    /// [`OutcomeUnknown`], no late result will ever reconcile this, so the
    /// conversation is permanently barred from new mutation (read-only follow-up
    /// only). This is the conservative recovery verdict when nothing better is known.
    ///
    /// [`OutcomeUnknown`]: ExecutionState::OutcomeUnknown
    Interrupted { since: String },
}

impl ExecutionState {
    /// Whether a mutating tool may be exposed/started right now. A new mutation is
    /// allowed only from a clean [`None`] state; while an outcome is unknown or the
    /// session was interrupted with no recoverable identity, only read-only
    /// follow-up is allowed.
    ///
    /// [`None`]: ExecutionState::None
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

    /// Lease fencing token. Rotated on every claim (and on lease takeover during
    /// recovery), it identifies the *current* turn owner. A [`SessionSeam::save`]
    /// CAS matches on this token **and** the `version`: the version blocks a stale
    /// snapshot from the same owner, while the token blocks a *revived* old owner
    /// whose lease already expired and was taken over — its save fails because the
    /// token has since rotated, so it can never overwrite the new owner's work.
    ///
    /// [`SessionSeam::save`]: crate::seam::SessionSeam::save
    #[serde(default)]
    pub lease_token: u64,

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
    Actor,
    Device,
}

impl PersistedAgentSession {
    /// Start a brand-new session bound to a subject, with a turn-boundary scope.
    pub fn new(
        conversation_id: impl Into<String>,
        actor_id: impl Into<String>,
        device_id: impl Into<String>,
        policy_revision: i64,
        scope: AgentScope,
        now: impl Into<String> + Clone,
    ) -> Self {
        Self {
            conversation_id: conversation_id.into(),
            conversation: Vec::new(),
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
            lease_token: 0,
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
    pub fn check_subject(&self, actor_id: &str, device_id: &str) -> Result<(), SubjectMismatch> {
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
        // Rotate the fencing token: this claim becomes the sole owner, and any
        // prior owner whose lease was taken over now holds a stale token.
        self.lease_token = self.lease_token.wrapping_add(1);
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

    /// Whether the per-turn step budget is exhausted (circuit breaker). The bound
    /// is supplied by the caller (via `LoopDeps`) so different runtimes can tune
    /// it — diagnose uses [`crate::MAX_STEPS_PER_TURN`], the latency-sensitive
    /// terminal copilot uses a tighter bound.
    pub fn turn_step_budget_exhausted(&self, max_steps_per_turn: u32) -> bool {
        self.current_turn_steps >= max_steps_per_turn
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

    /// Append a mid-conversation system-event notification identified by `event_id`,
    /// idempotently. The appended [`ChatRole::SystemEvent`] message's `message_id`
    /// **is** the `event_id`, so a redelivery of the same logical event — the
    /// durable completion queue is allowed to deliver more than once and requires
    /// the sink to dedupe — finds it already present and is a no-op.
    ///
    /// Returns whether the message was newly appended (`true`) or already present
    /// (`false`). Pure: the caller persists the mutated session under its own CAS.
    ///
    /// [`ChatRole::SystemEvent`]: crate::chat::ChatRole::SystemEvent
    pub fn append_event_if_absent(
        &mut self,
        event_id: &str,
        text: impl Into<String>,
        now: impl Into<String>,
    ) -> bool {
        if self.conversation.iter().any(|m| m.message_id == event_id) {
            return false;
        }
        self.conversation
            .push(crate::chat::ChatMessage::system_event(event_id, text));
        self.updated_at = now.into();
        true
    }

    /// Deliver a completed background execution's result into the conversation,
    /// idempotently keyed by `event_id`. This is the durable completion queue's sink,
    /// covering every way the result can relate to the conversation:
    ///
    /// - **Already delivered** — a message keyed by `event_id` is present (a
    ///   foreground result the loop keyed on the delivery id, or a prior delivery of
    ///   this same event): a no-op (`false`).
    /// - **Recovered unknown outcome** — the execution machine is
    ///   [`ExecutionState::OutcomeUnknown`] for this `execution_id`: replace the
    ///   placeholder tool result in place and re-key it to `event_id` so a later
    ///   redelivery is recognized as already delivered.
    /// - **Open tool call** — the call is still unanswered (a foreground save was
    ///   lost before it closed the call): close it with the real result, keyed by
    ///   `event_id`, keeping the model history well-formed.
    /// - **Closed tool call** — the call already has a result (a background dispatch
    ///   closed it with a running-task placeholder): append the completion as a
    ///   [`ChatRole::SystemEvent`] keyed by `event_id`.
    ///
    /// In the last three cases an outstanding [`ExecutionState::Executing`] for this
    /// execution is cleared so a follow-up may mutate again. Returns whether the
    /// session was mutated. Pure: the caller persists under its own CAS.
    ///
    /// [`ChatRole::SystemEvent`]: crate::chat::ChatRole::SystemEvent
    pub fn apply_completion(
        &mut self,
        event_id: &str,
        execution_id: &str,
        tool_call_id: &str,
        result_text: impl Into<String>,
        now: impl Into<String>,
    ) -> bool {
        // Idempotent: a message keyed by this event is already in the conversation.
        if self.conversation.iter().any(|m| m.message_id == event_id) {
            return false;
        }
        let result_text = result_text.into();
        let now = now.into();

        // A recovered unknown outcome for this execution: replace the placeholder in
        // place and re-key it to the event id so a redelivery dedups on it above.
        if let ExecutionState::OutcomeUnknown {
            execution_id: current,
            placeholder_message_id,
            ..
        } = &self.execution_state
            && current == execution_id
        {
            let placeholder_id = placeholder_message_id.clone();
            if let Some(msg) = self
                .conversation
                .iter_mut()
                .find(|m| m.message_id == placeholder_id)
            {
                msg.text = result_text;
                msg.message_id = event_id.to_string();
            }
            self.execution_state = ExecutionState::None;
            self.updated_at = now;
            return true;
        }

        // Otherwise close the open call with the real result, or — if it is already
        // closed — append the completion as a system event; both keyed by event id.
        let open = unclosed_tool_call_ids(&self.conversation)
            .iter()
            .any(|id| id == tool_call_id);
        if open {
            self.conversation
                .push(crate::chat::ChatMessage::tool_result(
                    event_id,
                    tool_call_id,
                    result_text,
                ));
        } else {
            self.conversation
                .push(crate::chat::ChatMessage::system_event(
                    event_id,
                    result_text,
                ));
        }
        if matches!(
            &self.execution_state,
            ExecutionState::Executing { execution_id: current, .. } if current == execution_id
        ) {
            self.execution_state = ExecutionState::None;
        }
        self.updated_at = now;
        true
    }

    /// The tool-call ids the conversation left unanswered — an assistant tool call
    /// with no matching tool result, left dangling when a turn was interrupted
    /// mid-execution. A recovering [`SessionSeam`] reads these to correlate the
    /// in-flight call with its durable work item before deciding the recovery
    /// [`verdict`]. First-seen order, de-duplicated.
    ///
    /// [`verdict`]: RecoveryVerdict
    /// [`SessionSeam`]: crate::seam::SessionSeam
    pub fn unclosed_tool_call_ids(&self) -> Vec<String> {
        unclosed_tool_call_ids(&self.conversation)
    }

    /// Recover an orphaned **active** session (its lease expired and was taken over)
    /// into a well-formed, settled state, per an explicitly supplied [`verdict`].
    ///
    /// A crash mid-turn can leave the conversation malformed: an assistant
    /// tool-call message with no matching tool result (the loop never returned from
    /// `confirm_and_exec`). Recovery closes every such unclosed tool call with a
    /// placeholder result so the model history replays cleanly, settles the turn to
    /// [`TurnState::Failed`] so a read-only follow-up can be claimed, and sets the
    /// execution machine according to the verdict:
    ///
    /// - [`NotExecuted`] — the command provably never ran: a plain "not executed"
    ///   result; execution machine cleared to [`None`] (a later turn may mutate).
    /// - [`OutcomeUnknown`] — dispatched with a recoverable identity: a placeholder
    ///   a late real result can replace in place ([`reconcile_late_result`]); only
    ///   read-only follow-up until then.
    /// - [`InterruptedUnknown`] — no recoverable identity: closed as interrupted and
    ///   the conversation is permanently barred from new mutation, without
    ///   fabricating an unreconcilable identity.
    ///
    /// The caller (a [`SessionSeam`]) decides the verdict and performs this inside
    /// the same atomic claim that rotates the lease token; recovery itself is pure.
    ///
    /// [`verdict`]: RecoveryVerdict
    /// [`NotExecuted`]: RecoveryVerdict::NotExecuted
    /// [`OutcomeUnknown`]: RecoveryVerdict::OutcomeUnknown
    /// [`InterruptedUnknown`]: RecoveryVerdict::InterruptedUnknown
    /// [`None`]: ExecutionState::None
    /// [`reconcile_late_result`]: Self::reconcile_late_result
    /// [`SessionSeam`]: crate::seam::SessionSeam
    pub fn recover_session(&mut self, verdict: RecoveryVerdict, now: impl Into<String>) {
        let now = now.into();
        let unclosed = unclosed_tool_call_ids(&self.conversation);
        match verdict {
            RecoveryVerdict::NotExecuted => {
                for call_id in &unclosed {
                    self.conversation
                        .push(crate::chat::ChatMessage::tool_result(
                            recovery_message_id(call_id),
                            call_id,
                            RECOVER_NOT_EXECUTED,
                        ));
                }
                self.execution_state = ExecutionState::None;
            }
            RecoveryVerdict::OutcomeUnknown {
                work_id,
                execution_id,
                exec_request_id,
            } => {
                // At most one mutating call is in flight at a time; close the first
                // unclosed call with a placeholder a late result can replace in
                // place, and record the unknown outcome with its identity.
                if let Some(call_id) = unclosed.first() {
                    let placeholder_id = recovery_message_id(call_id);
                    self.conversation
                        .push(crate::chat::ChatMessage::tool_result(
                            placeholder_id.clone(),
                            call_id,
                            RECOVER_OUTCOME_UNKNOWN,
                        ));
                    self.execution_state = ExecutionState::OutcomeUnknown {
                        work_id,
                        execution_id,
                        exec_request_id,
                        placeholder_message_id: placeholder_id,
                        since: now.clone(),
                    };
                }
                // Defensive: any further stragglers are closed as not-executed.
                for call_id in unclosed.iter().skip(1) {
                    self.conversation
                        .push(crate::chat::ChatMessage::tool_result(
                            recovery_message_id(call_id),
                            call_id,
                            RECOVER_NOT_EXECUTED,
                        ));
                }
            }
            RecoveryVerdict::InterruptedUnknown => {
                for call_id in &unclosed {
                    self.conversation
                        .push(crate::chat::ChatMessage::tool_result(
                            recovery_message_id(call_id),
                            call_id,
                            RECOVER_INTERRUPTED,
                        ));
                }
                self.execution_state = ExecutionState::Interrupted { since: now.clone() };
            }
        }
        self.finish_turn(TurnState::Failed, now);
    }
}

/// The placeholder tool-result text written when recovery proves a mutating call
/// never ran.
const RECOVER_NOT_EXECUTED: &str = "not executed: the turn was interrupted before this command ran";
/// The placeholder text for a recovered call whose outcome is unknown but
/// reconcilable; a late real result replaces it in place.
const RECOVER_OUTCOME_UNKNOWN: &str =
    "execution outcome unknown; the command may have executed; do not assume success";
/// The placeholder text for a recovered call with no recoverable identity (the
/// conversation is barred from further mutation).
const RECOVER_INTERRUPTED: &str = "the turn was interrupted; this command's outcome is unknown and cannot be reconciled; do not assume success";

/// Deterministic message id for a recovery-appended tool result, derived from the
/// closed tool call so it is unique within the conversation and stable for the
/// `OutcomeUnknown` placeholder's later in-place reconciliation.
fn recovery_message_id(tool_call_id: &str) -> String {
    format!("recover-{tool_call_id}")
}

/// Find tool-call ids that have no matching tool-result message — an assistant
/// tool call left unanswered when a turn was interrupted mid-execution. Preserves
/// first-seen order and de-duplicates.
fn unclosed_tool_call_ids(conversation: &[crate::chat::ChatMessage]) -> Vec<String> {
    let answered: std::collections::HashSet<&str> = conversation
        .iter()
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    let mut out: Vec<String> = Vec::new();
    for m in conversation {
        for c in &m.tool_calls {
            if !answered.contains(c.id.as_str()) && !out.iter().any(|x| x == &c.id) {
                out.push(c.id.clone());
            }
        }
    }
    out
}

/// The verdict a [`SessionSeam`] supplies to [`PersistedAgentSession::recover_session`]
/// when taking over an orphaned active session — how to close an outstanding
/// mutating tool call. Determining it requires runtime knowledge the pure session
/// does not hold (whether a durable work item was dispatched), so it is passed in.
///
/// [`SessionSeam`]: crate::seam::SessionSeam
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryVerdict {
    /// The command provably never ran (no dispatch); safe to clear the execution
    /// machine and allow a later mutation.
    NotExecuted,
    /// The command was dispatched and carries a recoverable identity; its outcome
    /// is unknown and a late result can reconcile it in place.
    OutcomeUnknown {
        work_id: i64,
        execution_id: String,
        exec_request_id: String,
    },
    /// No recoverable identity is known; close conservatively and bar further
    /// mutation without fabricating an unreconcilable identity.
    InterruptedUnknown,
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
    /// rejects a different actor / device.
    #[test]
    fn subject_check_ignores_connection_but_pins_identity() {
        let s = session();
        assert!(s.check_subject("actor-1", "device-1").is_ok());
        assert_eq!(
            s.check_subject("actor-9", "device-1"),
            Err(SubjectMismatch::Actor)
        );
        assert_eq!(
            s.check_subject("actor-1", "device-9"),
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
            assert!(!s.turn_step_budget_exhausted(crate::MAX_STEPS_PER_TURN));
            s.record_step(TokenUsage {
                input_tokens: Some(10),
                output_tokens: Some(2),
                ..Default::default()
            });
        }
        assert!(s.turn_step_budget_exhausted(crate::MAX_STEPS_PER_TURN));
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

    /// An event is appended once and keyed by its `event_id`; a redelivery of the
    /// same event finds it present and is a no-op (the durable queue may deliver
    /// more than once). A different event_id appends a second message.
    #[test]
    fn append_event_if_absent_is_idempotent_by_event_id() {
        let mut s = session();
        let base = s.conversation.len();

        assert!(s.append_event_if_absent("ev-1", "task exec_a finished: exit 0", "t1"));
        assert_eq!(s.conversation.len(), base + 1);
        let msg = s.conversation.last().unwrap();
        assert_eq!(msg.message_id, "ev-1");
        assert_eq!(msg.role, crate::chat::ChatRole::SystemEvent);
        assert_eq!(msg.text, "task exec_a finished: exit 0");

        // Redelivery of the same event_id is a no-op, even with different text.
        assert!(!s.append_event_if_absent("ev-1", "task exec_a finished: exit 99", "t2"));
        assert_eq!(s.conversation.len(), base + 1, "no duplicate append");
        assert_eq!(
            s.conversation.last().unwrap().text,
            "task exec_a finished: exit 0",
            "the first-delivered text is kept"
        );

        // A distinct event_id appends a new message.
        assert!(s.append_event_if_absent("ev-2", "task exec_b finished: exit 0", "t3"));
        assert_eq!(s.conversation.len(), base + 2);
    }

    /// A completion for an outstanding background dispatch (Executing, tool call
    /// already closed with a running-task placeholder) appends a system event and
    /// clears the execution machine; a redelivery is a no-op.
    #[test]
    fn apply_completion_appends_system_event_for_a_closed_call() {
        use crate::chat::{ChatMessage, ChatRole, ToolCallRef};
        let mut s = session();
        s.conversation.push(ChatMessage::assistant_tool_calls(
            "a1",
            String::new(),
            vec![ToolCallRef {
                id: "call-1".into(),
                name: "exec_command".into(),
                arguments_json: "{}".into(),
            }],
        ));
        // The dispatch already closed the call with a running-task placeholder.
        s.conversation.push(ChatMessage::tool_result(
            "run-1",
            "call-1",
            "dispatched; still running",
        ));
        s.execution_state = ExecutionState::Executing {
            work_id: 8,
            execution_id: "e9".into(),
            exec_request_id: "exec_t9".into(),
        };
        let base = s.conversation.len();

        assert!(s.apply_completion("work:8:done", "e9", "call-1", "exit_code=0", "t1"));
        assert_eq!(s.conversation.len(), base + 1);
        let ev = s.conversation.last().unwrap();
        assert_eq!(ev.message_id, "work:8:done");
        assert_eq!(ev.role, ChatRole::SystemEvent);
        assert_eq!(ev.text, "exit_code=0");
        assert_eq!(
            s.execution_state,
            ExecutionState::None,
            "the dispatch is settled"
        );

        // Redelivery of the same event is a no-op.
        assert!(!s.apply_completion("work:8:done", "e9", "call-1", "exit_code=0", "t2"));
        assert_eq!(s.conversation.len(), base + 1);
    }

    /// A completion for a tool call still open (a foreground save was lost before it
    /// closed the call) closes it with the real result, keyed by the event id.
    #[test]
    fn apply_completion_closes_an_open_call() {
        use crate::chat::{ChatMessage, ChatRole, ToolCallRef};
        let mut s = session();
        s.conversation.push(ChatMessage::assistant_tool_calls(
            "a1",
            String::new(),
            vec![ToolCallRef {
                id: "call-1".into(),
                name: "exec_command".into(),
                arguments_json: "{}".into(),
            }],
        ));
        // No tool result for call-1 yet — the call is open.
        assert_eq!(s.unclosed_tool_call_ids(), vec!["call-1".to_string()]);

        assert!(s.apply_completion("work:8:done", "e9", "call-1", "exit_code=0", "t1"));
        assert!(
            s.unclosed_tool_call_ids().is_empty(),
            "the call is now closed"
        );
        let msg = s.conversation.last().unwrap();
        assert_eq!(msg.message_id, "work:8:done");
        assert_eq!(msg.role, ChatRole::Tool);
        assert_eq!(msg.tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(msg.text, "exit_code=0");
    }

    /// A completion for a recovered unknown outcome replaces the placeholder in place
    /// (no second result) and re-keys it to the event id, so a redelivery dedups.
    #[test]
    fn apply_completion_reconciles_unknown_outcome_and_rekeys() {
        let mut s = session();
        s.conversation.push(crate::chat::ChatMessage::tool_result(
            "ph-1",
            "call-1",
            "execution outcome unknown; the command may have executed",
        ));
        s.execution_state = ExecutionState::OutcomeUnknown {
            work_id: 8,
            execution_id: "e9".into(),
            exec_request_id: "exec_t9".into(),
            placeholder_message_id: "ph-1".into(),
            since: "2026-06-20T00:00:00Z".into(),
        };
        let base = s.conversation.len();

        assert!(s.apply_completion("work:8:done", "e9", "call-1", "exit_code=0", "t1"));
        assert_eq!(s.conversation.len(), base, "no second result appended");
        assert_eq!(s.execution_state, ExecutionState::None);
        let msg = s
            .conversation
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call-1"))
            .unwrap();
        assert_eq!(msg.text, "exit_code=0");
        assert_eq!(msg.message_id, "work:8:done", "the placeholder is re-keyed");

        // Redelivery finds the re-keyed message present and is a no-op.
        assert!(!s.apply_completion("work:8:done", "e9", "call-1", "again", "t2"));
        assert_eq!(s.conversation.len(), base);
    }

    /// A completion whose event id is already present (a foreground result the loop
    /// keyed on the delivery id) is a no-op — it never doubles the result.
    #[test]
    fn apply_completion_dedups_a_foreground_keyed_result() {
        let mut s = session();
        s.conversation.push(crate::chat::ChatMessage::tool_result(
            "work:8:done",
            "call-1",
            "exit_code=0",
        ));
        let base = s.conversation.len();
        assert!(!s.apply_completion("work:8:done", "e9", "call-1", "exit_code=0", "t1"));
        assert_eq!(s.conversation.len(), base);
    }

    /// Each claim rotates the fencing token, so a stale prior owner can be told
    /// apart from the current one.
    #[test]
    fn begin_turn_rotates_lease_token() {
        let mut s = session();
        assert_eq!(s.lease_token, 0);
        s.begin_turn("t1", None, None, 7, s.scope_snapshot.clone(), "t")
            .unwrap();
        assert_eq!(s.lease_token, 1, "first claim rotates to 1");
        s.finish_turn(TurnState::Idle, "t");
        s.begin_turn("t2", None, None, 7, s.scope_snapshot.clone(), "t")
            .unwrap();
        assert_eq!(s.lease_token, 2, "second claim rotates again");
    }

    /// The public accessor surfaces dangling tool calls (no matching tool result)
    /// so a recovering seam can correlate them with durable work; answered calls are
    /// excluded.
    #[test]
    fn unclosed_tool_call_ids_lists_only_dangling_calls() {
        use crate::chat::{ChatMessage, ToolCallRef};
        let mut s = session();
        s.conversation.push(ChatMessage::assistant_tool_calls(
            "a1",
            String::new(),
            vec![
                ToolCallRef {
                    id: "answered".into(),
                    name: "read".into(),
                    arguments_json: "{}".into(),
                },
                ToolCallRef {
                    id: "dangling".into(),
                    name: "exec_command".into(),
                    arguments_json: "{}".into(),
                },
            ],
        ));
        s.conversation
            .push(ChatMessage::tool_result("r1", "answered", "ok"));
        assert_eq!(s.unclosed_tool_call_ids(), vec!["dangling".to_string()]);
    }

    /// Recovery of an interrupted mutating turn with no recoverable identity closes
    /// the unclosed tool call, settles to Failed, and bars further mutation.
    #[test]
    fn recover_interrupted_unknown_closes_and_bars_mutation() {
        use crate::chat::{ChatMessage, ChatRole, ToolCallRef};
        let mut s = session();
        s.begin_turn("t1", None, None, 7, s.scope_snapshot.clone(), "t")
            .unwrap();
        s.conversation
            .push(ChatMessage::text("u1", ChatRole::User, "restart it"));
        // Assistant requested a mutating call that never got a tool result (crash
        // during the approval/exec wait → AwaitingApproval).
        s.conversation.push(ChatMessage::assistant_tool_calls(
            "a1",
            String::new(),
            vec![ToolCallRef {
                id: "call-x".into(),
                name: "exec_command".into(),
                arguments_json: "{}".into(),
            }],
        ));
        s.turn_state = TurnState::AwaitingApproval;

        s.recover_session(RecoveryVerdict::InterruptedUnknown, "2026-06-20T01:00:00Z");

        assert_eq!(s.turn_state, TurnState::Failed);
        assert!(
            matches!(s.execution_state, ExecutionState::Interrupted { .. }),
            "interrupted bars new mutation"
        );
        assert!(!s.execution_state.allows_new_mutation());
        let closed = s
            .conversation
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call-x"))
            .expect("the unclosed call was closed");
        assert_eq!(closed.role, ChatRole::Tool);
        assert!(closed.text.contains("interrupted"));
    }

    /// Recovery with `NotExecuted` closes the call as not-run and clears the
    /// execution machine so a later turn may mutate again.
    #[test]
    fn recover_not_executed_clears_execution_state() {
        use crate::chat::{ChatMessage, ToolCallRef};
        let mut s = session();
        s.begin_turn("t1", None, None, 7, s.scope_snapshot.clone(), "t")
            .unwrap();
        s.conversation.push(ChatMessage::assistant_tool_calls(
            "a1",
            String::new(),
            vec![ToolCallRef {
                id: "call-y".into(),
                name: "exec_command".into(),
                arguments_json: "{}".into(),
            }],
        ));
        s.recover_session(RecoveryVerdict::NotExecuted, "t2");
        assert_eq!(s.turn_state, TurnState::Failed);
        assert_eq!(s.execution_state, ExecutionState::None);
        assert!(s.execution_state.allows_new_mutation());
    }

    /// Recovery with `OutcomeUnknown` records the identity + a placeholder that a
    /// late real result reconciles in place.
    #[test]
    fn recover_outcome_unknown_records_reconcilable_placeholder() {
        use crate::chat::{ChatMessage, ToolCallRef};
        let mut s = session();
        s.begin_turn("t1", None, None, 7, s.scope_snapshot.clone(), "t")
            .unwrap();
        s.conversation.push(ChatMessage::assistant_tool_calls(
            "a1",
            String::new(),
            vec![ToolCallRef {
                id: "call-z".into(),
                name: "exec_command".into(),
                arguments_json: "{}".into(),
            }],
        ));
        s.recover_session(
            RecoveryVerdict::OutcomeUnknown {
                work_id: 9,
                execution_id: "exec-9".into(),
                exec_request_id: "rq-9".into(),
            },
            "t2",
        );
        assert_eq!(s.turn_state, TurnState::Failed);
        assert!(!s.execution_state.allows_new_mutation());
        // A late result for exec-9 reconciles the placeholder in place.
        assert!(s.reconcile_late_result("exec-9", "exit_code=0", "t3"));
        assert_eq!(s.execution_state, ExecutionState::None);
        let closed = s
            .conversation
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call-z"))
            .unwrap();
        assert_eq!(closed.text, "exit_code=0");
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
