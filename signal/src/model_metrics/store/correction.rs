//! Durable correction opportunities remain entirely inside observation storage.

use super::*;
use desk_diagnose_core::model_observability::correction::{
    CorrectionFact, CorrectionFence, CorrectionResponse,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CorrectionState {
    pub(super) fence: CorrectionFence,
    pub(super) updated_at_ms: i64,
    pub(super) projected_call_id: Option<String>,
    pub(super) projected_fence: Option<CorrectionFence>,
    pub(super) response: Option<CorrectionResponse>,
}

fn eligible(tool: &ToolSnapshot) -> bool {
    tool.conclusion == InputConclusion::Rejected
        && tool.tool_key != "unknown"
        && tool
            .stages
            .get(&Stage::Dispatch)
            .is_none_or(|stage| *stage == StageOutcome::NotReached)
}

impl Store {
    pub(super) async fn resolve_correction(
        &self,
        txn: &DatabaseTransaction,
        mut event: ObservationEvent,
        original: ObservationEvent,
        fact: &CorrectionFact,
    ) -> Result<relation::Resolution, DbErr> {
        let Some(row) = compact::Entity::find_by_id(&original.object_id)
            .one(txn)
            .await?
        else {
            return Ok(relation::Resolution::Pending);
        };
        if row.kind != "tool" {
            return Ok(relation::Resolution::Conflict);
        }
        let state: CompactState = decode(&row.snapshot_json)?;
        if !state.event.is_bounded()
            || state.event.object_id != original.object_id
            || state.event.call_id != original.call_id
            || state.event.attribution != original.attribution
            || state.event.started_at_ms != original.started_at_ms
        {
            return Ok(relation::Resolution::Conflict);
        }
        let ObservationPayload::Tool(tool) = &state.event.payload else {
            return Ok(relation::Resolution::Conflict);
        };
        match fact {
            CorrectionFact::Feedback { fence } => {
                let ObservationPayload::Tool(feedback) = &event.payload else {
                    return Ok(relation::Resolution::Conflict);
                };
                if !eligible(feedback) {
                    return Ok(relation::Resolution::Ignored);
                }
                if event.object_id != original.object_id
                    || event.call_id != original.call_id
                    || event.attribution != original.attribution
                    || !fence.matches_attribution(&original.attribution)
                {
                    return Ok(relation::Resolution::Conflict);
                }
                if state
                    .correction
                    .as_ref()
                    .is_some_and(|current| current.fence != *fence)
                {
                    return Ok(relation::Resolution::Conflict);
                }
            }
            CorrectionFact::Projected { next_call_id, .. }
            | CorrectionFact::Response { next_call_id, .. } => {
                if !eligible(tool) {
                    return Ok(
                        if tool.conclusion == InputConclusion::Unknown && tool.tool_key != "unknown"
                        {
                            relation::Resolution::Pending
                        } else {
                            relation::Resolution::Ignored
                        },
                    );
                }
                let Some(correction) = &state.correction else {
                    return Ok(relation::Resolution::Pending);
                };
                if correction
                    .projected_call_id
                    .as_ref()
                    .is_some_and(|current| current != next_call_id)
                {
                    return Ok(relation::Resolution::Ignored);
                }
                if let CorrectionFact::Response {
                    fence, response, ..
                } = fact
                {
                    if correction.projected_call_id.is_none() {
                        return Ok(relation::Resolution::Pending);
                    }
                    if correction.projected_fence.as_ref() != Some(fence) {
                        return Ok(relation::Resolution::Conflict);
                    }
                    if let Some(previous) = &correction.response {
                        return Ok(if previous == response {
                            relation::Resolution::Ignored
                        } else {
                            relation::Resolution::Conflict
                        });
                    }
                    if correction.fence.same_input(fence)
                        && correction.fence.same_model(fence)
                        && let CorrectionResponse::Returned(Some(candidate)) = response
                        && candidate.tool_key == tool.tool_key
                    {
                        let Some(candidate_row) = compact::Entity::find_by_id(&candidate.object_id)
                            .one(txn)
                            .await?
                        else {
                            return Ok(relation::Resolution::Pending);
                        };
                        if candidate_row.kind != "tool" {
                            return Ok(relation::Resolution::Conflict);
                        }
                        let candidate_state: CompactState = decode(&candidate_row.snapshot_json)?;
                        if !candidate_state.event.is_bounded()
                            || candidate_state.event.call_id.as_deref() != Some(next_call_id)
                            || !fence.matches_attribution(&candidate_state.event.attribution)
                        {
                            return Ok(relation::Resolution::Conflict);
                        }
                        let ObservationPayload::Tool(next) = &candidate_state.event.payload else {
                            return Ok(relation::Resolution::Conflict);
                        };
                        if next.ordinal != 0
                            || next.tool_key != tool.tool_key
                            || next
                                .correction_of
                                .as_ref()
                                .is_some_and(|parent| parent != &original.object_id)
                        {
                            return Ok(relation::Resolution::Conflict);
                        }
                    }
                }
            }
        }
        event.object_id = state.event.object_id;
        event.call_id = state.event.call_id;
        event.started_at_ms = state.event.started_at_ms;
        event.attribution = state.event.attribution;
        // Synthetic metadata does not consume a producer's progress sequence.
        if matches!(fact, CorrectionFact::Feedback { .. }) {
            event.sequence = event.sequence.max(state.event.sequence);
        } else {
            event.sequence = state.event.sequence;
            event.payload = state.event.payload;
        }
        Ok(relation::Resolution::Resolved(event))
    }

    pub(super) async fn apply_correction(
        &self,
        txn: &DatabaseTransaction,
        observed: &ObservationEvent,
        config: &MetricsSettings,
        now: i64,
        fact: &CorrectionFact,
    ) -> Result<Option<&'static str>, DbErr> {
        let row = compact::Entity::find_by_id(&observed.object_id)
            .one(txn)
            .await?
            .ok_or_else(|| DbErr::Custom("metrics correction source unavailable".into()))?;
        let state: CompactState = decode(&row.snapshot_json)?;
        let mut original = state.event.clone();
        original.relation = None;
        original.occurred_at_ms = observed.occurred_at_ms;
        original.phase = ObservationPhase::Stage;
        if matches!(fact, CorrectionFact::Feedback { .. }) {
            original.payload = observed.payload.clone();
            original.sequence = observed.sequence;
        }
        let ObservationPayload::Tool(tool) = &mut original.payload else {
            return Ok(Some("association_conflict"));
        };
        let inherited_group = state
            .group
            .map(|group| super::correction_group::GroupMetadata {
                root_id: group.root_id,
                summary: None,
            });
        let mut correction = state.correction;
        let mut candidate_event = None;
        match fact {
            CorrectionFact::Feedback { fence } => {
                if correction.is_none() {
                    correction = Some(CorrectionState {
                        fence: fence.clone(),
                        updated_at_ms: observed.occurred_at_ms,
                        projected_call_id: None,
                        projected_fence: None,
                        response: None,
                    });
                    tool.correction_status = CorrectionStatus::AwaitingResponse;
                }
            }
            CorrectionFact::Projected {
                fence,
                next_call_id,
            } => {
                let current = correction.as_mut().ok_or_else(|| {
                    DbErr::Custom("metrics correction opportunity unavailable".into())
                })?;
                if current.projected_call_id.is_none() {
                    current.updated_at_ms = current.updated_at_ms.max(observed.occurred_at_ms);
                    current.projected_call_id = Some(next_call_id.clone());
                    current.projected_fence = Some(fence.clone());
                    tool.correction_status = if !current.fence.same_input(fence) {
                        CorrectionStatus::NotComparable
                    } else if !current.fence.same_model(fence) {
                        CorrectionStatus::Switched
                    } else {
                        CorrectionStatus::AwaitingResponse
                    };
                }
            }
            CorrectionFact::Response {
                fence, response, ..
            } => {
                let current = correction.as_mut().ok_or_else(|| {
                    DbErr::Custom("metrics correction opportunity unavailable".into())
                })?;
                current.updated_at_ms = current.updated_at_ms.max(observed.occurred_at_ms);
                current.response = Some(response.clone());
                tool.correction_status = if !current.fence.same_input(fence) {
                    CorrectionStatus::NotComparable
                } else if !current.fence.same_model(fence) {
                    CorrectionStatus::Switched
                } else {
                    match response {
                        CorrectionResponse::NoResponse => CorrectionStatus::NoResponse,
                        CorrectionResponse::NotComparable => CorrectionStatus::NotComparable,
                        CorrectionResponse::Returned(Some(candidate))
                            if candidate.tool_key == tool.tool_key =>
                        {
                            let row = compact::Entity::find_by_id(&candidate.object_id)
                                .one(txn)
                                .await?
                                .ok_or_else(|| {
                                    DbErr::Custom("metrics correction candidate unavailable".into())
                                })?;
                            let next: CompactState = decode(&row.snapshot_json)?;
                            let mut event = next.event;
                            event.relation = None;
                            event.phase = ObservationPhase::Stage;
                            let ObservationPayload::Tool(next) = &mut event.payload else {
                                return Ok(Some("association_conflict"));
                            };
                            next.correction_of = Some(original.object_id.clone());
                            tool.correction_input = match next.conclusion {
                                InputConclusion::Unknown => None,
                                conclusion => Some(conclusion),
                            };
                            candidate_event = Some(event);
                            CorrectionStatus::Linked
                        }
                        CorrectionResponse::Returned(_) => CorrectionStatus::Ambiguous,
                    }
                };
            }
        }
        if let Some(reason) = self
            .apply_state(txn, &original, config, now, correction)
            .await?
        {
            return Ok(Some(reason));
        }
        if let Some(candidate) = candidate_event
            && let Some(reason) = self
                .apply_state_with_group(
                    txn,
                    &candidate,
                    config,
                    now,
                    None,
                    inherited_group.map(Some),
                )
                .await?
        {
            return Ok(Some(reason));
        }
        self.sync_correction_conclusion(txn, &original.object_id, config, now)
            .await?;
        self.sync_correction_groups(txn, &original.object_id, config, now)
            .await?;
        Ok(None)
    }

    /// Update the original cohort when a linked candidate acquires an input
    /// verdict later. Approval or native completion never supplies that verdict.
    pub(super) async fn sync_correction_conclusion(
        &self,
        txn: &DatabaseTransaction,
        object_id: &str,
        config: &MetricsSettings,
        now: i64,
    ) -> Result<Option<&'static str>, DbErr> {
        let Some(row) = compact::Entity::find_by_id(object_id).one(txn).await? else {
            return Ok(None);
        };
        if row.kind != "tool" {
            return Ok(None);
        }
        let state: CompactState = decode(&row.snapshot_json)?;
        let ObservationPayload::Tool(candidate) = &state.event.payload else {
            return Ok(None);
        };
        if candidate.conclusion == InputConclusion::Unknown {
            return Ok(None);
        }
        let Some(parent) = &candidate.correction_of else {
            return Ok(None);
        };
        let Some(row) = compact::Entity::find_by_id(parent).one(txn).await? else {
            resources::partial(txn).await?;
            return Ok(None);
        };
        if row.kind != "tool" {
            resources::partial(txn).await?;
            return Ok(None);
        }
        let original: CompactState = match decode(&row.snapshot_json) {
            Ok(state) => state,
            Err(_) => {
                resources::partial(txn).await?;
                return Ok(None);
            }
        };
        let Some(correction) = &original.correction else {
            resources::partial(txn).await?;
            return Ok(None);
        };
        if !matches!(&correction.response,Some(CorrectionResponse::Returned(Some(next))) if next.object_id==object_id)
            || !correction
                .fence
                .matches_attribution(&state.event.attribution)
        {
            resources::partial(txn).await?;
            return Ok(None);
        }
        let mut event = original.event;
        event.relation = None;
        event.phase = ObservationPhase::Stage;
        event.occurred_at_ms = state.event.occurred_at_ms;
        let ObservationPayload::Tool(tool) = &mut event.payload else {
            resources::partial(txn).await?;
            return Ok(None);
        };
        if tool.correction_status != CorrectionStatus::Linked {
            resources::partial(txn).await?;
            return Ok(None);
        }
        tool.correction_input = Some(candidate.conclusion);
        let isolated = txn.begin().await?;
        match self.apply_state(&isolated, &event, config, now, None).await {
            Ok(None) => isolated.commit().await?,
            Ok(Some(_)) | Err(DbErr::Custom(_)) => {
                isolated.rollback().await?;
                resources::partial(txn).await?;
            }
            Err(error) => {
                isolated.rollback().await?;
                return Err(error);
            }
        }
        Ok(None)
    }
}
