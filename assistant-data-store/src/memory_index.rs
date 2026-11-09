use crate::entity::{agent_assistant_origin as origin, agent_memory as memory};
use crate::entity::{
    agent_assistant_search_document as document, agent_assistant_search_term as term,
};
use crate::{AssistantStoreError, memory_quota as quota, signed};
use desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicy;
use desk_diagnose_core::ai_assistant::retrieval::index_source;
use desk_diagnose_core::native_ids::{self, Kind};
use quota::FactQuotaDelta;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QuerySelect,
};
use std::collections::BTreeSet;

pub struct PreparedIndex {
    pub delta: FactQuotaDelta,
    removed_sources: Vec<(String, String)>,
    old_documents: Vec<document::Model>,
    old_terms: Vec<term::Model>,
    documents: Vec<document::Model>,
    terms: Vec<term::Model>,
}

/// Database-assigned IDs are retained only after all projected logical fields
/// match. Deleted identities are semantic keys because SQLite can reuse row IDs.
pub struct IndexPostImage {
    removed: Vec<(String, String)>,
    source_keys: BTreeSet<(String, String)>,
    documents: Vec<document::Model>,
    terms: Vec<term::Model>,
}
impl IndexPostImage {
    pub async fn validate_on<C: ConnectionTrait>(&self, db: &C) -> Result<(), AssistantStoreError> {
        for (actor, id) in &self.removed {
            if document::Entity::find()
                .filter(document::Column::ActorId.eq(actor))
                .filter(document::Column::DocumentId.eq(id))
                .one(db)
                .await?
                .is_some()
                || term::Entity::find()
                    .filter(term::Column::ActorId.eq(actor))
                    .filter(term::Column::DocumentId.eq(id))
                    .one(db)
                    .await?
                    .is_some()
            {
                return Err(AssistantStoreError::Inaccessible);
            }
        }
        for (actor, source_id) in &self.source_keys {
            let mut expected = self
                .documents
                .iter()
                .filter(|row| &row.actor_id == actor && &row.source_id == source_id)
                .collect::<Vec<_>>();
            let mut current = document::Entity::find()
                .filter(document::Column::ActorId.eq(actor))
                .filter(document::Column::SourceKind.eq("memory"))
                .filter(document::Column::SourceId.eq(source_id))
                .limit(expected.len() as u64 + 1)
                .all(db)
                .await?;
            current.sort_by_key(|row| row.id);
            expected.sort_by_key(|row| row.id);
            if current.len() != expected.len()
                || current
                    .iter()
                    .zip(expected)
                    .any(|(actual, expected)| actual != expected)
            {
                return Err(AssistantStoreError::Inaccessible);
            }
        }
        for expected in &self.documents {
            if document::Entity::find_by_id(expected.id)
                .one(db)
                .await?
                .as_ref()
                != Some(expected)
            {
                return Err(AssistantStoreError::Inaccessible);
            }
            let mut terms = self
                .terms
                .iter()
                .filter(|row| row.document_id == expected.document_id)
                .collect::<Vec<_>>();
            let mut current = term::Entity::find()
                .filter(term::Column::ActorId.eq(&expected.actor_id))
                .filter(term::Column::DocumentId.eq(&expected.document_id))
                .limit(terms.len() as u64 + 1)
                .all(db)
                .await?;
            current.sort_by_key(|row| row.id);
            terms.sort_by_key(|row| row.id);
            if current.len() != terms.len()
                || current
                    .iter()
                    .zip(terms)
                    .any(|(actual, expected)| actual != expected)
            {
                return Err(AssistantStoreError::Inaccessible);
            }
        }
        Ok(())
    }
}

