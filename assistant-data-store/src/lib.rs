//! Personal-assistant storage shared by Manager and standalone Signal.
//! Callers own transactions, native authorization, configuration and billing.
//! A stored snapshot establishes content identity, never permission to use it.

pub mod citations;
pub mod entity;
pub mod evidence;
pub mod history_native;
pub mod history_query;
pub mod keyword_search;
pub mod memory_control;
pub mod memory_history;
pub mod memory_index;
pub mod memory_keyword;
pub mod memory_quota;
pub mod memory_write;
pub mod output_gc;
pub mod projection;
pub mod retrieval_cache;
pub mod source_lifecycle;
pub mod suppression;

use entity::agent_assistant_quota as quota;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, Set, sea_query::OnConflict,
};

#[derive(Debug)]
pub enum AssistantStoreError {
    Inaccessible,
    SourceUnavailable,
    Capacity,
    Invalid(&'static str),
    Stale { current_revision: u64 },
    Backend(DbErr),
}

impl From<DbErr> for AssistantStoreError {
    fn from(error: DbErr) -> Self {
        Self::Backend(error)
    }
}

impl AssistantStoreError {
    #[inline]
    pub fn into_agent_error(self) -> desk_agent_protocol::AgentError {
        use desk_agent_protocol::AgentErrorKind::{Internal, InvalidInput};
        let (kind, message, retryable, safe_for_model) = match self {
            Self::Inaccessible => (Internal, "assistant data is inaccessible", false, false),
            Self::SourceUnavailable => (
                InvalidInput,
                "Requested memory evidence is unavailable. Use a source returned by search_memories, read_memory_history or current memories; use read_conversation for history search results.",
                false,
                true,
            ),
            Self::Capacity => (
                InvalidInput,
                "Assistant data capacity or read budget reached; the requested scope is incomplete",
                false,
                true,
            ),
            Self::Invalid(message) => (Internal, message, false, false),
            Self::Stale { .. } => (
                Internal,
                "assistant data changed; synchronize before retrying",
                true,
                false,
            ),
            Self::Backend(_) => (
                Internal,
                "assistant data storage is unavailable",
                true,
                false,
            ),
        };
        desk_agent_protocol::AgentError {
            kind,
            message: message.into(),
            retryable,
            safe_for_model,
            error_code: None,
        }
    }
}

/// Only call after the original transaction's final business write.
pub async fn ensure_quota_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
) -> Result<(), AssistantStoreError> {
    quota::Entity::insert(quota::ActiveModel {
        actor_id: Set(actor.into()),
        history_records: Set(0),
        history_body_bytes: Set(0),
        memory_records: Set(0),
        memory_body_bytes: Set(0),
        term_records: Set(0),
        metadata_bytes: Set(8192),
        output_gc_next_scan_at_ms: Set(Some(chrono::Utc::now().timestamp_millis())),
        revision: Set(1),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(quota::Column::ActorId)
            .do_nothing()
            .to_owned(),
    )
    .exec_without_returning(db)
    .await?;
    Ok(())
}

#[inline]
pub fn increment(value: i64) -> Result<i64, AssistantStoreError> {
    value
        .checked_add(1)
        .filter(|v| *v > 0)
        .ok_or(AssistantStoreError::Invalid(
            "assistant generation exhausted",
        ))
}
#[inline]
pub fn signed(value: u64) -> Result<i64, AssistantStoreError> {
    i64::try_from(value).map_err(|_| AssistantStoreError::Invalid("assistant counter out of range"))
}
#[inline]
pub fn unsigned(value: i64) -> Result<u64, AssistantStoreError> {
    u64::try_from(value)
        .map_err(|_| AssistantStoreError::Invalid("invalid assistant counter state"))
}

pub async fn runtime_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
) -> Result<entity::agent_assistant_runtime::Model, AssistantStoreError> {
    entity::agent_assistant_runtime::Entity::find()
        .filter(entity::agent_assistant_runtime::Column::ActorId.eq(actor))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Invalid(
            "missing assistant runtime row",
        ))
}

#[cfg(test)]
mod tests;
