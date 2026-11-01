//! Shared SQL boundaries for relevance, time enumeration and original windows.
//! Native actor/task fences and final source authorization remain with callers.

use super::{AssistantStoreError, keyword_search::HistorySearchAuthorization, signed};
use crate::entity::{
    agent_assistant_history as history, agent_assistant_search_document as document,
    agent_assistant_search_term as term,
};
use desk_agent_protocol::ai_assistant::history::{ConversationSearchRequest, HistoryTimeField};
use sea_orm::ExprTrait;
use sea_orm::sea_query::{Condition, Expr, Func, Query, SelectStatement};
use std::collections::BTreeSet;

pub fn base(
    context: &HistorySearchAuthorization<'_>,
    devices: &[String],
) -> Result<SelectStatement, AssistantStoreError> {
    let mut query = Query::select();
    query.from(history::Entity).and_where(
        Expr::col((history::Entity, history::Column::DeviceRef)).is_in(devices.iter().cloned()),
    );
    apply_scope(&mut query, context)?;
    Ok(query)
}

pub fn apply_scope(
    query: &mut SelectStatement,
    context: &HistorySearchAuthorization<'_>,
) -> Result<(), AssistantStoreError> {
    query
        .and_where(Expr::col((history::Entity, history::Column::ActorId)).eq(context.actor_id))
        .and_where(
            Expr::col((history::Entity, history::Column::AdmittedGeneration))
                .lte(signed(context.corpus_generation)?),
        )
        .and_where(Expr::col((history::Entity, history::Column::DeletedAtMs)).is_null())
        .and_where(
            Expr::col((history::Entity, history::Column::ExpiresAtMs))
                .gt(signed(context.now_unix_ms)?),
        )
        .and_where(
            Expr::col((history::Entity, history::Column::RecordedAtMs)).gt(signed(
                context.now_unix_ms.saturating_sub(
                    u64::from(context.platform.history_retention_days) * 86_400_000,
                ),
            )?),
        )
        .and_where(
            Expr::col((history::Entity, history::Column::Kind))
                .is_in(kinds(&context.scope.allowed_kinds)?),
        );
    if let Some(range) = &context.scope.time_range {
        query
            .and_where(
                Expr::col((history::Entity, history::Column::RecordedAtMs))
                    .gte(signed(range.from_unix_ms)?),
            )
            .and_where(
                Expr::col((history::Entity, history::Column::RecordedAtMs))
                    .lt(signed(range.until_unix_ms)?),
            );
    }
    Ok(())
}

pub fn apply_search(
    query: &mut SelectStatement,
    context: &HistorySearchAuthorization<'_>,
    request: &ConversationSearchRequest,
) -> Result<(), AssistantStoreError> {
    if !request.conversation_ids.is_empty() {
        query.and_where(
            Expr::col((history::Entity, history::Column::ConversationId))
                .is_in(request.conversation_ids.iter().cloned()),
        );
    }
    if request.exclude_current_conversation {
        query.and_where(
            Expr::col((history::Entity, history::Column::RootConversationId))
                .ne(context.scope.root_conversation_id.clone()),
        );
    }
    if !request.source_kinds.is_empty() {
        query.and_where(
            Expr::col((history::Entity, history::Column::Kind))
                .is_in(kinds(&request.source_kinds)?),
        );
    }
    if let Some(range) = &request.time_range {
        if request.time_field == HistoryTimeField::RecordedAt {
            query
                .and_where(
                    Expr::col((history::Entity, history::Column::RecordedAtMs))
                        .gte(signed(range.from_unix_ms)?),
                )
                .and_where(
                    Expr::col((history::Entity, history::Column::RecordedAtMs))
                        .lt(signed(range.until_unix_ms)?),
                );
        } else {
            query.cond_where(
                Condition::any()
                    .add(
                        Condition::all()
                            .add(
                                Expr::col((history::Entity, history::Column::NativeActionAtMs))
                                    .gte(signed(range.from_unix_ms)?),
                            )
                            .add(
                                Expr::col((history::Entity, history::Column::NativeActionAtMs))
                                    .lt(signed(range.until_unix_ms)?),
                            ),
                    )
                    .add(
                        Condition::all()
                            .add(
                                Expr::col((history::Entity, history::Column::NativeActionAtMs))
                                    .is_null(),
                            )
                            .add(valid_interval())
                            .add(
                                Expr::col((history::Entity, history::Column::NativeActionFromMs))
                                    .lt(signed(range.until_unix_ms)?),
                            )
                            .add(
                                Expr::col((history::Entity, history::Column::NativeActionUntilMs))
                                    .gt(signed(range.from_unix_ms)?),
                            ),
                    ),
            );
        }
    }
    Ok(())
}

