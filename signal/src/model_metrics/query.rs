//! Bounded, consistent query snapshots over validated observation projections.

use crate::config::ConfigConnection;

use crate::config::connection::DatabaseTransaction;
use std::collections::{BTreeMap, BTreeSet};

use desk_diagnose_core::model_observability::{ObservationEvent, aggregate::Totals};
use desk_signal_facade::{
    model::model_metrics::*,
    service::model_metrics::{ResolvedQuery, observed_summary, record, timestamp},
};
use sea_orm::{
    AccessMode, ColumnTrait, Condition, DbErr, EntityTrait, IsolationLevel, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, TransactionTrait,
};

use super::{
    entity::{
        model_metric_compact as compact, model_metric_event as event,
        model_metric_health as health, model_metric_record as detail,
        model_metric_rollup as rollup, model_metric_state as settings,
    },
    store::{SeriesDimensions, Store, decode},
};

const MAX_QUERY_ROWS: u64 = 20_000;

struct ReadRows {
    rows: Vec<(rollup::Model, SeriesDimensions, Totals)>,
    partial: bool,
    available_from: i64,
    retained_from: i64,
}

fn retained_coverage(
    query: &ResolvedQuery,
    now: i64,
    available_from: i64,
    retained_from: i64,
    partial: bool,
    manager: bool,
) -> QueryCoverage {
    let mut result = query.coverage(now, available_from, partial, manager);
    if retained_from > available_from
        && query.from_ms < retained_from
        && query.requested_to_ms > available_from
    {
        result.trimmed_before = Some(timestamp(retained_from));
        result.effective_from = timestamp(query.from_ms.max(retained_from).min(query.to_ms));
        result.sample_status = if query.requested_to_ms <= retained_from {
            SampleStatus::Unavailable
        } else {
            SampleStatus::Partial
        };
    }
    result
}

fn retained_summary(
    totals: &Totals,
    query: &ResolvedQuery,
    partial: bool,
    available_from: i64,
    retained_from: i64,
) -> MetricsSummary {
    let mut result = observed_summary(totals, query, partial, available_from);
    if retained_from > available_from
        && query.requested_to_ms > available_from
        && query.requested_to_ms <= retained_from
    {
        for rate in &mut result.rates {
            if rate.sample_status == SampleStatus::NotApplicable {
                continue;
            }
            rate.sample_status = SampleStatus::Unavailable;
            rate.numerator = None;
            rate.denominator = None;
            rate.value = None;
            rate.reason = Some(MetricRateReason::RetentionTrim);
        }
    }
    result
}

impl Store {
    async fn read_rows(
        &self,
        txn: &DatabaseTransaction,
        query: &ResolvedQuery,
        kind: &str,
    ) -> Result<ReadRows, DbErr> {
        self.read_rows_with_runtime(txn, query, kind, None, None)
            .await
    }

    async fn read_rows_with_runtime(
        &self,
        txn: &DatabaseTransaction,
        query: &ResolvedQuery,
        kind: &str,
        category: Option<desk_diagnose_core::model_observability::RuntimeCategory>,
        definition: Option<desk_diagnose_core::model_observability::runtime::RuntimeDefinition>,
    ) -> Result<ReadRows, DbErr> {
        let config = settings::Entity::find_by_id(1)
            .one(txn)
            .await?
            .ok_or_else(|| DbErr::Custom("metrics unavailable".into()))?;
        if query.requested_to_ms <= config.available_from_ms {
            return Ok(ReadRows {
                rows: Vec::new(),
                partial: false,
                available_from: config.available_from_ms,
                retained_from: config.rollup_trim_before_ms,
            });
        }
        let mut qualified = Condition::all();
        if let Some(value) = &query.provider_id {
            qualified = qualified.add(rollup::Column::ProviderId.eq(value));
        }
        if let Some(value) = &query.model_id {
            qualified = qualified.add(rollup::Column::ModelId.eq(value));
        }
        if let Some(value) = query.surface {
            qualified = qualified.add(
                rollup::Column::Surface.eq(desk_signal_facade::service::model_metrics::tag(&value)),
            );
        }
        if let Some(value) = query.purpose {
            qualified = qualified.add(
                rollup::Column::Purpose.eq(desk_signal_facade::service::model_metrics::tag(&value)),
            );
        }
        if let Some(value) = query.origin {
            qualified = qualified.add(
                rollup::Column::Origin.eq(desk_signal_facade::service::model_metrics::tag(&value)),
            );
        }
        if let Some(value) = &query.contract_revision {
            qualified = qualified.add(rollup::Column::ContractRevision.eq(value));
        }
        if let Some(value) = &query.tool {
            qualified = qualified.add(rollup::Column::Tool.eq(value));
        }
        if let Some(value) = category {
            qualified = qualified.add(
                rollup::Column::RuntimeCategory
                    .eq(desk_signal_facade::service::model_metrics::tag(&value)),
            );
        }
        if let Some(value) = definition {
            qualified = qualified.add(
                rollup::Column::RuntimeDefinition
                    .eq(desk_signal_facade::service::model_metrics::tag(&value)),
            );
        }
        let mut selection = rollup::Entity::find()
            .filter(rollup::Column::BucketMs.gte(query.from_ms))
            .filter(rollup::Column::BucketMs.lt(query.to_ms))
            .filter(rollup::Column::BucketMs.gte(config.rollup_trim_before_ms))
            .filter(rollup::Column::GranularityMs.eq(query.granularity_ms))
            .filter({
                let kinds = Condition::any().add(rollup::Column::SeriesKind.eq(kind));
                if !self.manager && matches!(kind, "global" | "model") {
                    kinds.add(rollup::Column::SeriesKind.eq(format!("usage_{kind}")))
                } else {
                    kinds
                }
            })
            .filter(rollup::Column::Probe.is_in(if query.include_probe {
                vec![false, true]
            } else {
                vec![false]
            }));
        if query.has_dimensions() || category.is_some() || definition.is_some() {
            selection = selection.filter(
                Condition::any()
                    .add(qualified)
                    .add(rollup::Column::Other.eq(true)),
            );
        }
        let rows = selection
            .order_by_asc(rollup::Column::BucketMs)
            .order_by_asc(rollup::Column::Id)
            .limit(MAX_QUERY_ROWS + 1)
            .all(txn)
            .await?;
        let association_gap = detail::Entity::find()
            .filter(detail::Column::Kind.eq("unassociated"))
            .one(txn)
            .await?
            .is_some();
        let mut partial =
            config.coverage_partial || association_gap || rows.len() as u64 > MAX_QUERY_ROWS;
        let mut result = Vec::new();
        for row in rows.into_iter().take(MAX_QUERY_ROWS as usize) {
            let dimensions: SeriesDimensions = decode(&row.dimensions_json)?;
            if !query.include_probe && dimensions.probe {
                continue;
            }
            if dimensions.other {
                partial = true;
                if query.has_dimensions() || category.is_some() || definition.is_some() {
                    continue;
                }
            }
            if let Some(attribution) = &dimensions.attribution
                && !query.matches(attribution, dimensions.tool.as_deref())
            {
                continue;
            }
            let totals: Totals = decode(&row.totals_json)?;
            partial |= row.partial;
            result.push((row, dimensions, totals));
        }
        Ok(ReadRows {
            rows: result,
            partial,
            available_from: config.available_from_ms,
            retained_from: config.rollup_trim_before_ms,
        })
    }

