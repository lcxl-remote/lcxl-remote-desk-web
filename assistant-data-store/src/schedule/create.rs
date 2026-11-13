//! Task creation and the consent a task runs under. Creation enables the task
//! in the same transaction unless an AI fresh-task proposal awaits a decision.
use super::{
    ScheduleStoreError,
    authority::authority_now,
    digest, json,
    session::{OrgMode, ScheduleHostStores, SessionGuard, SessionKey, TouchColumn},
};
use crate::entity::agent_schedule as entity;
use desk_agent_protocol::schedule::ScheduleCreation;
use desk_diagnose_core::schedule::{
    SCHEDULE_CALC_VERSION, lifecycle::FailureState, normalize_creation,
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set, sea_query::OnConflict};

/// Creates the task row for an owner's creation request, capturing the
/// retrieval or definition consent it runs under.
pub async fn create_task_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    db: &C,
    owner: i32,
    request: &ScheduleCreation,
    now_ms: i64,
    original: Option<&desk_diagnose_core::session::PersistedAgentSession>,
) -> Result<entity::Model, ScheduleStoreError> {
    if owner <= 0 || now_ms < 0 {
        return Err(ScheduleStoreError::Invalid);
    }
    let request = normalize_creation(request).map_err(|_| ScheduleStoreError::Invalid)?;
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
        capture_retrieval_consent_on(host, db, owner, &schedule_id, &request, original)
    })
    .await?;
    let prompt_actor = owner.to_string();
    let prompt_retention = if request.kind
        == desk_agent_protocol::schedule::ScheduledTaskKind::FreshTask
        && request.creation_source == desk_agent_protocol::schedule::ScheduleCreationSource::Manual
    {
        Some(
            desk_diagnose_core::future::send_phase(|| {
                host.owner_prompt_retention(db, &prompt_actor, &request.target_device_id)
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
async fn capture_retrieval_consent_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    db: &C,
    owner: i32,
    schedule_id: &str,
    request: &ScheduleCreation,
    original: Option<&desk_diagnose_core::session::PersistedAgentSession>,
) -> Result<Option<String>, ScheduleStoreError> {
    if !host.account_owner(owner) {
        return Err(ScheduleStoreError::NotFound);
    }
    use desk_diagnose_core::{
        ai_assistant::scheduled_retrieval::TaskRetrievalConsent, session::PersistedAgentSession,
    };
    let source = original
        .map(|source| source.conversation_id.as_str())
        .or(request.source_conversation_id.as_deref());
    let Some(source) = source else {
        return desk_diagnose_core::future::send_phase(|| {
            capture_definition_consent_on(host, db, owner, schedule_id, request)
        })
        .await
        .map(Some)
        .map_err(|error| match error {
            crate::AssistantStoreError::Backend(error) => ScheduleStoreError::Backend(error),
            crate::AssistantStoreError::Inaccessible => ScheduleStoreError::NotFound,
            _ => ScheduleStoreError::Invalid,
        });
    };

    host.lock_conversation(db, source).await?;
    let actor = owner.to_string();
    let row = host
        .find_session(
            db,
            SessionKey::Source {
                conversation: source,
                actor: &actor,
                device: &request.target_device_id,
            },
            OrgMode::PersonalOnly,
            false,
        )
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
        host.original_read_context(db, &owner_input).await?
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
    let captured_at = authority_now(db).await?;
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
    let locked = host
        .touch_session(
            db,
            TouchColumn::Version,
            SessionGuard {
                id: Some(row.id),
                conversation: Some(source),
                actor: Some(&session.actor_id),
                device: Some(&session.device_id),
                version: Some(row.version),
                lease_token: Some(row.lease_token),
                state_json: Some(&row.state_json),
                ..SessionGuard::any()
            },
        )
        .await?;
    if locked != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let current = host
        .find_session(db, SessionKey::Id(row.id), OrgMode::Any, false)
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

async fn capture_definition_consent_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    db: &C,
    owner: i32,
    schedule_id: &str,
    request: &ScheduleCreation,
) -> Result<String, crate::AssistantStoreError> {
    use crate as data;
    use desk_diagnose_core::ai_assistant::scheduled_retrieval::TaskRetrievalConsent;
    let actor = owner.to_string();
    let instructions = host.definition_instructions(db, owner).await?;
    let runtime = data::runtime_on(db, &actor).await?;
    let now = authority_now(db).await.map_err(|error| match error {
        ScheduleStoreError::Backend(error) => data::AssistantStoreError::Backend(error),
        _ => data::AssistantStoreError::Invalid("invalid task snapshot clock"),
    })?;
    let mut consent = TaskRetrievalConsent::capture_definition(
        schedule_id,
        1,
        &actor,
        &request.target_device_id,
        &instructions,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity_table::EntityTableExt;
    use crate::schedule::test_host::{Call, Recorder};
    use desk_agent_protocol::{
        AgentScope, ExecutionMode,
        schedule::{ScheduleCreationSource, ScheduleRule, ScheduleSpec, ScheduledTaskKind},
    };
    use desk_diagnose_core::session::PersistedAgentSession;
    use sea_orm::{Database, Schema};

    #[tokio::test]
    async fn a_source_consent_locks_the_conversation_before_reading_the_personal_source() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute(&Schema::new(db.get_database_backend()).entity_table(entity::Entity))
            .await
            .unwrap();
        let host = Recorder::new(PersistedAgentSession::new(
            "source",
            "2",
            "device",
            1,
            AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-11-13T00:00:00Z",
        ));
        let request = ScheduleCreation {
            client_create_key: "create-1".into(),
            kind: ScheduledTaskKind::FreshTask,
            target_device_id: "device".into(),
            title: "Report".into(),
            prompt: "Write a daily report".into(),
            locale: None,
            model_id: None,
            spec: ScheduleSpec {
                schema_version: 1,
                rule: ScheduleRule::Daily {
                    utc_time: "06:00:00".into(),
                },
            },
            time_confirmation: None,
            source_conversation_id: Some("source".into()),
            requirement_revision: None,
            creation_source: ScheduleCreationSource::Manual,
        };
        // The recorded session belongs to another actor, so capture stops
        // after the read; the lock and the read shape are the contract.
        assert!(matches!(
            create_task_on(&host, &db, 1, &request, 1000, None).await,
            Err(ScheduleStoreError::Conflict)
        ));
        assert_eq!(
            host.calls(),
            [
                Call::Lock("source".into()),
                Call::FindSource(
                    "source".into(),
                    "1".into(),
                    "device".into(),
                    OrgMode::PersonalOnly
                ),
            ]
        );
    }
}
