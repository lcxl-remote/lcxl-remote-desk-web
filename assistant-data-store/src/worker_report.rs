//! Immutable worker reports share the native delegation inbox transaction.

use crate::entity::agent_subagent_inbox as inbox;
use desk_agent_protocol::ai_assistant::subagent::{
    AiAssistantSubAgentQuestion, AiAssistantSubAgentReport, AiAssistantSubAgentReports,
    AiAssistantSubAgentResult, SubAgentReportKind, SubAgentState,
};
use desk_agent_protocol::data_lineage::{DataEnvelope, DestinationIdentity, Sensitivity};
use desk_diagnose_core::subagent::{
    group::DelegationGroup,
    notification::NotificationEvent,
    report::{
        MAX_REPORT_BATCH_BYTES, MAX_REPORT_BATCH_ITEMS, ReportKind, ReportSubmission, StoredReport,
        WorkerReport, WorkerReportPage, report_page,
    },
    state::SubAgentRun,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DbErr, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, Set,
};

fn invalid() -> DbErr {
    DbErr::Custom("invalid delegated worker report".into())
}

fn report_rows(run: &SubAgentRun) -> sea_orm::Select<inbox::Entity> {
    inbox::Entity::find()
        .filter(inbox::Column::TaskId.eq(&run.binding.task_id))
        .filter(inbox::Column::GroupId.eq(&run.binding.group_id))
        .filter(inbox::Column::RootConversationId.eq(&run.binding.root_conversation_id))
        .filter(inbox::Column::ActorId.eq(&run.actor_id))
        .filter(inbox::Column::DeviceId.eq(&run.device_id))
        .filter(inbox::Column::ReportSeq.is_not_null())
}

fn decode_report(row: &inbox::Model, run: &SubAgentRun) -> Result<StoredReport, DbErr> {
    let stored = decode_record(row)?;
    if stored.report.worker_id != run.worker_id || stored.report.request_id != run.binding.task_id {
        return Err(invalid());
    }
    Ok(stored)
}

fn decode_record(row: &inbox::Model) -> Result<StoredReport, DbErr> {
    let stored: StoredReport = serde_json::from_str(&row.event_json).map_err(|_| invalid())?;
    let report = &stored.report;
    if row.event_kind != report.content.kind.inbox_kind()
        || report.report_id != row.event_id
        || report.request_id != row.task_id
        || i64::try_from(report.seq).ok() != row.report_seq
    {
        return Err(invalid());
    }
    stored.validate().map_err(|_| invalid())?;
    Ok(stored)
}

fn group_reports(group: &DelegationGroup) -> Result<sea_orm::Select<inbox::Entity>, DbErr> {
    Ok(inbox::Entity::find()
        .filter(inbox::Column::GroupId.eq(&group.group_id))
        .filter(inbox::Column::RootConversationId.eq(&group.root_conversation_id))
        .filter(inbox::Column::ActorId.eq(&group.actor_id))
        .filter(
            inbox::Column::ParentInputRevision
                .eq(i64::try_from(group.parent_input_revision).map_err(|_| invalid())?),
        )
        .filter(
            inbox::Column::ParentControlRevision
                .eq(i64::try_from(group.parent_control_revision).map_err(|_| invalid())?),
        )
        .filter(inbox::Column::ReportSeq.is_not_null()))
}

fn allows_projection(envelope: &DataEnvelope, destination: &DestinationIdentity) -> bool {
    envelope.allowed_destinations.contains(destination)
        && !(envelope.sensitivity == Sensitivity::Secret && destination.is_external())
}

fn projectable_reports(
    group: &DelegationGroup,
    destination: &DestinationIdentity,
    now_ms: i64,
) -> Result<sea_orm::Select<inbox::Entity>, DbErr> {
    projectable_at(
        group_reports(group)?.filter(inbox::Column::InterpretedAtMs.is_null()),
        destination,
        now_ms,
    )
}

fn projectable_at(
    query: sea_orm::Select<inbox::Entity>,
    destination: &DestinationIdentity,
    now_ms: i64,
) -> Result<sea_orm::Select<inbox::Entity>, DbErr> {
    let destination_json = serde_json::to_string(destination).map_err(|_| invalid())?;
    Ok(retained_at(
        query.filter(inbox::Column::ReportModelDestination.eq(destination_json)),
        now_ms,
    ))
}

