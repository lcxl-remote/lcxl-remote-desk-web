//! Prepare one bounded control group before its caller closes the native session.
//! Finalization is the single assistant quota mutation and performs no later SQL.

use super::*;
use desk_agent_protocol::ai_assistant::memory::MemoryForgetTarget;

pub struct ForgetGroupRequest {
    pub operation_id: String,
    pub producer_message_id: String,
    pub call_message_sha256: String,
    pub owner_input_seq: u64,
    pub targets: Vec<MemoryForgetTarget>,
}

#[must_use]
pub enum ControlPreparation {
    Unchanged(AssistantDataControlReceipt),
    Prepared(Box<PendingControl>),
}

/// This is an owner-control storage operation, not a model authorization.
/// A model caller must retain its original producer, intent and observed-target
/// fences through preparation and exact native group closure before finalization.
pub async fn prepare_forget_group_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    request: &ForgetGroupRequest,
    now: u64,
) -> Result<ControlPreparation, AssistantStoreError> {
    let hash = request_hash(&(
        "model-memory-forget-group-v1",
        &request.operation_id,
        &request.producer_message_id,
        &request.call_message_sha256,
        request.owner_input_seq,
        &request.targets,
    ))?;
    if let Some(receipt) = replay_on(db, actor, &request.operation_id, &hash).await? {
        return Ok(ControlPreparation::Unchanged(receipt));
    }
    let state = runtime_on(db, actor).await?;
    if request.owner_input_seq > unsigned(state.input_seq)? {
        return Err(AssistantStoreError::Inaccessible);
    }
    let mut rows = Vec::with_capacity(request.targets.len());
    for target in &request.targets {
        let row = memory::Entity::find()
            .filter(memory::Column::ActorId.eq(actor))
            .filter(memory::Column::MemoryId.eq(&target.memory_id))
            .one(db)
            .await?;
        let Some(row) = row.filter(|row| {
            unsigned(row.revision).ok() == Some(target.expected_revision)
                && row.status != "forgotten"
                && unsigned(row.accepted_input_seq).is_ok_and(|seq| seq <= request.owner_input_seq)
        }) else {
            return Ok(ControlPreparation::Unchanged(stale(
                &request.operation_id,
                DataControlReason::MemoryForget,
                &state,
            )?));
        };
        rows.push(row);
    }
    Ok(ControlPreparation::Prepared(Box::new(
        prepare_apply_on(
            db,
            actor,
            &request.operation_id,
            &hash,
            DataControlReason::MemoryForget,
            state,
            rows,
            Some(request),
            now,
        )
        .await?,
    )))
}

