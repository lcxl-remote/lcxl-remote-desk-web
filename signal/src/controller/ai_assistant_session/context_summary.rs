use super::*;
use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession};
use sea_orm::{ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter};

#[utoipa::path(
    tag = TAG,
    summary = "Read the summary one context compaction produced",
    params(
        ("session" = String, Query, description = "Opaque session id from the snapshot"),
        ("turn" = String, Query, description = "Turn id of the compaction notice"),
        ("generation" = u32, Query, description = "Checkpoint generation of the compaction notice"),
    ),
    responses((status = 200, description = "Summary state, or a uniform \
        not-found/not-accessible response", body = RestResponse<AiAssistantContextSummaryDto>)),
)]
#[get("/my/ai-assistant-session/context-summary")]
pub async fn get_ai_assistant_context_summary(
    query: web::Query<AiAssistantContextSummaryQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    match read_summary(
        crate::db::get_db(),
        &SINGLE_ACCOUNT_USER_ID.to_string(),
        &query,
    )
    .await
    {
        Ok(Some(summary)) => Ok(HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(RestResponse::succeed_with_data(summary))),
        Ok(None) => Ok(not_accessible()),
        Err(_) => Err(DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "AI Assistant session unavailable",
        )),
    }
}

/// Read-only owner view: the session row must belong to the actor.
async fn read_summary(
    db: &DatabaseConnection,
    actor: &str,
    query: &AiAssistantContextSummaryQuery,
) -> Result<Option<AiAssistantContextSummaryDto>, DbErr> {
    use crate::entity::agent_session;
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&query.session))
        .filter(agent_session::Column::ActorId.eq(actor))
        .one(db)
        .await?;
    Ok(row
        .and_then(|row| PersistedAgentSession::decode_json(&row.state_json).ok())
        .filter(|session| session.surface == AgentSessionSurface::AiAssistant)
        .map(|session| {
            AiAssistantContextSummaryDto::from_session(&session, &query.turn, query.generation)
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_diagnose_core::model_context::{ContextNotice, compacted_history_for_test};
    use sea_orm::{ActiveModelTrait, Set};

    #[actix_web::test]
    async fn only_the_owner_reads_a_compaction_summary() {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let (conversation, state) = compacted_history_for_test(
            "turn-7",
            r#"{"goals":[{"text":"Keep the goal","source_message_ids":["old-a"]}]}"#,
        );
        let mut session = PersistedAgentSession::new(
            "owned",
            "1",
            "",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-08-23T00:00:00Z",
        );
        session.surface = AgentSessionSurface::AiAssistant;
        session.conversation = conversation;
        session.model_context_state = state;
        session
            .context_notices
            .push(ContextNotice::compacted("turn-7", 1, 2));
        let now = chrono::Utc::now();
        crate::entity::agent_session::ActiveModel {
            conversation_id: Set("owned".into()),
            actor_id: Set("1".into()),
            device_id: Set(String::new()),
            state_json: Set(session.encode_json_for_storage().unwrap()),
            version: Set(session.version),
            lease_token: Set(0),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        let query = |session: &str, turn: &str| AiAssistantContextSummaryQuery {
            session: session.into(),
            turn: turn.into(),
            generation: 1,
        };
        let found = read_summary(&db, "1", &query("owned", "turn-7"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.state, ContextSummaryStateDto::Available);
        assert_eq!(
            found.summary.unwrap().sections[0].facts[0].text,
            "Keep the goal"
        );
        assert_eq!(
            read_summary(&db, "1", &query("owned", "turn-8"))
                .await
                .unwrap()
                .unwrap()
                .state,
            ContextSummaryStateDto::Unavailable
        );
        assert!(
            read_summary(&db, "2", &query("owned", "turn-7"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            read_summary(&db, "1", &query("missing", "turn-7"))
                .await
                .unwrap()
                .is_none()
        );
    }
}