fn retained_at(
    query: sea_orm::Select<inbox::Entity>,
    now_ms: i64,
) -> sea_orm::Select<inbox::Entity> {
    query.filter(
        Condition::any()
            .add(inbox::Column::ReportExpiresAtMs.is_null())
            .add(inbox::Column::ReportExpiresAtMs.gt(now_ms)),
    )
}

/// The host has checked the owner and native parent. This view neither
/// authorizes model egress nor changes report delivery state.
pub async fn owner_result_on(
    db: &impl ConnectionTrait,
    run: &SubAgentRun,
    before_seq: Option<u64>,
    now_ms: i64,
) -> Result<AiAssistantSubAgentResult, DbErr> {
    const PAGE_SIZE: usize = 10;
    const BYTE_LIMIT: usize = 128 * 1024;
    let mut query = retained_at(report_rows(run), now_ms);
    if let Some(before) = before_seq {
        let before = i64::try_from(before)
            .ok()
            .filter(|seq| *seq > 0)
            .ok_or_else(invalid)?;
        query = query.filter(inbox::Column::ReportSeq.lt(before));
    }
    let rows = query
        .order_by_desc(inbox::Column::ReportSeq)
        .limit(PAGE_SIZE as u64 + 1)
        .all(db)
        .await?;
    let mut items = Vec::new();
    let mut remaining = BYTE_LIMIT;
    for row in rows.iter().take(PAGE_SIZE) {
        let stored = decode_report(row, run)?;
        let report = stored.report;
        let item = AiAssistantSubAgentReport {
            report_id: report.report_id,
            seq: report.seq,
            kind: match report.content.kind {
                ReportKind::Progress => SubAgentReportKind::Progress,
                ReportKind::NeedsInput => SubAgentReportKind::NeedsInput,
                ReportKind::Completed => SubAgentReportKind::Completed,
                ReportKind::Failed => SubAgentReportKind::Failed,
            },
            summary: report.content.summary,
            question: report.content.question,
            evidence_refs: report.content.evidence_refs,
            receipt_refs: report.receipt_refs,
        };
        let size = serde_json::to_vec(&item).map_err(|_| invalid())?.len() + 1;
        let Some(left) = remaining.checked_sub(size) else {
            break;
        };
        remaining = left;
        items.push(item);
    }
    let next_before_seq = if rows.len() > items.len() {
        Some(items.last().ok_or_else(invalid)?.seq)
    } else {
        None
    };
    items.reverse();
    let mut result = run.result();
    result.reports = AiAssistantSubAgentReports {
        items,
        next_before_seq,
    };
    if run.state == SubAgentState::WaitingInput
        && now_ms < run.binding.deadline_ms
        && let Some(question_id) = run.pending_question_id.as_deref()
        && let Some(question) = open_question_on(db, run, question_id, now_ms, true).await?
    {
        result.open_question = Some(AiAssistantSubAgentQuestion {
            question_id: question_id.into(),
            text: question.report.content.question.ok_or_else(invalid)?,
        });
    }
    Ok(result)
}

/// The batch has revalidated this anchor against its original native work.
/// Keep the page within the same request, source fence and model destination.
pub struct CoordinationHistoryPage {
    pub after_seq: u64,
    pub limit: u32,
    pub state_revision: u64,
    pub byte_limit: usize,
}

