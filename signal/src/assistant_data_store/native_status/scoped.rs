//! Optional native-state recording at the original writer's final phase. Scope
//! metadata is not an execution, source-read or model-send authorization.
use super::*;
use crate::{
    config::{ConfigConnection, connection::DatabaseTransaction},
    entity::agent_session,
};
use desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicy;
use desk_diagnose_core::session::AgentSessionSurface;
use sea_orm::{DbErr, TransactionTrait};

pub(crate) struct NativeStatusScope {
    actor: String,
    native_device: String,
    root: String,
    device_ref: String,
    instance: String,
    pub(crate) policy: AssistantDataPolicy,
}

/// Acquire before the caller's root/task/work locks. Existing subject locks
/// can be held already; this neither changes nor substitutes their decisions.
pub(crate) async fn scope_on(
    db: &DatabaseTransaction,
    actor: &str,
    native_device: &str,
    root: &str,
) -> Result<Option<Box<NativeStatusScope>>, DbErr> {
    if actor != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Ok(None);
    }
    let config = db.config_read().await;
    let instance = config.metadata.instance.clone();
    let device_ref = desk_diagnose_core::native_ids::stable(
        desk_diagnose_core::native_ids::Kind::AssistantDeviceBinding,
        &(&instance, &native_device),
    );
    let policy = config.assistant_data_policy.clone();
    Ok(Some(Box::new(NativeStatusScope {
        actor: actor.into(),
        native_device: native_device.into(),
        root: root.into(),
        device_ref,
        instance,
        policy,
    })))
}

pub(crate) struct PreparedObservation<'a> {
    scope: &'a NativeStatusScope,
    original: Box<agent_session::Model>,
    session: Box<PersistedAgentSession>,
    status: Box<PreparedNativeStatus>,
}
impl NativeStatusScope {
    pub(crate) async fn prepare_on<'a>(
        &'a self,
        db: &DatabaseTransaction,
    ) -> Result<Option<Box<PreparedObservation<'a>>>, AssistantStoreError> {
        let Some(original) = self.root_on(db).await? else {
            return Ok(None);
        };
        let session = Box::new(
            PersistedAgentSession::decode_json(&original.state_json)
                .map_err(|_| AssistantStoreError::Inaccessible)?,
        );
        if session.actor_id != self.actor
            || session.device_id != self.native_device
            || session.conversation_id != self.root
            || session.surface != AgentSessionSurface::AiAssistant
            || !session.agent_role.is_main()
        {
            return Ok(None);
        }
        let Some(data) = session
            .assistant_data
            .as_ref()
            .filter(|data| !data.temporary)
        else {
            return Ok(None);
        };
        data.validate().map_err(AssistantStoreError::Invalid)?;
        if data.source_device_ref != self.device_ref
            || data.source_instance_id.as_deref() != Some(self.instance.as_str())
            || !super::super::source_lifecycle::conversation_readable_on(
                db,
                &self.actor,
                &self.root,
                &self.root,
            )
            .await?
            || super::super::memory::source_device_on(db, &self.actor, &self.device_ref)
                .await?
                .is_none()
        {
            return Ok(None);
        }
        let status = super::prepare_on(db, &session, self.policy.revision).await?;
        let capture = status
            .projection
            .capture(data.current_actor_input_seq, None);
        if capture
            .native
            .as_ref()
            .and_then(|native| native.status_snapshot.as_ref())
            .is_some_and(|snapshot| snapshot.items.is_empty())
        {
            return Ok(None);
        }
        let now = unsigned(chrono::Utc::now().timestamp_millis())?;
        if capture
            .recorded_at_unix_ms
            .saturating_add(u64::from(self.policy.history_retention_days) * 86_400_000)
            <= now
        {
            return Ok(None);
        }
        use crate::entity::agent_assistant_origin as origin;
        if origin::Entity::find()
            .filter(origin::Column::ActorId.eq(&self.actor))
            .filter(origin::Column::OriginId.eq(status.projection.origin_id()))
            .one(db)
            .await?
            .is_some()
        {
            return Ok(None);
        }
        Ok(Some(Box::new(PreparedObservation {
            scope: self,
            original: Box::new(original),
            session,
            status,
        })))
    }

    /// Observe while the original root and native rows still exist. Drop their
    /// bodies before GC; the final phase retains only this last-known metadata.
    pub(crate) async fn prepare_reclaim_on(
        self: Box<Self>,
        db: &DatabaseTransaction,
    ) -> Result<Option<Box<ReclaimObservation>>, AssistantStoreError> {
        let Some(prepared) = self.prepare_on(db).await? else {
            return Ok(None);
        };
        prepared.validate_on(db).await?;
        let projection = prepared.status.projection;
        let actor_seq = prepared
            .session
            .assistant_data
            .as_ref()
            .unwrap()
            .current_actor_input_seq;
        Ok(Some(Box::new(ReclaimObservation {
            scope: self,
            projection,
            actor_seq,
        })))
    }

    async fn root_on(
        &self,
        db: &DatabaseTransaction,
    ) -> Result<Option<agent_session::Model>, AssistantStoreError> {
        Ok(agent_session::Entity::find()
            .filter(agent_session::Column::ActorId.eq(&self.actor))
            .filter(agent_session::Column::DeviceId.eq(&self.native_device))
            .filter(agent_session::Column::ConversationId.eq(&self.root))
            .one(db)
            .await?)
    }

    async fn capture_inner(&self, db: &DatabaseTransaction) -> Result<(), AssistantStoreError> {
        let Some(prepared) = self.prepare_on(db).await? else {
            return Ok(());
        };
        let capture = prepared.capture();
        let captures = [&capture];
        let pending =
            super::super::history::staged::prepare_many_on(db, &captures, &self.policy).await?;
        prepared.validate_on(db).await?;
        pending.finalize_on().await?;
        Ok(())
    }

    /// All original native mutations must precede this optional final phase.
    /// Savepoints keep a history outage from discarding a real native result.
    pub(crate) async fn capture_on(&self, db: &DatabaseTransaction) -> Result<(), DbErr> {
        let optional = db.begin().await?;
        let result = desk_diagnose_core::future::boxed(|| self.capture_inner(&optional)).await;
        self.finish_capture_on(db, optional, result).await
    }

    async fn finish_capture_on(
        &self,
        db: &DatabaseTransaction,
        optional: DatabaseTransaction,
        result: Result<(), AssistantStoreError>,
    ) -> Result<(), DbErr> {
        match result {
            Ok(()) => optional.commit().await,
            Err(error) => {
                optional.rollback().await?;
                let reason = match error {
                    AssistantStoreError::Capacity => "omitted_capacity",
                    AssistantStoreError::Backend(_) => "omitted_error",
                    _ => "omitted_policy",
                };
                let gap = db.begin().await?;
                let now = u64::try_from(chrono::Utc::now().timestamp_millis())
                    .map_err(|_| DbErr::Custom("invalid native history observation time".into()))?;
                match super::super::history::record_scope_gap_on(
                    &gap,
                    &self.actor,
                    now,
                    &self.policy,
                    reason,
                )
                .await
                {
                    Ok(()) => gap.commit().await?,
                    Err(_) => {
                        gap.rollback().await?;
                        log::warn!("native state retained without optional history or coverage");
                    }
                }
                Ok(())
            }
        }
    }
}
impl PreparedObservation<'_> {
    pub(crate) fn capture(&self) -> desk_diagnose_core::ai_assistant::source::HistoryCapture<'_> {
        self.status.projection.capture(
            self.session
                .assistant_data
                .as_ref()
                .unwrap()
                .current_actor_input_seq,
            Some(
                desk_diagnose_core::ai_assistant::source::HistorySourceDirectory {
                    instance_id: &self.scope.instance,
                    audience: &self.scope.native_device,
                    device_name: None,
                },
            ),
        )
    }
    pub(crate) async fn validate_on(
        &self,
        db: &DatabaseTransaction,
    ) -> Result<(), AssistantStoreError> {
        self.status.validate_on(db, &self.session).await?;
        if self.scope.root_on(db).await?.as_ref() != Some(self.original.as_ref())
            || super::super::memory::source_device_on(db, &self.scope.actor, &self.scope.device_ref)
                .await?
                .is_none()
            || !super::super::source_lifecycle::conversation_readable_on(
                db,
                &self.scope.actor,
                &self.scope.root,
                &self.scope.root,
            )
            .await?
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        Ok(())
    }
}

