use super::super::signed;
use super::*;
use desk_agent_protocol::ai_assistant::history::ConversationReadView;
use sea_orm::ExprTrait;
use sea_orm::sea_query::{Condition, Order};

pub(crate) struct WindowPage {
    pub items: Vec<HistoryScanItem>,
    pub previous: Option<PageAnchor>,
    pub next: Option<PageAnchor>,
    pub has_more: bool,
    pub reasons: Vec<String>,
    pub anchor_position: WindowPosition,
}
struct DirectionPage {
    items: Vec<HistoryScanItem>,
    through: Option<WindowPosition>,
    more: bool,
}
struct Direction<'a> {
    conversation: &'a str,
    boundary: Option<&'a WindowPosition>,
    forward: bool,
    limit: usize,
    pending: &'a [HistoryScanItem],
}

pub(crate) async fn read_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
    query: &FrozenRetrievalQuery,
    page: Option<&PageAnchor>,
    owner_devices: &BTreeSet<String>,
) -> Result<WindowPage, AssistantStoreError> {
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
) -> Result<WindowPage, AssistantStoreError> {
    let mut work = ScanWork::new(context, duration);
    let (devices, changed) = devices_on(db, context, query, page, owner_devices).await?;
    let FrozenRetrievalRequest::Window {
        request,
        anchor,
        conversation_id,
    } = &query.request
    else {
        return Err(AssistantStoreError::Inaccessible);
    };
    if !work.may_read() {
        return Err(AssistantStoreError::Invalid(
            "history scan exhausted before original anchor validation",
        ));
    }
    work.visited += 1;
    let original_source = keyword_search::readable_history_on(
        db,
        context,
        &anchor.source_id,
        anchor.revision,
        &anchor.content_sha256,
    )
    .await?;
    if original_source.row.conversation_id != *conversation_id
        || !devices.contains(&original_source.row.device_ref)
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let original = ThinHistory {
        source_id: original_source.row.source_id.clone(),
        revision: original_source.row.revision,
        content_sha256: original_source.row.content_sha256.clone(),
        conversation_id: original_source.row.conversation_id.clone(),
        admitted_generation: original_source.row.admitted_generation,
        fragment_index: original_source.row.fragment_index,
        recorded_at_ms: original_source.row.recorded_at_ms,
        native_action_at_ms: original_source.row.native_action_at_ms,
        native_action_from_ms: original_source.row.native_action_from_ms,
        native_action_until_ms: original_source.row.native_action_until_ms,
        time_key: original_source.row.recorded_at_ms,
    }
    .item(context.corpus_generation)?;
    drop(original_source);
    if changed {
        work.reasons.insert("scope_changed");
    }
    let mut before = original.position.clone();
    let mut after = original.position.clone();
    for issued in &query.snapshot.continuations {
        if let Some((position, forward)) = position(&issued.anchor) {
            if forward {
                after = after.max(position);
            } else {
                before = before.min(position);
            }
        }
    }
    let mut items = Vec::new();
    let (older, newer);
    match page {
        None | Some(PageAnchor::Start) if request.view == ConversationReadView::Latest => {
            let selected = direction_on(
                db,
                context,
                query,
                &devices,
                Direction {
                    conversation: conversation_id,
                    boundary: None,
                    forward: false,
                    limit: request.limit as usize,
                    pending: &items,
                },
                &mut work,
            )
            .await?;
            if let Some(position) = selected.through {
                before = position;
            }
            if let Some(last) = selected.items.first() {
                after = last.position.clone();
            }
            older = selected.more;
            newer = false;
            items = selected.items;
        }
        None | Some(PageAnchor::Start) => {
            if !query.was_seen(&original.source) {
                items.push(original.clone());
            }
            let before_limit = (request.limit as usize - items.len()) / 2;
            let selected = direction_on(
                db,
                context,
                query,
                &devices,
                Direction {
                    conversation: conversation_id,
                    boundary: Some(&before),
                    forward: false,
                    limit: before_limit,
                    pending: &items,
                },
                &mut work,
            )
            .await?;
            if let Some(position) = selected.through {
                before = before.min(position);
            }
            older = selected.more;
            items.extend(selected.items);
            let selected = direction_on(
                db,
                context,
                query,
                &devices,
                Direction {
                    conversation: conversation_id,
                    boundary: Some(&after),
                    forward: true,
                    limit: request.limit as usize - items.len(),
                    pending: &items,
                },
                &mut work,
            )
            .await?;
            if let Some(position) = selected.through {
                after = after.max(position);
            }
            newer = selected.more;
            items.extend(selected.items);
        }
        Some(anchor @ PageAnchor::Window { .. }) => {
            let (boundary, forward) = position(anchor).ok_or(AssistantStoreError::Inaccessible)?;
            let selected = direction_on(
                db,
                context,
                query,
                &devices,
                Direction {
                    conversation: conversation_id,
                    boundary: Some(&boundary),
                    forward,
                    limit: request.limit as usize,
                    pending: &items,
                },
                &mut work,
            )
            .await?;
            if forward {
                if let Some(position) = selected.through {
                    after = after.max(position);
                }
                newer = selected.more;
                items = selected.items;
                older = direction_on(
                    db,
                    context,
                    query,
                    &devices,
                    Direction {
                        conversation: conversation_id,
                        boundary: Some(&before),
                        forward: false,
                        limit: 0,
                        pending: &items,
                    },
                    &mut work,
                )
                .await?
                .more;
            } else {
                if let Some(position) = selected.through {
                    before = before.min(position);
                }
                older = selected.more;
                items = selected.items;
                newer = direction_on(
                    db,
                    context,
                    query,
                    &devices,
                    Direction {
                        conversation: conversation_id,
                        boundary: Some(&after),
                        forward: true,
                        limit: 0,
                        pending: &items,
                    },
                    &mut work,
                )
                .await?
                .more;
            }
        }
        _ => return Err(AssistantStoreError::Inaccessible),
    }
    items.sort_by(|a, b| a.position.cmp(&b.position));
    keyword_search::validate_context_on(db, context).await?;
    Ok(WindowPage {
        items,
        previous: older.then(|| before.anchor(false)),
        next: newer.then(|| after.anchor(true)),
        has_more: older || newer,
        reasons: work.reasons(),
        anchor_position: original.position,
    })
}

