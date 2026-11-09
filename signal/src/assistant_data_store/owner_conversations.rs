//! Retained root list, separate from live connections and native execution rows.
use super::{AssistantStoreError, signed, unsigned};
use crate::{
    config::ConfigConnection,
    entity::{
        agent_assistant_history as history, agent_assistant_history_conversation as directory,
        agent_assistant_runtime as runtime, agent_session as session,
    },
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use desk_agent_protocol::ai_assistant::{
    owner_history::{
        AssistantHistoryConversation, AssistantHistoryConversations,
        AssistantHistoryConversationsRequest,
    },
    source::SourceAvailability,
    user_instructions::AssistantDataPolicy,
};
use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    sea_query::Condition,
};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    actor: String,
    policy: u64,
    watermark: u64,
    before: i64,
    id: String,
}
fn cursor(value: &Cursor) -> Result<String, AssistantStoreError> {
    Ok(format!(
        "hc1_{}",
        URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(value).map_err(|_| AssistantStoreError::Inaccessible)?)
    ))
}
pub(crate) async fn list_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    actor: &str,
    request: &AssistantHistoryConversationsRequest,
    policy: &AssistantDataPolicy,
    now: u64,
) -> Result<AssistantHistoryConversations, AssistantStoreError> {
    if request.limit == 0
        || request.limit > 20
        || request
            .cursor
            .as_ref()
            .is_some_and(|value| value.len() > 1024)
    {
        return Err(AssistantStoreError::Invalid(
            "invalid retained history list",
        ));
    }
    let runtime = runtime::Entity::find()
        .filter(runtime::Column::ActorId.eq(actor))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    let watermark = unsigned(runtime.history_data_generation)?;
    let previous: Option<Cursor> = request
        .cursor
        .as_ref()
        .map(|raw| {
            let bytes = URL_SAFE_NO_PAD
                .decode(
                    raw.strip_prefix("hc1_")
                        .ok_or(AssistantStoreError::Invalid("invalid history list cursor"))?,
                )
                .map_err(|_| AssistantStoreError::Invalid("invalid history list cursor"))?;
            let value: Cursor = serde_json::from_slice(&bytes)
                .map_err(|_| AssistantStoreError::Invalid("invalid history list cursor"))?;
            if value.actor != actor
                || value.policy != policy.revision
                || value.watermark > watermark
                || value.before <= 0
                || value.id.len() > 256
            {
                return Err(AssistantStoreError::Invalid("history list cursor changed"));
            }
            Ok(value)
        })
        .transpose()?;
    let observed = previous.as_ref().map_or(watermark, |value| value.watermark);
    let mut query = directory::Entity::find()
        .filter(directory::Column::ActorId.eq(actor))
        .filter(directory::Column::ParentConversationId.is_null())
        .filter(directory::Column::Status.ne("user_deleted"))
        .filter(directory::Column::LastRecordedAtMs.gt(signed(
            now.saturating_sub(u64::from(policy.history_retention_days) * 86_400_000),
        )?));
    if let Some(old) = &previous {
        query = query.filter(
            Condition::any()
                .add(directory::Column::LastRecordedAtMs.lt(old.before))
                .add(
                    Condition::all()
                        .add(directory::Column::LastRecordedAtMs.eq(old.before))
                        .add(directory::Column::ConversationId.lt(&old.id)),
                ),
        );
    }
    let maximum = u64::from(policy.max_retrieval_document_visits)
        .min(u64::from(policy.limits.retrieval.owner_scan_documents));
    let began = Instant::now();
    let duration = Duration::from_millis(policy.limits.retrieval.owner_scan_ms);
    let mut remaining = maximum;
    let rows = query
        .order_by_desc(directory::Column::LastRecordedAtMs)
        .order_by_desc(directory::Column::ConversationId)
        .limit(maximum + 1)
        .all(db)
        .await?;
    let mut result = AssistantHistoryConversations {
        conversations: Vec::new(),
        next_cursor: None,
        partial: previous
            .as_ref()
            .is_some_and(|old| old.watermark != watermark),
    };
    let mut before = None;
    let mut scanned: usize = 0;
    for root in rows.iter().take(maximum as usize) {
        if result.conversations.len() == request.limit as usize {
            break;
        }
        if remaining == 0 || began.elapsed() >= duration {
            result.partial = true;
            break;
        }
        remaining -= 1;
        scanned += 1;
        before = Some(root);
        if root.root_conversation_id != root.conversation_id || root.revision <= 0 {
            result.partial = true;
            continue;
        }
        let query = history::Entity::find()
            .filter(history::Column::ActorId.eq(actor))
            .filter(history::Column::ConversationId.eq(&root.conversation_id))
            .filter(history::Column::RootConversationId.eq(&root.conversation_id))
            .filter(history::Column::DeviceRef.eq(&root.device_ref))
            .filter(history::Column::DeletedAtMs.is_null())
            .filter(history::Column::ExpiresAtMs.gt(signed(now)?))
            .filter(history::Column::AdmittedGeneration.lte(signed(observed)?))
            .order_by_desc(history::Column::AdmittedGeneration)
            .order_by_asc(history::Column::FragmentIndex)
            .order_by_asc(history::Column::Id);
        let mut selected = None;
        let mut offset = 0;
        while remaining > 0 && began.elapsed() < duration {
            let page = remaining.min(u64::from(policy.limits.processing.history_scan_page_size));
            let fragments = query.clone().offset(offset).limit(page).all(db).await?;
            let finished = fragments.len() < page as usize;
            offset += fragments.len() as u64;
            for row in fragments {
                if began.elapsed() >= duration {
                    result.partial = true;
                    break;
                }
                remaining -= 1;
                let source =
                    super::owner_source::history_source_on(db, actor, &row, policy, now).await?;
                if source.content.is_some()
                    && source.locator.is_some()
                    && matches!(
                        source.availability,
                        SourceAvailability::Available
                            | SourceAvailability::Partial
                            | SourceAvailability::Redacted
                    )
                {
                    selected = Some(source);
                    break;
                }
            }
            if selected.is_some() || finished {
                break;
            }
        }
        result.partial |= remaining == 0 || began.elapsed() >= duration;
        let Some(source) = selected else {
            result.partial = true;
            continue;
        };
        let native = session::Entity::find()
            .filter(session::Column::ActorId.eq(actor))
            .filter(session::Column::ConversationId.eq(&root.conversation_id))
            .one(db)
            .await?;
        let native = native
            .as_ref()
            .filter(|row| {
                row.conversation_schema_version
                    == Some(i32::from(
                        desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
                    ))
            })
            .and_then(|row| PersistedAgentSession::decode_json(&row.state_json).ok())
            .filter(|value| {
                value.actor_id == actor
                    && value.conversation_id == root.conversation_id
                    && value.agent_role.is_main()
                    && value.surface == AgentSessionSurface::AiAssistant
                    && value.assistant_data.as_ref().is_some_and(|data| {
                        !data.temporary && data.source_device_ref == root.device_ref
                    })
            });
        let item = AssistantHistoryConversation {
            conversation_id: root.conversation_id.clone(),
            source,
            execution_available: native.is_some(),
            active: native.is_some_and(|value| value.turn_state.is_active()),
        };
        result.conversations.push(item);
        if serde_json::to_vec(&result)
            .map_err(|_| AssistantStoreError::Inaccessible)?
            .len()
            > policy.limits.conversation.page_bytes as usize
            && result.conversations.len() > 1
        {
            result.conversations.pop();
            scanned -= 1;
            before = rows.get(scanned.saturating_sub(1));
            break;
        }
    }
    if rows.len() > scanned
        && let Some(last) = before
    {
        result.next_cursor = Some(cursor(&Cursor {
            actor: actor.into(),
            policy: policy.revision,
            watermark: observed,
            before: last.last_recorded_at_ms,
            id: last.conversation_id.clone(),
        })?);
    }
    if result.conversations.is_empty() && result.next_cursor.is_some() && scanned == 0 {
        return Err(AssistantStoreError::Capacity);
    }
    for item in &result.conversations {
        if super::owner_source::owner_source_on(db, actor, &item.source.source_id, policy, now)
            .await?
            != item.source
        {
            return Err(AssistantStoreError::Inaccessible);
        }
    }
    Ok(result)
}

pub(crate) async fn owner_history_conversations(
    db: &crate::config::connection::DatabaseConnection,
    request: &AssistantHistoryConversationsRequest,
    now: u64,
) -> Result<AssistantHistoryConversations, AssistantStoreError> {
    use sea_orm::TransactionTrait;
    let txn = db.begin().await?;
    let actor = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string();
    super::ensure_runtime_on(&txn, &actor).await?;
    let config = txn.config_read().await;
    let result = desk_diagnose_core::future::boxed(|| {
        list_on(&txn, &actor, request, &config.assistant_data_policy, now)
    })
    .await?;
    txn.commit().await?;
    Ok(result)
}
