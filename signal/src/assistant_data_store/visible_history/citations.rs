//! Read-only repair of optional citation metadata from the exact native answer.
//! A locator never grants body access; source resolution keeps its current gate.
use super::*;
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};

pub(crate) fn load_on<'a, C: ConnectionTrait>(
    db: &'a C,
    actor: &'a str,
    conversation: &'a str,
    answers: &'a [(String, String)],
    now: u64,
    policy: &'a AssistantDataPolicy,
) -> impl std::future::Future<
    Output = Result<
        BTreeMap<String, desk_diagnose_core::ai_assistant::citation::AnswerCitations>,
        AssistantStoreError,
    >,
> + Send
+ 'a {
    desk_diagnose_core::future::send_phase(|| async move {
        let mut result = super::super::citations::load_on(
            db,
            actor,
            conversation,
            answers,
            now,
            policy.limits.conversation.citation_page_answers,
        )
        .await?;
        let missing = answers
            .iter()
            .filter(|(id, text)| !result.contains_key(id) && text.contains("〔来源:"))
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return Ok(result);
        }
        let Some(row) = agent_session::Entity::find()
            .filter(agent_session::Column::ActorId.eq(actor))
            .filter(agent_session::Column::ConversationId.eq(conversation))
            .filter(
                agent_session::Column::ConversationSchemaVersion.eq(i32::from(
                    desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
                )),
            )
            .one(db)
            .await?
        else {
            return Ok(result);
        };
        let session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| AssistantStoreError::Inaccessible)?;
        let Some(data) = session
            .assistant_data
            .as_ref()
            .filter(|data| !data.temporary)
        else {
            return Ok(result);
        };
        if session.actor_id != actor || session.conversation_id != conversation {
            return Err(AssistantStoreError::Inaccessible);
        }
        for (id, text) in missing {
            let Some(message) = session.conversation.iter().find(|message| {
                message.message_id == *id
                    && message.text == *text
                    && message.role == ChatRole::Assistant
            }) else {
                continue;
            };
            let Some(output) = message
                .data_envelope
                .as_ref()
                .filter(|output| is_native_output_policy_source(output))
            else {
                continue;
            };
            let Some(archive) =
                super::super::output_archive::find_on(db, conversation, output).await?
            else {
                continue;
            };
            let proof = &archive.proof;
            if proof.source_set.actor_id != actor
                || proof.source_set.source_device_ref != data.source_device_ref
            {
                return Err(AssistantStoreError::Inaccessible);
            }
            if proof
                .sources
                .evidence
                .completed_at_unix_ms
                .saturating_add(u64::from(policy.history_retention_days) * 86_400_000)
                <= now
            {
                continue;
            }
            proof
                .bind_visible_message(message)
                .map_err(AssistantStoreError::Invalid)?;
            crate::model_egress_store::SignalModelEgressStore::verify_retained_output_on(
                db,
                &proof.source_set,
                &proof.sources,
                &proof.output,
            )
            .await?;
            let roots = super::native::citation_roots_on(db, proof).await?;
            result.insert(
                id.clone(),
                desk_diagnose_core::ai_assistant::citation::for_answer(
                    id,
                    text,
                    &roots,
                    &policy.limits.conversation,
                ),
            );
        }
        Ok(result)
    })
}
