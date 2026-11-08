//! Assistant data invocation admission under original actor/device/session/task fences.
//! Complete every native/source read before the final result/quota writes.

use super::{AssistantStoreError, unsigned};
use crate::entity::agent_assistant_data_authorization as authorization;
use crate::{
    config::{ConfigConnection, connection::DatabaseTransaction},
    entity::{agent_assistant_origin as origin, agent_session},
};
use desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicy;
use desk_diagnose_core::ai_assistant::retrieval_query::QuerySourceVersion;
use desk_diagnose_core::{
    ai_assistant::{
        assistant_data_admission::OriginalAssistantDataCall,
        data_access::{AssistantDataToolAccess, scope_sha256},
        seam::AssistantDataCall,
    },
    model_egress::NativeModelOutputSources,
    session::PersistedAgentSession,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QuerySelect};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    Current,
    StaleControl,
    SkipWrite(&'static str),
}
impl Disposition {
    fn metadata_only(self) -> bool {
        self != Self::Current
    }
}

pub(crate) struct NativeAssistantDataFence<'a> {
    pub db: &'a DatabaseTransaction,
    pub context: AssistantDataCall<'a>,
    pub access: AssistantDataToolAccess,
    pub policy: AssistantDataPolicy,
    pub original: OriginalAssistantDataCall,
    row: agent_session::Model,
    producer: Box<NativeModelOutputSources>,
    archive: Option<super::output_archive::StoredOutput>,
    owner_input: Option<origin::Model>,
    control_grants: Vec<authorization::Model>,
    observed_sources: Vec<QuerySourceVersion>,
    deadline: u64,
    disposition: Disposition,
}

/// The caller owns this transaction through scans, final validation and commit.
/// The actual file snapshot and SQLite writer fence survive through commit.
pub(crate) async fn prepare_on<'a>(
    db: &'a DatabaseTransaction,
    context: AssistantDataCall<'a>,
    now: u64,
) -> Result<Box<NativeAssistantDataFence<'a>>, AssistantStoreError> {
    let original = OriginalAssistantDataCall::retrieval(&context, now)
        .map_err(AssistantStoreError::Invalid)?;
    prepare_data_on(db, context, original, now).await
}

pub(crate) async fn prepare_memory_write_on<'a>(
    db: &'a DatabaseTransaction,
    context: AssistantDataCall<'a>,
    now: u64,
) -> Result<Box<NativeAssistantDataFence<'a>>, AssistantStoreError> {
    let original = OriginalAssistantDataCall::memory_write(&context, now)
        .map_err(AssistantStoreError::Invalid)?;
    prepare_data_on(db, context, original, now).await
}

pub(crate) async fn prepare_memory_control_on<'a>(
    db: &'a DatabaseTransaction,
    context: AssistantDataCall<'a>,
    call: &desk_diagnose_core::ai_assistant::seam::MemoryForgetCall<'_>,
) -> Result<Box<NativeAssistantDataFence<'a>>, AssistantStoreError> {
    let now = context.now_unix_ms;
    let original = OriginalAssistantDataCall::memory_control(&context, call)
        .map_err(AssistantStoreError::Invalid)?;
    prepare_data_on(db, context, original, now).await
}

