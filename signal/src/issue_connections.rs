//! OSS connection configuration persists through the host's atomic file update.

use crate::config::{ConfigConnection, connection::DatabaseConnection};
use desk_signal_facade::mcp_client::settings::{IssueConnectionsConfig, IssueConnectionsUpdate};
use sea_orm::DbErr;

#[derive(Debug)]
pub enum WriteError {
    Conflict,
    Invalid(&'static str),
    Backend(DbErr),
}

impl From<DbErr> for WriteError {
    fn from(error: DbErr) -> Self {
        Self::Backend(error)
    }
}

pub async fn read<C: ConfigConnection>(db: &C) -> Result<IssueConnectionsConfig, DbErr> {
    Ok(db.config_read().await.issue_connections.clone())
}

pub async fn select_issues(
    db: &impl ConfigConnection,
    actor: &str,
    selected: &[desk_agent_protocol::external_tools::IssueSelection],
) -> Result<Vec<desk_diagnose_core::external_tools::IssueAccess>, desk_agent_protocol::AgentError> {
    if selected.is_empty() {
        return Ok(Vec::new());
    }
    let unavailable = || {
        desk_diagnose_core::subagent::invalid(
            "Selected external issue or connection is unavailable",
        )
    };
    let config = read(db).await.map_err(|_| unavailable())?;
    let issues = config
        .bind_issues(actor, selected)
        .await
        .map_err(|_| unavailable())?;
    if read(db).await.map_err(|_| unavailable())?.revision != config.revision {
        return Err(unavailable());
    }
    Ok(issues)
}

/// Current subscription and connection checks at the existing dispatch fence.
pub(crate) async fn check_work_on(
    db: &impl crate::config::ConfigConnection,
    session: &desk_diagnose_core::session::PersistedAgentSession,
    requires_device: bool,
    now: i64,
) -> Result<(), desk_assistant_data_store::AssistantStoreError> {
    use desk_assistant_data_store::{AssistantStoreError, issue_authority};
    let Some(admission) =
        issue_authority::authorized_session_on(db, session, requires_device, now).await?
    else {
        return Ok(());
    };
    let config = read(db).await?;
    if config
        .connection_for_issue(&session.actor_id, &admission.issue)
        .is_none()
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}

pub async fn permits_issues(
    db: &impl ConfigConnection,
    actor: &str,
    issues: &[desk_diagnose_core::external_tools::IssueAccess],
) -> Result<bool, DbErr> {
    if issues.is_empty() {
        return Ok(true);
    }
    let config = read(db).await?;
    Ok(issues
        .iter()
        .all(|issue| config.connection_for_issue(actor, issue).is_some()))
}

pub async fn update(
    db: &DatabaseConnection,
    request: &IssueConnectionsUpdate,
) -> Result<IssueConnectionsConfig, WriteError> {
    db.config_context()
        .update(|config| {
            if config.issue_connections.revision != request.expected_revision {
                return Err(WriteError::Conflict);
            }
            let candidate = config
                .issue_connections
                .candidate(request)
                .map_err(WriteError::Invalid)?;
            config.issue_connections = candidate.clone();
            Ok(Some(candidate))
        })
        .await?
        .ok_or(WriteError::Conflict)
}
