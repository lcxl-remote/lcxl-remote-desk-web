//! Versioned, provider-neutral model history views and persisted window state.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::chat::{ChatMessage, ChatRole};
use crate::replay::{ReplayDisposition, SourceContextKey};
use crate::trim::model_context_cost;
use crate::{MAX_MODEL_CONTEXT_BYTES, MIN_MODEL_CONTEXT_BYTES};

mod checkpoint;

pub use checkpoint::*;

pub const MODEL_CONTEXT_STATE_SCHEMA_VERSION: u16 = 2;
pub const CONTEXT_STRATEGY_SCHEMA_VERSION: u16 = 1;
pub const PLATFORM_CONTEXT_POLICY_SCHEMA_VERSION: u32 = 1;
pub const MAX_CONTEXT_POLICY_ENTRIES: usize = 8;
pub const MAX_CONTEXT_NOTICES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextManagementStrategy {
    Window,
    CheckpointSummary,
}

impl ContextManagementStrategy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::CheckpointSummary => "checkpoint_summary",
        }
    }
}

/// Product policy persisted by the central service, independent of deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformContextPolicy {
    pub schema_version: u32,
    pub revision: u64,
    pub strategy: ContextManagementStrategy,
}

impl Default for PlatformContextPolicy {
    fn default() -> Self {
        Self {
            schema_version: PLATFORM_CONTEXT_POLICY_SCHEMA_VERSION,
            revision: 0,
            strategy: ContextManagementStrategy::CheckpointSummary,
        }
    }
}

