use super::*;
use crate::config::connection::DatabaseTransaction;
use desk_assistant_data_store::main_request::coordination::{self, Batch};
use desk_diagnose_core::{
    chat::ModelTurn,
    seam::{ModelRequest, NullTurnSink},
};
use sea_orm::sea_query::Expr;

pub(super) async fn close_unclaimed(
    db: &DatabaseConnection,
    head: &agent_main_request::Model,
    device: &str,
    reason: MainRequestFailure,
) -> Result<(), AgentError> {
    let txn = crate::db::begin_write(db, agent_session::Entity)
        .await
        .map_err(storage)?;
    let Some((row, mut session)) = batch_session(&txn, head, device).await? else {
        return Ok(());
    };
    if main_request::fail_unclaimed(&txn, head, &mut session, chrono::Utc::now(), reason)
        .await
        .map_err(|error| error.into_agent_error())?
    {
        store_session(&txn, &row, &mut session).await?;
    }
    txn.commit().await.map_err(storage)
}

pub(crate) async fn fail_destination(
    db: &DatabaseConnection,
    actor: &str,
    conversation: &str,
    request: &str,
) -> Result<(), AgentError> {
    let Some(row) = main_request::load(db, actor, conversation, request)
        .await
        .map_err(|error| error.into_agent_error())?
    else {
        return Ok(());
    };
    if row.pending_input_json.is_none() {
        return Ok(());
    }
    if row.input_revision.is_none() {
        return main_request::fail_unactivated(
            db,
            &row,
            chrono::Utc::now().timestamp_millis(),
            MainRequestFailure::DestinationChanged,
        )
        .await
        .map_err(|error| error.into_agent_error());
    }
    let frozen = FrozenOwnerInput::decode(&row).map_err(|error| error.into_agent_error())?;
    close_unclaimed(
        db,
        &row,
        &frozen.device_id,
        MainRequestFailure::DestinationChanged,
    )
    .await
}

impl SignalMainRequestExecutor {
    pub(super) async fn coordinate(
        &self,
        head: &agent_main_request::Model,
        device: &str,
        owner: i32,
    ) -> Result<bool, AgentError> {
        let Some((batch, request)) = self.reserve_batch(head, device).await? else {
            return Ok(false);
        };
        let cancel = cancellation::register(owner, &batch.id);
        let result = tokio::time::timeout(
            Duration::from_secs(coordination::MODEL_TIMEOUT_SECS),
            self.call_batch(head, &batch, request, (*cancel).clone()),
        )
        .await
        .ok()
        .and_then(Result::ok);
        self.finish_batch(head, device, &batch, result.as_ref())
            .await?;
        Ok(true)
    }

