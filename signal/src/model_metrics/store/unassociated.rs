//! Bounded unresolved facts are independent of normal observation contributions.

use super::*;
use desk_signal_facade::service::model_metrics::unassociated::UnassociatedSnapshot;
use sea_orm::QueryTrait;

fn identity(object_id: &str) -> String {
    format!("unassociated.{:x}", Sha256::digest(object_id.as_bytes()))
}

impl Store {
    pub(super) async fn association_candidates(
        &self,
        txn: &DatabaseTransaction,
        now: i64,
    ) -> Result<Vec<event::Model>, DbErr> {
        let aliases = compact::Entity::find()
            .select_only()
            .column(compact::Column::ObjectId)
            .filter(compact::Column::Kind.is_in(["binding", "call"]))
            .into_query();
        let operations = compact::Entity::find()
            .select_only()
            .column(compact::Column::ObjectId)
            .filter(compact::Column::Kind.eq("operation"))
            .into_query();
        event::Entity::find()
            .filter(event::Column::Applied.eq(true))
            .filter(event::Column::AssociationPending.eq(true))
            .filter(event::Column::NextAssociationAtMs.lte(now))
            .filter(event::Column::AssociationAlias.in_subquery(aliases))
            .filter(
                Condition::any()
                    .add(event::Column::RequiresOperationStart.eq(false))
                    .add(event::Column::ObjectId.in_subquery(operations)),
            )
            .order_by_asc(event::Column::NextAssociationAtMs)
            .order_by_asc(event::Column::Id)
            .limit(20)
            .all(txn)
            .await
    }