/// Only observed target metadata is needed to reject an obsolete control.
/// This grants neither a current fact read nor permission to mutate any target.
pub async fn targets_are_stale_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    owner_input_seq: u64,
    targets: &[MemoryForgetTarget],
) -> Result<bool, AssistantStoreError> {
    for target in targets {
        let row = memory::Entity::find()
            .select_only()
            .column(memory::Column::Revision)
            .column(memory::Column::Status)
            .column(memory::Column::AcceptedInputSeq)
            .filter(memory::Column::ActorId.eq(actor))
            .filter(memory::Column::MemoryId.eq(&target.memory_id))
            .into_tuple::<(i64, String, i64)>()
            .one(db)
            .await?;
        let Some((revision, status, accepted_input_seq)) = row else {
            return Ok(true);
        };
        if unsigned(revision)? != target.expected_revision
            || status == "forgotten"
            || unsigned(accepted_input_seq)? > owner_input_seq
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[must_use]
pub struct PendingControl {
    pub(super) actor: String,
    pub(super) row: receipt::Model,
    pub(super) metadata_delta: i64,
    pub(super) original_runtime: runtime::Model,
    pub(super) before_memories: Vec<memory::Model>,
    pub(super) before_sources: Vec<Vec<source::Model>>,
    pub(super) before_revisions: Vec<Vec<revision::Model>>,
    pub(super) expected_runtime: runtime::Model,
    pub(super) originals: Vec<(String, i64)>,
    pub(super) expected_memories: Vec<memory::Model>,
    pub(super) expected_sources: Vec<Vec<source::Model>>,
    pub(super) expected_revisions: Vec<Vec<revision::Model>>,
    pub(super) expected_tombstones: Vec<tombstone::Model>,
    pub(super) expected_suppressions: SuppressionImages,
}
impl PendingControl {
    /// This original view is limited to finalizing this already-admitted control.
    /// Every caller still issues its original SELECT and supplies the actual row.
    pub fn runtime_before(
        &self,
        current: runtime::Model,
    ) -> Result<runtime::Model, AssistantStoreError> {
        if self.row.tool_group_id.is_none() || current != self.expected_runtime {
            return Err(AssistantStoreError::Inaccessible);
        }
        Ok(self.original_runtime.clone())
    }
    pub fn head_before(
        &self,
        current: memory::Model,
    ) -> Result<memory::Model, AssistantStoreError> {
        original_row(
            current,
            self.before_memories.iter().zip(&self.expected_memories),
            |row| row.id,
        )
    }
    pub fn version_before(
        &self,
        current: revision::Model,
    ) -> Result<revision::Model, AssistantStoreError> {
        original_row(
            current,
            self.before_revisions
                .iter()
                .flatten()
                .zip(self.expected_revisions.iter().flatten()),
            |row| row.id,
        )
    }
    pub fn source_before(
        &self,
        current: source::Model,
    ) -> Result<source::Model, AssistantStoreError> {
        original_row(
            current,
            self.before_sources
                .iter()
                .flatten()
                .zip(self.expected_sources.iter().flatten()),
            |row| row.id,
        )
    }
    pub fn owns_suppression(&self, source: &KnownMemorySource) -> bool {
        self.expected_suppressions.values().any(|row| {
            row.source_id == source.source_id
                && u64::try_from(row.source_revision).ok() == Some(source.revision)
                && row.origin_id == source.origin_id
        })
    }
    pub fn revokes_usage(
        &self,
        usage: &desk_diagnose_core::ai_assistant::seam::AssistantDataReadUse,
    ) -> bool {
        usage.memories.iter().any(|memory| {
            self.originals.iter().any(|(id, through)| {
                memory.memory_id == *id
                    && i64::try_from(memory.revision).is_ok_and(|revision| revision <= *through)
            })
        }) || usage.history_dependencies.iter().any(|source| {
            self.owns_suppression(&KnownMemorySource {
                source_id: source.source_id.clone(),
                revision: source.source_revision,
                origin_id: source.origin_id,
            })
        })
    }
    pub fn revocation_time(&self) -> i64 {
        self.row.created_at_ms
    }
    pub fn receipt(&self) -> Result<AssistantDataControlReceipt, AssistantStoreError> {
        project_receipt(&self.row, false)
    }

    pub async fn bind_tool_group_on<C: ConnectionTrait>(
        &mut self,
        db: &C,
        producer_message_id: &str,
    ) -> Result<(), AssistantStoreError> {
        if self.row.tool_group_id.as_deref() != Some(producer_message_id)
            || self.row.tool_group_bound
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        self.validate_owned_on(db).await?;
        let old_bytes = receipt_metadata_bytes(&self.row)?;
        let mut expected = self.row.clone();
        expected.tool_group_bound = true;

        let mut active: receipt::ActiveModel = self.row.clone().into();
        active.tool_group_bound = Set(true);

        if active.update(db).await? != expected {
            return Err(AssistantStoreError::Inaccessible);
        }
        self.metadata_delta = self
            .metadata_delta
            .checked_add(receipt_metadata_bytes(&expected)? - old_bytes)
            .ok_or(AssistantStoreError::Capacity)?;
        self.row = expected;
        self.validate_owned_on(db).await
    }

    pub async fn validate_owned_on<C: ConnectionTrait>(
        &self,
        db: &C,
    ) -> Result<(), AssistantStoreError> {
        if runtime_on(db, &self.actor).await? != self.expected_runtime
            || receipt::Entity::find_by_id(self.row.id)
                .one(db)
                .await?
                .as_ref()
                != Some(&self.row)
            || receipt::Entity::find()
                .filter(receipt::Column::ActorId.eq(&self.actor))
                .filter(receipt::Column::OperationId.eq(&self.row.operation_id))
                .one(db)
                .await?
                .as_ref()
                != Some(&self.row)
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        for (index, (memory_id, revision)) in self.originals.iter().enumerate() {
            if memory::Entity::find()
                .filter(memory::Column::ActorId.eq(&self.actor))
                .filter(memory::Column::MemoryId.eq(memory_id))
                .one(db)
                .await?
                .as_ref()
                != Some(&self.expected_memories[index])
                || sources_on(db, &self.actor, memory_id, *revision).await?
                    != self.expected_sources[index]
                || revisions_on(db, &self.actor, memory_id, *revision).await?
                    != self.expected_revisions[index]
            {
                return Err(AssistantStoreError::Inaccessible);
            }
        }
        let stones = tombstone::Entity::find()
            .filter(tombstone::Column::ActorId.eq(&self.actor))
            .filter(tombstone::Column::OperationId.eq(&self.row.operation_id))
            .order_by_asc(tombstone::Column::Id)
            .all(db)
            .await?;
        if stones != self.expected_tombstones {
            return Err(AssistantStoreError::Inaccessible);
        }
        let expected = self
            .expected_suppressions
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for chunk in expected.chunks(128) {
            let known = chunk
                .iter()
                .map(known_source)
                .collect::<Result<Vec<_>, _>>()?;
            let current =
                super::super::suppression::matching_rows_on(db, &self.actor, &known).await?;
            if image_map(current)? != image_map(chunk.to_vec())? {
                return Err(AssistantStoreError::Inaccessible);
            }
        }

        Ok(())
    }

    pub async fn finalize_on<C: ConnectionTrait>(
        self,
        db: &C,
    ) -> Result<AssistantDataControlReceipt, AssistantStoreError> {
        if self.row.tool_group_id.is_some() != self.row.tool_group_bound {
            return Err(AssistantStoreError::Inaccessible);
        }
        self.validate_owned_on(db).await?;
        // Controls bypass learning caps; ordinary admission reserves room for them.
        super::super::projection::adjust_metadata_on(db, &self.actor, self.metadata_delta, None)
            .await?;
        project_receipt(&self.row, false)
    }
}

pub(super) async fn sources_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    memory_id: &str,
    before: i64,
) -> Result<Vec<source::Model>, AssistantStoreError> {
    Ok(source::Entity::find()
        .filter(source::Column::ActorId.eq(actor))
        .filter(source::Column::MemoryId.eq(memory_id))
        .filter(source::Column::MemoryRevision.lte(before))
        .order_by_asc(source::Column::Id)
        .all(db)
        .await?)
}
pub(super) async fn revisions_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    memory_id: &str,
    before: i64,
) -> Result<Vec<revision::Model>, AssistantStoreError> {
    Ok(revision::Entity::find()
        .filter(revision::Column::ActorId.eq(actor))
        .filter(revision::Column::MemoryId.eq(memory_id))
        .filter(revision::Column::Revision.lte(before))
        .order_by_asc(revision::Column::Id)
        .all(db)
        .await?)
}

