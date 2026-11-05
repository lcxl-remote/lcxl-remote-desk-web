//! Lifecycle writes share the original conversation/account transaction.
use super::*;

pub async fn stop_request_on(
    db: &impl ConnectionTrait,
    actor: &str,
    request: &str,
    now: i64,
) -> Result<(), AssistantStoreError> {
    stop_on(
        db,
        Condition::all()
            .add(event::Column::ActorId.eq(actor))
            .add(event::Column::RequestId.eq(request)),
        now,
    )
    .await
}

pub async fn stop_conversation_on(
    db: &impl ConnectionTrait,
    actor: &str,
    conversation: &str,
    now: i64,
) -> Result<(), AssistantStoreError> {
    stop_on(db, conversation_scope(actor, conversation), now).await
}

pub async fn retire_conversation_on(
    db: &impl ConnectionTrait,
    actor: &str,
    conversation: &str,
    now: i64,
) -> Result<(), AssistantStoreError> {
    retire_on(db, conversation_scope(actor, conversation), now).await?;
    source::Entity::delete_many()
        .filter(source::Column::ActorId.eq(actor))
        .filter(source::Column::ConversationId.eq(conversation))
        .exec(db)
        .await?;
    Ok(())
}

pub async fn retire_actor_on(
    db: &impl ConnectionTrait,
    actor: &str,
    now: i64,
) -> Result<(), AssistantStoreError> {
    retire_on(
        db,
        Condition::all().add(event::Column::ActorId.eq(actor)),
        now,
    )
    .await?;
    source::Entity::delete_many()
        .filter(source::Column::ActorId.eq(actor))
        .exec(db)
        .await?;
    Ok(())
}

fn conversation_scope(actor: &str, conversation: &str) -> Condition {
    Condition::all()
        .add(event::Column::ActorId.eq(actor))
        .add(event::Column::ConversationId.eq(conversation))
}

async fn stop_on(
    db: &impl ConnectionTrait,
    scope: Condition,
    now: i64,
) -> Result<(), AssistantStoreError> {
    event::Entity::update_many()
        .filter(scope)
        .filter(event::Column::State.is_in(["pending", "attached"]))
        .set(event::ActiveModel {
            state: Set("stopped".into()),
            reason: Set("work_stopped".into()),
            updated_at_ms: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await?;
    Ok(())
}

async fn retire_on(
    db: &impl ConnectionTrait,
    scope: Condition,
    now: i64,
) -> Result<(), AssistantStoreError> {
    stop_on(db, scope.clone(), now).await?;
    event::Entity::update_many()
        .filter(scope)
        .filter(event::Column::RetiredAtMs.is_null())
        .set(event::ActiveModel {
            snapshot_json: Set(None),
            issue_header_json: Set(None),
            issue_header_sha256: Set(None),
            admitted_policy_json: Set(None),
            admitted_policy_sha256: Set(None),
            retired_at_ms: Set(Some(now)),
            updated_at_ms: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await?;
    Ok(())
}