async fn prepare_data_on<'a>(
    db: &'a DatabaseTransaction,
    context: AssistantDataCall<'a>,
    original: OriginalAssistantDataCall,
    now: u64,
) -> Result<Box<NativeAssistantDataFence<'a>>, AssistantStoreError> {
    let session = context.session;
    let data = session
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    if session.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Err(AssistantStoreError::Inaccessible);
    }
    let config = db.config_read().await;
    let binding = crate::assistant_data_store::source_device_ref(
        &config.metadata.instance,
        &session.device_id,
    );
    if data.source_device_ref != binding
        || data.source_instance_id.as_deref() != Some(config.metadata.instance.as_str())
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    super::ensure_runtime_on(db, &session.actor_id).await?;
    let policy = config.assistant_data_policy.clone();
    let row = session_on(db, &context).await?;
    let persisted = Box::new(
        PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| AssistantStoreError::Invalid("invalid original retrieval session"))?,
    );
    let mut deadline = validate_session(&row, &persisted, &context, &original, now)?;
    let stale_control_targets = control_targets_stale_on(db, &context, &original).await?;
    let access = access_on(
        db,
        &context,
        &policy,
        &original,
        now,
        None,
        stale_control_targets || original.is_memory_write(),
    )
    .await?;
    let mut disposition = if stale_control_targets {
        Disposition::StaleControl
    } else {
        write_disposition_on(db, &context, &original, &access, now).await?
    };
    if !disposition.metadata_only()
        && let Some(until) = original
            .scope_deadline(&context, &access, now)
            .map_err(AssistantStoreError::Invalid)?
    {
        deadline = deadline.min(until);
    }
    if let Some(until) = super::model_gate::task_deadline_on(db, &persisted, now).await? {
        deadline = deadline.min(until);
    }
    let prepared = desk_diagnose_core::future::boxed(|| {
        producer_on(
            db,
            &context,
            &original,
            &access,
            &policy,
            now,
            None,
            disposition.metadata_only(),
            None,
        )
    })
    .await;
    let (producer, source_deadline, observed_sources, control_grants, archive) = match prepared {
        Err(AssistantStoreError::Inaccessible)
            if original.is_memory_write() && disposition == Disposition::Current =>
        {
            // Only an unavailable current source may become a no-write result.
            // The complete accepted native metadata must independently validate.
            disposition = Disposition::SkipWrite("source_unavailable");
            desk_diagnose_core::future::boxed(|| {
                producer_on(
                    db, &context, &original, &access, &policy, now, None, true, None,
                )
            })
            .await?
        }
        other => other?,
    };
    let owner_input = control_owner_input_on(db, &context, &original, &producer).await?;
    if let Some(until) = source_deadline {
        deadline = deadline.min(until);
    }
    if deadline <= now.max(chrono::Utc::now().timestamp_millis() as u64) {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(Box::new(NativeAssistantDataFence {
        db,
        context,
        access,
        policy,
        original,
        row,
        producer,
        archive,
        owner_input,
        control_grants,
        observed_sources,
        deadline,
        disposition,
    }))
}