    pub async fn overview(
        &self,
        query: &ResolvedQuery,
        now: i64,
    ) -> Result<MetricsOverview, DbErr> {
        query
            .validate_aggregate_filters()
            .map_err(|reason| DbErr::Custom(reason.into()))?;
        let txn = self
            .db
            .begin_with_config(
                Some(IsolationLevel::RepeatableRead),
                Some(AccessMode::ReadOnly),
            )
            .await?;
        let kind = if query.tool.is_some() {
            "tool"
        } else if query.has_dimensions() {
            "model"
        } else {
            "global"
        };
        let rows = self.read_rows(&txn, query, kind).await?;
        let mut totals = Totals::default();
        for (_, _, value) in &rows.rows {
            totals
                .sum(value)
                .ok_or_else(|| DbErr::Custom("metrics total overflow".into()))?;
        }
        let mut previous_query = query.clone();
        let span = query.to_ms - query.from_ms;
        previous_query.requested_to_ms = query.requested_from_ms;
        previous_query.requested_from_ms = query
            .requested_from_ms
            .saturating_sub(query.requested_to_ms - query.requested_from_ms)
            .max(0);
        previous_query.to_ms = query.from_ms;
        previous_query.from_ms = query.from_ms.saturating_sub(span).max(0);
        let previous_rows = self.read_rows(&txn, &previous_query, kind).await?;
        let mut previous_totals = Totals::default();
        for (_, _, value) in &previous_rows.rows {
            previous_totals
                .sum(value)
                .ok_or_else(|| DbErr::Custom("metrics total overflow".into()))?;
        }
        let result = MetricsOverview {
            coverage: retained_coverage(
                query,
                now,
                rows.available_from,
                rows.retained_from,
                rows.partial,
                self.manager,
            ),
            summary: retained_summary(
                &totals,
                query,
                rows.partial,
                rows.available_from,
                rows.retained_from,
            ),
            previous: Some(retained_summary(
                &previous_totals,
                &previous_query,
                previous_rows.partial,
                previous_rows.available_from,
                previous_rows.retained_from,
            )),
        };
        txn.commit().await?;
        Ok(result)
    }

    pub async fn series(&self, query: &ResolvedQuery, now: i64) -> Result<MetricsSeries, DbErr> {
        query
            .validate_aggregate_filters()
            .map_err(|reason| DbErr::Custom(reason.into()))?;
        let txn = self
            .db
            .begin_with_config(
                Some(IsolationLevel::RepeatableRead),
                Some(AccessMode::ReadOnly),
            )
            .await?;
        let kind = if query.tool.is_some() {
            "tool"
        } else if query.has_dimensions() {
            "model"
        } else {
            "global"
        };
        let rows = self.read_rows(&txn, query, kind).await?;
        let mut points = BTreeMap::<i64, Totals>::new();
        for (row, _, totals) in rows.rows {
            points
                .entry(row.bucket_ms)
                .or_default()
                .sum(&totals)
                .ok_or_else(|| DbErr::Custom("metrics total overflow".into()))?;
        }
        let result = MetricsSeries {
            coverage: retained_coverage(
                query,
                now,
                rows.available_from,
                rows.retained_from,
                rows.partial,
                self.manager,
            ),
            points: points
                .into_iter()
                .map(|(bucket, totals)| MetricSeriesPoint {
                    bucket: timestamp(bucket),
                    summary: retained_summary(
                        &totals,
                        query,
                        rows.partial,
                        rows.available_from,
                        rows.retained_from,
                    ),
                })
                .collect(),
        };
        txn.commit().await?;
        Ok(result)
    }