/// Prepared content and provenance were validated by the caller under its native fence.
pub struct MemoryIndexInput<'a> {
    pub next: &'a memory::Model,
    pub removed: BTreeSet<String>,
    pub text: &'a str,
    pub source_origin_id: &'a String,
    pub origin: &'a origin::Model,
    pub policy: &'a AssistantDataPolicy,
    pub body_expiry: Option<i64>,
}
pub async fn prepare_on<C: ConnectionTrait>(
    db: &C,
    input: MemoryIndexInput<'_>,
) -> Result<PreparedIndex, AssistantStoreError> {
    let MemoryIndexInput {
        next,
        removed,
        text,
        source_origin_id,
        origin,
        policy,
        body_expiry,
    } = input;
    let mut prepared = prepare_removed_on(db, &next.actor_id, removed).await?;
    let mut delta = prepared.delta;
    let projected = index_source(text).map_err(AssistantStoreError::Invalid)?;
    let mut documents = Vec::new();
    let mut terms = Vec::new();
    for part in projected.fragments {
        let document_id = native_ids::stable(
            Kind::AssistantSearchDocument,
            &(
                "memory-index-v1",
                &next.memory_id,
                next.revision,
                part.fragment_index,
            ),
        );
        let doc = document::Model {
            id: 0,
            document_id: document_id.clone(),
            actor_id: next.actor_id.clone(),
            device_ref: next.device_ref.clone(),
            source_kind: "memory".into(),
            source_id: next.memory_id.clone(),
            history_id: None,
            source_revision: next.revision,
            corpus_generation: next.state_generation,
            origin_id: source_origin_id.clone(),
            root_conversation_id: origin.root_conversation_id.clone(),
            conversation_id: origin.conversation_id.clone(),
            fragment_index: part
                .fragment_index
                .try_into()
                .map_err(|_| AssistantStoreError::Capacity)?,
            byte_start: part
                .byte_start
                .try_into()
                .map_err(|_| AssistantStoreError::Capacity)?,
            byte_end: part
                .byte_end
                .try_into()
                .map_err(|_| AssistantStoreError::Capacity)?,
            token_count: part
                .token_count
                .try_into()
                .map_err(|_| AssistantStoreError::Capacity)?,
            policy_revision: signed(policy.revision)?,
            partial: projected.partial,
            expires_at_ms: body_expiry,
        };
        delta.metadata_bytes = delta
            .metadata_bytes
            .checked_add(quota::document_metadata(&doc)?)
            .ok_or(AssistantStoreError::Capacity)?;
        for (text, frequency) in part.terms {
            let row = term::Model {
                id: 0,
                actor_id: next.actor_id.clone(),
                document_id: document_id.clone(),
                term: text,
                frequency: i32::try_from(frequency).map_err(|_| AssistantStoreError::Capacity)?,
                source_revision: next.revision,
            };
            delta.metadata_bytes = delta
                .metadata_bytes
                .checked_add(quota::term_metadata(&row)?)
                .ok_or(AssistantStoreError::Capacity)?;
            terms.push(row);
        }
        documents.push(doc);
    }
    delta.term_records = delta
        .term_records
        .checked_add(terms.len() as i64)
        .ok_or(AssistantStoreError::Capacity)?;
    prepared.delta = delta;
    prepared.documents = documents;
    prepared.terms = terms;
    Ok(prepared)
}

pub async fn prepare_removal_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    removed: BTreeSet<String>,
) -> Result<PreparedIndex, AssistantStoreError> {
    prepare_removed_on(db, actor, removed).await
}

async fn prepare_removed_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    removed: BTreeSet<String>,
) -> Result<PreparedIndex, AssistantStoreError> {
    let old_documents = if removed.is_empty() {
        Vec::new()
    } else {
        document::Entity::find()
            .filter(document::Column::ActorId.eq(actor))
            .filter(document::Column::SourceKind.eq("memory"))
            .filter(document::Column::SourceId.is_in(removed.iter().cloned()))
            .all(db)
            .await?
    };
    let mut delta = FactQuotaDelta::default();
    let mut old_terms = Vec::new();
    for doc in &old_documents {
        let rows = term::Entity::find()
            .filter(term::Column::ActorId.eq(actor))
            .filter(term::Column::DocumentId.eq(&doc.document_id))
            .all(db)
            .await?;
        delta.metadata_bytes = delta
            .metadata_bytes
            .checked_sub(quota::document_metadata(doc)?)
            .ok_or(AssistantStoreError::Capacity)?;
        for row in &rows {
            delta.metadata_bytes = delta
                .metadata_bytes
                .checked_sub(quota::term_metadata(row)?)
                .ok_or(AssistantStoreError::Capacity)?;
        }
        delta.term_records = delta
            .term_records
            .checked_sub(rows.len() as i64)
            .ok_or(AssistantStoreError::Capacity)?;
        old_terms.extend(rows);
    }
    Ok(PreparedIndex {
        delta,
        removed_sources: removed
            .into_iter()
            .map(|id| (actor.to_owned(), id))
            .collect(),
        old_documents,
        old_terms,
        documents: Vec::new(),
        terms: Vec::new(),
    })
}

