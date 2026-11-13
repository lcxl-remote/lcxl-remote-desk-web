//! Owner-scoped durable draft storage; transport authorization happens upstream.
use crate::config::connection::DatabaseConnection;
use crate::entity::agent_schedule as entity;
use desk_agent_protocol::schedule::ScheduleDraft;
#[cfg(test)]
use desk_assistant_data_store::entity_table::EntityTableExt;
use desk_diagnose_core::schedule::{
    SCHEDULE_CALC_VERSION, lifecycle::FailureState, normalize_draft,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set, sea_query::OnConflict};

mod history;
mod model_management;
mod proposal;
mod proposal_review;
pub(crate) use proposal_review::{decide_by_ai_on, review_proposal_on};
mod review_wait;
mod search;
pub use desk_assistant_data_store::schedule::RunHistoryPage;
mod fresh_admission;
mod fresh_children;
mod fresh_children_recovery;
mod fresh_children_settlement;
mod fresh_model;
mod fresh_recovery;
mod fresh_result;
mod fresh_session;
pub(crate) use desk_assistant_data_store::schedule::TaskReceiptContext;
mod approval_admission;
mod approval_claim;
mod approval_expiry;
mod approval_request;
mod continuation_wait;
mod directory_receipt;
mod fresh_permission_recovery;
pub(crate) use approval_admission::lock_fresh_approval_on;
pub(crate) use approval_request::{validate_task_directory_on, validate_task_permission_on};
pub use fresh_admission::{ClaimedFreshTask, FreshTaskClaim};
pub use fresh_model::FreshModelDispatch;
mod materializer;
pub use materializer::ScheduleScanReport;
pub(crate) mod budget;
pub(crate) use desk_assistant_data_store::schedule::CurrentDelegationSourceAuthority;
pub use desk_assistant_data_store::schedule::{
    CurrentTaskAuthority, TaskBudgetKind, TaskBudgetRequest,
};
mod cancel;
mod device_wait;
mod edit;
mod subject;
pub use resume_activation::ProposalDecider;
pub use subject::{SignalTaskSubjectVerifier, TaskSubjectVerifier};
mod queue;
mod resume_admission;
pub(crate) use resume_admission::{fresh_action_authority_on, lock_action_session};
mod continuation_candidates;
mod continuation_heartbeat;
mod resume_activation;
mod resume_claim;
mod resume_lease;
pub use continuation_heartbeat::ScheduleHeartbeat;
pub use desk_assistant_data_store::schedule::ScheduleStoreError;
use desk_assistant_data_store::schedule::{digest, json, key};
pub use resume_claim::{ClaimedContinuation, ContinuationClaim, ContinuationPermissionClaim};
pub use resume_lease::{ContinuationLease, FreshTaskLease};
mod resume_result;
mod settlement;

/// A conversation timer must run before its source session can be reclaimed.
/// Checked under the caller's transaction against the current window.
pub(super) async fn check_resume_retention(
    txn: &crate::config::connection::DatabaseTransaction,
    spec: &desk_agent_protocol::schedule::ScheduleSpec,
    now_ms: i64,
) -> Result<(), ScheduleStoreError> {
    let days = crate::usage_retention::load(txn).await?.agent_session_days;
    desk_diagnose_core::schedule::retention::conversation_resume_fits_retention(
        spec,
        now_ms,
        desk_diagnose_core::schedule::retention::retention_ms_from_days(days),
    )
    .map_err(|exceeded| ScheduleStoreError::ExceedsSessionRetention {
        max_delay_seconds: exceeded.max_delay_seconds,
    })
}