impl NativeAssistantDataFence<'_> {
    pub(crate) fn producer_archive(&self) -> Option<&super::output_archive::StoredOutput> {
        self.archive.as_ref()
    }

    pub(crate) fn session_row(&self) -> &agent_session::Model {
        &self.row
    }

    /// A stale result has no control side effects and contains no current fact.
    pub(crate) async fn stale_control_receipt_on(
        &self,
        operation_id: &str,
        now: u64,
    ) -> Result<
        Option<desk_agent_protocol::ai_assistant::memory::AssistantDataControlReceipt>,
        AssistantStoreError,
    > {
        if self.disposition != Disposition::StaleControl {
            return Ok(None);
        }
        self.finish_on(now).await?;
        use desk_agent_protocol::ai_assistant::memory::{
            AssistantDataControlReceipt, DataControlOutcome, DataControlReason,
        };
        Ok(Some(AssistantDataControlReceipt {
            operation_id: operation_id.into(),
            outcome: DataControlOutcome::StaleTarget,
            reason: DataControlReason::MemoryForget,
            affected_count: 0,
            control_generation: self.access.control_generation,
            memory_state_generation: self.access.memory_state_generation,
            cleanup_pending: false,
        }))
    }

    pub(crate) fn write_skip_reason(&self) -> Option<&'static str> {
        match self.disposition {
            Disposition::SkipWrite(reason) => Some(reason),
            _ => None,
        }
    }

    /// Observation is a locator prerequisite only; current source/task access
    /// must still be checked for the actual new read and eventual model send.
    pub(crate) fn observed_version(&self, source_id: &str) -> Option<&QuerySourceVersion> {
        self.observed_sources
            .iter()
            .filter(|source| source.source_id == source_id)
            .max_by_key(|source| source.revision)
    }
    /// A separate original SELECT and exact raw comparison precede result stores.
    /// A transaction must not be treated as an immutable authorization snapshot.
    pub(crate) async fn finish_on(&self, now: u64) -> Result<u64, AssistantStoreError> {
        self.finish_validated_on(now, None).await
    }

    pub(crate) async fn finish_after_control_on(
        &self,
        now: u64,
        control: &super::memory_control::PendingControl,
    ) -> Result<u64, AssistantStoreError> {
        if !matches!(self.original, OriginalAssistantDataCall::MemoryControl(_))
            || self.owner_input.is_none()
            || self.disposition != Disposition::Current
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        control.validate_owned_on(self.db).await?;
        let deadline = self.finish_validated_on(now, Some(control)).await?;
        control.validate_owned_on(self.db).await?;
        Ok(deadline)
    }

    async fn finish_validated_on(
        &self,
        now: u64,
        control: Option<&super::memory_control::PendingControl>,
    ) -> Result<u64, AssistantStoreError> {
        let row = session_on(self.db, &self.context).await?;
        if row.state_json != self.row.state_json
            || row.version != self.row.version
            || row.lease_token != self.row.lease_token
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        let persisted = Box::new(
            PersistedAgentSession::decode_json(&row.state_json)
                .map_err(|_| AssistantStoreError::Invalid("invalid final retrieval session"))?,
        );
        let mut deadline = validate_session(&row, &persisted, &self.context, &self.original, now)?
            .min(self.deadline);
        if access_on(
            self.db,
            &self.context,
            &self.policy,
            &self.original,
            now,
            control,
            self.disposition.metadata_only() || self.original.is_memory_write(),
        )
        .await?
            != self.access
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        if !self.disposition.metadata_only()
            && let Some(until) = self
                .original
                .scope_deadline(&self.context, &self.access, now)
                .map_err(AssistantStoreError::Invalid)?
        {
            deadline = deadline.min(until);
        }
        if let Some(until) = super::model_gate::task_deadline_on(self.db, &persisted, now).await? {
            deadline = deadline.min(until);
        }
        if self.original.is_memory_write() {
            let current =
                write_disposition_on(self.db, &self.context, &self.original, &self.access, now)
                    .await?;
            let expected = if self.disposition == Disposition::SkipWrite("source_unavailable") {
                Disposition::Current
            } else {
                self.disposition
            };
            if current != expected {
                return Err(AssistantStoreError::Inaccessible);
            }
            if self.disposition == Disposition::SkipWrite("source_unavailable") {
                match desk_diagnose_core::future::boxed(|| {
                    producer_on(
                        self.db,
                        &self.context,
                        &self.original,
                        &self.access,
                        &self.policy,
                        now,
                        None,
                        false,
                        self.archive.as_ref(),
                    )
                })
                .await
                {
                    Err(AssistantStoreError::Inaccessible) => {}
                    Err(error) => return Err(error),
                    Ok(_) => return Err(AssistantStoreError::Inaccessible),
                }
            }
        }
        let (producer, source_deadline, observed_sources, control_grants, archive) =
            desk_diagnose_core::future::boxed(|| {
                producer_on(
                    self.db,
                    &self.context,
                    &self.original,
                    &self.access,
                    &self.policy,
                    now,
                    control.map(|control| (control, self.control_grants.as_slice())),
                    self.disposition.metadata_only(),
                    self.archive.as_ref(),
                )
            })
            .await?;
        if let Some(until) = source_deadline {
            deadline = deadline.min(until);
        }
        if (control.is_none()
            && control_targets_stale_on(self.db, &self.context, &self.original).await?
                != (self.disposition == Disposition::StaleControl))
            || control_owner_input_on(self.db, &self.context, &self.original, &producer).await?
                != self.owner_input
            || control_grants != self.control_grants
            || archive.as_ref().map(|value| &value.row)
                != self.archive.as_ref().map(|value| &value.row)
            || producer != self.producer
            || observed_sources != self.observed_sources
            || deadline <= now.max(chrono::Utc::now().timestamp_millis() as u64)
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        Ok(deadline)
    }
}

async fn session_on(
    db: &DatabaseTransaction,
    context: &AssistantDataCall<'_>,
) -> Result<agent_session::Model, AssistantStoreError> {
    agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&context.session.conversation_id))
        .filter(agent_session::Column::ActorId.eq(&context.session.actor_id))
        .filter(agent_session::Column::DeviceId.eq(&context.session.device_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)
}

fn validate_session(
    row: &agent_session::Model,
    persisted: &PersistedAgentSession,
    context: &AssistantDataCall<'_>,
    original: &OriginalAssistantDataCall,
    now: u64,
) -> Result<u64, AssistantStoreError> {
    original
        .persisted_producer(context, persisted, now)
        .map_err(AssistantStoreError::Invalid)?;
    let deadline = unsigned(
        row.lease_deadline
            .ok_or(AssistantStoreError::Inaccessible)?
            .timestamp_millis(),
    )?;
    if row.version != persisted.version
        || unsigned(row.lease_token)? != persisted.lease_token
        || deadline <= now
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(original
        .unused_read_deadline()
        .map_or(deadline, |until| deadline.min(until)))
}