pub async fn validate_original_on<C: ConnectionTrait>(
    db: &C,
    prepared: &PreparedIndex,
) -> Result<(), AssistantStoreError> {
    for row in &prepared.old_documents {
        if document::Entity::find_by_id(row.id).one(db).await?.as_ref() != Some(row) {
            return Err(AssistantStoreError::Inaccessible);
        }
        let mut expected = prepared
            .old_terms
            .iter()
            .filter(|term| term.document_id == row.document_id)
            .collect::<Vec<_>>();
        let mut current = term::Entity::find()
            .filter(term::Column::ActorId.eq(&row.actor_id))
            .filter(term::Column::DocumentId.eq(&row.document_id))
            .limit(expected.len() as u64 + 1)
            .all(db)
            .await?;
        current.sort_by_key(|term| term.id);
        expected.sort_by_key(|term| term.id);
        if current.len() != expected.len()
            || current
                .iter()
                .zip(expected)
                .any(|(current, expected)| current != expected)
        {
            return Err(AssistantStoreError::Inaccessible);
        }
    }
    Ok(())
}

pub async fn apply_on<C: ConnectionTrait>(
    db: &C,
    prepared: PreparedIndex,
) -> Result<IndexPostImage, AssistantStoreError> {
    let mut removed_images = Vec::with_capacity(prepared.old_documents.len());
    // Re-read each complete original document/term set before refunding it.
    // This catches added or changed terms without one SQL round trip per token.
    for row in prepared.old_documents {
        let current = document::Entity::find_by_id(row.id)
            .one(db)
            .await?
            .ok_or(AssistantStoreError::Inaccessible)?;
        if current != row {
            return Err(AssistantStoreError::Inaccessible);
        }
        let mut expected = prepared
            .old_terms
            .iter()
            .filter(|term| term.document_id == row.document_id)
            .collect::<Vec<_>>();
        let mut terms = term::Entity::find()
            .filter(term::Column::ActorId.eq(&row.actor_id))
            .filter(term::Column::DocumentId.eq(&row.document_id))
            .limit(expected.len() as u64 + 1)
            .all(db)
            .await?;
        terms.sort_by_key(|term| term.id);
        expected.sort_by_key(|term| term.id);
        if terms.len() != expected.len()
            || terms
                .iter()
                .zip(expected)
                .any(|(current, original)| current != original)
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        let removed = term::Entity::delete_many()
            .filter(term::Column::ActorId.eq(&row.actor_id))
            .filter(term::Column::DocumentId.eq(&row.document_id))
            .exec(db)
            .await?;
        if removed.rows_affected != terms.len() as u64 {
            return Err(AssistantStoreError::Inaccessible);
        }
        let current = document::Entity::find_by_id(row.id)
            .one(db)
            .await?
            .ok_or(AssistantStoreError::Inaccessible)?;
        if current != row {
            return Err(AssistantStoreError::Inaccessible);
        }
        let changed = document::Entity::delete_by_id(row.id).exec(db).await?;
        if changed.rows_affected != 1 {
            return Err(AssistantStoreError::Inaccessible);
        }
        removed_images.push((row.actor_id, row.document_id));
    }
    let mut documents = Vec::with_capacity(prepared.documents.len());
    for mut expected in prepared.documents {
        let mut active: document::ActiveModel = expected.clone().into();
        active.id = sea_orm::ActiveValue::NotSet;
        let row = active.insert(db).await?;
        expected.id = row.id;
        if row != expected {
            return Err(AssistantStoreError::Inaccessible);
        }
        documents.push(expected);
    }
    let mut expected_terms = prepared.terms;
    for chunk in expected_terms.chunks(100) {
        let rows = chunk
            .iter()
            .cloned()
            .map(|row| {
                let mut active: term::ActiveModel = row.into();
                active.id = sea_orm::ActiveValue::NotSet;
                active
            })
            .collect::<Vec<_>>();
        term::Entity::insert_many(rows).exec(db).await?;
    }
    for document in &documents {
        let mut expected = expected_terms
            .iter_mut()
            .filter(|row| row.document_id == document.document_id)
            .collect::<Vec<_>>();
        let mut actual = term::Entity::find()
            .filter(term::Column::ActorId.eq(&document.actor_id))
            .filter(term::Column::DocumentId.eq(&document.document_id))
            .limit(expected.len() as u64 + 1)
            .all(db)
            .await?;
        actual.sort_by(|a, b| a.term.cmp(&b.term));
        expected.sort_by(|a, b| a.term.cmp(&b.term));
        if actual.len() != expected.len() {
            return Err(AssistantStoreError::Inaccessible);
        }
        for (actual, expected) in actual.into_iter().zip(expected) {
            expected.id = actual.id;
            if *expected != actual {
                return Err(AssistantStoreError::Inaccessible);
            }
        }
    }
    let mut source_keys = prepared
        .removed_sources
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    source_keys.extend(
        documents
            .iter()
            .map(|row| (row.actor_id.clone(), row.source_id.clone())),
    );
    let image = IndexPostImage {
        removed: removed_images,
        source_keys,
        documents,
        terms: expected_terms,
    };
    image.validate_on(db).await?;
    Ok(image)
}