    pub async fn groups(
        &self,
        query: &ResolvedQuery,
        tools: bool,
        now: i64,
    ) -> Result<MetricsGroups, DbErr> {
        query
            .validate_group_sort(tools)
            .map_err(|reason| DbErr::Custom(reason.into()))?;
        if !tools && query.tool.is_some() {
            return Err(DbErr::Custom(
                "model groups do not support a tool selector".into(),
            ));
        }
        let txn = self
            .db
            .begin_with_config(
                Some(IsolationLevel::RepeatableRead),
                Some(AccessMode::ReadOnly),
            )
            .await?;
        let rows = self
            .read_rows(&txn, query, if tools { "tool" } else { "model" })
            .await?;
        let mut groups = BTreeMap::<String, (SeriesDimensions, Totals)>::new();
        let mut associated = BTreeMap::<
            String,
            BTreeMap<
                (String, String),
                (
                    desk_diagnose_core::model_observability::Attribution,
                    u64,
                    BTreeSet<String>,
                ),
            >,
        >::new();
        let mut other = Totals::default();
        let mut has_other = false;
        for (_, dimensions, value) in rows.rows {
            if dimensions.other {
                other
                    .sum(&value)
                    .ok_or_else(|| DbErr::Custom("metrics total overflow".into()))?;
                has_other = true;
                continue;
            }
            let Some(attribution) = &dimensions.attribution else {
                continue;
            };
            let key = if tools {
                dimensions.tool.clone().unwrap_or_else(|| "unknown".into())
            } else {
                format!("{}:{}", attribution.provider_id, attribution.model_id)
            };
            if attribution.model_identity_known() {
                use desk_diagnose_core::model_observability::aggregate::Count;
                let model = associated
                    .entry(key.clone())
                    .or_default()
                    .entry((
                        attribution.provider_id.clone(),
                        attribution.model_id.clone(),
                    ))
                    .or_insert_with(|| (attribution.clone(), 0, BTreeSet::new()));
                model.2.insert(attribution.configuration_revision.clone());
                model.1 = model
                    .1
                    .checked_add(value.contribution().get(Count::Tools))
                    .ok_or_else(|| DbErr::Custom("metrics total overflow".into()))?;
            }
            groups
                .entry(key)
                .or_insert_with(|| (dimensions.clone(), Totals::default()))
                .1
                .sum(&value)
                .ok_or_else(|| DbErr::Custom("metrics total overflow".into()))?;
        }
        let mut groups: Vec<_> = groups.into_iter().collect();
        groups.sort_by(|a, b| {
            desk_signal_facade::service::model_metrics::compare_groups(
                &a.1.1,
                &b.1.1,
                query.group_sort.unwrap_or_default(),
                query.error,
                tools,
            )
            .then_with(|| a.0.cmp(&b.0))
        });
        let mut selected = Vec::new();
        for (index, (key, (dimensions, totals))) in groups.into_iter().enumerate() {
            if index >= query.limit as usize {
                other
                    .sum(&totals)
                    .ok_or_else(|| DbErr::Custom("metrics total overflow".into()))?;
                has_other = true;
                continue;
            }
            let attribution = dimensions.attribution.as_ref();
            let (provider_id, model_id, model_name) =
                desk_signal_facade::service::model_metrics::group_identity(if tools {
                    None
                } else {
                    attribution
                });
            let all_models: Vec<_> = associated
                .remove(&key)
                .unwrap_or_default()
                .into_values()
                .collect();
            let configurations_limited = rows.partial
                || all_models.len() > 32
                || all_models
                    .iter()
                    .any(|(_, _, revisions)| revisions.len() > 16);
            let configurations = all_models
                .iter()
                .take(32)
                .map(|(model, _, revisions)| MetricConfigurationSample {
                    provider_id: model.provider_id.clone(),
                    model_id: model.model_id.clone(),
                    model_name: model.model_name.clone(),
                    revisions: revisions.iter().take(16).cloned().collect(),
                })
                .collect();
            let mut models: Vec<_> = all_models
                .into_iter()
                .filter(|(_, count, _)| tools && *count > 0)
                .collect();
            models.sort_by(|a, b| {
                b.1.cmp(&a.1).then_with(|| {
                    (&a.0.provider_id, &a.0.model_id).cmp(&(&b.0.provider_id, &b.0.model_id))
                })
            });
            let other_model_count = models.len().saturating_sub(10).to_string();
            let associated_models = models
                .into_iter()
                .take(10)
                .map(|(model, count, _)| MetricAssociatedModel {
                    provider_id: model.provider_id,
                    model_id: model.model_id,
                    model_name: model.model_name,
                    tool_inputs: count.to_string(),
                })
                .collect();
            selected.push(MetricsGroup {
                configurations,
                configurations_limited,
                associated_models,
                other_model_count,
                key,
                provider_id,
                model_id,
                model_name,
                tool: dimensions.tool,
                summary: retained_summary(
                    &totals,
                    query,
                    rows.partial,
                    rows.available_from,
                    rows.retained_from,
                ),
            });
        }
        let result = MetricsGroups {
            coverage: retained_coverage(
                query,
                now,
                rows.available_from,
                rows.retained_from,
                rows.partial,
                self.manager,
            ),
            groups: selected,
            other: has_other.then(|| {
                retained_summary(&other, query, true, rows.available_from, rows.retained_from)
            }),
        };
        txn.commit().await?;
        Ok(result)
    }

