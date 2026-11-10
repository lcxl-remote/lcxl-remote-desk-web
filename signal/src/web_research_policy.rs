//! File-owned OSS bound on the bytes one public web page read may take.
use crate::config::{ConfigConnection, connection::DatabaseConnection};
use desk_agent_protocol::ai_assistant::web_research_policy::{
    UpdateWebResearchPolicy, WebResearchPolicy,
};

#[derive(Debug)]
pub enum WriteError {
    Conflict,
    Invalid,
    Db(sea_orm::DbErr),
}
impl From<sea_orm::DbErr> for WriteError {
    fn from(error: sea_orm::DbErr) -> Self {
        Self::Db(error)
    }
}

pub async fn read<C: ConfigConnection>(db: &C) -> WebResearchPolicy {
    db.config_read().await.web_research_policy.clone()
}

/// The response bytes one public web page read may take.
pub async fn max_fetch_bytes<C: ConfigConnection>(db: &C) -> usize {
    db.config_read().await.web_research_policy.max_fetch_bytes as usize
}

pub async fn update(
    db: &DatabaseConnection,
    request: &UpdateWebResearchPolicy,
) -> Result<WebResearchPolicy, WriteError> {
    db.config_context()
        .update(|config| {
            if config.web_research_policy.revision != request.expected_revision {
                return Err(WriteError::Conflict);
            }
            let next = config
                .web_research_policy
                .candidate(request.max_fetch_bytes)
                .map_err(|_| WriteError::Invalid)?;
            config.web_research_policy = next.clone();
            Ok(Some(next))
        })
        .await?
        .ok_or(WriteError::Conflict)
}