use crate::entity::agent_assistant_source_suppression as source_suppression;
type SuppressionKey = (String, i64, i64);
pub(super) type SuppressionImages =
    std::collections::BTreeMap<SuppressionKey, source_suppression::Model>;
fn image_key(row: &source_suppression::Model) -> SuppressionKey {
    (row.source_id.clone(), row.source_revision, row.origin_id)
}
fn image_map(
    rows: Vec<source_suppression::Model>,
) -> Result<SuppressionImages, AssistantStoreError> {
    let mut mapped = SuppressionImages::new();
    for row in rows {
        if mapped.insert(image_key(&row), row).is_some() {
            return Err(AssistantStoreError::Inaccessible);
        }
    }
    Ok(mapped)
}
fn known_source(row: &source_suppression::Model) -> Result<KnownMemorySource, AssistantStoreError> {
    Ok(KnownMemorySource {
        source_id: row.source_id.clone(),
        revision: unsigned(row.source_revision)?,
        origin_id: row.origin_id,
    })
}

pub(super) async fn suppress_sources_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    known: &[KnownMemorySource],
    cutoff: i64,
    generation: i64,
    now: i64,
    owned: &mut SuppressionImages,
) -> Result<i64, AssistantStoreError> {
    let mut original =
        image_map(super::super::suppression::matching_rows_on(db, actor, known).await?)?;
    let mut expected = SuppressionImages::new();
    for source in known {
        let key = (
            source.source_id.clone(),
            signed(source.revision)?,
            source.origin_id,
        );
        let current = original.remove(&key);
        if owned
            .get(&key)
            .is_some_and(|previous| current.as_ref() != Some(previous))
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        let mut next = current.unwrap_or(source_suppression::Model {
            id: 0,
            actor_id: actor.into(),
            source_id: source.source_id.clone(),
            source_revision: signed(source.revision)?,
            origin_id: source.origin_id,
            cutoff_input_seq: cutoff,
            control_generation: generation,
            created_at_ms: now,
        });
        next.cutoff_input_seq = cutoff;
        next.control_generation = generation;
        expected.insert(key, next);
    }
    if !original.is_empty() {
        return Err(AssistantStoreError::Inaccessible);
    }
    let bytes =
        super::super::suppression::suppress_on(db, actor, known, cutoff, generation, now).await?;
    let mut actual =
        image_map(super::super::suppression::matching_rows_on(db, actor, known).await?)?;
    for (key, mut expected) in expected {
        let current = actual
            .remove(&key)
            .ok_or(AssistantStoreError::Inaccessible)?;
        if expected.id == 0 {
            expected.id = current.id;
        }
        if current.id <= 0 || current != expected {
            return Err(AssistantStoreError::Inaccessible);
        }
        owned.insert(key, expected);
    }
    if !actual.is_empty() {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(bytes)
}

fn original_row<'a, T: Clone + PartialEq + 'a>(
    current: T,
    rows: impl Iterator<Item = (&'a T, &'a T)>,
    id: impl Fn(&T) -> i64,
) -> Result<T, AssistantStoreError> {
    if let Some((before, after)) = rows
        .into_iter()
        .find(|(_, after)| id(after) == id(&current))
    {
        if after != &current {
            return Err(AssistantStoreError::Inaccessible);
        }
        Ok(before.clone())
    } else {
        Ok(current)
    }
}
