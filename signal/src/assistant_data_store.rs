//! Runtime assistant state is transactional SQLite data, independent of file settings.

use crate::{config::ConfigConnection, entity::agent_assistant_runtime as runtime};
use desk_diagnose_core::{
    ai_assistant::{data_control::AssistantDataSession, history::AssistantRetrievalBudget},
    native_ids::{self, Kind},
    session::{AgentSessionSurface, PersistedAgentSession},
};
use sea_orm::DbErr;
use sea_orm::{ActiveModelTrait, ConnectionTrait, EntityTrait, Set, sea_query::OnConflict};

pub(crate) mod history;
use desk_assistant_data_store::history_native;
mod memory;
mod memory_control;
pub(crate) mod native_result;
mod native_status;
mod native_summary;
mod source_lifecycle;
mod visible_history;

mod access;
use desk_assistant_data_store::citations;
mod conversation_search;
mod conversation_window;
mod current_memory;
mod current_native;
mod evidence;
use desk_assistant_data_store::history_query;
mod history_scan;
mod history_view;
mod keyword_search;
mod memory_history;
mod memory_keyword;
mod memory_write;
pub(crate) use memory_write::write as write_memory;
mod memory_search;
mod memory_source;
mod memory_view;
pub(crate) mod model_gate;
pub(crate) mod model_sources;
mod native_export;
pub(crate) mod output_archive;
mod owner_conversations;
mod owner_copy;
mod owner_search;
mod owner_source;
mod owner_window;
mod prior_outputs;
mod processing;
use desk_assistant_data_store::projection;
mod request_manifest;
use desk_assistant_data_store::retrieval_cache;
mod retrieval_native;
mod retrieval_result;
use desk_assistant_data_store::suppression;

#[cfg(test)]
pub(crate) mod tests;

pub(crate) use access::context_access;
pub(crate) use conversation_search::search as search_conversations;
pub(crate) use conversation_window::read as read_conversation;
#[cfg(test)]
pub(crate) use history_native::read as read_native_history;
pub(crate) use memory::memories;
pub(crate) use memory_control::clear_memories;
pub(crate) use memory_control::forget_memory;
pub(crate) use native_status::NativeStatusScope;
pub(crate) use native_status::ReclaimObservation;
pub(crate) use native_status::scope_on as native_scope_on;
pub(crate) use output_archive::gc::schedule::run_due_once as run_output_gc_once;
pub(crate) use output_archive::{
    StoredOutput, find_on as find_output_on, find_reusing_on as find_output_reusing_on,
};
pub(crate) use owner_conversations::owner_history_conversations;
pub(crate) use owner_search::owner_history_search;
pub(crate) use owner_source::owner_source;
pub(crate) use owner_window::owner_history_window;
pub(crate) use processing::processing;
pub(crate) use request_manifest::prepare as prepare_request_manifest;
pub(crate) use source_lifecycle::delete_retained_root_on;
pub(crate) use source_lifecycle::mark_manual_delete_on;
pub(crate) use visible_history::citations::load_on as load_citations_on;
pub(crate) use visible_history::prepare_on as prepare_visible_history_on;

pub(crate) use current_memory::prepare as prepare_current_memory;
pub use desk_assistant_data_store::AssistantStoreError;
pub(crate) use desk_assistant_data_store::{
    ensure_quota_on, increment, runtime_on, signed, unsigned,
};
pub(crate) use memory_control::native::forget as forget_native_memory;
pub(crate) use memory_history::read as read_memory_history;
pub(crate) use memory_search::search as search_memories;
pub(crate) use memory_source::read as read_memory_source;
pub(crate) use visible_history::retained_native_sources;

pub(crate) async fn ensure_runtime_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    actor: &str,
) -> Result<(), AssistantStoreError> {
    if actor != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Err(AssistantStoreError::Inaccessible);
    }
    let config = db.config_read().await;
    // This is the first SQLite write, before reading any owner data or generations.
    runtime::Entity::insert(runtime::ActiveModel {
        actor_id: Set(actor.into()),
        settings_instance_id: Set(Some(config.metadata.instance.clone())),
        input_seq: Set(0),
        history_data_generation: Set(0),
        memory_state_generation: Set(1),
        control_generation: Set(1),

        learning_cutoff_input_seq: Set(0),
        settings_revision_seen: Set(signed(config.assistant_data_settings.revision)?),
        revision: Set(1),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(runtime::Column::ActorId)
            .do_nothing()
            .to_owned(),
    )
    .exec_without_returning(db)
    .await?;
    Ok(())
}