async fn access_on(
    db: &DatabaseTransaction,
    context: &AssistantDataCall<'_>,
    policy: &AssistantDataPolicy,
    original: &OriginalAssistantDataCall,
    now: u64,
    control: Option<&super::memory_control::PendingControl>,
    metadata_only: bool,
) -> Result<AssistantDataToolAccess, AssistantStoreError> {
    let data = context
        .session
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let runtime = super::reconcile_settings_on(db, &context.session.actor_id).await?;
    let runtime = match control {
        Some(control) => control.runtime_before(runtime)?,
        None => runtime,
    };
    let config = db.config_read().await;
    if config.assistant_data_policy != *policy
        || runtime.settings_instance_id.as_deref() != Some(config.metadata.instance.as_str())
        || data.source_instance_id.as_deref() != Some(config.metadata.instance.as_str())
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let access = AssistantDataToolAccess {
        settings: config.assistant_data_settings.clone(),
        policy_revision: policy.revision,
        memory_state_generation: unsigned(runtime.memory_state_generation)?,
        control_generation: unsigned(runtime.control_generation)?,
    };
    access.validate().map_err(AssistantStoreError::Invalid)?;
    if (metadata_only && unsigned(runtime.input_seq)? < data.current_actor_input_seq)
        || access.memory_state_generation < data.memory_state_generation
        || access.control_generation < data.control_generation
        || (!metadata_only
            && (access.memory_state_generation != data.memory_state_generation
                || access.control_generation != data.control_generation))
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    if !metadata_only {
        original
            .scope_deadline(context, &access, now)
            .map_err(AssistantStoreError::Invalid)?;
    }
    Ok(access)
}

async fn control_targets_stale_on(
    db: &DatabaseTransaction,
    context: &AssistantDataCall<'_>,
    original: &OriginalAssistantDataCall,
) -> Result<bool, AssistantStoreError> {
    match original {
        OriginalAssistantDataCall::MemoryControl(group) => {
            if context.session.trigger_origin != desk_diagnose_core::session::TriggerOrigin::User {
                return Err(AssistantStoreError::Inaccessible);
            }
            super::memory_control::targets_are_stale_on(
                db,
                &context.session.actor_id,
                group.owner_input_seq(),
                group.targets(),
            )
            .await
        }
        _ => Ok(false),
    }
}

async fn write_disposition_on(
    db: &DatabaseTransaction,
    context: &AssistantDataCall<'_>,
    original: &OriginalAssistantDataCall,
    access: &AssistantDataToolAccess,
    now: u64,
) -> Result<Disposition, AssistantStoreError> {
    if !original.is_memory_write() {
        return Ok(Disposition::Current);
    }
    let data = context
        .session
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let original_scope = data
        .memory_access_scope
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    if desk_diagnose_core::ai_assistant::memory_access::usable_memory_scope(
        context.session,
        original_scope.accepted_at_unix_ms,
    )
    .is_none()
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    if let desk_diagnose_core::ai_assistant::data_tools::ParsedDataCall::ManageMemory(
        desk_agent_protocol::ai_assistant::memory::MemoryWriteRequest::Update {
            memory_id,
            expected_revision,
            ..
        },
    ) = context.arguments
    {
        use crate::entity::agent_memory as memory;
        let target = memory::Entity::find()
            .select_only()
            .column(memory::Column::Revision)
            .column(memory::Column::Status)
            .filter(memory::Column::ActorId.eq(&context.session.actor_id))
            .filter(memory::Column::MemoryId.eq(memory_id))
            .into_tuple::<(i64, String)>()
            .one(db)
            .await?;
        let current = match target {
            Some((revision, status)) => {
                unsigned(revision)? == *expected_revision
                    && matches!(status.as_str(), "active" | "pending_conflict")
            }
            None => false,
        };
        if !current {
            return Ok(Disposition::SkipWrite("stale_target"));
        }
    }
    if !access.settings.automatic_memory_enabled
        || access.control_generation != data.control_generation
        || access.memory_state_generation != data.memory_state_generation
        || access.policy_revision != original_scope.policy_revision
        || desk_diagnose_core::ai_assistant::memory_access::usable_memory_scope(
            context.session,
            now,
        )
        .is_none()
    {
        return Ok(Disposition::SkipWrite("source_changed"));
    }
    Ok(Disposition::Current)
}