pub async fn coordination_history_on(
    db: &impl ConnectionTrait,
    anchor: &inbox::Model,
    destination: &DestinationIdentity,
    now_ms: i64,
    page: CoordinationHistoryPage,
    labels: &mut Vec<DataEnvelope>,
) -> Result<Option<serde_json::Value>, DbErr> {
    let CoordinationHistoryPage {
        after_seq,
        limit,
        state_revision,
        byte_limit,
    } = page;
    let query = inbox::Entity::find()
        .filter(inbox::Column::WorkbenchId.eq(anchor.workbench_id.clone()))
        .filter(inbox::Column::ActorId.eq(&anchor.actor_id))
        .filter(inbox::Column::DeviceId.eq(&anchor.device_id))
        .filter(inbox::Column::RootConversationId.eq(&anchor.root_conversation_id))
        .filter(inbox::Column::GroupId.eq(&anchor.group_id))
        .filter(inbox::Column::TaskId.eq(&anchor.task_id))
        .filter(inbox::Column::ParentInputRevision.eq(anchor.parent_input_revision))
        .filter(inbox::Column::ParentControlRevision.eq(anchor.parent_control_revision))
        .filter(
            inbox::Column::StateRevision.lte(i64::try_from(state_revision).map_err(|_| invalid())?),
        )
        .filter(inbox::Column::ReportSeq.gt(i64::try_from(after_seq).map_err(|_| invalid())?))
        .order_by_asc(inbox::Column::ReportSeq)
        .limit(u64::from(limit) + 1);
    let worker = decode_projectable(anchor, destination)?.report.worker_id;
    let mut stored = Vec::new();
    for row in projectable_at(query, destination, now_ms)?.all(db).await? {
        let item = decode_projectable(&row, destination)?;
        if item.report.worker_id != worker {
            return Err(invalid());
        }
        stored.push(item);
    }
    // The report-page builder can reject a first record that cannot fit.
    let page = report_page(
        &anchor.task_id,
        Vec::new(),
        after_seq,
        limit,
        byte_limit as u64,
        stored,
        labels,
    );
    page.ok()
        .map(|page| serde_json::to_value(page).map_err(|_| invalid()))
        .transpose()
}

pub fn decode_projectable(
    row: &inbox::Model,
    destination: &DestinationIdentity,
) -> Result<StoredReport, DbErr> {
    let stored = decode_record(row)?;
    if !allows_projection(&stored.envelope, destination)
        || stored.envelope.retention.expires_at_unix_ms
            != row.report_expires_at_ms.map(|at| at as u64)
    {
        return Err(invalid());
    }
    Ok(stored)
}

pub async fn retained_terminal_report_on(
    db: &impl ConnectionTrait,
    previous: &SubAgentRun,
    destination: &DestinationIdentity,
    now_ms: i64,
) -> Result<Option<StoredReport>, DbErr> {
    let kind = match previous.state {
        desk_diagnose_core::subagent::SubAgentState::Completed => ReportKind::Completed,
        desk_diagnose_core::subagent::SubAgentState::Failed => ReportKind::Failed,
        _ => return Ok(None),
    };
    let query = report_rows(previous)
        .filter(inbox::Column::EventKind.eq(kind.inbox_kind()))
        .filter(
            inbox::Column::StateRevision
                .eq(i64::try_from(previous.state_revision).map_err(|_| invalid())?),
        )
        .order_by_desc(inbox::Column::ReportSeq);
    let Some(row) = projectable_at(query, destination, now_ms)?.one(db).await? else {
        return Ok(None);
    };
    let stored = decode_projectable(&row, destination)?;
    if stored.report.worker_id != previous.worker_id
        || row.workbench_id.as_deref() != previous.binding.source.workbench_id()
    {
        return Err(invalid());
    }
    Ok(Some(stored))
}

pub async fn batch_candidates_on(
    db: &impl ConnectionTrait,
    workbench: &str,
    actor: &str,
    destination: &DestinationIdentity,
    now_ms: i64,
) -> Result<Vec<inbox::Model>, DbErr> {
    batch_eligible(workbench, actor, destination, now_ms)?
        .order_by_asc(inbox::Column::Id)
        .limit((MAX_REPORT_BATCH_ITEMS * 2) as u64)
        .all(db)
        .await
}

pub(crate) fn batch_eligible(
    workbench: &str,
    actor: &str,
    destination: &DestinationIdentity,
    now_ms: i64,
) -> Result<sea_orm::Select<inbox::Entity>, DbErr> {
    let destination = serde_json::to_string(destination).map_err(|_| invalid())?;
    Ok(inbox::Entity::find()
        .filter(inbox::Column::WorkbenchId.eq(workbench))
        .filter(inbox::Column::ActorId.eq(actor))
        .filter(inbox::Column::DeviceId.eq(""))
        .filter(inbox::Column::ReportSeq.is_not_null())
        .filter(inbox::Column::ReportModelDestination.eq(destination))
        .filter(inbox::Column::CoordinatedAtMs.is_null())
        .filter(
            inbox::Column::CoordinationAttempts.lt(crate::main_request::coordination::MAX_ATTEMPTS),
        )
        .filter(inbox::Column::InterpretedAtMs.is_null())
        .filter(inbox::Column::ModelNotifiedAtMs.is_null())
        .filter(inbox::Column::NotificationAttemptedTurnId.is_null())
        .filter(inbox::Column::WakeRetiredAtMs.is_null())
        .filter(
            Condition::any()
                .add(inbox::Column::CoordinationRetryAtMs.is_null())
                .add(inbox::Column::CoordinationRetryAtMs.lte(now_ms)),
        )
        .filter(
            Condition::any()
                .add(inbox::Column::ReportExpiresAtMs.is_null())
                .add(inbox::Column::ReportExpiresAtMs.gt(now_ms)),
        ))
}

