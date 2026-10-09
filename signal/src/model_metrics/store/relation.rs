//! Isolated alias writes and deferred facts preserve original model attribution.

use super::*;

pub(super) enum Resolution {
    Resolved(ObservationEvent),
    Pending,
    PendingSource(ObservationEvent, AssociationGapKind),
    Ignored,
    Unavailable,
    Conflict,
}

impl Store {
    pub(super) async fn bind_relation(
        &self,
        txn: &DatabaseTransaction,
        config: &MetricsSettings,
        alias: &ObservationAlias,
        source: &ObservationEvent,
        now: i64,
    ) -> Result<(), DbErr> {
        let id = alias.object_id();
        let mut source = source.clone();
        source.relation = None;
        if let Some(existing) = compact::Entity::find_by_id(&id).one(txn).await? {
            let original: ObservationEvent = decode(&existing.snapshot_json)?;
            if existing.kind != "binding"
                || original.object_id != source.object_id
                || original.call_id != source.call_id
                || original.started_at_ms != source.started_at_ms
                || original.attribution != source.attribution
            {
                resources::partial(txn).await?;
            }
            return Ok(());
        }
        let snapshot = encode(&source)?;
        let contribution = encode(&Contribution::default())?;
        let bytes = resources::charge(&[
            &id,
            source.call_id.as_deref().unwrap_or(""),
            "binding",
            &snapshot,
            &contribution,
        ])?;
        if !resources::resize(txn, config, StorageKind::Compact, 1, bytes).await? {
            resources::partial(txn).await?;
            return Ok(());
        }
        compact::Entity::insert(compact::ActiveModel {
            storage_bytes: Set(bytes),
            object_id: Set(id),
            call_id: Set(source.call_id.clone()),
            kind: Set("binding".into()),
            started_at_ms: Set(source.started_at_ms),
            updated_at_ms: Set(now),
            snapshot_json: Set(snapshot),
            contribution_json: Set(contribution),
            state_revision: Set(1),
            detail_trimmed: Set(true),
        })
        .exec_without_returning(txn)
        .await?;
        Ok(())
    }

    pub(super) async fn resolve_relation(
        &self,
        txn: &DatabaseTransaction,
        mut event: ObservationEvent,
    ) -> Result<Resolution, DbErr> {
        let Some(relation) = event.relation.clone() else {
            return Ok(Resolution::Resolved(event));
        };
        if matches!(relation, ObservationRelation::Bind(_)) {
            return Ok(Resolution::Resolved(event));
        }
        if let ObservationRelation::ProtocolCorrection { fact, .. } = &relation {
            return self.resolve_protocol_correction(txn, event, fact).await;
        }
        let Some(binding) = compact::Entity::find_by_id(relation.alias().object_id())
            .one(txn)
            .await?
        else {
            return Ok(Resolution::Pending);
        };
        if binding.kind != "binding" {
            return Ok(Resolution::Conflict);
        }
        let Ok(source) = decode::<ObservationEvent>(&binding.snapshot_json) else {
            return Ok(Resolution::Unavailable);
        };
        if !source.is_bounded() || !matches!(source.payload, ObservationPayload::Tool(_)) {
            return Ok(Resolution::Unavailable);
        }
        if let ObservationRelation::Correction { fact, .. } = &relation {
            let original = source.clone();
            return self
                .resolve_correction(txn, event, source, fact)
                .await
                .map(|resolution| match resolution {
                    Resolution::Pending => Resolution::PendingSource(
                        original,
                        AssociationGapKind::CorrectionPrerequisite,
                    ),
                    other => other,
                });
        }
        event.attribution = source.attribution.clone();
        event.call_id = source.call_id.clone();
        event.relation = None;
        match relation {
            ObservationRelation::ResolveOperation(_)
            | ObservationRelation::ResolveStartedOperation(_) => {
                let ObservationPayload::Operation(operation) = &mut event.payload else {
                    return Ok(Resolution::Conflict);
                };
                if event.object_id != relation.alias().operation_id(operation.ordinal) {
                    return Ok(Resolution::Conflict);
                }
                let ObservationPayload::Tool(tool) = &source.payload else {
                    return Ok(Resolution::Conflict);
                };
                operation.tool_key = Some(tool.tool_key.clone());
                operation.tool_observation_id = Some(source.object_id.clone());
                if matches!(relation, ObservationRelation::ResolveStartedOperation(_)) {
                    let Some(current) = compact::Entity::find_by_id(&event.object_id)
                        .one(txn)
                        .await?
                    else {
                        return Ok(Resolution::PendingSource(
                            source.clone(),
                            AssociationGapKind::OperationStart,
                        ));
                    };
                    if current.kind != "operation" {
                        return Ok(Resolution::Conflict);
                    }
                    let Ok(state) = decode::<CompactState>(&current.snapshot_json) else {
                        return Ok(Resolution::Unavailable);
                    };
                    let ObservationPayload::Operation(original) = &state.event.payload else {
                        return Ok(Resolution::Unavailable);
                    };
                    if !state.event.is_bounded()
                        || state.event.object_id != event.object_id
                        || state.event.started_at_ms != current.started_at_ms
                        || current.call_id != event.call_id
                        || state.event.call_id != event.call_id
                        || state.event.attribution != event.attribution
                        || original.ordinal != operation.ordinal
                        || original.tool_observation_id != operation.tool_observation_id
                        || original.tool_key != operation.tool_key
                    {
                        return Ok(Resolution::Conflict);
                    }
                    event.started_at_ms = state.event.started_at_ms;
                }
            }
            ObservationRelation::ResolveTool(_) | ObservationRelation::Link { .. } => {
                let ObservationPayload::Tool(patch) = &event.payload else {
                    return Ok(Resolution::Conflict);
                };
                let original = (
                    source.object_id.clone(),
                    source.call_id.clone(),
                    source.started_at_ms,
                    source.attribution.clone(),
                );
                let current = compact::Entity::find_by_id(&source.object_id)
                    .one(txn)
                    .await?;
                let mut base = if let Some(current) = current {
                    let Ok(state) = decode::<CompactState>(&current.snapshot_json) else {
                        return Ok(Resolution::Unavailable);
                    };
                    state.event
                } else {
                    source
                };
                if base.object_id != original.0
                    || base.call_id != original.1
                    || base.started_at_ms != original.2
                    || base.attribution != original.3
                {
                    return Ok(Resolution::Conflict);
                }
                let ObservationPayload::Tool(tool) = &mut base.payload else {
                    return Ok(Resolution::Conflict);
                };
                for (stage, outcome) in &patch.stages {
                    tool.stages.insert(*stage, *outcome);
                }
                if patch.conclusion != InputConclusion::Unknown {
                    tool.conclusion = patch.conclusion;
                    tool.issue = patch.issue;
                }
                if patch.permission != PermissionOutcome::NotReached {
                    tool.permission = patch.permission;
                }
                event.object_id = base.object_id;
                event.started_at_ms = base.started_at_ms;
                event.sequence = base
                    .sequence
                    .checked_add(1)
                    .ok_or_else(|| DbErr::Custom("metrics stage sequence exhausted".into()))?;
                event.payload = base.payload;
                if let ObservationRelation::Link { target, .. } = relation {
                    event.relation = Some(ObservationRelation::Bind(target));
                }
            }
            ObservationRelation::Bind(_)
            | ObservationRelation::Correction { .. }
            | ObservationRelation::ProtocolCorrection { .. } => unreachable!(),
        }
        Ok(Resolution::Resolved(event))
    }
}