    pub async fn runtime_groups(
        &self,
        query: &ResolvedQuery,
        category: Option<desk_diagnose_core::model_observability::RuntimeCategory>,
        definition: Option<desk_diagnose_core::model_observability::runtime::RuntimeDefinition>,
        now: i64,
    ) -> Result<MetricsRuntimeGroups, DbErr> {
        if query.provider_id.is_some()
            || query.model_id.is_some()
            || query.tool.is_some()
            || query.error.is_some()
        {
            return Err(DbErr::Custom(
                "runtime observations do not support model, tool, or request-error filters".into(),
            ));
        }
        query
            .validate_aggregate_filters()
            .map_err(|reason| DbErr::Custom(reason.into()))?;
        let txn = self
            .db
            .begin_with_config(
                Some(IsolationLevel::RepeatableRead),
                Some(AccessMode::ReadOnly),
            )
            .await?;
        let rows = self
            .read_rows_with_runtime(&txn, query, "runtime", category, definition)
            .await?;
        let mut groups = BTreeMap::<String, (SeriesDimensions, Totals)>::new();
        let mut other = Totals::default();
        let mut has_other = false;
        for (_, dims, totals) in rows.rows {
            if dims.other {
                other
                    .sum(&totals)
                    .ok_or_else(|| DbErr::Custom("metrics total overflow".into()))?;
                has_other = true;
                continue;
            }
            let Some(key) = dims.runtime_definition else {
                continue;
            };
            if category.is_some_and(|value| value != key.category())
                || definition.is_some_and(|value| value != key)
            {
                continue;
            }
            let group_key = super::store::encode(&(
                key,
                &dims.runtime_labels,
                dims.attribution.as_ref().map(|a| &a.contract_revision),
            ))?;
            groups
                .entry(group_key)
                .or_insert_with(|| (dims.clone(), Totals::default()))
                .1
                .sum(&totals)
                .ok_or_else(|| DbErr::Custom("metrics total overflow".into()))?;
        }
        let mut ordered: Vec<_> = groups.into_iter().collect();
        ordered.sort_by(|a, b| {
            b.1.1
                .contribution()
                .get(desk_diagnose_core::model_observability::aggregate::Count::RuntimeEvents)
                .cmp(
                    &a.1.1.contribution().get(
                        desk_diagnose_core::model_observability::aggregate::Count::RuntimeEvents,
                    ),
                )
                .then_with(|| a.0.cmp(&b.0))
        });
        let mut selected = Vec::new();
        for (index, (_, (dims, totals))) in ordered.into_iter().enumerate() {
            if index >= query.limit as usize {
                other
                    .sum(&totals)
                    .ok_or_else(|| DbErr::Custom("metrics total overflow".into()))?;
                has_other = true;
                continue;
            }
            let Some(definition) = dims.runtime_definition else {
                continue;
            };
            selected.push(MetricsRuntimeGroup {
                definition: desk_signal_facade::service::model_metrics::tag(&definition),
                category: desk_signal_facade::service::model_metrics::tag(&definition.category()),
                labels: dims
                    .runtime_labels
                    .as_ref()
                    .map(desk_signal_facade::service::model_metrics::runtime_labels)
                    .unwrap_or_default(),
                contract_revision: dims
                    .attribution
                    .as_ref()
                    .map(|a| a.contract_revision.clone()),
                summary: retained_summary(
                    &totals,
                    query,
                    rows.partial,
                    rows.available_from,
                    rows.retained_from,
                ),
            });
        }
        let result = MetricsRuntimeGroups {
            coverage: retained_coverage(
                query,
                now,
                rows.available_from,
                rows.retained_from,
                rows.partial,
                self.manager,
            ),
            groups: selected,
            other: has_other.then(|| {
                retained_summary(&other, query, true, rows.available_from, rows.retained_from)
            }),
        };
        txn.commit().await?;
        Ok(result)
    }

