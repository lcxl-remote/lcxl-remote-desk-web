//! Versioned, provider-neutral model history views and persisted window state.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::chat::{ChatMessage, ChatRole};
use crate::replay::{ReplayDisposition, SourceContextKey};
use crate::trim::model_context_cost;
use crate::{MAX_MODEL_CONTEXT_BYTES, MIN_MODEL_CONTEXT_BYTES};

pub const MODEL_CONTEXT_STATE_SCHEMA_VERSION: u16 = 1;
pub const CONTEXT_STRATEGY_SCHEMA_VERSION: u16 = 1;
pub const MAX_CONTEXT_POLICY_ENTRIES: usize = 8;
pub const MAX_CONTEXT_NOTICES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextManagementStrategy {
    Window,
}

impl ContextManagementStrategy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Window => "window",
        }
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
    pub strategy: ContextManagementStrategy,
    pub platform_context_policy_revision: i64,
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
            strategy: ContextManagementStrategy::Window,
            platform_context_policy_revision: 1,
            context_strategy_schema_version: CONTEXT_STRATEGY_SCHEMA_VERSION,
        })
    }

    pub fn key(&self) -> ContextPolicyKey {
        ContextPolicyKey::derive(self)
    }

    pub const fn low_watermark_bytes(&self) -> usize {
        self.max_context_bytes - self.max_context_bytes / 4
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelContextState {
    pub schema_version: u16,
    #[serde(default)]
    pub entries: Vec<ModelContextEntry>,
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
    /// Reserved for the separately planned checkpoint-summary strategy. Version
    /// 1 rejects every non-null value instead of guessing a future schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<serde_json::Value>,
    pub last_used_session_version: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextNoticeKind {
    Trimmed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextNotice {
    pub id: String,
    pub turn_id: String,
    pub kind: ContextNoticeKind,
}

impl ContextNotice {
    pub fn trimmed(turn_id: impl Into<String>) -> Self {
        let turn_id = turn_id.into();
        Self {
            id: format!("context-trimmed:{turn_id}"),
            turn_id,
            kind: ContextNoticeKind::Trimmed,
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
struct MessageGroup {
    start: usize,
    end: usize,
    cost: usize,
    replay_safe: bool,
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

    let groups = group_messages(conversation, &policy.source_context_key)?;
    let policy_key = policy.key();
    let existing_entry = state
        .entries
        .iter()
        .find(|entry| entry.policy_key == policy_key);
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
    let total_cost = safe_groups
        .iter()
        .fold(0usize, |total, group| total.saturating_add(group.cost));
    let mut selected_group_index = replay_floor.min(groups.len());

    if total_cost > policy.max_context_bytes {
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
        if last.cost > policy.max_context_bytes {
            return Err(ModelContextError::ContextItemTooLarge {
                group_head_message_id: conversation[last.start].message_id.clone(),
                cost: last.cost,
                high_watermark: policy.max_context_bytes,
            });
        }

        let low = policy.low_watermark_bytes();
        let mut used = 0usize;
        selected_group_index = groups.len() - 1;
        for index in (replay_floor..groups.len()).rev() {
            let cost = groups[index].cost;
            if index != groups.len() - 1 && used.saturating_add(cost) > low {
                break;
            }
            used = used.saturating_add(cost);
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

fn validate_state(state: &ModelContextState) -> Result<(), ModelContextError> {
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
        if entry.strategy != ContextManagementStrategy::Window || entry.checkpoint.is_some() {
            return Err(ModelContextError::UnsupportedStrategy);
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

fn upsert_entry(state: &mut ModelContextState, entry: ModelContextEntry) {
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

fn group_messages(
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
            let replay_safe = replay_is_safe(&conversation[start], source);
            groups.push(MessageGroup {
                start,
                end: index,
                cost: conversation[start..index]
                    .iter()
                    .fold(0usize, |total, item| {
                        total.saturating_add(model_context_cost(item))
                    }),
                replay_safe,
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
        });
        index += 1;
    }
    debug_assert!(groups.iter().all(|group| group.start < group.end));
    Ok(groups)
}

fn replay_is_safe(message: &ChatMessage, source: &SourceContextKey) -> bool {
    match message.replay_disposition.as_ref() {
        Some(ReplayDisposition::NotRequired { source_context_key }) => source_context_key == source,
        Some(ReplayDisposition::Present { envelope }) => {
            &envelope.source_context_key == source && envelope.validate().is_ok()
        }
        Some(ReplayDisposition::Unavailable { .. }) | None => false,
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
    fn future_checkpoint_seam_can_prepend_a_fenced_summary_at_a_group_boundary() {
        let source = source("same");
        let mut conversation = vec![user("old", 10)];
        conversation.extend(tool_group(source.clone()));
        conversation.push(user("recent", 10));

        // A future checkpoint may cover through the complete tool group, then
        // prepend one synthetic summary to the untouched raw suffix. This test is
        // deliberately construction-only: the v1 runtime state below still
        // rejects any non-null checkpoint.
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
                checkpoint: Some(serde_json::json!({"future": true})),
                last_used_session_version: 1,
            }],
        };
        assert_eq!(
            build_model_context_view(&conversation, &mut state, &policy, 1),
            Err(ModelContextError::UnsupportedStrategy)
        );
    }
}