pub async fn pending_wake_groups_on(
    db: &impl ConnectionTrait,
    groups: impl IntoIterator<Item = String>,
    now_ms: i64,
) -> Result<Vec<String>, DbErr> {
    inbox::Entity::find()
        .select_only()
        .column(inbox::Column::GroupId)
        .distinct()
        .filter(inbox::Column::GroupId.is_in(groups))
        .filter(inbox::Column::ReportModelDestination.is_not_null())
        .filter(inbox::Column::InterpretedAtMs.is_null())
        .filter(inbox::Column::ModelNotifiedAtMs.is_null())
        .filter(inbox::Column::NotificationAttemptedTurnId.is_null())
        .filter(inbox::Column::WakeRetiredAtMs.is_null())
        .filter(inbox::Column::EventKind.is_in([
            ReportKind::NeedsInput.inbox_kind(),
            ReportKind::Completed.inbox_kind(),
            ReportKind::Failed.inbox_kind(),
        ]))
        .filter(
            Condition::any()
                .add(inbox::Column::ReportExpiresAtMs.is_null())
                .add(inbox::Column::ReportExpiresAtMs.gt(now_ms)),
        )
        .into_tuple()
        .all(db)
        .await
}

/// Select new wake candidates or reload the exact records of a prepared wake.
/// Delivery and interpretation are tracked separately on the same reports.
pub async fn wake_reports_on<C: ConnectionTrait>(
    txn: &C,
    group: &DelegationGroup,
    destination: &DestinationIdentity,
    now_ms: i64,
    expected: Option<&[NotificationEvent]>,
) -> Result<Vec<(NotificationEvent, StoredReport)>, DbErr> {
    if now_ms >= group.limits.deadline_ms {
        return Ok(Vec::new());
    }
    let query = projectable_reports(group, destination, now_ms)?
        .filter(inbox::Column::ModelNotifiedAtMs.is_null())
        .filter(inbox::Column::EventKind.is_in([
            ReportKind::NeedsInput.inbox_kind(),
            ReportKind::Completed.inbox_kind(),
            ReportKind::Failed.inbox_kind(),
        ]));
    let query = if let Some(expected) = expected {
        query.filter(inbox::Column::EventId.is_in(expected.iter().map(|event| &event.event_id)))
    } else {
        query
            .filter(inbox::Column::NotificationAttemptedTurnId.is_null())
            .filter(inbox::Column::WakeRetiredAtMs.is_null())
            .limit((MAX_REPORT_BATCH_ITEMS * 2) as u64)
    };
    query
        .order_by_asc(inbox::Column::Id)
        .all(txn)
        .await?
        .iter()
        .map(|row| {
            let stored = decode_projectable(row, destination)?;
            let event = NotificationEvent {
                event_id: row.event_id.clone(),
                task_id: row.task_id.clone(),
                state_revision: u64::try_from(row.state_revision).map_err(|_| invalid())?,
            };
            Ok((event, stored))
        })
        .collect()
}

pub async fn retire_wake_on(
    txn: &impl ConnectionTrait,
    event_id: &str,
    now_ms: i64,
) -> Result<(), DbErr> {
    inbox::Entity::update_many()
        .set(inbox::ActiveModel {
            wake_retired_at_ms: Set(Some(now_ms)),
            ..Default::default()
        })
        .filter(inbox::Column::EventId.eq(event_id))
        .filter(inbox::Column::WakeRetiredAtMs.is_null())
        .exec(txn)
        .await?;
    Ok(())
}

