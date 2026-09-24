//! Bounded per-input working state for a long-lived AI Assistant conversation.
//!
//! `input_revision` is the epoch identity. A new user input deterministically
//! resets this state; permission decisions and background continuations inherit
//! it. Durable action facts remain outside the epoch and are never deleted by a
//! focus reset.

use serde::{Deserialize, Serialize};

use crate::{chat::TokenUsage, context_attachment::MAX_CONTEXT_ATTACHMENTS};

pub const FOCUS_EPOCH_SCHEMA_VERSION: u16 = 2;
pub const MAX_FOCUS_EPOCH_STEPS: u32 = 160;
pub const MAX_FOCUS_ATTACHMENT_ID_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalSegmentIdentity {
    pub goal_id: String,
    pub source_message_id: String,
    pub segment_seq: u32,
    pub goal_revision: u64,
    pub lease_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusEpochState {
    pub schema_version: u16,
    pub input_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_segment: Option<GoalSegmentIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_status_input_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_attachment_ids: Vec<String>,
    #[serde(default)]
    pub steps_used: u32,
    #[serde(default)]
    pub tokens_used: TokenUsage,
    #[serde(default)]
    pub history_lookup_calls: u16,
    #[serde(default)]
    pub history_result_bytes: u64,
}

impl Default for FocusEpochState {
    fn default() -> Self {
        Self {
            schema_version: FOCUS_EPOCH_SCHEMA_VERSION,
            input_revision: 0,
            goal_segment: None,
            task_status_input_revision: None,
            selected_attachment_ids: Vec::new(),
            steps_used: 0,
            tokens_used: TokenUsage::default(),
            history_lookup_calls: 0,
            history_result_bytes: 0,
        }
    }
}

impl FocusEpochState {
    pub fn reset(
        &mut self,
        input_revision: u64,
        selected_attachment_ids: impl IntoIterator<Item = String>,
    ) -> Result<(), &'static str> {
        let selected_attachment_ids = canonical_attachment_ids(selected_attachment_ids)?;
        *self = Self {
            input_revision,
            selected_attachment_ids,
            ..Self::default()
        };
        Ok(())
    }

    pub fn bind_task_status(&mut self) {
        self.task_status_input_revision = Some(self.input_revision);
    }

    /// Start another bounded work segment for the same actual user input.
    /// Durable action, approval, and lifetime facts live outside this state.
    pub fn begin_goal_segment(
        &mut self,
        input_revision: u64,
        goal_id: &str,
        source_message_id: &str,
        segment_seq: u32,
        goal_revision: u64,
        lease_epoch: u64,
    ) -> Result<(), &'static str> {
        if self.schema_version != FOCUS_EPOCH_SCHEMA_VERSION
            || self.input_revision != input_revision
            || goal_id.is_empty()
            || goal_id.len() > 256
            || goal_id.trim() != goal_id
            || source_message_id.is_empty()
            || source_message_id.len() > 256
            || source_message_id.trim() != source_message_id
            || segment_seq == 0
            || goal_revision == 0
            || lease_epoch == 0
            || self.goal_segment.as_ref().is_some_and(|previous| {
                previous.goal_id != goal_id
                    || previous.source_message_id != source_message_id
                    || segment_seq <= previous.segment_seq
                    || lease_epoch <= previous.lease_epoch
                    || goal_revision < previous.goal_revision
            })
        {
            return Err("invalid goal segment identity");
        }
        self.goal_segment = Some(GoalSegmentIdentity {
            goal_id: goal_id.to_owned(),
            source_message_id: source_message_id.to_owned(),
            segment_seq,
            goal_revision,
            lease_epoch,
        });
        self.steps_used = 0;
        self.tokens_used = TokenUsage::default();
        self.history_lookup_calls = 0;
        self.history_result_bytes = 0;
        Ok(())
    }

    pub fn record_step(&mut self, usage: TokenUsage) {
        self.steps_used = self.steps_used.saturating_add(1);
        add_usage(&mut self.tokens_used, usage);
    }

    pub fn record_tokens(&mut self, usage: TokenUsage) {
        add_usage(&mut self.tokens_used, usage);
    }

    pub fn step_budget_exhausted(&self) -> bool {
        self.steps_used >= MAX_FOCUS_EPOCH_STEPS
    }

    pub fn record_history_result(&mut self, bytes: usize) -> Result<(), &'static str> {
        let next_calls = self.history_lookup_calls.saturating_add(1);
        let next_bytes = self.history_result_bytes.saturating_add(bytes as u64);
        if next_calls > crate::conversation_history::MAX_HISTORY_LOOKUPS_PER_FOCUS
            || next_bytes > crate::conversation_history::MAX_HISTORY_BYTES_PER_FOCUS
        {
            return Err("focus epoch history lookup budget exceeded");
        }
        self.history_lookup_calls = next_calls;
        self.history_result_bytes = next_bytes;
        Ok(())
    }

    pub fn validate(
        &self,
        session_input_revision: u64,
        has_task_status: bool,
    ) -> Result<(), &'static str> {
        if self.schema_version != FOCUS_EPOCH_SCHEMA_VERSION {
            return Err("unsupported focus epoch schema version");
        }
        if self.input_revision != session_input_revision {
            return Err("focus epoch input revision mismatch");
        }
        if self.goal_segment.as_ref().is_some_and(|segment| {
            segment.goal_id.is_empty()
                || segment.goal_id.len() > 256
                || segment.goal_id.trim() != segment.goal_id
                || segment.source_message_id.is_empty()
                || segment.source_message_id.len() > 256
                || segment.source_message_id.trim() != segment.source_message_id
                || segment.segment_seq == 0
                || segment.goal_revision == 0
                || segment.lease_epoch == 0
        }) {
            return Err("invalid goal segment identity");
        }
        if self.steps_used > MAX_FOCUS_EPOCH_STEPS {
            return Err("focus epoch step budget exceeded");
        }
        if self.history_lookup_calls > crate::conversation_history::MAX_HISTORY_LOOKUPS_PER_FOCUS
            || self.history_result_bytes > crate::conversation_history::MAX_HISTORY_BYTES_PER_FOCUS
        {
            return Err("focus epoch history lookup budget exceeded");
        }
        if self.task_status_input_revision != has_task_status.then_some(self.input_revision) {
            return Err("task status is not bound to the current focus epoch");
        }
        canonical_attachment_ids(self.selected_attachment_ids.clone())?;
        Ok(())
    }
}

