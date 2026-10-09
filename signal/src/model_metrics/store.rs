//! Bounded observation persistence, atomic projection and retention.

mod cleanup;
mod correction;
mod correction_group;
mod protocol_correction;
mod relation;
mod resources;
mod unassociated;

use desk_diagnose_core::model_observability::capacity::{
    CLEANUP_BATCH_ROWS, StorageKind, charged_bytes, data_budget, low_water, retention_priority,
};
use desk_diagnose_core::model_observability::{
    aggregate::{self, Contribution, MergeResult, Totals},
    *,
};
use desk_signal_facade::{
    model::model_metrics::*,
    service::model_metrics::{record, tag},
};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, DatabaseConnection, DatabaseTransaction, DbErr,
    EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, TransactionTrait,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use super::entity::{
    model_metric_compact as compact, model_metric_event as event, model_metric_health as health,
    model_metric_lease as lease, model_metric_record as detail, model_metric_rollup as rollup,
    model_metric_settings as settings,
};

const HOUR: i64 = 3_600_000;
const DAY: i64 = 86_400_000;

const MAX_PROJECTION_JSON_BYTES: usize = 64 * 1024;

pub fn encode<T: Serialize>(value: &T) -> Result<String, DbErr> {
    let value = serde_json::to_string(value)
        .map_err(|_| DbErr::Custom("metrics encoding failed".into()))?;
    if value.len() > MAX_PROJECTION_JSON_BYTES {
        return Err(DbErr::Custom("metrics projection size unavailable".into()));
    }
    Ok(value)
}

