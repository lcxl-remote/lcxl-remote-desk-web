//! Metadata-only context occupancy based on the most recently prepared window.

use crate::{chat::ChatMessage, model_context::PinnedContextPolicy, trim::model_context_cost};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsageBasis {
    pub limit_bytes: usize,
    pub strategy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_budget: Option<ContextRequestBudget>,
    retained_ids: Vec<String>,
    synthetic_bytes: usize,
    observed_len: usize,
    observed_tail_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextUsage {
    pub used_bytes: usize,
    pub limit_bytes: usize,
    pub strategy: String,
    pub breakdown: ContextUsageBreakdown,
    pub request_budget: Option<ContextRequestBudget>,
}

/// Metadata for the overhead reserved before selecting the history window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRequestBudget {
    pub total_bytes: usize,
    pub system_prompt_bytes: usize,
    pub tool_definitions_bytes: usize,
    pub other_overhead_bytes: usize,
}

impl ContextRequestBudget {
    pub fn matches_history_limit(&self, limit_bytes: usize) -> bool {
        self.system_prompt_bytes
            .checked_add(self.tool_definitions_bytes)
            .and_then(|overhead| overhead.checked_add(self.other_overhead_bytes))
            .and_then(|overhead| self.total_bytes.checked_sub(overhead))
            .is_some_and(|remaining| remaining > 0 && remaining == limit_bytes)
    }
}

/// Costs of the prepared history only; contains no message or replay content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextUsageBreakdown {
    pub messages_bytes: usize,
    pub tools_bytes: usize,
    pub replay_bytes: usize,
    pub projected_bytes: usize,
}

impl ContextUsageBasis {
    pub fn observe(
        history: &[ChatMessage],
        projected: &[ChatMessage],
        policy: &PinnedContextPolicy,
    ) -> Self {
        let ids: std::collections::HashSet<_> =
            history.iter().map(|m| m.message_id.as_str()).collect();
        Self {
            limit_bytes: policy.history_context_bytes(),
            request_budget: None,
            strategy: match policy.strategy {
                crate::model_context::ContextManagementStrategy::Window => "window",
                crate::model_context::ContextManagementStrategy::CheckpointSummary => {
                    "checkpoint_summary"
                }
            }
            .into(),
            retained_ids: projected
                .iter()
                .filter(|m| ids.contains(m.message_id.as_str()))
                .map(|m| m.message_id.clone())
                .collect(),
            synthetic_bytes: projected
                .iter()
                .filter(|m| !ids.contains(m.message_id.as_str()))
                .map(model_context_cost)
                .sum(),
            observed_len: history.len(),
            observed_tail_id: history.last().map(|m| m.message_id.clone()),
        }
    }

    /// Missing or inconsistent display metadata cannot invalidate history usage.
    pub fn with_request_budget(mut self, budget: Option<ContextRequestBudget>) -> Self {
        self.request_budget =
            budget.filter(|budget| budget.matches_history_limit(self.limit_bytes));
        self
    }

