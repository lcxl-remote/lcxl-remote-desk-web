//! Shared generation revocation after an authenticated native conversation deletion.
//! The host retains its original tombstone SELECT, identity checks and deletion CAS.

use crate::entity::agent_assistant_runtime as runtime;
use crate::{AssistantStoreError, increment};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, sea_query::Expr};

pub async fn revoke_deleted_conversation_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
) -> Result<(), AssistantStoreError> {
    // An ordinary conversation may never have initialized assistant data.
    // Native removal still succeeds without creating new assistant rows.
    let Some(state) = runtime::Entity::find()
        .filter(runtime::Column::ActorId.eq(actor))
        .one(db)
        .await?
    else {
        return Ok(());
    };
    let mut update = runtime::Entity::update_many()
        .col_expr(
            runtime::Column::InputSeq,
            Expr::value(increment(state.input_seq)?),
        )
        .col_expr(
            runtime::Column::HistoryDataGeneration,
            Expr::value(increment(state.history_data_generation)?),
        )
        .col_expr(
            runtime::Column::MemoryStateGeneration,
            Expr::value(increment(state.memory_state_generation)?),
        )
        .col_expr(
            runtime::Column::ControlGeneration,
            Expr::value(increment(state.control_generation)?),
        )
        .col_expr(
            runtime::Column::Revision,
            Expr::value(increment(state.revision)?),
        )
        .filter(runtime::Column::Id.eq(state.id))
        .filter(runtime::Column::ActorId.eq(actor))
        .filter(runtime::Column::Revision.eq(state.revision))
        .filter(runtime::Column::InputSeq.eq(state.input_seq))
        .filter(runtime::Column::HistoryDataGeneration.eq(state.history_data_generation))
        .filter(runtime::Column::MemoryStateGeneration.eq(state.memory_state_generation))
        .filter(runtime::Column::ControlGeneration.eq(state.control_generation))
        .filter(runtime::Column::LearningCutoffInputSeq.eq(state.learning_cutoff_input_seq))
        .filter(runtime::Column::SettingsRevisionSeen.eq(state.settings_revision_seen));
    update = match &state.settings_instance_id {
        Some(instance) => update.filter(runtime::Column::SettingsInstanceId.eq(instance)),
        None => update.filter(runtime::Column::SettingsInstanceId.is_null()),
    };
    if update.exec(db).await?.rows_affected != 1 {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}