#[allow(clippy::too_many_arguments)]
async fn producer_on(
    db: &DatabaseTransaction,
    context: &AssistantDataCall<'_>,
    original: &OriginalAssistantDataCall,
    access: &AssistantDataToolAccess,
    policy: &AssistantDataPolicy,
    now: u64,
    control: Option<(
        &super::memory_control::PendingControl,
        &[authorization::Model],
    )>,
    metadata_only: bool,
    known: Option<&super::output_archive::StoredOutput>,
) -> Result<
    (
        Box<NativeModelOutputSources>,
        Option<u64>,
        Vec<QuerySourceVersion>,
        Vec<authorization::Model>,
        Option<super::output_archive::StoredOutput>,
    ),
    AssistantStoreError,
> {
    let (sources, decoded) =
        crate::model_egress_store::SignalModelEgressStore::read_context_output_with_archive_on(
            db,
            &context.session.conversation_id,
            context.producer_output_envelope,
            known,
        )
        .await?;
    let sources = Box::new(sources);
    original
        .accepted_producer(context, &sources, now)
        .map_err(AssistantStoreError::Invalid)?;
    let archive = super::output_archive::find_reusing_on(
        db,
        &context.session.conversation_id,
        context.producer_output_envelope,
        decoded.as_ref(),
    )
    .await?;
    let Some(archive) = archive else {
        if original.requires_native_archive() {
            return Err(AssistantStoreError::Inaccessible);
        }
        if sources
            .inputs
            .iter()
            .any(|input| input.source_provider_id == "assistant-data-source")
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        return Ok((
            sources,
            context
                .producer_output_envelope
                .retention
                .expires_at_unix_ms,
            Vec::new(),
            Vec::new(),
            None,
        ));
    };
    let data = context
        .session
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    if archive.proof.sources != *sources
        || archive.proof.source_set.actor_id != context.session.actor_id
        || archive.proof.source_set.source_device_ref != data.source_device_ref
        || archive.proof.input_revision != context.session.input_revision
        || archive.proof.actor_input_seq != data.current_actor_input_seq
        || archive.proof.input_recorded_at_unix_ms != data.current_input_recorded_at_unix_ms
        || archive.proof.statement_timezone != data.current_input_timezone
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let scope = scope_sha256(context.session).map_err(AssistantStoreError::Invalid)?;
    let mut deadline = context
        .producer_output_envelope
        .retention
        .expires_at_unix_ms;

    let mut observed = BTreeMap::<(String, u64), QuerySourceVersion>::new();
    let mut observed_control_targets = BTreeSet::new();
    let grants = match control {
        Some((control, originals)) => {
            super::native_export::load_call_set_before_control_on(
                db,
                &archive.proof.source_set,
                control,
                originals,
            )
            .await?
        }
        None if metadata_only => {
            super::native_export::load_consumed_metadata_on(db, &archive.proof.source_set)
                .await?
                .into_iter()
                .map(|value| value.authorization)
                .collect()
        }
        None => super::native_export::load_call_set_on(db, &archive.proof.source_set).await?,
    };
    let mut observed_reads = Vec::new();
    for label in sources.input_snapshot.policy_sources() {
        if label.provenance.source_provider_id == "assistant-conversation-snapshot" {
            match super::projection::load_conversation_snapshot_on(
                db,
                &context.session.actor_id,
                &context.session.conversation_id,
                &data.source_device_ref,
                label,
            )
            .await
            {
                Ok(read) => observed_reads.push(read.proof),
                Err(AssistantStoreError::Backend(error)) => return Err(error.into()),
                Err(_) => {}
            }
        }
    }
    let mut control_grants = Vec::new();
    for stored in grants {
        if matches!(original, OriginalAssistantDataCall::MemoryControl(_)) {
            control_grants.push(stored.row.clone());
        }
        let grant = stored.proof;
        if grant.input_revision != context.session.input_revision
            || grant.actor_input_seq != data.current_actor_input_seq
            || grant.retrieval_episode_id != data.retrieval_budget.episode_id
            || grant.task_id != data.retrieval_task_id
            || grant.scope_sha256 != scope
            || grant.policy_revision
                != if metadata_only {
                    archive.proof.source_set.context.policy_revision
                } else {
                    access.policy_revision
                }
            || grant.memory_state_generation
                != if metadata_only {
                    data.memory_state_generation
                } else {
                    access.memory_state_generation
                }
            || grant.control_generation
                != if metadata_only {
                    data.control_generation
                } else {
                    access.control_generation
                }
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        let source =
            super::projection::load_on(db, &context.session.actor_id, &grant.projection_id)
                .await?
                .ok_or(AssistantStoreError::Inaccessible)?;
        if !data.read_uses.contains(&source.proof.usage.reference()) {
            return Err(AssistantStoreError::Inaccessible);
        }
        if matches!(original, OriginalAssistantDataCall::MemoryControl(_)) {
            grant
                .validate_projection(&source.proof)
                .map_err(AssistantStoreError::Invalid)?;
        }

        if !metadata_only
            && let Some(until) = desk_diagnose_core::future::boxed(|| {
                super::model_sources::validate_session_sources_on(
                    db,
                    context.session,
                    &source.proof.usage,
                    access,
                    context.destination,
                    policy,
                    now,
                    control.map(|(control, _)| control),
                )
            })
            .await?
        {
            deadline = Some(deadline.map_or(until, |old| old.min(until)));
        }
        observed_reads.push(source.proof);
    }
    for source in observed_reads {
        if let OriginalAssistantDataCall::MemoryControl(group) = original {
            for reference in &source.usage.memories {
                if group.targets().iter().any(|target| {
                    target.memory_id == reference.memory_id
                        && target.expected_revision == reference.revision
                }) {
                    observed_control_targets
                        .insert((reference.memory_id.clone(), reference.revision));
                }
            }
        }
        for root in source
            .usage
            .roots
            .iter()
            .chain(&source.usage.decision_roots)
        {
            let dependency = source
                .usage
                .history_dependencies
                .iter()
                .find(|dependency| {
                    dependency.source_id == root.source_id
                        && dependency.source_revision == root.source_revision
                })
                .ok_or(AssistantStoreError::Inaccessible)?;
            let version = QuerySourceVersion {
                source_id: root.source_id.clone(),
                revision: root.source_revision,
                content_sha256: dependency.content_sha256.clone(),
            };
            if let Some(previous) = observed.insert(
                (root.source_id.clone(), root.source_revision),
                version.clone(),
            ) && previous != version
            {
                return Err(AssistantStoreError::Inaccessible);
            }
        }
    }
    if let OriginalAssistantDataCall::MemoryControl(group) = original
        && observed_control_targets.len() != group.targets().len()
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    control_grants.sort_by_key(|row| row.id);
    Ok((
        sources,
        deadline,
        observed.into_values().collect(),
        control_grants,
        Some(archive),
    ))
}

