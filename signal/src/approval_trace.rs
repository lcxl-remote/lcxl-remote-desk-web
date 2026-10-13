//! Private approval details. Read authorization never follows device sharing.
use crate::config::connection::DatabaseConnection;
use crate::entity::{
    agent_approval_review as review, agent_approval_trace as trace,
    agent_approval_trace_part as part, agent_session, agent_subagent_run,
};
use async_trait::async_trait;
use desk_diagnose_core::approval_review::{ApprovalReviewCandidate, ApprovalSource};
use desk_diagnose_core::session::PersistedAgentSession;
use desk_signal_facade::model::approval_trace::*;
use desk_signal_facade::service::approval_trace::{
    ApprovalTraceSink, TraceAppendError, TraceBackend, TraceCompletion, TraceIdentity,
};
use sea_orm::{
    ActiveValue::Set,
    ColumnTrait, Condition, ConnectionTrait, DbErr, EntityTrait, ExprTrait, QueryFilter,
    QueryOrder, QuerySelect, QueryTrait, TransactionTrait,
    sea_query::{Expr, OnConflict},
};
use std::sync::Arc;

use crate::entity::{agent_capability_grant as grant, agent_run_event};
use desk_diagnose_core::approval_observation::ReviewTracking;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PageCursor {
    scope: String,
    at: i64,
    id: String,
}

const QUOTA: &str = "__approval_trace_quota";
fn invalid() -> DbErr {
    DbErr::Custom("approval detail unavailable".into())
}
#[derive(Clone)]
pub struct Store {
    pub db: DatabaseConnection,
}

