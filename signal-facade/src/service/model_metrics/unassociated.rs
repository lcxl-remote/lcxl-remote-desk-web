//! Retained facts with no invented model attribution, cohort or contribution.

use super::{tag, timestamp};
use crate::model::model_metrics::{
    AssociationGapKind, AssociationGapState, MetricsQuery, UnassociatedModel, UnassociatedQuery,
    UnassociatedRecord,
};
use desk_diagnose_core::model_observability::{
    Attribution, ObservationEvent, ObservationPayload, PermissionOutcome,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnassociatedSnapshot {
    pub id: String,
    pub object_id: String,
    pub received_at_ms: i64,
    pub occurred_at_ms: i64,
    pub updated_at_ms: i64,
    pub kind: String,
    pub phase: String,
    pub missing: AssociationGapKind,
    pub state: AssociationGapState,
    pub attribution: Option<Attribution>,
    pub call_id: Option<String>,
    pub tool_observation_id: Option<String>,
    pub tool: Option<String>,
    pub ordinal: Option<u32>,
    pub permission: Option<String>,
    pub fact_outcome: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedUnassociatedQuery {
    pub from_ms: i64,
    pub to_ms: i64,
    pub cursor: Option<String>,
    pub limit: u32,
    range: super::ResolvedQuery,
}

impl ResolvedUnassociatedQuery {
    pub fn resolve(query: &UnassociatedQuery, now: i64) -> Result<Self, &'static str> {
        if query.cursor.is_some() && (query.from.is_none() || query.to.is_none()) {
            return Err("unassociated pagination requires a fixed time range");
        }
        let resolved = super::ResolvedQuery::resolve(
            &MetricsQuery {
                from: query.from.clone(),
                to: query.to.clone(),
                cursor: query.cursor.clone(),
                limit: query.limit,
                ..Default::default()
            },
            now,
        )?;
        Ok(Self {
            from_ms: resolved.requested_from_ms,
            to_ms: resolved.requested_to_ms,
            cursor: query.cursor.clone(),
            limit: resolved.limit,
            range: resolved,
        })
    }

    fn context(&self, manager: bool) -> String {
        let mode = if manager { "manager" } else { "oss" };
        format!(
            "{:x}",
            Sha256::digest(
                format!("unassociated:{mode}:{}:{}", self.from_ms, self.to_ms).as_bytes()
            )
        )
    }

    pub fn cursor(&self, manager: bool, time: i64, id: &str) -> String {
        format!("{}~{time}~{id}", self.context(manager))
    }

    pub fn boundary(&self, manager: bool) -> Result<Option<(i64, String)>, &'static str> {
        let Some(cursor) = &self.cursor else {
            return Ok(None);
        };
        let mut parts = cursor.split('~');
        let context = parts.next().ok_or("invalid unassociated cursor")?;
        let time = parts
            .next()
            .ok_or("invalid unassociated cursor")?
            .parse::<i64>()
            .map_err(|_| "invalid unassociated cursor")?;
        let id = parts.next().ok_or("invalid unassociated cursor")?;
        if context != self.context(manager)
            || parts.next().is_some()
            || time < self.from_ms
            || time >= self.to_ms
            || !id.strip_prefix("unassociated.").is_some_and(|digest| {
                digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
            })
        {
            return Err("invalid unassociated cursor");
        }
        Ok(Some((time, id.to_owned())))
    }

    pub fn coverage(
        &self,
        now: i64,
        available_from: i64,
        partial: bool,
        manager: bool,
    ) -> crate::model::model_metrics::QueryCoverage {
        let mut coverage = self.range.coverage(now, available_from, partial, manager);
        coverage.not_collected_before =
            (self.from_ms < available_from).then(|| timestamp(available_from));
        coverage.sample_status = if self.to_ms <= available_from {
            crate::model::model_metrics::SampleStatus::NotCollected
        } else if partial || self.from_ms < available_from {
            crate::model::model_metrics::SampleStatus::Partial
        } else {
            crate::model::model_metrics::SampleStatus::Complete
        };
        coverage.effective_from = timestamp(self.from_ms.max(available_from).min(self.to_ms));
        coverage.effective_to = timestamp(self.to_ms);
        coverage.cohort_basis = "fact_received".into();
        coverage.usage_source = "observations".into();
        coverage.usage_filter_dimensions.clear();
        coverage
    }
}

