//! Correction chains are evaluated from durable, validated observation edges.

use super::*;
use desk_diagnose_core::model_observability::correction::CorrectionResponse;
use desk_diagnose_core::model_observability::correction_group::{
    CorrectionGroup, CorrectionGroupOutcome, MAX_GROUP_LINKS, tool_category, valid_identity,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GroupMetadata {
    pub root_id: String,
    pub summary: Option<CorrectionGroup>,
}

impl GroupMetadata {
    pub(super) fn is_bounded(&self, object_id: &str) -> bool {
        valid_identity(&self.root_id)
            && self.summary.as_ref().is_none_or(|summary| {
                summary.is_bounded()
                    && summary.root_id == self.root_id
                    && summary.root_id == object_id
            })
    }
}

fn tool(state: &CompactState) -> Result<&ToolSnapshot, DbErr> {
    match &state.event.payload {
        ObservationPayload::Tool(tool) => Ok(tool),
        _ => Err(DbErr::Custom(
            "metrics correction group kind unavailable".into(),
        )),
    }
}

fn edge(parent: &CompactState, child: &CompactState) -> bool {
    let (Ok(parent_tool), Ok(child_tool), Some(fact)) =
        (tool(parent), tool(child), &parent.correction)
    else {
        return false;
    };
    let Some(fence) = &fact.projected_fence else {
        return false;
    };
    parent_tool.correction_status == CorrectionStatus::Linked
        && matches!(&fact.response,Some(CorrectionResponse::Returned(Some(candidate)))
            if candidate.object_id==child.event.object_id && candidate.tool_key==child_tool.tool_key)
        && child_tool.correction_of.as_deref() == Some(parent.event.object_id.as_str())
        && child_tool.tool_key == parent_tool.tool_key
        && child_tool.ordinal == 0
        && fact.projected_call_id.as_deref() == child.event.call_id.as_deref()
        && fact.fence.same_input(fence)
        && fact.fence.same_model(fence)
        && fence.matches_attribution(&child.event.attribution)
}

impl Store {
    async fn group_tool(&self, txn: &DatabaseTransaction, id: &str) -> Result<CompactState, DbErr> {
        if !valid_identity(id) {
            return Err(DbErr::Custom(
                "metrics correction group identity unavailable".into(),
            ));
        }
        let row = compact::Entity::find_by_id(id)
            .one(txn)
            .await?
            .ok_or_else(|| DbErr::Custom("metrics correction group source unavailable".into()))?;
        let state: CompactState = decode(&row.snapshot_json)?;
        if row.kind != "tool"
            || !state.event.is_bounded()
            || state.event.object_id != id
            || state
                .group
                .as_ref()
                .is_some_and(|group| !group.is_bounded(id))
            || state
                .correction
                .as_ref()
                .is_some_and(|fact| !fact.fence.is_bounded() || fact.updated_at_ms < 0)
        {
            return Err(DbErr::Custom(
                "metrics correction group source invalid".into(),
            ));
        }
        tool(&state)?;
        Ok(state)
    }

    /// A failed group update cannot discard the current input's ordinary facts.
    /// The savepoint includes promotion, demotion, contribution and byte deltas.
    pub(super) async fn sync_correction_groups(
        &self,
        txn: &DatabaseTransaction,
        id: &str,
        config: &MetricsSettings,
        now: i64,
    ) -> Result<(), DbErr> {
        let Some(row) = compact::Entity::find_by_id(id).one(txn).await? else {
            return Ok(());
        };
        if row.kind != "tool" {
            return Ok(());
        }
        let isolated = txn.begin().await?;
        match self
            .update_correction_groups(&isolated, id, config, now)
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
        Ok(())
    }

    async fn update_correction_groups(
        &self,
        txn: &DatabaseTransaction,
        id: &str,
        config: &MetricsSettings,
        now: i64,
    ) -> Result<(), DbErr> {
        let mut root = self.group_tool(txn, id).await?;
        if root.correction.is_none() && tool(&root)?.correction_of.is_none() {
            return Ok(());
        }
        let mut visited = BTreeSet::new();
        for depth in 0..=MAX_GROUP_LINKS {
            if !visited.insert(root.event.object_id.clone()) {
                return Err(DbErr::Custom("metrics correction group cycle".into()));
            }
            let Some(parent_id) = tool(&root)?.correction_of.as_deref() else {
                break;
            };
            if depth == MAX_GROUP_LINKS {
                return Err(DbErr::Custom(
                    "metrics correction group traversal unavailable".into(),
                ));
            }
            let parent = self.group_tool(txn, parent_id).await?;
            if !edge(&parent, &root) {
                return Err(DbErr::Custom(
                    "metrics correction group edge unavailable".into(),
                ));
            }
            root = parent;
        }
        let Some(root_fact) = &root.correction else {
            return Err(DbErr::Custom(
                "metrics correction group feedback unavailable".into(),
            ));
        };
        if !root_fact.fence.matches_attribution(&root.event.attribution) {
            return Err(DbErr::Custom(
                "metrics correction group attribution unavailable".into(),
            ));
        }
        let fence = root_fact.fence.clone();
        let root_id = root.event.object_id.clone();
        let category = tool_category(&tool(&root)?.tool_key);
        let mut nodes = vec![root];
        let mut linked = 0;
        let mut updated = nodes[0].event.occurred_at_ms.max(
            nodes[0]
                .correction
                .as_ref()
                .map_or(0, |fact| fact.updated_at_ms),
        );
        let mut overflow = None;
        let outcome = loop {
            let current = nodes
                .last()
                .ok_or_else(|| DbErr::Custom("metrics correction group empty".into()))?;
            let current_tool = tool(current)?;
            if let Some(fact) = &current.correction {
                if !fence.same_input(&fact.fence) {
                    break CorrectionGroupOutcome::NotComparable;
                }
                if !fence.same_model(&fact.fence) {
                    break CorrectionGroupOutcome::Switched;
                }
            }
            if linked > 0 {
                match current_tool.conclusion {
                    InputConclusion::Unknown => break CorrectionGroupOutcome::AwaitingValidation,
                    InputConclusion::Accepted => break CorrectionGroupOutcome::InputAccepted,
                    InputConclusion::Rejected if current.correction.is_none() => {
                        break CorrectionGroupOutcome::InputRejected;
                    }
                    InputConclusion::Rejected => {}
                }
            }
            match current_tool.correction_status {
                CorrectionStatus::AwaitingResponse => {
                    break CorrectionGroupOutcome::AwaitingResponse;
                }
                CorrectionStatus::Ambiguous => break CorrectionGroupOutcome::Ambiguous,
                CorrectionStatus::NotComparable => break CorrectionGroupOutcome::NotComparable,
                CorrectionStatus::Switched => break CorrectionGroupOutcome::Switched,
                CorrectionStatus::NoResponse => break CorrectionGroupOutcome::NoResponse,
                CorrectionStatus::Uncorrelated => break CorrectionGroupOutcome::Unavailable,
                CorrectionStatus::Linked => {}
            }
            let Some(CorrectionResponse::Returned(Some(candidate))) = current
                .correction
                .as_ref()
                .and_then(|fact| fact.response.as_ref())
            else {
                return Err(DbErr::Custom(
                    "metrics correction group response unavailable".into(),
                ));
            };
            if nodes
                .iter()
                .any(|node| node.event.object_id == candidate.object_id)
            {
                return Err(DbErr::Custom("metrics correction group cycle".into()));
            }
            let next = self.group_tool(txn, &candidate.object_id).await?;
            if !edge(current, &next) {
                return Err(DbErr::Custom(
                    "metrics correction group edge unavailable".into(),
                ));
            }
            updated = updated.max(next.event.occurred_at_ms).max(
                next.correction
                    .as_ref()
                    .map_or(0, |fact| fact.updated_at_ms),
            );
            if linked == MAX_GROUP_LINKS {
                overflow = Some(next);
                resources::partial(txn).await?;
                break CorrectionGroupOutcome::Unavailable;
            }
            linked += 1;
            nodes.push(next);
        };
        let last = nodes
            .last()
            .ok_or_else(|| DbErr::Custom("metrics correction group empty".into()))?
            .event
            .object_id
            .clone();
        let summary = CorrectionGroup {
            root_id: root_id.clone(),
            last_input_id: last,
            category,
            reason: None,
            outcome,
            linked_attempts: linked,
            updated_at_ms: updated,
        };
        if let Some(overflow) = overflow {
            nodes.push(overflow);
        }
        // A candidate's feedback can precede its parent's response. Demoting its
        // provisional root in this transaction prevents persistent double counts.
        for (index, node) in nodes.into_iter().enumerate() {
            let metadata = GroupMetadata {
                root_id: root_id.clone(),
                summary: (index == 0).then(|| summary.clone()),
            };
            let mut event = node.event;
            event.relation = None;
            event.phase = ObservationPhase::Stage;
            if let Some(reason) = self
                .apply_state_with_group(txn, &event, config, now, None, Some(Some(metadata)))
                .await?
            {
                return Err(DbErr::Custom(reason.into()));
            }
        }
        Ok(())
    }

    /// Queries join bounded current compact metadata; business DTOs and receipt
    /// payloads never carry correction-group state.
    pub(crate) async fn enrich_correction_groups(
        &self,
        txn: &DatabaseTransaction,
        records: &mut [ObservationRecord],
    ) -> Result<(), DbErr> {
        let ids: BTreeSet<_> = records
            .iter()
            .filter(|record| matches!(record.kind.as_str(), "tool" | "call"))
            .map(|record| record.id.clone())
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        if ids.len() > 201 {
            return Err(DbErr::Custom(
                "metrics correction query budget unavailable".into(),
            ));
        }
        let rows = compact::Entity::find()
            .filter(compact::Column::ObjectId.is_in(ids))
            .limit(201)
            .all(txn)
            .await?;
        let rows: BTreeMap<_, _> = rows
            .into_iter()
            .map(|row| (row.object_id, row.snapshot_json))
            .collect();
        for record in records
            .iter_mut()
            .filter(|record| matches!(record.kind.as_str(), "tool" | "call"))
        {
            let known = record.correction_of.is_some()
                || record
                    .correction_status
                    .as_deref()
                    .is_some_and(|status| status != "uncorrelated");
            let Some(json) = rows.get(&record.id) else {
                record.correction_group_unavailable |= known;
                continue;
            };
            let state: CompactState = match decode(json) {
                Ok(state) => state,
                Err(_) => {
                    record.correction_group_unavailable |= known;
                    continue;
                }
            };
            if !state.event.is_bounded()
                || state.event.object_id != record.id
                || state.event.call_id != record.call_id
                || state.event.attribution.provider_id != record.provider_id
                || state.event.attribution.model_id != record.model_id
                || state.event.attribution.configuration_revision != record.configuration_revision
                || state.event.attribution.contract_revision != record.contract_revision
            {
                record.correction_group_unavailable |= known;
                continue;
            }
            let Some(group) = state.group else {
                record.correction_group_unavailable |= known;
                continue;
            };
            if !group.is_bounded(&record.id) {
                record.correction_group_unavailable |= known;
                continue;
            }
            record.correction_group_unavailable = false;
            record.correction_group_root = Some(group.root_id);
            record.correction_group = group.summary.map(|group| CorrectionGroupRecord {
                root_id: group.root_id,
                last_input_id: group.last_input_id,
                category: tag(&group.category),
                reason: group.reason.map(|reason| tag(&reason)),
                outcome: tag(&group.outcome),
                linked_attempts: group.linked_attempts.to_string(),
                updated_at: desk_signal_facade::service::model_metrics::timestamp(
                    group.updated_at_ms,
                ),
            });
        }
        let roots: BTreeSet<_> = records
            .iter()
            .filter_map(|record| record.correction_group_root.clone())
            .collect();
        if roots.is_empty() {
            return Ok(());
        }
        let roots = compact::Entity::find()
            .filter(compact::Column::ObjectId.is_in(roots))
            .limit(201)
            .all(txn)
            .await?;
        let retained: BTreeSet<_> = roots
            .into_iter()
            .filter_map(|row| {
                let state: CompactState = decode(&row.snapshot_json).ok()?;
                let group = state.group.as_ref()?;
                let summary = group.summary.as_ref()?;
                (matches!(row.kind.as_str(), "tool" | "call")
                    && state.event.is_bounded()
                    && state.event.object_id == row.object_id
                    && group.is_bounded(&row.object_id)
                    && summary.outcome != CorrectionGroupOutcome::Unavailable)
                    .then_some(row.object_id)
            })
            .collect();
        for record in records {
            if record
                .correction_group_root
                .as_ref()
                .is_some_and(|root| !retained.contains(root))
            {
                record.correction_group_unavailable = true;
            }
        }
        Ok(())
    }
}
