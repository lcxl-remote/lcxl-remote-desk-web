//! Recovery facts update the original call without creating synthetic calls.

use super::correction_group::GroupMetadata;
use super::*;
use desk_diagnose_core::model_observability::correction::CorrectionFence;
use desk_diagnose_core::model_observability::correction_group::{
    CorrectionGroup, CorrectionGroupOutcome,
};
use desk_diagnose_core::model_observability::protocol_correction::{
    ProtocolCorrectionCheck, ProtocolCorrectionFact, ProtocolCorrectionReason,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProtocolState {
    pub reason: ProtocolCorrectionReason,
    pub fence: Option<CorrectionFence>,
    pub next_call_id: Option<String>,
    pub next_fence: Option<CorrectionFence>,
    pub check: Option<ProtocolCorrectionCheck>,
    pub returned: bool,
    pub updated_at_ms: i64,
}

impl ProtocolState {
    fn is_bounded(&self) -> bool {
        self.updated_at_ms >= 0
            && self.fence.as_ref().is_none_or(CorrectionFence::is_bounded)
            && self
                .next_fence
                .as_ref()
                .is_none_or(CorrectionFence::is_bounded)
            && self.next_call_id.as_deref().is_none_or(
                desk_diagnose_core::model_observability::correction_group::valid_identity,
            )
            && (self.next_call_id.is_some()
                || self.next_fence.is_none() && self.check.is_none() && !self.returned)
    }
}

fn eligible(call: &CallSnapshot, reason: ProtocolCorrectionReason) -> Option<bool> {
    if matches!(
        call.outcome,
        RequestOutcome::Pending | RequestOutcome::ObservationIncomplete
    ) {
        return None;
    }
    if reason == ProtocolCorrectionReason::ToolChoiceFallback {
        return Some(call.outcome != RequestOutcome::Returned);
    }
    if call.outcome != RequestOutcome::Returned {
        return Some(false);
    }
    if call.output == OutputOutcome::NotEvaluated {
        return None;
    }
    Some(match reason {
        ProtocolCorrectionReason::EmptyResponse => call.output == OutputOutcome::EmptyResponse,
        ProtocolCorrectionReason::TruncatedOutput => call.output == OutputOutcome::OutputTruncated,
        ProtocolCorrectionReason::CompletionInterpretation
        | ProtocolCorrectionReason::PermissionPlanProtocol
        | ProtocolCorrectionReason::PermissionActionMissing => {
            call.output == OutputOutcome::InvalidStructuredOutput
        }
        ProtocolCorrectionReason::PermissionProtocol => matches!(
            call.output,
            OutputOutcome::InvalidProtocol | OutputOutcome::InvalidStructuredOutput
        ),
        ProtocolCorrectionReason::GoalControlMissing => call.output == OutputOutcome::Accepted,
        ProtocolCorrectionReason::ToolChoiceFallback => unreachable!(),
    })
}

impl Store {
    pub(super) async fn resolve_protocol_correction(
        &self,
        txn: &DatabaseTransaction,
        mut event: ObservationEvent,
        fact: &ProtocolCorrectionFact,
    ) -> Result<relation::Resolution, DbErr> {
        let Some(row) = compact::Entity::find_by_id(fact.source_call_id())
            .one(txn)
            .await?
        else {
            return Ok(relation::Resolution::Pending);
        };
        let state: CompactState = decode(&row.snapshot_json)?;
        if row.kind != "call"
            || !state.event.is_bounded()
            || state.event.object_id != fact.source_call_id()
            || state.event.started_at_ms != row.started_at_ms
            || state.protocol_correction.as_ref().is_some_and(|state| {
                !state.is_bounded()
                    || state
                        .fence
                        .as_ref()
                        .is_some_and(|fence| !fence.matches_attribution(&event.attribution))
            })
        {
            return Ok(relation::Resolution::Conflict);
        }
        let source = state.event.clone();
        let ObservationPayload::Call(call) = &source.payload else {
            return Ok(relation::Resolution::Conflict);
        };
        let pending = || {
            relation::Resolution::PendingSource(
                source.clone(),
                AssociationGapKind::CorrectionPrerequisite,
            )
        };
        if event.attribution != source.attribution || event.started_at_ms != source.started_at_ms {
            return Ok(relation::Resolution::Conflict);
        }
        match fact {
            ProtocolCorrectionFact::Feedback { fence, reason, .. } => {
                if fence
                    .as_ref()
                    .is_some_and(|fence| !fence.matches_attribution(&source.attribution))
                {
                    return Ok(relation::Resolution::Conflict);
                }
                match eligible(call, *reason) {
                    None => return Ok(pending()),
                    Some(false) => return Ok(relation::Resolution::Ignored),
                    Some(true) => {}
                }
                if state
                    .protocol_correction
                    .as_ref()
                    .is_some_and(|previous| previous.fence != *fence || previous.reason != *reason)
                {
                    return Ok(relation::Resolution::Conflict);
                }
            }
            ProtocolCorrectionFact::Projected { next_call_id, .. }
            | ProtocolCorrectionFact::Returned { next_call_id, .. }
            | ProtocolCorrectionFact::Response { next_call_id, .. } => {
                let Some(previous) = &state.protocol_correction else {
                    return Ok(pending());
                };
                if previous
                    .next_call_id
                    .as_ref()
                    .is_some_and(|first| first != next_call_id)
                {
                    return Ok(relation::Resolution::Ignored);
                }
                if let ProtocolCorrectionFact::Returned { fence, .. }
                | ProtocolCorrectionFact::Response { fence, .. } = fact
                {
                    if previous.next_call_id.is_none() {
                        return Ok(pending());
                    }
                    if previous.next_fence != *fence {
                        return Ok(relation::Resolution::Conflict);
                    }
                    if let ProtocolCorrectionFact::Response { check, .. } = fact
                        && let Some(old) = previous.check
                    {
                        return Ok(if old == *check {
                            relation::Resolution::Ignored
                        } else {
                            relation::Resolution::Conflict
                        });
                    }
                    let Some(next) = compact::Entity::find_by_id(next_call_id).one(txn).await?
                    else {
                        return Ok(pending());
                    };
                    let next_state: CompactState = decode(&next.snapshot_json)?;
                    if next.kind != "call"
                        || !next_state.event.is_bounded()
                        || next_state.event.object_id != *next_call_id
                        || !matches!(next_state.event.payload, ObservationPayload::Call(_))
                        || fence.as_ref().is_some_and(|fence| {
                            !fence.matches_attribution(&next_state.event.attribution)
                        })
                    {
                        return Ok(relation::Resolution::Conflict);
                    }
                }
            }
        }
        event.object_id = source.object_id;
        event.call_id = source.call_id;
        event.started_at_ms = source.started_at_ms;
        event.sequence = source.sequence;
        event.payload = source.payload;
        Ok(relation::Resolution::Resolved(event))
    }

    pub(super) async fn apply_protocol_correction(
        &self,
        txn: &DatabaseTransaction,
        observed: &ObservationEvent,
        config: &MetricsSettings,
        now: i64,
        fact: &ProtocolCorrectionFact,
    ) -> Result<Option<&'static str>, DbErr> {
        let row = compact::Entity::find_by_id(&observed.object_id)
            .one(txn)
            .await?
            .ok_or_else(|| {
                DbErr::Custom("metrics protocol correction source unavailable".into())
            })?;
        let current: CompactState = decode(&row.snapshot_json)?;
        let mut state = current.protocol_correction;
        match fact {
            ProtocolCorrectionFact::Feedback { fence, reason, .. } => {
                if state.is_none() {
                    state = Some(ProtocolState {
                        reason: *reason,
                        fence: fence.clone(),
                        next_call_id: None,
                        next_fence: None,
                        check: None,
                        returned: false,
                        updated_at_ms: observed.occurred_at_ms,
                    });
                }
            }
            ProtocolCorrectionFact::Projected {
                fence,
                next_call_id,
                ..
            } => {
                let state = state.as_mut().ok_or_else(|| {
                    DbErr::Custom("metrics protocol correction feedback unavailable".into())
                })?;
                if state.next_call_id.is_none() {
                    state.next_call_id = Some(next_call_id.clone());
                    state.next_fence = fence.clone();
                    state.updated_at_ms = state.updated_at_ms.max(observed.occurred_at_ms);
                }
            }
            ProtocolCorrectionFact::Returned { .. } => {
                let state = state.as_mut().ok_or_else(|| {
                    DbErr::Custom("metrics protocol correction projection unavailable".into())
                })?;
                state.returned = true;
                state.updated_at_ms = state.updated_at_ms.max(observed.occurred_at_ms);
            }
            ProtocolCorrectionFact::Response { check, .. } => {
                let state = state.as_mut().ok_or_else(|| {
                    DbErr::Custom("metrics protocol correction projection unavailable".into())
                })?;
                state.check = Some(*check);
                state.updated_at_ms = state.updated_at_ms.max(observed.occurred_at_ms);
            }
        }
        let state = state
            .ok_or_else(|| DbErr::Custom("metrics protocol correction state unavailable".into()))?;
        let outcome = match (
            &state.fence,
            &state.next_fence,
            state.next_call_id.is_some(),
            state.check,
        ) {
            (None, _, _, _) => CorrectionGroupOutcome::NotComparable,
            (_, _, false, _) => CorrectionGroupOutcome::AwaitingResponse,
            (Some(original), Some(next), true, _) if !original.same_input(next) => {
                CorrectionGroupOutcome::NotComparable
            }
            (Some(original), Some(next), true, _) if !original.same_model(next) => {
                CorrectionGroupOutcome::Switched
            }
            (_, None, true, _) => CorrectionGroupOutcome::NotComparable,
            (_, _, true, None) => {
                if state.returned {
                    CorrectionGroupOutcome::AwaitingOutputCheck
                } else {
                    CorrectionGroupOutcome::AwaitingResponse
                }
            }
            (_, _, true, Some(ProtocolCorrectionCheck::Passed)) => {
                CorrectionGroupOutcome::OutputAccepted
            }
            (_, _, true, Some(ProtocolCorrectionCheck::Rejected)) => {
                CorrectionGroupOutcome::OutputRejected
            }
            (_, _, true, Some(ProtocolCorrectionCheck::Unavailable)) => {
                CorrectionGroupOutcome::Unavailable
            }
            (_, _, true, Some(ProtocolCorrectionCheck::NoResponse)) => {
                CorrectionGroupOutcome::NoResponse
            }
        };
        let summary = CorrectionGroup {
            root_id: current.event.object_id.clone(),
            last_input_id: state
                .next_call_id
                .clone()
                .unwrap_or_else(|| current.event.object_id.clone()),
            category: state.reason.category(),
            reason: Some(state.reason),
            outcome,
            linked_attempts: u32::from(state.next_call_id.is_some()),
            updated_at_ms: state.updated_at_ms,
        };
        let metadata = GroupMetadata {
            root_id: summary.root_id.clone(),
            summary: Some(summary),
        };
        let mut event = current.event;
        event.relation = None;
        event.phase = ObservationPhase::Stage;
        let next = state.next_call_id.clone();
        if let Some(reason) = self
            .apply_state_metadata(
                txn,
                &event,
                config,
                now,
                None,
                Some(Some(metadata)),
                Some(state),
            )
            .await?
        {
            return Ok(Some(reason));
        }
        // A subsequent call may independently become a recovery source for a
        // different branch. Its own root must not be demoted to a member.
        if matches!(
            fact,
            ProtocolCorrectionFact::Returned { .. } | ProtocolCorrectionFact::Response { .. }
        ) && let Some(next) = next
        {
            let isolated = txn.begin().await?;
            match self
                .link_protocol_member(&isolated, &next, &event.object_id, config, now)
                .await
            {
                Ok(()) => isolated.commit().await?,
                Err(DbErr::Custom(_)) => {
                    isolated.rollback().await?;
                    resources::partial(txn).await?;
                }
                Err(error) => {
                    isolated.rollback().await?;
                    return Err(error);
                }
            }
        }
        Ok(None)
    }

    async fn link_protocol_member(
        &self,
        txn: &DatabaseTransaction,
        id: &str,
        root: &str,
        config: &MetricsSettings,
        now: i64,
    ) -> Result<(), DbErr> {
        let Some(row) = compact::Entity::find_by_id(id).one(txn).await? else {
            return Ok(());
        };
        let state: CompactState = decode(&row.snapshot_json)?;
        if row.kind != "call" || !state.event.is_bounded() || state.event.object_id != id {
            return Err(DbErr::Custom(
                "metrics protocol correction member unavailable".into(),
            ));
        }
        if state.protocol_correction.is_some() {
            return Ok(());
        }
        if state
            .group
            .as_ref()
            .is_some_and(|group| group.root_id != root)
        {
            return Err(DbErr::Custom(
                "metrics protocol correction member conflict".into(),
            ));
        }
        let metadata = GroupMetadata {
            root_id: root.into(),
            summary: None,
        };
        let mut event = state.event;
        event.relation = None;
        event.phase = ObservationPhase::Stage;
        if let Some(reason) = self
            .apply_state_with_group(txn, &event, config, now, None, Some(Some(metadata)))
            .await?
        {
            return Err(DbErr::Custom(reason.into()));
        }
        Ok(())
    }
}