impl Store {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
    pub async fn capture(
        &self,
        candidate: &ApprovalReviewCandidate,
        claim: &crate::agent_approval_store::ClaimedPermissionReview,
    ) -> ApprovalTraceSink {
        let settings = tokio::time::timeout(
            std::time::Duration::from_millis(150),
            load_settings(&self.db),
        )
        .await
        .ok()
        .and_then(Result::ok);
        let unavailable = settings.is_none();
        let settings = settings.unwrap_or_else(|| ApprovalTraceSettings::defaults(false));
        let (source_kind, source_id) = match &candidate.source {
            ApprovalSource::PermissionItem {
                request_id,
                item_id,
            } => ("permission_item", format!("{request_id}/{item_id}")),
            ApprovalSource::OssCommand { request_id } => ("oss_command", request_id.clone()),
            ApprovalSource::ManagerAction { work_id } => ("manager_action", work_id.clone()),
            ApprovalSource::ConcreteCall { call_id, .. } => ("concrete_call", call_id.clone()),
            ApprovalSource::ManagerCommand { work_id, .. } => {
                ("manager_command", work_id.to_string())
            }
        };
        let mut sink = ApprovalTraceSink::start(
            Arc::new(self.clone()),
            TraceIdentity {
                id: candidate.candidate_id.clone(),
                owner_id: candidate.owner_id.clone(),
                conversation_id: candidate.conversation_id.clone(),
                root_conversation_id: claim.call_authority.root_conversation_id.clone(),
                device_id: candidate.device_id.clone(),
                source_kind: source_kind.into(),
                source_id,
                lease_epoch: claim.lease_epoch as i64,
                model_revision: claim.model_config_revision as i64,
                created_at_ms: chrono::Utc::now().timestamp_millis(),
            },
            settings,
        );
        if unavailable {
            sink.storage_unavailable();
        }
        sink
    }
    async fn subject(
        &self,
        conversation: &str,
        owner: &str,
    ) -> Result<agent_session::Model, DbErr> {
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(conversation))
            .filter(agent_session::Column::ActorId.eq(owner))
            .one(&self.db)
            .await?
            .ok_or_else(invalid)?;
        let session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
        if session.surface != desk_diagnose_core::session::AgentSessionSurface::AiAssistant
            || session.actor_id != owner
            || session.device_id != row.device_id
            || session.conversation_id != conversation
        {
            return Err(invalid());
        }
        let root = session.approval_delegation_conversation_id();
        if root != conversation {
            let parent = agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(root))
                .filter(agent_session::Column::ActorId.eq(owner))
                .filter(agent_session::Column::DeviceId.eq(&row.device_id))
                .one(&self.db)
                .await?
                .ok_or_else(invalid)?;
            let parent =
                PersistedAgentSession::decode_json(&parent.state_json).map_err(|_| invalid())?;
            if parent.actor_id != owner
                || parent.device_id != row.device_id
                || parent.surface != desk_diagnose_core::session::AgentSessionSurface::AiAssistant
            {
                return Err(invalid());
            }
        }
        Ok(row)
    }
    fn scope<C: ColumnTrait>(
        column: C,
        subject: &agent_session::Model,
        owner: &str,
        children: bool,
    ) -> Condition {
        let mut scope = Condition::any().add(column.eq(&subject.conversation_id));
        if children {
            let children = agent_subagent_run::Entity::find()
                .select_only()
                .column(agent_subagent_run::Column::ChildConversationId)
                .filter(agent_subagent_run::Column::RootConversationId.eq(&subject.conversation_id))
                .filter(agent_subagent_run::Column::ActorId.eq(owner))
                .filter(agent_subagent_run::Column::DeviceId.eq(&subject.device_id));
            scope = scope.add(column.in_subquery(children.into_query()));
        }
        scope
    }
    pub async fn list(
        &self,
        owner: &str,
        query: &ApprovalReviewsQuery,
    ) -> Result<ApprovalReviewsDto, DbErr> {
        let children = query.include_children.unwrap_or(true);
        let subject = self.subject(&query.session, owner).await?;
        let scope = desk_diagnose_core::approval_observation::review_alias(&[
            owner,
            &query.session,
            if children { "children" } else { "current" },
        ]);
        let cursor = query
            .cursor
            .as_deref()
            .map(|v| serde_json::from_str::<PageCursor>(v).map_err(|_| invalid()))
            .transpose()?;
        if cursor.as_ref().is_some_and(|v| v.scope != scope) {
            return Err(invalid());
        }
        let limit = query.limit.unwrap_or(30).clamp(1, 100) as usize;
        let mut selection = review::Entity::find()
            .filter(Self::scope(
                review::Column::ConversationId,
                &subject,
                owner,
                children,
            ))
            .filter(review::Column::ActorId.eq(owner));
        let mut events = agent_run_event::Entity::find()
            .filter(Self::scope(
                agent_run_event::Column::RunId,
                &subject,
                owner,
                children,
            ))
            .filter(agent_run_event::Column::ActorId.eq(Some(owner.to_string())))
            .filter(agent_run_event::Column::ApprovalStatisticsJson.is_not_null());
        if let Some(cursor) = &cursor {
            selection = selection.filter(
                Condition::any()
                    .add(review::Column::CreatedAt.lt(cursor.at))
                    .add(
                        Condition::all()
                            .add(review::Column::CreatedAt.eq(cursor.at))
                            .add(review::Column::CandidateId.lt(&cursor.id)),
                    ),
            );

            // Event ids form the prefix of the readable early-failure identity.
            let event_id = cursor
                .id
                .strip_prefix("event:")
                .and_then(|v| v.rsplit_once(':'))
                .map_or(
                    if cursor.id.as_str() > "event:" {
                        "~"
                    } else {
                        ""
                    },
                    |(v, _)| v,
                );
            events = events.filter(
                Condition::any()
                    .add(agent_run_event::Column::ApprovalStatisticsAtMs.lt(cursor.at))
                    .add(
                        Condition::all()
                            .add(agent_run_event::Column::ApprovalStatisticsAtMs.eq(cursor.at))
                            .add(agent_run_event::Column::EventId.lte(event_id)),
                    ),
            );
        }
        let rows = selection
            .order_by_desc(review::Column::CreatedAt)
            .order_by_desc(review::Column::CandidateId)
            .limit((limit + 1) as u64)
            .all(&self.db)
            .await?;
        let early = events
            .order_by_desc(agent_run_event::Column::ApprovalStatisticsAtMs)
            .order_by_desc(agent_run_event::Column::EventId)
            .limit((limit + 1) as u64)
            .all(&self.db)
            .await?;
        let mut records = Vec::new();
        for row in rows {
            let mut dto = self.dto(&row).await?;
            dto.decision = None;
            dto.timeline.clear();
            dto.grant_states.clear();
            dto.task = None;
            records.push(dto);
        }
        for event in early {
            let observations: Vec<ReviewTracking> = event
                .approval_statistics_json
                .as_deref()
                .and_then(|v| serde_json::from_str(v).ok())
                .unwrap_or_default();
            for value in observations {
                let dto = self.early_dto(&event, &value, owner).await?;
                if cursor
                    .as_ref()
                    .is_none_or(|c| (dto.created_at_ms, &dto.id) < (c.at, &c.id))
                {
                    records.push(dto);
                }
            }
        }
        records.sort_by(|a, b| (b.created_at_ms, &b.id).cmp(&(a.created_at_ms, &a.id)));
        let more = records.len() > limit;
        records.truncate(limit);
        let next_cursor = if more {
            records
                .last()
                .map(|v| {
                    serde_json::to_string(&PageCursor {
                        scope,
                        at: v.created_at_ms,
                        id: v.id.clone(),
                    })
                    .map_err(|_| invalid())
                })
                .transpose()?
        } else {
            None
        };
        Ok(ApprovalReviewsDto {
            reviews: records,
            next_cursor,
        })
    }
    async fn early_dto(
        &self,
        event: &agent_run_event::Model,
        value: &ReviewTracking,
        owner: &str,
    ) -> Result<ApprovalReviewDto, DbErr> {
        let subject = self.subject(&event.run_id, owner).await?;
        let session =
            PersistedAgentSession::decode_json(&subject.state_json).map_err(|_| invalid())?;
        Ok(ApprovalReviewDto {
            id: format!("event:{}:{}", event.event_id, value.object_id),
            session: event.run_id.clone(),
            root_session: Some(session.approval_delegation_conversation_id().to_owned()),
            session_label: self.session_label(&event.run_id, owner).await?,
            task: self.session_task(&event.run_id, owner).await?,
            session_kind: desk_signal_facade::service::model_metrics::tag(
                &value.snapshot.session_kind,
            ),
            source_kind: value.snapshot.source_kind.clone(),
            source_id: event.event_id.clone(),
            status: desk_signal_facade::service::model_metrics::tag(&value.snapshot.outcome),
            reason: value
                .snapshot
                .reason
                .map(|r| desk_signal_facade::service::model_metrics::tag(&r)),
            model_name: None,
            protocol: None,
            model_revision: "0".into(),
            output_limit: None,
            created_at_ms: value.snapshot.eligible_at_ms,
            finished_at_ms: None,
            business_finished_at_ms: value.snapshot.finished_at_ms,
            stop_reason: None,
            usage: None,
            recording_state: "not_sent".into(),
            captured_bytes: "0".into(),
            decision: None,
            grant_states: vec![],
            execution_state: None,
            timeline: vec![ApprovalTraceStageDto {
                stage: "review_failed".into(),
                origin: "server".into(),
                at_ms: value
                    .snapshot
                    .finished_at_ms
                    .unwrap_or(value.snapshot.eligible_at_ms),
            }],
        })
    }
    async fn session_task(
        &self,
        session: &str,
        owner: &str,
    ) -> Result<
        Option<desk_agent_protocol::ai_assistant::subagent::AiAssistantSubAgentSummary>,
        DbErr,
    > {
        let row = agent_subagent_run::Entity::find()
            .filter(agent_subagent_run::Column::ChildConversationId.eq(session))
            .filter(agent_subagent_run::Column::ActorId.eq(owner))
            .one(&self.db)
            .await?;
        Ok(row
            .and_then(|r| {
                serde_json::from_str::<desk_diagnose_core::subagent::state::SubAgentRun>(
                    &r.state_json,
                )
                .ok()
            })
            .filter(|r| r.actor_id == owner && r.child_conversation_id == session)
            .map(|r| r.summary()))
    }
    async fn session_label(&self, session: &str, owner: &str) -> Result<Option<String>, DbErr> {
        let row = agent_subagent_run::Entity::find()
            .filter(agent_subagent_run::Column::ChildConversationId.eq(session))
            .filter(agent_subagent_run::Column::ActorId.eq(owner))
            .one(&self.db)
            .await?;
        Ok(row
            .and_then(|r| {
                serde_json::from_str::<desk_diagnose_core::subagent::state::SubAgentRun>(
                    &r.state_json,
                )
                .ok()
            })
            .map(|r| r.name))
    }
    async fn row(&self, owner: &str, session: &str, id: &str) -> Result<review::Model, DbErr> {
        let subject = self.subject(session, owner).await?;
        review::Entity::find()
            .filter(review::Column::CandidateId.eq(id))
            .filter(review::Column::ActorId.eq(owner))
            .filter(Self::scope(
                review::Column::ConversationId,
                &subject,
                owner,
                true,
            ))
            .one(&self.db)
            .await?
            .ok_or_else(invalid)
    }
    async fn dto(&self, row: &review::Model) -> Result<ApprovalReviewDto, DbErr> {
        let subject = self
            .subject(&row.conversation_id, &row.actor_id.clone())
            .await?;
        if subject.device_id != row.device_id {
            return Err(invalid());
        }
        let record = trace::Entity::find_by_id(&row.candidate_id)
            .filter(trace::Column::OwnerId.eq(row.actor_id.clone()))
            .filter(trace::Column::ConversationId.eq(&row.conversation_id))
            .filter(trace::Column::DeviceId.eq(&row.device_id))
            .one(&self.db)
            .await?;
        let now = chrono::Utc::now().timestamp_millis();
        let owner = row.actor_id.clone();
        let tracking = row
            .statistics_json
            .as_deref()
            .and_then(|v| serde_json::from_str::<ReviewTracking>(v).ok());
        let mut grant_states = Vec::new();
        let mut timeline = Vec::new();
        if let Some(tracking) = &tracking
            && let Some(at) = tracking.snapshot.finished_at_ms
        {
            timeline.push(ApprovalTraceStageDto {
                stage: "business_settled".into(),
                origin: "server".into(),
                at_ms: at,
            });
        }
        let grants = grant::Entity::find()
            .filter(grant::Column::RunId.eq(&row.conversation_id))
            .filter(grant::Column::ActorId.eq(&owner))
            .limit(100)
            .all(&self.db)
            .await?;
        for item in grants {
            if let Ok(issued) = serde_json::from_str::<
                desk_agent_protocol::capability_grant::CapabilityGrant,
            >(&item.issued_payload_json)
                && matches!(issued.issued_by,desk_agent_protocol::capability_grant::CapabilityGrantIssuer::AiApproval(ref p) if p.candidate_id==row.candidate_id)
            {
                let status = if item.status == "active" && issued.expires_at_unix_ms <= now as u64 {
                    "expired".into()
                } else {
                    item.status
                };
                grant_states.push(status);
                timeline.push(ApprovalTraceStageDto {
                    stage: "grant_written".into(),
                    origin: "server".into(),
                    at_ms: item.created_at.timestamp_millis(),
                });
            }
        }
        let mut execution_state = None;
        if row.source_kind == "concrete_call" {
            use crate::entity::agent_action_item as work;
            if let Some(item) = work::Entity::find()
                .filter(work::Column::ConversationId.eq(&row.conversation_id))
                .filter(work::Column::ActorId.eq(&owner))
                .filter(work::Column::ToolCallId.eq(&row.source_id))
                .one(&self.db)
                .await?
            {
                execution_state = Some(if item.result_json.is_none() && item.status == "done" {
                    "unknown".into()
                } else {
                    item.status
                });
                if let Some(at) = item.dispatch_intent_at {
                    timeline.push(ApprovalTraceStageDto {
                        stage: "dispatch_recorded".into(),
                        origin: "executor".into(),
                        at_ms: at.timestamp_millis(),
                    });
                }
                if item.result_json.is_some() {
                    timeline.push(ApprovalTraceStageDto {
                        stage: "execution_result".into(),
                        origin: "executor".into(),
                        at_ms: item.updated_at.timestamp_millis(),
                    });
                }
            }
        }
        Ok(ApprovalReviewDto {
            id: row.candidate_id.clone(),
            session: row.conversation_id.clone(),
            source_kind: row.source_kind.clone(),
            source_id: row.source_id.clone(),
            status: tracking
                .as_ref()
                .map(|t| desk_signal_facade::service::model_metrics::tag(&t.snapshot.outcome))
                .unwrap_or_else(|| row.status.clone()),
            model_revision: row.model_config_revision.to_string(),
            session_label: self.session_label(&row.conversation_id, &owner).await?,
            task: self.session_task(&row.conversation_id, &owner).await?,
            decision: row
                .decision_json
                .as_deref()
                .and_then(|v| serde_json::from_str(v).ok()),
            grant_states,
            execution_state,
            timeline,
            business_finished_at_ms: tracking.as_ref().and_then(|t| t.snapshot.finished_at_ms),
            root_session: record.as_ref().and_then(|r| r.root_conversation_id.clone()),
            session_kind: tracking
                .as_ref()
                .map(|t| desk_signal_facade::service::model_metrics::tag(&t.snapshot.session_kind))
                .unwrap_or_else(|| {
                    record
                        .as_ref()
                        .map_or("unknown".into(), |r| r.session_kind.clone())
                }),
            reason: tracking
                .as_ref()
                .and_then(|t| t.snapshot.reason)
                .map(|r| desk_signal_facade::service::model_metrics::tag(&r))
                .or_else(|| record.as_ref().and_then(|r| r.failure_reason.clone())),
            model_name: record.as_ref().and_then(|r| r.model_name.clone()),
            protocol: record.as_ref().and_then(|r| r.protocol.clone()),
            output_limit: record
                .as_ref()
                .and_then(|r| r.output_limit.map(|v| v.to_string())),
            created_at_ms: row.created_at,
            finished_at_ms: record.as_ref().and_then(|r| r.finished_at_ms),
            stop_reason: record.as_ref().and_then(|r| r.stop_reason.clone()),
            usage: record
                .as_ref()
                .and_then(|r| r.usage_json.as_deref())
                .and_then(|v| serde_json::from_str(v).ok()),
            recording_state: record.as_ref().map_or(
                if row.statistics_json.is_some() {
                    "unavailable"
                } else {
                    "not_recorded"
                }
                .into(),
                |r| {
                    if r.expires_at_ms <= now {
                        "expired".into()
                    } else {
                        r.recording_state.clone()
                    }
                },
            ),
            captured_bytes: record.map_or("0".into(), |r| r.captured_bytes.to_string()),
        })
    }
    pub async fn detail(
        &self,
        owner: &str,
        query: &ApprovalReviewQuery,
    ) -> Result<ApprovalReviewDto, DbErr> {
        if let Some((event_id, object_id)) = query
            .id
            .strip_prefix("event:")
            .and_then(|v| v.rsplit_once(':'))
        {
            let subject = self.subject(&query.session, owner).await?;
            let event = agent_run_event::Entity::find()
                .filter(agent_run_event::Column::EventId.eq(event_id))
                .filter(agent_run_event::Column::ActorId.eq(Some(owner.to_string())))
                .filter(Self::scope(
                    agent_run_event::Column::RunId,
                    &subject,
                    owner,
                    true,
                ))
                .one(&self.db)
                .await?
                .ok_or_else(invalid)?;
            let values: Vec<ReviewTracking> = event
                .approval_statistics_json
                .as_deref()
                .and_then(|v| serde_json::from_str(v).ok())
                .unwrap_or_default();
            let value = values
                .iter()
                .find(|v| v.object_id == object_id)
                .ok_or_else(invalid)?;
            return self.early_dto(&event, value, owner).await;
        }
        self.dto(&self.row(owner, &query.session, &query.id).await?)
            .await
    }
    pub async fn parts(
        &self,
        owner: &str,
        query: &ApprovalPartsQuery,
    ) -> Result<ApprovalTracePartsDto, DbErr> {
        if !["input", "reasoning", "answer", "decision", "event"].contains(&query.kind.as_str()) {
            return Err(invalid());
        }
        let dto = self
            .detail(
                owner,
                &ApprovalReviewQuery {
                    session: query.session.clone(),
                    id: query.id.clone(),
                },
            )
            .await?;
        if [
            "expired",
            "budget_cleaned",
            "deleted",
            "not_recorded",
            "not_sent",
            "disabled",
        ]
        .contains(&dto.recording_state.as_str())
        {
            return Ok(ApprovalTracePartsDto {
                parts: vec![],
                next_after: None,
                recording_state: dto.recording_state,
            });
        }
        let mut select = part::Entity::find()
            .filter(part::Column::TraceId.eq(&query.id))
            .filter(part::Column::Kind.eq(&query.kind));
        if let Some(after) = query.after {
            select = select.filter(part::Column::Sequence.gt(i64::from(after)));
        }
        let mut rows = select
            .order_by_asc(part::Column::Sequence)
            .limit(5)
            .all(&self.db)
            .await?;
        let more = rows.len() > 4;
        rows.truncate(4);
        let next_after = more
            .then(|| rows.last().map(|r| r.sequence as u32))
            .flatten();
        Ok(ApprovalTracePartsDto {
            parts: rows
                .into_iter()
                .map(|r| ApprovalTracePartDto {
                    sequence: r.sequence as u32,
                    kind: r.kind,
                    text: r.text,
                    at_ms: r.at_ms,
                })
                .collect(),
            next_after,
            recording_state: dto.recording_state,
        })
    }
    pub async fn delete_for_session(&self, conversation: &str, owner: &str) -> Result<(), DbErr> {
        let txn = self.db.begin().await?;
        self.quota(&txn).await?;
        let rows = trace::Entity::find()
            .filter(
                Condition::any()
                    .add(trace::Column::ConversationId.eq(conversation))
                    .add(trace::Column::RootConversationId.eq(conversation)),
            )
            .filter(trace::Column::OwnerId.eq(owner))
            .all(&txn)
            .await?;
        for row in rows {
            self.remove_body_on(&txn, &row, "deleted").await?;
        }
        txn.commit().await
    }
    async fn quota<C: ConnectionTrait>(&self, db: &C) -> Result<trace::Model, DbErr> {
        trace::Entity::insert(trace::ActiveModel {
            id: Set(QUOTA.into()),
            owner_id: Set("system".into()),
            conversation_id: Set("system".into()),
            device_id: Set(String::new()),
            root_conversation_id: Set(None),
            session_kind: Set("unknown".into()),
            source_kind: Set(String::new()),
            source_id: Set(String::new()),
            lease_epoch: Set(0),
            model_revision: Set(0),
            model_name: Set(None),
            protocol: Set(None),
            output_limit: Set(None),
            stop_reason: Set(None),
            usage_json: Set(None),
            failure_reason: Set(None),
            recording_state: Set("quota".into()),
            captured_bytes: Set(0),
            created_at_ms: Set(0),
            finished_at_ms: Set(None),
            expires_at_ms: Set(i64::MAX),
        })
        .on_conflict(
            OnConflict::column(trace::Column::Id)
                .do_nothing()
                .to_owned(),
        )
        .exec_without_returning(db)
        .await?;
        trace::Entity::find_by_id(QUOTA)
            .lock_exclusive()
            .one(db)
            .await?
            .ok_or_else(invalid)
    }
    async fn remove_body_on<C: ConnectionTrait>(
        &self,
        db: &C,
        row: &trace::Model,
        state: &str,
    ) -> Result<(), DbErr> {
        part::Entity::delete_many()
            .filter(part::Column::TraceId.eq(&row.id))
            .exec(db)
            .await?;
        trace::Entity::update_many()
            .col_expr(trace::Column::RecordingState, Expr::value(state))
            .col_expr(trace::Column::CapturedBytes, Expr::value(0))
            .filter(trace::Column::Id.eq(&row.id))
            .exec(db)
            .await?;
        trace::Entity::update_many()
            .col_expr(
                trace::Column::CapturedBytes,
                Expr::col(trace::Column::CapturedBytes).sub(row.captured_bytes),
            )
            .filter(trace::Column::Id.eq(QUOTA))
            .exec(db)
            .await?;
        Ok(())
    }
    pub async fn cleanup(&self) -> Result<(), DbErr> {
        let settings = load_settings(&self.db).await?;
        let txn = self.db.begin().await?;
        self.quota(&txn).await?;
        let now = chrono::Utc::now().timestamp_millis();
        trace::Entity::update_many()
            .col_expr(
                trace::Column::ExpiresAtMs,
                Expr::col(trace::Column::CreatedAtMs)
                    .add(i64::from(settings.retention_days) * 86_400_000),
            )
            .filter(trace::Column::Id.ne(QUOTA))
            .filter(
                Expr::col(trace::Column::ExpiresAtMs).gt(Expr::col(trace::Column::CreatedAtMs)
                    .add(i64::from(settings.retention_days) * 86_400_000)),
            )
            .exec(&txn)
            .await?;
        let rows = trace::Entity::find()
            .filter(trace::Column::Id.ne(QUOTA))
            .filter(trace::Column::ExpiresAtMs.lte(now))
            .filter(trace::Column::CapturedBytes.gt(0))
            .limit(100)
            .all(&txn)
            .await?;
        for row in rows {
            self.remove_body_on(&txn, &row, "expired").await?;
        }
        let quota = self.quota(&txn).await?;
        let budget = settings
            .storage_budget_bytes
            .parse::<i64>()
            .map_err(|_| invalid())?;
        if quota.captured_bytes >= budget {
            let rows = trace::Entity::find()
                .filter(trace::Column::Id.ne(QUOTA))
                .filter(trace::Column::RecordingState.ne("collecting"))
                .filter(trace::Column::CapturedBytes.gt(0))
                .order_by_asc(trace::Column::CreatedAtMs)
                .limit(100)
                .all(&txn)
                .await?;
            let mut bytes = quota.captured_bytes;
            for row in rows {
                if bytes < budget - budget / 10 {
                    break;
                }
                bytes -= row.captured_bytes;
                self.remove_body_on(&txn, &row, "budget_cleaned").await?;
            }
        }
        let orphan = trace::Entity::find()
            .filter(trace::Column::Id.ne(QUOTA))
            .filter(
                Condition::any()
                    .add(
                        trace::Column::ConversationId.not_in_subquery(
                            agent_session::Entity::find()
                                .select_only()
                                .column(agent_session::Column::ConversationId)
                                .into_query(),
                        ),
                    )
                    .add(
                        Condition::all()
                            .add(trace::Column::RootConversationId.is_not_null())
                            .add(
                                trace::Column::RootConversationId.not_in_subquery(
                                    agent_session::Entity::find()
                                        .select_only()
                                        .column(agent_session::Column::ConversationId)
                                        .into_query(),
                                ),
                            ),
                    ),
            )
            .limit(100)
            .all(&txn)
            .await?;
        for row in orphan {
            self.remove_body_on(&txn, &row, "deleted").await?;
            trace::Entity::delete_by_id(row.id).exec(&txn).await?;
        }
        // Header tombstones outlive body expiry only while the authoritative review exists.
        let retired = trace::Entity::find()
            .filter(trace::Column::Id.ne(QUOTA))
            .filter(trace::Column::CapturedBytes.eq(0))
            .filter(trace::Column::ExpiresAtMs.lte(now))
            .filter(
                trace::Column::Id.not_in_subquery(
                    review::Entity::find()
                        .select_only()
                        .column(review::Column::CandidateId)
                        .into_query(),
                ),
            )
            .limit(100)
            .all(&txn)
            .await?;
        for row in retired {
            trace::Entity::delete_by_id(row.id).exec(&txn).await?;
        }
        txn.commit().await
    }
    pub async fn run_maintenance(self) {
        let mut reviews_after = 0i64;
        let mut events_after = 0i64;
        loop {
            // Replaying committed source metadata is safe on every node: the metric
            // store owns shared deduplication and atomic contribution replacement.
            if let Err(error) = self.cleanup().await {
                log::warn!(
                    "approval detail cleanup failed: {}",
                    error.sql_err().is_some()
                );
            }
            let _ = self.reconcile(&mut reviews_after, &mut events_after).await;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    }
    async fn reconcile(
        &self,
        reviews_after: &mut i64,
        events_after: &mut i64,
    ) -> Result<(), DbErr> {
        let now = chrono::Utc::now().timestamp_millis();
        let rows = review::Entity::find()
            .filter(review::Column::Id.gt(*reviews_after))
            .filter(review::Column::StatisticsJson.is_not_null())
            .order_by_asc(review::Column::Id)
            .limit(200)
            .all(&self.db)
            .await?;
        *reviews_after = rows.last().map_or(0, |v| v.id);
        for row in rows {
            if let Some(tracking) = row
                .statistics_json
                .as_deref()
                .and_then(|v| serde_json::from_str::<ReviewTracking>(v).ok())
            {
                crate::approval_metrics::submit(
                    &tracking,
                    if tracking.snapshot.terminal() {
                        desk_diagnose_core::model_observability::ObservationPhase::Completed
                    } else {
                        desk_diagnose_core::model_observability::ObservationPhase::Started
                    },
                    if tracking.snapshot.terminal() { 2 } else { 0 },
                    now,
                );
            }
        }
        let rows = agent_run_event::Entity::find()
            .filter(agent_run_event::Column::Id.gt(*events_after))
            .filter(agent_run_event::Column::ApprovalStatisticsJson.is_not_null())
            .order_by_asc(agent_run_event::Column::Id)
            .limit(200)
            .all(&self.db)
            .await?;
        *events_after = rows.last().map_or(0, |v| v.id);
        for row in rows {
            let values: Vec<ReviewTracking> = row
                .approval_statistics_json
                .as_deref()
                .and_then(|v| serde_json::from_str(v).ok())
                .unwrap_or_default();
            for tracking in values {
                crate::approval_metrics::submit(
                    &tracking,
                    desk_diagnose_core::model_observability::ObservationPhase::Completed,
                    2,
                    now,
                );
            }
        }
        Ok(())
    }
}
#[async_trait]
impl TraceBackend for Store {
    async fn begin(&self, id: &TraceIdentity, settings: &ApprovalTraceSettings) -> Result<(), ()> {
        let txn = self.db.begin().await.map_err(|_| ())?;
        self.quota(&txn).await.map_err(|_| ())?;
        let session = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&id.conversation_id))
            .filter(agent_session::Column::ActorId.eq(&id.owner_id))
            .filter(agent_session::Column::DeviceId.eq(&id.device_id))
            .one(&txn)
            .await
            .map_err(|_| ())?;
        if session.is_none() {
            return Err(());
        }
        if id.root_conversation_id != id.conversation_id
            && agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(&id.root_conversation_id))
                .filter(agent_session::Column::ActorId.eq(&id.owner_id))
                .one(&txn)
                .await
                .map_err(|_| ())?
                .is_none()
        {
            return Err(());
        }
        trace::Entity::insert(trace::ActiveModel {
            id: Set(id.id.clone()),
            owner_id: Set(id.owner_id.clone()),
            conversation_id: Set(id.conversation_id.clone()),
            device_id: Set(id.device_id.clone()),
            root_conversation_id: Set(Some(id.root_conversation_id.clone())),
            session_kind: Set(if id.conversation_id == id.root_conversation_id {
                "main"
            } else {
                "subagent"
            }
            .into()),
            source_kind: Set(id.source_kind.clone()),
            source_id: Set(id.source_id.clone()),
            lease_epoch: Set(id.lease_epoch),
            model_revision: Set(id.model_revision),
            model_name: Set(None),
            protocol: Set(None),
            output_limit: Set(None),
            stop_reason: Set(None),
            usage_json: Set(None),
            failure_reason: Set(None),
            recording_state: Set("collecting".into()),
            captured_bytes: Set(0),
            created_at_ms: Set(id.created_at_ms),
            finished_at_ms: Set(None),
            expires_at_ms: Set(id.created_at_ms + i64::from(settings.retention_days) * 86_400_000),
        })
        .on_conflict(
            OnConflict::column(trace::Column::Id)
                .do_nothing()
                .to_owned(),
        )
        .exec_without_returning(&txn)
        .await
        .map_err(|_| ())?;
        txn.commit().await.map_err(|_| ())
    }
    async fn append(
        &self,
        id: &TraceIdentity,
        value: ApprovalTracePartDto,
        settings: &ApprovalTraceSettings,
    ) -> Result<(), TraceAppendError> {
        let txn = self
            .db
            .begin()
            .await
            .map_err(|_| TraceAppendError::Unavailable)?;
        let quota = self
            .quota(&txn)
            .await
            .map_err(|_| TraceAppendError::Unavailable)?;
        let row = trace::Entity::find_by_id(&id.id)
            .filter(trace::Column::LeaseEpoch.eq(id.lease_epoch))
            .filter(trace::Column::RecordingState.eq("collecting"))
            .filter(trace::Column::ExpiresAtMs.gt(chrono::Utc::now().timestamp_millis()))
            .one(&txn)
            .await
            .map_err(|_| TraceAppendError::Unavailable)?
            .ok_or(TraceAppendError::Deleted)?;
        let alive = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&id.conversation_id))
            .filter(agent_session::Column::ActorId.eq(&id.owner_id))
            .filter(agent_session::Column::DeviceId.eq(&id.device_id))
            .one(&txn)
            .await
            .map_err(|_| TraceAppendError::Unavailable)?
            .is_some();
        let root_alive = id.root_conversation_id == id.conversation_id
            || agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(&id.root_conversation_id))
                .filter(agent_session::Column::ActorId.eq(&id.owner_id))
                .one(&txn)
                .await
                .map_err(|_| TraceAppendError::Unavailable)?
                .is_some();
        let bytes = i64::try_from(value.text.len()).map_err(|_| TraceAppendError::Unavailable)?;
        if !alive || !root_alive {
            return Err(TraceAppendError::Deleted);
        }
        let key = format!("{}:{}:{}", id.id, id.lease_epoch, value.sequence);
        if let Some(stored) = part::Entity::find_by_id(&key)
            .one(&txn)
            .await
            .map_err(|_| TraceAppendError::Unavailable)?
        {
            if stored.kind != value.kind || stored.text != value.text {
                return Err(TraceAppendError::Unavailable);
            }
            return txn
                .commit()
                .await
                .map_err(|_| TraceAppendError::Unavailable);
        }
        if row.captured_bytes + bytes > i64::from(settings.max_trace_bytes) {
            return Err(TraceAppendError::SingleLimit);
        }
        if quota.captured_bytes + bytes
            > settings
                .storage_budget_bytes
                .parse::<i64>()
                .map_err(|_| TraceAppendError::Unavailable)?
        {
            return Err(TraceAppendError::StorageFull);
        }
        let result = part::Entity::insert(part::ActiveModel {
            id: Set(format!("{}:{}:{}", id.id, id.lease_epoch, value.sequence)),
            trace_id: Set(id.id.clone()),
            sequence: Set(i64::from(value.sequence)),
            kind: Set(value.kind),
            text: Set(value.text),
            at_ms: Set(value.at_ms),
        })
        .on_conflict(OnConflict::column(part::Column::Id).do_nothing().to_owned())
        .exec_without_returning(&txn)
        .await
        .map_err(|_| TraceAppendError::Unavailable)?;
        if result > 0 {
            for key in [&id.id, QUOTA] {
                trace::Entity::update_many()
                    .col_expr(
                        trace::Column::CapturedBytes,
                        Expr::col(trace::Column::CapturedBytes).add(bytes),
                    )
                    .filter(trace::Column::Id.eq(key))
                    .exec(&txn)
                    .await
                    .map_err(|_| TraceAppendError::Unavailable)?;
            }
        }
        txn.commit()
            .await
            .map_err(|_| TraceAppendError::Unavailable)
    }
    async fn finish(
        &self,
        id: &TraceIdentity,
        value: TraceCompletion,
        state: &str,
    ) -> Result<(), ()> {
        trace::Entity::update_many()
            .col_expr(trace::Column::RecordingState, Expr::value(state))
            .col_expr(trace::Column::ModelName, Expr::value(value.model_name))
            .col_expr(trace::Column::Protocol, Expr::value(value.protocol))
            .col_expr(trace::Column::OutputLimit, Expr::value(value.output_limit))
            .col_expr(trace::Column::StopReason, Expr::value(value.stop_reason))
            .col_expr(trace::Column::UsageJson, Expr::value(value.usage_json))
            .col_expr(trace::Column::FailureReason, Expr::value(value.reason))
            .col_expr(
                trace::Column::FinishedAtMs,
                Expr::value(value.finished_at_ms),
            )
            .filter(trace::Column::Id.eq(&id.id))
            .filter(trace::Column::LeaseEpoch.eq(id.lease_epoch))
            .filter(trace::Column::RecordingState.eq("collecting"))
            .exec(&self.db)
            .await
            .map_err(|_| ())?;
        Ok(())
    }
}

use crate::config::ConfigConnection;
pub async fn load_settings(db: &DatabaseConnection) -> Result<ApprovalTraceSettings, DbErr> {
    Ok(db.config_read().await.approval_trace.clone())
}
pub async fn save_settings(
    db: &DatabaseConnection,
    mut requested: ApprovalTraceSettings,
) -> Result<Option<ApprovalTraceSettings>, DbErr> {
    requested.validate().map_err(|_| invalid())?;
    let expected = requested.revision.clone();
    requested.revision = expected
        .parse::<u64>()
        .map_err(|_| invalid())?
        .checked_add(1)
        .ok_or_else(invalid)?
        .to_string();
    db.config_context()
        .update::<_, DbErr, _>(|config| {
            if config.approval_trace.revision != expected {
                return Ok(None);
            }
            config.approval_trace = requested.clone();
            Ok(Some(requested))
        })
        .await
}

#[cfg(test)]
mod tests;
