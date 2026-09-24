//! Durable, provider-neutral limits and state transitions for a continued goal.

use crate::chat::ChatRole;
use crate::dynamic_run::{AGENT_RUN_EVENT_SCHEMA_VERSION, AgentRunEvent, AgentRunEventKind};
use crate::session::PersistedAgentSession;
use desk_agent_protocol::ai_assistant::goal_budget::{GoalBudgetLimits, GoalBudgetPolicy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const GOAL_SCHEMA_VERSION: u16 = 1;
pub const DEFAULT_ACTIVE_TIME_MS: u64 = 2 * 60 * 60 * 1_000;
pub const DEFAULT_DEADLINE_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
pub const DEFAULT_MODEL_TOKENS: u64 = 100_000;
pub const DEFAULT_MODEL_CALLS: u32 = 160;
pub const DEFAULT_TOOL_CALLS: u32 = 200;
pub const DEFAULT_SLICES: u32 = 20;
pub const DEFAULT_STALLED_SLICES: u32 = 3;
pub const GOAL_OPEN_REQUEST_TTL_MS: u64 = 24 * 60 * 60 * 1_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalOpening {
    OwnerRequest,
    AiRequestApproved {
        request_id: String,
        decision_event_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalModelBinding {
    pub connection_id: String,
    pub connection_revision: u64,
    pub profile_revision: u64,
    pub model_id: String,
}

impl GoalOpening {
    pub fn validate(&self) -> Result<(), GoalError> {
        if let Self::AiRequestApproved {
            request_id,
            decision_event_id,
        } = self
        {
            valid_id(request_id)?;
            valid_id(decision_event_id)?;
        }
        Ok(())
    }
}

impl GoalModelBinding {
    pub fn from_destination(
        destination: &desk_agent_protocol::data_lineage::DestinationIdentity,
    ) -> Result<Self, GoalError> {
        let desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id,
            connection_revision,
            model_id,
            profile_revision,
        } = destination
        else {
            return Err(GoalError::InvalidIdentity);
        };
        let binding = Self {
            connection_id: connection_id.clone(),
            connection_revision: *connection_revision,
            profile_revision: (*profile_revision)
                .try_into()
                .map_err(|_| GoalError::InvalidIdentity)?,
            model_id: model_id.clone(),
        };
        binding.validate()?;
        Ok(binding)
    }

    pub fn validate(&self) -> Result<(), GoalError> {
        valid_id(&self.connection_id)?;
        valid_id(&self.model_id)?;
        if self.connection_revision == 0 || self.profile_revision == 0 {
            return Err(GoalError::InvalidIdentity);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalOpenRequestState {
    Pending,
    Approved,
    Denied,
    Expired,
    Withdrawn,
}

impl GoalOpenRequestState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Expired => "expired",
            Self::Withdrawn => "withdrawn",
        }
    }

    pub fn from_code(code: &str) -> Result<Self, GoalError> {
        match code {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "denied" => Ok(Self::Denied),
            "expired" => Ok(Self::Expired),
            "withdrawn" => Ok(Self::Withdrawn),
            _ => Err(GoalError::InvalidState),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalOpenRequest {
    pub schema_version: u16,
    pub request_id: String,
    pub conversation_id: String,
    pub owner_id: String,
    pub device_id: String,
    pub source_message_id: String,
    pub input_revision: u64,
    pub goal_text: String,
    /// Present only when the proposal changes an existing, stopped goal.
    pub target_goal_id: Option<String>,
    pub target_goal_revision: Option<u64>,
    /// A new goal may continue an earlier completed goal without reopening it.
    pub previous_completed_goal_id: Option<String>,
    pub limits: GoalLimits,
    pub model_binding: GoalModelBinding,
    pub state: GoalOpenRequestState,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub decided_at_unix_ms: Option<u64>,
    pub decision_event_id: Option<String>,
    pub resulting_goal_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalOpenRequestEvent {
    pub event: AgentRunEvent,
    pub request: GoalOpenRequest,
}

impl GoalOpenRequestEvent {
    pub fn id_for(
        request_id: &str,
        state: GoalOpenRequestState,
        kind: AgentRunEventKind,
    ) -> Result<String, GoalError> {
        valid_id(request_id)?;
        if !matches!(
            kind,
            AgentRunEventKind::GoalOpenRequested | AgentRunEventKind::GoalOpenDecided
        ) {
            return Err(GoalError::InvalidState);
        }
        let identity = format!("{}:{}:{}", request_id, state.as_str(), kind.as_str());
        Ok(format!(
            "goal-open-event-{:x}",
            Sha256::digest(identity.as_bytes())
        ))
    }

    pub fn new(
        request: &GoalOpenRequest,
        kind: AgentRunEventKind,
        event_seq: u64,
        created_at: String,
    ) -> Result<Self, GoalError> {
        request.validate()?;
        if !matches!(
            kind,
            AgentRunEventKind::GoalOpenRequested | AgentRunEventKind::GoalOpenDecided
        ) {
            return Err(GoalError::InvalidState);
        }
        let value = Self {
            event: AgentRunEvent {
                schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                event_id: Self::id_for(&request.request_id, request.state, kind)?,
                run_id: request.conversation_id.clone(),
                event_seq,
                input_revision: request.input_revision,
                kind,
                correlation_id: Some(request.request_id.clone()),
                source_envelope_ids: Vec::new(),
                result_envelope_ids: Vec::new(),
                created_at,
            },
            request: request.clone(),
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), GoalError> {
        self.request.validate()?;
        self.event.validate().map_err(|_| GoalError::InvalidState)?;
        if self.event.run_id != self.request.conversation_id
            || self.event.input_revision != self.request.input_revision
            || self.event.correlation_id.as_deref() != Some(self.request.request_id.as_str())
            || (self.event.kind == AgentRunEventKind::GoalOpenDecided
                && self.request.decision_event_id.as_deref() != Some(self.event.event_id.as_str()))
            || !matches!(
                (self.event.kind, self.request.state),
                (
                    AgentRunEventKind::GoalOpenRequested,
                    GoalOpenRequestState::Pending
                ) | (
                    AgentRunEventKind::GoalOpenDecided,
                    GoalOpenRequestState::Approved
                ) | (
                    AgentRunEventKind::GoalOpenDecided,
                    GoalOpenRequestState::Denied
                ) | (
                    AgentRunEventKind::GoalOpenDecided,
                    GoalOpenRequestState::Expired
                ) | (
                    AgentRunEventKind::GoalOpenDecided,
                    GoalOpenRequestState::Withdrawn
                )
            )
        {
            return Err(GoalError::InvalidState);
        }
        Ok(())
    }
}

impl GoalOpenRequest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        request_id: String,
        conversation_id: String,
        owner_id: String,
        device_id: String,
        source_message_id: String,
        input_revision: u64,
        goal_text: String,
        limits: GoalLimits,
        model_binding: GoalModelBinding,
        created_at_unix_ms: u64,
    ) -> Result<Self, GoalError> {
        let expires_at_unix_ms = created_at_unix_ms
            .checked_add(GOAL_OPEN_REQUEST_TTL_MS)
            .ok_or(GoalError::ArithmeticOverflow)?;
        let request = Self {
            schema_version: GOAL_SCHEMA_VERSION,
            request_id,
            conversation_id,
            owner_id,
            device_id,
            source_message_id,
            input_revision,
            goal_text,
            target_goal_id: None,
            target_goal_revision: None,
            previous_completed_goal_id: None,
            limits,
            model_binding,
            state: GoalOpenRequestState::Pending,
            created_at_unix_ms,
            expires_at_unix_ms,
            decided_at_unix_ms: None,
            decision_event_id: None,
            resulting_goal_id: None,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), GoalError> {
        if self.schema_version != GOAL_SCHEMA_VERSION
            || self.input_revision == 0
            || self.created_at_unix_ms == 0
            || self.expires_at_unix_ms <= self.created_at_unix_ms
            || self.expires_at_unix_ms - self.created_at_unix_ms > GOAL_OPEN_REQUEST_TTL_MS
            || self.goal_text.trim().is_empty()
            || self.goal_text.len() > 16 * 1_024
            || self.target_goal_id.is_some() != self.target_goal_revision.is_some()
            || (self.target_goal_id.is_some() && self.previous_completed_goal_id.is_some())
            || self.target_goal_revision == Some(0)
        {
            return Err(GoalError::InvalidIdentity);
        }
        for id in [
            &self.request_id,
            &self.conversation_id,
            &self.owner_id,
            &self.device_id,
            &self.source_message_id,
        ] {
            valid_id(id)?;
        }
        if let Some(id) = &self.target_goal_id {
            valid_id(id)?;
        }
        if let Some(id) = &self.previous_completed_goal_id {
            valid_id(id)?;
        }
        self.limits.validate()?;
        self.model_binding.validate()?;
        match self.state {
            GoalOpenRequestState::Pending
                if self.decided_at_unix_ms.is_none()
                    && self.decision_event_id.is_none()
                    && self.resulting_goal_id.is_none() => {}
            GoalOpenRequestState::Approved
                if self.decided_at_unix_ms.is_some()
                    && self.decision_event_id.is_some()
                    && self.resulting_goal_id.is_some()
                    && self
                        .target_goal_id
                        .as_ref()
                        .is_none_or(|id| self.resulting_goal_id.as_ref() == Some(id)) => {}
            GoalOpenRequestState::Denied
            | GoalOpenRequestState::Expired
            | GoalOpenRequestState::Withdrawn
                if self.decided_at_unix_ms.is_some()
                    && self.decision_event_id.is_some()
                    && self.resulting_goal_id.is_none() => {}
            _ => return Err(GoalError::InvalidState),
        }
        if let Some(decided_at) = self.decided_at_unix_ms {
            if decided_at < self.created_at_unix_ms
                || (self.state != GoalOpenRequestState::Expired
                    && decided_at >= self.expires_at_unix_ms)
            {
                return Err(GoalError::InvalidState);
            }
        }
        if let Some(id) = &self.decision_event_id {
            valid_id(id)?;
        }
        if let Some(id) = &self.resulting_goal_id {
            valid_id(id)?;
        }
        Ok(())
    }

    pub fn approve(
        &mut self,
        current_input_revision: u64,
        current_model_binding: &GoalModelBinding,
        goal_id: String,
        decision_event_id: String,
        now_unix_ms: u64,
        previous_completed: Option<&GoalRun>,
    ) -> Result<GoalRun, GoalError> {
        if self.target_goal_id.is_some() {
            return Err(GoalError::InvalidState);
        }
        self.require_current(current_input_revision, current_model_binding, now_unix_ms)?;
        if self.previous_completed_goal_id.as_deref()
            != previous_completed.map(|goal| goal.goal_id.as_str())
        {
            return Err(GoalError::InvalidState);
        }
        valid_id(&goal_id)?;
        valid_id(&decision_event_id)?;
        let mut goal = GoalRun::new(
            goal_id.clone(),
            self.conversation_id.clone(),
            self.owner_id.clone(),
            self.device_id.clone(),
            self.goal_text.clone(),
            self.source_message_id.clone(),
            GoalOpening::AiRequestApproved {
                request_id: self.request_id.clone(),
                decision_event_id: decision_event_id.clone(),
            },
            self.model_binding.clone(),
            self.input_revision,
            now_unix_ms,
            self.limits,
        )?;
        if let Some(previous) = previous_completed {
            goal = goal.with_previous_completed(previous)?;
        }
        self.state = GoalOpenRequestState::Approved;
        self.decided_at_unix_ms = Some(now_unix_ms);
        self.decision_event_id = Some(decision_event_id);
        self.resulting_goal_id = Some(goal_id);
        self.validate()?;
        Ok(goal)
    }

    /// A revision is a new owner decision over the exact proposed text. It
    /// preserves the existing goal identity, usage and action history.
    pub fn approve_revision(
        &mut self,
        goal: &mut GoalRun,
        current_input_revision: u64,
        current_model_binding: &GoalModelBinding,
        decision_event_id: String,
        now_unix_ms: u64,
    ) -> Result<(), GoalError> {
        self.require_current(current_input_revision, current_model_binding, now_unix_ms)?;
        if self.target_goal_id.as_deref() != Some(goal.goal_id.as_str())
            || self.target_goal_revision != Some(goal.goal_revision)
            || self.conversation_id != goal.conversation_id
            || self.owner_id != goal.owner_id
            || self.device_id != goal.device_id
            || &goal.model_binding != current_model_binding
        {
            return Err(GoalError::StaleRevision);
        }
        valid_id(&decision_event_id)?;
        goal.approve_revision(
            &self.goal_text,
            &self.source_message_id,
            self.input_revision,
            now_unix_ms,
        )?;
        self.state = GoalOpenRequestState::Approved;
        self.decided_at_unix_ms = Some(now_unix_ms);
        self.decision_event_id = Some(decision_event_id);
        self.resulting_goal_id = Some(goal.goal_id.clone());
        self.validate()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_revision(
        request_id: String,
        goal: &GoalRun,
        source_message_id: String,
        input_revision: u64,
        goal_text: String,
        model_binding: GoalModelBinding,
        created_at_unix_ms: u64,
    ) -> Result<Self, GoalError> {
        goal.can_propose_revision(input_revision)?;
        if goal.model_binding != model_binding {
            return Err(GoalError::StaleRevision);
        }
        let mut request = Self::new(
            request_id,
            goal.conversation_id.clone(),
            goal.owner_id.clone(),
            goal.device_id.clone(),
            source_message_id,
            input_revision,
            goal_text,
            goal.limits,
            model_binding,
            created_at_unix_ms,
        )?;
        request.target_goal_id = Some(goal.goal_id.clone());
        request.target_goal_revision = Some(goal.goal_revision);
        request.validate()?;
        Ok(request)
    }

    pub fn close(
        &mut self,
        state: GoalOpenRequestState,
        decision_event_id: String,
        now_unix_ms: u64,
    ) -> Result<(), GoalError> {
        if self.state != GoalOpenRequestState::Pending
            || !matches!(
                state,
                GoalOpenRequestState::Denied
                    | GoalOpenRequestState::Expired
                    | GoalOpenRequestState::Withdrawn
            )
            || now_unix_ms < self.created_at_unix_ms
            || (state == GoalOpenRequestState::Expired && now_unix_ms < self.expires_at_unix_ms)
            || (state != GoalOpenRequestState::Expired && now_unix_ms >= self.expires_at_unix_ms)
        {
            return Err(GoalError::InvalidState);
        }
        valid_id(&decision_event_id)?;
        self.state = state;
        self.decided_at_unix_ms = Some(now_unix_ms);
        self.decision_event_id = Some(decision_event_id);
        self.validate()
    }

    fn require_current(
        &self,
        current_input_revision: u64,
        current_model_binding: &GoalModelBinding,
        now_unix_ms: u64,
    ) -> Result<(), GoalError> {
        self.validate()?;
        if self.state != GoalOpenRequestState::Pending
            || self.input_revision != current_input_revision
            || &self.model_binding != current_model_binding
        {
            return Err(GoalError::StaleRevision);
        }
        if now_unix_ms < self.created_at_unix_ms || now_unix_ms >= self.expires_at_unix_ms {
            return Err(GoalError::DeadlineReached);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalCheckpoint {
    pub segment_seq: u32,
    pub event_seq: u64,
    pub summary: String,
    pub evidence_ids: Vec<String>,
    pub protected_attachment_ids: Vec<String>,
}

/// Append-only session event for a goal transition. The original run id stays
/// the conversation id; goal and slice identities are nested in the payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalLedgerEvent {
    pub event: AgentRunEvent,
    pub goal_id: String,
    pub goal_revision: u64,
    pub slice_seq: u32,
    pub lease_epoch: u64,
    pub state_version: u64,
    pub resulting_state: GoalState,
}

impl GoalLedgerEvent {
    pub fn new(
        goal: &GoalRun,
        kind: AgentRunEventKind,
        event_seq: u64,
        created_at: String,
    ) -> Result<Self, GoalError> {
        if !matches!(
            kind,
            AgentRunEventKind::GoalOpened
                | AgentRunEventKind::GoalInputReceived
                | AgentRunEventKind::GoalRevised
                | AgentRunEventKind::GoalSliceClaimed
                | AgentRunEventKind::GoalSliceSettled
        ) {
            return Err(GoalError::InvalidState);
        }
        let identity = format!("{}:{}:{}", goal.goal_id, goal.state_version, kind.as_str());
        let event = Self {
            event: AgentRunEvent {
                schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                event_id: format!("goal-event-{:x}", Sha256::digest(identity.as_bytes())),
                run_id: goal.conversation_id.clone(),
                event_seq,
                input_revision: goal.input_revision,
                kind,
                correlation_id: Some(goal.goal_id.clone()),
                source_envelope_ids: Vec::new(),
                result_envelope_ids: Vec::new(),
                created_at,
            },
            goal_id: goal.goal_id.clone(),
            goal_revision: goal.goal_revision,
            slice_seq: goal.slice_seq,
            lease_epoch: goal.lease_epoch,
            state_version: goal.state_version,
            resulting_state: goal.state,
        };
        event.validate_for(goal)?;
        Ok(event)
    }

    pub fn validate_for(&self, goal: &GoalRun) -> Result<(), GoalError> {
        self.event.validate().map_err(|_| GoalError::InvalidState)?;
        if self.event.run_id != goal.conversation_id
            || self.event.input_revision != goal.input_revision
            || self.event.correlation_id.as_deref() != Some(goal.goal_id.as_str())
            || self.goal_id != goal.goal_id
            || self.goal_revision != goal.goal_revision
            || self.slice_seq != goal.slice_seq
            || self.lease_epoch != goal.lease_epoch
            || self.state_version != goal.state_version
            || self.resulting_state != goal.state
            || !matches!(
                self.event.kind,
                AgentRunEventKind::GoalOpened
                    | AgentRunEventKind::GoalInputReceived
                    | AgentRunEventKind::GoalRevised
                    | AgentRunEventKind::GoalSliceClaimed
                    | AgentRunEventKind::GoalSliceSettled
            )
            || (self.event.kind == AgentRunEventKind::GoalOpened
                && (self.slice_seq != 0 || self.resulting_state != GoalState::Queued))
            || (self.event.kind == AgentRunEventKind::GoalSliceClaimed
                && self.resulting_state != GoalState::Running)
            || (matches!(
                self.event.kind,
                AgentRunEventKind::GoalRevised | AgentRunEventKind::GoalInputReceived
            ) && self.resulting_state == GoalState::Running)
            || (self.event.kind == AgentRunEventKind::GoalSliceSettled
                && self.resulting_state == GoalState::Running)
        {
            return Err(GoalError::InvalidState);
        }
        Ok(())
    }
}

impl GoalCheckpoint {
    pub fn validate(&self) -> Result<(), GoalError> {
        if self.segment_seq == 0
            || self.event_seq == 0
            || self.summary.trim().is_empty()
            || self.summary.len() > 4_096
            || self.evidence_ids.len() > 32
            || self.protected_attachment_ids.len() > 32
        {
            return Err(GoalError::InvalidIdentity);
        }
        for id in self
            .evidence_ids
            .iter()
            .chain(&self.protected_attachment_ids)
        {
            valid_id(id)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalLimits {
    pub active_time_ms: u64,
    pub model_tokens: u64,
    pub model_calls: u32,
    pub tool_calls: u32,
    pub slices: u32,
    pub stalled_slices: u32,
}

impl Default for GoalLimits {
    fn default() -> Self {
        Self {
            active_time_ms: DEFAULT_ACTIVE_TIME_MS,
            model_tokens: DEFAULT_MODEL_TOKENS,
            model_calls: DEFAULT_MODEL_CALLS,
            tool_calls: DEFAULT_TOOL_CALLS,
            slices: DEFAULT_SLICES,
            stalled_slices: DEFAULT_STALLED_SLICES,
        }
    }
}

impl GoalLimits {
    /// Maximum platform-configurable budget for one goal.
    pub const fn policy_ceiling() -> Self {
        Self {
            active_time_ms: 24 * 60 * 60 * 1_000,
            model_tokens: 2_000_000,
            model_calls: 1_000,
            tool_calls: 2_000,
            slices: 200,
            stalled_slices: 10,
        }
    }

    pub fn validate(self) -> Result<(), GoalError> {
        if self.active_time_ms == 0
            || self.model_tokens == 0
            || self.model_calls == 0
            || self.tool_calls == 0
            || self.slices == 0
            || self.stalled_slices == 0
        {
            return Err(GoalError::InvalidLimits);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub model_calls: u32,
    pub tool_calls: u32,
    pub slices: u32,
    pub active_time_ms: u64,
}

impl GoalUsage {
    pub fn within(self, upper: Self) -> bool {
        self.total_tokens()
            .zip(upper.total_tokens())
            .is_some_and(|(actual, maximum)| actual <= maximum)
            && self.model_calls <= upper.model_calls
            && self.tool_calls <= upper.tool_calls
            && self.slices <= upper.slices
            && self.active_time_ms <= upper.active_time_ms
    }

    pub fn total_tokens(self) -> Option<u64> {
        self.input_tokens
            .checked_add(self.output_tokens)?
            .checked_add(self.cache_read_tokens)?
            .checked_add(self.cache_write_tokens)
    }

    pub fn checked_add(self, delta: Self) -> Option<Self> {
        Some(Self {
            input_tokens: self.input_tokens.checked_add(delta.input_tokens)?,
            output_tokens: self.output_tokens.checked_add(delta.output_tokens)?,
            cache_read_tokens: self
                .cache_read_tokens
                .checked_add(delta.cache_read_tokens)?,
            cache_write_tokens: self
                .cache_write_tokens
                .checked_add(delta.cache_write_tokens)?,
            model_calls: self.model_calls.checked_add(delta.model_calls)?,
            tool_calls: self.tool_calls.checked_add(delta.tool_calls)?,
            slices: self.slices.checked_add(delta.slices)?,
            active_time_ms: self.active_time_ms.checked_add(delta.active_time_ms)?,
        })
    }

    pub fn fits(self, limits: GoalLimits) -> bool {
        self.total_tokens()
            .is_some_and(|tokens| tokens <= limits.model_tokens)
            && self.model_calls <= limits.model_calls
            && self.tool_calls <= limits.tool_calls
            && self.slices <= limits.slices
            && self.active_time_ms <= limits.active_time_ms
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPauseReason {
    Owner,
    Budget,
    Stalled,
    Recovery,
    ContextTooSmall,
    AttachmentMissing,
    AttachmentCapacity,
    NeedsNextStep,
}

impl GoalPauseReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Budget => "budget",
            Self::Stalled => "stalled",
            Self::Recovery => "recovery",
            Self::ContextTooSmall => "context_too_small",
            Self::AttachmentMissing => "attachment_missing",
            Self::AttachmentCapacity => "attachment_capacity",
            Self::NeedsNextStep => "needs_next_step",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalWaitReason {
    Approval,
    Work,
    Device,
    Model,
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum GoalState {
    Queued,
    Running,
    Waiting(GoalWaitReason),
    Paused(GoalPauseReason),
    Blocked,
    Completed,
    Failed,
    Cancelled,
}

impl GoalState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    pub fn can_claim(self) -> bool {
        matches!(self, Self::Queued)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalError {
    InvalidIdentity,
    InvalidLimits,
    InvalidState,
    BudgetExceeded,
    DeadlineReached,
    StaleRevision,
    StaleLease,
    ArithmeticOverflow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalControl {
    Continue {
        progress: String,
        next_step: String,
    },
    Wait {
        reason: GoalWaitReason,
        reference_id: String,
    },
    Complete {
        evidence_ids: Vec<String>,
        summary: String,
    },
    Blocked {
        reason: String,
    },
}

/// The model can submit only `Control`; the runtime may settle a segment into
/// a fail-closed pause after protocol, budget, or lease-independent failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalSegmentEnd {
    Control(GoalControl),
    Pause(GoalPauseReason),
    /// Only the runtime can classify an upstream model failure as transient.
    WaitForModel {
        retry_after_unix_ms: Option<u64>,
    },
    /// A permanent provider rejection cannot be retried with the same binding.
    BlockModel,
}

/// Owner actions are distinct from the model's segment-ending control tool.
/// They never authorize a device operation or reset cumulative usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalOwnerAction {
    Pause,
    Resume,
    RetryStalled,
    Cancel,
}

/// A decided permission either returns control to the goal coordinator or
/// remains stopped under an owner pause/cancellation. Only conversations with
/// no goal use the ordinary permission-resume path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalPermissionWake {
    NoGoal,
    Queued,
    Held,
}

impl GoalControl {
    pub fn validate(&self) -> Result<(), GoalError> {
        let bounded = |value: &str| !value.trim().is_empty() && value.len() <= 2_048;
        match self {
            Self::Continue {
                progress,
                next_step,
            } if bounded(progress) && bounded(next_step) => {}
            Self::Wait {
                reason: GoalWaitReason::Approval | GoalWaitReason::Work | GoalWaitReason::User,
                reference_id,
            } if bounded(reference_id) => {}
            Self::Complete {
                evidence_ids,
                summary,
            } if bounded(summary)
                && !evidence_ids.is_empty()
                && evidence_ids.len() <= 32
                && evidence_ids.iter().all(|value| bounded(value)) => {}
            Self::Blocked { reason } if bounded(reason) => {}
            _ => return Err(GoalError::InvalidIdentity),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalRun {
    pub schema_version: u16,
    pub goal_id: String,
    pub conversation_id: String,
    pub owner_id: String,
    pub device_id: String,
    pub goal_text: String,
    pub source_message_id: String,
    /// Immutable owner-approved opening text and its original evidence.
    pub original_goal_text: String,
    pub original_source_message_id: String,
    pub opening: GoalOpening,
    pub previous_completion: Option<GoalCompletionReference>,
    pub model_binding: GoalModelBinding,
    pub input_revision: u64,
    pub goal_revision: u64,
    /// Input revision at which the assistant stopped to ask for clarification.
    /// A proposal requires a later, persisted owner message.
    pub user_reply_floor_revision: Option<u64>,
    pub state: GoalState,
    pub state_version: u64,
    pub slice_seq: u32,
    pub limits: GoalLimits,
    pub used: GoalUsage,
    pub reserved: GoalUsage,
    /// Bounded in-flight reservations keyed by a stable dispatch identity.
    pub budget_reservations: BTreeMap<String, GoalUsage>,
    /// Idempotent settlement identities; only usage counters are retained.
    pub budget_settlements: BTreeMap<String, GoalUsage>,
    /// Digests of distinct, server-observed successful tool results. This keeps
    /// repeated reads from resetting the stalled-segment guard after compaction.
    pub seen_result_fingerprints: BTreeSet<String>,
    pub stalled_slices: u32,
    pub created_at_unix_ms: u64,
    pub deadline_unix_ms: u64,
    pub next_attempt_unix_ms: Option<u64>,
    /// Consecutive offline observations, used only for bounded device retry.
    pub device_wait_attempts: u8,
    /// Preserved across due-time wake and claim until a model response succeeds.
    pub model_wait_attempts: u8,
    pub lease_epoch: u64,
    pub status_reason: Option<String>,
    pub checkpoint: Option<GoalCheckpoint>,
    pub last_progress_event_seq: Option<u64>,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalCompletionReference {
    pub goal_id: String,
    pub event_seq: u64,
    pub summary: String,
    pub evidence_ids: Vec<String>,
    pub protected_attachment_ids: Vec<String>,
}

impl GoalCompletionReference {
    fn validate(&self) -> Result<(), GoalError> {
        valid_id(&self.goal_id)?;
        if self.event_seq == 0
            || self.summary.trim().is_empty()
            || self.summary.len() > 2_048
            || self.evidence_ids.is_empty()
            || self.evidence_ids.len() > 32
            || self.protected_attachment_ids.len() > 32
        {
            return Err(GoalError::InvalidIdentity);
        }
        for id in &self.evidence_ids {
            if id.trim().is_empty() || id.len() > 2_048 {
                return Err(GoalError::InvalidIdentity);
            }
        }
        for id in &self.protected_attachment_ids {
            valid_id(id)?;
        }
        Ok(())
    }

    pub fn from_completed(
        previous: &GoalRun,
        conversation_id: &str,
        owner_id: &str,
        device_id: &str,
    ) -> Result<Self, GoalError> {
        previous.validate()?;
        if previous.state != GoalState::Completed
            || previous.conversation_id != conversation_id
            || previous.owner_id != owner_id
            || previous.device_id != device_id
        {
            return Err(GoalError::InvalidState);
        }
        let checkpoint = previous
            .checkpoint
            .as_ref()
            .ok_or(GoalError::InvalidState)?;
        let reference = Self {
            goal_id: previous.goal_id.clone(),
            event_seq: checkpoint.event_seq,
            summary: checkpoint.summary.clone(),
            evidence_ids: checkpoint.evidence_ids.clone(),
            protected_attachment_ids: checkpoint.protected_attachment_ids.clone(),
        };
        reference.validate()?;
        Ok(reference)
    }
}

impl GoalRun {
    pub fn owns_waited_work(&self, task_id: &str) -> bool {
        self.checkpoint.as_ref().is_some_and(|checkpoint| {
            checkpoint.summary.strip_prefix("Waiting for ") == Some(task_id)
                && checkpoint.evidence_ids.iter().any(|id| id == task_id)
        })
    }

    pub fn owned_pending_work_event_ids(&self, session: &PersistedAgentSession) -> Vec<String> {
        session
            .pending_auto_triggers
            .iter()
            .filter(|pending| {
                session.conversation.iter().any(|message| {
                    message.message_id == pending.event_id
                        && message
                            .background_task_id
                            .as_deref()
                            .is_some_and(|task_id| self.owns_waited_work(task_id))
                        && message.pending_delivery_format.is_some()
                })
            })
            .map(|pending| pending.event_id.clone())
            .collect()
    }

    /// A work wakeup requires the server's settled completion receipt for the
    /// exact task this goal is waiting on. Model text and an initial dispatch
    /// placeholder are not completion evidence.
    pub fn completed_work_event_id<'a>(
        &self,
        session: &'a PersistedAgentSession,
    ) -> Option<&'a str> {
        if self.state != GoalState::Waiting(GoalWaitReason::Work)
            || session.input_revision != self.input_revision
        {
            return None;
        }
        let task_id = self.status_reason.as_deref()?;
        if session
            .execution_state
            .tasks()
            .iter()
            .any(|task| task.action_request_id == task_id)
        {
            return None;
        }
        session
            .conversation
            .iter()
            .find(|message| {
                message.background_task_id.as_deref() == Some(task_id)
                    && matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput)
                    && message.pending_delivery_format.is_some()
            })
            .map(|message| message.message_id.as_str())
    }

    pub fn validate(&self) -> Result<(), GoalError> {
        if self.schema_version != GOAL_SCHEMA_VERSION
            || self.input_revision == 0
            || self.goal_revision == 0
            || self
                .user_reply_floor_revision
                .is_some_and(|floor| floor > self.input_revision)
            || self.state_version == 0
            || self.created_at_unix_ms == 0
            || self.updated_at_unix_ms < self.created_at_unix_ms
            || self.deadline_unix_ms <= self.created_at_unix_ms
            || self.used.checked_add(self.reserved).is_none()
            || (self.state == GoalState::Running) != (self.reserved.slices == 1)
            || self.device_wait_attempts > 7
            || self.model_wait_attempts > 7
            || (self.state == GoalState::Waiting(GoalWaitReason::Device))
                != (self.device_wait_attempts > 0)
            || (self.state == GoalState::Waiting(GoalWaitReason::Model)
                && self.model_wait_attempts == 0)
            || (self.model_wait_attempts > 0
                && !matches!(
                    self.state,
                    GoalState::Queued
                        | GoalState::Running
                        | GoalState::Waiting(GoalWaitReason::Model)
                ))
        {
            return Err(GoalError::InvalidState);
        }
        if self.budget_reservations.len() > 512 || self.budget_settlements.len() > 512 {
            return Err(GoalError::InvalidState);
        }
        if self.seen_result_fingerprints.len() > 512
            || self.seen_result_fingerprints.iter().any(|fingerprint| {
                fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return Err(GoalError::InvalidState);
        }
        let mut reserved = GoalUsage {
            slices: self.reserved.slices,
            ..GoalUsage::default()
        };
        for (id, usage) in &self.budget_reservations {
            valid_id(id)?;
            if usage.slices != 0 || self.budget_settlements.contains_key(id) {
                return Err(GoalError::InvalidState);
            }
            reserved = reserved
                .checked_add(*usage)
                .ok_or(GoalError::ArithmeticOverflow)?;
        }
        if reserved != self.reserved {
            return Err(GoalError::InvalidState);
        }
        for id in self.budget_settlements.keys() {
            valid_id(id)?;
        }
        for id in [
            &self.goal_id,
            &self.conversation_id,
            &self.owner_id,
            &self.device_id,
            &self.source_message_id,
            &self.original_source_message_id,
        ] {
            valid_id(id)?;
        }
        self.opening.validate()?;
        if let Some(previous) = &self.previous_completion {
            previous.validate()?;
            if previous.goal_id == self.goal_id {
                return Err(GoalError::InvalidIdentity);
            }
        }
        self.model_binding.validate()?;
        self.limits.validate()?;
        if self.goal_text.trim().is_empty()
            || self.goal_text.len() > 16 * 1_024
            || self.original_goal_text.trim().is_empty()
            || self.original_goal_text.len() > 16 * 1_024
        {
            return Err(GoalError::InvalidIdentity);
        }
        if let Some(checkpoint) = &self.checkpoint {
            checkpoint.validate()?;
            if checkpoint.segment_seq > self.slice_seq {
                return Err(GoalError::InvalidState);
            }
        }
        Ok(())
    }

    pub fn status_code(&self) -> &'static str {
        match self.state {
            GoalState::Queued => "queued",
            GoalState::Running => "running",
            GoalState::Waiting(GoalWaitReason::Approval) => "waiting_approval",
            GoalState::Waiting(GoalWaitReason::Work) => "waiting_work",
            GoalState::Waiting(GoalWaitReason::Device) => "waiting_device",
            GoalState::Waiting(GoalWaitReason::Model) => "waiting_model",
            GoalState::Waiting(GoalWaitReason::User) => "waiting_user",
            GoalState::Paused(_) => "paused",
            GoalState::Blocked => "blocked",
            GoalState::Completed => "completed",
            GoalState::Failed => "failed",
            GoalState::Cancelled => "cancelled",
        }
    }

    pub fn new(
        goal_id: String,
        conversation_id: String,
        owner_id: String,
        device_id: String,
        goal_text: String,
        source_message_id: String,
        opening: GoalOpening,
        model_binding: GoalModelBinding,
        input_revision: u64,
        created_at_unix_ms: u64,
        limits: GoalLimits,
    ) -> Result<Self, GoalError> {
        limits.validate()?;
        for id in [
            &goal_id,
            &conversation_id,
            &owner_id,
            &device_id,
            &source_message_id,
        ] {
            valid_id(id)?;
        }
        model_binding.validate()?;
        opening.validate()?;
        if goal_text.trim().is_empty()
            || goal_text.len() > 16 * 1_024
            || input_revision == 0
            || created_at_unix_ms == 0
        {
            return Err(GoalError::InvalidIdentity);
        }
        let deadline_unix_ms = created_at_unix_ms
            .checked_add(DEFAULT_DEADLINE_MS)
            .ok_or(GoalError::ArithmeticOverflow)?;
        Ok(Self {
            schema_version: GOAL_SCHEMA_VERSION,
            goal_id,
            conversation_id,
            owner_id,
            device_id,
            original_goal_text: goal_text.clone(),
            original_source_message_id: source_message_id.clone(),
            goal_text,
            source_message_id,
            opening,
            previous_completion: None,
            model_binding,
            input_revision,
            goal_revision: 1,
            user_reply_floor_revision: None,
            state: GoalState::Queued,
            state_version: 1,
            slice_seq: 0,
            limits,
            used: GoalUsage::default(),
            reserved: GoalUsage::default(),
            budget_reservations: BTreeMap::new(),
            budget_settlements: BTreeMap::new(),
            seen_result_fingerprints: BTreeSet::new(),
            stalled_slices: 0,
            created_at_unix_ms,
            deadline_unix_ms,
            next_attempt_unix_ms: None,
            device_wait_attempts: 0,
            model_wait_attempts: 0,
            lease_epoch: 0,
            status_reason: None,
            checkpoint: None,
            last_progress_event_seq: None,
            updated_at_unix_ms: created_at_unix_ms,
        })
    }

    /// Bind a new run to the immutable outcome of a completed run in the same
    /// owner conversation. The caller loads the predecessor under its write
    /// transaction so a model or browser cannot forge the completion result.
    pub fn with_previous_completed(mut self, previous: &GoalRun) -> Result<Self, GoalError> {
        self.validate()?;
        if self.previous_completion.is_some() || previous.goal_id == self.goal_id {
            return Err(GoalError::InvalidState);
        }
        self.previous_completion = Some(GoalCompletionReference::from_completed(
            previous,
            &self.conversation_id,
            &self.owner_id,
            &self.device_id,
        )?);
        self.validate()?;
        Ok(self)
    }

    pub fn available_for(&self, delta: GoalUsage) -> Result<(), GoalError> {
        let projected = self
            .used
            .checked_add(self.reserved)
            .and_then(|total| total.checked_add(delta))
            .ok_or(GoalError::ArithmeticOverflow)?;
        if projected.fits(self.limits) {
            Ok(())
        } else {
            Err(GoalError::BudgetExceeded)
        }
    }

    pub fn next_slice_budget_available(&self) -> bool {
        self.available_for(GoalUsage {
            input_tokens: 1,
            model_calls: 1,
            slices: 1,
            active_time_ms: 1,
            ..GoalUsage::default()
        })
        .is_ok()
    }

    /// Refresh effective platform limits without resetting already incurred
    /// usage. Lowered limits stop the next reservation or segment claim.
    pub fn apply_budget_policy(&mut self, policy: &GoalBudgetPolicy) -> Result<(), GoalError> {
        self.limits = crate::goal_budget::effective_limits(policy)?;
        self.deadline_unix_ms =
            crate::goal_budget::deadline_unix_ms(policy, self.created_at_unix_ms)?;
        self.validate()
    }

    pub fn effective_budget_limits(&self) -> GoalBudgetLimits {
        GoalBudgetLimits {
            active_time_ms: (self.limits.active_time_ms != u64::MAX)
                .then_some(self.limits.active_time_ms),
            deadline_ms: (self.deadline_unix_ms != i64::MAX as u64).then_some(
                self.deadline_unix_ms
                    .saturating_sub(self.created_at_unix_ms),
            ),
            model_tokens: (self.limits.model_tokens != u64::MAX)
                .then_some(self.limits.model_tokens),
            model_calls: (self.limits.model_calls != u32::MAX).then_some(self.limits.model_calls),
            tool_calls: (self.limits.tool_calls != u32::MAX).then_some(self.limits.tool_calls),
            slices: (self.limits.slices != u32::MAX).then_some(self.limits.slices),
            stalled_slices: (self.limits.stalled_slices != u32::MAX)
                .then_some(self.limits.stalled_slices),
        }
    }

    pub fn reserve_with_id(
        &mut self,
        id: &str,
        delta: GoalUsage,
        now_unix_ms: u64,
    ) -> Result<bool, GoalError> {
        valid_id(id)?;
        if self.state != GoalState::Running
            || now_unix_ms < self.updated_at_unix_ms
            || now_unix_ms >= self.deadline_unix_ms
            || delta.slices != 0
            || delta == GoalUsage::default()
            || self.budget_reservations.len() >= 512
        {
            return Err(GoalError::InvalidState);
        }
        if let Some(existing) = self.budget_reservations.get(id) {
            return if *existing == delta {
                Ok(false)
            } else {
                Err(GoalError::InvalidState)
            };
        }
        if self.budget_settlements.contains_key(id) {
            return Err(GoalError::InvalidState);
        }
        self.available_for(delta)?;
        self.reserved = self
            .reserved
            .checked_add(delta)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.budget_reservations.insert(id.to_owned(), delta);
        self.state_version = self
            .state_version
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.updated_at_unix_ms = now_unix_ms;
        Ok(true)
    }

    pub fn settle_with_id(
        &mut self,
        id: &str,
        actual: GoalUsage,
        now_unix_ms: u64,
    ) -> Result<bool, GoalError> {
        valid_id(id)?;
        if let Some(existing) = self.budget_settlements.get(id) {
            return if *existing == actual {
                Ok(false)
            } else {
                Err(GoalError::InvalidState)
            };
        }
        if self.state != GoalState::Running
            || now_unix_ms < self.updated_at_unix_ms
            || self.budget_settlements.len() >= 512
        {
            return Err(GoalError::InvalidState);
        }
        let reserved = *self
            .budget_reservations
            .get(id)
            .ok_or(GoalError::InvalidState)?;
        if !actual.within(reserved) {
            return Err(GoalError::BudgetExceeded);
        }
        let remaining = self
            .reserved
            .checked_sub(reserved)
            .ok_or(GoalError::InvalidState)?;
        let next = self
            .used
            .checked_add(actual)
            .ok_or(GoalError::ArithmeticOverflow)?;
        next.checked_add(remaining)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.reserved = remaining;
        self.used = next;
        self.budget_reservations.remove(id);
        self.budget_settlements.insert(id.to_owned(), actual);
        self.state_version = self
            .state_version
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.updated_at_unix_ms = now_unix_ms;
        Ok(true)
    }

    pub fn claim_slice(&mut self, now_unix_ms: u64) -> Result<(u32, u64), GoalError> {
        if !self.state.can_claim() {
            return Err(GoalError::InvalidState);
        }
        if now_unix_ms >= self.deadline_unix_ms {
            return Err(GoalError::DeadlineReached);
        }
        if self
            .next_attempt_unix_ms
            .is_some_and(|next| now_unix_ms < next)
        {
            return Err(GoalError::InvalidState);
        }
        if !self.next_slice_budget_available() {
            return Err(GoalError::BudgetExceeded);
        }
        let slice_seq = self
            .slice_seq
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        let lease_epoch = self
            .lease_epoch
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        let reserved_slices = self
            .reserved
            .slices
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        let state_version = self
            .state_version
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.slice_seq = slice_seq;
        self.lease_epoch = lease_epoch;
        self.reserved.slices = reserved_slices;
        self.state = GoalState::Running;
        self.state_version = state_version;
        self.status_reason = None;
        self.updated_at_unix_ms = now_unix_ms;
        Ok((self.slice_seq, self.lease_epoch))
    }

    pub fn require_fence(
        &self,
        input_revision: u64,
        goal_revision: u64,
        lease_epoch: u64,
    ) -> Result<(), GoalError> {
        if self.input_revision != input_revision || self.goal_revision != goal_revision {
            return Err(GoalError::StaleRevision);
        }
        if self.state != GoalState::Running || self.lease_epoch != lease_epoch {
            return Err(GoalError::StaleLease);
        }
        Ok(())
    }

    /// Deduplicate observed results over the entire goal, including after
    /// conversation-history compression. The caller must derive these digests
    /// from actual settled tool receipts, never the model's progress statement.
    pub fn observe_result_fingerprints(
        &mut self,
        fingerprints: &[String],
    ) -> Result<bool, GoalError> {
        if self.state != GoalState::Running {
            return Err(GoalError::InvalidState);
        }
        let mut newly_seen = false;
        for fingerprint in fingerprints {
            if fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(GoalError::InvalidIdentity);
            }
            if !self.seen_result_fingerprints.contains(fingerprint) {
                if self.seen_result_fingerprints.len() >= 512 {
                    return Err(GoalError::BudgetExceeded);
                }
                self.seen_result_fingerprints.insert(fingerprint.clone());
                newly_seen = true;
            }
        }
        Ok(newly_seen)
    }

    pub fn finish_slice(
        &mut self,
        input_revision: u64,
        goal_revision: u64,
        lease_epoch: u64,
        control: &GoalControl,
        progressed: bool,
        no_pending_work: bool,
        checkpoint_event_seq: u64,
        protected_attachment_ids: Vec<String>,
        now_unix_ms: u64,
    ) -> Result<(), GoalError> {
        self.require_fence(input_revision, goal_revision, lease_epoch)?;
        control.validate()?;
        if self.reserved.slices != 1 || !self.budget_reservations.is_empty() {
            return Err(GoalError::InvalidState);
        }
        if matches!(control, GoalControl::Complete { .. }) && !no_pending_work {
            return Err(GoalError::InvalidState);
        }
        if now_unix_ms < self.updated_at_unix_ms
            || checkpoint_event_seq == 0
            || self
                .checkpoint
                .as_ref()
                .is_some_and(|last| checkpoint_event_seq <= last.event_seq)
        {
            return Err(GoalError::InvalidState);
        }
        let (summary, evidence_ids) = match control {
            GoalControl::Continue { progress, .. } => (progress.clone(), Vec::new()),
            GoalControl::Wait { reference_id, .. } => (
                format!("Waiting for {reference_id}"),
                vec![reference_id.clone()],
            ),
            GoalControl::Complete {
                evidence_ids,
                summary,
            } => (summary.clone(), evidence_ids.clone()),
            GoalControl::Blocked { reason } => (reason.clone(), Vec::new()),
        };
        let checkpoint = GoalCheckpoint {
            segment_seq: self.slice_seq,
            event_seq: checkpoint_event_seq,
            summary,
            evidence_ids,
            protected_attachment_ids,
        };
        checkpoint.validate()?;
        let used_slices = self
            .used
            .slices
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        let state_version = self
            .state_version
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        let stalled_slices = if progressed || !matches!(control, GoalControl::Continue { .. }) {
            self.stalled_slices
        } else {
            self.stalled_slices
                .checked_add(1)
                .ok_or(GoalError::ArithmeticOverflow)?
        };
        self.used.slices = used_slices;
        self.reserved.slices = 0;
        self.state_version = state_version;
        self.stalled_slices = if progressed { 0 } else { stalled_slices };
        let next_slice_budget_available = self.next_slice_budget_available();
        self.state = match control {
            GoalControl::Continue { .. } if !next_slice_budget_available => {
                GoalState::Paused(GoalPauseReason::Budget)
            }
            GoalControl::Continue { .. } if self.stalled_slices >= self.limits.stalled_slices => {
                GoalState::Paused(GoalPauseReason::Stalled)
            }
            GoalControl::Continue { .. } => GoalState::Queued,
            GoalControl::Wait { reason, .. } => GoalState::Waiting(*reason),
            GoalControl::Complete { .. } => GoalState::Completed,
            GoalControl::Blocked { .. } => GoalState::Blocked,
        };
        if self.state == GoalState::Waiting(GoalWaitReason::User) {
            self.user_reply_floor_revision = Some(self.input_revision);
        }
        if self.state == GoalState::Waiting(GoalWaitReason::Device) {
            self.device_wait_attempts = 1;
            self.next_attempt_unix_ms = Some(self.next_device_check(now_unix_ms));
        } else {
            self.device_wait_attempts = 0;
            self.next_attempt_unix_ms = None;
        }
        self.model_wait_attempts = 0;
        self.status_reason = match control {
            GoalControl::Continue { next_step, .. } => Some(next_step.clone()),
            GoalControl::Wait { reference_id, .. } => Some(reference_id.clone()),
            GoalControl::Complete { summary, .. } => Some(summary.clone()),
            GoalControl::Blocked { reason } => Some(reason.clone()),
        };
        if self.state == GoalState::Paused(GoalPauseReason::Budget) {
            self.status_reason = Some("budget_exhausted".into());
        }
        self.checkpoint = Some(checkpoint);
        if progressed {
            self.last_progress_event_seq = Some(checkpoint_event_seq);
        }
        self.updated_at_unix_ms = now_unix_ms;
        Ok(())
    }

    pub fn finish_paused_slice(
        &mut self,
        input_revision: u64,
        goal_revision: u64,
        lease_epoch: u64,
        reason: GoalPauseReason,
        checkpoint_event_seq: u64,
        protected_attachment_ids: Vec<String>,
        now_unix_ms: u64,
    ) -> Result<(), GoalError> {
        self.require_fence(input_revision, goal_revision, lease_epoch)?;
        if self.reserved.slices != 1
            || !self.budget_reservations.is_empty()
            || now_unix_ms < self.updated_at_unix_ms
            || checkpoint_event_seq == 0
            || self
                .checkpoint
                .as_ref()
                .is_some_and(|last| checkpoint_event_seq <= last.event_seq)
        {
            return Err(GoalError::InvalidState);
        }
        let checkpoint = GoalCheckpoint {
            segment_seq: self.slice_seq,
            event_seq: checkpoint_event_seq,
            summary: format!("Segment paused: {}", reason.as_str()),
            evidence_ids: Vec::new(),
            protected_attachment_ids,
        };
        checkpoint.validate()?;
        self.used.slices = self
            .used
            .slices
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.reserved.slices = 0;
        self.state_version = self
            .state_version
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.state = GoalState::Paused(reason);
        self.status_reason = Some(reason.as_str().into());
        self.next_attempt_unix_ms = None;
        self.model_wait_attempts = 0;
        self.checkpoint = Some(checkpoint);
        self.updated_at_unix_ms = now_unix_ms;
        Ok(())
    }

    pub fn finish_model_wait_slice(
        &mut self,
        input_revision: u64,
        goal_revision: u64,
        lease_epoch: u64,
        retry_after_unix_ms: Option<u64>,
        checkpoint_event_seq: u64,
        protected_attachment_ids: Vec<String>,
        now_unix_ms: u64,
    ) -> Result<(), GoalError> {
        let prior_attempts = self.model_wait_attempts;
        self.finish_paused_slice(
            input_revision,
            goal_revision,
            lease_epoch,
            GoalPauseReason::Recovery,
            checkpoint_event_seq,
            protected_attachment_ids,
            now_unix_ms,
        )?;
        if now_unix_ms >= self.deadline_unix_ms {
            self.state = GoalState::Failed;
            self.status_reason = Some("absolute_deadline_reached".into());
        } else if self
            .available_for(GoalUsage {
                slices: 1,
                model_calls: 1,
                ..GoalUsage::default()
            })
            .is_err()
        {
            self.state = GoalState::Paused(GoalPauseReason::Budget);
            self.status_reason = Some("budget_exhausted".into());
        } else {
            self.model_wait_attempts = prior_attempts.saturating_add(1).clamp(1, 7);
            self.state = GoalState::Waiting(GoalWaitReason::Model);
            self.next_attempt_unix_ms =
                Some(self.next_model_check(now_unix_ms, retry_after_unix_ms));
            self.status_reason = Some("model_temporarily_unavailable".into());
        }
        if let Some(checkpoint) = &mut self.checkpoint {
            checkpoint.summary = self.status_reason.clone().ok_or(GoalError::InvalidState)?;
        }
        self.validate()?;
        Ok(())
    }

    pub fn finish_blocked_model_slice(
        &mut self,
        input_revision: u64,
        goal_revision: u64,
        lease_epoch: u64,
        checkpoint_event_seq: u64,
        protected_attachment_ids: Vec<String>,
        now_unix_ms: u64,
    ) -> Result<(), GoalError> {
        self.finish_paused_slice(
            input_revision,
            goal_revision,
            lease_epoch,
            GoalPauseReason::Recovery,
            checkpoint_event_seq,
            protected_attachment_ids,
            now_unix_ms,
        )?;
        self.state = GoalState::Blocked;
        self.status_reason = Some("model_request_rejected".into());
        if let Some(checkpoint) = &mut self.checkpoint {
            checkpoint.summary = "model_request_rejected".into();
        }
        self.validate()?;
        Ok(())
    }

    /// Fence a crashed segment after the action ledger has settled its session.
    /// Unknown usage is charged at the reserved upper bound. A reconciled,
    /// result-known session can be queued for a new segment; otherwise it stays
    /// paused for an owner to resolve the unknown action.
    pub fn recover_interrupted_slice(
        &mut self,
        safe_to_resume: bool,
        now_unix_ms: u64,
    ) -> Result<(), GoalError> {
        if self.state != GoalState::Running
            || self.reserved.slices != 1
            || now_unix_ms < self.updated_at_unix_ms
            || self.budget_settlements.len() + self.budget_reservations.len() > 512
        {
            return Err(GoalError::InvalidState);
        }
        let charge = self.reserved;
        self.used = self
            .used
            .checked_add(charge)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.reserved = GoalUsage::default();
        self.budget_settlements
            .append(&mut self.budget_reservations);
        self.lease_epoch = self
            .lease_epoch
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.state_version = self
            .state_version
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.state = if now_unix_ms >= self.deadline_unix_ms {
            GoalState::Failed
        } else if safe_to_resume
            && self
                .available_for(GoalUsage {
                    slices: 1,
                    ..GoalUsage::default()
                })
                .is_ok()
        {
            GoalState::Queued
        } else {
            GoalState::Paused(if safe_to_resume {
                GoalPauseReason::Budget
            } else {
                GoalPauseReason::Recovery
            })
        };
        self.status_reason = Some(
            match self.state {
                GoalState::Queued => "recovered_after_reconciliation",
                GoalState::Failed => "absolute_deadline_reached",
                GoalState::Paused(GoalPauseReason::Budget) => "budget_exhausted",
                _ => "reconcile_interrupted_work",
            }
            .into(),
        );
        self.next_attempt_unix_ms = None;
        self.model_wait_attempts = 0;
        self.updated_at_unix_ms = now_unix_ms;
        self.validate()?;
        Ok(())
    }

    pub fn revise_input(&mut self, next_revision: u64, now_unix_ms: u64) -> Result<(), GoalError> {
        if self.state.is_terminal()
            || self.input_revision.checked_add(1) != Some(next_revision)
            || now_unix_ms < self.updated_at_unix_ms
        {
            return Err(GoalError::InvalidState);
        }
        let state_version = self
            .state_version
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        let prior_state = self.state;
        let prior_status_reason = self.status_reason.clone();
        let interrupted = self.state == GoalState::Running;
        let awaiting_work = self.state == GoalState::Waiting(GoalWaitReason::Work);
        let uncertain_budget = !self.budget_reservations.is_empty();
        if self.budget_settlements.len() + self.budget_reservations.len() > 512 {
            return Err(GoalError::InvalidState);
        }
        let charge = self.budget_reservations.values().try_fold(
            GoalUsage {
                slices: u32::from(interrupted),
                ..GoalUsage::default()
            },
            |total, reservation| {
                total
                    .checked_add(*reservation)
                    .ok_or(GoalError::ArithmeticOverflow)
            },
        )?;
        let next_used = self
            .used
            .checked_add(charge)
            .ok_or(GoalError::ArithmeticOverflow)?;
        let next_reserved = self
            .reserved
            .checked_sub(charge)
            .ok_or(GoalError::InvalidState)?;
        if !next_used
            .checked_add(next_reserved)
            .is_some_and(|usage| usage.fits(self.limits))
        {
            return Err(GoalError::BudgetExceeded);
        }
        self.used = next_used;
        self.reserved = next_reserved;
        self.budget_settlements
            .append(&mut self.budget_reservations);
        if interrupted {
            // The claimed slice was entered even if its result is now stale.
            // Reserve upper bounds become conservative usage when the old
            // owner is fenced. They cannot be silently refunded or replayed.
            self.lease_epoch = self
                .lease_epoch
                .checked_add(1)
                .ok_or(GoalError::ArithmeticOverflow)?;
        }
        self.input_revision = next_revision;
        self.state_version = state_version;
        self.state = if now_unix_ms >= self.deadline_unix_ms {
            GoalState::Failed
        } else if interrupted
            || awaiting_work
            || uncertain_budget
            || prior_state == GoalState::Paused(GoalPauseReason::Recovery)
        {
            GoalState::Paused(GoalPauseReason::Recovery)
        } else if let GoalState::Paused(reason) = prior_state {
            GoalState::Paused(reason)
        } else {
            // A new message is evidence for clarification, never an implicit
            // goal-text revision or permission to resume background work.
            GoalState::Waiting(GoalWaitReason::User)
        };
        if self.state == GoalState::Waiting(GoalWaitReason::User)
            && prior_state != GoalState::Waiting(GoalWaitReason::User)
        {
            // An unsolicited message stops work, then the assistant asks what
            // should change. This message alone is not a goal revision request.
            self.user_reply_floor_revision = Some(next_revision);
        }
        self.next_attempt_unix_ms = None;
        self.device_wait_attempts = 0;
        self.model_wait_attempts = 0;
        self.status_reason = match self.state {
            GoalState::Failed => Some("absolute_deadline_reached".into()),
            GoalState::Paused(GoalPauseReason::Recovery) => {
                Some("reconcile_interrupted_work".into())
            }
            GoalState::Paused(GoalPauseReason::Budget) => Some("budget_exhausted".into()),
            GoalState::Paused(_) => prior_status_reason,
            GoalState::Waiting(GoalWaitReason::User) => {
                Some("new_user_input_requires_goal_review".into())
            }
            _ => None,
        };
        self.updated_at_unix_ms = now_unix_ms;
        Ok(())
    }

    /// Transition a settled goal to a user-visible pause. A running segment
    /// must first be fenced and settled with its session in one transaction.
    pub fn pause_settled(
        &mut self,
        reason: GoalPauseReason,
        now_unix_ms: u64,
    ) -> Result<(), GoalError> {
        if self.state.is_terminal()
            || self.state == GoalState::Running
            || now_unix_ms < self.updated_at_unix_ms
        {
            return Err(GoalError::InvalidState);
        }
        self.state = GoalState::Paused(reason);
        self.state_version = self
            .state_version
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.next_attempt_unix_ms = None;
        self.device_wait_attempts = 0;
        self.model_wait_attempts = 0;
        self.updated_at_unix_ms = now_unix_ms;
        Ok(())
    }

    pub fn can_propose_revision(&self, input_revision: u64) -> Result<(), GoalError> {
        if self.state != GoalState::Waiting(GoalWaitReason::User)
            || self.input_revision != input_revision
            || self
                .user_reply_floor_revision
                .is_none_or(|floor| input_revision <= floor)
        {
            return Err(GoalError::InvalidState);
        }
        Ok(())
    }

    pub fn approve_revision(
        &mut self,
        text: &str,
        source_message_id: &str,
        input_revision: u64,
        now_unix_ms: u64,
    ) -> Result<(), GoalError> {
        self.can_propose_revision(input_revision)?;
        valid_id(source_message_id)?;
        if text.trim().is_empty()
            || text.len() > 16 * 1_024
            || now_unix_ms < self.updated_at_unix_ms
            || now_unix_ms >= self.deadline_unix_ms
        {
            return Err(GoalError::InvalidState);
        }
        self.goal_text = text.to_owned();
        self.source_message_id = source_message_id.to_owned();
        self.goal_revision = self
            .goal_revision
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.state_version = self
            .state_version
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.state = GoalState::Queued;
        self.user_reply_floor_revision = None;
        self.status_reason = None;
        self.checkpoint = None;
        self.next_attempt_unix_ms = None;
        self.stalled_slices = 0;
        self.device_wait_attempts = 0;
        self.model_wait_attempts = 0;
        self.updated_at_unix_ms = now_unix_ms;
        self.validate()
    }

    /// Do not claim a new slice while an earlier device action still needs
    /// reconciliation. The owner must see why the queued goal stopped.
    pub fn pause_for_unresolved_work(&mut self, now_unix_ms: u64) -> Result<(), GoalError> {
        if self.state != GoalState::Queued {
            return Err(GoalError::InvalidState);
        }
        self.pause_settled(GoalPauseReason::Recovery, now_unix_ms)?;
        self.status_reason = Some("reconcile_interrupted_work".into());
        self.validate()
    }

    pub fn block_model_before_claim(&mut self, now_unix_ms: u64) -> Result<(), GoalError> {
        if self.state != GoalState::Queued {
            return Err(GoalError::InvalidState);
        }
        self.pause_settled(GoalPauseReason::Recovery, now_unix_ms)?;
        self.state = GoalState::Blocked;
        self.status_reason = Some("model_request_rejected".into());
        self.validate()?;
        Ok(())
    }

    /// Apply an owner command after the store has fenced the conversation and
    /// established that no segment is running. The store writes the resulting
    /// goal, session version and audit event in one transaction.
    pub fn apply_owner_action(
        &mut self,
        action: GoalOwnerAction,
        now_unix_ms: u64,
    ) -> Result<(), GoalError> {
        match action {
            GoalOwnerAction::Pause => self.pause_settled(GoalPauseReason::Owner, now_unix_ms),
            GoalOwnerAction::Resume if self.state == GoalState::Waiting(GoalWaitReason::User) => {
                self.wake_from(GoalWaitReason::User, now_unix_ms)
            }
            GoalOwnerAction::Resume => self.resume_paused(false, now_unix_ms),
            GoalOwnerAction::RetryStalled
                if self.state == GoalState::Paused(GoalPauseReason::Stalled) =>
            {
                self.resume_paused(true, now_unix_ms)
            }
            GoalOwnerAction::RetryStalled => Err(GoalError::InvalidState),
            GoalOwnerAction::Cancel => self.cancel_settled(now_unix_ms),
        }
    }

    /// A wakeup only changes scheduling. The caller verifies the specific
    /// approval, work result, device or model condition before invoking it.
    pub fn wake_from(&mut self, reason: GoalWaitReason, now_unix_ms: u64) -> Result<(), GoalError> {
        if self.state != GoalState::Waiting(reason)
            || now_unix_ms < self.updated_at_unix_ms
            || now_unix_ms >= self.deadline_unix_ms
        {
            return Err(GoalError::InvalidState);
        }
        self.state = GoalState::Queued;
        self.state_version = self
            .state_version
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.next_attempt_unix_ms = None;
        if reason == GoalWaitReason::Device {
            self.device_wait_attempts = 0;
        }
        if reason == GoalWaitReason::User {
            self.user_reply_floor_revision = None;
        }
        self.status_reason = None;
        self.updated_at_unix_ms = now_unix_ms;
        Ok(())
    }

    /// Rotate an unresolved background wait behind other due goals without
    /// spending a planning slice or counting it as stalled progress.
    pub fn defer_work_recheck(&mut self, now_unix_ms: u64) -> Result<(), GoalError> {
        if self.state != GoalState::Waiting(GoalWaitReason::Work)
            || now_unix_ms < self.updated_at_unix_ms
            || now_unix_ms >= self.deadline_unix_ms
        {
            return Err(GoalError::InvalidState);
        }
        self.state_version = self
            .state_version
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.next_attempt_unix_ms =
            Some(now_unix_ms.saturating_add(5_000).min(self.deadline_unix_ms));
        self.updated_at_unix_ms = now_unix_ms;
        Ok(())
    }

    /// Device absence is a scheduler observation, not a spent work segment.
    /// Keep the goal out of the runnable FIFO until its bounded recheck time.
    pub fn wait_for_device(&mut self, now_unix_ms: u64) -> Result<(), GoalError> {
        if self.state != GoalState::Queued
            || now_unix_ms < self.updated_at_unix_ms
            || now_unix_ms >= self.deadline_unix_ms
        {
            return Err(GoalError::InvalidState);
        }
        self.state = GoalState::Waiting(GoalWaitReason::Device);
        self.state_version = self
            .state_version
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.device_wait_attempts = 1;
        self.next_attempt_unix_ms = Some(self.next_device_check(now_unix_ms));
        self.status_reason = Some("device_unavailable".into());
        self.updated_at_unix_ms = now_unix_ms;
        Ok(())
    }

    /// An offline recheck rotates the FIFO without consuming a slice or
    /// incrementing the stalled-segment count.
    pub fn defer_device_recheck(&mut self, now_unix_ms: u64) -> Result<(), GoalError> {
        if self.state != GoalState::Waiting(GoalWaitReason::Device)
            || now_unix_ms < self.updated_at_unix_ms
            || now_unix_ms >= self.deadline_unix_ms
        {
            return Err(GoalError::InvalidState);
        }
        self.state_version = self
            .state_version
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.device_wait_attempts = self.device_wait_attempts.saturating_add(1).min(7);
        self.next_attempt_unix_ms = Some(self.next_device_check(now_unix_ms));
        self.updated_at_unix_ms = now_unix_ms;
        Ok(())
    }

    fn next_device_check(&self, now_unix_ms: u64) -> u64 {
        let power = u32::from(self.device_wait_attempts.saturating_sub(1)).min(6);
        let delay_ms = (30_000_u64 << power).min(30 * 60 * 1_000);
        now_unix_ms
            .saturating_add(delay_ms)
            .min(self.deadline_unix_ms)
    }

    /// A transient model failure pauses scheduling before another slice is
    /// claimed. The attempt count survives a due-time wake, preventing a
    /// permanently failing provider from being retried every 30 seconds.
    pub fn wait_for_model(
        &mut self,
        now_unix_ms: u64,
        retry_after_unix_ms: Option<u64>,
    ) -> Result<(), GoalError> {
        if self.state != GoalState::Queued
            || now_unix_ms < self.updated_at_unix_ms
            || now_unix_ms >= self.deadline_unix_ms
        {
            return Err(GoalError::InvalidState);
        }
        if self
            .available_for(GoalUsage {
                slices: 1,
                model_calls: 1,
                ..GoalUsage::default()
            })
            .is_err()
        {
            return self.pause_settled(GoalPauseReason::Budget, now_unix_ms);
        }
        self.model_wait_attempts = self.model_wait_attempts.saturating_add(1).clamp(1, 7);
        self.state = GoalState::Waiting(GoalWaitReason::Model);
        self.state_version = self
            .state_version
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.next_attempt_unix_ms = Some(self.next_model_check(now_unix_ms, retry_after_unix_ms));
        self.status_reason = Some("model_temporarily_unavailable".into());
        self.updated_at_unix_ms = now_unix_ms;
        Ok(())
    }

    fn next_model_check(&self, now_unix_ms: u64, retry_after_unix_ms: Option<u64>) -> u64 {
        let power = u32::from(self.model_wait_attempts.saturating_sub(1)).min(6);
        let base = (30_000_u64 << power).min(30 * 60 * 1_000);
        let digest =
            Sha256::digest(format!("{}:{}", self.goal_id, self.model_wait_attempts).as_bytes());
        let jitter = u64::from(digest[0]) * (base / 10) / 255;
        now_unix_ms
            .saturating_add(base.saturating_add(jitter).min(30 * 60 * 1_000))
            .max(retry_after_unix_ms.unwrap_or(0))
            .min(self.deadline_unix_ms)
    }

    /// Ordinary resume preserves stagnation and consumption. Clearing the
    /// consecutive-stall count requires a separate explicit owner retry.
    pub fn resume_paused(
        &mut self,
        explicit_retry: bool,
        now_unix_ms: u64,
    ) -> Result<(), GoalError> {
        let GoalState::Paused(reason) = self.state else {
            return Err(GoalError::InvalidState);
        };
        if now_unix_ms < self.updated_at_unix_ms || now_unix_ms >= self.deadline_unix_ms {
            return Err(GoalError::DeadlineReached);
        }
        if reason == GoalPauseReason::Stalled && !explicit_retry {
            return Err(GoalError::InvalidState);
        }
        if !self.next_slice_budget_available() {
            return Err(GoalError::BudgetExceeded);
        }
        if explicit_retry && reason == GoalPauseReason::Stalled {
            self.stalled_slices = 0;
        }
        self.state = GoalState::Queued;
        self.state_version = self
            .state_version
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.next_attempt_unix_ms = None;
        self.model_wait_attempts = 0;
        self.status_reason = None;
        self.updated_at_unix_ms = now_unix_ms;
        Ok(())
    }

    pub fn cancel_settled(&mut self, now_unix_ms: u64) -> Result<(), GoalError> {
        if self.state.is_terminal()
            || self.state == GoalState::Running
            || now_unix_ms < self.updated_at_unix_ms
        {
            return Err(GoalError::InvalidState);
        }
        self.state = GoalState::Cancelled;
        self.state_version = self
            .state_version
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.next_attempt_unix_ms = None;
        self.device_wait_attempts = 0;
        self.model_wait_attempts = 0;
        self.updated_at_unix_ms = now_unix_ms;
        Ok(())
    }

    pub fn fail_expired_settled(&mut self, now_unix_ms: u64) -> Result<(), GoalError> {
        if self.state.is_terminal()
            || self.state == GoalState::Running
            || now_unix_ms < self.deadline_unix_ms
            || now_unix_ms < self.updated_at_unix_ms
        {
            return Err(GoalError::InvalidState);
        }
        self.state = GoalState::Failed;
        self.state_version = self
            .state_version
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?;
        self.status_reason = Some("absolute_deadline_reached".into());
        self.next_attempt_unix_ms = None;
        self.device_wait_attempts = 0;
        self.model_wait_attempts = 0;
        self.updated_at_unix_ms = now_unix_ms;
        Ok(())
    }
}

fn valid_id(value: &str) -> Result<(), GoalError> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        Err(GoalError::InvalidIdentity)
    } else {
        Ok(())
    }
}

impl GoalUsage {
    fn checked_sub(self, delta: Self) -> Option<Self> {
        Some(Self {
            input_tokens: self.input_tokens.checked_sub(delta.input_tokens)?,
            output_tokens: self.output_tokens.checked_sub(delta.output_tokens)?,
            cache_read_tokens: self
                .cache_read_tokens
                .checked_sub(delta.cache_read_tokens)?,
            cache_write_tokens: self
                .cache_write_tokens
                .checked_sub(delta.cache_write_tokens)?,
            model_calls: self.model_calls.checked_sub(delta.model_calls)?,
            tool_calls: self.tool_calls.checked_sub(delta.tool_calls)?,
            slices: self.slices.checked_sub(delta.slices)?,
            active_time_ms: self.active_time_ms.checked_sub(delta.active_time_ms)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_binding() -> GoalModelBinding {
        GoalModelBinding {
            connection_id: "gateway".into(),
            connection_revision: 1,
            profile_revision: 1,
            model_id: "model".into(),
        }
    }

    #[test]
    fn goal_model_binding_comes_only_from_a_valid_model_destination() {
        use desk_agent_protocol::data_lineage::DestinationIdentity;
        let destination = DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        };
        assert_eq!(
            GoalModelBinding::from_destination(&destination),
            Ok(model_binding())
        );
        assert_eq!(
            GoalModelBinding::from_destination(&DestinationIdentity::LocalArtifact {
                workspace_id: "workspace".into(),
            }),
            Err(GoalError::InvalidIdentity)
        );
        let invalid = DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: -1,
        };
        assert_eq!(
            GoalModelBinding::from_destination(&invalid),
            Err(GoalError::InvalidIdentity)
        );
    }

    fn open_request() -> GoalOpenRequest {
        GoalOpenRequest::new(
            "request".into(),
            "run".into(),
            "owner".into(),
            "device".into(),
            "message".into(),
            1,
            "Finish the report".into(),
            GoalLimits::default(),
            model_binding(),
            1_000,
        )
        .unwrap()
    }

    #[test]
    fn proposed_goal_needs_a_fresh_owner_decision_and_matching_model() {
        let mut request = open_request();
        assert_eq!(
            request.approve(
                2,
                &model_binding(),
                "goal".into(),
                "decision".into(),
                1_001,
                None
            ),
            Err(GoalError::StaleRevision),
        );
        let mut changed_model = model_binding();
        changed_model.profile_revision += 1;
        assert_eq!(
            request.approve(
                1,
                &changed_model,
                "goal".into(),
                "decision".into(),
                1_001,
                None
            ),
            Err(GoalError::StaleRevision),
        );
        let goal = request
            .approve(
                1,
                &model_binding(),
                "goal".into(),
                "decision".into(),
                1_001,
                None,
            )
            .unwrap();
        assert_eq!(goal.goal_text, "Finish the report");
        assert!(matches!(
            goal.opening,
            GoalOpening::AiRequestApproved { .. }
        ));
        assert_eq!(request.state, GoalOpenRequestState::Approved);
        assert_eq!(
            request.approve(
                1,
                &model_binding(),
                "again".into(),
                "again".into(),
                1_002,
                None
            ),
            Err(GoalError::StaleRevision),
        );
    }

    #[test]
    fn completed_goal_reference_is_only_valid_for_a_new_proposal() {
        let mut request = open_request();
        request.previous_completed_goal_id = Some("completed-goal".into());
        request.validate().unwrap();
        request.target_goal_id = Some("active-goal".into());
        request.target_goal_revision = Some(1);
        assert_eq!(request.validate(), Err(GoalError::InvalidIdentity));
    }

    #[test]
    fn approving_a_reopened_goal_requires_the_verified_previous_result() {
        let mut previous = goal();
        previous.claim_slice(1_001).unwrap();
        previous
            .finish_slice(
                1,
                1,
                1,
                &GoalControl::Complete {
                    summary: "Partial result".into(),
                    evidence_ids: vec!["receipt".into()],
                },
                true,
                true,
                7,
                vec!["attachment".into()],
                1_002,
            )
            .unwrap();
        let mut request = open_request();
        request.previous_completed_goal_id = Some(previous.goal_id.clone());
        assert_eq!(
            request.approve(
                1,
                &model_binding(),
                "new-goal".into(),
                "decision".into(),
                1_003,
                None
            ),
            Err(GoalError::InvalidState)
        );
        let reopened = request
            .approve(
                1,
                &model_binding(),
                "new-goal".into(),
                "decision".into(),
                1_003,
                Some(&previous),
            )
            .unwrap();
        assert_eq!(
            reopened.previous_completion.as_ref().unwrap().summary,
            "Partial result"
        );
        assert_eq!(previous.state, GoalState::Completed);
    }

    #[test]
    fn expired_proposal_cannot_be_approved_or_silently_renewed() {
        let mut request = open_request();
        let deadline = request.expires_at_unix_ms;
        assert_eq!(
            request.approve(
                1,
                &model_binding(),
                "goal".into(),
                "decision".into(),
                deadline,
                None,
            ),
            Err(GoalError::DeadlineReached),
        );
        request
            .close(
                GoalOpenRequestState::Expired,
                "expired-event".into(),
                deadline,
            )
            .unwrap();
        assert_eq!(request.state, GoalOpenRequestState::Expired);
    }

    #[test]
    fn goal_request_decision_event_matches_its_durable_status() {
        let mut request = open_request();
        let decision_id = GoalOpenRequestEvent::id_for(
            &request.request_id,
            GoalOpenRequestState::Denied,
            AgentRunEventKind::GoalOpenDecided,
        )
        .unwrap();
        request
            .close(GoalOpenRequestState::Denied, decision_id.clone(), 1_001)
            .unwrap();
        let event = GoalOpenRequestEvent::new(
            &request,
            AgentRunEventKind::GoalOpenDecided,
            2,
            "2026-09-23T00:00:00Z".into(),
        )
        .unwrap();
        assert_eq!(event.event.event_id, decision_id);
        assert_eq!(event.request.state, GoalOpenRequestState::Denied);
    }

    fn goal() -> GoalRun {
        GoalRun::new(
            "goal".into(),
            "run".into(),
            "owner".into(),
            "device".into(),
            "Finish the report".into(),
            "message".into(),
            GoalOpening::OwnerRequest,
            GoalModelBinding {
                connection_id: "gateway".into(),
                connection_revision: 1,
                profile_revision: 1,
                model_id: "model".into(),
            },
            1,
            1_000,
            GoalLimits::default(),
        )
        .unwrap()
    }

    #[test]
    fn an_uncommitted_reservation_still_counts_against_the_limit() {
        let mut goal = goal();
        goal.validate().unwrap();
        goal.claim_slice(1_001).unwrap();
        goal.reserve_with_id(
            "model-1",
            GoalUsage {
                input_tokens: DEFAULT_MODEL_TOKENS,
                ..GoalUsage::default()
            },
            1_002,
        )
        .unwrap();
        assert_eq!(
            goal.available_for(GoalUsage {
                output_tokens: 1,
                ..GoalUsage::default()
            }),
            Err(GoalError::BudgetExceeded)
        );
        goal.settle_with_id(
            "model-1",
            GoalUsage {
                input_tokens: 10,
                ..GoalUsage::default()
            },
            1_003,
        )
        .unwrap();
        assert_eq!(goal.used.total_tokens(), Some(10));
        goal.validate().unwrap();
    }

    #[test]
    fn slice_identity_and_lease_are_monotonic() {
        let mut goal = goal();
        assert_eq!(goal.claim_slice(1_001), Ok((1, 1)));
        assert_eq!(goal.require_fence(1, 1, 1), Ok(()));
        assert_eq!(goal.require_fence(1, 1, 0), Err(GoalError::StaleLease));
        assert_eq!(goal.claim_slice(1_002), Err(GoalError::InvalidState));
    }

    #[test]
    fn reconciled_orphan_charges_uncertain_budget_and_starts_a_new_slice() {
        let mut goal = goal();
        goal.claim_slice(1_001).unwrap();
        goal.reserve_with_id(
            "model-1",
            GoalUsage {
                input_tokens: 12_000,
                model_calls: 1,
                ..GoalUsage::default()
            },
            1_002,
        )
        .unwrap();
        goal.recover_interrupted_slice(true, 1_003).unwrap();
        assert_eq!(goal.state, GoalState::Queued);
        assert_eq!(goal.used.slices, 1);
        assert_eq!(goal.used.input_tokens, 12_000);
        assert_eq!(goal.reserved, GoalUsage::default());
        assert_eq!(goal.require_fence(1, 1, 1), Err(GoalError::StaleLease));
        assert_eq!(goal.claim_slice(1_004), Ok((2, 3)));
    }

    #[test]
    fn unresolved_orphan_does_not_resume_itself() {
        let mut goal = goal();
        goal.claim_slice(1_001).unwrap();
        goal.recover_interrupted_slice(false, 1_002).unwrap();
        assert_eq!(goal.state, GoalState::Paused(GoalPauseReason::Recovery));
        assert_eq!(goal.claim_slice(1_003), Err(GoalError::InvalidState));
    }

    #[test]
    fn queued_goal_with_unresolved_work_pauses_visibly_without_spending_budget() {
        let mut goal = goal();
        goal.pause_for_unresolved_work(1_001).unwrap();
        assert_eq!(goal.state, GoalState::Paused(GoalPauseReason::Recovery));
        assert_eq!(
            goal.status_reason.as_deref(),
            Some("reconcile_interrupted_work")
        );
        assert_eq!(goal.used, GoalUsage::default());
        assert_eq!(goal.claim_slice(1_002), Err(GoalError::InvalidState));
        goal.validate().unwrap();
    }

    #[test]
    fn offline_device_waits_without_spending_a_slice_or_stall_budget() {
        let mut goal = goal();
        goal.wait_for_device(1_001).unwrap();
        assert_eq!(goal.state, GoalState::Waiting(GoalWaitReason::Device));
        assert_eq!(goal.slice_seq, 0);
        assert_eq!(goal.used, GoalUsage::default());
        assert_eq!(goal.stalled_slices, 0);
        assert_eq!(goal.next_attempt_unix_ms, Some(31_001));
        goal.defer_device_recheck(31_001).unwrap();
        assert_eq!(goal.next_attempt_unix_ms, Some(91_001));
        goal.wake_from(GoalWaitReason::Device, 91_001).unwrap();
        assert_eq!(goal.state, GoalState::Queued);
        assert_eq!(goal.device_wait_attempts, 0);
        assert_eq!(goal.claim_slice(91_002), Ok((1, 1)));
    }

    #[test]
    fn unresolved_background_work_rotates_without_spending_another_slice() {
        let mut goal = goal();
        let (_, epoch) = goal.claim_slice(1_001).unwrap();
        goal.finish_slice(
            1,
            1,
            epoch,
            &GoalControl::Wait {
                reason: GoalWaitReason::Work,
                reference_id: "task-1".into(),
            },
            false,
            false,
            1,
            vec![],
            1_002,
        )
        .unwrap();
        assert!(goal.owns_waited_work("task-1"));
        assert!(!goal.owns_waited_work("task-2"));
        let mut stopped = goal.clone();
        stopped.cancel_settled(1_003).unwrap();
        assert!(stopped.owns_waited_work("task-1"));
        let used = goal.used;
        goal.defer_work_recheck(1_003).unwrap();
        assert_eq!(goal.state, GoalState::Waiting(GoalWaitReason::Work));
        assert_eq!(goal.status_reason.as_deref(), Some("task-1"));
        assert_eq!(goal.next_attempt_unix_ms, Some(6_003));
        assert_eq!(goal.used, used);
        assert_eq!(goal.stalled_slices, 0);
        goal.wake_from(GoalWaitReason::Work, 6_003).unwrap();
        assert_eq!(goal.state, GoalState::Queued);
        assert_eq!(goal.claim_slice(6_004), Ok((2, 2)));
    }

    #[test]
    fn background_wake_requires_the_matching_server_completion_receipt() {
        let mut goal = goal();
        let (_, epoch) = goal.claim_slice(1_001).unwrap();
        goal.finish_slice(
            1,
            1,
            epoch,
            &GoalControl::Wait {
                reason: GoalWaitReason::Work,
                reference_id: "task-1".into(),
            },
            false,
            false,
            1,
            vec![],
            1_002,
        )
        .unwrap();
        let mut session = PersistedAgentSession::new(
            "run",
            "owner",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-09-23T00:00:00Z",
        );
        session.input_revision = 1;
        let mut wrong = crate::chat::ChatMessage::tool_result("other-event", "call", "done");
        wrong.background_task_id = Some("task-2".into());
        wrong.pending_delivery_format = Some(crate::seam::ToolOutputFormat::Text);
        session.conversation.push(wrong);
        assert_eq!(goal.completed_work_event_id(&session), None);
        let mut placeholder =
            crate::chat::ChatMessage::tool_result("placeholder", "call", "pending");
        placeholder.background_task_id = Some("task-1".into());
        session.conversation.push(placeholder);
        assert_eq!(goal.completed_work_event_id(&session), None);
        let mut completion = crate::chat::ChatMessage::tool_result("completion", "call", "done");
        completion.background_task_id = Some("task-1".into());
        completion.pending_delivery_format = Some(crate::seam::ToolOutputFormat::Text);
        session.conversation.push(completion);
        assert_eq!(goal.completed_work_event_id(&session), Some("completion"));
        session
            .pending_auto_triggers
            .push(crate::session::PendingAutoTrigger {
                work_id: 1,
                kind: crate::session::WorkKind::AgentExec,
                execution_id: "execution".into(),
                tool_call_id: "call".into(),
                event_id: "completion".into(),
                chain_id: "chain".into(),
                resolution_org_id: None,
                since: "2026-09-23T00:00:00Z".into(),
            });
        assert_eq!(
            goal.owned_pending_work_event_ids(&session),
            vec!["completion".to_string()]
        );
        session.input_revision = 2;
        assert_eq!(goal.completed_work_event_id(&session), None);
    }

    #[test]
    fn transient_model_wait_backs_off_without_spending_a_slice() {
        let mut goal = goal();
        goal.wait_for_model(1_001, None).unwrap();
        let first_due = goal.next_attempt_unix_ms.unwrap();
        assert_eq!(goal.state, GoalState::Waiting(GoalWaitReason::Model));
        assert_eq!(goal.model_wait_attempts, 1);
        assert_eq!(goal.used.slices, 0);
        goal.wake_from(GoalWaitReason::Model, first_due).unwrap();
        assert_eq!(goal.model_wait_attempts, 1);
        goal.wait_for_model(first_due + 1, None).unwrap();
        assert_eq!(goal.model_wait_attempts, 2);
        assert!(goal.next_attempt_unix_ms.unwrap() >= first_due + 60_001);
        let second_due = goal.next_attempt_unix_ms.unwrap();
        goal.wake_from(GoalWaitReason::Model, second_due).unwrap();
        assert_eq!(goal.claim_slice(second_due + 1), Ok((1, 1)));
        assert_eq!(goal.model_wait_attempts, 2);
    }

    #[test]
    fn failed_model_segments_settle_the_slice_and_preserve_exponential_backoff() {
        let mut goal = goal();
        let (_, first_epoch) = goal.claim_slice(1_001).unwrap();
        goal.finish_model_wait_slice(1, 1, first_epoch, None, 1, vec![], 1_002)
            .unwrap();
        assert_eq!(goal.state, GoalState::Waiting(GoalWaitReason::Model));
        assert_eq!(goal.used.slices, 1);
        assert_eq!(goal.model_wait_attempts, 1);
        let first_due = goal.next_attempt_unix_ms.unwrap();
        goal.wake_from(GoalWaitReason::Model, first_due).unwrap();
        let (_, second_epoch) = goal.claim_slice(first_due + 1).unwrap();
        goal.finish_model_wait_slice(1, 1, second_epoch, None, 2, vec![], first_due + 2)
            .unwrap();
        assert_eq!(goal.used.slices, 2);
        assert_eq!(goal.model_wait_attempts, 2);
        assert!(goal.next_attempt_unix_ms.unwrap() >= first_due + 60_002);
        goal.validate().unwrap();
    }

    #[test]
    fn permanent_model_rejection_blocks_after_settling_the_slice() {
        let mut goal = goal();
        let (_, epoch) = goal.claim_slice(1_001).unwrap();
        goal.finish_blocked_model_slice(1, 1, epoch, 1, vec![], 1_002)
            .unwrap();
        assert_eq!(goal.state, GoalState::Blocked);
        assert_eq!(goal.used.slices, 1);
        assert_eq!(goal.claim_slice(1_003), Err(GoalError::InvalidState));
        goal.validate().unwrap();
    }

    #[test]
    fn permanent_model_preflight_rejection_spends_no_slice() {
        let mut goal = goal();
        goal.block_model_before_claim(1_001).unwrap();
        assert_eq!(goal.state, GoalState::Blocked);
        assert_eq!(goal.used, GoalUsage::default());
        assert_eq!(goal.slice_seq, 0);
        assert_eq!(goal.claim_slice(1_002), Err(GoalError::InvalidState));
    }

    #[test]
    fn provider_retry_after_can_extend_but_not_outlive_goal_deadline() {
        let mut first_goal = goal();
        first_goal
            .wait_for_model(1_001, Some(1_001 + 3_600_000))
            .unwrap();
        assert_eq!(first_goal.next_attempt_unix_ms, Some(1_001 + 3_600_000));
        let mut goal = goal();
        let deadline = goal.deadline_unix_ms;
        goal.wait_for_model(1_001, Some(deadline + 3_600_000))
            .unwrap();
        assert_eq!(goal.next_attempt_unix_ms, Some(deadline));
    }

    #[test]
    fn new_user_input_fences_running_slice_without_refunding_unknown_usage() {
        let mut goal = goal();
        goal.claim_slice(1_001).unwrap();
        goal.reserve_with_id(
            "model-1",
            GoalUsage {
                input_tokens: 10_000,
                model_calls: 1,
                ..GoalUsage::default()
            },
            1_002,
        )
        .unwrap();
        goal.revise_input(2, 1_003).unwrap();
        goal.validate().unwrap();
        assert_eq!(goal.state, GoalState::Paused(GoalPauseReason::Recovery));
        assert_eq!(goal.used.slices, 1);
        assert_eq!(goal.reserved.slices, 0);
        assert_eq!(goal.reserved.input_tokens, 0);
        assert_eq!(goal.used.input_tokens, 10_000);
        assert_eq!(goal.used.model_calls, 1);
        assert_eq!(goal.require_fence(1, 1, 1), Err(GoalError::StaleRevision));
        assert_eq!(goal.claim_slice(1_004), Err(GoalError::InvalidState));
    }

    #[test]
    fn new_user_input_holds_the_goal_without_revising_its_text() {
        let mut goal = goal();
        goal.state = GoalState::Waiting(GoalWaitReason::User);
        goal.revise_input(2, 1_001).unwrap();
        goal.validate().unwrap();
        assert_eq!(goal.state, GoalState::Waiting(GoalWaitReason::User));
        assert_eq!(goal.goal_revision, 1);
        assert_eq!(goal.goal_text, "Finish the report");
        assert_eq!(goal.claim_slice(1_002), Err(GoalError::InvalidState));
    }

    #[test]
    fn goal_revision_needs_a_reply_after_the_stop_and_keeps_the_budget() {
        let mut goal = goal();
        goal.used.model_calls = 3;
        let (_, epoch) = goal.claim_slice(1_001).unwrap();
        goal.finish_slice(
            1,
            1,
            epoch,
            &GoalControl::Wait {
                reason: GoalWaitReason::User,
                reference_id: "clarify-scope".into(),
            },
            false,
            false,
            1,
            vec![],
            1_002,
        )
        .unwrap();
        assert_eq!(goal.can_propose_revision(1), Err(GoalError::InvalidState));
        goal.revise_input(2, 1_003).unwrap();
        let mut request = GoalOpenRequest::new_revision(
            "revision-request".into(),
            &goal,
            "owner-reply".into(),
            2,
            "Finish the report with the corrected scope".into(),
            model_binding(),
            1_004,
        )
        .unwrap();
        let before = goal.used;
        request
            .approve_revision(
                &mut goal,
                2,
                &model_binding(),
                "revision-decision".into(),
                1_005,
            )
            .unwrap();
        assert_eq!(goal.goal_revision, 2);
        assert_eq!(goal.goal_id, "goal");
        assert_eq!(goal.original_goal_text, "Finish the report");
        assert_eq!(goal.original_source_message_id, "message");
        assert_eq!(goal.used, before);
        assert_eq!(goal.state, GoalState::Queued);
        assert_eq!(request.resulting_goal_id.as_deref(), Some("goal"));
    }

    #[test]
    fn new_input_does_not_erase_budget_pause_or_extend_deadline() {
        let mut goal = goal();
        goal.state = GoalState::Paused(GoalPauseReason::Budget);
        goal.revise_input(2, 1_001).unwrap();
        assert_eq!(goal.state, GoalState::Paused(GoalPauseReason::Budget));
        assert_eq!(goal.limits, GoalLimits::default());
        goal.revise_input(3, goal.deadline_unix_ms).unwrap();
        assert_eq!(goal.state, GoalState::Failed);
        assert_eq!(goal.deadline_unix_ms, 1_000 + DEFAULT_DEADLINE_MS);
    }

    #[test]
    fn lower_platform_budget_preserves_history_and_blocks_new_reservations() {
        let mut goal = goal();
        goal.used.model_calls = 10;
        let mut policy = crate::goal_budget::initial();
        policy.limits.model_calls = Some(5);
        goal.apply_budget_policy(&policy).unwrap();
        assert_eq!(goal.used.model_calls, 10);
        assert_eq!(
            goal.available_for(GoalUsage {
                model_calls: 1,
                ..GoalUsage::default()
            }),
            Err(GoalError::BudgetExceeded)
        );
        policy.limits.model_calls = None;
        goal.apply_budget_policy(&policy).unwrap();
        assert!(
            goal.available_for(GoalUsage {
                model_calls: 1,
                ..GoalUsage::default()
            })
            .is_ok()
        );
    }

    #[test]
    fn owner_controls_preserve_usage_and_require_a_settled_goal() {
        let mut goal = goal();
        goal.used.model_calls = 10;
        goal.apply_owner_action(GoalOwnerAction::Pause, 1_001)
            .unwrap();
        assert_eq!(goal.state, GoalState::Paused(GoalPauseReason::Owner));
        assert_eq!(
            goal.apply_owner_action(GoalOwnerAction::RetryStalled, 1_002),
            Err(GoalError::InvalidState)
        );
        assert_eq!(goal.used.model_calls, 10);
        goal.apply_owner_action(GoalOwnerAction::Resume, 1_003)
            .unwrap();
        assert_eq!(goal.state, GoalState::Queued);
        goal.claim_slice(1_004).unwrap();
        assert_eq!(
            goal.apply_owner_action(GoalOwnerAction::Cancel, 1_005),
            Err(GoalError::InvalidState)
        );
    }

    #[test]
    fn control_cannot_complete_with_unsettled_actions() {
        let mut goal = goal();
        goal.claim_slice(1_001).unwrap();
        let control = GoalControl::Complete {
            evidence_ids: vec!["receipt".into()],
            summary: "Done".into(),
        };
        assert_eq!(
            goal.finish_slice(1, 1, 1, &control, true, false, 1, vec![], 1_002),
            Err(GoalError::InvalidState)
        );
        goal.finish_slice(1, 1, 1, &control, true, true, 1, vec![], 1_002)
            .unwrap();
        assert_eq!(goal.state, GoalState::Completed);
        assert_eq!(goal.used.slices, 1);
    }

    #[test]
    fn new_run_references_only_a_completed_goal_for_the_same_subject() {
        let mut previous = goal();
        previous.claim_slice(1_001).unwrap();
        previous
            .finish_slice(
                1,
                1,
                1,
                &GoalControl::Complete {
                    evidence_ids: vec!["receipt".into()],
                    summary: "First result".into(),
                },
                true,
                true,
                7,
                vec!["attachment".into()],
                1_002,
            )
            .unwrap();
        let mut next = goal();
        next.goal_id = "next-goal".into();
        next.source_message_id = "next-message".into();
        next.original_source_message_id = "next-message".into();
        next.input_revision = 2;
        let linked = next.clone().with_previous_completed(&previous).unwrap();
        assert_eq!(linked.previous_completion.as_ref().unwrap().goal_id, "goal");
        assert_eq!(linked.previous_completion.as_ref().unwrap().event_seq, 7);
        assert_eq!(
            linked.previous_completion.as_ref().unwrap().summary,
            "First result"
        );
        assert_eq!(
            linked.previous_completion.as_ref().unwrap().evidence_ids,
            vec!["receipt"]
        );
        assert_eq!(
            linked
                .previous_completion
                .as_ref()
                .unwrap()
                .protected_attachment_ids,
            vec!["attachment"]
        );
        assert_eq!(previous.state, GoalState::Completed);
        assert_eq!(previous.used.slices, 1);

        let mut other_subject = previous.clone();
        other_subject.owner_id = "other-owner".into();
        assert_eq!(
            next.clone().with_previous_completed(&other_subject),
            Err(GoalError::InvalidState)
        );
        let mut unfinished = previous;
        unfinished.state = GoalState::Queued;
        assert_eq!(
            next.with_previous_completed(&unfinished),
            Err(GoalError::InvalidState)
        );
    }

    #[test]
    fn continuation_on_the_final_slice_pauses_for_budget() {
        let mut goal = goal();
        goal.limits.slices = 1;
        goal.claim_slice(1_001).unwrap();
        goal.finish_slice(
            1,
            1,
            1,
            &GoalControl::Continue {
                progress: "Inspected the source".into(),
                next_step: "Write it".into(),
            },
            true,
            true,
            1,
            vec![],
            1_002,
        )
        .unwrap();
        assert_eq!(goal.state, GoalState::Paused(GoalPauseReason::Budget));
        assert_eq!(goal.status_reason.as_deref(), Some("budget_exhausted"));
    }

    #[test]
    fn repeated_result_does_not_reset_stalled_slice_count() {
        let mut goal = goal();
        let receipt = "a".repeat(64);
        let control = GoalControl::Continue {
            progress: "Checked the same object".into(),
            next_step: "Check again".into(),
        };
        goal.claim_slice(1_001).unwrap();
        assert!(
            goal.observe_result_fingerprints(&[receipt.clone()])
                .unwrap()
        );
        goal.finish_slice(1, 1, 1, &control, true, true, 1, vec![], 1_002)
            .unwrap();
        assert_eq!(goal.stalled_slices, 0);
        goal.claim_slice(1_003).unwrap();
        assert!(!goal.observe_result_fingerprints(&[receipt]).unwrap());
        goal.finish_slice(1, 1, 2, &control, false, true, 2, vec![], 1_004)
            .unwrap();
        assert_eq!(goal.stalled_slices, 1);
        goal.validate().unwrap();
    }
}