fn canonical_attachment_ids(
    ids: impl IntoIterator<Item = String>,
) -> Result<Vec<String>, &'static str> {
    let mut ids = ids.into_iter().collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    if ids.len() > MAX_CONTEXT_ATTACHMENTS {
        return Err("too many focus epoch attachment ids");
    }
    if ids
        .iter()
        .any(|id| id.is_empty() || id.len() > MAX_FOCUS_ATTACHMENT_ID_BYTES)
    {
        return Err("invalid focus epoch attachment id");
    }
    Ok(ids)
}

fn add_usage(total: &mut TokenUsage, delta: TokenUsage) {
    total.input_tokens = add(total.input_tokens, delta.input_tokens);
    total.output_tokens = add(total.output_tokens, delta.output_tokens);
    total.cache_read_tokens = add(total.cache_read_tokens, delta.cache_read_tokens);
    total.cache_write_tokens = add(total.cache_write_tokens, delta.cache_write_tokens);
}

fn add(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_is_replace_canonical_and_resets_budget() {
        let mut state = FocusEpochState {
            input_revision: 1,
            task_status_input_revision: Some(1),
            selected_attachment_ids: vec!["old".into()],
            steps_used: 9,
            tokens_used: TokenUsage {
                input_tokens: Some(10),
                ..Default::default()
            },
            history_lookup_calls: 2,
            history_result_bytes: 2048,
            ..Default::default()
        };
        state
            .reset(2, ["b".to_string(), "a".to_string(), "a".to_string()])
            .unwrap();
        assert_eq!(state.input_revision, 2);
        assert_eq!(state.selected_attachment_ids, ["a", "b"]);
        assert_eq!(state.steps_used, 0);
        assert_eq!(state.tokens_used, TokenUsage::default());
        assert_eq!(state.history_lookup_calls, 0);
        assert_eq!(state.history_result_bytes, 0);
        assert_eq!(state.task_status_input_revision, None);
    }

    #[test]
    fn validation_rejects_cross_epoch_task_status_and_over_budget_state() {
        let mut state = FocusEpochState {
            input_revision: 3,
            task_status_input_revision: Some(2),
            ..Default::default()
        };
        assert!(state.validate(3, true).is_err());
        state.task_status_input_revision = Some(3);
        state.steps_used = MAX_FOCUS_EPOCH_STEPS + 1;
        assert!(state.validate(3, true).is_err());
    }

    #[test]
    fn goal_segment_resets_only_temporary_working_budget() {
        let mut state = FocusEpochState::default();
        state.reset(3, ["attachment".to_string()]).unwrap();
        state.bind_task_status();
        state.record_step(TokenUsage {
            input_tokens: Some(20),
            ..Default::default()
        });
        state.record_history_result(64).unwrap();
        state
            .begin_goal_segment(3, "goal", "source", 1, 1, 1)
            .unwrap();
        assert_eq!(state.steps_used, 0);
        assert_eq!(state.tokens_used, TokenUsage::default());
        assert_eq!(state.history_lookup_calls, 0);
        assert_eq!(state.selected_attachment_ids, ["attachment"]);
        assert_eq!(state.task_status_input_revision, Some(3));
        assert_eq!(
            state.begin_goal_segment(3, "goal", "source", 1, 1, 1),
            Err("invalid goal segment identity")
        );
        state
            .begin_goal_segment(3, "goal", "source", 2, 1, 2)
            .unwrap();
    }
}
