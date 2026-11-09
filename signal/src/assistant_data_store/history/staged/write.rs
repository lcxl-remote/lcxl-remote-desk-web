use super::*;
use desk_diagnose_core::native_ids::{self, Kind};

/// `sdoc_` plus 22 base64url characters.
const SEARCH_DOCUMENT_ID_LEN: usize = 27;
use sea_orm::sea_query::Expr;

/// All original source SELECTs are complete. Only quota/coverage metadata can
/// be reread here; source rows come from the original inspection or own writes.
#[allow(clippy::too_many_arguments)]
pub(super) async fn write_on(
    txn: &DatabaseTransaction,
    capture: &HistoryCapture<'_>,
    prepared: &PreparedHistoryCapture,
    inspected: &Inspection,
    runtime_row: runtime::Model,
    existing_conversation: Option<conversation::Model>,
    existing_binding: Option<binding::Model>,
    policy: &AssistantDataPolicy,
) -> Result<Written, AdmissionFailure> {
    let recorded = signed(capture.recorded_at_unix_ms)?;
    let expires = signed(prepared.proof.expires_at_unix_ms)?;
    let kind = enum_string(&capture.kind)?;
    let source_policy = json(capture.source_policy)?;
    let dependencies = json(&capture.dependencies)?;
    let retention_proof = json(&prepared.proof)?;
    let original_sha256 = format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(capture.content.as_bytes())
    );
    let state_json = capture.native.map(json).transpose()?;
    let native_time = capture
        .native
        .map(|value| value.columns())
        .unwrap_or_default();
    let native_action_at = native_time.at_unix_ms.map(signed).transpose()?;
    let native_action_from = native_time.from_unix_ms.map(signed).transpose()?;
    let native_action_until = native_time.until_unix_ms.map(signed).transpose()?;
    let existing_origin = inspected.existing_origin.clone();
    let admitted_generation = increment(runtime_row.history_data_generation)?;
    let new_parts = inspected
        .new_parts
        .iter()
        .map(|index| {
            let part = &prepared.index.fragments[*index];
            (
                part,
                &prepared.retained_content[part.byte_start as usize..part.byte_end as usize],
            )
        })
        .collect::<Vec<_>>();
    // These identities are located before acquiring the quota row. Subsequent
    // writes touch only history-owned tables, never native task/session state.
    ensure_quota_on(txn, capture.actor_id).await?;
    ensure_coverage_slots_on(txn, capture.actor_id, expires).await?;
    let mut query = quota::Entity::find().filter(quota::Column::ActorId.eq(capture.actor_id));
    if txn.get_database_backend() == DatabaseBackend::Postgres {
        query = query.lock_exclusive();
    }
    let row = query
        .one(txn)
        .await?
        .ok_or(AssistantStoreError::Invalid("missing assistant quota row"))?;
    let records = 1_i64; // Count the native source once, independently of its fragments.
    let bytes = new_parts
        .iter()
        .map(|(_, body)| body.len() as i64)
        .sum::<i64>();
    let terms = new_parts
        .iter()
        .map(|(part, _)| part.terms.len() as i64)
        .sum::<i64>();
    let mut metadata = 0_i64;
    for (part, _) in &new_parts {
        // Account UTF-8 metadata plus fixed-width scalar slots. Payload fields
        // and indexed terms have their own counters and are not hidden here.
        metadata += (prepared.origin_id.len()
            + capture.actor_id.len()
            + capture.device_ref.len()
            + capture.conversation_id.len()
            + capture.root_conversation_id.len()
            + capture.native_id.len()
            + kind.len()
            + capture.role.len()
            + dependencies.len()
            + retention_proof.len()
            + capture.statement_timezone.len()
            + state_json.as_ref().map_or(0, String::len)
            + native_time.basis.as_ref().map_or(0, String::len)
            + 17 * 8) as i64;
        metadata += part
            .terms
            .keys()
            .map(|text| text.len() as i64 + capture.actor_id.len() as i64 + 24)
            .sum::<i64>();
        // The document row stores the formatted source id and its document id.
        metadata += (desk_agent_protocol::ai_assistant::source_ref::SourceRef::MAX_LEN
            + SEARCH_DOCUMENT_ID_LEN
            + prepared.origin_id.len()
            + capture.root_conversation_id.len()
            + capture.conversation_id.len()
            + capture.device_ref.len()
            + capture.actor_id.len()
            + 12 * 8) as i64;
    }
    if existing_origin.is_none() {
        metadata += (prepared.origin_id.len()
            + capture.actor_id.len()
            + capture.device_ref.len()
            + capture.conversation_id.len()
            + capture.root_conversation_id.len()
            + capture.native_id.len()
            + kind.len()
            + original_sha256.len()
            + source_policy.len()
            + capture.statement_timezone.len()
            + 6 * 8) as i64;
    }
    if existing_conversation.is_none() {
        metadata += (capture.actor_id.len()
            + capture.device_ref.len()
            + capture.conversation_id.len()
            + capture.root_conversation_id.len()
            + 6 * 8
            + "active".len()) as i64;
    }
    if let Some(directory) = capture.directory.filter(|_| existing_binding.is_none()) {
        metadata += (capture.device_ref.len()
            + directory.instance_id.len()
            + directory.audience.len()
            + directory.device_name.map_or(0, str::len)
            + 3 * 8) as i64;
    }
    let next_records = add(row.history_records, records)?;
    let next_bytes = add(row.history_body_bytes, bytes)?;
    let next_terms = add(row.term_records, terms)?;
    let next_metadata = add(row.metadata_bytes, metadata)?;
    if next_records as u64 > policy.max_history_records
        || next_bytes as u64 > policy.max_history_body_bytes
        || next_terms as u64 > policy.max_term_records
        || next_metadata as u64 > policy.max_metadata_bytes
    {
        return Err(AdmissionFailure::Capacity);
    }
    let changed = quota::Entity::update_many()
        .col_expr(quota::Column::HistoryRecords, Expr::value(next_records))
        .col_expr(quota::Column::HistoryBodyBytes, Expr::value(next_bytes))
        .col_expr(quota::Column::TermRecords, Expr::value(next_terms))
        .col_expr(quota::Column::MetadataBytes, Expr::value(next_metadata))
        .col_expr(
            quota::Column::Revision,
            Expr::value(increment(row.revision)?),
        )
        .filter(quota::Column::Id.eq(row.id))
        .filter(quota::Column::ActorId.eq(&row.actor_id))
        .filter(quota::Column::Revision.eq(row.revision))
        .filter(quota::Column::MetadataBytes.eq(row.metadata_bytes))
        .filter(quota::Column::HistoryRecords.eq(row.history_records))
        .filter(quota::Column::HistoryBodyBytes.eq(row.history_body_bytes))
        .filter(quota::Column::TermRecords.eq(row.term_records))
        .exec(txn)
        .await?;
    require_one(changed.rows_affected)?;
    let mut runtime_update = runtime::Entity::update_many()
        .col_expr(
            runtime::Column::HistoryDataGeneration,
            Expr::value(admitted_generation),
        )
        .col_expr(
            runtime::Column::Revision,
            Expr::value(increment(runtime_row.revision)?),
        )
        .filter(runtime::Column::Id.eq(runtime_row.id))
        .filter(runtime::Column::ActorId.eq(&runtime_row.actor_id))
        .filter(runtime::Column::Revision.eq(runtime_row.revision))
        .filter(runtime::Column::InputSeq.eq(runtime_row.input_seq))
        .filter(runtime::Column::HistoryDataGeneration.eq(runtime_row.history_data_generation))
        .filter(runtime::Column::ControlGeneration.eq(runtime_row.control_generation))
        .filter(runtime::Column::MemoryStateGeneration.eq(runtime_row.memory_state_generation))
        .filter(runtime::Column::LearningCutoffInputSeq.eq(runtime_row.learning_cutoff_input_seq))
        .filter(runtime::Column::SettingsRevisionSeen.eq(runtime_row.settings_revision_seen));
    runtime_update = match &runtime_row.settings_instance_id {
        Some(value) => runtime_update.filter(runtime::Column::SettingsInstanceId.eq(value)),
        None => runtime_update.filter(runtime::Column::SettingsInstanceId.is_null()),
    };
    require_one(runtime_update.exec(txn).await?.rows_affected)?;
    let mut next_runtime = runtime_row.clone();
    next_runtime.history_data_generation = admitted_generation;
    next_runtime.revision = increment(runtime_row.revision)?;
    let next_binding = match capture.directory {
        Some(directory) => Some(
            desk_diagnose_core::future::send_phase(|| {
                write_binding_on(txn, capture, directory, existing_binding, recorded)
            })
            .await?,
        ),
        None => None,
    };
    if existing_origin.is_none() {
        require_one(
            origin::Entity::insert(origin::ActiveModel {
                origin_id: Set(prepared.origin_id.clone()),
                actor_id: Set(capture.actor_id.into()),
                device_ref: Set(capture.device_ref.into()),
                conversation_id: Set(capture.conversation_id.into()),
                root_conversation_id: Set(capture.root_conversation_id.into()),
                native_id: Set(capture.native_id.into()),
                kind: Set(kind.clone()),
                original_sha256: Set(original_sha256.clone()),
                accepted_input_seq: Set(signed(capture.accepted_input_seq)?),
                history_admitted: Set(true),
                deletion_generation: Set(1),
                deleted_at_ms: Set(None),
                policy_json: Set(source_policy),
                created_at_ms: Set(recorded),
                statement_timezone: Set(capture.statement_timezone.into()),
                ..Default::default()
            })
            .exec_without_returning(txn)
            .await?,
        )?;
    } else if let Some(current) = existing_origin {
        require_one(
            origin::Entity::update_many()
                .col_expr(origin::Column::HistoryAdmitted, Expr::value(true))
                .filter(origin::Column::Id.eq(current.id))
                .filter(origin::Column::ActorId.eq(&current.actor_id))
                .filter(origin::Column::OriginId.eq(&current.origin_id))
                .filter(origin::Column::DeviceRef.eq(&current.device_ref))
                .filter(origin::Column::ConversationId.eq(&current.conversation_id))
                .filter(origin::Column::RootConversationId.eq(&current.root_conversation_id))
                .filter(origin::Column::NativeId.eq(&current.native_id))
                .filter(origin::Column::Kind.eq(&current.kind))
                .filter(origin::Column::OriginalSha256.eq(&current.original_sha256))
                .filter(origin::Column::PolicyJson.eq(&current.policy_json))
                .filter(origin::Column::AcceptedInputSeq.eq(current.accepted_input_seq))
                .filter(origin::Column::HistoryAdmitted.eq(current.history_admitted))
                .filter(origin::Column::DeletionGeneration.eq(current.deletion_generation))
                .filter(origin::Column::DeletedAtMs.is_null())
                .filter(origin::Column::CreatedAtMs.eq(current.created_at_ms))
                .filter(origin::Column::StatementTimezone.eq(&current.statement_timezone))
                .exec(txn)
                .await?
                .rows_affected,
        )?;
    }
    let next_conversation = desk_diagnose_core::future::send_phase(|| {
        write_conversation_on(txn, capture, existing_conversation, recorded)
    })
    .await?;
    let mut history_ids = Vec::with_capacity(new_parts.len());
    for (part, body) in new_parts {
        let history_id = history::Entity::insert(history::ActiveModel {
            origin_id: Set(prepared.origin_id.clone()),
            actor_id: Set(capture.actor_id.into()),
            device_ref: Set(capture.device_ref.into()),
            conversation_id: Set(capture.conversation_id.into()),
            root_conversation_id: Set(capture.root_conversation_id.into()),
            native_id: Set(capture.native_id.into()),
            fragment_index: Set(
                i32::try_from(part.fragment_index).map_err(|_| AdmissionFailure::Capacity)?
            ),
            kind: Set(kind.clone()),
            role: Set(capture.role.into()),
            content: Set(body.into()),
            revision: Set(signed(capture.revision)?),
            admitted_generation: Set(admitted_generation),
            recorded_at_ms: Set(recorded),
            statement_timezone: Set(capture.statement_timezone.into()),
            native_action_at_ms: Set(native_action_at),
            native_action_from_ms: Set(native_action_from),
            native_action_until_ms: Set(native_action_until),
            time_basis: Set(native_time.basis.clone()),
            state_json: Set(state_json.clone()),
            dependencies_json: Set(dependencies.clone()),
            retention_proof_json: Set(retention_proof.clone()),
            expires_at_ms: Set(expires),
            admission: Set(if prepared.redacted {
                "redacted"
            } else {
                "indexed"
            }
            .into()),
            deleted_at_ms: Set(None),
            created_at_ms: Set(recorded),
            updated_at_ms: Set(recorded),
            ..Default::default()
        })
        .exec(txn)
        .await?
        .last_insert_id;
        history_ids.push((part.fragment_index, history_id));
        let source_id =
            desk_agent_protocol::ai_assistant::source_ref::SourceRef::history(history_id)
                .to_string();
        let document_id = native_ids::stable(
            Kind::AssistantSearchDocument,
            &("search-document", &source_id, capture.revision),
        );
        require_one(
            document::Entity::insert(document::ActiveModel {
                document_id: Set(document_id.clone()),
                actor_id: Set(capture.actor_id.into()),
                device_ref: Set(capture.device_ref.into()),
                source_kind: Set("history".into()),
                source_id: Set(source_id),
                history_id: Set(Some(history_id)),
                source_revision: Set(signed(capture.revision)?),
                corpus_generation: Set(admitted_generation),
                origin_id: Set(prepared.origin_id.clone()),
                root_conversation_id: Set(capture.root_conversation_id.into()),
                conversation_id: Set(capture.conversation_id.into()),
                fragment_index: Set(i32::try_from(part.fragment_index).map_err(|_| AdmissionFailure::Capacity)?),
                byte_start: Set(i32::try_from(part.byte_start).map_err(|_| AdmissionFailure::Capacity)?),
                byte_end: Set(i32::try_from(part.byte_end).map_err(|_| AdmissionFailure::Capacity)?),
                token_count: Set(i32::try_from(part.token_count).map_err(|_| AdmissionFailure::Capacity)?),
                policy_revision: Set(signed(capture.source_policy.policy_revision)?),
                partial: Set(prepared.index.partial),
                expires_at_ms: Set(Some(expires)),
                ..Default::default()
            })
            .exec_without_returning(txn)
            .await?,
        )?;
        // Keep SQLite and PostgreSQL bind counts bounded for high-cardinality text.
        let rows = part
            .terms
            .iter()
            .map(|(text, frequency)| term::ActiveModel {
                actor_id: Set(capture.actor_id.into()),
                document_id: Set(document_id.clone()),
                term: Set(text.clone()),
                frequency: Set(*frequency as i32),
                source_revision: Set(capture.revision as i64),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        for chunk in rows.chunks(100) {
            term::Entity::insert_many(chunk.iter().cloned())
                .exec(txn)
                .await?;
        }
    }
    update_coverage_on(
        txn,
        capture,
        expires,
        if prepared.index.partial {
            "partial"
        } else {
            "indexed"
        },
        records,
    )
    .await?;
    let history_ids = prepared
        .index
        .fragments
        .iter()
        .zip(&inspected.existing_ids)
        .map(|(part, existing)| {
            existing
                .or_else(|| {
                    history_ids
                        .iter()
                        .find(|(fragment, _)| *fragment == part.fragment_index)
                        .map(|(_, id)| *id)
                })
                .ok_or(AssistantStoreError::Inaccessible)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Written {
        history_ids,
        runtime: next_runtime,
        conversation: next_conversation,
        binding: next_binding,
    })
}

async fn write_binding_on(
    txn: &DatabaseTransaction,
    capture: &HistoryCapture<'_>,
    directory: desk_diagnose_core::ai_assistant::source::HistorySourceDirectory<'_>,
    existing_binding: Option<binding::Model>,
    recorded: i64,
) -> Result<binding::Model, AdmissionFailure> {
    let row = if let Some(current) = existing_binding {
        current
    } else {
        let mut row = binding::Model {
            id: 0,
            binding_id: capture.device_ref.into(),
            instance_id: directory.instance_id.into(),
            audience: directory.audience.into(),
            device_name: directory.device_name.map(str::to_string),
            first_seen_at_ms: recorded,
            last_seen_at_ms: recorded,
        };
        let mut active: binding::ActiveModel = row.clone().into();
        active.id = sea_orm::ActiveValue::NotSet;
        row.id = binding::Entity::insert(active)
            .exec(txn)
            .await?
            .last_insert_id;
        row
    };
    Ok(row)
}

async fn write_conversation_on(
    txn: &DatabaseTransaction,
    capture: &HistoryCapture<'_>,
    existing_conversation: Option<conversation::Model>,
    recorded: i64,
) -> Result<conversation::Model, AdmissionFailure> {
    let row = if let Some(current) = existing_conversation {
        let mut next = current.clone();
        next.first_recorded_at_ms = current.first_recorded_at_ms.min(recorded);
        next.last_recorded_at_ms = current.last_recorded_at_ms.max(recorded);
        next.revision = increment(current.revision)?;
        require_one(
            conversation::Entity::update_many()
                .col_expr(
                    conversation::Column::FirstRecordedAtMs,
                    Expr::value(next.first_recorded_at_ms),
                )
                .col_expr(
                    conversation::Column::LastRecordedAtMs,
                    Expr::value(next.last_recorded_at_ms),
                )
                .col_expr(conversation::Column::Revision, Expr::value(next.revision))
                .filter(conversation::Column::Id.eq(current.id))
                .filter(conversation::Column::ActorId.eq(&current.actor_id))
                .filter(conversation::Column::ConversationId.eq(&current.conversation_id))
                .filter(conversation::Column::DeviceRef.eq(&current.device_ref))
                .filter(conversation::Column::RootConversationId.eq(&current.root_conversation_id))
                .filter(conversation::Column::Revision.eq(current.revision))
                .filter(conversation::Column::DeletionGeneration.eq(current.deletion_generation))
                .filter(conversation::Column::Status.eq(&current.status))
                .filter(conversation::Column::FirstRecordedAtMs.eq(current.first_recorded_at_ms))
                .filter(conversation::Column::LastRecordedAtMs.eq(current.last_recorded_at_ms))
                .exec(txn)
                .await?
                .rows_affected,
        )?;
        next
    } else {
        let mut row = conversation::Model {
            id: 0,
            conversation_id: capture.conversation_id.into(),
            actor_id: capture.actor_id.into(),
            device_ref: capture.device_ref.into(),
            root_conversation_id: capture.root_conversation_id.into(),
            parent_conversation_id: (capture.root_conversation_id != capture.conversation_id)
                .then(|| capture.root_conversation_id.into()),
            title: None,
            status: "active".into(),
            first_recorded_at_ms: recorded,
            last_recorded_at_ms: recorded,
            deletion_generation: 1,
            revision: 1,
        };
        let mut active: conversation::ActiveModel = row.clone().into();
        active.id = sea_orm::ActiveValue::NotSet;
        row.id = conversation::Entity::insert(active)
            .exec(txn)
            .await?
            .last_insert_id;
        row
    };
    Ok(row)
}

fn require_one(rows: u64) -> Result<(), AdmissionFailure> {
    if rows == 1 {
        Ok(())
    } else {
        Err(AdmissionFailure::Policy)
    }
}