pub fn native_time_available() -> Condition {
    Condition::any()
        .add(Expr::col((history::Entity, history::Column::NativeActionAtMs)).gt(0))
        .add(
            Condition::all()
                .add(Expr::col((history::Entity, history::Column::NativeActionAtMs)).is_null())
                .add(valid_interval()),
        )
}
fn valid_interval() -> Condition {
    Condition::all()
        .add(Expr::col((history::Entity, history::Column::NativeActionFromMs)).gt(0))
        .add(
            Expr::col((history::Entity, history::Column::NativeActionUntilMs)).gt(Expr::col((
                history::Entity,
                history::Column::NativeActionFromMs,
            ))),
        )
}
pub fn time_expression(field: HistoryTimeField) -> Expr {
    if field == HistoryTimeField::RecordedAt {
        Expr::col((history::Entity, history::Column::RecordedAtMs))
    } else {
        Func::coalesce([
            Expr::col((history::Entity, history::Column::NativeActionAtMs)),
            Expr::col((history::Entity, history::Column::NativeActionFromMs)),
        ])
        .into()
    }
}

/// Optional keywords narrow an otherwise temporal page. EXISTS avoids returning
/// a source repeatedly for different matching terms and never invokes ranking.
pub fn apply_terms(
    query: &mut SelectStatement,
    context: &HistorySearchAuthorization<'_>,
    terms: &BTreeSet<String>,
) -> Result<(), AssistantStoreError> {
    if terms.is_empty() {
        return Ok(());
    }
    let mut matches = Query::select();
    matches
        .expr(Expr::value(1))
        .from(term::Entity)
        .inner_join(
            document::Entity,
            Expr::col((term::Entity, term::Column::DocumentId))
                .equals((document::Entity, document::Column::DocumentId)),
        )
        .and_where(Expr::col((term::Entity, term::Column::ActorId)).eq(context.actor_id))
        .and_where(Expr::col((document::Entity, document::Column::ActorId)).eq(context.actor_id))
        .and_where(Expr::col((document::Entity, document::Column::SourceKind)).eq("history"))
        .and_where(
            Expr::col((document::Entity, document::Column::CorpusGeneration))
                .lte(signed(context.corpus_generation)?),
        )
        .and_where(
            Expr::col((document::Entity, document::Column::SourceId))
                .equals((history::Entity, history::Column::SourceId)),
        )
        .and_where(
            Expr::col((document::Entity, document::Column::SourceRevision))
                .equals((history::Entity, history::Column::Revision)),
        )
        .and_where(
            Expr::col((document::Entity, document::Column::SourceRevision))
                .equals((term::Entity, term::Column::SourceRevision)),
        )
        .and_where(
            Expr::col((document::Entity, document::Column::ContentSha256))
                .equals((history::Entity, history::Column::ContentSha256)),
        )
        .and_where(
            Expr::col((document::Entity, document::Column::DeviceRef))
                .equals((history::Entity, history::Column::DeviceRef)),
        )
        .and_where(Expr::col((term::Entity, term::Column::Term)).is_in(terms.iter().cloned()));
    query.and_where(Expr::exists(matches));
    Ok(())
}

fn kinds(
    values: &[desk_agent_protocol::ai_assistant::source::AssistantSourceKind],
) -> Result<Vec<String>, AssistantStoreError> {
    values
        .iter()
        .map(|kind| {
            serde_json::to_value(kind)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
        })
        .collect::<Option<_>>()
        .ok_or(AssistantStoreError::Invalid("invalid history source kinds"))
}