/// Original owner input is a control prerequisite, never a learning grant.
/// Reissue the same selector and compare its whole row at final admission.
async fn control_owner_input_on(
    db: &DatabaseTransaction,
    context: &AssistantDataCall<'_>,
    original: &OriginalAssistantDataCall,
    producer: &NativeModelOutputSources,
) -> Result<Option<origin::Model>, AssistantStoreError> {
    use sha2::{Digest, Sha256};
    let OriginalAssistantDataCall::MemoryControl(group) = original else {
        return Ok(None);
    };
    let session = context.session;
    let data = session
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let mut rows = origin::Entity::find()
        .filter(origin::Column::ActorId.eq(&session.actor_id))
        .filter(origin::Column::RootConversationId.eq(&session.conversation_id))
        .filter(origin::Column::AcceptedInputSeq.eq(super::signed(group.owner_input_seq())?))
        .filter(origin::Column::Kind.eq("input"))
        .limit(2)
        .all(db)
        .await?;
    if rows.len() != 1 {
        return Err(AssistantStoreError::Inaccessible);
    }
    let input = rows.pop().unwrap();
    let message = session
        .conversation
        .iter()
        .find(|message| message.message_id == input.native_id)
        .ok_or(AssistantStoreError::Inaccessible)?;
    if input.conversation_id != session.conversation_id
        || input.deleted_at_ms.is_some()
        || input.device_ref != data.source_device_ref
        || unsigned(input.created_at_ms)? != data.current_input_recorded_at_unix_ms
        || input.statement_timezone != data.current_input_timezone
        || message.role != desk_diagnose_core::chat::ChatRole::User
        || format!("{:x}", Sha256::digest(message.text.as_bytes())) != input.original_sha256
        || !producer
            .input_snapshot
            .policy_sources()
            .iter()
            .any(|label| {
                label.provenance.source_provider_id == "ai-assistant-user"
                    && label.provenance.source_object_id.as_deref()
                        == Some(input.native_id.as_str())
            })
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(Some(input))
}