impl UnassociatedSnapshot {
    /// The optional source is a validated original tool or call, never the
    /// current model or the deferred fact's placeholder attribution.
    pub fn new(
        id: String,
        event: &ObservationEvent,
        received_at_ms: i64,
        now: i64,
        missing: AssociationGapKind,
        state: AssociationGapState,
        source: Option<&ObservationEvent>,
    ) -> Option<Self> {
        if !event.is_bounded()
            || event.started_at_ms < 0
            || event.occurred_at_ms < 0
            || received_at_ms < 0
            || now < 0
        {
            return None;
        }
        let source = source.filter(|source| {
            source.is_bounded()
                && matches!(
                    &source.payload,
                    ObservationPayload::Tool(_) | ObservationPayload::Call(_)
                )
        });
        let (ordinal, permission, fact_outcome) = match &event.payload {
            ObservationPayload::Tool(tool) => (
                None,
                (tool.permission != PermissionOutcome::NotReached).then(|| tag(&tool.permission)),
                None,
            ),
            ObservationPayload::Operation(operation) => {
                (Some(operation.ordinal), None, Some(tag(&operation.outcome)))
            }
            _ => (None, None, None),
        };
        let source_tool = source.and_then(|source| {
            if let ObservationPayload::Tool(tool) = &source.payload {
                Some(tool)
            } else {
                None
            }
        });
        Some(Self {
            id,
            object_id: event.object_id.clone(),
            received_at_ms,
            occurred_at_ms: event.occurred_at_ms,
            updated_at_ms: now,
            kind: tag(&event.payload.kind()),
            phase: tag(&event.phase),
            missing,
            state,
            attribution: source.map(|source| source.attribution.clone()),
            call_id: source.and_then(|source| {
                if matches!(source.payload, ObservationPayload::Call(_)) {
                    Some(source.object_id.clone())
                } else {
                    source.call_id.clone()
                }
            }),
            tool_observation_id: source
                .filter(|source| matches!(source.payload, ObservationPayload::Tool(_)))
                .map(|source| source.object_id.clone()),
            tool: source_tool.map(|tool| tool.tool_key.clone()),
            ordinal,
            permission,
            fact_outcome,
        })
    }