/// A snapshot observed and fully reselected before the original GC removes rows.
/// It is always historical; root absence is checked again before final quota.
pub(crate) struct ReclaimObservation {
    scope: Box<NativeStatusScope>,
    projection: desk_diagnose_core::ai_assistant::native_history::NativeStatusHistoryProjection,
    actor_seq: u64,
}
impl ReclaimObservation {
    fn capture(&self) -> desk_diagnose_core::ai_assistant::source::HistoryCapture<'_> {
        self.projection.capture(
            self.actor_seq,
            Some(
                desk_diagnose_core::ai_assistant::source::HistorySourceDirectory {
                    instance_id: &self.scope.instance,
                    audience: &self.scope.native_device,
                    device_name: None,
                },
            ),
        )
    }
    async fn validate_removed_on(
        &self,
        db: &DatabaseTransaction,
    ) -> Result<(), AssistantStoreError> {
        if self.scope.root_on(db).await?.is_some()
            || super::super::memory::source_device_on(db, &self.scope.actor, &self.scope.device_ref)
                .await?
                .is_none()
            || !super::super::source_lifecycle::conversation_readable_on(
                db,
                &self.scope.actor,
                &self.scope.root,
                &self.scope.root,
            )
            .await?
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        Ok(())
    }
    async fn capture_many_inner(
        observations: &[Box<Self>],
        db: &DatabaseTransaction,
    ) -> Result<(), AssistantStoreError> {
        let first = &observations[0];
        for observation in observations {
            if observation.scope.actor != first.scope.actor
                || observation.scope.policy != first.scope.policy
            {
                return Err(AssistantStoreError::Inaccessible);
            }
            observation.validate_removed_on(db).await?;
        }
        let captures = observations
            .iter()
            .map(|observation| observation.capture())
            .collect::<Vec<_>>();
        let references = captures.iter().collect::<Vec<_>>();
        let pending =
            super::super::history::staged::prepare_many_on(db, &references, &first.scope.policy)
                .await?;
        for observation in observations {
            observation.validate_removed_on(db).await?;
        }
        pending.finalize_on().await?;
        Ok(())
    }
    /// One SQLite cleanup batch belongs to the original single account. All
    /// source reads precede its one final quota update, including multi-root GC.
    pub(crate) async fn capture_many_on(
        observations: &[Box<Self>],
        db: &DatabaseTransaction,
    ) -> Result<(), DbErr> {
        let Some(first) = observations.first() else {
            return Ok(());
        };
        let optional = db.begin().await?;
        let result =
            desk_diagnose_core::future::boxed(|| Self::capture_many_inner(observations, &optional))
                .await;
        first.scope.finish_capture_on(db, optional, result).await
    }
}