    pub async fn unassociated(
        &self,
        query: &desk_signal_facade::service::model_metrics::unassociated::ResolvedUnassociatedQuery,
        now: i64,
    ) -> Result<MetricsUnassociated, DbErr> {
        use desk_signal_facade::service::model_metrics::unassociated::UnassociatedSnapshot;
        let boundary = query
            .boundary(self.manager)
            .map_err(|reason| DbErr::Custom(reason.into()))?;
        let txn = self
            .db
            .begin_with_config(
                Some(IsolationLevel::RepeatableRead),
                Some(AccessMode::ReadOnly),
            )
            .await?;
        let config = settings::Entity::find_by_id(1)
            .one(&txn)
            .await?
            .ok_or_else(|| DbErr::Custom("metrics unavailable".into()))?;
        let retention = txn.config_read().await.model_metrics.clone();
        let cutoff = now.saturating_sub(i64::from(retention.detail_days) * 86_400_000);
        let mut selection = detail::Entity::find()
            .filter(detail::Column::Kind.eq("unassociated"))
            .filter(
                detail::Column::ReceivedAtMs
                    .gte(query.from_ms.max(config.available_from_ms).max(cutoff)),
            )
            .filter(detail::Column::ReceivedAtMs.lt(query.to_ms));
        if let Some((time, id)) = boundary {
            selection = selection.filter(
                Condition::any()
                    .add(detail::Column::ReceivedAtMs.lt(time))
                    .add(
                        Condition::all()
                            .add(detail::Column::ReceivedAtMs.eq(time))
                            .add(detail::Column::ObjectId.lt(id)),
                    ),
            );
        }
        let rows = selection
            .order_by_desc(detail::Column::ReceivedAtMs)
            .order_by_desc(detail::Column::ObjectId)
            .limit(u64::from(query.limit) + 1)
            .all(&txn)
            .await?;
        let more = rows.len() > query.limit as usize;
        let rows: Vec<_> = rows.into_iter().take(query.limit as usize).collect();
        let cursor = if more {
            rows.last()
                .map(|row| query.cursor(self.manager, row.received_at_ms, &row.object_id))
        } else {
            None
        };
        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            let snapshot: UnassociatedSnapshot = decode(&row.snapshot_json)?;
            if snapshot.id != row.object_id
                || snapshot.received_at_ms != row.received_at_ms
                || row.started_at_ms.is_some()
            {
                return Err(DbErr::Custom(
                    "metrics association projection unavailable".into(),
                ));
            }
            let mut record = snapshot.record(now);
            if snapshot.state == AssociationGapState::Waiting
                && snapshot.received_at_ms
                    < now.saturating_sub(i64::from(retention.mutable_days) * 86_400_000)
            {
                record.state = AssociationGapState::OutsideWindow;
            }
            records.push(record);
        }
        let mut coverage = query.coverage(
            now,
            config.available_from_ms,
            config.coverage_partial || !records.is_empty(),
            self.manager,
        );
        if config.trimmed_details > 0
            && query.from_ms < cutoff
            && query.to_ms > config.available_from_ms
        {
            coverage.trimmed_before = Some(timestamp(cutoff));
            coverage.effective_from = timestamp(
                query
                    .from_ms
                    .max(config.available_from_ms)
                    .max(cutoff)
                    .min(query.to_ms),
            );
            coverage.sample_status = if query.to_ms <= cutoff {
                SampleStatus::Unavailable
            } else {
                SampleStatus::Partial
            };
        }
        let result = MetricsUnassociated {
            coverage,
            records,
            next_cursor: cursor,
        };
        txn.commit().await?;
        Ok(result)
    }

    fn call_selection(
        &self,
        query: &ResolvedQuery,
        now: i64,
    ) -> Result<(sea_orm::Select<detail::Entity>, i64), DbErr> {
        query
            .validate_call_filters()
            .map_err(|reason| DbErr::Custom(reason.into()))?;
        let boundary = query
            .call_boundary(self.manager, now)
            .map_err(|reason| DbErr::Custom(reason.into()))?;
        let received_before = boundary.as_ref().map_or(now, |(_, _, time)| *time);
        let mut selection = detail::Entity::find()
            .filter(detail::Column::ReceivedAtMs.lte(received_before))
            .filter(detail::Column::StartedAtMs.lte(received_before))
            .filter(detail::Column::StartedAtMs.gte(query.from_ms))
            .filter(detail::Column::StartedAtMs.lt(query.to_ms))
            .filter(
                detail::Column::Kind.eq(desk_signal_facade::service::model_metrics::tag(
                    &query.record_kind(),
                )),
            );
        if let Some(value) = &query.provider_id {
            selection = selection.filter(detail::Column::ProviderId.eq(value));
        }
        if let Some(value) = &query.model_id {
            selection = selection.filter(detail::Column::ModelId.eq(value));
        }
        if let Some(value) = query.surface {
            selection = selection.filter(
                detail::Column::Surface.eq(desk_signal_facade::service::model_metrics::tag(&value)),
            );
        }
        if let Some(value) = query.purpose {
            selection = selection.filter(
                detail::Column::Purpose.eq(desk_signal_facade::service::model_metrics::tag(&value)),
            );
        }
        if let Some(value) = query.origin {
            selection = selection.filter(
                detail::Column::Origin.eq(desk_signal_facade::service::model_metrics::tag(&value)),
            );
        }
        if !query.include_probe {
            selection = selection.filter(detail::Column::Purpose.ne("probe"));
        }
        if let Some(value) = &query.tool {
            selection = selection.filter(detail::Column::Tool.eq(value));
        }
        if let Some(value) = &query.contract_revision {
            selection = selection.filter(detail::Column::ContractRevision.eq(value));
        }
        if let Some(value) = query.error {
            use desk_diagnose_core::model_observability::ErrorSelector;
            selection = selection.filter(match value {
                ErrorSelector::Request(_) => detail::Column::Outcome.eq(value.category()),
                ErrorSelector::Output(_) => detail::Column::Output.eq(value.category()),
                ErrorSelector::Input(_) => detail::Column::Issue.eq(value.category()),
            });
        }
        if let Some(outcome) = query.calls.outcome.as_deref() {
            selection = selection.filter(if outcome == "request_error" {
                detail::Column::Outcome.is_in([
                    "http_error",
                    "provider_error",
                    "transport_error",
                    "timeout",
                    "stream_error",
                ])
            } else {
                detail::Column::Outcome.eq(outcome)
            });
        }
        if query.calls.min_duration_ms.is_some() || query.calls.latency.is_some() {
            let column = if query.calls.latency == Some(MetricLatency::FirstContent) {
                detail::Column::FirstContentMs
            } else {
                detail::Column::DurationMs
            };
            selection =
                selection.filter(column.gte(i64::from(query.calls.min_duration_ms.unwrap_or(0))));
        }
        if let Some(permission) = query.calls.permission {
            selection = selection.filter(
                detail::Column::Permission
                    .eq(desk_signal_facade::service::model_metrics::tag(&permission)),
            );
        }
        if let Some(dispatched) = query.calls.dispatched {
            selection = selection.filter(detail::Column::Dispatched.eq(dispatched));
        }
        if let Some((time, id, _)) = boundary {
            selection = selection.filter(
                Condition::any()
                    .add(detail::Column::StartedAtMs.lt(time))
                    .add(
                        Condition::all()
                            .add(detail::Column::StartedAtMs.eq(time))
                            .add(detail::Column::ObjectId.lt(id)),
                    ),
            );
        }
        Ok((
            selection
                .order_by_desc(detail::Column::StartedAtMs)
                .order_by_desc(detail::Column::ObjectId)
                .limit(u64::from(query.limit) + 1),
            received_before,
        ))
    }

    /// Uses exactly the production page selection, including all selected columns.
    pub fn call_page_statement(
        &self,
        query: &ResolvedQuery,
        now: i64,
    ) -> Result<sea_orm::Statement, DbErr> {
        use sea_orm::QueryTrait;
        Ok(self
            .call_selection(query, now)?
            .0
            .build(self.db.get_database_backend()))
    }

    pub async fn calls(&self, query: &ResolvedQuery, now: i64) -> Result<MetricsCalls, DbErr> {
        query
            .validate_call_filters()
            .map_err(|reason| DbErr::Custom(reason.into()))?;
        let txn = self
            .db
            .begin_with_config(
                Some(IsolationLevel::RepeatableRead),
                Some(AccessMode::ReadOnly),
            )
            .await?;
        let config = settings::Entity::find_by_id(1)
            .one(&txn)
            .await?
            .ok_or_else(|| DbErr::Custom("metrics unavailable".into()))?;
        let (selection, received_before) = self.call_selection(query, now)?;
        let rows = selection.all(&txn).await?;
        let more = rows.len() > query.limit as usize;
        let selected: Vec<_> = rows.into_iter().take(query.limit as usize).collect();
        let cursor = if more {
            selected
                .last()
                .map(|row| {
                    query
                        .call_cursor(
                            self.manager,
                            received_before,
                            row.started_at_ms.unwrap_or_default(),
                            &row.object_id,
                        )
                        .map_err(|reason| DbErr::Custom(reason.into()))
                })
                .transpose()?
        } else {
            None
        };
        let mut records: Vec<_> = selected
            .into_iter()
            .map(|row| {
                decode::<ObservationEvent>(&row.snapshot_json).map(|event| {
                    let mut value = record(&event, false);
                    value.correction_group_unavailable = row.correction_group_observed;
                    value
                })
            })
            .collect::<Result<_, _>>()?;
        self.enrich_tool_counts(&txn, &mut records, config.coverage_partial)
            .await?;
        self.enrich_correction_groups(&txn, &mut records).await?;
        let result = MetricsCalls {
            received_before: timestamp(received_before),
            coverage: query.coverage(
                now,
                config.available_from_ms,
                config.coverage_partial,
                self.manager,
            ),
            records,
            next_cursor: cursor,
        };
        txn.commit().await?;
        Ok(result)
    }

    async fn enrich_tool_counts(
        &self,
        txn: &DatabaseTransaction,
        records: &mut [ObservationRecord],
        partial: bool,
    ) -> Result<(), DbErr> {
        use desk_diagnose_core::model_observability::aggregate::{Contribution, Count};
        let ids: Vec<_> = records
            .iter()
            .filter(|row| row.kind == "call")
            .map(|row| row.id.clone())
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        let children = compact::Entity::find()
            .select_only()
            .column(compact::Column::CallId)
            .column(compact::Column::ContributionJson)
            .filter(compact::Column::CallId.is_in(ids))
            .filter(compact::Column::Kind.eq("tool"))
            .order_by_asc(compact::Column::CallId)
            .order_by_asc(compact::Column::ObjectId)
            .limit(MAX_QUERY_ROWS + 1)
            .into_tuple::<(Option<String>, String)>()
            .all(txn)
            .await?;
        let capped = children.len() > MAX_QUERY_ROWS as usize;
        let mut counts = BTreeMap::<String, (u64, u64, u64, bool)>::new();
        for (id, contribution) in children.into_iter().take(MAX_QUERY_ROWS as usize) {
            let Some(id) = id else {
                continue;
            };
            let count = counts.entry(id).or_default();
            match decode::<Contribution>(&contribution) {
                Ok(value)
                    if value.get(Count::Tools) == 1
                        && value
                            .get(Count::InputAccepted)
                            .checked_add(value.get(Count::InputRejected))
                            .is_some_and(|known| known <= 1) =>
                {
                    count.0 += 1;
                    count.1 += value.get(Count::InputRejected);
                    count.2 += value.get(Count::InputAccepted) + value.get(Count::InputRejected);
                }
                _ => {
                    count.3 = true;
                }
            }
        }
        for record in records.iter_mut().filter(|row| row.kind == "call") {
            let (tools, rejected, concluded, corrupt) =
                counts.remove(&record.id).unwrap_or_default();
            let expected = record
                .generated_tool_count
                .as_deref()
                .and_then(|value| value.parse::<u64>().ok());
            let complete =
                !partial && !capped && !corrupt && expected == Some(tools) && concluded == tools;
            if tools > 0 || complete {
                record.tool_count = Some(tools.to_string());
                record.input_rejected_count = Some(rejected.to_string());
            }
            record.tool_counts_status = if complete {
                SampleStatus::Complete
            } else if tools > 0 {
                SampleStatus::Partial
            } else {
                SampleStatus::Unknown
            };
        }
        Ok(())
    }

    pub async fn call_detail(&self, id: &str) -> Result<Option<MetricsCallDetail>, DbErr> {
        if id.is_empty() || id.len() > 192 {
            return Err(DbErr::Custom("invalid observation identity".into()));
        }
        let txn = self
            .db
            .begin_with_config(
                Some(IsolationLevel::RepeatableRead),
                Some(AccessMode::ReadOnly),
            )
            .await?;
        let row = detail::Entity::find_by_id(id).one(&txn).await?;
        let Some(row) = row else {
            txn.commit().await?;
            return Ok(None);
        };
        if row.kind == "unassociated" {
            txn.commit().await?;
            return Ok(None);
        }
        let event: ObservationEvent = decode(&row.snapshot_json)?;
        let root = event.call_id.as_deref().unwrap_or(&event.object_id);
        let children = detail::Entity::find()
            .filter(detail::Column::CallId.eq(root))
            .filter(detail::Column::ObjectId.ne(id))
            .filter(detail::Column::Kind.ne("unassociated"))
            .order_by_asc(detail::Column::StartedAtMs)
            .order_by_asc(detail::Column::ObjectId)
            .limit(201)
            .all(&txn)
            .await?;
        let truncated = children.len() > 200;
        let mut related: Vec<_> = children
            .into_iter()
            .take(200)
            .map(|row| {
                decode::<ObservationEvent>(&row.snapshot_json).map(|event| {
                    let mut value = record(&event, false);
                    value.correction_group_unavailable = row.correction_group_observed;
                    value
                })
            })
            .collect::<Result<_, _>>()?;
        let mut call = record(&event, false);
        call.correction_group_unavailable = row.correction_group_observed;
        let partial = settings::Entity::find_by_id(1)
            .one(&txn)
            .await?
            .is_none_or(|config| config.coverage_partial);
        self.enrich_tool_counts(&txn, std::slice::from_mut(&mut call), partial)
            .await?;
        self.enrich_tool_counts(&txn, &mut related, partial).await?;
        self.enrich_correction_groups(&txn, std::slice::from_mut(&mut call))
            .await?;
        self.enrich_correction_groups(&txn, &mut related).await?;
        let result = MetricsCallDetail {
            call,
            related,
            related_truncated: truncated,
        };
        txn.commit().await?;
        Ok(Some(result))
    }

    pub async fn status(&self, now: i64) -> Result<MetricsStatus, DbErr> {
        let txn = self
            .db
            .begin_with_config(
                Some(IsolationLevel::RepeatableRead),
                Some(AccessMode::ReadOnly),
            )
            .await?;
        let row = settings::Entity::find_by_id(1)
            .one(&txn)
            .await?
            .ok_or_else(|| DbErr::Custom("metrics unavailable".into()))?;
        let config = txn.config_read().await.model_metrics.clone();
        let nodes = health::Entity::find()
            .filter(health::Column::ReportedAtMs.gte(now.saturating_sub(7 * 86_400_000)))
            .limit(1_001)
            .all(&txn)
            .await?;
        let backlog = event::Entity::find()
            .filter(event::Column::Applied.eq(false))
            .count(&txn)
            .await?;
        let oldest = event::Entity::find()
            .filter(event::Column::Applied.eq(false))
            .order_by_asc(event::Column::Id)
            .one(&txn)
            .await?;
        let counts = nodes
            .iter()
            .try_fold((0u64, 0u64), |(dropped, discarded), node| {
                Some((
                    dropped.checked_add(node.dropped_events.parse::<u64>().ok()?)?,
                    discarded.checked_add(node.discarded_events.parse::<u64>().ok()?)?,
                ))
            });
        let unknown = nodes.is_empty()
            || nodes.len() > 1_000
            || counts.is_none()
            || nodes
                .iter()
                .any(|node| node.reported_at_ms < now.saturating_sub(60_000));
        let (dropped, discarded) = counts.unwrap_or_default();
        let effective = !unknown
            && nodes
                .iter()
                .all(|node| node.config_revision.as_deref() == Some(&config.revision));
        let unassociated = detail::Entity::find()
            .filter(detail::Column::Kind.eq("unassociated"))
            .count(&txn)
            .await?;
        let degraded = unassociated > 0
            || row.coverage_partial
            || dropped > 0
            || discarded > 0
            || nodes
                .iter()
                .any(|node| node.state == "degraded" || node.state == "unavailable");
        let state = if unknown {
            ComponentState::CacheExpired
        } else if !config.enabled {
            ComponentState::Disabled
        } else if degraded {
            ComponentState::Degraded
        } else {
            ComponentState::Ready
        };
        let mut gaps = Vec::new();
        if row.rollup_trim_before_ms > 0 {
            gaps.push(CoverageGap {
                from: timestamp(row.available_from_ms),
                to: Some(timestamp(row.rollup_trim_before_ms)),
                reason: "rollup_capacity_trim".into(),
            });
        }
        if row.trimmed_details > 0 {
            gaps.push(CoverageGap {
                from: timestamp(row.available_from_ms),
                to: None,
                reason: "details_trimmed".into(),
            });
        }
        if row.coverage_partial {
            gaps.push(CoverageGap {
                from: timestamp(row.available_from_ms),
                to: None,
                reason: "partial_capture_or_storage_trim".into(),
            });
        }
        if unassociated > 0 {
            gaps.push(CoverageGap {
                from: timestamp(row.available_from_ms),
                to: None,
                reason: "unassociated_facts".into(),
            });
        }
        if unknown {
            gaps.push(CoverageGap {
                from: timestamp(now.saturating_sub(60_000)),
                to: None,
                reason: "node_health_unknown".into(),
            });
        }
        let result = MetricsStatus {
            state,
            enabled: Some(config.enabled),
            schema_version: row.schema_version as u32,
            definition_version: desk_diagnose_core::model_observability::DEFINITION_VERSION,
            available_from: Some(timestamp(row.available_from_ms)),
            settings_revision: Some(config.revision.clone()),
            settings_effective: effective,
            last_persisted: nodes
                .iter()
                .filter_map(|node| node.persisted_at_ms)
                .max()
                .map(timestamp),
            last_aggregated: nodes
                .iter()
                .filter_map(|node| node.aggregated_at_ms)
                .min()
                .map(timestamp),
            as_of: timestamp(now),
            backlog: Some(backlog.to_string()),
            oldest_pending: oldest.map(|event| timestamp(event.received_at_ms)),
            dropped_events: (!unknown).then(|| dropped.to_string()),
            discarded_events: (!unknown).then(|| discarded.to_string()),
            unassociated_records: Some(unassociated.to_string()),
            instrumented_surfaces: vec![],
            unsupported_surfaces: vec![],
            gaps,
            storage: Some(MetricsStorage {
                charged_bytes: row.storage_used_bytes.to_string(),
                budget_bytes: config.storage_budget_bytes.clone(),
                reserved_bytes:
                    desk_diagnose_core::model_observability::capacity::RESOURCE_RESERVE_BYTES
                        .to_string(),
                physical_allocated_bytes: row
                    .physical_allocated_bytes
                    .filter(|value| *value >= 0)
                    .map(|value| value.to_string()),
                physical_sampled_at: row.physical_sampled_at_ms.map(timestamp),
                cleanup_active: row.storage_cleanup_active,
                rows: vec![
                    MetricsStorageRows {
                        kind: MetricsStorageKind::Event,
                        rows: row.event_rows.to_string(),
                        budget: config.event_row_budget.to_string(),
                        cleanup_active: row.event_cleanup_active,
                    },
                    MetricsStorageRows {
                        kind: MetricsStorageKind::Compact,
                        rows: row.compact_rows.to_string(),
                        budget: config.compact_row_budget.to_string(),
                        cleanup_active: row.compact_cleanup_active,
                    },
                    MetricsStorageRows {
                        kind: MetricsStorageKind::Detail,
                        rows: row.detail_rows.to_string(),
                        budget: config.detail_row_budget.to_string(),
                        cleanup_active: row.detail_cleanup_active,
                    },
                    MetricsStorageRows {
                        kind: MetricsStorageKind::Rollup,
                        rows: row.rollup_rows.to_string(),
                        budget: config.rollup_row_budget.to_string(),
                        cleanup_active: row.rollup_cleanup_active,
                    },
                ],
                trimmed_details: row.trimmed_details.to_string(),
                dropped_pending_events: row.dropped_pending_events.to_string(),
                frozen_before: (row.frozen_before_ms > 0).then(|| timestamp(row.frozen_before_ms)),
                rollup_trim_before: (row.rollup_trim_before_ms > 0)
                    .then(|| timestamp(row.rollup_trim_before_ms)),
            }),
            retention: Some(config),
            reason: unknown.then(|| "node_health_unknown".into()),
        };
        txn.commit().await?;
        Ok(result)
    }
}