pub async fn project_pending_on<C: ConnectionTrait>(
    txn: &C,
    group: &DelegationGroup,
    destination: &DestinationIdentity,
    now_ms: i64,
    available_bytes: usize,
    labels: &mut Vec<DataEnvelope>,
) -> Result<Vec<serde_json::Value>, DbErr> {
    if now_ms >= group.limits.deadline_ms || available_bytes == 0 {
        return Ok(Vec::new());
    }
    let mut remaining = available_bytes.min(MAX_REPORT_BATCH_BYTES);
    let mut items = Vec::new();
    for row in projectable_reports(group, destination, now_ms)?
        .order_by_asc(inbox::Column::Id)
        .limit(MAX_REPORT_BATCH_ITEMS as u64)
        .all(txn)
        .await?
    {
        let stored = decode_projectable(&row, destination)?;
        let item = stored.report.coordination_item();
        let Some(bytes_left) = remaining.checked_sub(item.to_string().len() + 1) else {
            break;
        };
        remaining = bytes_left;
        labels.push(stored.envelope);
        items.push(item);
    }
    Ok(items)
}

/// The hosts authenticate the exact model projection and hold the root lock.
/// The decision and the native tool receipt commit in the same transaction.
pub async fn settle_projected_on<C: ConnectionTrait>(
    txn: &C,
    group: &DelegationGroup,
    report_ids: Vec<String>,
    turn_id: &str,
    now_ms: i64,
) -> Result<(), DbErr> {
    if now_ms >= group.limits.deadline_ms {
        return Err(invalid());
    }
    let rows = group_reports(group)?
        .filter(inbox::Column::EventId.is_in(report_ids.iter()))
        .all(txn)
        .await?;
    if rows.len() != report_ids.len() {
        return Err(invalid());
    }
    for row in rows {
        let stored = decode_record(&row)?;
        if group.source.workbench_id().is_some()
            && row.owner_handoff_at_ms.is_none()
            && let Some(question) = stored.report.question_id.as_deref()
        {
            use crate::entity::agent_subagent_run as run;
            let task = run::Entity::find()
                .filter(run::Column::TaskId.eq(&row.task_id))
                .filter(run::Column::GroupId.eq(&group.group_id))
                .filter(run::Column::ActorId.eq(&group.actor_id))
                .one(txn)
                .await?;
            if let Some(task) = task
                && crate::worker_request::decode_run(&task)?
                    .pending_question_id
                    .as_deref()
                    == Some(question)
            {
                return Err(invalid());
            }
        }
        if row.interpreted_at_ms.is_none() {
            let mut update: inbox::ActiveModel = row.into();
            update.interpreted_at_ms = Set(Some(now_ms));
            update.interpreted_turn_id = Set(Some(turn_id.to_owned()));
            update.update(txn).await?;
        }
    }
    Ok(())
}

/// Callers hold the root and child control locks or the SQLite writer lock.
/// Report sequence is independent of the request's native state revisions.
pub async fn append_report_on<C: ConnectionTrait>(
    txn: &C,
    group: &DelegationGroup,
    run: &SubAgentRun,
    submission: ReportSubmission<'_>,
    sources: &[DataEnvelope],
    now_ms: i64,
) -> Result<WorkerReport, DbErr> {
    let ReportSubmission {
        generation,
        origin,
        model_destination,
        content,
        receipt_refs,
    } = submission;
    // The unique event id is the report identity, so a retried submission
    // finds its original report on every instance.
    let report_id = desk_diagnose_core::native_ids::stable(
        desk_diagnose_core::native_ids::Kind::WorkerReport,
        &(&run.binding.task_id, generation, origin),
    );
    let query = report_rows(run);
    if let Some(row) = query
        .clone()
        .filter(inbox::Column::EventId.eq(&report_id))
        .one(txn)
        .await?
    {
        let original = decode_report(&row, run)?.report;
        if original.content != content || original.receipt_refs != receipt_refs {
            return Err(invalid());
        }
        return Ok(original);
    }
    let previous = query
        .select_only()
        .column(inbox::Column::ReportSeq)
        .order_by_desc(inbox::Column::ReportSeq)
        .into_tuple::<i64>()
        .one(txn)
        .await?
        .unwrap_or(0);
    let seq = previous.checked_add(1).ok_or_else(invalid)?;
    let question_id = (content.kind == ReportKind::NeedsInput).then(|| report_id.clone());
    let report = WorkerReport {
        report_id,
        worker_id: run.worker_id.clone(),
        request_id: run.binding.task_id.clone(),
        execution_generation: generation,
        seq: u64::try_from(seq).map_err(|_| invalid())?,
        question_id,
        content,
        receipt_refs,
    };
    let stored = StoredReport::new(report, sources).map_err(|_| invalid())?;
    let report_model_destination = allows_projection(&stored.envelope, model_destination)
        .then(|| serde_json::to_string(model_destination))
        .transpose()
        .map_err(|_| invalid())?;
    let report_expires_at_ms = stored
        .envelope
        .retention
        .expires_at_unix_ms
        .map(i64::try_from)
        .transpose()
        .map_err(|_| invalid())?;
    inbox::ActiveModel {
        event_id: Set(stored.report.report_id.clone()),
        root_conversation_id: Set(run.binding.root_conversation_id.clone()),
        group_id: Set(run.binding.group_id.clone()),
        task_id: Set(run.binding.task_id.clone()),
        actor_id: Set(run.actor_id.clone()),
        device_id: Set(run.device_id.clone()),
        state_revision: Set(i64::try_from(run.state_revision).map_err(|_| invalid())?),
        report_seq: Set(Some(seq)),
        report_model_destination: Set(report_model_destination),
        report_expires_at_ms: Set(report_expires_at_ms),
        workbench_id: Set(group.source.workbench_id().map(str::to_owned)),
        coordination_attempts: Set(group.source.workbench_id().map(|_| 0)),
        event_kind: Set(stored.report.content.kind.inbox_kind().into()),
        event_json: Set(serde_json::to_string(&stored).map_err(|_| invalid())?),
        parent_input_revision: Set(
            i64::try_from(group.parent_input_revision).map_err(|_| invalid())?
        ),
        parent_control_revision: Set(
            i64::try_from(group.parent_control_revision).map_err(|_| invalid())?
        ),
        created_at: Set(now_ms),
        ..Default::default()
    }
    .insert(txn)
    .await?;
    Ok(stored.report)
}