async fn direction_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
    query: &FrozenRetrievalQuery,
    devices: &[String],
    direction: Direction<'_>,
    work: &mut ScanWork,
) -> Result<DirectionPage, AssistantStoreError> {
    let Direction {
        conversation,
        boundary,
        forward,
        limit,
        pending,
    } = direction;
    let mut through = boundary.cloned();
    let mut items = Vec::new();
    let mut more = false;
    'pages: loop {
        if !work.may_read() {
            more = true;
            break;
        }
        let mut select = history_query::base(context, devices)?;
        select.and_where(
            Expr::col((history::Entity, history::Column::ConversationId)).eq(conversation),
        );
        columns(&mut select, HistoryTimeField::RecordedAt);
        if let Some(position) = &through {
            select.cond_where(bound(position, forward)?);
        }
        let order = if forward { Order::Asc } else { Order::Desc };
        let page_size = std::cmp::Ord::min(
            u64::from(context.platform.limits.processing.history_scan_page_size),
            (work.maximum - work.visited) as u64,
        );
        select
            .order_by(
                (history::Entity, history::Column::AdmittedGeneration),
                order.clone(),
            )
            .order_by(
                (history::Entity, history::Column::FragmentIndex),
                order.clone(),
            )
            .order_by((history::Entity, history::Column::SourceId), order)
            .limit(page_size);
        let rows = ThinHistory::find_by_statement(db.get_database_backend().build(&select))
            .all(db)
            .await?;
        let finished = rows.len() < page_size as usize;
        for row in rows {
            if !work.may_read() {
                more = true;
                break 'pages;
            }
            work.visited += 1;
            let position = WindowPosition {
                admitted_generation: unsigned(row.admitted_generation)?,
                fragment_index: u32::try_from(row.fragment_index)
                    .map_err(|_| AssistantStoreError::Inaccessible)?,
                source_id: row.source_id.clone(),
            };
            if row.already_seen(query, pending) {
                through = Some(position);
                continue;
            }
            let candidate = readable_on(db, context, &row, work).await?;
            if let Some(item) = candidate
                && !query.was_seen(&item.source)
                && !pending.iter().any(|prior| prior.source == item.source)
            {
                if items.len() >= limit {
                    more = true;
                    break 'pages;
                }
                items.push(item);
            }
            through = Some(position);
        }
        if finished {
            break;
        }
    }
    Ok(DirectionPage {
        items,
        through,
        more,
    })
}

fn bound(position: &WindowPosition, forward: bool) -> Result<Condition, AssistantStoreError> {
    let generation = Expr::col((history::Entity, history::Column::AdmittedGeneration));
    let fragment = Expr::col((history::Entity, history::Column::FragmentIndex));
    let source = Expr::col((history::Entity, history::Column::SourceId));
    let relation = if forward {
        generation.clone().gt(signed(position.admitted_generation)?)
    } else {
        generation.clone().lt(signed(position.admitted_generation)?)
    };
    let fragment_relation = if forward {
        fragment.clone().gt(i64::from(position.fragment_index))
    } else {
        fragment.clone().lt(i64::from(position.fragment_index))
    };
    let source_relation = if forward {
        source.gt(position.source_id.clone())
    } else {
        source.lt(position.source_id.clone())
    };
    Ok(Condition::any()
        .add(relation)
        .add(
            Condition::all()
                .add(generation.clone().eq(signed(position.admitted_generation)?))
                .add(fragment_relation),
        )
        .add(
            Condition::all()
                .add(generation.eq(signed(position.admitted_generation)?))
                .add(fragment.eq(i64::from(position.fragment_index)))
                .add(source_relation),
        ))
}
fn position(anchor: &PageAnchor) -> Option<(WindowPosition, bool)> {
    match anchor {
        PageAnchor::Window {
            admitted_generation,
            fragment_index,
            source_id,
            forward,
        } => Some((
            WindowPosition {
                admitted_generation: *admitted_generation,
                fragment_index: *fragment_index,
                source_id: source_id.clone(),
            },
            *forward,
        )),
        _ => None,
    }
}