#[derive(Clone)]
pub struct ScheduleStore {
    db: DatabaseConnection,
}
impl ScheduleStore {
    pub(crate) async fn database_time(&self) -> Result<i64, ScheduleStoreError> {
        desk_assistant_data_store::schedule::database_now(&self.db).await
    }

    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Pausing scheduling never pretends to cancel an already dispatched action.
    pub async fn pause(
        &self,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
        now_ms: i64,
    ) -> Result<entity::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        desk_assistant_data_store::schedule::pause_on(
            &txn,
            owner,
            schedule_id,
            expected_revision,
            now_ms,
        )
        .await?;
        txn.commit().await?;
        self.read(owner, schedule_id).await
    }

    /// Test helper: insert a pending task row without enabling it.
    #[cfg(test)]
    pub async fn create_draft(
        &self,
        owner: i32,
        request: &ScheduleDraft,
        now_ms: i64,
    ) -> Result<entity::Model, ScheduleStoreError> {
        use sea_orm::TransactionTrait;
        let txn = self.db.begin().await?;
        let task = Self::create_draft_on(&txn, owner, request, now_ms, None).await?;
        txn.commit().await?;
        Ok(task)
    }

    pub(crate) async fn create_draft_on(
        db: &crate::config::connection::DatabaseTransaction,
        owner: i32,
        request: &ScheduleDraft,
        now_ms: i64,
        original: Option<&desk_diagnose_core::session::PersistedAgentSession>,
    ) -> Result<entity::Model, ScheduleStoreError> {
        if owner <= 0 || now_ms < 0 {
            return Err(ScheduleStoreError::Invalid);
        }
        let request = normalize_draft(request).map_err(|_| ScheduleStoreError::Invalid)?;
        // An AI fresh-task proposal waits in the turn that proposed it.
        let proposal_conversation_id = original
            .filter(|_| {
                request.kind == desk_agent_protocol::schedule::ScheduledTaskKind::FreshTask
                    && request.creation_source
                        == desk_agent_protocol::schedule::ScheduleCreationSource::AiProposal
            })
            .map(|session| session.conversation_id.clone());
        let creation_identity = digest(&json(&(owner, &request.client_create_key))?);
        let payload_hash = digest(&json(&request)?);
        // An idempotent retry returns the original consent, even after a later
        // input or settings change. It must not capture another episode.
        if let Some(existing) = entity::Entity::find()
            .filter(entity::Column::CreationIdentity.eq(&creation_identity))
            .filter(entity::Column::OwnerUserId.eq(owner))
            .one(db)
            .await?
        {
            return if existing.creation_payload_sha256 == payload_hash {
                Ok(existing)
            } else {
                Err(ScheduleStoreError::Conflict)
            };
        }
        let schedule_id = desk_diagnose_core::native_ids::from_bytes(
            desk_diagnose_core::native_ids::Kind::Schedule,
            uuid::Uuid::new_v4().as_bytes(),
        );
        let retrieval_consent = desk_diagnose_core::future::send_phase(|| {
            Self::capture_retrieval_consent_on(db, owner, &schedule_id, &request, original)
        })
        .await?;
        let prompt_actor = owner.to_string();
        let prompt_retention = if request.kind
            == desk_agent_protocol::schedule::ScheduledTaskKind::FreshTask
            && request.creation_source
                == desk_agent_protocol::schedule::ScheduleCreationSource::Manual
        {
            Some(
                desk_diagnose_core::future::send_phase(|| {
                    Self::owner_prompt_retention_on(db, &prompt_actor, &request.target_device_id)
                })
                .await?,
            )
        } else {
            None
        };
        let model = entity::ActiveModel {
            schedule_id: Set(schedule_id),
            owner_user_id: Set(owner),
            target_device_id: Set(request.target_device_id),
            kind: Set(json(&request.kind)?.trim_matches('"').to_string()),
            // Every task starts pending; creation enables it in the same
            // transaction unless an AI fresh-task proposal awaits a decision.
            status: Set("pending_review".into()),
            title: Set(request.title),
            prompt: Set(request.prompt),
            locale: Set(request.locale),
            model_id: Set(request.model_id),
            spec_json: Set(json(&request.spec)?),
            calc_version: Set(SCHEDULE_CALC_VERSION.into()),
            revision: Set(1),
            task_revision: Set(1),
            next_run_at: Set(None),
            recurrence_cursor_at: Set(None),
            grace_seconds: Set(3600),
            creation_source: Set(json(&request.creation_source)?
                .trim_matches('"')
                .to_string()),
            creation_identity: Set(creation_identity.clone()),
            creation_payload_sha256: Set(payload_hash.clone()),
            source_conversation_id: Set(request.source_conversation_id),
            requirement_revision: Set(request.requirement_revision.map(|r| r as i64)),
            retrieval_consent_json: Set(retrieval_consent),
            prompt_retention_json: Set(prompt_retention),
            proposal_conversation_id: Set(proposal_conversation_id),
            failure_state_json: Set(json(&FailureState::default())?),
            active_run_id: Set(None),
            created_at: Set(now_ms),
            updated_at: Set(now_ms),
            ..Default::default()
        };
        entity::Entity::insert(model)
            .on_conflict(
                OnConflict::column(entity::Column::CreationIdentity)
                    .do_nothing()
                    .to_owned(),
            )
            .exec_without_returning(db)
            .await?;
        let row = entity::Entity::find()
            .filter(entity::Column::CreationIdentity.eq(creation_identity))
            .filter(entity::Column::OwnerUserId.eq(owner))
            .one(db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if row.creation_payload_sha256 != payload_hash {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(row)
    }

    /// Capture only the original owner-bound source under the caller's root
    /// transaction. Neither the public draft nor current global settings can
    /// supply or enlarge this consent.
    async fn capture_retrieval_consent_on(
        db: &crate::config::connection::DatabaseTransaction,
        owner: i32,
        schedule_id: &str,
        request: &ScheduleDraft,
        original: Option<&desk_diagnose_core::session::PersistedAgentSession>,
    ) -> Result<Option<String>, ScheduleStoreError> {
        if owner != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID {
            return Err(ScheduleStoreError::NotFound);
        }
        use crate::entity::agent_session;
        use desk_diagnose_core::{
            ai_assistant::scheduled_retrieval::TaskRetrievalConsent, session::PersistedAgentSession,
        };
        let source = original
            .map(|source| source.conversation_id.as_str())
            .or(request.source_conversation_id.as_deref());
        let Some(source) = source else {
            return desk_diagnose_core::future::send_phase(|| {
                Self::capture_definition_consent_on(db, owner, schedule_id, request)
            })
            .await
            .map(Some)
            .map_err(|error| match error {
                crate::assistant_data_store::AssistantStoreError::Backend(error) => {
                    ScheduleStoreError::Backend(error)
                }
                crate::assistant_data_store::AssistantStoreError::Inaccessible => {
                    ScheduleStoreError::NotFound
                }
                _ => ScheduleStoreError::Invalid,
            });
        };

        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(source))
            .filter(agent_session::Column::ActorId.eq(owner.to_string()))
            .filter(agent_session::Column::DeviceId.is_in([request.target_device_id.as_str(), ""]))
            .one(db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let session = Box::new(
            PersistedAgentSession::decode_json(&row.state_json)
                .map_err(|_| ScheduleStoreError::Invalid)?,
        );
        if session.actor_id != owner.to_string()
            || session.device_id != row.device_id
            || session.conversation_id != source
            || session.version != row.version
            || i64::try_from(session.lease_token).ok() != Some(row.lease_token)
            || request
                .source_conversation_id
                .as_deref()
                .is_some_and(|id| id != source)
            || request
                .requirement_revision
                .is_some_and(|revision| revision != session.input_revision)
            || original.is_some_and(|expected| expected != session.as_ref())
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let target = session
            .admitted_assistant_request_targets()
            .and_then(|targets| {
                targets
                    .iter()
                    .find(|target| target.device_id == request.target_device_id)
            })
            .cloned()
            .ok_or(ScheduleStoreError::Invalid)?;
        // A child proposes with what its main conversation delegated: the frozen
        // read selection and owner data, never inputs of its own.
        let delegated = match session.agent_role.binding() {
            Some(binding) => {
                let run = crate::entity::agent_subagent_run::Entity::find()
                    .filter(crate::entity::agent_subagent_run::Column::TaskId.eq(&binding.task_id))
                    .filter(
                        crate::entity::agent_subagent_run::Column::ChildConversationId
                            .eq(&session.conversation_id),
                    )
                    .one(db)
                    .await?
                    .ok_or(ScheduleStoreError::NotFound)?;
                Some(
                    serde_json::from_str::<
                        desk_diagnose_core::subagent::creation::TaskCreationEnvelope,
                    >(&run.creation_envelope_json)
                    .map_err(|_| ScheduleStoreError::Invalid)?
                    .source,
                )
            }
            None => None,
        };
        let mut owner_input = session.clone();
        owner_input.scheduled_request = None;
        // No accepted input means no inherited read selection; it grants no resources.
        let mut original_context = if let Some(creation) = &delegated {
            creation.child_read_context()
        } else if session.latest_input_seq == 0 {
            None
        } else {
            crate::agent_run_event_store::input_context::original_on(db, &owner_input)
                .await
                .map_err(|_| ScheduleStoreError::Invalid)?
        };
        if let Some(context) = &mut original_context {
            context
                .object_attachments
                .retain(|object| object.device_id == target.device_id);
            context
                .live_targets
                .retain(|live| live.device_id == target.device_id);
            if session.device_id.is_empty() {
                let selected = context.clone();
                context.tool_names.retain(|name| {
                    desk_diagnose_core::web_research::is_web_tool(name)
                        || selected.is_selected_object_tool(name)
                });
            }
        }
        let captured_at = desk_assistant_data_store::schedule::authority_now(db).await?;
        let captured_at = captured_at
            .try_into()
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let consent = match &delegated {
            Some(creation) => TaskRetrievalConsent::capture_delegated(
                schedule_id,
                1,
                &session,
                creation,
                target,
                original_context,
                captured_at,
            ),
            None => TaskRetrievalConsent::capture(
                schedule_id,
                1,
                &session,
                target,
                original_context,
                captured_at,
            ),
        }
        .map_err(|_| ScheduleStoreError::Invalid)?
        .encode()
        .map_err(|_| ScheduleStoreError::Invalid)?;
        // All source writers use this row. Retain both the complete decode and
        // the exact original bytes; a transaction is not a single snapshot.
        let locked = agent_session::Entity::update_many()
            .set(agent_session::ActiveModel {
                version: Set(row.version),
                ..Default::default()
            })
            .filter(agent_session::Column::Id.eq(row.id))
            .filter(agent_session::Column::ConversationId.eq(source))
            .filter(agent_session::Column::ActorId.eq(&session.actor_id))
            .filter(agent_session::Column::DeviceId.eq(&session.device_id))
            .filter(agent_session::Column::Version.eq(row.version))
            .filter(agent_session::Column::LeaseToken.eq(row.lease_token))
            .filter(agent_session::Column::StateJson.eq(&row.state_json))
            .exec(db)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let current = agent_session::Entity::find_by_id(row.id)
            .one(db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let decoded = Box::new(
            PersistedAgentSession::decode_json(&current.state_json)
                .map_err(|_| ScheduleStoreError::Invalid)?,
        );
        if current != row || decoded != session {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(Some(consent))
    }

    async fn capture_definition_consent_on(
        db: &crate::config::connection::DatabaseTransaction,
        owner: i32,
        schedule_id: &str,
        request: &ScheduleDraft,
    ) -> Result<String, crate::assistant_data_store::AssistantStoreError> {
        use crate::{assistant_data_store as data, config::ConfigConnection};
        use desk_diagnose_core::ai_assistant::scheduled_retrieval::TaskRetrievalConsent;
        if owner != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID {
            return Err(data::AssistantStoreError::Inaccessible);
        }
        let actor = owner.to_string();
        data::ensure_runtime_on(db, &actor).await?;
        let config = db.config_read().await;
        let runtime = data::runtime_on(db, &actor).await?;
        let now = desk_assistant_data_store::schedule::authority_now(db)
            .await
            .map_err(|error| match error {
                ScheduleStoreError::Backend(error) => data::AssistantStoreError::Backend(error),
                _ => data::AssistantStoreError::Invalid("invalid task snapshot clock"),
            })?;
        let mut consent = TaskRetrievalConsent::capture_definition(
            schedule_id,
            1,
            &actor,
            &request.target_device_id,
            &config.assistant_user_instructions,
            request
                .time_confirmation
                .as_ref()
                .map_or("UTC", |confirmation| confirmation.input.timezone.as_str()),
            data::unsigned(now)?,
        )
        .map_err(data::AssistantStoreError::Invalid)?;
        let seed = consent.assistant_seed.as_mut().unwrap();
        seed.memory_state_generation = data::unsigned(runtime.memory_state_generation)?;
        seed.control_generation = data::unsigned(runtime.control_generation)?;
        consent.encode().map_err(data::AssistantStoreError::Invalid)
    }

    pub async fn read(
        &self,
        owner: i32,
        schedule_id: &str,
    ) -> Result<entity::Model, ScheduleStoreError> {
        desk_assistant_data_store::schedule::read(&self.db, owner, schedule_id).await
    }

    pub async fn list(
        &self,
        owner: i32,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<entity::Model>, ScheduleStoreError> {
        desk_assistant_data_store::schedule::list(&self.db, owner, after_id, limit).await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use sea_orm::TransactionTrait;

    #[tokio::test]
    async fn pause_preserves_active_work_and_blockers_and_rejects_stale_revision() {
        use desk_agent_protocol::schedule::SchedulePauseReason;
        let store = store().await;
        let row = store.create_draft(1, &draft(), 1000).await.unwrap();
        let mut failures = FailureState::default();
        failures
            .pause_reasons
            .insert(SchedulePauseReason::UnknownSideEffect);
        entity::Entity::update_many()
            .set(entity::ActiveModel {
                status: Set("active".into()),
                next_run_at: Set(Some(5000)),
                active_run_id: Set(Some("running-1".into())),
                failure_state_json: Set(json(&failures).unwrap()),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(row.id))
            .exec(&store.db)
            .await
            .unwrap();
        let paused = store.pause(1, &row.schedule_id, 1, 2000).await.unwrap();
        assert_eq!(paused.active_run_id.as_deref(), Some("running-1"));
        assert!(paused.next_run_at.is_none());
        let state: FailureState = serde_json::from_str(&paused.failure_state_json).unwrap();
        assert!(state.pause_reasons.contains(&SchedulePauseReason::User));
        assert!(
            state
                .pause_reasons
                .contains(&SchedulePauseReason::UnknownSideEffect)
        );
        assert!(matches!(
            store.pause(1, &row.schedule_id, 1, 3000).await,
            Err(ScheduleStoreError::Conflict)
        ));
        assert!(matches!(
            store.pause(2, &row.schedule_id, 2, 3000).await,
            Err(ScheduleStoreError::NotFound)
        ));
    }
    use desk_agent_protocol::schedule::{
        ScheduleCreationSource, ScheduleRule, ScheduleSpec, ScheduledTaskKind,
    };
    use sea_orm::{ConnectionTrait, Schema};
    pub(crate) fn draft() -> ScheduleDraft {
        ScheduleDraft {
            time_confirmation: None,
            client_create_key: "create-1".into(),
            kind: ScheduledTaskKind::FreshTask,
            target_device_id: "device-1".into(),
            title: "Report".into(),
            prompt: "Write a daily report".into(),
            locale: Some("en".into()),
            model_id: None,
            spec: ScheduleSpec {
                schema_version: 1,
                rule: ScheduleRule::Daily {
                    utc_time: "06:00:00".into(),
                },
            },
            source_conversation_id: None,
            requirement_revision: None,
            creation_source: ScheduleCreationSource::Manual,
        }
    }
    /// Storage-only fixtures do not represent current device readiness.
    pub(crate) struct AcceptSubject;
    #[async_trait::async_trait]
    impl TaskSubjectVerifier for AcceptSubject {
        async fn lock_subject(
            &self,
            _: &crate::config::connection::DatabaseTransaction,
            _: &entity::Model,
        ) -> Result<(), ScheduleStoreError> {
            Ok(())
        }
    }

    /// An enabled fresh task on `device`, created as `owner` would create it.
    /// With `source`, the task carries that session's retrieval consent.
    pub(crate) async fn enabled_fresh_task_on(
        db: crate::config::connection::DatabaseConnection,
        device: Option<&str>,
        owner: i32,
        source: Option<&desk_diagnose_core::session::PersistedAgentSession>,
    ) -> (ScheduleStore, entity::Model) {
        let schema = Schema::new(db.get_database_backend());
        db.execute(
            schema
                .entity_table(crate::entity::agent_assistant_runtime::Entity)
                .if_not_exists(),
        )
        .await
        .unwrap();
        for mut table in [
            schema.entity_table(entity::Entity),
            schema.entity_table(crate::entity::agent_schedule_run::Entity),
            schema.entity_table(crate::entity::agent_task_budget_reservation::Entity),
        ] {
            db.execute(table.if_not_exists()).await.unwrap();
        }
        for mut index in schema
            .create_index_from_entity(entity::Entity)
            .into_iter()
            .chain(schema.create_index_from_entity(crate::entity::agent_schedule_run::Entity))
            .chain(
                schema
                    .create_index_from_entity(crate::entity::agent_task_budget_reservation::Entity),
            )
        {
            db.execute(index.if_not_exists()).await.unwrap();
        }
        let store = ScheduleStore::new(db);
        let mut draft = draft();
        if let Some(device) = device {
            draft.target_device_id = device.into();
        }
        // Tests keep the earlier fixture's limits unless they pinned their own.
        let current = crate::schedule_budget_policy::read(&store.db)
            .await
            .unwrap();
        if current.revision == 0 {
            crate::schedule_budget_policy::update(
                &store.db,
                &desk_agent_protocol::schedule::policy::UpdateScheduleBudgetPolicy {
                    expected_revision: 0,
                    maximum: desk_agent_protocol::schedule::policy::TaskBudget {
                        max_runs_per_utc_day: 24,
                        max_calls_per_run: 10,
                        max_model_tokens_per_run: 10_000,
                        max_runtime_seconds: 300,
                    },
                },
            )
            .await
            .unwrap();
        }
        let task = match source {
            Some(source) => {
                let txn = crate::db::begin_write(&store.db, entity::Entity)
                    .await
                    .unwrap();
                let now = desk_assistant_data_store::schedule::authority_now(&txn)
                    .await
                    .unwrap();
                let pending =
                    ScheduleStore::create_draft_on(&txn, owner, &draft, now, Some(source))
                        .await
                        .unwrap();
                let task = ScheduleStore::activate_on(&txn, owner, &pending, false)
                    .await
                    .unwrap();
                txn.commit().await.unwrap();
                task
            }
            None => store.create_task(owner, &draft).await.unwrap(),
        };
        (store, task)
    }

    pub(super) async fn store() -> ScheduleStore {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.entity_table(crate::entity::agent_assistant_runtime::Entity))
            .await
            .unwrap();
        db.execute(&schema.entity_table(crate::entity::agent_schedule_run::Entity))
            .await
            .unwrap();
        db.execute(&schema.entity_table(crate::entity::agent_goal_run::Entity))
            .await
            .unwrap();
        db.execute(&schema.entity_table(entity::Entity))
            .await
            .unwrap();
        for index in schema.create_index_from_entity(entity::Entity) {
            db.execute(&index).await.unwrap();
        }
        ScheduleStore::new(db)
    }

    pub(super) async fn retrieval_source(
        store: &ScheduleStore,
        enabled: bool,
    ) -> desk_diagnose_core::session::PersistedAgentSession {
        use crate::entity::agent_session;
        use desk_agent_protocol::{
            AgentScope, ExecutionMode,
            ai_assistant::user_instructions::{AssistantDataSettings, UserInstructions},
        };
        use desk_diagnose_core::{
            ai_assistant::{data_control::AssistantDataSession, history::AssistantRetrievalBudget},
            session::{AgentSessionSurface, PersistedAgentSession},
        };
        use sea_orm::ActiveModelTrait;
        store
            .db
            .execute(
                &Schema::new(store.db.get_database_backend())
                    .entity_table(agent_session::Entity)
                    .if_not_exists()
                    .to_owned(),
            )
            .await
            .unwrap();
        let mut session = PersistedAgentSession::new(
            "source",
            "1",
            "device-1",
            1,
            AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-10-23T00:00:00Z",
        );
        session.surface = AgentSessionSurface::AiAssistant;
        session.input_revision = 1;
        session.bind_assistant_input_targets(&[]).unwrap();
        session.begin_focus_epoch(1, Vec::<String>::new()).unwrap();
        let settings = AssistantDataSettings {
            automatic_memory_enabled: enabled,
            model_history_search_enabled: enabled,
            ..Default::default()
        };
        session.assistant_data = Some(Box::new(AssistantDataSession {
            limits: Box::default(),
            temporary: false,
            source_device_ref: "original-device-public-id".into(),
            source_instance_id: None,
            current_actor_input_seq: 1,
            current_input_recorded_at_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
            current_input_timezone: "UTC".into(),
            instructions: desk_diagnose_core::ai_assistant::settings::freeze_instructions(
                &UserInstructions::default(),
                1,
                100,
            )
            .unwrap(),
            memory_state_generation: 1,
            control_generation: 1,
            current_memories: vec![],
            retrieval_budget: AssistantRetrievalBudget::new("original-episode".into()).unwrap(),
            retrieval_task_id: None,
            history_access_scope:
                desk_diagnose_core::ai_assistant::data_access::owner_history_scope(
                    "1",
                    "source",
                    "original-device-public-id",
                    1,
                    100,
                    "original-episode",
                    1,
                    &settings,
                    "UTC",
                    false,
                )
                .unwrap(),
            memory_access_scope:
                desk_diagnose_core::ai_assistant::memory_access::owner_memory_scope(
                    "1",
                    "source",
                    "original-device-public-id",
                    1,
                    100,
                    "original-episode",
                    1,
                    &settings,
                    false,
                )
                .unwrap(),
            history_dependencies: vec![],
            read_uses: vec![],
        }));
        agent_session::ActiveModel {
            conversation_id: Set(session.conversation_id.clone()),
            actor_id: Set(session.actor_id.clone()),
            device_id: Set(session.device_id.clone()),
            state_json: Set(session.encode_json_for_storage().unwrap()),
            version: Set(session.version),
            lease_token: Set(session.lease_token as i64),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&store.db)
        .await
        .unwrap();
        session
    }

    #[tokio::test]
    async fn scheduled_retrieval_consent_is_atomic_and_retries_keep_the_original_scope() {
        for enabled in [false, true] {
            let store = store().await;
            let session = retrieval_source(&store, enabled).await;
            exercise_retrieval_consent(store, session, enabled).await;
        }
    }

    async fn exercise_retrieval_consent(
        store: ScheduleStore,
        session: desk_diagnose_core::session::PersistedAgentSession,
        enabled: bool,
    ) {
        use crate::entity::agent_session;
        use desk_diagnose_core::ai_assistant::scheduled_retrieval::TaskRetrievalConsent;
        let original = agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        store.db.execute_unprepared("CREATE TRIGGER reject_task_consent BEFORE INSERT ON agent_schedule BEGIN SELECT RAISE(ABORT, 'injected'); END").await.unwrap();
        let input = draft();
        let txn = store.db.begin().await.unwrap();
        assert!(
            desk_diagnose_core::future::send_phase(|| ScheduleStore::create_draft_on(
                &txn,
                1,
                &input,
                1000,
                Some(&session)
            ))
            .await
            .is_err()
        );
        txn.rollback().await.unwrap();
        assert!(
            entity::Entity::find()
                .all(&store.db)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            agent_session::Entity::find_by_id(original.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            original
        );
        store
            .db
            .execute_unprepared("DROP TRIGGER reject_task_consent")
            .await
            .unwrap();
        let txn = store.db.begin().await.unwrap();
        let task = desk_diagnose_core::future::send_phase(|| {
            ScheduleStore::create_draft_on(&txn, 1, &input, 1000, Some(&session))
        })
        .await
        .unwrap();
        txn.commit().await.unwrap();
        let consent =
            TaskRetrievalConsent::decode(task.retrieval_consent_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            consent.source_device_ref.as_deref(),
            Some("original-device-public-id")
        );
        assert_eq!(consent.history_scope.is_some(), enabled);
        assert_eq!(consent.memory_scope.is_some(), enabled);
        assert_eq!(
            consent.source_conversation_id.as_deref(),
            Some(session.conversation_id.as_str())
        );
        assert_eq!(consent.source_input_revision, Some(1));
        assert!(
            consent.captured_at_unix_ms
                >= session
                    .assistant_data
                    .as_ref()
                    .unwrap()
                    .current_input_recorded_at_unix_ms
        );
        assert!(consent.captured_at_unix_ms <= chrono::Utc::now().timestamp_millis() as u64);
        // A retry after the source changed cannot replace the first consent.
        let mut later = session.clone();
        later.input_revision += 1;
        later.assistant_data = None;
        agent_session::Entity::update_many()
            .set(agent_session::ActiveModel {
                state_json: Set(later.encode_json_for_storage().unwrap()),
                ..Default::default()
            })
            .filter(agent_session::Column::Id.eq(original.id))
            .exec(&store.db)
            .await
            .unwrap();
        let txn = store.db.begin().await.unwrap();
        let retry = desk_diagnose_core::future::send_phase(|| {
            ScheduleStore::create_draft_on(&txn, 1, &input, 2000, Some(&later))
        })
        .await
        .unwrap();
        txn.commit().await.unwrap();
        assert_eq!(retry, task);
        // Only an enabled task can be edited.
        let task = store
            .activate_pending_for_test(1, &task.schedule_id, task.revision)
            .await
            .unwrap();
        let edited = store
            .change_prompt(1, &task.schedule_id, task.revision, "A revised report")
            .await
            .unwrap();
        let mut revised = consent;
        revised.task_revision += 1;
        assert_eq!(
            desk_assistant_data_store::schedule::retrieval_consent(&edited).unwrap(),
            Some(revised)
        );
    }

    #[test]
    fn scheduled_retrieval_consent_fits_small_stacks() {
        const STACK: &str = "LRD_TEST_SCHEDULE_CONSENT_STACK_BYTES";
        if let Some(bytes) = std::env::var_os(STACK) {
            let bytes: usize = bytes.to_str().unwrap().parse().unwrap();
            assert!([1024 * 1024, 2 * 1024 * 1024].contains(&bytes));
            actix_web::rt::System::new().block_on(async {
                let mut cases = Vec::new();
                for enabled in [false, true] {
                    let store = store().await;
                    let source = retrieval_source(&store, enabled).await;
                    cases.push((store, source, enabled));
                }
                let (done, ready) = tokio::sync::oneshot::channel();
                let thread = std::thread::Builder::new()
                    .name("schedule-consent-stack".into())
                    .stack_size(bytes)
                    .spawn(move || {
                        actix_web::rt::System::new().block_on(async move {
                            eprintln!("scheduled retrieval consent running on {bytes} byte thread");
                            for (store, source, enabled) in cases {
                                desk_diagnose_core::future::boxed(|| {
                                    exercise_retrieval_consent(store, source, enabled)
                                })
                                .await;
                            }
                            let _ = done.send(());
                        });
                    })
                    .unwrap();
                ready.await.unwrap();
                thread.join().unwrap();
            });
            return;
        }
        for bytes in [1024 * 1024, 2 * 1024 * 1024] {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "scheduled_retrieval_consent_fits_small_stacks",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(STACK, bytes.to_string())
                .env_remove("RUST_MIN_STACK")
                .status()
                .unwrap();
            assert!(
                status.success(),
                "scheduled consent failed on {bytes} bytes: {status}"
            );
        }
    }

    #[tokio::test]
    async fn scheduled_retrieval_consent_rejects_spoofed_sources_without_creating_tasks() {
        let store = store().await;
        let original = retrieval_source(&store, true).await;
        let input = draft();
        for change in ["actor", "device", "version", "surface"] {
            let mut altered = original.clone();
            match change {
                "actor" => altered.actor_id = "2".into(),
                "device" => altered.device_id = "other-device".into(),
                "version" => altered.version += 1,
                _ => {
                    altered.surface =
                        desk_diagnose_core::session::AgentSessionSurface::TerminalAiAssistant
                }
            }
            let txn = store.db.begin().await.unwrap();
            assert!(
                desk_diagnose_core::future::send_phase(|| ScheduleStore::create_draft_on(
                    &txn,
                    1,
                    &input,
                    1000,
                    Some(&altered)
                ))
                .await
                .is_err(),
                "{change}"
            );
            txn.rollback().await.unwrap();
        }
        assert!(
            entity::Entity::find()
                .all(&store.db)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn create_is_idempotent_but_changed_payload_conflicts() {
        let store = store().await;
        let input = draft();
        let first = store.create_draft(1, &input, 1000).await.unwrap();
        let second = store.create_draft(1, &input, 2000).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(first.status, "pending_review");
        assert!(first.next_run_at.is_none());
        let mut changed = input.clone();
        changed.prompt = "Different requirement".into();
        assert!(matches!(
            store.create_draft(1, &changed, 3000).await,
            Err(ScheduleStoreError::Conflict)
        ));
        assert_eq!(store.list(1, 0, 100).await.unwrap().len(), 1);
    }
    #[tokio::test]
    async fn ownership_and_client_keys_are_isolated_and_pagination_is_bounded() {
        let store = store().await;
        let first = store.create_draft(1, &draft(), 1000).await.unwrap();
        assert!(matches!(
            store.create_draft(2, &draft(), 1000).await,
            Err(ScheduleStoreError::NotFound)
        ));
        assert!(matches!(
            store.read(2, &first.schedule_id).await,
            Err(ScheduleStoreError::NotFound)
        ));
        assert!(store.list(2, 0, 100).await.unwrap().is_empty());
        assert!(store.list(1, first.id, 100).await.unwrap().is_empty());
        assert!(matches!(
            store.list(1, 0, 101).await,
            Err(ScheduleStoreError::Invalid)
        ));
    }
}

mod late_receipts;

mod outcome_review;
pub(crate) use outcome_review::reviewed_at as outcome_reviewed_at;

mod cancel_actions;