pub async fn read_open_question_on<C: ConnectionTrait>(
    txn: &C,
    run: &SubAgentRun,
    question_id: &str,
    now_ms: i64,
) -> Result<StoredReport, DbErr> {
    open_question_on(txn, run, question_id, now_ms, false)
        .await?
        .ok_or_else(invalid)
}

async fn open_question_on(
    txn: &impl ConnectionTrait,
    run: &SubAgentRun,
    question_id: &str,
    now_ms: i64,
    owner_view: bool,
) -> Result<Option<StoredReport>, DbErr> {
    if run.pending_question_id.as_deref() != Some(question_id) {
        return Err(invalid());
    }
    let mut query =
        retained_at(report_rows(run), now_ms).filter(inbox::Column::EventId.eq(question_id));
    if owner_view && run.binding.source.workbench_id().is_some() {
        query = query.filter(inbox::Column::OwnerHandoffAtMs.is_not_null());
    }
    let Some(row) = query.one(txn).await? else {
        return Ok(None);
    };
    let stored = decode_report(&row, run)?;
    if stored.report.question_id.as_deref() != Some(question_id) {
        return Err(invalid());
    }
    Ok(Some(stored))
}

/// The model has a native observation of this exact question and the caller
/// holds the parent/child control transaction. Keep the report open for reply.
pub async fn handoff_question_on(
    txn: &impl ConnectionTrait,
    run: &SubAgentRun,
    question_id: &str,
    observed_seq: u64,
    turn_id: &str,
    now_ms: i64,
) -> Result<(), DbErr> {
    if run.binding.source.workbench_id().is_none()
        || run.state != SubAgentState::WaitingInput
        || now_ms >= run.binding.deadline_ms
        || read_open_question_on(txn, run, question_id, now_ms)
            .await?
            .report
            .seq
            != observed_seq
    {
        return Err(invalid());
    }
    inbox::Entity::update_many()
        .filter(inbox::Column::EventId.eq(question_id))
        .filter(inbox::Column::OwnerHandoffAtMs.is_null())
        .set(inbox::ActiveModel {
            owner_handoff_at_ms: Set(Some(now_ms)),
            interpreted_at_ms: Set(Some(now_ms)),
            interpreted_turn_id: Set(Some(turn_id.into())),
            ..Default::default()
        })
        .exec(txn)
        .await?;
    Ok(())
}

