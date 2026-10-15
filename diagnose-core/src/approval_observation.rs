//! Content-free review facts and safe, readable provider projections.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Main,
    Subagent,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOutcome {
    Pending,
    Approved,
    Denied,
    Fault,
    PolicyBlocked,
    Cancelled,
    Superseded,
    Expired,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewReason {
    Configuration,
    Provider,
    Transport,
    Timeout,
    Stream,
    OutputTruncated,
    InvalidDecision,
    Policy,
    Budget,
    Revoked,
    SourceChanged,
    CandidateExpired,
    LeaseExpired,
    Processing,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSnapshot {
    pub source_kind: String,
    pub session_kind: SessionKind,
    pub outcome: ReviewOutcome,
    pub reason: Option<ReviewReason>,
    pub eligible_at_ms: i64,
    pub provider_started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
}

impl ReviewSnapshot {
    pub fn waiting_ms(&self) -> Option<u64> {
        duration(self.eligible_at_ms, self.finished_at_ms?)
    }
    pub fn before_model_ms(&self) -> Option<u64> {
        duration(self.eligible_at_ms, self.provider_started_at_ms?)
    }
    pub fn terminal(&self) -> bool {
        self.outcome != ReviewOutcome::Pending
    }
}

pub fn duration(start: i64, end: i64) -> Option<u64> {
    if start <= 0 {
        return None;
    }
    u64::try_from(end.checked_sub(start)?).ok()
}

/// Stable across scans, retries and model configuration changes; never exposed.
pub fn review_alias(parts: &[&str]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((part.len() as u64).to_le_bytes());
        digest.update(part.as_bytes());
    }
    format!("approval.{:x}", digest.finalize())
}

/// Only protocol fields with known non-credential semantics are copied.
pub fn readable_request(body: &Value) -> Value {
    let mut result = Map::new();
    for key in [
        "model",
        "system",
        "messages",
        "max_tokens",
        "max_completion_tokens",
        "temperature",
        "top_p",
        "stream",
        "thinking",
        "reasoning",
        "reasoning_effort",
        "enable_thinking",
        "stream_options",
        "response_format",
        "tool_choice",
    ] {
        if let Some(value) = body.get(key) {
            result.insert(key.into(), redact(value));
        }
    }
    let omitted = body
        .as_object()
        .map_or(0, |map| map.len().saturating_sub(result.len()));
    result.insert("omitted_fields".into(), Value::from(omitted));
    Value::Object(result)
}

fn redact(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            if matches!(
                object.get("type").and_then(Value::as_str),
                Some("image" | "image_url" | "redacted_thinking")
            ) {
                return serde_json::json!({"type": object.get("type"), "omitted": true});
            }
            Value::Object(
                object
                    .iter()
                    .filter_map(|(key, value)| {
                        let lower = key.to_ascii_lowercase();
                        if [
                            "api_key",
                            "authorization",
                            "cookie",
                            "signature",
                            "headers",
                            "credentials",
                            "password",
                            "access_token",
                            "encrypted_content",
                            "replay_signature",
                        ]
                        .contains(&lower.as_str())
                        {
                            return None;
                        }
                        Some((key.clone(), redact(value)))
                    })
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.iter().map(redact).collect()),
        _ => value.clone(),
    }
}