    async fn reserve_batch(
        &self,
        head: &agent_main_request::Model,
        device: &str,
    ) -> Result<Option<(Batch, ModelRequest)>, AgentError> {
        let txn = crate::db::begin_write(&self.db, agent_session::Entity)
            .await
            .map_err(storage)?;
        let Some((row, mut session)) = batch_session(&txn, head, device).await? else {
            return Ok(None);
        };
        if agent_run_event_store::main_requests::slot_held(&txn, &session).await? {
            return Ok(None);
        }
        let Some(batch) = coordination::reserve(
            &txn,
            &mut session,
            head,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .map_err(|error| error.into_agent_error())?
        else {
            return Ok(None);
        };
        store_session(&txn, &row, &mut session).await?;
        txn.commit().await.map_err(storage)?;
        Ok(Some(batch))
    }

    async fn finish_batch(
        &self,
        head: &agent_main_request::Model,
        device: &str,
        batch: &Batch,
        turn: Option<&ModelTurn>,
    ) -> Result<(), AgentError> {
        let txn = crate::db::begin_write(&self.db, agent_session::Entity)
            .await
            .map_err(storage)?;
        let Some((row, mut session)) = batch_session(&txn, head, device).await? else {
            return Ok(());
        };
        let now_ms = chrono::Utc::now().timestamp_millis();
        if !batch.is_current(&session, now_ms) {
            return Ok(());
        }
        if self.gate.is_enabled()
            && !agent_run_event_store::main_requests::slot_held(&txn, &session).await?
        {
            coordination::settle(&txn, batch, turn, now_ms)
                .await
                .map_err(|error| error.into_agent_error())?;
        }
        session.main_coordination = None;
        store_session(&txn, &row, &mut session).await?;
        txn.commit().await.map_err(storage)
    }

    async fn call_batch(
        &self,
        head: &agent_main_request::Model,
        batch: &Batch,
        request: ModelRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ModelTurn, AgentError> {
        let config = crate::model_provider::load(&self.db)
            .await
            .map_err(storage)?;
        if config.destination_identity().map_err(|_| unavailable())? != batch.destination {
            return Err(unavailable());
        }
        let snapshot = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&head.conversation_id))
            .filter(agent_session::Column::ActorId.eq(&head.actor_id))
            .one(&self.db)
            .await
            .map_err(storage)?
            .ok_or_else(unavailable)?;
        let session = PersistedAgentSession::decode_boxed_json(&snapshot.state_json)
            .map_err(|_| unavailable())?;
        if !self.gate.is_enabled()
            || !batch.is_current(&session, chrono::Utc::now().timestamp_millis())
        {
            return Err(unavailable());
        }
        let inner = SignalModelSeam::from_config(&config)?
            .with_context_db(self.db.clone())
            .with_cancellation(cancel);
        let model = MeteredModel {
            fresh_task: None,
            inner,
            db: self.db.clone(),
            destination: batch.destination.clone(),
            selected_source_tools: Default::default(),
            export_authorization_id: batch.id.clone(),
            permission_resume: false,
            completed_compression_receipt: std::cell::RefCell::new(None),
            model_call_ordinal: std::sync::atomic::AtomicU64::new(0),
        };
        model.call(request, &mut NullTurnSink).await
    }
}

async fn batch_session(
    txn: &DatabaseTransaction,
    head: &agent_main_request::Model,
    device: &str,
) -> Result<Option<(agent_session::Model, Box<PersistedAgentSession>)>, AgentError> {
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&head.conversation_id))
        .filter(agent_session::Column::ActorId.eq(&head.actor_id))
        .filter(agent_session::Column::DeviceId.eq(device))
        .one(txn)
        .await
        .map_err(storage)?;
    row.map(|row| {
        let session =
            PersistedAgentSession::decode_boxed_json(&row.state_json).map_err(|_| unavailable())?;
        if session.version != row.version
            || session.lease_token as i64 != row.lease_token
            || session.check_subject(&head.actor_id, device).is_err()
        {
            return Err(unavailable());
        }
        Ok((row, session))
    })
    .transpose()
}

async fn store_session(
    txn: &DatabaseTransaction,
    row: &agent_session::Model,
    session: &mut PersistedAgentSession,
) -> Result<(), AgentError> {
    use agent_session::Column;
    session.version = row.version.checked_add(1).ok_or_else(unavailable)?;
    let changed = agent_session::Entity::update_many()
        .col_expr(Column::Version, Expr::value(session.version))
        .col_expr(Column::LeaseToken, Expr::value(session.lease_token as i64))
        .col_expr(
            Column::StateJson,
            Expr::value(
                session
                    .encode_json_for_storage()
                    .map_err(|_| unavailable())?,
            ),
        )
        .filter(Column::Id.eq(row.id))
        .filter(Column::Version.eq(row.version))
        .filter(Column::LeaseToken.eq(row.lease_token))
        .filter(Column::StateJson.eq(&row.state_json))
        .exec(txn)
        .await
        .map_err(storage)?;
    if changed.rows_affected != 1 {
        return Err(unavailable());
    }
    Ok(())
}

fn unavailable() -> AgentError {
    transport_error("Queued assistant coordination is unavailable")
}

fn storage(_: sea_orm::DbErr) -> AgentError {
    unavailable()
}