pub fn owner_handoff_is_open(
    row: &inbox::Model,
    run: &SubAgentRun,
    now_ms: i64,
) -> Result<bool, DbErr> {
    if row.owner_handoff_at_ms.is_none()
        || row.report_expires_at_ms.is_some_and(|at| at <= now_ms)
        || now_ms >= run.binding.deadline_ms
        || run.state != SubAgentState::WaitingInput
        || run.pending_question_id.as_deref() != Some(row.event_id.as_str())
    {
        return Ok(false);
    }
    Ok(decode_report(row, run)?.report.question_id.as_deref() == Some(row.event_id.as_str()))
}

pub async fn first_matching_report_on<C: ConnectionTrait>(
    txn: &C,
    run: &SubAgentRun,
    after_seq: u64,
    kinds: &[ReportKind],
    destination: &DestinationIdentity,
    now_ms: i64,
) -> Result<Option<StoredReport>, DbErr> {
    let row = projectable_at(report_rows(run), destination, now_ms)?
        .filter(inbox::Column::ReportSeq.gt(i64::try_from(after_seq).map_err(|_| invalid())?))
        .filter(inbox::Column::EventKind.is_in(kinds.iter().map(|kind| kind.inbox_kind())))
        .order_by_asc(inbox::Column::ReportSeq)
        .one(txn)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let report = decode_projectable(&row, destination)?;
    if report.report.worker_id != run.worker_id {
        return Err(invalid());
    }
    Ok(Some(report))
}

pub struct ReportPageRead<'a> {
    pub after_seq: u64,
    pub limit: u32,
    pub byte_limit: u64,
    pub destination: &'a DestinationIdentity,
    pub now_ms: i64,
}

pub async fn read_report_page_on<C: ConnectionTrait>(
    txn: &C,
    run: &SubAgentRun,
    query: ReportPageRead<'_>,
    labels: &mut Vec<DataEnvelope>,
) -> Result<(WorkerReportPage, Option<String>), DbErr> {
    let rows = projectable_at(report_rows(run), query.destination, query.now_ms)?
        .filter(inbox::Column::ReportSeq.gt(i64::try_from(query.after_seq).map_err(|_| invalid())?))
        .order_by_asc(inbox::Column::ReportSeq)
        .limit(u64::from(query.limit) + 1)
        .all(txn)
        .await?;
    let stored = rows
        .iter()
        .map(|row| {
            let stored = decode_projectable(row, query.destination)?;
            if stored.report.worker_id != run.worker_id {
                return Err(invalid());
            }
            Ok(stored)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let criteria = if stored.is_empty() {
        Vec::new()
    } else {
        run.binding.acceptance_criteria.clone()
    };
    let page = report_page(
        &run.binding.task_id,
        criteria,
        query.after_seq,
        query.limit,
        query.byte_limit,
        stored,
        labels,
    )
    .map_err(|_| invalid())?;
    let terminal = page
        .reports
        .iter()
        .zip(rows)
        .find(|(report, row)| {
            report.content.kind.matches_terminal_state(run.state)
                && u64::try_from(row.state_revision).ok() == Some(run.state_revision)
        })
        .map(|(report, _)| report.report_id.clone());
    Ok((page, terminal))
}

/// Bind a model-visible terminal report back to the original native state event.
pub async fn validate_terminal_observation_on(
    txn: &impl ConnectionTrait,
    event: &inbox::Model,
    report: &WorkerReport,
) -> Result<(), DbErr> {
    let original: desk_agent_protocol::ai_assistant::subagent::AiAssistantSubAgentEvent =
        serde_json::from_str(&event.event_json).map_err(|_| invalid())?;
    if original.task.task_id != report.request_id
        || original.task.worker_id != report.worker_id
        || u64::try_from(event.state_revision).ok() != Some(original.task.state_revision)
        || !report
            .content
            .kind
            .matches_terminal_state(original.task.state)
    {
        return Err(invalid());
    }
    let row = inbox::Entity::find()
        .filter(inbox::Column::EventId.eq(&report.report_id))
        .filter(inbox::Column::TaskId.eq(&event.task_id))
        .filter(inbox::Column::GroupId.eq(&event.group_id))
        .filter(inbox::Column::RootConversationId.eq(&event.root_conversation_id))
        .filter(inbox::Column::ActorId.eq(&event.actor_id))
        .filter(inbox::Column::DeviceId.eq(&event.device_id))
        .filter(inbox::Column::StateRevision.eq(event.state_revision))
        .filter(inbox::Column::ReportSeq.is_not_null())
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    if decode_record(&row)?.report != *report {
        return Err(invalid());
    }
    Ok(())
}