    pub fn record(&self, now: i64) -> UnassociatedRecord {
        let state = if self.state == AssociationGapState::Waiting
            && now >= self.received_at_ms.saturating_add(30_000)
        {
            AssociationGapState::Unavailable
        } else {
            self.state
        };
        UnassociatedRecord {
            id: self.id.clone(),
            kind: self.kind.clone(),
            phase: self.phase.clone(),
            started_at: None,
            received_at: timestamp(self.received_at_ms),
            occurred_at: timestamp(self.occurred_at_ms),
            updated_at: timestamp(self.updated_at_ms),
            missing: self.missing,
            state,
            original_model: self
                .attribution
                .as_ref()
                .filter(|a| a.model_identity_known())
                .map(|a| UnassociatedModel {
                    provider_id: a.provider_id.clone(),
                    model_id: a.model_id.clone(),
                    model_name: a.model_name.clone(),
                    purpose: tag(&a.purpose),
                    surface: tag(&a.surface),
                    origin: tag(&a.origin),
                    configuration_scope: tag(&a.configuration_scope),
                    configuration_revision: a.configuration_revision.clone(),
                    contract_revision: a.contract_revision.clone(),
                    protocol: tag(&a.protocol),
                }),
            call_id: self.call_id.clone(),
            tool_observation_id: self.tool_observation_id.clone(),
            tool: self.tool.clone(),
            ordinal: self.ordinal,
            permission: self.permission.clone(),
            fact_outcome: self.fact_outcome.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_diagnose_core::model_observability::{InputIssue, ObservationAlias, ObservationPhase};
    use std::collections::BTreeMap;

    #[test]
    fn reception_is_not_a_model_start_and_late_waiting_remains_a_gap() {
        let event = ObservationEvent::deferred_tool(
            ObservationAlias::permission_request("server-run", "server-request").unwrap(),
            ObservationPhase::Completed,
            1,
            2_000,
            BTreeMap::new(),
            PermissionOutcome::Cancelled,
            None,
            InputIssue::None,
        );
        let original = serde_json::to_string(&event).unwrap();
        let snapshot = UnassociatedSnapshot::new(
            "unassociated-server-id".into(),
            &event,
            2_100,
            2_200,
            AssociationGapKind::Attribution,
            AssociationGapState::Waiting,
            None,
        )
        .unwrap();
        let fresh = snapshot.record(2_200);
        assert!(
            fresh.started_at.is_none()
                && fresh.original_model.is_none()
                && fresh.call_id.is_none()
                && fresh.tool.is_none()
        );
        assert_eq!(fresh.state, AssociationGapState::Waiting);
        assert_eq!(fresh.permission.as_deref(), Some("cancelled"));
        let late = snapshot.record(40_000);
        assert_eq!(late.state, AssociationGapState::Unavailable);
        assert_eq!(late.received_at, fresh.received_at);
        assert!(late.started_at.is_none() && late.original_model.is_none());
        assert_eq!(serde_json::to_string(&event).unwrap(), original);
        let mut unresolved = snapshot;
        unresolved.attribution = Some(Attribution::unresolved(
            desk_diagnose_core::model_observability::Purpose::Agent,
            desk_diagnose_core::model_observability::Surface::Assistant,
            desk_diagnose_core::model_observability::Origin::Unknown,
        ));
        assert!(unresolved.record(40_000).original_model.is_none());
        assert!(
            UnassociatedSnapshot::new(
                "id".into(),
                &event,
                -1,
                2_200,
                AssociationGapKind::Attribution,
                AssociationGapState::Waiting,
                None
            )
            .is_none()
        );
    }

    #[test]
    fn pagination_cannot_reuse_normal_cursors_or_change_range_or_role() {
        let filters = UnassociatedQuery {
            from: Some(timestamp(1_000)),
            to: Some(timestamp(50_000)),
            limit: Some(1),
            cursor: None,
        };
        let resolved = ResolvedUnassociatedQuery::resolve(&filters, 60_000).unwrap();
        let id = format!("unassociated.{}", "a".repeat(64));
        let cursor = resolved.cursor(false, 2_000, &id);
        let paged = ResolvedUnassociatedQuery::resolve(
            &UnassociatedQuery {
                cursor: Some(cursor.clone()),
                ..filters.clone()
            },
            60_000,
        )
        .unwrap();
        assert_eq!(paged.boundary(false).unwrap(), Some((2_000, id)));
        assert!(paged.boundary(true).is_err());
        let changed = ResolvedUnassociatedQuery::resolve(
            &UnassociatedQuery {
                from: Some(timestamp(1_001)),
                cursor: Some(cursor),
                ..filters.clone()
            },
            60_000,
        )
        .unwrap();
        assert!(changed.boundary(false).is_err());
        let normal = ResolvedUnassociatedQuery::resolve(
            &UnassociatedQuery {
                cursor: Some("2000~normal-call".into()),
                ..filters
            },
            60_000,
        )
        .unwrap();
        assert!(normal.boundary(false).is_err());
        assert!(
            ResolvedUnassociatedQuery::resolve(
                &UnassociatedQuery {
                    cursor: Some("cursor".into()),
                    ..Default::default()
                },
                60_000
            )
            .is_err()
        );
        assert!(serde_json::from_str::<UnassociatedQuery>(r#"{"model_id":"model-a"}"#).is_err());
        assert!(serde_json::from_str::<UnassociatedQuery>(r#"{"include_probe":false}"#).is_err());
        let coverage = resolved.coverage(60_000, 1_000, false, true);
        assert_eq!(coverage.cohort_basis, "fact_received");
        assert_eq!(coverage.usage_source, "observations");
        assert!(coverage.usage_filter_dimensions.is_empty());
        assert_eq!(
            (coverage.effective_from, coverage.effective_to),
            (timestamp(1_000), timestamp(50_000))
        );
    }
}