    // Keep the observed fact, missing prerequisite and optional source independent.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn retain_unassociated(
        &self,
        txn: &DatabaseTransaction,
        row: &event::Model,
        observed: &ObservationEvent,
        config: &MetricsSettings,
        now: i64,
        missing: AssociationGapKind,
        state: AssociationGapState,
        source: Option<&ObservationEvent>,
    ) -> Result<(), DbErr> {
        let id = identity(&row.object_id);
        let current = detail::Entity::find_by_id(&id).one(txn).await?;
        if current.is_none() {
            let earlier = event::Entity::find()
                .filter(event::Column::ObjectId.eq(&row.object_id))
                .filter(event::Column::Id.ne(row.id))
                .filter(event::Column::AssociationPending.eq(true))
                .one(txn)
                .await?;
            // A deleted or unretained detail remains a visible coverage gap;
            // repeated prerequisite checks must not reconstruct it.
            if row.association_pending || earlier.is_some() {
                return Ok(());
            }
        }
        let mut incoming = UnassociatedSnapshot::new(
            id.clone(),
            observed,
            row.received_at_ms,
            now,
            missing,
            state,
            source,
        )
        .ok_or_else(|| DbErr::Custom("metrics association fact unavailable".into()))?;
        if let Some(current) = &current {
            if current.kind != "unassociated" {
                return Err(DbErr::Custom(
                    "metrics association identity conflict".into(),
                ));
            }
            let previous: UnassociatedSnapshot = decode(&current.snapshot_json)?;
            if previous.object_id != row.object_id {
                return Err(DbErr::Custom(
                    "metrics association identity conflict".into(),
                ));
            }
            incoming.received_at_ms = previous.received_at_ms.min(row.received_at_ms);
            if incoming.attribution.is_none() {
                incoming.attribution = previous.attribution.clone();
                incoming.call_id = previous.call_id.clone();
                incoming.tool_observation_id = previous.tool_observation_id.clone();
                incoming.tool = previous.tool.clone();
            } else if previous.attribution.is_some()
                && (previous.attribution != incoming.attribution
                    || previous.call_id != incoming.call_id
                    || previous.tool_observation_id != incoming.tool_observation_id)
            {
                incoming = previous.clone();
                incoming.state = AssociationGapState::Conflict;
                incoming.updated_at_ms = now;
            }
            if observed.occurred_at_ms < previous.occurred_at_ms {
                incoming.occurred_at_ms = previous.occurred_at_ms;
                incoming.phase = previous.phase;
                incoming.ordinal = previous.ordinal;
                incoming.permission = previous.permission;
                incoming.fact_outcome = previous.fact_outcome;
            }
            if previous.state != AssociationGapState::Waiting {
                incoming.state = previous.state;
            }
        }
        let snapshot = encode(&incoming)?;
        let attrs = incoming.attribution.as_ref();
        let provider = attrs.map(|a| a.provider_id.clone()).unwrap_or_default();
        let model = attrs.map(|a| a.model_id.clone()).unwrap_or_default();
        let purpose = attrs.map(|a| tag(&a.purpose)).unwrap_or_default();
        let surface = attrs.map(|a| tag(&a.surface)).unwrap_or_default();
        let origin = attrs.map(|a| tag(&a.origin)).unwrap_or_default();
        let contract = attrs
            .map(|a| a.contract_revision.clone())
            .unwrap_or_default();
        let state = tag(&incoming.state);
        let bytes = resources::charge(&[
            &id,
            incoming.call_id.as_deref().unwrap_or(""),
            "unassociated",
            &provider,
            &model,
            &purpose,
            &surface,
            &origin,
            &contract,
            incoming.tool.as_deref().unwrap_or(""),
            &state,
            &snapshot,
        ])?;
        let old_bytes = current.as_ref().map_or(0, |row| row.storage_bytes);
        if old_bytes < 0 {
            return Err(DbErr::Custom("metrics accounting unavailable".into()));
        }
        if !resources::resize(
            txn,
            config,
            StorageKind::Detail,
            if current.is_none() { 1 } else { 0 },
            bytes - old_bytes,
        )
        .await?
        {
            resources::partial(txn).await?;
            return Ok(());
        }
        detail::Entity::insert(detail::ActiveModel {
            storage_bytes: Set(bytes),
            object_id: Set(id),
            call_id: Set(incoming.call_id.clone()),
            kind: Set("unassociated".into()),
            provider_id: Set(provider),
            model_id: Set(model),
            purpose: Set(purpose),
            surface: Set(surface),
            origin: Set(origin),
            contract_revision: Set(contract),
            tool: Set(incoming.tool.clone()),
            outcome: Set(state),
            output: Set(None),
            duration_ms: Set(None),
            first_content_ms: Set(None),
            permission: Set(None),
            dispatched: Set(None),
            issue: Set(None),
            retention_priority: Set(2),
            correction_group_observed: Set(false),
            started_at_ms: Set(None),
            received_at_ms: Set(incoming.received_at_ms),
            updated_at_ms: Set(now),
            snapshot_json: Set(snapshot),
        })
        .on_conflict(
            OnConflict::column(detail::Column::ObjectId)
                .update_columns([
                    detail::Column::StorageBytes,
                    detail::Column::CallId,
                    detail::Column::ProviderId,
                    detail::Column::ModelId,
                    detail::Column::Purpose,
                    detail::Column::Surface,
                    detail::Column::Origin,
                    detail::Column::ContractRevision,
                    detail::Column::Tool,
                    detail::Column::Outcome,
                    detail::Column::ReceivedAtMs,
                    detail::Column::UpdatedAtMs,
                    detail::Column::SnapshotJson,
                ])
                .to_owned(),
        )
        .exec_without_returning(txn)
        .await?;
        Ok(())
    }

    pub(super) async fn finish_association(
        &self,
        txn: &DatabaseTransaction,
        row: &event::Model,
        config: &MetricsSettings,
        reason: Option<&str>,
    ) -> Result<(), DbErr> {
        event::Entity::update_many()
            .set(event::ActiveModel {
                applied: Set(true),
                association_pending: Set(false),
                discarded_reason: Set(reason.map(str::to_owned)),
                ..Default::default()
            })
            .filter(event::Column::Id.eq(row.id))
            .exec(txn)
            .await?;
        if reason.is_some() {
            return Ok(());
        }
        if row.association_alias.is_none() && !row.association_pending {
            return Ok(());
        }
        let id = identity(&row.object_id);
        let Some(detail) = detail::Entity::find_by_id(&id).one(txn).await? else {
            return Ok(());
        };
        let snapshot: UnassociatedSnapshot =
            match decode::<UnassociatedSnapshot>(&detail.snapshot_json) {
                Ok(snapshot)
                    if detail.kind == "unassociated"
                        && snapshot.id == detail.object_id
                        && snapshot.object_id == row.object_id =>
                {
                    snapshot
                }
                _ => {
                    resources::partial(txn).await?;
                    return Ok(());
                }
            };
        if snapshot.state != AssociationGapState::Waiting {
            return Ok(());
        }
        let outstanding = event::Entity::find()
            .filter(event::Column::ObjectId.eq(&row.object_id))
            .filter(
                Condition::any()
                    .add(event::Column::AssociationPending.eq(true))
                    .add(event::Column::DiscardedReason.is_not_null()),
            )
            .one(txn)
            .await?;
        if outstanding.is_some() {
            return Ok(());
        }
        let removed = detail::Entity::delete_by_id(id).exec(txn).await?;
        if removed.rows_affected != 1 {
            return Err(DbErr::Custom("metrics association cleanup conflict".into()));
        }
        resources::release(txn, config, StorageKind::Detail, 1, detail.storage_bytes).await?;
        Ok(())
    }

    pub(super) async fn end_unassociated(
        &self,
        txn: &DatabaseTransaction,
        row: &event::Model,
        now: i64,
        state: AssociationGapState,
    ) -> Result<(), DbErr> {
        let isolated = txn.begin().await?;
        match self
            .update_unassociated_end(&isolated, row, now, state)
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

    async fn update_unassociated_end(
        &self,
        txn: &DatabaseTransaction,
        row: &event::Model,
        now: i64,
        state: AssociationGapState,
    ) -> Result<(), DbErr> {
        let id = identity(&row.object_id);
        if let Some(current) = detail::Entity::find_by_id(id).one(txn).await? {
            let mut snapshot: UnassociatedSnapshot = decode(&current.snapshot_json)?;
            snapshot.state = state;
            snapshot.updated_at_ms = now;
            let encoded = encode(&snapshot)?;
            // Terminal tags can change the snapshot's charge; use the same
            // accounting path instead of mutating JSON without resizing it.
            let (config_row, config) = resources::configuration(txn).await?;
            let delta = i64::try_from(encoded.len())
                .ok()
                .and_then(|size| size.checked_sub(current.snapshot_json.len() as i64))
                .and_then(|size| {
                    size.checked_add(tag(&state).len() as i64 - current.outcome.len() as i64)
                })
                .and_then(|delta| delta.checked_mul(2))
                .ok_or_else(|| DbErr::Custom("metrics accounting unavailable".into()))?;
            if resources::resize(txn, &config, StorageKind::Detail, 0, delta).await? {
                detail::Entity::update_many()
                    .set(detail::ActiveModel {
                        storage_bytes: Set(current.storage_bytes.checked_add(delta).ok_or_else(
                            || DbErr::Custom("metrics accounting unavailable".into()),
                        )?),
                        snapshot_json: Set(encoded),
                        outcome: Set(tag(&state)),
                        updated_at_ms: Set(now),
                        ..Default::default()
                    })
                    .filter(detail::Column::ObjectId.eq(&current.object_id))
                    .exec(txn)
                    .await?;
            } else if !config_row.coverage_partial {
                resources::partial(txn).await?;
            }
        }
        Ok(())
    }
}