pub fn decode<T: DeserializeOwned>(value: &str) -> Result<T, DbErr> {
    if value.len() > MAX_PROJECTION_JSON_BYTES {
        return Err(DbErr::Custom("metrics projection size unavailable".into()));
    }
    serde_json::from_str(value).map_err(|_| DbErr::Custom("metrics format unavailable".into()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeriesDimensions {
    pub attribution: Option<Attribution>,
    pub probe: bool,
    pub tool: Option<String>,
    pub runtime_definition: Option<runtime::RuntimeDefinition>,
    pub runtime_labels: Option<runtime::RuntimeLabels>,
    pub other: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Assignment {
    key: String,
    width: i64,
    bucket: i64,
    series_kind: String,
    object_kind: String,
    dimensions: SeriesDimensions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompactState {
    event: ObservationEvent,
    assignments: Vec<Assignment>,
    correction: Option<correction::CorrectionState>,
    group: Option<correction_group::GroupMetadata>,
    protocol_correction: Option<protocol_correction::ProtocolState>,
}

#[derive(Clone)]
pub struct Store {
    pub db: DatabaseConnection,
    pub manager: bool,
    pub node_id: String,
}

#[derive(Debug, Default)]
pub struct BatchResult {
    pub applied: u32,
    pub discarded: u32,
    pub outside_window: u32,
    pub last_received_ms: Option<i64>,
}

impl Store {
    pub fn new(db: DatabaseConnection, manager: bool, node_id: String) -> Self {
        Self {
            db,
            manager,
            node_id,
        }
    }

    pub async fn initialize_settings(&self, now: i64) -> Result<MetricsSettings, DbErr> {
        let defaults = MetricsSettings::defaults(self.manager);
        settings::Entity::insert(settings::ActiveModel {
            id: Set(1),
            schema_version: Set(EVENT_SCHEMA_VERSION as i32),
            revision: Set(1),
            settings_json: Set(encode(&defaults)?),
            available_from_ms: Set(now),
            updated_at_ms: Set(now),
            frozen_before_ms: Set(0),
            coverage_partial: Set(false),
            event_rows: Set(0),
            compact_rows: Set(0),
            detail_rows: Set(0),
            rollup_rows: Set(0),
            storage_used_bytes: Set(0),
            event_cleanup_active: Set(false),
            compact_cleanup_active: Set(false),
            detail_cleanup_active: Set(false),
            rollup_cleanup_active: Set(false),
            storage_cleanup_active: Set(false),
            trimmed_details: Set(0),
            dropped_pending_events: Set(0),
            rollup_trim_before_ms: Set(0),
            physical_allocated_bytes: Set(None),
            physical_sampled_at_ms: Set(None),
        })
        .on_conflict(
            OnConflict::column(settings::Column::Id)
                .do_nothing()
                .to_owned(),
        )
        .exec_without_returning(&self.db)
        .await?;
        lease::Entity::insert(lease::ActiveModel {
            id: Set(1),
            owner: Set(String::new()),
            expires_at_ms: Set(0),
            generation: Set(0),
        })
        .on_conflict(
            OnConflict::column(lease::Column::Id)
                .do_nothing()
                .to_owned(),
        )
        .exec_without_returning(&self.db)
        .await?;
        self.load_settings().await
    }

    pub async fn load_settings(&self) -> Result<MetricsSettings, DbErr> {
        let row = settings::Entity::find_by_id(1)
            .one(&self.db)
            .await?
            .ok_or_else(|| DbErr::Custom("metrics settings unavailable".into()))?;
        if row.schema_version != EVENT_SCHEMA_VERSION as i32 || !resources::valid(&row) {
            return Err(DbErr::Custom("metrics schema unavailable".into()));
        }
        let value: MetricsSettings = decode(&row.settings_json)?;
        value
            .validate()
            .map_err(|_| DbErr::Custom("metrics settings unavailable".into()))?;
        if value.revision != row.revision.to_string() {
            return Err(DbErr::Custom("metrics settings revision conflict".into()));
        }
        Ok(value)
    }

    pub async fn save_settings(
        &self,
        mut value: MetricsSettings,
        now: i64,
    ) -> Result<Option<MetricsSettings>, DbErr> {
        value
            .validate()
            .map_err(|reason| DbErr::Custom(reason.into()))?;
        let revision = value
            .revision
            .parse::<i64>()
            .map_err(|_| DbErr::Custom("metrics revision out of range".into()))?;
        let next = revision
            .checked_add(1)
            .ok_or_else(|| DbErr::Custom("metrics revision exhausted".into()))?;
        value.revision = next.to_string();
        let changed = settings::Entity::update_many()
            .set(settings::ActiveModel {
                revision: Set(next),
                settings_json: Set(encode(&value)?),
                updated_at_ms: Set(now),
                ..Default::default()
            })
            .filter(settings::Column::Id.eq(1))
            .filter(settings::Column::Revision.eq(revision))
            .exec(&self.db)
            .await?;
        Ok((changed.rows_affected == 1).then_some(value))
    }

    pub async fn persist(&self, events: &[ObservationEvent], now: i64) -> Result<u32, DbErr> {
        if events.len() > 100 {
            return Err(DbErr::Custom("metrics batch exceeds budget".into()));
        }
        let txn = self.db.begin().await?;
        let (_, config) = resources::configuration(&txn).await?;
        let mut discarded = 0;
        for observed in events {
            let payload = encode(observed)?;
            if !observed.is_bounded() || payload.len() > MAX_EVENT_BYTES {
                discarded += 1;
                resources::partial(&txn).await?;
                continue;
            }
            let slot = format!(
                "{}.{}.{}",
                observed.object_id, observed.phase as u8, observed.sequence
            );
            let stored = event::Entity::find()
                .filter(
                    Condition::any()
                        .add(event::Column::EventId.eq(&observed.event_id))
                        .add(event::Column::PhaseSlot.eq(&slot)),
                )
                .all(&txn)
                .await?;
            if !stored.is_empty() {
                if !same_fact(&stored, observed) {
                    discarded += 1;
                    resources::partial(&txn).await?;
                }
                continue;
            }
            let association_alias = observed
                .relation
                .as_ref()
                .map(ObservationRelation::source_object_id);
            let bytes = resources::charge(&[
                &observed.event_id,
                &observed.object_id,
                &slot,
                &payload,
                association_alias.as_deref().unwrap_or(""),
            ])?;
            if !resources::resize(&txn, &config, StorageKind::Event, 1, bytes).await? {
                discarded += 1;
                resources::partial(&txn).await?;
                continue;
            }
            let inserted=event::Entity::insert(event::ActiveModel {
                storage_bytes: Set(bytes), event_id: Set(observed.event_id.clone()), object_id: Set(observed.object_id.clone()), phase_slot: Set(slot.clone()),
                payload_json: Set(payload), schema_version: Set(EVENT_SCHEMA_VERSION as i32),
                received_at_ms: Set(now), occurred_at_ms: Set(observed.occurred_at_ms), applied: Set(false),
                binding: Set(match &observed.relation {
                    Some(ObservationRelation::Bind(_))=>3,
                    Some(ObservationRelation::Correction { fact:desk_diagnose_core::model_observability::correction::CorrectionFact::Feedback { .. },.. })=>3,
                    Some(ObservationRelation::Correction { fact:desk_diagnose_core::model_observability::correction::CorrectionFact::Projected { .. },.. })=>2,
                    Some(ObservationRelation::Correction { .. })=>1,
                    Some(ObservationRelation::ProtocolCorrection { fact:desk_diagnose_core::model_observability::protocol_correction::ProtocolCorrectionFact::Feedback { .. },.. })=>3,
                    Some(ObservationRelation::ProtocolCorrection { fact:desk_diagnose_core::model_observability::protocol_correction::ProtocolCorrectionFact::Projected { .. },.. })=>2,
                    Some(ObservationRelation::ProtocolCorrection { .. })=>1,
                    Some(ObservationRelation::Link { .. })=>2,
                    Some(ObservationRelation::ResolveOperation(_) | ObservationRelation::ResolveTool(_)) if observed.phase==ObservationPhase::Dispatched=>1,
                    _=>0,
                }),
                association_pending:Set(false),association_alias:Set(association_alias),
                requires_operation_start:Set(matches!(&observed.relation,Some(ObservationRelation::ResolveStartedOperation(_)))),
                next_association_at_ms:Set(0),
                discarded_reason: Set(None), ..Default::default()
            }).on_conflict(OnConflict::new().do_nothing().to_owned())
                .exec_without_returning(&txn).await?;
            if inserted == 0 {
                resources::release(&txn, &config, StorageKind::Event, 1, bytes).await?;
                let stored = event::Entity::find()
                    .filter(
                        Condition::any()
                            .add(event::Column::EventId.eq(&observed.event_id))
                            .add(event::Column::PhaseSlot.eq(&slot)),
                    )
                    .all(&txn)
                    .await?;
                if !same_fact(&stored, observed) {
                    discarded += 1;
                    resources::partial(&txn).await?;
                }
            }
        }
        txn.commit().await?;
        Ok(discarded)
    }

    async fn claim(&self, txn: &DatabaseTransaction, now: i64) -> Result<bool, DbErr> {
        let changed = lease::Entity::update_many()
            .set(lease::ActiveModel {
                owner: Set(self.node_id.clone()),
                expires_at_ms: Set(now.saturating_add(10_000)),
                ..Default::default()
            })
            .col_expr(
                lease::Column::Generation,
                sea_orm::ExprTrait::add(Expr::col(lease::Column::Generation), 1),
            )
            .filter(lease::Column::Id.eq(1))
            .filter(
                Condition::any()
                    .add(lease::Column::ExpiresAtMs.lte(now))
                    .add(lease::Column::Owner.eq(&self.node_id)),
            )
            .exec(txn)
            .await?;
        Ok(changed.rows_affected == 1)
    }

    pub async fn aggregate(&self, now: i64) -> Result<BatchResult, DbErr> {
        let txn = self.db.begin().await?;
        if !self.claim(&txn, now).await? {
            txn.rollback().await?;
            return Ok(BatchResult::default());
        }
        let (config_row, config) = resources::configuration(&txn).await?;
        // Give every dependency class a bounded share. A missing source or a
        // result waiting for dispatch must not starve its later prerequisite.
        let mut pending = Vec::with_capacity(100);
        for priority in [3, 2, 1, 0] {
            pending.extend(
                event::Entity::find()
                    .filter(event::Column::Applied.eq(false))
                    .filter(event::Column::Binding.eq(priority))
                    .order_by_asc(event::Column::Id)
                    .limit(25)
                    .all(&txn)
                    .await?,
            );
        }
        if pending.len() < 100 {
            let remaining_limit = 100 - pending.len();
            let mut remaining = event::Entity::find().filter(event::Column::Applied.eq(false));
            if !pending.is_empty() {
                remaining =
                    remaining.filter(event::Column::Id.is_not_in(pending.iter().map(|row| row.id)));
            }
            pending.extend(
                remaining
                    .order_by_desc(event::Column::Binding)
                    .order_by_asc(event::Column::Id)
                    .limit(remaining_limit as u64)
                    .all(&txn)
                    .await?,
            );
        }
        pending.sort_by_key(|row| (std::cmp::Reverse(row.binding), row.id));
        let mut result = BatchResult::default();
        for row in pending {
            self.process_event(&txn, &row, &config_row, &config, now, &mut result)
                .await?;
        }
        // Recovery has its own fixed share and runs after new prerequisites.
        // Raw unresolved facts are acknowledged, so they do not occupy normal
        // event slots or contribute to model, input, operation or token totals.
        for row in self.association_candidates(&txn, now).await? {
            self.process_event(&txn, &row, &config_row, &config, now, &mut result)
                .await?;
        }
        if result.discarded > 0 {
            resources::partial(&txn).await?;
        }
        txn.commit().await?;
        Ok(result)
    }

    async fn process_event(
        &self,
        txn: &DatabaseTransaction,
        row: &event::Model,
        config_row: &settings::Model,
        config: &MetricsSettings,
        now: i64,
        result: &mut BatchResult,
    ) -> Result<(), DbErr> {
        result.last_received_ms = Some(
            result
                .last_received_ms
                .map_or(row.received_at_ms, |time| time.max(row.received_at_ms)),
        );
        let raw = match decode::<ObservationEvent>(&row.payload_json) {
            Ok(value) if value.is_bounded() => value,
            _ => {
                self.finish_association(txn, row, config, Some("invalid_format"))
                    .await?;
                result.discarded += 1;
                return Ok(());
            }
        };
        if raw.occurred_at_ms < 0
            || raw.started_at_ms < 0
            || raw.occurred_at_ms > now.saturating_add(300_000)
        {
            self.end_unassociated(txn, row, now, AssociationGapState::Unavailable)
                .await?;
            self.finish_association(txn, row, config, Some("clock_skew"))
                .await?;
            result.discarded += 1;
            return Ok(());
        }
        if row.association_pending
            && row.received_at_ms < now.saturating_sub(i64::from(config.mutable_days) * DAY)
        {
            self.end_unassociated(txn, row, now, AssociationGapState::OutsideWindow)
                .await?;
            self.finish_association(txn, row, config, Some("outside_window"))
                .await?;
            result.discarded += 1;
            result.outside_window += 1;
            return Ok(());
        }
        let resolution = match self.resolve_relation(txn, raw.clone()).await {
            Ok(resolution) => resolution,
            Err(DbErr::Custom(_)) => relation::Resolution::Unavailable,
            Err(error) => return Err(error),
        };
        let observed = match resolution {
            relation::Resolution::Resolved(value) => value,
            relation::Resolution::Ignored => {
                self.finish_association(txn, row, config, None).await?;
                result.applied += 1;
                return Ok(());
            }
            unresolved => {
                let (missing, state, source, pending) = match unresolved {
                    relation::Resolution::Pending => (
                        AssociationGapKind::Attribution,
                        AssociationGapState::Waiting,
                        None,
                        true,
                    ),
                    relation::Resolution::PendingSource(source, missing) => {
                        (missing, AssociationGapState::Waiting, Some(source), true)
                    }
                    relation::Resolution::Conflict => (
                        AssociationGapKind::Attribution,
                        AssociationGapState::Conflict,
                        None,
                        false,
                    ),
                    relation::Resolution::Unavailable => (
                        AssociationGapKind::Attribution,
                        AssociationGapState::Unavailable,
                        None,
                        false,
                    ),
                    relation::Resolution::Resolved(_) | relation::Resolution::Ignored => {
                        unreachable!()
                    }
                };
                let isolated = txn.begin().await?;
                let reason = match self
                    .retain_unassociated(
                        &isolated,
                        row,
                        &raw,
                        config,
                        now,
                        missing,
                        state,
                        source.as_ref(),
                    )
                    .await
                {
                    Ok(()) => {
                        isolated.commit().await?;
                        if pending {
                            None
                        } else {
                            Some(if state == AssociationGapState::Conflict {
                                "association_conflict"
                            } else {
                                "association_unavailable"
                            })
                        }
                    }
                    Err(DbErr::Custom(_)) => {
                        isolated.rollback().await?;
                        Some("projection_unavailable")
                    }
                    Err(error) => {
                        isolated.rollback().await?;
                        return Err(error);
                    }
                };
                if pending && reason.is_none() {
                    event::Entity::update_many()
                        .set(event::ActiveModel {
                            applied: Set(true),
                            association_pending: Set(true),
                            next_association_at_ms: Set(if row.association_pending {
                                now.saturating_add(1_000)
                            } else {
                                now
                            }),
                            discarded_reason: Set(None),
                            ..Default::default()
                        })
                        .filter(event::Column::Id.eq(row.id))
                        .exec(txn)
                        .await?;
                } else {
                    self.finish_association(txn, row, config, reason).await?;
                    result.discarded += 1;
                }
                return Ok(());
            }
        };
        let reason = if !observed.is_bounded() {
            Some("invalid_format")
        } else if observed.started_at_ms > now.saturating_add(300_000)
            || observed.occurred_at_ms > now.saturating_add(300_000)
        {
            Some("clock_skew")
        } else if observed.started_at_ms < config_row.available_from_ms {
            Some("before_capture")
        } else if observed.started_at_ms < config_row.frozen_before_ms
            || observed.started_at_ms < now.saturating_sub(i64::from(config.mutable_days) * DAY)
        {
            result.outside_window += 1;
            Some("outside_window")
        } else {
            let isolated = txn.begin().await?;
            match self.apply(&isolated, &observed, config, now).await {
                Ok(None) => {
                    isolated.commit().await?;
                    None
                }
                Ok(Some(reason)) => {
                    isolated.rollback().await?;
                    Some(reason)
                }
                Err(DbErr::Custom(_)) => {
                    isolated.rollback().await?;
                    Some("projection_unavailable")
                }
                Err(error) => {
                    isolated.rollback().await?;
                    return Err(error);
                }
            }
        };
        if row.association_pending && reason.is_some() {
            self.end_unassociated(
                txn,
                row,
                now,
                if reason == Some("outside_window") {
                    AssociationGapState::OutsideWindow
                } else {
                    AssociationGapState::Unavailable
                },
            )
            .await?;
        }
        self.finish_association(txn, row, config, reason).await?;
        if reason.is_some() {
            result.discarded += 1;
        } else {
            result.applied += 1;
        }
        Ok(())
    }

    async fn apply(
        &self,
        txn: &DatabaseTransaction,
        observed: &ObservationEvent,
        config: &MetricsSettings,
        now: i64,
    ) -> Result<Option<&'static str>, DbErr> {
        if let Some(ObservationRelation::ProtocolCorrection { fact, .. }) = &observed.relation {
            return self
                .apply_protocol_correction(txn, observed, config, now, fact)
                .await;
        }
        if let Some(ObservationRelation::Correction { fact, .. }) = &observed.relation {
            return self
                .apply_correction(txn, observed, config, now, fact)
                .await;
        }
        if let Some(reason) = self.apply_state(txn, observed, config, now, None).await? {
            return Ok(Some(reason));
        }
        self.sync_correction_conclusion(txn, &observed.object_id, config, now)
            .await?;
        self.sync_correction_groups(txn, &observed.object_id, config, now)
            .await?;
        Ok(None)
    }

    async fn apply_state(
        &self,
        txn: &DatabaseTransaction,
        observed: &ObservationEvent,
        config: &MetricsSettings,
        now: i64,
        correction: Option<correction::CorrectionState>,
    ) -> Result<Option<&'static str>, DbErr> {
        self.apply_state_with_group(txn, observed, config, now, correction, None)
            .await
    }

    async fn apply_state_with_group(
        &self,
        txn: &DatabaseTransaction,
        observed: &ObservationEvent,
        config: &MetricsSettings,
        now: i64,
        correction: Option<correction::CorrectionState>,
        group: Option<Option<correction_group::GroupMetadata>>,
    ) -> Result<Option<&'static str>, DbErr> {
        self.apply_state_metadata(txn, observed, config, now, correction, group, None)
            .await
    }

    // Independent correction axes preserve omitted, cleared and changed metadata.
    #[allow(clippy::too_many_arguments)]
    async fn apply_state_metadata(
        &self,
        txn: &DatabaseTransaction,
        observed: &ObservationEvent,
        config: &MetricsSettings,
        now: i64,
        correction: Option<correction::CorrectionState>,
        group: Option<Option<correction_group::GroupMetadata>>,
        protocol: Option<protocol_correction::ProtocolState>,
    ) -> Result<Option<&'static str>, DbErr> {
        let existing = compact::Entity::find_by_id(&observed.object_id)
            .one(txn)
            .await?;
        let (mut state, old, revision, trimmed) = if let Some(row) = &existing {
            let state: CompactState = decode(&row.snapshot_json)?;
            let old: Contribution = decode(&row.contribution_json)?;
            (state, old, row.state_revision, row.detail_trimmed)
        } else {
            let assignments = self.assignments(txn, observed, config).await?;
            (
                CompactState {
                    event: observed.clone(),
                    assignments,
                    correction: None,
                    group: None,
                    protocol_correction: None,
                },
                Contribution::default(),
                0,
                false,
            )
        };
        let correction_changed = correction
            .as_ref()
            .is_some_and(|incoming| state.correction.as_ref() != Some(incoming));
        if correction_changed {
            state.correction = correction;
        }
        let protocol_changed = protocol
            .as_ref()
            .is_some_and(|incoming| state.protocol_correction.as_ref() != Some(incoming));
        if let Some(protocol) = protocol {
            state.protocol_correction = Some(protocol);
        }
        let group_changed = group
            .as_ref()
            .is_some_and(|incoming| state.group != *incoming);
        if let Some(group) = group {
            state.group = group;
        }
        if existing.is_some() {
            match aggregate::merge(&mut state.event, observed) {
                MergeResult::Duplicate
                    if !correction_changed && !group_changed && !protocol_changed =>
                {
                    if let Some(ObservationRelation::Bind(alias)) = &observed.relation {
                        self.bind_relation(txn, config, alias, &state.event, now)
                            .await?;
                    }
                    return Ok(None);
                }
                MergeResult::Conflict => return Ok(Some("state_conflict")),
                MergeResult::Applied | MergeResult::Duplicate => {}
            }
        }
        if !self.manager
            && !state
                .assignments
                .iter()
                .any(|a| a.series_kind.starts_with("usage_"))
            && let ObservationPayload::Call(call) = &state.event.payload
            && let Some(metered_at) = call.metered_at_ms
        {
            let mut usage_event = state.event.clone();
            usage_event.started_at_ms = metered_at;
            let usage_assignments = self.assignments(txn, &usage_event, config).await?;
            state
                .assignments
                .extend(usage_assignments.into_iter().filter_map(|mut a| {
                    if a.series_kind != "global" && a.series_kind != "model" {
                        return None;
                    }
                    a.series_kind = format!("usage_{}", a.series_kind);
                    a.key = series_key(
                        a.width,
                        a.bucket,
                        &a.series_kind,
                        &a.object_kind,
                        &a.dimensions,
                    )
                    .ok()?;
                    Some(a)
                }));
        }
        let mut new = aggregate::contribution(&state.event.payload, !self.manager);
        if let Some(group) = &state.group {
            if !group.is_bounded(&state.event.object_id) {
                return Err(DbErr::Custom("metrics correction group unavailable".into()));
            }
            if let Some(summary) = &group.summary {
                summary.contribute(&mut new);
            }
        }
        let mut prepared = Vec::with_capacity(state.assignments.len());
        let mut missing = 0i64;
        let mut rollup_delta = 0i64;
        for assigned in &state.assignments {
            let current = rollup::Entity::find()
                .filter(rollup::Column::SeriesKey.eq(&assigned.key))
                .one(txn)
                .await?;
            let mut totals = current
                .as_ref()
                .map(|row| decode::<Totals>(&row.totals_json))
                .transpose()?
                .unwrap_or_default();
            if current.as_ref().is_some_and(|row| row.frozen) {
                return Ok(Some("outside_window"));
            }
            let previous =
                contribution_axis(&old, &assigned.series_kind, assigned.dimensions.other);
            let incoming =
                contribution_axis(&new, &assigned.series_kind, assigned.dimensions.other);
            if totals.change(&previous, &incoming).is_none() {
                return Err(DbErr::Custom("metrics arithmetic unavailable".into()));
            }
            let dimensions = encode(&assigned.dimensions)?;
            let totals = encode(&totals)?;
            let attr = assigned.dimensions.attribution.as_ref();
            let provider_id = attr.map(|a| a.provider_id.clone());
            let model_id = attr.map(|a| a.model_id.clone());
            let surface = attr.map(|a| tag(&a.surface));
            let purpose = attr.map(|a| tag(&a.purpose));
            let origin = attr.map(|a| tag(&a.origin));
            let contract_revision = attr.map(|a| a.contract_revision.clone());
            let runtime_category = assigned
                .dimensions
                .runtime_definition
                .map(|value| tag(&value.category()));
            let runtime_definition = assigned
                .dimensions
                .runtime_definition
                .map(|value| tag(&value));
            let bytes = resources::charge(&[
                &assigned.key,
                &assigned.series_kind,
                &assigned.object_kind,
                &dimensions,
                &totals,
                provider_id.as_deref().unwrap_or(""),
                model_id.as_deref().unwrap_or(""),
                surface.as_deref().unwrap_or(""),
                purpose.as_deref().unwrap_or(""),
                origin.as_deref().unwrap_or(""),
                contract_revision.as_deref().unwrap_or(""),
                assigned.dimensions.tool.as_deref().unwrap_or(""),
                runtime_category.as_deref().unwrap_or(""),
                runtime_definition.as_deref().unwrap_or(""),
            ])?;
            let old_bytes = current.as_ref().map_or(0, |row| row.storage_bytes);
            if old_bytes < 0 {
                return Err(DbErr::Custom("metrics accounting unavailable".into()));
            }
            rollup_delta = rollup_delta
                .checked_add(bytes - old_bytes)
                .ok_or_else(|| DbErr::Custom("metrics byte accounting unavailable".into()))?;
            if current.is_none() {
                missing += 1;
            }
            prepared.push(rollup::ActiveModel {
                storage_bytes: Set(bytes),
                series_key: Set(assigned.key.clone()),
                bucket_ms: Set(assigned.bucket),
                granularity_ms: Set(assigned.width),
                series_kind: Set(assigned.series_kind.clone()),
                object_kind: Set(assigned.object_kind.clone()),
                dimensions_json: Set(dimensions),
                totals_json: Set(totals),
                provider_id: Set(provider_id),
                model_id: Set(model_id),
                surface: Set(surface),
                purpose: Set(purpose),
                origin: Set(origin),
                contract_revision: Set(contract_revision),
                tool: Set(assigned.dimensions.tool.clone()),
                probe: Set(assigned.dimensions.probe),
                other: Set(assigned.dimensions.other),
                runtime_category: Set(runtime_category),
                runtime_definition: Set(runtime_definition),
                partial: Set(
                    assigned.dimensions.other || current.as_ref().is_some_and(|row| row.partial)
                ),
                frozen: Set(false),
                updated_at_ms: Set(now),
                ..Default::default()
            });
        }
        let snapshot = encode(&state)?;
        let contribution = encode(&new)?;
        let kind = tag(&state.event.payload.kind());
        let compact_bytes = resources::charge(&[
            &observed.object_id,
            state.event.call_id.as_deref().unwrap_or(""),
            &kind,
            &snapshot,
            &contribution,
        ])?;
        let old_compact_bytes = existing.as_ref().map_or(0, |row| row.storage_bytes);
        if old_compact_bytes < 0 {
            return Err(DbErr::Custom("metrics accounting unavailable".into()));
        }
        if !resources::resize(txn, config, StorageKind::Rollup, missing, rollup_delta).await? {
            return Ok(Some("rollup_budget"));
        }
        if !resources::resize(
            txn,
            config,
            StorageKind::Compact,
            if existing.is_none() { 1 } else { 0 },
            compact_bytes - old_compact_bytes,
        )
        .await?
        {
            return Ok(Some("compact_budget"));
        }
        for active in prepared {
            rollup::Entity::insert(active)
                .on_conflict(
                    OnConflict::column(rollup::Column::SeriesKey)
                        .update_columns([
                            rollup::Column::StorageBytes,
                            rollup::Column::TotalsJson,
                            rollup::Column::Partial,
                            rollup::Column::UpdatedAtMs,
                        ])
                        .to_owned(),
                )
                .exec_without_returning(txn)
                .await?;
        }
        let mut trimmed = trimmed;
        if !trimmed
            && state.event.started_at_ms >= now.saturating_sub(i64::from(config.detail_days) * DAY)
        {
            let current = detail::Entity::find_by_id(&observed.object_id)
                .one(txn)
                .await?;
            let projected = record(&state.event, false);
            let snapshot = encode(&state.event)?;
            let bytes = resources::charge(&[
                &projected.id,
                projected.call_id.as_deref().unwrap_or(""),
                &projected.kind,
                &projected.provider_id,
                &projected.model_id,
                &projected.purpose,
                &projected.surface,
                &projected.origin,
                &projected.contract_revision,
                projected.tool.as_deref().unwrap_or(""),
                &projected.outcome,
                projected.output.as_deref().unwrap_or(""),
                projected.input_issue.as_deref().unwrap_or(""),
                projected.permission.as_deref().unwrap_or(""),
                &snapshot,
            ])?;
            let old_bytes = current.as_ref().map_or(0, |row| row.storage_bytes);
            if old_bytes < 0 {
                return Err(DbErr::Custom("metrics accounting unavailable".into()));
            }
            if resources::resize(
                txn,
                config,
                StorageKind::Detail,
                if current.is_none() { 1 } else { 0 },
                bytes - old_bytes,
            )
            .await?
            {
                detail::Entity::insert(detail::ActiveModel {
                    storage_bytes: Set(bytes),
                    object_id: Set(projected.id),
                    call_id: Set(projected.call_id),
                    kind: Set(projected.kind),
                    provider_id: Set(projected.provider_id),
                    model_id: Set(projected.model_id),
                    purpose: Set(projected.purpose),
                    surface: Set(projected.surface),
                    origin: Set(projected.origin),
                    contract_revision: Set(projected.contract_revision),
                    tool: Set(projected.tool),
                    outcome: Set(projected.outcome),
                    output: Set(projected.output),
                    issue: Set(projected.input_issue),
                    duration_ms: Set(projected
                        .duration_ms
                        .and_then(|value| i64::try_from(value).ok())),
                    first_content_ms: Set(projected
                        .first_content_ms
                        .and_then(|value| i64::try_from(value).ok())),
                    permission: Set(projected.permission),
                    dispatched: Set(projected.dispatched),
                    retention_priority: Set(retention_priority(&state.event.payload)),
                    correction_group_observed: Set(state.group.is_some()),
                    started_at_ms: Set(Some(state.event.started_at_ms)),
                    received_at_ms: Set(current.as_ref().map_or(now, |row| row.received_at_ms)),
                    updated_at_ms: Set(now),
                    snapshot_json: Set(snapshot),
                })
                .on_conflict(
                    OnConflict::column(detail::Column::ObjectId)
                        .update_columns([
                            detail::Column::StorageBytes,
                            detail::Column::SnapshotJson,
                            detail::Column::UpdatedAtMs,
                            detail::Column::Outcome,
                            detail::Column::Output,
                            detail::Column::Issue,
                            detail::Column::DurationMs,
                            detail::Column::FirstContentMs,
                            detail::Column::Permission,
                            detail::Column::Dispatched,
                            detail::Column::RetentionPriority,
                            detail::Column::CorrectionGroupObserved,
                        ])
                        .to_owned(),
                )
                .exec_without_returning(txn)
                .await?;
            } else {
                if let Some(current) = current {
                    detail::Entity::delete_by_id(&current.object_id)
                        .exec(txn)
                        .await?;
                    resources::release(txn, config, StorageKind::Detail, 1, current.storage_bytes)
                        .await?;
                }
                trimmed = true;
                settings::Entity::update_many()
                    .col_expr(
                        settings::Column::TrimmedDetails,
                        sea_orm::ExprTrait::add(Expr::col(settings::Column::TrimmedDetails), 1),
                    )
                    .filter(settings::Column::Id.eq(1))
                    .exec(txn)
                    .await?;
            }
        }
        compact::Entity::insert(compact::ActiveModel {
            storage_bytes: Set(compact_bytes),
            object_id: Set(observed.object_id.clone()),
            call_id: Set(state.event.call_id.clone()),
            kind: Set(kind),
            started_at_ms: Set(state.event.started_at_ms),
            updated_at_ms: Set(now),
            snapshot_json: Set(snapshot),
            contribution_json: Set(contribution),
            state_revision: Set(revision
                .checked_add(1)
                .ok_or_else(|| DbErr::Custom("metrics state revision exhausted".into()))?),
            detail_trimmed: Set(trimmed),
        })
        .on_conflict(
            OnConflict::column(compact::Column::ObjectId)
                .update_columns([
                    compact::Column::StorageBytes,
                    compact::Column::SnapshotJson,
                    compact::Column::ContributionJson,
                    compact::Column::StateRevision,
                    compact::Column::UpdatedAtMs,
                    compact::Column::DetailTrimmed,
                ])
                .to_owned(),
        )
        .exec_without_returning(txn)
        .await?;
        if let Some(ObservationRelation::Bind(alias)) = &observed.relation {
            self.bind_relation(txn, config, alias, &state.event, now)
                .await?;
        }
        Ok(None)
    }

    async fn assignments(
        &self,
        txn: &DatabaseTransaction,
        observed: &ObservationEvent,
        config: &MetricsSettings,
    ) -> Result<Vec<Assignment>, DbErr> {
        let probe = observed.attribution.purpose == Purpose::Probe;
        let mut dimensions = vec![(
            "global",
            SeriesDimensions {
                attribution: None,
                probe,
                tool: None,
                runtime_definition: None,
                runtime_labels: None,
                other: false,
            },
        )];
        if let ObservationPayload::Runtime(runtime) = &observed.payload {
            dimensions.push((
                "runtime",
                SeriesDimensions {
                    attribution: Some(observed.attribution.clone()),
                    probe,
                    tool: None,
                    runtime_definition: Some(runtime.definition),
                    runtime_labels: Some(runtime.labels.clone()),
                    other: false,
                },
            ));
        } else {
            dimensions.push((
                "model",
                SeriesDimensions {
                    attribution: Some(observed.attribution.clone()),
                    probe,
                    tool: None,
                    runtime_definition: None,
                    runtime_labels: None,
                    other: false,
                },
            ));
            let tool_key = match &observed.payload {
                ObservationPayload::Tool(tool) => Some(&tool.tool_key),
                ObservationPayload::Operation(operation) => operation.tool_key.as_ref(),
                _ => None,
            };
            if let Some(tool_key) = tool_key {
                dimensions.push((
                    "tool",
                    SeriesDimensions {
                        attribution: Some(observed.attribution.clone()),
                        probe,
                        tool: Some(tool_key.clone()),
                        runtime_definition: None,
                        runtime_labels: None,
                        other: false,
                    },
                ));
            }
        }
        let mut result = Vec::new();
        for width in [300_000, HOUR] {
            let bucket = observed.started_at_ms.div_euclid(width) * width;
            for (series_kind, original) in &dimensions {
                let mut dims = original.clone();
                let object_kind = tag(&observed.payload.kind());
                let mut key = series_key(width, bucket, series_kind, &object_kind, &dims)?;
                if *series_kind != "global"
                    && rollup::Entity::find()
                        .filter(rollup::Column::SeriesKey.eq(&key))
                        .one(txn)
                        .await?
                        .is_none()
                {
                    let used = rollup::Entity::find()
                        .filter(rollup::Column::BucketMs.eq(bucket))
                        .filter(rollup::Column::GranularityMs.eq(width))
                        .filter(rollup::Column::SeriesKind.ne("global"))
                        .count(txn)
                        .await?;
                    if used >= u64::from(config.series_per_bucket) {
                        dims = SeriesDimensions {
                            attribution: None,
                            probe,
                            tool: None,
                            runtime_definition: None,
                            runtime_labels: None,
                            other: true,
                        };
                        key = series_key(width, bucket, series_kind, &object_kind, &dims)?;
                    }
                }
                result.push(Assignment {
                    key,
                    width,
                    bucket,
                    series_kind: (*series_kind).into(),
                    object_kind,
                    dimensions: dims,
                });
            }
        }
        Ok(result)
    }

    pub async fn report_health(
        &self,
        health: &desk_signal_facade::service::model_metrics::collector::WriterHealth,
    ) -> Result<(), DbErr> {
        if self.node_id.is_empty()
            || self.node_id.len() > 128
            || self.node_id.chars().any(char::is_control)
        {
            return Err(DbErr::Custom("metrics node identity unavailable".into()));
        }
        let txn = self.db.begin().await?;
        resources::configuration(&txn).await?;
        // Bound metadata independently inside the reserved component allowance.
        if health::Entity::find_by_id(&self.node_id)
            .one(&txn)
            .await?
            .is_none()
            && health::Entity::find().count(&txn).await? >= 128
        {
            resources::partial(&txn).await?;
            txn.commit().await?;
            return Err(DbErr::Custom(
                "metrics node health capacity unavailable".into(),
            ));
        }
        health::Entity::insert(health::ActiveModel {
            node_id: Set(self.node_id.clone()),
            reported_at_ms: Set(health.reported_at_ms),
            state: Set(tag(&health.state)),
            persisted_at_ms: Set(health.last_persisted_ms),
            aggregated_at_ms: Set(health.last_aggregated_ms),
            dropped_events: Set(health.dropped.to_string()),
            discarded_events: Set(health.discarded.to_string()),
            config_revision: Set(health.settings_revision.clone()),
            reason: Set(health.reason.map(str::to_owned)),
        })
        .on_conflict(
            OnConflict::column(health::Column::NodeId)
                .update_columns([
                    health::Column::ReportedAtMs,
                    health::Column::State,
                    health::Column::PersistedAtMs,
                    health::Column::AggregatedAtMs,
                    health::Column::DroppedEvents,
                    health::Column::DiscardedEvents,
                    health::Column::ConfigRevision,
                    health::Column::Reason,
                ])
                .to_owned(),
        )
        .exec_without_returning(&txn)
        .await?;
        txn.commit().await?;
        Ok(())
    }

    pub async fn record_physical_allocation(&self, bytes: i64, now: i64) -> Result<(), DbErr> {
        if bytes < 0 {
            return Err(DbErr::Custom(
                "metrics physical measurement unavailable".into(),
            ));
        }
        settings::Entity::update_many()
            .set(settings::ActiveModel {
                physical_allocated_bytes: Set(Some(bytes)),
                physical_sampled_at_ms: Set(Some(now)),
                ..Default::default()
            })
            .filter(settings::Column::Id.eq(1))
            .exec(&self.db)
            .await?;
        Ok(())
    }
}

fn same_fact(rows: &[event::Model], observed: &ObservationEvent) -> bool {
    rows.len() == 1
        && decode::<ObservationEvent>(&rows[0].payload_json)
            .ok()
            .is_some_and(|mut stored| {
                stored.event_id.clone_from(&observed.event_id);
                // Replaying a stable association or initial permission snapshot is the
                // same fact even when another business continuation submits it later.
                if matches!(&observed.relation, Some(ObservationRelation::Link { .. }))
                    || (matches!(
                        &observed.relation,
                        Some(ObservationRelation::ResolveTool(_))
                    ) && observed.phase == ObservationPhase::Stage
                        && observed.sequence == 0)
                {
                    stored.occurred_at_ms = observed.occurred_at_ms;
                }
                stored == *observed
            })
}

fn series_key(
    width: i64,
    bucket: i64,
    series_kind: &str,
    object_kind: &str,
    dimensions: &SeriesDimensions,
) -> Result<String, DbErr> {
    let identity = encode(&(width, bucket, series_kind, object_kind, dimensions))?;
    Ok(format!("{:x}", Sha256::digest(identity.as_bytes())))
}

#[async_trait::async_trait]
impl desk_signal_facade::service::model_metrics::collector::MetricsBackend for Store {
    async fn settings(&self) -> Result<MetricsSettings, ()> {
        self.initialize_settings(chrono::Utc::now().timestamp_millis())
            .await
            .map_err(|_| ())
    }
    async fn persist(&self, events: &[ObservationEvent], now: i64) -> Result<u32, ()> {
        Store::persist(self, events, now).await.map_err(|_| ())
    }
    async fn aggregate(
        &self,
        now: i64,
    ) -> Result<desk_signal_facade::service::model_metrics::collector::AggregateProgress, ()> {
        let result = Store::aggregate(self, now).await.map_err(|_| ())?;
        Ok(
            desk_signal_facade::service::model_metrics::collector::AggregateProgress {
                applied: result.applied,
                discarded: result.discarded,
                last_received_ms: result.last_received_ms,
            },
        )
    }
    async fn cleanup(&self, now: i64) -> Result<(), ()> {
        Store::cleanup(self, now).await.map_err(|_| ())
    }
    async fn report(
        &self,
        health: &desk_signal_facade::service::model_metrics::collector::WriterHealth,
    ) -> Result<(), ()> {
        self.report_health(health).await.map_err(|_| ())
    }
}

fn contribution_axis(value: &Contribution, series_kind: &str, other: bool) -> Contribution {
    use aggregate::Count;
    let usage = series_kind.starts_with("usage_");
    let mut result = value.clone();
    result.counts.retain(|key, _| {
        let token = matches!(
            key,
            Count::MeteredCalls
                | Count::InputTokens
                | Count::OutputTokens
                | Count::CacheReadTokens
                | Count::CacheWriteTokens
        );
        if usage { token } else { !token }
    });
    if usage {
        result.duration_ms = None;
        result.first_content_ms = None;
        result.quantities.clear();
        result.error_counts.clear();
    }
    if value.kind == ObjectKind::Runtime && (series_kind == "global" || other) {
        result.counts.remove(&Count::RuntimeValue);
        result.quantities.clear();
        result.duration_ms = None;
        result.first_content_ms = None;
    }
    result
}

#[cfg(test)]
mod tests {
    mod end_to_end;
    async fn fixture_store(now: i64) -> super::Store {
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        crate::model_metrics::runtime::create_schema(&db)
            .await
            .unwrap();
        let store = super::Store::new(db, false, "first-node".into());
        store.initialize_settings(now).await.unwrap();
        store
    }
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../signal-facade/tests/fixtures/model_metrics_store.rs"
    ));
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../signal-facade/tests/fixtures/model_metrics_correction.rs"
    ));
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../signal-facade/tests/fixtures/model_metrics_protocol_correction.rs"
    ));
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../signal-facade/tests/fixtures/model_metrics_unassociated.rs"
    ));
}
