//! Recover terminal rehearsal records without invoking a model or dispatching tools.
use super::*;
use crate::entity::agent_session;
use desk_diagnose_core::{
    chat::ChatRole,
    session::{PersistedAgentSession, TurnState},
};
use sea_orm::{QueryOrder, QuerySelect};

#[derive(Debug, Default)]
pub struct RehearsalRecoveryReport {
    pub scanned: usize,
    pub settled: usize,
    pub deferred: usize,
    pub next_cursor: Option<i64>,
}

impl ScheduleStore {
    /// Candidates are hints; the existing terminal transactions recheck every fence.
    pub async fn recover_terminal_rehearsals(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<RehearsalRecoveryReport, ScheduleStoreError> {
        if after_id < 0 || limit == 0 || limit > 32 {
            return Err(ScheduleStoreError::Invalid);
        }
        let candidates = rehearsal::Entity::find()
            .filter(rehearsal::Column::Status.eq("running"))
            .filter(rehearsal::Column::Id.gt(after_id))
            .order_by_asc(rehearsal::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await?;
        let mut report = RehearsalRecoveryReport {
            scanned: candidates.len(),
            next_cursor: (candidates.len() == limit as usize)
                .then(|| candidates.last().unwrap().id),
            ..Default::default()
        };
        for candidate in candidates {
            let row = agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(&candidate.conversation_id))
                .one(&self.db)
                .await?;
            let snapshot = row
                .as_ref()
                .and_then(|row| PersistedAgentSession::decode_json(&row.state_json).ok());
            let settled = match snapshot.as_ref().map(|session| session.turn_state) {
                Some(TurnState::Failed | TurnState::Cancelled) => self
                    .finish_terminal_rehearsal(candidate.owner_user_id, &candidate.rehearsal_id)
                    .await
                    .map(|_| true),
                Some(TurnState::Idle) => {
                    if let Some(answer) = snapshot.as_ref().and_then(|session| {
                        session
                            .conversation
                            .iter()
                            .rev()
                            .find(|message| message.role == ChatRole::Assistant)
                    }) {
                        self.finish_answered_rehearsal(
                            candidate.owner_user_id,
                            &candidate.rehearsal_id,
                            &answer.text,
                        )
                        .await
                        .map(|_| true)
                    } else {
                        Ok(false)
                    }
                }
                _ => Ok(false),
            };
            let settled = match settled {
                Ok(settled) => settled,
                Err(ScheduleStoreError::Backend(error)) => {
                    return Err(ScheduleStoreError::Backend(error));
                }
                Err(_) => false,
            };
            if settled {
                report.settled += 1;
            } else {
                report.deferred += 1;
            }
        }
        Ok(report)
    }
}
