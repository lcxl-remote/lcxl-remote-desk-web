//! Bounded owner-only retained fragments, independent of execution-state lifetime.
//! Cursor rows define immutable ordering, never authority; every page rechecks
//! the owner, exact anchor, device, retention and each returned source.
use super::{AssistantStoreError, unsigned};
use crate::{config::ConfigConnection, entity::agent_assistant_history as history};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use desk_agent_protocol::ai_assistant::{
    history_window::{
        AssistantHistoryFragment, AssistantHistoryWindow, AssistantHistoryWindowRequest,
        AssistantHistoryWindowView,
    },
    source::{AssistantSourceKind, SourceAvailability},
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, ExprTrait, QueryFilter, QueryOrder, QuerySelect,
    sea_query::{Condition, Expr},
};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

const LIMIT: u64 = 20;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    source_id: String,
    revision: u64,
    view: AssistantHistoryWindowView,
    anchor: String,
    tail: String,
    tail_revision: i64,
    tail_hash: String,
    boundary: String,
    forward: bool,
}
impl Cursor {
    fn encode(&self) -> Result<String, AssistantStoreError> {
        let bytes = serde_json::to_vec(self).map_err(|_| AssistantStoreError::Inaccessible)?;
        let value = format!("rh1_{}", URL_SAFE_NO_PAD.encode(bytes));
        if value.len() > 2048 {
            return Err(AssistantStoreError::Capacity);
        }
        Ok(value)
    }
}
fn available(value: &desk_agent_protocol::ai_assistant::source::AssistantSourceResolution) -> bool {
    matches!(
        value.availability,
        SourceAvailability::Available | SourceAvailability::Partial | SourceAvailability::Redacted
    ) && value.content.is_some()
        && value.locator.is_some()
}
fn position(row: &history::Model) -> (i64, i32, &str) {
    (row.admitted_generation, row.fragment_index, &row.source_id)
}
fn bound(row: &history::Model, forward: bool, inclusive: bool) -> Condition {
    let generation = Expr::col(history::Column::AdmittedGeneration);
    let fragment = Expr::col(history::Column::FragmentIndex);
    let source = Expr::col(history::Column::SourceId);
    let mut result = Condition::any()
        .add(if forward {
            generation.clone().gt(row.admitted_generation)
        } else {
            generation.clone().lt(row.admitted_generation)
        })
        .add(
            Condition::all()
                .add(generation.clone().eq(row.admitted_generation))
                .add(if forward {
                    fragment.clone().gt(row.fragment_index)
                } else {
                    fragment.clone().lt(row.fragment_index)
                }),
        )
        .add(
            Condition::all()
                .add(generation.clone().eq(row.admitted_generation))
                .add(fragment.clone().eq(row.fragment_index))
                .add(if forward {
                    source.clone().gt(&row.source_id)
                } else {
                    source.clone().lt(&row.source_id)
                }),
        );
    if inclusive {
        result = result.add(
            Condition::all()
                .add(generation.eq(row.admitted_generation))
                .add(fragment.eq(row.fragment_index))
                .add(source.eq(&row.source_id)),
        );
    }
    result
}
fn base(actor: &str, anchor: &history::Model) -> sea_orm::Select<history::Entity> {
    history::Entity::find()
        .filter(history::Column::ActorId.eq(actor))
        .filter(history::Column::ConversationId.eq(&anchor.conversation_id))
        .filter(history::Column::RootConversationId.eq(&anchor.root_conversation_id))
        .filter(history::Column::DeviceRef.eq(&anchor.device_ref))
        .filter(history::Column::DeletedAtMs.is_null())
}
async fn exact_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    actor: &str,
    anchor: &history::Model,
    id: &str,
    policy: &desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicy,
    now: u64,
) -> Result<history::Model, AssistantStoreError> {
    let row = base(actor, anchor)
        .filter(history::Column::SourceId.eq(id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if !available(
        &desk_diagnose_core::future::send_phase(|| {
            super::owner_source::history_source_on(db, actor, &row, policy, now)
        })
        .await?,
    ) {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(row)
}

pub(crate) async fn read_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    actor: &str,
    request: &AssistantHistoryWindowRequest,
    policy: &desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicy,
    now: u64,
) -> Result<AssistantHistoryWindow, AssistantStoreError> {
    request.validate().map_err(AssistantStoreError::Invalid)?;
    let original = desk_diagnose_core::future::send_phase(|| {
        super::owner_source::owner_source_on(db, actor, &request.source_id, policy, now)
    })
    .await?;
    if original.revision != request.revision || !available(&original) {
        return Err(AssistantStoreError::Inaccessible);
    }
    let locator = original
        .locator
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    // A memory handle can resolve to one exact original fragment. Ambiguous
    // same-text records never select an arbitrary conversation/message.
    let candidates = if let Some(row) = history::Entity::find()
        .filter(history::Column::ActorId.eq(actor))
        .filter(history::Column::SourceId.eq(&request.source_id))
        .one(db)
        .await?
    {
        vec![row]
    } else {
        history::Entity::find()
            .filter(history::Column::ActorId.eq(actor))
            .filter(history::Column::ConversationId.eq(&locator.conversation_id))
            .filter(history::Column::ContentSha256.eq(&locator.content_sha256))
            .filter(history::Column::FragmentIndex.eq(i64::from(locator.fragment_index)))
            .filter(
                history::Column::RecordedAtMs.eq(super::signed(
                    original
                        .recorded_at_unix_ms
                        .ok_or(AssistantStoreError::Inaccessible)?,
                )?),
            )
            .limit(2)
            .all(db)
            .await?
    };
    let [anchor] = candidates.as_slice() else {
        return Err(AssistantStoreError::Inaccessible);
    };
    let resolved = desk_diagnose_core::future::send_phase(|| {
        super::owner_source::history_source_on(db, actor, anchor, policy, now)
    })
    .await?;
    if resolved.locator != original.locator
        || resolved.content != original.content
        || !available(&resolved)
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let cursor: Option<Cursor> = request
        .cursor
        .as_ref()
        .map(|raw| {
            let bytes = URL_SAFE_NO_PAD
                .decode(
                    raw.strip_prefix("rh1_")
                        .ok_or(AssistantStoreError::Inaccessible)?,
                )
                .map_err(|_| AssistantStoreError::Inaccessible)?;
            serde_json::from_slice(&bytes).map_err(|_| AssistantStoreError::Inaccessible)
        })
        .transpose()?;
    let mut partial = false;
    let tail = if let Some(cursor) = &cursor {
        if cursor.source_id != request.source_id
            || cursor.revision != request.revision
            || cursor.view != request.view
            || cursor.anchor != anchor.source_id
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        let tail = desk_diagnose_core::future::send_phase(|| {
            exact_on(db, actor, anchor, &cursor.tail, policy, now)
        })
        .await?;
        if tail.revision != cursor.tail_revision
            || tail.content_sha256 != cursor.tail_hash
            || position(&tail) < position(anchor)
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        tail
    } else {
        let maximum = std::cmp::min(
            u64::from(policy.max_retrieval_document_visits),
            u64::from(policy.limits.retrieval.owner_scan_documents),
        );
        let began = Instant::now();
        let duration = Duration::from_millis(policy.limits.retrieval.owner_scan_ms);
        let mut scanned = 0;
        let mut tail = None;
        while scanned < maximum && began.elapsed() < duration {
            let page = std::cmp::min(
                maximum - scanned,
                u64::from(policy.limits.processing.history_scan_page_size),
            );
            let rows = base(actor, anchor)
                .order_by_desc(history::Column::AdmittedGeneration)
                .order_by_desc(history::Column::FragmentIndex)
                .order_by_desc(history::Column::SourceId)
                .offset(scanned)
                .limit(page)
                .all(db)
                .await?;
            let finished = rows.len() < page as usize;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                if began.elapsed() >= duration {
                    break;
                }
                scanned += 1;
                if available(
                    &desk_diagnose_core::future::send_phase(|| {
                        super::owner_source::history_source_on(db, actor, &row, policy, now)
                    })
                    .await?,
                ) {
                    tail = Some(row);
                    break;
                }
                partial = true;
            }
            if tail.is_some() || finished {
                break;
            }
        }
        if tail.is_none() {
            partial = true;
        }
        // The already-validated anchor remains readable when tail discovery
        // exhausts its scan budget. It does not establish an exhaustive tail.
        tail.unwrap_or_else(|| anchor.clone())
    };
    let mut selected = Vec::new();
    if let Some(cursor) = &cursor {
        let boundary = desk_diagnose_core::future::send_phase(|| {
            exact_on(db, actor, anchor, &cursor.boundary, policy, now)
        })
        .await?;
        if position(&boundary) > position(&tail) {
            return Err(AssistantStoreError::Inaccessible);
        }
        let forward_page = cursor.forward;
        let query = base(actor, anchor)
            .filter(bound(&tail, false, true))
            .filter(bound(&boundary, forward_page, false));
        selected = if forward_page {
            query
                .order_by_asc(history::Column::AdmittedGeneration)
                .order_by_asc(history::Column::FragmentIndex)
                .order_by_asc(history::Column::SourceId)
                .limit(LIMIT + 1)
                .all(db)
                .await?
        } else {
            query
                .order_by_desc(history::Column::AdmittedGeneration)
                .order_by_desc(history::Column::FragmentIndex)
                .order_by_desc(history::Column::SourceId)
                .limit(LIMIT + 1)
                .all(db)
                .await?
        };
    } else if request.view == AssistantHistoryWindowView::Latest {
        selected = base(actor, anchor)
            .filter(bound(&tail, false, true))
            .order_by_desc(history::Column::AdmittedGeneration)
            .order_by_desc(history::Column::FragmentIndex)
            .order_by_desc(history::Column::SourceId)
            .limit(LIMIT + 1)
            .all(db)
            .await?;
    } else {
        selected.push(anchor.clone());
        selected.extend(
            base(actor, anchor)
                .filter(bound(anchor, false, false))
                .order_by_desc(history::Column::AdmittedGeneration)
                .order_by_desc(history::Column::FragmentIndex)
                .order_by_desc(history::Column::SourceId)
                .limit(9)
                .all(db)
                .await?,
        );
        selected.extend(
            base(actor, anchor)
                .filter(bound(&tail, false, true))
                .filter(bound(anchor, true, false))
                .order_by_asc(history::Column::AdmittedGeneration)
                .order_by_asc(history::Column::FragmentIndex)
                .order_by_asc(history::Column::SourceId)
                .limit(10)
                .all(db)
                .await?,
        );
    }
    let mut result = AssistantHistoryWindow {
        conversation_id: anchor.conversation_id.clone(),
        anchor_source_id: anchor.source_id.clone(),
        anchor_revision: unsigned(anchor.revision)?,
        observed_through_generation: unsigned(tail.admitted_generation)?,
        fragments: Vec::new(),
        previous_cursor: None,
        next_cursor: None,
        partial,
    };
    let mut emitted = Vec::new();
    for row in selected {
        if emitted.len() >= LIMIT as usize {
            break;
        }
        let source = desk_diagnose_core::future::send_phase(|| {
            super::owner_source::history_source_on(db, actor, &row, policy, now)
        })
        .await?;
        if !available(&source) {
            result.partial = true;
            continue;
        }
        let kind = match row.kind.as_str() {
            "input" => AssistantSourceKind::Input,
            "assistant_message" => AssistantSourceKind::AssistantMessage,
            "receipt" => AssistantSourceKind::Receipt,
            "native_status" => AssistantSourceKind::NativeStatus,
            _ => return Err(AssistantStoreError::Inaccessible),
        };
        result.fragments.push(AssistantHistoryFragment {
            source,
            role: row.role.clone(),
            kind,
        });
        emitted.push(row);
    }
    if emitted.is_empty() {
        return Err(AssistantStoreError::Capacity);
    }
    emitted.sort_by(|a, b| position(a).cmp(&position(b)));
    result.fragments.sort_by_key(|fragment| {
        emitted
            .iter()
            .position(|row| row.source_id == fragment.source.source_id)
    });
    while result.fragments.len() > 1
        && serde_json::to_vec(&result)
            .map_err(|_| AssistantStoreError::Inaccessible)?
            .len()
            > policy.limits.conversation.page_bytes as usize
    {
        let trim_back = cursor.as_ref().is_some_and(|cursor| cursor.forward)
            || (cursor.is_none()
                && request.view == AssistantHistoryWindowView::Context
                && emitted[0].source_id == anchor.source_id);
        if trim_back {
            result.fragments.pop();
            emitted.pop();
        } else {
            result.fragments.remove(0);
            emitted.remove(0);
        }
    }
    let first = &emitted[0];
    let last = emitted.last().ok_or(AssistantStoreError::Inaccessible)?;
    let make_cursor = |boundary: &history::Model, forward| {
        Cursor {
            source_id: request.source_id.clone(),
            revision: request.revision,
            view: request.view,
            anchor: anchor.source_id.clone(),
            tail: tail.source_id.clone(),
            tail_revision: tail.revision,
            tail_hash: tail.content_sha256.clone(),
            boundary: boundary.source_id.clone(),
            forward,
        }
        .encode()
    };
    if base(actor, anchor)
        .filter(bound(first, false, false))
        .limit(1)
        .one(db)
        .await?
        .is_some()
    {
        result.previous_cursor = Some(make_cursor(first, false)?);
    }
    if position(last) < position(&tail) {
        result.next_cursor = Some(make_cursor(last, true)?);
    }
    // Full row equality and the original owner locator are re-read after page
    // construction. Sharing a transaction is not a snapshot guarantee.
    for row in &emitted {
        if desk_diagnose_core::future::send_phase(|| {
            exact_on(db, actor, anchor, &row.source_id, policy, now)
        })
        .await?
            != *row
        {
            return Err(AssistantStoreError::Inaccessible);
        }
    }
    if desk_diagnose_core::future::send_phase(|| {
        super::owner_source::owner_source_on(db, actor, &request.source_id, policy, now)
    })
    .await?
        != original
        || desk_diagnose_core::future::send_phase(|| {
            exact_on(db, actor, anchor, &tail.source_id, policy, now)
        })
        .await?
            != tail
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(result)
}

pub(crate) async fn owner_history_window(
    db: &crate::config::connection::DatabaseConnection,
    request: &AssistantHistoryWindowRequest,
    now: u64,
) -> Result<AssistantHistoryWindow, AssistantStoreError> {
    use sea_orm::TransactionTrait;
    let txn = db.begin().await?;
    let actor = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string();
    super::ensure_runtime_on(&txn, &actor).await?;
    let config = txn.config_read().await;
    let result = desk_diagnose_core::future::boxed(|| {
        read_on(&txn, &actor, request, &config.assistant_data_policy, now)
    })
    .await?;
    txn.commit().await?;
    Ok(result)
}