impl PlatformContextPolicy {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != PLATFORM_CONTEXT_POLICY_SCHEMA_VERSION {
            return Err("unsupported context-management schema version");
        }
        Ok(())
    }

    pub fn candidate(&self, strategy: ContextManagementStrategy) -> Result<Self, &'static str> {
        self.validate()?;
        Ok(Self {
            schema_version: PLATFORM_CONTEXT_POLICY_SCHEMA_VERSION,
            revision: self.revision.checked_add(1).ok_or("revision overflow")?,
            strategy,
        })
    }

    pub fn pin(
        &self,
        source: SourceContextKey,
        profile_revision: i64,
        budget: usize,
    ) -> Result<PinnedContextPolicy, ModelContextError> {
        self.validate()
            .map_err(|_| ModelContextError::UnsupportedStrategy)?;
        let mut policy = PinnedContextPolicy::window(source, profile_revision, budget)?;
        policy.strategy = self.strategy;
        policy.platform_context_policy_revision = self.revision;
        Ok(policy)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContextPolicyKey(String);

impl ContextPolicyKey {
    pub fn derive(policy: &PinnedContextPolicy) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"lrdm-context-policy-v1\0");
        for component in [
            policy.source_context_key.as_str().to_string(),
            policy.profile_revision.to_string(),
            policy.max_context_bytes.to_string(),
            policy.strategy.as_str().to_string(),
            policy.platform_context_policy_revision.to_string(),
            policy.context_strategy_schema_version.to_string(),
        ] {
            digest.update((component.len() as u64).to_be_bytes());
            digest.update(component.as_bytes());
        }
        if policy.strategy == ContextManagementStrategy::CheckpointSummary {
            for component in [
                CONTEXT_SUMMARY_PROMPT_VERSION.to_string(),
                CONTEXT_SUMMARY_SCHEMA_VERSION.to_string(),
            ] {
                digest.update((component.len() as u64).to_be_bytes());
                digest.update(component.as_bytes());
            }
        }
        Self(format!("v1:{:x}", digest.finalize()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedContextPolicy {
    pub source_context_key: SourceContextKey,
    pub profile_revision: i64,
    pub max_context_bytes: usize,
    /// Bytes reserved for the per-step system prompt, API tool specifications
    /// and request framing. This is ephemeral and deliberately excluded from
    /// the persisted policy key.
    pub request_overhead_bytes: usize,
    pub strategy: ContextManagementStrategy,
    pub platform_context_policy_revision: u64,
    pub context_strategy_schema_version: u16,
}

impl PinnedContextPolicy {
    pub fn window(
        source_context_key: SourceContextKey,
        profile_revision: i64,
        max_context_bytes: usize,
    ) -> Result<Self, ModelContextError> {
        if profile_revision < 1 {
            return Err(ModelContextError::InvalidProfileRevision(profile_revision));
        }
        if !(MIN_MODEL_CONTEXT_BYTES..=MAX_MODEL_CONTEXT_BYTES).contains(&max_context_bytes) {
            return Err(ModelContextError::InvalidBudget(max_context_bytes));
        }
        Ok(Self {
            source_context_key,
            profile_revision,
            max_context_bytes,
            request_overhead_bytes: 0,
            strategy: ContextManagementStrategy::Window,
            platform_context_policy_revision: 1,
            context_strategy_schema_version: CONTEXT_STRATEGY_SCHEMA_VERSION,
        })
    }

    pub fn checkpoint_summary(
        source_context_key: SourceContextKey,
        profile_revision: i64,
        max_context_bytes: usize,
        platform_context_policy_revision: u64,
    ) -> Result<Self, ModelContextError> {
        let mut policy = Self::window(source_context_key, profile_revision, max_context_bytes)?;
        policy.strategy = ContextManagementStrategy::CheckpointSummary;
        policy.platform_context_policy_revision = platform_context_policy_revision;
        Ok(policy)
    }

    pub fn key(&self) -> ContextPolicyKey {
        ContextPolicyKey::derive(self)
    }

    pub const fn low_watermark_bytes(&self) -> usize {
        let budget = self.history_context_bytes();
        budget - budget / 4
    }

    pub const fn history_context_bytes(&self) -> usize {
        self.max_context_bytes
            .saturating_sub(self.request_overhead_bytes)
    }

    pub fn with_request_overhead_bytes(
        mut self,
        request_overhead_bytes: usize,
    ) -> Result<Self, ModelContextError> {
        self.request_overhead_bytes = request_overhead_bytes;
        if self.history_context_bytes() == 0 {
            return Err(ModelContextError::InvalidBudget(
                self.history_context_bytes(),
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelContextState {
    pub schema_version: u16,
    #[serde(default)]
    pub entries: Vec<ModelContextEntry>,
}

impl ModelContextState {
    /// Upgrade the only supported historical representation. Schema v1 reserved
    /// `checkpoint` as null, so the migration is lossless and deliberately rejects
    /// any non-null/unknown checkpoint before changing the version.
    pub fn upgrade_from_v1(&mut self) -> Result<bool, ModelContextError> {
        match self.schema_version {
            MODEL_CONTEXT_STATE_SCHEMA_VERSION => {
                validate_state(self)?;
                Ok(false)
            }
            1 => {
                if self.entries.iter().any(|entry| {
                    entry.strategy != ContextManagementStrategy::Window
                        || entry.checkpoint.is_some()
                }) {
                    return Err(ModelContextError::UnsupportedStrategy);
                }
                self.schema_version = MODEL_CONTEXT_STATE_SCHEMA_VERSION;
                validate_state(self)?;
                Ok(true)
            }
            version => Err(ModelContextError::UnsupportedStateSchema(version)),
        }
    }
}

impl Default for ModelContextState {
    fn default() -> Self {
        Self {
            schema_version: MODEL_CONTEXT_STATE_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelContextEntry {
    pub policy_key: ContextPolicyKey,
    pub strategy: ContextManagementStrategy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_group_head_message_id: Option<String>,
    /// When every current group is excluded, retain the same logical suffix
    /// boundary as "immediately after this immutable group head". Without this
    /// tail cursor, `None` would be indistinguishable from an untrimmed history
    /// and the next appended user turn would emit a duplicate trim notice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_after_group_head_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<ContextCheckpoint>,
    pub last_used_session_version: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextNoticeKind {
    Trimmed,
    Compacted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextNotice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_message_id: Option<String>,
    pub id: String,
    pub turn_id: String,
    pub kind: ContextNoticeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_generation: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub covered_message_count: Option<u32>,
}

impl ContextNotice {
    pub fn trimmed(turn_id: impl Into<String>) -> Self {
        let turn_id = turn_id.into();
        Self {
            id: format!("context-trimmed:{turn_id}"),
            created_at: None,
            after_message_id: None,
            turn_id,
            kind: ContextNoticeKind::Trimmed,
            checkpoint_generation: None,
            covered_message_count: None,
        }
    }

    pub fn compacted(
        turn_id: impl Into<String>,
        generation: u32,
        covered_message_count: u32,
    ) -> Self {
        let turn_id = turn_id.into();
        Self {
            id: format!("context-compacted:{turn_id}:{generation}"),
            created_at: None,
            after_message_id: None,
            turn_id,
            kind: ContextNoticeKind::Compacted,
            checkpoint_generation: Some(generation),
            covered_message_count: Some(covered_message_count),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelContextView {
    pub messages: Vec<ChatMessage>,
    pub policy_key: ContextPolicyKey,
    pub floor_group_head_message_id: Option<String>,
    pub floor_advanced: bool,
}

#[derive(Debug)]
pub(crate) struct MessageGroup {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) cost: usize,
    pub(crate) replay_safe: bool,
    pub(crate) summary_eligible: bool,
    pub(crate) discard_only: bool,
}

/// Build the sole model-facing history view and update the current policy entry.
/// The stored conversation itself is never changed.
pub fn build_model_context_view(
    conversation: &[ChatMessage],
    state: &mut ModelContextState,
    policy: &PinnedContextPolicy,
    session_version: i64,
) -> Result<ModelContextView, ModelContextError> {
    validate_state(state)?;
    if policy.strategy != ContextManagementStrategy::Window
        || policy.context_strategy_schema_version != CONTEXT_STRATEGY_SCHEMA_VERSION
    {
        return Err(ModelContextError::UnsupportedStrategy);
    }
    if !(MIN_MODEL_CONTEXT_BYTES..=MAX_MODEL_CONTEXT_BYTES).contains(&policy.max_context_bytes) {
        return Err(ModelContextError::InvalidBudget(policy.max_context_bytes));
    }
    if policy.history_context_bytes() == 0 {
        return Err(ModelContextError::InvalidBudget(
            policy.history_context_bytes(),
        ));
    }

    let groups = group_messages(conversation, &policy.source_context_key)?;
    let policy_key = policy.key();
    let existing_entry = state
        .entries
        .iter()
        .find(|entry| entry.policy_key == policy_key);
    if existing_entry.is_some_and(|entry| entry.strategy != policy.strategy) {
        return Err(ModelContextError::UnsupportedStrategy);
    }
    let existing_group_index = match existing_entry {
        Some(entry) if entry.floor_group_head_message_id.is_some() => {
            let id = entry
                .floor_group_head_message_id
                .as_deref()
                .unwrap_or_default();
            Some(
                groups
                    .iter()
                    .position(|group| conversation[group.start].message_id == id)
                    .ok_or_else(|| ModelContextError::MissingPersistedFloor(id.to_string()))?,
            )
        }
        Some(entry) if entry.floor_after_group_head_message_id.is_some() => {
            let id = entry
                .floor_after_group_head_message_id
                .as_deref()
                .unwrap_or_default();
            Some(
                groups
                    .iter()
                    .position(|group| conversation[group.start].message_id == id)
                    .map(|index| index + 1)
                    .ok_or_else(|| ModelContextError::MissingPersistedFloor(id.to_string()))?,
            )
        }
        _ => None,
    };
    let initial_group_index = existing_group_index.unwrap_or(0).min(groups.len());

    // A replay-unsafe group advances the suffix floor beyond that group. This
    // preserves a stable, monotonic prefix boundary rather than creating holes.
    let replay_floor = groups
        .iter()
        .enumerate()
        .skip(initial_group_index)
        .filter(|(_, group)| !group.replay_safe)
        .map(|(index, _)| index + 1)
        .max()
        .unwrap_or(initial_group_index);

    let safe_groups = &groups[replay_floor.min(groups.len())..];
    let total_cost = checked_cost_sum(safe_groups.iter().map(|group| group.cost))?;
    let mut selected_group_index = replay_floor.min(groups.len());

    let history_budget = policy.history_context_bytes();
    if total_cost > history_budget {
        let Some(last) = safe_groups.last() else {
            selected_group_index = groups.len();
            return finish_view(
                conversation,
                state,
                policy,
                session_version,
                policy_key,
                existing_group_index,
                selected_group_index,
                &groups,
            );
        };
        if last.cost > history_budget {
            return Err(ModelContextError::ContextItemTooLarge {
                group_head_message_id: conversation[last.start].message_id.clone(),
                cost: last.cost,
                high_watermark: history_budget,
            });
        }

        let low = policy.low_watermark_bytes();
        let mut used = 0usize;
        selected_group_index = groups.len() - 1;
        for index in (replay_floor..groups.len()).rev() {
            let cost = groups[index].cost;
            let next = used
                .checked_add(cost)
                .ok_or(ModelContextError::ContextCostOverflow)?;
            if index != groups.len() - 1 && next > low {
                break;
            }
            used = next;
            selected_group_index = index;
        }
    }

    finish_view(
        conversation,
        state,
        policy,
        session_version,
        policy_key,
        existing_group_index,
        selected_group_index,
        &groups,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_view(
    conversation: &[ChatMessage],
    state: &mut ModelContextState,
    policy: &PinnedContextPolicy,
    session_version: i64,
    policy_key: ContextPolicyKey,
    existing_group_index: Option<usize>,
    selected_group_index: usize,
    groups: &[MessageGroup],
) -> Result<ModelContextView, ModelContextError> {
    let old_position = existing_group_index.unwrap_or(0);
    if selected_group_index < old_position {
        return Err(ModelContextError::FloorRegression);
    }
    let floor_group_head_message_id = groups
        .get(selected_group_index)
        .map(|group| conversation[group.start].message_id.clone());
    let floor_after_group_head_message_id = (selected_group_index == groups.len())
        .then(|| {
            groups
                .last()
                .map(|group| conversation[group.start].message_id.clone())
        })
        .flatten();
    let floor_advanced = selected_group_index > old_position;
    let start = groups
        .get(selected_group_index)
        .map_or(conversation.len(), |group| group.start);

    upsert_entry(
        state,
        ModelContextEntry {
            policy_key: policy_key.clone(),
            strategy: policy.strategy,
            floor_group_head_message_id: floor_group_head_message_id.clone(),
            floor_after_group_head_message_id,
            checkpoint: None,
            last_used_session_version: session_version,
        },
    );
    Ok(ModelContextView {
        messages: conversation[start..].to_vec(),
        policy_key,
        floor_group_head_message_id,
        floor_advanced,
    })
}

pub(crate) fn validate_state(state: &ModelContextState) -> Result<(), ModelContextError> {
    if state.schema_version != MODEL_CONTEXT_STATE_SCHEMA_VERSION {
        return Err(ModelContextError::UnsupportedStateSchema(
            state.schema_version,
        ));
    }
    if state.entries.len() > MAX_CONTEXT_POLICY_ENTRIES {
        return Err(ModelContextError::TooManyPolicyEntries(state.entries.len()));
    }
    let mut keys = HashSet::new();
    for entry in &state.entries {
        match entry.strategy {
            ContextManagementStrategy::Window if entry.checkpoint.is_none() => {}
            // A discard-only floor reconciliation can legitimately precede the
            // first provider-backed checkpoint. The strategy/floor is still
            // typed and versioned; a later planner either returns a raw Ready
            // view or creates the first checkpoint.
            ContextManagementStrategy::CheckpointSummary => {}
            _ => return Err(ModelContextError::UnsupportedStrategy),
        }
        if entry.floor_group_head_message_id.is_some()
            && entry.floor_after_group_head_message_id.is_some()
        {
            return Err(ModelContextError::AmbiguousPersistedFloor);
        }
        if !keys.insert(entry.policy_key.clone()) {
            return Err(ModelContextError::DuplicatePolicyEntry);
        }
    }
    Ok(())
}

pub(crate) fn upsert_entry(state: &mut ModelContextState, entry: ModelContextEntry) {
    if let Some(existing) = state
        .entries
        .iter_mut()
        .find(|existing| existing.policy_key == entry.policy_key)
    {
        *existing = entry;
        return;
    }
    if state.entries.len() == MAX_CONTEXT_POLICY_ENTRIES {
        let evict = state
            .entries
            .iter()
            .enumerate()
            .min_by(|(_, left), (_, right)| {
                (left.last_used_session_version, left.policy_key.as_str())
                    .cmp(&(right.last_used_session_version, right.policy_key.as_str()))
            })
            .map(|(index, _)| index)
            .expect("bounded list is non-empty");
        state.entries.remove(evict);
    }
    state.entries.push(entry);
}

pub(crate) fn group_messages(
    conversation: &[ChatMessage],
    source: &SourceContextKey,
) -> Result<Vec<MessageGroup>, ModelContextError> {
    let mut groups = Vec::new();
    let mut index = 0;
    while index < conversation.len() {
        let message = &conversation[index];
        if message.role == ChatRole::Tool {
            return Err(ModelContextError::OrphanToolResult(
                message.message_id.clone(),
            ));
        }
        if message.role == ChatRole::Assistant && !message.tool_calls.is_empty() {
            let start = index;
            index += 1;
            while index < conversation.len() && conversation[index].role == ChatRole::Tool {
                index += 1;
            }
            let expected: HashSet<&str> = conversation[start]
                .tool_calls
                .iter()
                .map(|call| call.id.as_str())
                .collect();
            let actual: HashSet<&str> = conversation[start + 1..index]
                .iter()
                .filter_map(|result| result.tool_call_id.as_deref())
                .collect();
            if expected != actual || actual.len() != index - start - 1 {
                return Err(ModelContextError::IncompleteToolGroup(
                    conversation[start].message_id.clone(),
                ));
            }
            let replay = classify_replay(&conversation[start], source);
            let cost = checked_cost_sum(conversation[start..index].iter().map(model_context_cost))?;
            groups.push(MessageGroup {
                start,
                end: index,
                cost,
                replay_safe: replay.replay_safe,
                summary_eligible: replay.summary_eligible,
                discard_only: replay.discard_only,
            });
            continue;
        }
        if message.replay_disposition.is_some() {
            return Err(ModelContextError::UnexpectedReplayDisposition(
                message.message_id.clone(),
            ));
        }
        groups.push(MessageGroup {
            start: index,
            end: index + 1,
            cost: model_context_cost(message),
            replay_safe: true,
            summary_eligible: true,
            discard_only: false,
        });
        index += 1;
    }
    debug_assert!(groups.iter().all(|group| group.start < group.end));
    Ok(groups)
}

pub(crate) fn checked_cost_sum(
    costs: impl IntoIterator<Item = usize>,
) -> Result<usize, ModelContextError> {
    costs.into_iter().try_fold(0usize, |sum, cost| {
        sum.checked_add(cost)
            .ok_or(ModelContextError::ContextCostOverflow)
    })
}

#[derive(Debug, Clone, Copy)]
struct ReplayClassification {
    replay_safe: bool,
    summary_eligible: bool,
    discard_only: bool,
}

fn classify_replay(message: &ChatMessage, source: &SourceContextKey) -> ReplayClassification {
    match message.replay_disposition.as_ref() {
        Some(ReplayDisposition::NotRequired { source_context_key }) => ReplayClassification {
            replay_safe: source_context_key == source,
            summary_eligible: true,
            discard_only: false,
        },
        Some(ReplayDisposition::Present { envelope }) if envelope.validate().is_err() => {
            ReplayClassification {
                replay_safe: false,
                summary_eligible: false,
                discard_only: true,
            }
        }
        Some(ReplayDisposition::Present { envelope }) => ReplayClassification {
            replay_safe: &envelope.source_context_key == source,
            summary_eligible: true,
            discard_only: false,
        },
        Some(ReplayDisposition::Unavailable { .. }) | None => ReplayClassification {
            replay_safe: false,
            summary_eligible: true,
            discard_only: false,
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelContextError {
    UnsupportedStateSchema(u16),
    TooManyPolicyEntries(usize),
    DuplicatePolicyEntry,
    AmbiguousPersistedFloor,
    UnsupportedStrategy,
    InvalidProfileRevision(i64),
    InvalidBudget(usize),
    MissingPersistedFloor(String),
    FloorRegression,
    OrphanToolResult(String),
    IncompleteToolGroup(String),
    UnexpectedReplayDisposition(String),
    InvalidProtectionReference(String),
    ProtectedStateTooLarge {
        cost: usize,
        high_watermark: usize,
    },
    ProtectedReplayUnsafe(String),
    NoCompressiblePrefix,
    CompressionInputTooLarge,
    ContextCostOverflow,
    SummaryTooLarge,
    InvalidCheckpoint(String),
    StaleCompressionPlan,
    ContextItemTooLarge {
        group_head_message_id: String,
        cost: usize,
        high_watermark: usize,
    },
}

impl std::fmt::Display for ModelContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedStateSchema(version) => {
                write!(f, "unsupported model context state version: {version}")
            }
            Self::TooManyPolicyEntries(count) => {
                write!(f, "model context state contains {count} policy entries")
            }
            Self::DuplicatePolicyEntry => f.write_str("duplicate model context policy entry"),
            Self::AmbiguousPersistedFloor => {
                f.write_str("model context entry contains two floor cursors")
            }
            Self::UnsupportedStrategy => f.write_str("unsupported model context strategy state"),
            Self::InvalidProfileRevision(revision) => {
                write!(f, "invalid context profile revision: {revision}")
            }
            Self::InvalidBudget(value) => write!(f, "invalid model context budget: {value}"),
            Self::MissingPersistedFloor(id) => {
                write!(f, "persisted model context floor no longer exists: {id}")
            }
            Self::FloorRegression => f.write_str("model context floor attempted to move backward"),
            Self::OrphanToolResult(id) => write!(f, "orphan tool result in conversation: {id}"),
            Self::IncompleteToolGroup(id) => {
                write!(f, "incomplete assistant tool-call group: {id}")
            }
            Self::UnexpectedReplayDisposition(id) => {
                write!(f, "non-tool-call message carries replay disposition: {id}")
            }
            Self::InvalidProtectionReference(id) => {
                write!(
                    f,
                    "context protection references missing message/tool id: {id}"
                )
            }
            Self::ProtectedStateTooLarge {
                cost,
                high_watermark,
            } => write!(
                f,
                "protected model context costs {cost} bytes, above {high_watermark}"
            ),
            Self::ProtectedReplayUnsafe(id) => {
                write!(f, "protected model context group is not replay-safe: {id}")
            }
            Self::NoCompressiblePrefix => f.write_str("no safe model context prefix to compress"),
            Self::CompressionInputTooLarge => {
                f.write_str("model context compression input exceeds its hard limit")
            }
            Self::ContextCostOverflow => f.write_str("model context cost overflow"),
            Self::SummaryTooLarge => f.write_str("context summary exceeds its hard limit"),
            Self::InvalidCheckpoint(reason) => write!(f, "invalid context checkpoint: {reason}"),
            Self::StaleCompressionPlan => f.write_str("context compression plan is stale"),
            Self::ContextItemTooLarge {
                group_head_message_id,
                cost,
                high_watermark,
            } => write!(
                f,
                "latest model context group {group_head_message_id} costs {cost} bytes, above {high_watermark}"
            ),
        }
    }
}

impl std::error::Error for ModelContextError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ToolCallRef;
    use crate::model_profile::WireProtocol;

    fn source(label: &str) -> SourceContextKey {
        SourceContextKey::derive(
            WireProtocol::OpenAiChatCompletions,
            label,
            "model:1",
            "test",
        )
    }

    fn policy(label: &str, high: usize) -> PinnedContextPolicy {
        PinnedContextPolicy::window(source(label), 1, high).unwrap()
    }

    fn user(id: &str, size: usize) -> ChatMessage {
        ChatMessage::text(id, ChatRole::User, "x".repeat(size))
    }

    fn tool_group(source: SourceContextKey) -> Vec<ChatMessage> {
        vec![
            ChatMessage::assistant_tool_calls_with_replay(
                "a1",
                "",
                vec![ToolCallRef {
                    id: "c1".to_string(),
                    name: "read".to_string(),
                    arguments_json: "{}".to_string(),
                }],
                ReplayDisposition::NotRequired {
                    source_context_key: source,
                },
            ),
            ChatMessage::tool_result("t1", "c1", "ok"),
        ]
    }

    #[test]
    fn per_step_request_overhead_reduces_history_without_changing_policy_identity() {
        let base = policy("overhead", MIN_MODEL_CONTEXT_BYTES * 2);
        let key = base.key();
        let reserved = base
            .clone()
            .with_request_overhead_bytes(MIN_MODEL_CONTEXT_BYTES)
            .unwrap();
        assert_eq!(reserved.key(), key);
        assert_eq!(reserved.history_context_bytes(), MIN_MODEL_CONTEXT_BYTES);

        let conversation = vec![user("old", MIN_MODEL_CONTEXT_BYTES), user("new", 512)];
        let mut state = ModelContextState::default();
        let view = build_model_context_view(&conversation, &mut state, &reserved, 1).unwrap();
        assert!(
            view.messages.iter().map(model_context_cost).sum::<usize>()
                <= reserved.history_context_bytes()
        );
    }

    #[test]
    fn under_high_keeps_append_only_floor() {
        let policy = policy("same", MIN_MODEL_CONTEXT_BYTES);
        let conversation = vec![user("u1", 10), user("u2", 10)];
        let mut state = ModelContextState::default();
        let first = build_model_context_view(&conversation, &mut state, &policy, 1).unwrap();
        assert_eq!(first.messages, conversation);
        assert!(!first.floor_advanced);
        let mut appended = conversation;
        appended.push(user("u3", 10));
        let second = build_model_context_view(&appended, &mut state, &policy, 2).unwrap();
        assert_eq!(second.messages, appended);
        assert!(!second.floor_advanced);
    }

    #[test]
    fn over_high_trims_to_low_and_floor_never_regresses() {
        let policy = policy("same", MIN_MODEL_CONTEXT_BYTES);
        let conversation = vec![user("u1", 2000), user("u2", 2000), user("u3", 1000)];
        let mut state = ModelContextState::default();
        let first = build_model_context_view(&conversation, &mut state, &policy, 1).unwrap();
        assert!(first.floor_advanced);
        assert_eq!(first.messages[0].message_id, "u2");
        let floor = first.floor_group_head_message_id;

        let second = build_model_context_view(&conversation, &mut state, &policy, 2).unwrap();
        assert!(!second.floor_advanced);
        assert_eq!(second.floor_group_head_message_id, floor);
    }

    #[test]
    fn latest_group_between_low_and_high_is_kept() {
        let policy = policy("same", MIN_MODEL_CONTEXT_BYTES);
        let conversation = vec![user("old", 2000), user("latest", 3200)];
        let mut state = ModelContextState::default();
        let view = build_model_context_view(&conversation, &mut state, &policy, 1).unwrap();
        assert_eq!(view.messages.len(), 1);
        assert_eq!(view.messages[0].message_id, "latest");
    }

    #[test]
    fn latest_group_over_high_fails_without_mutating_state() {
        let policy = policy("same", MIN_MODEL_CONTEXT_BYTES);
        let conversation = vec![user("latest", 5000)];
        let mut state = ModelContextState::default();
        let before = state.clone();
        assert!(matches!(
            build_model_context_view(&conversation, &mut state, &policy, 1),
            Err(ModelContextError::ContextItemTooLarge { .. })
        ));
        assert_eq!(state, before);
    }

    #[test]
    fn source_mismatch_omits_the_whole_tool_group() {
        let old_source = source("old");
        let mut conversation = vec![user("u1", 10)];
        conversation.extend(tool_group(old_source));
        conversation.push(user("u2", 10));
        let policy = policy("new", MIN_MODEL_CONTEXT_BYTES);
        let mut state = ModelContextState::default();
        let view = build_model_context_view(&conversation, &mut state, &policy, 1).unwrap();
        assert_eq!(view.messages.len(), 1);
        assert_eq!(view.messages[0].message_id, "u2");
        assert!(view.floor_advanced);
    }

    #[test]
    fn excluded_tail_cursor_does_not_reannounce_on_next_append() {
        let old_source = source("old");
        let mut conversation = vec![user("u1", 10)];
        conversation.extend(tool_group(old_source));
        let policy = policy("new", MIN_MODEL_CONTEXT_BYTES);
        let mut state = ModelContextState::default();

        let first = build_model_context_view(&conversation, &mut state, &policy, 1).unwrap();
        assert!(first.messages.is_empty());
        assert!(first.floor_advanced);
        let entry = &state.entries[0];
        assert_eq!(
            entry.floor_after_group_head_message_id.as_deref(),
            Some("a1")
        );

        conversation.push(user("u2", 10));
        let second = build_model_context_view(&conversation, &mut state, &policy, 2).unwrap();
        assert_eq!(second.messages.len(), 1);
        assert_eq!(second.messages[0].message_id, "u2");
        assert!(!second.floor_advanced);
        assert_eq!(
            state.entries[0].floor_group_head_message_id.as_deref(),
            Some("u2")
        );
        assert!(state.entries[0].floor_after_group_head_message_id.is_none());
    }

    #[test]
    fn context_entries_are_bounded_and_deterministically_evicted() {
        let conversation = vec![user("u1", 10)];
        let mut state = ModelContextState::default();
        for revision in 1..=9 {
            let mut policy = policy("same", MIN_MODEL_CONTEXT_BYTES);
            policy.profile_revision = revision;
            build_model_context_view(&conversation, &mut state, &policy, revision).unwrap();
        }
        assert_eq!(state.entries.len(), MAX_CONTEXT_POLICY_ENTRIES);
        assert!(
            state
                .entries
                .iter()
                .all(|entry| entry.last_used_session_version >= 2)
        );
    }

    #[test]
    fn platform_policy_defaults_to_summary_revision_zero_and_revision_is_keyed() {
        let platform = PlatformContextPolicy::default();
        assert_eq!(
            platform.schema_version,
            PLATFORM_CONTEXT_POLICY_SCHEMA_VERSION
        );
        assert_eq!(platform.revision, 0);
        assert_eq!(
            platform.strategy,
            ContextManagementStrategy::CheckpointSummary
        );

        let zero =
            PinnedContextPolicy::checkpoint_summary(source("same"), 1, MIN_MODEL_CONTEXT_BYTES, 0)
                .unwrap();
        let one =
            PinnedContextPolicy::checkpoint_summary(source("same"), 1, MIN_MODEL_CONTEXT_BYTES, 1)
                .unwrap();
        assert_ne!(zero.key(), one.key());
        let window = platform
            .candidate(ContextManagementStrategy::Window)
            .unwrap();
        assert_eq!(window.revision, 1);
        let pinned = window
            .pin(source("same"), 1, MIN_MODEL_CONTEXT_BYTES)
            .unwrap();
        assert_eq!(pinned.strategy, ContextManagementStrategy::Window);
        assert_ne!(pinned.key(), zero.key());
        let mut invalid = platform.clone();
        invalid.schema_version += 1;
        assert!(
            invalid
                .pin(source("same"), 1, MIN_MODEL_CONTEXT_BYTES)
                .is_err()
        );
        invalid = platform;
        invalid.revision = u64::MAX;
        assert!(
            invalid
                .candidate(ContextManagementStrategy::Window)
                .is_err()
        );
    }

    #[test]
    fn context_cost_accumulation_fails_closed_on_overflow() {
        assert_eq!(
            checked_cost_sum([usize::MAX, 1]),
            Err(ModelContextError::ContextCostOverflow)
        );
    }

    #[test]
    fn versioned_context_state_rejects_unknown_fields() {
        assert!(
            serde_json::from_value::<ModelContextState>(serde_json::json!({
                "schema_version": MODEL_CONTEXT_STATE_SCHEMA_VERSION,
                "entries": [],
                "future_state": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ModelContextState>(serde_json::json!({
                "schema_version": MODEL_CONTEXT_STATE_SCHEMA_VERSION,
                "entries": [{
                    "policy_key": "v1:test",
                    "strategy": "window",
                    "last_used_session_version": 1,
                    "future_entry": true
                }]
            }))
            .is_err()
        );
    }

    #[test]
    fn context_summary_is_grouped_and_window_rejects_a_strategy_mismatch() {
        let source = source("same");
        let mut conversation = vec![user("old", 10)];
        conversation.extend(tool_group(source.clone()));
        conversation.push(user("recent", 10));

        // A checkpoint may cover through the complete tool group, then prepend one
        // synthetic summary to the untouched raw suffix.
        let raw_suffix = conversation[3..].to_vec();
        let mut future_view = vec![ChatMessage::context_summary(
            "checkpoint:future",
            "The user asked for a diagnosis; one read-only inspection completed.",
        )];
        future_view.extend(raw_suffix);
        let groups = group_messages(&future_view, &source).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(future_view[0].role, ChatRole::ContextSummary);
        assert!(model_context_cost(&future_view[0]) > future_view[0].text.len());

        let policy = policy("same", MIN_MODEL_CONTEXT_BYTES);
        let mut state = ModelContextState {
            schema_version: MODEL_CONTEXT_STATE_SCHEMA_VERSION,
            entries: vec![ModelContextEntry {
                policy_key: policy.key(),
                strategy: ContextManagementStrategy::Window,
                floor_group_head_message_id: None,
                floor_after_group_head_message_id: None,
                checkpoint: None,
                last_used_session_version: 1,
            }],
        };
        state.entries[0].strategy = ContextManagementStrategy::CheckpointSummary;
        assert_eq!(
            build_model_context_view(&conversation, &mut state, &policy, 1),
            Err(ModelContextError::UnsupportedStrategy)
        );
    }
}