/// Read the actual file settings without reconciling or modifying runtime data.
pub(crate) async fn settings_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    actor: &str,
) -> Result<
    desk_agent_protocol::ai_assistant::user_instructions::AssistantDataSettings,
    AssistantStoreError,
> {
    if actor != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Err(AssistantStoreError::Inaccessible);
    }
    let settings = db.config_read().await.assistant_data_settings.clone();
    desk_diagnose_core::ai_assistant::settings::validate_settings(&settings)
        .map_err(AssistantStoreError::Invalid)?;
    Ok(settings)
}

/// File revisions survive process restarts and cannot be reduced to a cached
/// boolean. An observed section change invalidates old maintenance authority;
/// newer accepted inputs remain eligible under the new settings revision.
pub(crate) async fn reconcile_settings_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    actor: &str,
) -> Result<runtime::Model, AssistantStoreError> {
    let config = db.config_read().await;
    let row = runtime_on(db, actor).await?;
    let same_instance =
        row.settings_instance_id.as_deref() == Some(config.metadata.instance.as_str());
    let revision = signed(config.assistant_data_settings.revision)?;
    if same_instance && row.settings_revision_seen > revision {
        return Err(AssistantStoreError::Invalid(
            "assistant settings revision moved backwards",
        ));
    }
    if same_instance && row.settings_revision_seen == revision {
        return Ok(row);
    }
    let mut active: runtime::ActiveModel = row.clone().into();
    active.settings_instance_id = Set(Some(config.metadata.instance.clone()));
    active.settings_revision_seen = Set(revision);
    active.control_generation = Set(increment(row.control_generation)?);

    active.learning_cutoff_input_seq = Set(row.learning_cutoff_input_seq.max(row.input_seq));
    active.revision = Set(increment(row.revision)?);
    Ok(active.update(db).await?)
}

pub(crate) fn source_device_ref(instance: &str, device: &str) -> String {
    if device.is_empty() {
        String::new()
    } else {
        native_ids::stable(Kind::AssistantDeviceBinding, &(instance, device))
    }
}

pub(crate) fn bind_input_memory_devices(
    session: &mut PersistedAgentSession,
    targets: &[desk_diagnose_core::dynamic_run::AssistantInputTarget],
) -> Result<(), &'static str> {
    let instance = session
        .assistant_data
        .as_ref()
        .and_then(|data| data.source_instance_id.as_deref())
        .ok_or("missing instance for assistant memory targets")?
        .to_owned();
    desk_diagnose_core::ai_assistant::memory_access::bind_request_memory_devices(
        session,
        targets
            .iter()
            .map(|target| source_device_ref(&instance, &target.device_id)),
    )
}

