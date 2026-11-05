//! Original-work control and data removal commit in the caller's native transaction.

use super::*;

fn conversation_scope(actor: &str, conversation: &str) -> Condition {
    Condition::all().add(reply::Column::ActorId.eq(actor)).add(
        Condition::any()
            .add(reply::Column::RootConversationId.eq(conversation))
            .add(reply::Column::ExecutionConversationId.eq(conversation)),
    )
}

/// An explicit stop-all also covers delivery awaiting dispatch after its Worker
/// completed. Already dispatched effects retain reconciliation-only state.
pub async fn cancel_conversation_on(
    db: &impl ConnectionTrait,
    actor: &str,
    conversation: &str,
    now_ms: i64,
) -> Result<(), AssistantStoreError> {
    cancel_on(
        db,
        conversation_scope(actor, conversation),
        "work_stopped",
        now_ms,
    )
    .await
}

/// Stop every executor of one workbench round while preserving dispatch facts.
pub(crate) async fn cancel_logical_request_on(
    db: &impl ConnectionTrait,
    actor: &str,
    workbench: &str,
    request: &str,
    now_ms: i64,
) -> Result<(), AssistantStoreError> {
    cancel_on(
        db,
        Condition::all()
            .add(reply::Column::ActorId.eq(actor))
            .add(reply::Column::RootConversationId.eq(workbench))
            .add(reply::Column::RequestId.eq(request))
            .add(reply::Column::RetiredAtMs.is_null()),
        "work_stopped",
        now_ms,
    )
    .await
}

/// Stops only this executor's original input. Earlier independent deliveries
/// in the same conversation and other executors keep their own authority.
pub async fn cancel_request_on(
    db: &impl ConnectionTrait,
    actor: &str,
    execution: &str,
    input_revision: u64,
    reason: &'static str,
    now_ms: i64,
) -> Result<(), AssistantStoreError> {
    cancel_on(
        db,
        Condition::all()
            .add(reply::Column::ActorId.eq(actor))
            .add(reply::Column::ExecutionConversationId.eq(execution))
            .add(
                reply::Column::InputRevision
                    .eq(i64::try_from(input_revision).map_err(|_| invalid())?),
            )
            .add(reply::Column::RetiredAtMs.is_null()),
        reason,
        now_ms,
    )
    .await
}

async fn cancel_on(
    db: &impl ConnectionTrait,
    scope: Condition,
    reason: &'static str,
    now_ms: i64,
) -> Result<(), AssistantStoreError> {
    for (states, next) in [
        (vec![INTERNAL, READY, CLAIMED], NOT_SENT),
        (vec![DISPATCHED, UNKNOWN], UNKNOWN),
    ] {
        reply::Entity::update_many()
            .filter(scope.clone())
            .filter(reply::Column::State.is_in(states))
            .col_expr(reply::Column::State, Expr::value(next))
            .col_expr(reply::Column::Reason, Expr::value(reason))
            .col_expr(reply::Column::ClaimToken, Expr::value(None::<String>))
            .col_expr(reply::Column::OwnerNode, Expr::value(None::<String>))
            .col_expr(reply::Column::LeaseExpiresAtMs, Expr::value(None::<i64>))
            .col_expr(reply::Column::NextAttemptAtMs, Expr::value(now_ms))
            .col_expr(reply::Column::UpdatedAtMs, Expr::value(now_ms))
            .exec(db)
            .await?;
    }
    Ok(())
}

/// Erases the result and its complete input metadata. The receipt key survives
/// because a dispatch already recorded before deletion may still have happened.
/// Callers retain their original subject/control locks and own the transaction.
pub async fn retire_conversation_on(
    db: &impl ConnectionTrait,
    actor: &str,
    conversation: &str,
    now_ms: i64,
) -> Result<(), AssistantStoreError> {
    crate::issue_authority::retire_conversation(db, actor, conversation, now_ms).await?;
    let scope = conversation_scope(actor, conversation).add(reply::Column::RetiredAtMs.is_null());
    retire_on(db, scope, "source_removed", now_ms).await
}

/// Retain only recovery keys when the original account-deletion transaction wins.
pub async fn retire_actor_on(
    db: &impl ConnectionTrait,
    actor: &str,
    now_ms: i64,
) -> Result<(), AssistantStoreError> {
    crate::issue_authority::retire_actor(db, actor, now_ms).await?;
    retire_on(
        db,
        Condition::all()
            .add(reply::Column::ActorId.eq(actor))
            .add(reply::Column::RetiredAtMs.is_null()),
        "source_removed",
        now_ms,
    )
    .await
}

async fn retire_on(
    db: &impl ConnectionTrait,
    scope: Condition,
    reason: &'static str,
    now_ms: i64,
) -> Result<(), AssistantStoreError> {
    cancel_on(db, scope.clone(), reason, now_ms).await?;
    reply::Entity::update_many()
        .filter(scope)
        .col_expr(reply::Column::ArtifactJson, Expr::value(None::<String>))
        .col_expr(reply::Column::RetiredAtMs, Expr::value(now_ms))
        .col_expr(reply::Column::UpdatedAtMs, Expr::value(now_ms))
        .exec(db)
        .await?;
    Ok(())
}

/// Bounded cleanup also covers internal and already delivered results. Receipt
/// metadata remains available for an effect whose response is still unknown.
pub async fn expire_on(
    db: &impl ConnectionTrait,
    now_ms: i64,
    limit: u64,
) -> Result<u64, AssistantStoreError> {
    crate::issue_authority::expire_on(db, now_ms, limit).await?;
    let scope = || {
        Condition::all()
            .add(reply::Column::RetiredAtMs.is_null())
            .add(reply::Column::ContentExpiresAtMs.lte(now_ms))
    };
    let ids = reply::Entity::find()
        .select_only()
        .column(reply::Column::Id)
        .filter(scope())
        .order_by_asc(reply::Column::Id)
        .limit(limit)
        .into_tuple::<i64>()
        .all(db)
        .await?;
    if ids.is_empty() {
        return Ok(0);
    }
    let count = ids.len() as u64;
    retire_on(
        db,
        scope().add(reply::Column::Id.is_in(ids)),
        "source_expired",
        now_ms,
    )
    .await?;
    Ok(count)
}
