//! Persist a fresh context under the same transaction as the occurrence claim.
use super::{CurrentTaskAuthority, ScheduleStore, ScheduleStoreError, entity};
use crate::config::connection::DatabaseTransaction;
use crate::entity::{agent_schedule_run, agent_session};
use desk_agent_protocol::AgentScope;
use desk_diagnose_core::{
    schedule::fresh_session::{FreshSessionInput, initial_session},
    session::PersistedAgentSession,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};

impl ScheduleStore {
    pub(super) async fn owner_prompt_retention_on(
        db: &DatabaseTransaction,
        actor: &str,
        device: &str,
    ) -> Result<String, ScheduleStoreError> {
        let policy = {
            use crate::config::ConfigConnection;
            db.config_read().await.assistant_data_policy.clone()
        };
        let proof = desk_diagnose_core::schedule::published_input::TaskPromptRetention::owner(
            actor,
            device,
            policy.revision,
        );
        proof
            .validate(actor, device)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        super::json(&proof)
    }

    /// The original proposal has already passed its owner/session/tool fence.
    /// Persist its complete original copy restrictions before publishing a task.
    pub(crate) async fn record_prompt_retention_on(
        db: &DatabaseTransaction,
        task: &mut entity::Model,
        session: &PersistedAgentSession,
        parent: &desk_diagnose_core::chat::ChatMessage,
    ) -> Result<(), ScheduleStoreError> {
        if task.kind != "fresh_task"
            || task.prompt_retention_json.is_some()
            || !desk_diagnose_core::ai_assistant::data_access::eligible_origin(session)
        {
            return Ok(());
        }
        use crate::assistant_data_store as data;
        let result: Result<Option<String>, data::AssistantStoreError> = async {
            let Some(label) = &parent.data_envelope else { return Ok(None); };
            let Some(archive) = data::find_output_on(db, &session.conversation_id, label).await? else { return Ok(None); };
            if archive.proof.source_set.actor_id != session.actor_id
                || session.assistant_data.as_ref().is_none_or(|state| archive.proof.source_set.source_device_ref != state.source_device_ref)
                || task.owner_user_id.to_string() != session.actor_id || !session.admits_assistant_device(&task.target_device_id) {
                return Err(data::AssistantStoreError::Inaccessible);
            }
            archive.proof.bind_visible_message(parent).map_err(data::AssistantStoreError::Invalid)?;
            let Some(retained) = data::retained_native_sources(&archive.proof) else { return Ok(None); };
            let mut source_policy = retained.policy;
            source_policy.independent_fact_allowed = false;
            source_policy.policy_sha256 = desk_diagnose_core::ai_assistant::source::policy_hash(&source_policy)
                .map_err(data::AssistantStoreError::Invalid)?;
            let dependencies = Vec::new();
            let proof = desk_diagnose_core::schedule::published_input::TaskPromptRetention {
                actor_id: session.actor_id.clone(), target_device_id: task.target_device_id.clone(), policy: source_policy, dependencies,
                model_source: Some(desk_diagnose_core::ai_assistant::source_registry::NativeDerivedInputPolicy::from_archive(&archive.proof)
                    .map_err(data::AssistantStoreError::Invalid)?),
            };
            proof.validate(&session.actor_id, &task.target_device_id).map_err(data::AssistantStoreError::Invalid)?;
            Ok(Some(serde_json::to_string(&proof).map_err(|_| data::AssistantStoreError::Invalid("invalid task prompt proof"))?))
        }.await;
        let proof = result.map_err(|error| match error {
            data::AssistantStoreError::Backend(error) => ScheduleStoreError::Backend(error),
            _ => ScheduleStoreError::Conflict,
        })?;
        let Some(proof) = proof else {
            return Ok(());
        };
        let changed = entity::Entity::update_many()
            .set(entity::ActiveModel {
                prompt_retention_json: Set(Some(proof.clone())),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::OwnerUserId.eq(task.owner_user_id))
            .filter(entity::Column::Revision.eq(task.revision))
            .filter(entity::Column::Prompt.eq(&task.prompt))
            .filter(entity::Column::PromptRetentionJson.is_null())
            .exec(db)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        task.prompt_retention_json = Some(proof);
        Ok(())
    }

    /// Historical source proof, not a new planner/action authorization. The
    /// occurrence's immutable snapshot survives edits of the current definition.
    pub(crate) async fn published_input_retention_on(
        db: &DatabaseTransaction,
        actor: &str,
        device_ref: &str,
        conversation: &str,
        source: &desk_agent_protocol::data_lineage::DataEnvelope,
    ) -> Result<
        Option<desk_diagnose_core::schedule::published_input::TaskPromptRetention>,
        ScheduleStoreError,
    > {
        use desk_diagnose_core::schedule::published_input::{
            TaskPromptRetention, model_bound_published_input,
        };
        let Some(work) = agent_schedule_run::Entity::find()
            .filter(agent_schedule_run::Column::RunId.eq(conversation))
            .filter(
                agent_schedule_run::Column::OwnerUserId.eq(actor
                    .parse::<i32>()
                    .map_err(|_| ScheduleStoreError::Invalid)?),
            )
            .one(db)
            .await?
        else {
            return Ok(None);
        };
        let task: entity::Model = serde_json::from_str(&work.task_snapshot_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if work.conversation_id != conversation
            || work.schedule_id != task.schedule_id
            || work.owner_user_id != task.owner_user_id
            || task.owner_user_id.to_string() != actor
            || task.kind != "fresh_task"
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let expected_ref = {
            use crate::config::ConfigConnection;
            let config = db.config_read().await;
            desk_diagnose_core::native_ids::stable(
                desk_diagnose_core::native_ids::Kind::AssistantDeviceBinding,
                &(&config.metadata.instance, &task.target_device_id),
            )
        };
        let consent = Self::retrieval_consent(&task)?.ok_or(ScheduleStoreError::Invalid)?;
        if consent
            .source_device_ref
            .as_deref()
            .unwrap_or(&expected_ref)
            != device_ref
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let device = task.target_device_id.as_str();
        let Some(raw) = task.prompt_retention_json.as_deref() else {
            return Ok(None);
        };
        if raw.len() > 64 * 1024 {
            return Err(ScheduleStoreError::Invalid);
        }
        let proof: TaskPromptRetention =
            serde_json::from_str(raw).map_err(|_| ScheduleStoreError::Invalid)?;
        proof
            .validate(actor, device)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let (_, contract) = super::publication::load_contract(
            db,
            task.owner_user_id,
            &task.schedule_id,
            task.contract_revision.ok_or(ScheduleStoreError::Conflict)?,
        )
        .await?;
        if i64::try_from(contract.contract().task_revision).ok() != Some(task.task_revision)
            || contract.contract().target_device_id != device
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let Some(destination) = source.allowed_destinations.first() else {
            return Ok(None);
        };
        let expected =
            model_bound_published_input(&work.run_id, &task.prompt, &contract, destination.clone())
                .map_err(|_| ScheduleStoreError::Invalid)?;
        if expected.data_envelope.as_ref() != Some(source) {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(Some(proof))
    }

    /// The caller resolves current subject/device policy and commits only after
    /// all admission checks pass. This writes no grants and starts no model.
    pub fn insert_fresh_session_on<'a>(
        txn: &'a DatabaseTransaction,
        authority: &'a CurrentTaskAuthority,
        policy_revision: i64,
        scope: AgentScope,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<PersistedAgentSession, ScheduleStoreError>>
                + Send
                + 'a,
        >,
    > {
        desk_diagnose_core::future::send_phase(|| {
            Self::insert_fresh_session_inner(txn, authority, policy_revision, scope)
        })
    }

    async fn insert_fresh_session_inner(
        txn: &DatabaseTransaction,
        authority: &CurrentTaskAuthority,
        policy_revision: i64,
        scope: AgentScope,
    ) -> Result<PersistedAgentSession, ScheduleStoreError> {
        let work = authority.run();
        let node = work
            .lease_owner
            .as_deref()
            .ok_or(ScheduleStoreError::Conflict)?;
        let current = Box::new(
            desk_diagnose_core::future::send_phase(|| {
                Self::lock_run_authority(
                    txn,
                    work.owner_user_id,
                    &authority.contract().contract().target_device_id,
                    &work.run_id,
                    node,
                    work.lease_epoch,
                )
            })
            .await?,
        );
        if current.provenance() != authority.provenance()
            || current.run() != work
            || work.attempt != 1
            || work.lease_epoch != 1
            || work.conversation_id != work.run_id
            || work.turn_id != format!("{}-turn", work.run_id)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        if agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&work.conversation_id))
            .one(txn)
            .await?
            .is_some()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        super::budget::reserve_run_budget(txn, &current).await?;
        let task: entity::Model = serde_json::from_str(&work.task_snapshot_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let now = chrono::DateTime::from_timestamp_millis(current.verified_at())
            .ok_or(ScheduleStoreError::Invalid)?;
        let deadline = chrono::DateTime::from_timestamp_millis(
            work.lease_deadline.ok_or(ScheduleStoreError::Conflict)?,
        )
        .ok_or(ScheduleStoreError::Invalid)?;
        let mut session = Box::new(
            initial_session(
                current.contract(),
                FreshSessionInput {
                    run_id: &work.run_id,
                    actor_id: &work.owner_user_id.to_string(),
                    prompt: &task.prompt,
                    locale: task.locale.as_deref(),
                    policy_revision,
                    scope,
                    now: &now.to_rfc3339(),
                },
            )
            .map_err(|_| ScheduleStoreError::Conflict)?,
        );
        desk_diagnose_core::future::send_phase(|| {
            Self::initialize_fresh_assistant_on(txn, &current, &task, &mut session)
        })
        .await?;
        session.version = 1;
        agent_session::Entity::insert(agent_session::ActiveModel {
            conversation_id: Set(session.conversation_id.clone()),
            actor_id: Set(session.actor_id.clone()),
            device_id: Set(session.device_id.clone()),
            conversation_schema_version: Set(Some(i32::from(
                desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
            ))),
            state_json: Set(session
                .encode_json_for_storage()
                .map_err(|_| ScheduleStoreError::Invalid)?),
            version: Set(session.version),
            lease_token: Set(session.lease_token as i64),
            lease_deadline: Set(Some(deadline)),
            created_at: Set(now),
            updated_at: Set(now),

            ..Default::default()
        })
        .exec_without_returning(txn)
        .await?;
        // Inserting can wait on a uniqueness lock; recheck expiry before returning.
        let after = Box::new(
            desk_diagnose_core::future::send_phase(|| {
                Self::lock_run_authority(
                    txn,
                    work.owner_user_id,
                    &session.device_id,
                    &work.run_id,
                    node,
                    work.lease_epoch,
                )
            })
            .await?,
        );
        if after.provenance() != current.provenance()
            || agent_schedule_run::Entity::find_by_id(work.id)
                .one(txn)
                .await?
                .as_ref()
                != Some(work)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(*session)
    }

    async fn initialize_fresh_assistant_on(
        txn: &DatabaseTransaction,
        authority: &CurrentTaskAuthority,
        task: &entity::Model,
        session: &mut PersistedAgentSession,
    ) -> Result<(), ScheduleStoreError> {
        use crate::config::ConfigConnection;
        use desk_diagnose_core::{
            ai_assistant::scheduled_retrieval::{RetrievalOccurrence, ScheduledRetrievalKind},
            native_ids::{self, Kind},
        };
        let consent = Self::retrieval_consent(task)?.ok_or(ScheduleStoreError::Conflict)?;
        let Some(_) = &consent.assistant_seed else {
            return Ok(());
        };
        let config = txn.config_read().await;
        let device_ref = native_ids::stable(
            Kind::AssistantDeviceBinding,
            &(&config.metadata.instance, &session.device_id),
        );
        let deadline = authority
            .delegation_source()?
            .deadline_ms()
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let occurrence = RetrievalOccurrence {
            schedule_id: &task.schedule_id,
            task_revision: task
                .task_revision
                .try_into()
                .map_err(|_| ScheduleStoreError::Invalid)?,
            run_id: &authority.run().run_id,
            kind: ScheduledRetrievalKind::FreshTask,
            source_conversation_id: consent.source_conversation_id.as_deref(),
            root_conversation_id: &authority.run().conversation_id,
            actor_id: &authority.run().owner_user_id.to_string(),
            device_id: &task.target_device_id,
            source_device_ref: consent.source_device_ref.as_deref().unwrap_or(&device_ref),
            source_instance_id: if consent.source_device_ref.is_some() {
                consent.source_instance_id.as_deref()
            } else {
                Some(&config.metadata.instance)
            },
            started_at_unix_ms: authority
                .run()
                .started_at
                .ok_or(ScheduleStoreError::Conflict)?
                .try_into()
                .map_err(|_| ScheduleStoreError::Invalid)?,
            original_deadline_unix_ms: Some(
                deadline
                    .try_into()
                    .map_err(|_| ScheduleStoreError::Invalid)?,
            ),
        };
        let limits = config.assistant_data_policy.limits.as_ref();
        consent
            .bind_fresh_task(session, &occurrence, limits)
            .map_err(|_| ScheduleStoreError::Conflict)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::publication::tests::{Verifier, fixture_on};
    use super::*;
    use crate::entity::{agent_task_authorization, agent_task_budget_reservation as budget_row};
    use desk_agent_protocol::ExecutionMode;
    use sea_orm::{ConnectionTrait, Schema, TransactionTrait};

    #[tokio::test]
    async fn fresh_occurrence_uses_frozen_source_context_without_copying_old_history_or_budget() {
        let (store, task, source, queued) = source_fixture().await;
        exercise_source_context(store, task, source, queued).await;
    }

    #[tokio::test]
    async fn selectable_fresh_occurrence_preserves_root_data_and_its_frozen_execution_device() {
        let (store, task, source, queued) = source_fixture_for(true).await;
        assert!(source.device_id.is_empty());
        assert!(
            source
                .assistant_data
                .as_ref()
                .unwrap()
                .source_device_ref
                .is_empty()
        );
        exercise_source_context(store, task, source, queued).await;
    }

    pub(crate) async fn source_fixture() -> (
        ScheduleStore,
        entity::Model,
        PersistedAgentSession,
        agent_schedule_run::Model,
    ) {
        source_fixture_for(false).await
    }

    async fn source_fixture_for(
        selectable: bool,
    ) -> (
        ScheduleStore,
        entity::Model,
        PersistedAgentSession,
        agent_schedule_run::Model,
    ) {
        use crate::config::ConfigConnection;
        use desk_agent_protocol::ai_assistant::user_instructions::UserInstructions;
        use sea_orm::ActiveModelTrait;
        let db = crate::assistant_data_store::tests::recovery_database().await;
        let store = ScheduleStore::new(db.clone());
        let mut source = super::super::tests::retrieval_source(&store, true).await;
        source.device_id = "device-1".into();
        source.input_revision += 1;
        source
            .begin_focus_epoch(source.input_revision, Vec::<String>::new())
            .unwrap();
        let device_ref = desk_diagnose_core::native_ids::stable(
            desk_diagnose_core::native_ids::Kind::AssistantDeviceBinding,
            &(
                &store.db.config_read().await.metadata.instance,
                &source.device_id,
            ),
        );
        crate::entity::agent_assistant_device_binding::ActiveModel {
            binding_id: Set(device_ref.clone()),
            instance_id: Set(store.db.config_read().await.metadata.instance.clone()),
            audience: Set(source.device_id.clone()),
            device_name: Set(Some("Original source device".into())),
            first_seen_at_ms: Set(100),
            last_seen_at_ms: Set(100),
            ..Default::default()
        }
        .insert(&store.db)
        .await
        .unwrap();
        if selectable {
            source.device_id.clear();
            source
                .bind_assistant_input_targets(&[
                    desk_diagnose_core::dynamic_run::AssistantInputTarget {
                        reference: device_ref.clone(),
                        device_id: "device-1".into(),
                        member_generation: None,
                    },
                ])
                .unwrap();
        }
        if !selectable {
            source.bind_assistant_input_targets(&[]).unwrap();
        }
        let data = source.assistant_data.as_mut().unwrap();
        data.source_device_ref = if selectable {
            String::new()
        } else {
            device_ref.clone()
        };
        data.source_instance_id = Some(store.db.config_read().await.metadata.instance.clone());
        data.history_access_scope
            .as_mut()
            .unwrap()
            .current_device_ref = data.source_device_ref.clone();
        data.memory_access_scope
            .as_mut()
            .unwrap()
            .current_device_ref = data.source_device_ref.clone();
        if selectable {
            data.history_access_scope = None;
            let memory = data.memory_access_scope.as_mut().unwrap();
            memory.device_rule =
                desk_agent_protocol::ai_assistant::history::AssistantDeviceScope::CurrentDevice;
            memory.request_device_refs = vec![device_ref.clone()];
        }
        data.instructions = desk_diagnose_core::ai_assistant::settings::freeze_instructions(
            &UserInstructions {
                revision: 7,
                content: "SOURCE_FROZEN_PERSONALIZATION".into(),
            },
            1,
            100,
        )
        .unwrap();
        data.retrieval_budget
            .reserve(
                "original-spent-call",
                &"b".repeat(64),
                desk_diagnose_core::ai_assistant::history::RetrievalCallKind::Relevance,
                4096,
                100,
            )
            .unwrap();
        source
            .conversation
            .push(desk_diagnose_core::chat::ChatMessage::text(
                "old-source-user",
                desk_diagnose_core::chat::ChatRole::User,
                "OLD_SOURCE_HISTORY_MUST_NOT_COPY",
            ));
        let row = agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let mut row: agent_session::ActiveModel = row.into();
        row.device_id = Set(source.device_id.clone());
        row.state_json = Set(source.encode_json_for_storage().unwrap());
        row.update(&store.db).await.unwrap();

        let (store, task, _, publication) =
            super::super::publication::tests::fixture_for_requirement(
                db,
                Some("device-1"),
                1,
                Some(&source),
            )
            .await;
        store
            .publish_task(1, &publication, &Verifier(true))
            .await
            .unwrap();
        let queued = store
            .enqueue_manual(1, &task.schedule_id, "source-seeded-fresh")
            .await
            .unwrap();
        (store, task, source, queued)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn published_input_uses_original_snapshot_and_fits_small_stacks() {
        for stack_bytes in [1024 * 1024, 2 * 1024 * 1024] {
            let (store, task, source, queued) = source_fixture().await;
            let contract = desk_diagnose_core::schedule::contract::parse_contract(
                &store
                    .read_contract(1, &task.schedule_id, task.contract_revision.unwrap())
                    .await
                    .unwrap()
                    .canonical_json,
            )
            .unwrap();
            let destination = desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "published-prompt".into(),
                connection_revision: 1,
                model_id: "fixture".into(),
                profile_revision: 1,
            };
            let message =
                desk_diagnose_core::schedule::published_input::model_bound_published_input(
                    &queued.run_id,
                    &task.prompt,
                    &contract,
                    destination,
                )
                .unwrap();
            let original = task.prompt_retention_json.clone().unwrap();
            let data = source.assistant_data.as_ref().unwrap();
            let device_ref = data.source_device_ref.clone();
            let current_revision = store.read(1, &task.schedule_id).await.unwrap().revision;
            store
                .change_prompt(
                    1,
                    &task.schedule_id,
                    current_revision,
                    "new independent owner requirement",
                )
                .await
                .unwrap();
            let current = store.read(1, &task.schedule_id).await.unwrap();
            // The retention record carries no prompt digest; the run snapshot
            // still binds the prompt text and task revision directly.
            assert_eq!(
                current.prompt_retention_json.as_deref(),
                Some(original.as_str())
            );
            assert_ne!(current.prompt, task.prompt);
            assert_ne!(current.task_revision, task.task_revision);
            let worker = std::thread::Builder::new()
                .name("published-input-small-stack".into())
                .stack_size(stack_bytes)
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    runtime.block_on(async move {
                        let txn = store.db.begin().await.unwrap();
                        let label = message.data_envelope.unwrap();
                        let proof = desk_diagnose_core::future::send_phase(|| {
                            ScheduleStore::published_input_retention_on(
                                &txn,
                                "1",
                                &device_ref,
                                &queued.run_id,
                                &label,
                            )
                        })
                        .await
                        .unwrap()
                        .unwrap();
                        assert_eq!(serde_json::to_string(&proof).unwrap(), original);
                        assert!(!proof.policy.independent_fact_allowed);
                        assert!(
                            ScheduleStore::published_input_retention_on(
                                &txn,
                                "2",
                                &device_ref,
                                &queued.run_id,
                                &label
                            )
                            .await
                            .unwrap()
                            .is_none()
                        );
                        let mut forged = label.clone();
                        forged.provenance.source_object_id = Some("another-run:input".into());
                        assert!(
                            ScheduleStore::published_input_retention_on(
                                &txn,
                                "1",
                                &device_ref,
                                &queued.run_id,
                                &forged
                            )
                            .await
                            .is_err()
                        );
                        txn.rollback().await.unwrap();
                    });
                })
                .unwrap();
            tokio::task::spawn_blocking(move || worker.join().unwrap())
                .await
                .unwrap();
        }
    }

    async fn exercise_source_context(
        store: ScheduleStore,
        task: entity::Model,
        source: PersistedAgentSession,
        queued: agent_schedule_run::Model,
    ) {
        let txn = store.db.begin().await.unwrap();
        let work = ScheduleStore::claim_queued_on(&txn, &queued.run_id, "node", 90)
            .await
            .unwrap();
        let authority = ScheduleStore::lock_run_authority(
            &txn,
            1,
            &task.target_device_id,
            &work.run_id,
            "node",
            work.lease_epoch,
        )
        .await
        .unwrap();
        let scope = AgentScope {
            granted: vec![],
            mode: ExecutionMode::SuggestOnly,
            expires_at: None,
            policy_name: None,
        };
        let session = ScheduleStore::insert_fresh_session_on(&txn, &authority, 1, scope)
            .await
            .unwrap();
        let data = session.assistant_data.as_ref().unwrap();
        assert_eq!(
            data.instructions,
            source.assistant_data.as_ref().unwrap().instructions
        );
        assert_eq!(data.retrieval_budget.relevance_calls, 0);
        assert_ne!(
            data.retrieval_budget.episode_id,
            source
                .assistant_data
                .as_ref()
                .unwrap()
                .retrieval_budget
                .episode_id
        );
        assert_eq!(
            data.retrieval_task_id.as_deref(),
            Some(work.run_id.as_str())
        );
        assert!(
            data.current_memories.is_empty()
                && data.read_uses.is_empty()
                && data.history_dependencies.is_empty()
        );
        assert_eq!(session.conversation.len(), 1);
        assert!(
            !session
                .encode_json_for_storage()
                .unwrap()
                .contains("OLD_SOURCE_HISTORY_MUST_NOT_COPY")
        );
        assert_eq!(
            data.memory_access_scope
                .as_ref()
                .unwrap()
                .expires_at_unix_ms,
            Some(
                authority
                    .delegation_source()
                    .unwrap()
                    .deadline_ms()
                    .unwrap() as u64
            )
        );
        txn.commit().await.unwrap();
        let stored = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&work.run_id))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            PersistedAgentSession::decode_json(&stored.state_json).unwrap(),
            session
        );
    }

    #[test]
    fn fresh_occurrence_context_fits_small_stacks() {
        const STACK: &str = "LRD_TEST_FRESH_ASSISTANT_STACK_BYTES";
        if let Some(bytes) = std::env::var_os(STACK) {
            let bytes: usize = bytes.to_str().unwrap().parse().unwrap();
            assert!([1024 * 1024, 2 * 1024 * 1024].contains(&bytes));
            actix_web::rt::System::new().block_on(async {
                let (store, task, source, queued) = source_fixture().await;
                let (done, ready) = tokio::sync::oneshot::channel();
                let thread = std::thread::Builder::new()
                    .name("fresh-assistant-stack".into())
                    .stack_size(bytes)
                    .spawn(move || {
                        actix_web::rt::System::new().block_on(async move {
                            eprintln!("fresh assistant occurrence running on {bytes} byte thread");
                            Box::pin(exercise_source_context(store, task, source, queued)).await;
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
            assert!(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "fresh_occurrence_context_fits_small_stacks",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env(STACK, bytes.to_string())
                    .env_remove("RUST_MIN_STACK")
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }

    #[tokio::test]
    async fn fresh_session_and_claim_commit_or_rollback_as_one_unit() {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        let (store, task, _, publication) = fixture_on(db).await;
        store
            .db
            .execute(
                &Schema::new(store.db.get_database_backend())
                    .create_table_from_entity(agent_session::Entity),
            )
            .await
            .unwrap();
        store
            .publish_task(1, &publication, &Verifier(true))
            .await
            .unwrap();
        let queued = store
            .enqueue_manual(1, &task.schedule_id, "fresh-session")
            .await
            .unwrap();
        let before = store.read(1, &task.schedule_id).await.unwrap();
        let scope = || AgentScope {
            granted: vec![],
            mode: ExecutionMode::SuggestOnly,
            expires_at: None,
            policy_name: None,
        };
        for revoked in [true, false] {
            let txn = store.db.begin().await.unwrap();
            let work = ScheduleStore::claim_queued_on(&txn, &queued.run_id, "fresh-node", 90)
                .await
                .unwrap();
            let authority = ScheduleStore::lock_run_authority(
                &txn,
                1,
                &task.target_device_id,
                &work.run_id,
                "fresh-node",
                work.lease_epoch,
            )
            .await
            .unwrap();
            let delegation = authority.delegation_source().unwrap();
            assert_eq!(&delegation.provenance, authority.provenance());
            assert_eq!(delegation.started_at_ms, work.started_at.unwrap());
            assert_eq!(
                delegation.validate().unwrap().digest(),
                authority.contract().digest()
            );
            let original_deadline = work.started_at.unwrap()
                + i64::from(authority.contract().contract().budget.max_runtime_seconds) * 1_000;
            assert_eq!(
                delegation.deadline_ms().unwrap(),
                original_deadline.min(delegation.authorization_expires_at_ms.unwrap_or(i64::MAX))
            );
            assert!(delegation.deadline_ms().unwrap() >= authority.valid_until());
            if revoked {
                agent_task_authorization::Entity::update_many()
                    .set(agent_task_authorization::ActiveModel {
                        revoked_at: Set(Some(authority.verified_at())),
                        ..Default::default()
                    })
                    .filter(agent_task_authorization::Column::ScheduleId.eq(&task.schedule_id))
                    .exec(&txn)
                    .await
                    .unwrap();
                assert!(
                    ScheduleStore::insert_fresh_session_on(&txn, &authority, 1, scope())
                        .await
                        .is_err()
                );
            } else {
                let session = ScheduleStore::insert_fresh_session_on(&txn, &authority, 1, scope())
                    .await
                    .unwrap();
                assert_eq!(session.conversation.len(), 1);
                assert_eq!(session.conversation_id, work.run_id);
                assert!(session.permission_requests.is_empty());
                assert!(
                    ScheduleStore::insert_fresh_session_on(&txn, &authority, 1, scope())
                        .await
                        .is_err()
                );
            }
            txn.rollback().await.unwrap();
            assert!(
                budget_row::Entity::find()
                    .all(&store.db)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before);
            assert_eq!(
                agent_schedule_run::Entity::find_by_id(queued.id)
                    .one(&store.db)
                    .await
                    .unwrap()
                    .unwrap(),
                queued
            );
            assert!(
                agent_session::Entity::find()
                    .all(&store.db)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        let txn = store.db.begin().await.unwrap();
        let work = ScheduleStore::claim_queued_on(&txn, &queued.run_id, "fresh-node", 90)
            .await
            .unwrap();
        let authority = ScheduleStore::lock_run_authority(
            &txn,
            1,
            &task.target_device_id,
            &work.run_id,
            "fresh-node",
            work.lease_epoch,
        )
        .await
        .unwrap();
        let session = ScheduleStore::insert_fresh_session_on(&txn, &authority, 1, scope())
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let rows = agent_session::Entity::find().all(&store.db).await.unwrap();
        assert_eq!(rows.len(), 1);
        let reservations = budget_row::Entity::find().all(&store.db).await.unwrap();
        assert_eq!(reservations.len(), 1);
        assert_eq!(reservations[0].kind, "run");
        assert_eq!(reservations[0].run_id, work.run_id);
        assert_eq!(reservations[0].reserved_units, 1);
        assert_eq!(
            PersistedAgentSession::decode_json(&rows[0].state_json)
                .unwrap()
                .conversation_id,
            session.conversation_id
        );
        assert_eq!(
            agent_schedule_run::Entity::find_by_id(queued.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            work
        );
        // Fresh executions renew both leases together without rewriting loop state.
        let lease = || super::super::FreshTaskLease {
            owner: 1,
            run_id: &work.run_id,
            node_id: "fresh-node",
            run_epoch: work.lease_epoch,
            session_token: session.lease_token,
        };
        assert!(store.renew_fresh_task(lease(), 300).await.unwrap());
        let renewed_session = agent_session::Entity::find_by_id(rows[0].id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let renewed_run = agent_schedule_run::Entity::find_by_id(work.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(renewed_session.version, rows[0].version);
        assert_eq!(renewed_session.state_json, rows[0].state_json);
        assert!(renewed_session.lease_deadline > rows[0].lease_deadline);
        assert!(renewed_run.lease_deadline > work.lease_deadline);
        assert!(
            !store
                .renew_conversation_resume(
                    super::super::ContinuationLease {
                        owner: 1,
                        run_id: &work.run_id,
                        node_id: "fresh-node",
                        run_epoch: work.lease_epoch,
                        session_token: session.lease_token,
                    },
                    300
                )
                .await
                .unwrap()
        );
        for case in ["session_expired", "run_expired", "cancelled", "revoked"] {
            match case {
                "session_expired" => {
                    agent_session::Entity::update_many()
                        .set(agent_session::ActiveModel {
                            lease_deadline: Set(Some(
                                chrono::DateTime::from_timestamp_millis(1).unwrap(),
                            )),
                            ..Default::default()
                        })
                        .exec(&store.db)
                        .await
                        .unwrap();
                }
                "run_expired" => {
                    agent_schedule_run::Entity::update_many()
                        .set(agent_schedule_run::ActiveModel {
                            lease_deadline: Set(Some(1)),
                            ..Default::default()
                        })
                        .exec(&store.db)
                        .await
                        .unwrap();
                }
                "cancelled" => {
                    agent_schedule_run::Entity::update_many()
                        .set(agent_schedule_run::ActiveModel {
                            cancel_requested_at: Set(Some(1)),
                            ..Default::default()
                        })
                        .exec(&store.db)
                        .await
                        .unwrap();
                }
                "revoked" => {
                    agent_task_authorization::Entity::update_many()
                        .set(agent_task_authorization::ActiveModel {
                            revoked_at: Set(Some(1)),
                            ..Default::default()
                        })
                        .exec(&store.db)
                        .await
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let before_task = store.read(1, &task.schedule_id).await.unwrap();
            let before_session = agent_session::Entity::find_by_id(rows[0].id)
                .one(&store.db)
                .await
                .unwrap();
            let before_run = agent_schedule_run::Entity::find_by_id(work.id)
                .one(&store.db)
                .await
                .unwrap();
            assert!(
                !matches!(store.renew_fresh_task(lease(), 300).await, Ok(true)),
                "{case}"
            );
            assert_eq!(
                store.read(1, &task.schedule_id).await.unwrap(),
                before_task,
                "{case}"
            );
            assert_eq!(
                agent_session::Entity::find_by_id(rows[0].id)
                    .one(&store.db)
                    .await
                    .unwrap(),
                before_session,
                "{case}"
            );
            assert_eq!(
                agent_schedule_run::Entity::find_by_id(work.id)
                    .one(&store.db)
                    .await
                    .unwrap(),
                before_run,
                "{case}"
            );
            agent_session::Entity::update_many()
                .set(agent_session::ActiveModel {
                    lease_deadline: Set(renewed_session.lease_deadline),
                    ..Default::default()
                })
                .exec(&store.db)
                .await
                .unwrap();
            agent_schedule_run::Entity::update_many()
                .set(agent_schedule_run::ActiveModel {
                    lease_deadline: Set(renewed_run.lease_deadline),
                    cancel_requested_at: Set(None),
                    ..Default::default()
                })
                .exec(&store.db)
                .await
                .unwrap();
            agent_task_authorization::Entity::update_many()
                .set(agent_task_authorization::ActiveModel {
                    revoked_at: Set(None),
                    ..Default::default()
                })
                .exec(&store.db)
                .await
                .unwrap();
        }
        use desk_diagnose_core::seam::LeaseHeartbeat;
        let cancel = tokio_util::sync::CancellationToken::new();
        let heartbeat =
            super::super::ScheduleHeartbeat::new_fresh(store.clone(), lease(), 300, cancel.clone())
                .await
                .unwrap();
        assert!(heartbeat.check_current().await);
        agent_schedule_run::Entity::update_many()
            .set(agent_schedule_run::ActiveModel {
                cancel_requested_at: Set(Some(1)),
                ..Default::default()
            })
            .exec(&store.db)
            .await
            .unwrap();
        assert!(!heartbeat.check_current().await);
        assert!(cancel.is_cancelled());
        agent_schedule_run::Entity::update_many()
            .set(agent_schedule_run::ActiveModel {
                cancel_requested_at: Set(None),
                ..Default::default()
            })
            .exec(&store.db)
            .await
            .unwrap();
        assert!(
            !heartbeat.check_current().await,
            "lost authority must remain terminal"
        );
        let cancel = tokio_util::sync::CancellationToken::new();
        let heartbeat =
            super::super::ScheduleHeartbeat::new_fresh(store.clone(), lease(), 300, cancel.clone())
                .await
                .unwrap();
        let guard = heartbeat.start("another-conversation".into(), session.lease_token);
        assert!(!heartbeat.is_healthy());
        assert!(cancel.is_cancelled());
        drop(guard);
        let renewed_session = agent_session::Entity::find_by_id(rows[0].id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let renewed_run = agent_schedule_run::Entity::find_by_id(work.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        // An error after the session update must not leave only one lease renewed.
        store.db.execute_unprepared("CREATE TRIGGER reject_fresh_renewal BEFORE UPDATE OF lease_deadline ON agent_schedule_run BEGIN SELECT RAISE(ABORT, 'injected'); END").await.unwrap();
        let before_task = store.read(1, &task.schedule_id).await.unwrap();
        assert!(store.renew_fresh_task(lease(), 300).await.is_err());
        assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before_task);
        assert_eq!(
            agent_session::Entity::find_by_id(rows[0].id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            renewed_session
        );
        assert_eq!(
            agent_schedule_run::Entity::find_by_id(work.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            renewed_run
        );
        let model_budget = super::super::TaskBudgetRequest {
            owner: 1,
            device: &task.target_device_id,
            run_id: &work.run_id,
            node: "fresh-node",
            lease_epoch: work.lease_epoch,
            kind: super::super::TaskBudgetKind::ModelTokens,
            rule_id: None,
            logical_key: "model-step-1",
            input_sha256: &"a".repeat(64),
            units: 10,
        };
        for change in ["version", "token", "message", "origin", "actor", "approval"] {
            let mut altered = session.clone();
            match change {
                "version" => altered.version += 1,
                "token" => altered.lease_token += 1,
                "message" => altered.conversation[0].text.push_str(" modified"),
                "origin" => {
                    altered.trigger_origin = desk_diagnose_core::session::TriggerOrigin::User
                }
                "actor" => altered.actor_id = "2".into(),
                "approval" => {
                    altered.turn_state = desk_diagnose_core::session::TurnState::AwaitingApproval
                }
                _ => unreachable!(),
            }
            let before = store.read(1, &task.schedule_id).await.unwrap();
            let txn = store.db.begin().await.unwrap();
            assert!(
                ScheduleStore::reserve_fresh_model_budget_on(&txn, &altered, &model_budget)
                    .await
                    .is_err(),
                "{change}"
            );
            txn.rollback().await.unwrap();
            assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before);
            assert_eq!(
                budget_row::Entity::find()
                    .all(&store.db)
                    .await
                    .unwrap()
                    .len(),
                1
            );
        }
        let txn = store.db.begin().await.unwrap();
        let reserved = ScheduleStore::reserve_fresh_model_budget_on(&txn, &session, &model_budget)
            .await
            .unwrap();
        assert_eq!(reserved.kind, "model_tokens");
        assert_eq!(reserved.reserved_units, 10);
        txn.rollback().await.unwrap();
        assert_eq!(
            budget_row::Entity::find()
                .all(&store.db)
                .await
                .unwrap()
                .len(),
            1
        );
        let txn = store.db.begin().await.unwrap();
        ScheduleStore::reserve_fresh_model_budget_on(&txn, &session, &model_budget)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        assert_eq!(
            budget_row::Entity::find()
                .all(&store.db)
                .await
                .unwrap()
                .len(),
            2
        );
        use crate::entity::model_egress_receipt;
        store
            .db
            .execute(
                &Schema::new(store.db.get_database_backend())
                    .create_table_from_entity(model_egress_receipt::Entity),
            )
            .await
            .unwrap();
        let dispatch_request = super::super::TaskBudgetRequest {
            logical_key: "model-step-2",
            ..model_budget
        };
        let audit = desk_diagnose_core::sink_authorizer::SinkProjectionAudit {
            destination: desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "oss-ai-gateway:1".into(),
                connection_revision: 1,
                model_id: "test-model".into(),
                profile_revision: 1,
            },
            envelope_ids: vec!["original-input".into()],
            total_bytes: 10,
        };
        store.db.execute_unprepared("CREATE TRIGGER reject_model_intent BEFORE INSERT ON model_egress_receipt BEGIN SELECT RAISE(ABORT, 'injected'); END").await.unwrap();
        let before = store.read(1, &task.schedule_id).await.unwrap();
        let budgets = budget_row::Entity::find().all(&store.db).await.unwrap();
        let txn = store.db.begin().await.unwrap();
        assert!(
            ScheduleStore::reserve_fresh_model_dispatch_on(
                &txn,
                &session,
                &dispatch_request,
                "export-task",
                2,
                None,
                &desk_diagnose_core::model_egress::NativeModelInputSnapshot::capture(
                    &audit,
                    &crate::model_egress_store::test_inputs(&audit)
                )
                .unwrap(),
            )
            .await
            .is_err()
        );
        txn.rollback().await.unwrap();
        assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before);
        assert_eq!(
            budget_row::Entity::find().all(&store.db).await.unwrap(),
            budgets
        );
        assert!(
            model_egress_receipt::Entity::find()
                .all(&store.db)
                .await
                .unwrap()
                .is_empty()
        );
        store
            .db
            .execute_unprepared("DROP TRIGGER reject_model_intent")
            .await
            .unwrap();
        let txn = store.db.begin().await.unwrap();
        let dispatch = ScheduleStore::reserve_fresh_model_dispatch_on(
            &txn,
            &session,
            &dispatch_request,
            "export-task",
            2,
            None,
            &desk_diagnose_core::model_egress::NativeModelInputSnapshot::capture(
                &audit,
                &crate::model_egress_store::test_inputs(&audit),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            dispatch.receipt.receipt_id,
            format!("task-model-{}", dispatch.budget.reservation_id)
        );
        assert_eq!(dispatch.budget.reserved_units, 10);
        txn.commit().await.unwrap();
        assert_eq!(
            budget_row::Entity::find()
                .all(&store.db)
                .await
                .unwrap()
                .len(),
            3
        );
        let before = store.read(1, &task.schedule_id).await.unwrap();
        let txn = store.db.begin().await.unwrap();
        assert!(
            ScheduleStore::reserve_fresh_model_dispatch_on(
                &txn,
                &session,
                &dispatch_request,
                "export-task",
                2,
                None,
                &desk_diagnose_core::model_egress::NativeModelInputSnapshot::capture(
                    &audit,
                    &crate::model_egress_store::test_inputs(&audit)
                )
                .unwrap(),
            )
            .await
            .is_err()
        );
        txn.rollback().await.unwrap();
        assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before);
        assert_eq!(
            budget_row::Entity::find()
                .all(&store.db)
                .await
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            model_egress_receipt::Entity::find()
                .all(&store.db)
                .await
                .unwrap(),
            vec![dispatch.receipt.clone()]
        );
        let txn = store.db.begin().await.unwrap();
        assert!(
            ScheduleStore::settle_fresh_model_dispatch_on(&txn, 1, &work.run_id, 2, None)
                .await
                .is_err()
        );
        txn.rollback().await.unwrap();
        crate::model_egress_store::SignalModelEgressStore::new(store.db.clone())
            .record_terminal_usage(
                &dispatch.receipt.receipt_id,
                &desk_diagnose_core::chat::TokenUsage {
                    input_tokens: Some(2),
                    output_tokens: Some(3),
                    cache_read_tokens: Some(4),
                    cache_write_tokens: Some(5),
                },
                None,
            )
            .await
            .unwrap();
        agent_task_authorization::Entity::update_many()
            .set(agent_task_authorization::ActiveModel {
                revoked_at: Set(Some(1)),
                ..Default::default()
            })
            .exec(&store.db)
            .await
            .unwrap();
        let mut expected = store.read(1, &task.schedule_id).await.unwrap();
        expected.revision += 1;
        let txn = store.db.begin().await.unwrap();
        let settled = ScheduleStore::settle_fresh_model_dispatch_on(&txn, 1, &work.run_id, 2, None)
            .await
            .unwrap();
        assert_eq!(settled.charged_units, 14);
        assert_eq!(settled.state, "overrun");
        txn.commit().await.unwrap();
        let txn = store.db.begin().await.unwrap();
        assert_eq!(
            ScheduleStore::settle_fresh_model_dispatch_on(&txn, 1, &work.run_id, 2, None)
                .await
                .unwrap(),
            settled
        );
        txn.commit().await.unwrap();
        assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), expected);
        assert!(
            agent_task_authorization::Entity::find()
                .all(&store.db)
                .await
                .unwrap()
                .iter()
                .all(|authorization| authorization.revoked_at == Some(1))
        );
    }
}