pub(crate) async fn accept_input_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    session: &mut PersistedAgentSession,
    client_timezone: &str,
    temporary: Option<bool>,
    now_unix_ms: u64,
) -> Result<(), DbErr> {
    if session.surface != AgentSessionSurface::AiAssistant
        || session.agent_role.binding().is_some()
        || session.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()
    {
        return Err(invalid(
            "assistant data input requires the personal main session",
        ));
    }
    if let Some(data) = &session.assistant_data {
        if temporary.is_some_and(|value| value != data.temporary) {
            return Err(invalid("temporary mode is fixed for this conversation"));
        }
    } else if session.latest_input_seq != 0 {
        return Err(invalid("missing first-input assistant data state"));
    }
    let config = db.config_read().await;
    let source_device_ref = source_device_ref(&config.metadata.instance, &session.device_id);
    if session.assistant_data.as_ref().is_some_and(|data| {
        data.source_device_ref != source_device_ref
            || data.source_instance_id.as_deref() != Some(config.metadata.instance.as_str())
    }) {
        return Err(invalid("assistant source binding changed"));
    }
    ensure_runtime_on(db, &session.actor_id)
        .await
        .map_err(|_| invalid("assistant runtime initialization failed"))?;
    let row = reconcile_settings_on(db, &session.actor_id)
        .await
        .map_err(|_| invalid("assistant settings could not be reconciled"))?;
    let next_input = row
        .input_seq
        .checked_add(1)
        .filter(|seq| *seq > 0)
        .ok_or_else(|| invalid("assistant input sequence exhausted"))?;
    let mut active: runtime::ActiveModel = row.clone().into();
    active.input_seq = Set(next_input);
    active.revision = Set(row
        .revision
        .checked_add(1)
        .ok_or_else(|| invalid("assistant runtime revision exhausted"))?);
    active.update(db).await?;
    let budget = AssistantRetrievalBudget::with_limits(
        native_ids::stable(
            Kind::RetrievalEpisode,
            &(&session.actor_id, &session.conversation_id, next_input),
        ),
        config.assistant_data_policy.limits.retrieval.episode,
    )
    .map_err(invalid)?;
    let is_temporary = session
        .assistant_data
        .as_ref()
        .map_or(temporary.unwrap_or(false), |data| data.temporary);
    let history_access_scope = desk_diagnose_core::ai_assistant::data_access::owner_history_scope(
        &session.actor_id,
        &session.conversation_id,
        &source_device_ref,
        next_input as u64,
        now_unix_ms,
        &budget.episode_id,
        config.assistant_data_policy.revision,
        &config.assistant_data_settings,
        client_timezone,
        is_temporary,
    )
    .map_err(invalid)?;
    let memory_access_scope = desk_diagnose_core::ai_assistant::memory_access::owner_memory_scope(
        &session.actor_id,
        &session.conversation_id,
        &source_device_ref,
        next_input as u64,
        now_unix_ms,
        &budget.episode_id,
        config.assistant_data_policy.revision,
        &config.assistant_data_settings,
        is_temporary,
    )
    .map_err(invalid)?;
    if let Some(data) = &mut session.assistant_data {
        data.current_memories.clear();
        data.read_uses.clear();
        data.history_dependencies.clear();
        data.memory_state_generation = u64::try_from(row.memory_state_generation)
            .map_err(|_| invalid("invalid memory state generation"))?;
        data.control_generation = u64::try_from(row.control_generation)
            .map_err(|_| invalid("invalid assistant control generation"))?;
        data.current_actor_input_seq = next_input as u64;
        data.current_input_recorded_at_unix_ms = now_unix_ms;
        data.current_input_timezone = client_timezone.into();
        data.retrieval_budget = budget;
        data.limits = config.assistant_data_policy.limits.clone();
        data.retrieval_task_id = None;
        data.history_access_scope = history_access_scope;
        data.memory_access_scope = memory_access_scope;
    } else {
        let snapshot = desk_diagnose_core::ai_assistant::settings::freeze_instructions(
            &config.assistant_user_instructions,
            next_input as u64,
            now_unix_ms,
        )
        .map_err(invalid)?;
        session.assistant_data = Some(Box::new(AssistantDataSession {
            limits: config.assistant_data_policy.limits.clone(),
            temporary: temporary.unwrap_or(false),
            current_actor_input_seq: next_input as u64,
            source_device_ref,
            source_instance_id: Some(config.metadata.instance.clone()),
            instructions: snapshot,
            current_input_recorded_at_unix_ms: now_unix_ms,
            current_input_timezone: client_timezone.into(),
            memory_state_generation: u64::try_from(row.memory_state_generation)
                .map_err(|_| invalid("invalid memory state generation"))?,
            control_generation: u64::try_from(row.control_generation)
                .map_err(|_| invalid("invalid assistant control generation"))?,
            current_memories: Vec::new(),
            retrieval_budget: budget,
            retrieval_task_id: None,
            history_access_scope,
            memory_access_scope,
            history_dependencies: Vec::new(),
            read_uses: Vec::new(),
        }));
    }
    Ok(())
}

fn invalid(reason: &str) -> DbErr {
    DbErr::Custom(reason.into())
}