impl Store {
    pub async fn model_usage(
        &self,
        query: &ResolvedQuery,
        model_name: Option<&str>,
        day: bool,
        _now: i64,
    ) -> Result<desk_signal_facade::model::model_usage::ModelUsageResult, DbErr> {
        use desk_diagnose_core::model_observability::aggregate::Count;
        use desk_signal_facade::model::model_usage::*;
        let txn = self
            .db
            .begin_with_config(
                Some(IsolationLevel::RepeatableRead),
                Some(AccessMode::ReadOnly),
            )
            .await?;
        let rows = self.read_rows(&txn, query, "usage_model").await?;
        let mut partial = rows.partial;
        let mut groups = BTreeMap::<(i64, String, String, String, String), Totals>::new();
        for (row, dims, totals) in rows.rows {
            if dims.other {
                partial = true;
                continue;
            }
            let Some(attr) = dims.attribution else {
                continue;
            };
            if model_name.is_some_and(|name| name != attr.model_name) {
                continue;
            }
            let bucket = if day {
                row.bucket_ms.div_euclid(86_400_000) * 86_400_000
            } else {
                row.bucket_ms
            };
            let key = (
                bucket,
                attr.provider_id,
                attr.model_id,
                attr.model_name,
                desk_signal_facade::service::model_metrics::tag(&attr.purpose),
            );
            groups
                .entry(key)
                .or_default()
                .sum(&totals)
                .ok_or_else(|| DbErr::Custom("metrics total overflow".into()))?;
        }
        let items = groups
            .into_iter()
            .map(
                |((bucket, provider_id, model_id, model_name, purpose), totals)| {
                    let value = totals.contribution();
                    ModelUsageItem {
                        provider_id,
                        model_id,
                        model_name,
                        purpose,
                        hour_bucket: timestamp(bucket),
                        subject_tier: None,
                        subject_user_id: None,
                        subject_org_id: None,
                        input_tokens: value.get(Count::InputTokens).to_string(),
                        output_tokens: value.get(Count::OutputTokens).to_string(),
                        cache_read_tokens: value.get(Count::CacheReadTokens).to_string(),
                        cache_write_tokens: value.get(Count::CacheWriteTokens).to_string(),
                        request_count: value.get(Count::MeteredCalls).to_string(),
                    }
                },
            )
            .collect();
        let result = ModelUsageResult {
            items,
            range: ModelUsageRange {
                from: timestamp(query.from_ms),
                to: timestamp(query.to_ms),
                granularity: if day { "day" } else { "hour" }.into(),
            },
            usage_source: "observations".into(),
            partial: partial || query.from_ms < rows.available_from,
            available_from: Some(timestamp(rows.available_from)),
        };
        txn.commit().await?;
        Ok(result)
    }
}
