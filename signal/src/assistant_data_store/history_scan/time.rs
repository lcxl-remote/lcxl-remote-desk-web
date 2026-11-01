use super::super::signed;
use super::*;
use desk_agent_protocol::ai_assistant::history::{HistoryOrder, HistorySort};
use desk_diagnose_core::ai_assistant::{history::validate_search_scope, retrieval::query_groups};
use sea_orm::ExprTrait;
use sea_orm::sea_query::{Condition, Order};

pub(crate) struct TimePage {
    pub items: Vec<HistoryScanItem>,
    pub continuation: Option<PageAnchor>,
    pub has_more: bool,
    pub effective_queries: Vec<Vec<String>>,
    pub reasons: Vec<String>,
}

pub(crate) async fn enumerate_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
    query: &FrozenRetrievalQuery,
    page: Option<&PageAnchor>,
    owner_devices: &BTreeSet<String>,
) -> Result<TimePage, AssistantStoreError> {
    scan_on(
        db,
        context,
        query,
        page,
        owner_devices,
        Duration::from_millis(context.platform.limits.retrieval.history_time_scan_ms),
    )
    .await
}

pub(crate) async fn scan_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
    query: &FrozenRetrievalQuery,
    page: Option<&PageAnchor>,
    owner_devices: &BTreeSet<String>,
    duration: Duration,
) -> Result<TimePage, AssistantStoreError> {
    let mut work = ScanWork::new(context, duration);
    let (devices, changed) = devices_on(db, context, query, page, owner_devices).await?;
    let FrozenRetrievalRequest::Conversations { request } = &query.request else {
        return Err(AssistantStoreError::Inaccessible);
    };
    validate_search_scope(context.scope, request, context.now_unix_ms)
        .map_err(AssistantStoreError::Invalid)?;
    if request.sort != HistorySort::Time {
        return Err(AssistantStoreError::Invalid(
            "time enumeration requires its original temporal request",
        ));
    }
    let mut groups = query_groups(&request.queries).map_err(AssistantStoreError::Invalid)?;
    groups.sort();
    let terms = groups.iter().flatten().cloned().collect::<BTreeSet<_>>();
    if !request.queries.is_empty() && terms.is_empty() {
        return Err(AssistantStoreError::Invalid(
            "keyword query has no searchable terms",
        ));
    }
    let mut position = match page {
        None | Some(PageAnchor::Start) => None,
        Some(PageAnchor::Time {
            time_unix_ms,
            source_id,
        }) => Some((*time_unix_ms, source_id.clone())),
        _ => return Err(AssistantStoreError::Inaccessible),
    };
    if changed {
        work.reasons.insert("scope_changed");
    }
    if request.time_field == HistoryTimeField::NativeActionAt {
        work.reasons.insert("unknown_native_times_excluded");
    }
    let mut items = Vec::new();
    let mut more = false;
    'pages: while !devices.is_empty() {
        if !work.may_read() {
            more = true;
            break;
        }
        let mut select = history_query::base(context, &devices)?;
        history_query::apply_search(&mut select, context, request)?;
        history_query::apply_terms(&mut select, context, &terms)?;
        columns(&mut select, request.time_field);
        if request.time_field == HistoryTimeField::NativeActionAt {
            select.cond_where(history_query::native_time_available());
        }
        let time = history_query::time_expression(request.time_field);
        if let Some((last_time, last_source)) = &position {
            let relation = if request.order == HistoryOrder::Asc {
                time.clone().gt(signed(*last_time)?)
            } else {
                time.clone().lt(signed(*last_time)?)
            };
            select.cond_where(
                Condition::any().add(relation).add(
                    Condition::all()
                        .add(time.clone().eq(signed(*last_time)?))
                        .add(
                            Expr::col((history::Entity, history::Column::SourceId))
                                .gt(last_source.clone()),
                        ),
                ),
            );
        }
        let limit = std::cmp::Ord::min(
            u64::from(context.platform.limits.processing.history_scan_page_size),
            (work.maximum - work.visited) as u64,
        );
        select
            .order_by_expr(
                time,
                if request.order == HistoryOrder::Asc {
                    Order::Asc
                } else {
                    Order::Desc
                },
            )
            .order_by((history::Entity, history::Column::SourceId), Order::Asc)
            .limit(limit);
        let rows = ThinHistory::find_by_statement(db.get_database_backend().build(&select))
            .all(db)
            .await?;
        let finished = rows.len() < limit as usize;
        for row in rows {
            if !work.may_read() {
                more = true;
                break 'pages;
            }
            work.visited += 1;
            if row.already_seen(query, &items) {
                position = Some((unsigned(row.time_key)?, row.source_id));
                continue;
            }
            let source = readable_on(db, context, &row, &mut work).await?;
            let raw_position = (unsigned(row.time_key)?, row.source_id.clone());
            if let Some(item) = source
                && !query.was_seen(&item.source)
            {
                if items.len() >= request.limit as usize {
                    more = true;
                    break 'pages;
                }
                if request.time_field == HistoryTimeField::NativeActionAt
                    && row.native_action_at_ms.is_none()
                {
                    work.reasons.insert("native_action_time_interval");
                }
                items.push(item);
            }
            position = Some(raw_position);
        }
        if finished {
            break;
        }
    }
    keyword_search::validate_context_on(db, context).await?;
    let continuation = more.then(|| {
        position.map_or(PageAnchor::Start, |(time_unix_ms, source_id)| {
            PageAnchor::Time {
                time_unix_ms,
                source_id,
            }
        })
    });
    Ok(TimePage {
        items,
        continuation,
        has_more: more,
        effective_queries: groups,
        reasons: work.reasons(),
    })
}