/// Readable deltas only; opaque signatures and replay blocks are excluded.
pub fn readable_reasoning_delta(data: &str) -> Option<String> {
    let value: Value = serde_json::from_str(data).ok()?;
    if let Some(text) = value["choices"][0]["delta"]["reasoning_content"].as_str() {
        return Some(text.to_owned());
    }
    if value["delta"]["type"] == "thinking_delta" {
        return value["delta"]["thinking"].as_str().map(str::to_owned);
    }
    if value["content_block"]["type"] == "thinking" {
        return value["content_block"]["thinking"]
            .as_str()
            .map(str::to_owned);
    }
    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewTracking {
    pub object_id: String,
    pub snapshot: ReviewSnapshot,
    pub attribution: crate::model_observability::Attribution,
}
impl ReviewTracking {
    pub fn new(authority: &crate::subagent::reservation::ReviewCallAuthority, now: i64) -> Self {
        let source_kind = match authority.source {
            crate::approval_review::ApprovalSource::GoalProposal { .. } => "goal_proposal",
            crate::approval_review::ApprovalSource::PermissionItem { .. } => "permission_item",
            crate::approval_review::ApprovalSource::DirectoryProposal { .. } => {
                "directory_proposal"
            }
            crate::approval_review::ApprovalSource::ConcreteCall { .. } => "concrete_call",
            crate::approval_review::ApprovalSource::OssCommand { .. } => "oss_command",
            crate::approval_review::ApprovalSource::ManagerCommand { .. } => "manager_command",
            crate::approval_review::ApprovalSource::ManagerAction { .. } => "manager_action",
        };
        Self::queued(
            &authority.actor_id,
            &authority.device_id,
            &authority.conversation_id,
            &authority.root_conversation_id,
            &authority.delegation_id,
            authority.delegation_revision,
            source_kind,
            &authority.source_record_id,
            now,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn queued(
        owner: &str,
        device: &str,
        conversation: &str,
        root: &str,
        delegation: &str,
        revision: u64,
        source_kind: &str,
        source_id: &str,
        now: i64,
    ) -> Self {
        Self {
            object_id: review_alias(&[
                owner,
                device,
                conversation,
                delegation,
                &revision.to_string(),
                source_kind,
                source_id,
            ]),
            snapshot: ReviewSnapshot {
                source_kind: source_kind.into(),
                session_kind: if root == conversation {
                    SessionKind::Main
                } else {
                    SessionKind::Subagent
                },
                outcome: ReviewOutcome::Pending,
                reason: None,
                eligible_at_ms: now,
                provider_started_at_ms: None,
                finished_at_ms: None,
            },
            attribution: crate::model_observability::Attribution::unresolved(
                crate::model_observability::Purpose::Approval,
                crate::model_observability::Surface::Assistant,
                crate::model_observability::Origin::System,
            ),
        }
    }
    pub fn settled(mut self, status: &str, reason: Option<ReviewReason>, now: i64) -> Self {
        self.snapshot.reason = reason.or(self.snapshot.reason);
        self.snapshot.outcome = match status {
            "approved" => ReviewOutcome::Approved,
            "denied" => ReviewOutcome::Denied,
            "reviewing" => ReviewOutcome::Pending,
            "expired" => {
                if self.snapshot.reason == Some(ReviewReason::Revoked) {
                    ReviewOutcome::Cancelled
                } else if self.snapshot.reason == Some(ReviewReason::LeaseExpired) {
                    ReviewOutcome::Fault
                } else {
                    ReviewOutcome::Expired
                }
            }
            _ => match self.snapshot.reason {
                Some(ReviewReason::Revoked) => ReviewOutcome::Cancelled,
                Some(ReviewReason::SourceChanged) => ReviewOutcome::Superseded,
                Some(ReviewReason::Budget | ReviewReason::Policy) => ReviewOutcome::PolicyBlocked,
                Some(ReviewReason::CandidateExpired) => ReviewOutcome::Expired,
                Some(ReviewReason::Unknown) | None => ReviewOutcome::Unknown,
                _ => ReviewOutcome::Fault,
            },
        };
        if self.snapshot.terminal() {
            self.snapshot.finished_at_ms = Some(now);
        }
        self
    }
    pub fn event(
        &self,
        phase: crate::model_observability::ObservationPhase,
        sequence: u32,
        _now: i64,
    ) -> crate::model_observability::ObservationEvent {
        crate::model_observability::ObservationEvent {
            schema_version: crate::model_observability::EVENT_SCHEMA_VERSION,
            event_id: format!("{}.{}", self.object_id, sequence),
            object_id: self.object_id.clone(),
            call_id: None,
            phase,
            sequence,
            started_at_ms: self.snapshot.eligible_at_ms,
            occurred_at_ms: match phase {
                crate::model_observability::ObservationPhase::Started => {
                    self.snapshot.eligible_at_ms
                }
                crate::model_observability::ObservationPhase::Output => self
                    .snapshot
                    .provider_started_at_ms
                    .unwrap_or(self.snapshot.eligible_at_ms),
                _ => self
                    .snapshot
                    .finished_at_ms
                    .unwrap_or(self.snapshot.eligible_at_ms),
            },
            attribution: self.attribution.clone(),
            payload: crate::model_observability::ObservationPayload::Approval(
                self.snapshot.clone(),
            ),
            relation: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn committed_cancellation_and_late_start_are_not_model_faults() {
        use crate::model_observability::{
            ObservationPhase,
            aggregate::{MergeResult, merge},
        };
        let queued = ReviewTracking::queued(
            "owner",
            "device",
            "child",
            "root",
            "delegation",
            1,
            "permission_item",
            "item",
            1000,
        );
        let cancelled = queued
            .clone()
            .settled("expired", Some(ReviewReason::Revoked), 9000);
        assert_eq!(cancelled.snapshot.outcome, ReviewOutcome::Cancelled);
        let mut final_event = cancelled.event(ObservationPhase::Completed, 2, 9000);
        let original = final_event.clone();
        assert_eq!(
            merge(
                &mut final_event,
                &queued.event(ObservationPhase::Started, 0, 1000)
            ),
            MergeResult::Duplicate
        );
        assert_eq!(final_event, original);
        assert_eq!(
            queued
                .settled("unavailable", Some(ReviewReason::Budget), 9000)
                .snapshot
                .outcome,
            ReviewOutcome::PolicyBlocked
        );
    }
    #[test]
    fn request_projection_excludes_credentials_and_opaque_content() {
        let body = serde_json::json!({"model":"m", "api_key":"secret", "custom":"secret", "messages":[{"role":"user", "content":[{"type":"image_url", "image_url":{"url":"data:secret"}}, {"type":"text", "text":"visible"}]}], "thinking":{"type":"enabled", "signature":"secret"}});
        let projected = readable_request(&body).to_string();
        assert!(!projected.contains("secret"));
        assert!(projected.contains("visible"));
    }
    #[test]
    fn timing_never_fills_missing_or_negative_values_with_zero() {
        assert_eq!(duration(1000, 9000), Some(8000));
        assert_eq!(duration(4000, 1000), None);
        assert_eq!(duration(0, 1000), None);
    }
    #[test]
    fn reasoning_is_not_bounded_by_the_general_display_limit() {
        let thought = "中".repeat(20_000);
        let value =
            serde_json::json!({"choices":[{"delta":{"reasoning_content":thought}}]}).to_string();
        assert_eq!(readable_reasoning_delta(&value).unwrap().len(), 60_000);
        assert!(
            readable_reasoning_delta(
                r#"{"delta":{"type":"signature_delta","signature":"private"}}"#
            )
            .is_none()
        );
    }
    #[test]
    fn length_prefix_prevents_ambiguous_aliases() {
        assert_ne!(review_alias(&["a", "bc"]), review_alias(&["ab", "c"]));
    }
}
