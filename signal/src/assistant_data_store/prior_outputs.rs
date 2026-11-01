//! Previously accepted answers are immutable content of this conversation.
use super::AssistantStoreError;
use desk_diagnose_core::{
    ai_assistant::source_registry::is_native_output_policy_source,
    model_egress::NativeModelInputSnapshot, session::PersistedAgentSession,
};
use sea_orm::ConnectionTrait;
pub(crate) fn validate_on<'a, C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &'a C,
    session: &'a PersistedAgentSession,
    snapshot: &'a NativeModelInputSnapshot,
    now: u64,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Option<u64>, AssistantStoreError>> + Send + 'a>,
> {
    desk_diagnose_core::future::send_phase(|| async move {
        let mut deadline: Option<u64> = None;
        for source in snapshot.policy_sources() {
            if !is_native_output_policy_source(source) {
                continue;
            }
            let stored = super::output_archive::find_on(db, &session.conversation_id, source)
                .await?
                .ok_or(AssistantStoreError::Inaccessible)?;
            if stored.proof.source_set.actor_id != session.actor_id {
                return Err(AssistantStoreError::Inaccessible);
            }
            if let Some(until) = stored
                .proof
                .retained_policy
                .as_ref()
                .and_then(|policy| policy.content_hard_expires_at_unix_ms)
            {
                if until <= now {
                    return Err(AssistantStoreError::Inaccessible);
                }
                deadline = Some(deadline.map_or(until, |old| old.min(until)));
            }
            let original =
                crate::model_egress_store::SignalModelEgressStore::read_context_output_reusing_on(
                    db,
                    &session.conversation_id,
                    source,
                    Some(&stored),
                )
                .await?;
            if original != stored.proof.sources {
                return Err(AssistantStoreError::Inaccessible);
            }
        }
        Ok(deadline)
    })
}
