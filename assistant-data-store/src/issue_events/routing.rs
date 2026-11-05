//! Link a frozen observation to the ordinary queue under the host's owner lock.

use super::*;
use crate::{
    issue_authority,
    main_request::{self, FrozenRequestInput, PENDING_PER_CONVERSATION},
};
use desk_agent_protocol::{
    ai_assistant::AiAssistantAsk, data_lineage::DestinationIdentity, external_tools::IssueSelection,
};
use desk_diagnose_core::{
    dynamic_run::AssistantInputTarget,
    external_tools::IssueAccess,
    input_read_context::ReadContextSelection,
    issue_authority::{IssueAdmission, IssueCommentSnapshot, IssueWorkSource, IssueWorkset},
    issue_events::{CommentDisposition, request_id, work_id},
    native_ids,
    session::PersistedAgentSession,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenIssueHeader {
    pub issue: IssueAccess,
    pub title: String,
    pub body: String,
}

pub struct PendingEvent {
    row: event::Model,
    pub comment: IssueSourceComment,
    pub original: IssueSubscription,
    pub current: IssueSubscription,
    pub header: Option<FrozenIssueHeader>,
}

impl PendingEvent {
    pub fn work_id(&self) -> Option<String> {
        let issue = &self.header.as_ref()?.issue.target;
        Some(work_id(
            &self.row.actor_id,
            &self.row.subscription_id,
            issue.repository_id,
            issue.issue_id,
        ))
    }

    pub fn can_use_devices(&self) -> bool {
        use desk_diagnose_core::issue_authority::IssueWorkKind::DeviceInvestigation;
        self.original.work_kind == DeviceInvestigation
            && self.current.work_kind == DeviceInvestigation
    }

    fn sender(&self) -> Result<u64, AssistantStoreError> {
        self.comment.sender_id.ok_or_else(invalid)
    }

    fn allows_header(&self, header: &FrozenIssueHeader) -> bool {
        self.comment.issue_number == Some(header.issue.target.issue_number)
            && self.original.allows_issue(&header.issue)
            && self.current.allows_issue(&header.issue)
    }
}

pub async fn candidates_on(
    db: &impl ConnectionTrait,
    after: i64,
    limit: u64,
) -> Result<Vec<event::Model>, AssistantStoreError> {
    Ok(event::Entity::find()
        .filter(event::Column::State.is_in(["pending", "attached"]))
        .filter(event::Column::RetiredAtMs.is_null())
        .filter(event::Column::Id.gt(after))
        .order_by_asc(event::Column::Id)
        .limit(limit)
        .all(db)
        .await?)
}

/// Re-read the event and original/current rules inside the owner transaction.
pub async fn pending_on(
    db: &impl ConnectionTrait,
    hint: &event::Model,
) -> Result<Option<PendingEvent>, AssistantStoreError> {
    let Some(row) = event::Entity::find_by_id(hint.id)
        .filter(event::Column::ActorId.eq(&hint.actor_id))
        .filter(event::Column::ConversationId.eq(&hint.conversation_id))
        .filter(event::Column::State.is_in(["pending", "attached"]))
        .filter(event::Column::RetiredAtMs.is_null())
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    let original = original_policy(&row)?;
    let comment = decode_comment(&row)?;
    let Some(current) = issue_authority::current(db, &row.actor_id, &row.subscription_id).await?
    else {
        return Ok(None);
    };
    if current.revision < original.revision
        || current.conversation_id != original.conversation_id
        || comment.disposition(&current, 0, false) != CommentDisposition::Pending
    {
        return Ok(None);
    }
    let header = match (&row.issue_header_json, &row.issue_header_sha256) {
        (None, None) => None,
        (Some(json), Some(hash)) if digest(json) == *hash => {
            Some(serde_json::from_str(json).map_err(|_| invalid())?)
        }
        _ => return Err(invalid()),
    };
    let pending = PendingEvent {
        row,
        comment,
        original,
        current,
        header,
    };
    if pending
        .header
        .as_ref()
        .is_some_and(|header| !pending.allows_header(header))
    {
        return Ok(None);
    }
    Ok(Some(pending))
}

pub async fn freeze_header_on(
    db: &impl ConnectionTrait,
    pending: &PendingEvent,
    header: &FrozenIssueHeader,
    now: i64,
) -> Result<bool, AssistantStoreError> {
    if pending.header.is_some() || !pending.allows_header(header) {
        return Ok(false);
    }
    header.issue.validate().map_err(|_| invalid())?;
    let json = serde_json::to_string(header).map_err(|_| invalid())?;
    if header.title.is_empty() || json.len() > 1024 * 1024 {
        return Err(AssistantStoreError::Capacity);
    }
    Ok(pending_update(pending)
        .filter(event::Column::IssueHeaderJson.is_null())
        .set(event::ActiveModel {
            issue_header_sha256: Set(Some(digest(&json))),
            issue_header_json: Set(Some(json)),
            updated_at_ms: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await?
        .rows_affected
        == 1)
}

pub async fn ignore_on(
    db: &impl ConnectionTrait,
    pending: &PendingEvent,
    reason: &'static str,
    now: i64,
) -> Result<(), AssistantStoreError> {
    pending_update(pending)
        .set(event::ActiveModel {
            state: Set("ignored".into()),
            reason: Set(reason.into()),
            snapshot_json: Set(None),
            admitted_policy_json: Set(None),
            admitted_policy_sha256: Set(None),
            issue_header_json: Set(None),
            issue_header_sha256: Set(None),
            updated_at_ms: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await?;
    Ok(())
}

/// A routed trigger retains its association even when a later request stops.
pub async fn latest_round_on(
    db: &impl ConnectionTrait,
    pending: &PendingEvent,
    now: i64,
) -> Result<Option<IssueAdmission>, AssistantStoreError> {
    let Some(work) = pending.work_id() else {
        return Ok(None);
    };
    let request = event::Entity::find()
        .select_only()
        .column(event::Column::RequestId)
        .filter(event::Column::ActorId.eq(&pending.row.actor_id))
        .filter(event::Column::ConversationId.eq(&pending.row.conversation_id))
        .filter(event::Column::WorkId.eq(work))
        .filter(event::Column::State.eq("routed"))
        .order_by_desc(event::Column::ConsumedAtMs)
        .order_by_desc(event::Column::Id)
        .into_tuple::<Option<String>>()
        .one(db)
        .await?
        .flatten();
    let Some(request) = request else {
        return Ok(None);
    };
    use crate::entity::agent_issue_admission as admission;
    let Some(row) = admission::Entity::find()
        .filter(admission::Column::ActorId.eq(&pending.row.actor_id))
        .filter(admission::Column::ConversationId.eq(&pending.row.conversation_id))
        .filter(admission::Column::SubscriptionId.eq(&pending.row.subscription_id))
        .filter(admission::Column::RequestId.eq(request))
        .filter(admission::Column::StoppedAtMs.is_null())
        .filter(admission::Column::RetiredAtMs.is_null())
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    let snapshot = issue_authority::decode_admission(&row)?;
    Ok((pending
        .header
        .as_ref()
        .is_some_and(|header| header.issue == snapshot.issue)
        && snapshot.usable(&pending.current, now))
    .then_some(snapshot))
}

/// A second comment can supply data to the current request only under both
/// its own frozen observation and the request's original admission.
pub async fn attach_on(
    db: &impl ConnectionTrait,
    pending: &PendingEvent,
    round: &IssueAdmission,
    now: i64,
) -> Result<bool, AssistantStoreError> {
    let Some(header) = &pending.header else {
        return Ok(false);
    };
    if header.issue != round.issue
        || !round.usable(&pending.current, now)
        || !round.subscription.allows_event(
            &round.issue,
            pending.sender()?,
            pending.comment.automation,
            false,
        )
    {
        return Ok(false);
    }
    if pending.row.state == "attached"
        && pending.row.request_id.as_deref() == Some(round.request_id.as_str())
    {
        return Ok(true);
    }
    Ok(pending_update(pending)
        .set(event::ActiveModel {
            state: Set("attached".into()),
            reason: Set("supplemental_material".into()),
            work_id: Set(pending.work_id()),
            request_id: Set(Some(round.request_id.clone())),
            updated_at_ms: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await?
        .rows_affected
        == 1)
}

/// Once its original round closes, a still-unconsumed comment can trigger a
/// new round. Freeze the title and body afresh when routing that new round.
pub async fn release_attachment_on(
    db: &impl ConnectionTrait,
    pending: &PendingEvent,
    now: i64,
) -> Result<bool, AssistantStoreError> {
    if pending.row.state != "attached" {
        return Ok(false);
    }
    Ok(pending_update(pending)
        .set(event::ActiveModel {
            state: Set("pending".into()),
            reason: Set("previous_round_ended".into()),
            request_id: Set(None),
            work_id: Set(None),
            issue_header_json: Set(None),
            issue_header_sha256: Set(None),
            updated_at_ms: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await?
        .rows_affected
        == 1)
}

/// The host checked the live round and root in this transaction. Snapshot,
/// first input, and trigger consumption commit together.
pub async fn enqueue_on(
    db: &impl ConnectionTrait,
    pending: &PendingEvent,
    root: &PersistedAgentSession,
    targets: &[AssistantInputTarget],
    destination: &DestinationIdentity,
    now: i64,
    deadline: i64,
) -> Result<String, AssistantStoreError> {
    let header = pending.header.as_ref().ok_or_else(invalid)?;
    let work = pending.work_id().ok_or_else(invalid)?;
    let request = request_id(&work, &pending.row.event_id);
    let sender = pending.sender()?;
    let client_id = root.client_conversation_id.as_ref().ok_or_else(invalid)?;
    if root.actor_id != pending.row.actor_id
        || root.conversation_id != pending.row.conversation_id
        || !main_request::is_workbench_root(root)
        || (!targets.is_empty() && !pending.can_use_devices())
        || !pending.allows_header(header)
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let mut comments = history_on(db, pending, &work).await?;
    comments.push(IssueCommentSnapshot {
        id: pending.comment.comment_id,
        author_id: sender,
        body: pending.comment.body.clone(),
    });
    let admission = IssueAdmission {
        authorization_id: IssueAdmission::id(&pending.original, &pending.row.event_id),
        subscription: pending.original.clone(),
        event_id: pending.row.event_id.clone(),
        sender_id: sender,
        sender_automation: pending.comment.automation,
        echo: false,
        issue: header.issue.clone(),
        workset: IssueWorkset {
            title: header.title.clone(),
            body: header.body.clone(),
            comments,
        },
        request_id: request.clone(),
        execution_root_id: native_ids::workbench_work_conversation(
            &root.actor_id,
            &root.conversation_id,
            &request,
        ),
        admitted_at_ms: now,
        expires_at_ms: Some(deadline),
    };
    if !admission.usable(&pending.current, now) {
        return Err(AssistantStoreError::Inaccessible);
    }
    let source =
        IssueWorkSource::from_admission(&admission).map_err(AssistantStoreError::Invalid)?;
    let message = source.purpose_message(destination).map_err(|_| invalid())?;
    let mut reads = ReadContextSelection::default();
    reads.external_issue_sources.push(source.reference.clone());
    reads.external_issues.push(header.issue.clone());
    let ask = AiAssistantAsk {
        question: message.text.clone(),
        client_message_id: message.message_id.clone(),
        client_timezone: "UTC".into(),
        conversation_id: Some(client_id.clone()),
        locale: root.response_locale.clone(),
        target_refs: targets
            .iter()
            .map(|target| target.reference.clone())
            .collect(),
        selected_issues: vec![IssueSelection {
            connection_ref: header.issue.connection_ref.clone(),
            owner: header.issue.target.owner.clone(),
            repository: header.issue.target.repository.clone(),
            issue_number: header.issue.target.issue_number,
        }],
        ..Default::default()
    };
    let input = FrozenRequestInput::capture_issue(&ask, targets, &reads, &message, source)?;
    issue_authority::insert_admission_on(db, &admission).await?;
    main_request::enqueue(
        db,
        &root.actor_id,
        &root.conversation_id,
        &input,
        None,
        PENDING_PER_CONVERSATION,
        now,
    )
    .await?;
    if pending_update(pending)
        .set(event::ActiveModel {
            state: Set("routed".into()),
            reason: Set("triggered_request".into()),
            work_id: Set(Some(work)),
            request_id: Set(Some(request.clone())),
            consumed_at_ms: Set(Some(now)),
            updated_at_ms: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await?
        .rows_affected
        != 1
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(request)
}

/// Queue reservations and nonterminal Worker requests keep a round open
/// independently of whether its coordinator currently holds a model lease.
pub async fn has_open_work_on(
    db: &impl ConnectionTrait,
    actor: &str,
    workbench: &str,
    request: &str,
) -> Result<bool, AssistantStoreError> {
    let Some(row) = main_request::load(db, actor, workbench, request).await? else {
        return Ok(false);
    };
    if row.pending_input_json.is_some() && matches!(row.state.as_str(), "queued" | "activated") {
        return Ok(true);
    }
    use crate::entity::agent_subagent_run as child;
    use desk_agent_protocol::ai_assistant::subagent::SubAgentState;
    let conversation = native_ids::workbench_work_conversation(actor, workbench, request);
    Ok(child::Entity::find()
        .select_only()
        .column(child::Column::Id)
        .filter(child::Column::ActorId.eq(actor))
        .filter(child::Column::RootConversationId.eq(conversation))
        .filter(child::Column::State.is_not_in([
            SubAgentState::Completed.as_str(),
            SubAgentState::Failed.as_str(),
            SubAgentState::Cancelled.as_str(),
        ]))
        .into_tuple::<i64>()
        .one(db)
        .await?
        .is_some())
}

/// The first selected Worker remains the work's Worker in later rounds. Its
/// native child request and execution context are created afresh each round.
pub async fn preferred_worker_on(
    db: &impl ConnectionTrait,
    actor: &str,
    source: &IssueWorkSource,
) -> Result<Option<String>, AssistantStoreError> {
    let row = routed_work(actor, source)
        .filter(event::Column::RequestId.eq(&source.request_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if row.first_worker_id.is_some() {
        return Ok(None);
    }
    let id = routed_work(actor, source)
        .select_only()
        .column(event::Column::FirstWorkerId)
        .filter(event::Column::RequestId.ne(&source.request_id))
        .filter(event::Column::FirstWorkerId.is_not_null())
        .order_by_desc(event::Column::ConsumedAtMs)
        .order_by_desc(event::Column::Id)
        .into_tuple::<Option<String>>()
        .one(db)
        .await?
        .flatten();
    if id
        .as_ref()
        .is_some_and(|id| !native_ids::is(native_ids::Kind::Worker, id))
    {
        return Err(invalid());
    }
    Ok(id)
}

pub async fn record_first_worker_on(
    db: &impl ConnectionTrait,
    actor: &str,
    source: &IssueWorkSource,
    worker: &str,
) -> Result<(), AssistantStoreError> {
    if !native_ids::is(native_ids::Kind::Worker, worker) {
        return Err(invalid());
    }
    event::Entity::update_many()
        .filter(event::Column::ActorId.eq(actor))
        .filter(event::Column::ConversationId.eq(&source.workbench_id))
        .filter(event::Column::SubscriptionId.eq(&source.subscription_id))
        .filter(event::Column::WorkId.eq(&source.work_id))
        .filter(event::Column::RequestId.eq(&source.request_id))
        .filter(event::Column::State.eq("routed"))
        .filter(event::Column::RetiredAtMs.is_null())
        .filter(event::Column::FirstWorkerId.is_null())
        .set(event::ActiveModel {
            first_worker_id: Set(Some(worker.into())),
            ..Default::default()
        })
        .exec(db)
        .await?;
    Ok(())
}

fn routed_work(actor: &str, source: &IssueWorkSource) -> sea_orm::Select<event::Entity> {
    event::Entity::find()
        .filter(event::Column::ActorId.eq(actor))
        .filter(event::Column::ConversationId.eq(&source.workbench_id))
        .filter(event::Column::SubscriptionId.eq(&source.subscription_id))
        .filter(event::Column::WorkId.eq(&source.work_id))
        .filter(event::Column::State.eq("routed"))
        .filter(event::Column::RetiredAtMs.is_null())
}

async fn history_on(
    db: &impl ConnectionTrait,
    pending: &PendingEvent,
    work: &str,
) -> Result<Vec<IssueCommentSnapshot>, AssistantStoreError> {
    let mut comments = Vec::new();
    let mut bytes = pending.comment.body.len();
    // Leave room for the title, body, source metadata and JSON escaping.
    // Recent accepted comments are useful; old history is not a prerequisite.
    let rows = event::Entity::find()
        .filter(event::Column::ActorId.eq(&pending.row.actor_id))
        .filter(event::Column::ConversationId.eq(&pending.row.conversation_id))
        .filter(event::Column::WorkId.eq(work))
        .filter(event::Column::State.is_in(["routed", "consumed"]))
        .filter(event::Column::RetiredAtMs.is_null())
        .filter(event::Column::Id.ne(pending.row.id))
        .order_by_desc(event::Column::SourceCreatedAtMs)
        .order_by_desc(event::Column::Id)
        .limit(32)
        .all(db)
        .await?;
    for row in rows {
        let comment = decode_comment(&row)?;
        if comment.disposition(&pending.original, 0, false) != CommentDisposition::Pending
            || comment.disposition(&pending.current, 0, false) != CommentDisposition::Pending
            || comment.issue_number != pending.comment.issue_number
        {
            continue;
        }
        bytes = bytes.saturating_add(comment.body.len());
        if bytes > 64 * 1024 {
            break;
        }
        comments.push(IssueCommentSnapshot {
            id: comment.comment_id,
            author_id: comment.sender_id.ok_or_else(invalid)?,
            body: comment.body,
        });
    }
    comments.reverse();
    Ok(comments)
}

fn pending_update(pending: &PendingEvent) -> sea_orm::UpdateMany<event::Entity> {
    event::Entity::update_many()
        .filter(event::Column::Id.eq(pending.row.id))
        .filter(event::Column::State.eq(&pending.row.state))
        .filter(event::Column::RequestId.eq(pending.row.request_id.clone()))
        .filter(event::Column::RetiredAtMs.is_null())
        .filter(event::Column::SnapshotSha256.eq(&pending.row.snapshot_sha256))
}