    /// Include replies, tool results and newly accepted input since observation.
    /// The next step may choose different tools/overhead, so this is not a promise
    /// that the same number of user-entered bytes will fit on the next request.
    pub fn usage(&self, history: &[ChatMessage]) -> Option<ContextUsage> {
        if self.limit_bytes == 0
            || history.len() < self.observed_len
            || self
                .observed_len
                .checked_sub(1)
                .and_then(|i| history.get(i))
                .map(|m| &m.message_id)
                != self.observed_tail_id.as_ref()
        {
            return None;
        }
        let ids: std::collections::HashSet<_> = self.retained_ids.iter().collect();
        let mut breakdown = ContextUsageBreakdown {
            projected_bytes: self.synthetic_bytes,
            ..Default::default()
        };
        let mut used_bytes = self.synthetic_bytes;
        for (_, message) in history
            .iter()
            .enumerate()
            .filter(|(i, m)| *i >= self.observed_len || ids.contains(&m.message_id))
        {
            let cost = model_context_cost(message);
            let replay = message
                .replay_disposition
                .as_ref()
                .map_or(0, crate::replay::ReplayDisposition::model_context_cost);
            breakdown.replay_bytes = breakdown.replay_bytes.checked_add(replay)?;
            let category =
                if message.role == crate::chat::ChatRole::Tool || !message.tool_calls.is_empty() {
                    &mut breakdown.tools_bytes
                } else {
                    &mut breakdown.messages_bytes
                };
            *category = category.checked_add(cost.checked_sub(replay)?)?;
            used_bytes = used_bytes.checked_add(cost)?;
        }
        Some(ContextUsage {
            used_bytes,
            limit_bytes: self.limit_bytes,
            strategy: self.strategy.clone(),
            breakdown,
            request_budget: self
                .request_budget
                .as_ref()
                .filter(|budget| budget.matches_history_limit(self.limit_bytes))
                .cloned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{chat::ChatRole, model_profile::WireProtocol, replay::SourceContextKey};

    fn policy() -> PinnedContextPolicy {
        PinnedContextPolicy::window(
            SourceContextKey::derive(
                WireProtocol::OpenAiChatCompletions,
                "provider",
                "model",
                "test",
            ),
            1,
            crate::MIN_MODEL_CONTEXT_BYTES * 2,
        )
        .unwrap()
        .with_request_overhead_bytes(1024)
        .unwrap()
    }

    #[test]
    fn request_budget_survives_persistence_without_recounting_history() {
        let policy = policy();
        let budget = ContextRequestBudget {
            total_bytes: policy.max_context_bytes,
            system_prompt_bytes: 256,
            tool_definitions_bytes: 512,
            other_overhead_bytes: 256,
        };
        let message = ChatMessage::text("u", ChatRole::User, "private conversation body");
        let mut history = vec![message.clone()];
        let basis = ContextUsageBasis::observe(&history, &history, &policy)
            .with_request_budget(Some(budget.clone()));
        let encoded = serde_json::to_string(&basis).unwrap();
        assert!(!encoded.contains(&message.text));
        let restored: ContextUsageBasis = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored, basis);
        let reply = ChatMessage::text("a", ChatRole::Assistant, "new response");
        history.push(reply.clone());
        let usage = restored.usage(&history).unwrap();
        assert_eq!(usage.request_budget, Some(budget));
        assert_eq!(usage.limit_bytes, policy.history_context_bytes());
        assert_eq!(
            usage.used_bytes,
            model_context_cost(&message) + model_context_cost(&reply)
        );
        assert_eq!(usage.breakdown.messages_bytes, usage.used_bytes);
        assert!(restored.usage(&[]).is_none());
    }

    #[test]
    fn unknown_or_invalid_request_metadata_does_not_invalidate_history_usage() {
        let policy = policy();
        let basis = ContextUsageBasis::observe(&[], &[], &policy);
        assert!(basis.usage(&[]).unwrap().request_budget.is_none());
        let valid = ContextRequestBudget {
            total_bytes: policy.max_context_bytes,
            system_prompt_bytes: 256,
            tool_definitions_bytes: 512,
            other_overhead_bytes: 256,
        };
        let invalid_budgets = [
            ContextRequestBudget {
                other_overhead_bytes: 255,
                ..valid.clone()
            },
            ContextRequestBudget {
                total_bytes: 512,
                ..valid.clone()
            },
            ContextRequestBudget {
                system_prompt_bytes: usize::MAX,
                ..valid.clone()
            },
            ContextRequestBudget {
                total_bytes: 1024,
                ..valid
            },
        ];
        for invalid in invalid_budgets {
            let mut restored = basis.clone();
            restored.request_budget = Some(invalid.clone());
            assert!(restored.usage(&[]).unwrap().request_budget.is_none());
            assert_eq!(
                basis.clone().with_request_budget(Some(invalid)).usage(&[]),
                basis.usage(&[])
            );
        }
    }

    #[test]
    fn counts_retained_history_summary_and_new_results_without_omitted_history() {
        let old = ChatMessage::text("old", ChatRole::User, "old content".repeat(100));
        let current = ChatMessage::text("current", ChatRole::User, "你好");
        let summary = ChatMessage::text("summary", ChatRole::ContextSummary, "summary");
        let mut history = vec![old, current.clone()];
        let policy = policy();
        let basis =
            ContextUsageBasis::observe(&history, &[summary.clone(), current.clone()], &policy);
        let result = ChatMessage::text("result", ChatRole::Assistant, "result");
        history.push(result.clone());
        let usage = basis.usage(&history).unwrap();
        assert_eq!(usage.limit_bytes, policy.max_context_bytes - 1024);
        assert_eq!(
            usage.used_bytes,
            model_context_cost(&summary)
                + model_context_cost(&current)
                + model_context_cost(&result)
        );
        let restored: ContextUsageBasis =
            serde_json::from_str(&serde_json::to_string(&basis).unwrap()).unwrap();
        assert_eq!(restored.usage(&history), Some(usage));
        let refreshed =
            ContextUsageBasis::observe(&history, std::slice::from_ref(&result), &policy);
        assert_eq!(
            refreshed.usage(&history).unwrap().used_bytes,
            model_context_cost(&result)
        );
        history.clear();
        assert!(basis.usage(&history).is_none());
    }

    #[test]
    fn preserves_strategy_and_reports_overflow_without_clamping_actual_usage() {
        let mut policy = policy();
        policy.strategy = crate::model_context::ContextManagementStrategy::CheckpointSummary;
        let basis = ContextUsageBasis::observe(&[], &[], &policy);
        let message = ChatMessage::text(
            "large",
            ChatRole::User,
            "x".repeat(policy.max_context_bytes),
        );
        let usage = basis.usage(&[message]).unwrap();
        assert_eq!(usage.strategy, "checkpoint_summary");
        assert!(usage.used_bytes > usage.limit_bytes);
    }

    #[test]
    fn breakdown_counts_retained_replay_once_and_excludes_display_reasoning_and_omitted_history() {
        use crate::{
            chat::ToolCallRef,
            replay::{ProviderReplayEnvelope, ReplayCodec, ReplayDisposition},
        };
        let policy = policy();
        let old = ChatMessage::text("old", ChatRole::User, "omitted".repeat(1000));
        let user = ChatMessage::text("u", ChatRole::User, "requirement");
        let mut call = ChatMessage::assistant_tool_calls(
            "call",
            "",
            vec![ToolCallRef {
                id: "tool".into(),
                name: "read_subagent_result".into(),
                arguments_json: "{}".into(),
            }],
        );
        call.reasoning = Some("display-only".repeat(1000));
        let replay = ReplayDisposition::Present {
            envelope: ProviderReplayEnvelope::new(
                ReplayCodec::OpenAiReasoningContent,
                policy.source_context_key.clone(),
                serde_json::json!("required provider replay"),
            ),
        };
        call.replay_disposition = Some(replay.clone());
        let summary = ChatMessage::text("summary", ChatRole::ContextSummary, "compact history");
        let mut history = vec![old, user.clone(), call.clone()];
        let basis = ContextUsageBasis::observe(
            &history,
            &[summary.clone(), user.clone(), call.clone()],
            &policy,
        );
        let result = ChatMessage::tool_result("result", "tool", "original receipt");
        history.push(result.clone());
        let usage = basis.usage(&history).unwrap();
        assert_eq!(usage.breakdown.messages_bytes, model_context_cost(&user));
        assert_eq!(usage.breakdown.replay_bytes, replay.model_context_cost());
        assert_eq!(
            usage.breakdown.tools_bytes,
            model_context_cost(&call) - replay.model_context_cost() + model_context_cost(&result)
        );
        assert_eq!(
            usage.breakdown.projected_bytes,
            model_context_cost(&summary)
        );
        assert_eq!(
            usage.used_bytes,
            usage.breakdown.messages_bytes
                + usage.breakdown.tools_bytes
                + usage.breakdown.replay_bytes
                + usage.breakdown.projected_bytes
        );
        call.reasoning = None;
        assert_eq!(model_context_cost(&call), model_context_cost(&history[2]));
    }
}
