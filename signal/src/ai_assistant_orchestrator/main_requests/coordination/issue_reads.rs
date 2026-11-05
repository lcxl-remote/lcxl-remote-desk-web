//! Current connection authority and native batch fences around the HTTP read.

use super::*;
use desk_assistant_data_store::main_request::coordination::{ExternalIssueReader, IssueReadScope};
use desk_diagnose_core::external_tools::{IssueAccess, IssueReadMethod, IssueReadOutput};

pub(super) struct Reader<'a> {
    pub executor: &'a SignalMainRequestExecutor,
    pub head: &'a agent_main_request::Model,
    pub batch: &'a Batch,
}

impl Reader<'_> {
    async fn check(&self, source: &IssueReadScope) -> Result<(), AgentError> {
        self.executor.check_batch(self.head, self.batch).await?;
        if !self
            .batch
            .issue_source_current(&self.executor.db, source)
            .await
            .map_err(|error| error.into_agent_error())?
        {
            return Err(unavailable());
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ExternalIssueReader for Reader<'_> {
    async fn permitted(&self, access: &IssueAccess) -> Result<bool, AgentError> {
        Ok(crate::issue_connections::read(&self.executor.db)
            .await
            .map_err(storage)?
            .connection_for_issue(&self.head.actor_id, access)
            .is_some())
    }

    async fn read(
        &self,
        source: &IssueReadScope,
        method: IssueReadMethod,
        snapshot: Option<IssueReadOutput>,
    ) -> Result<IssueReadOutput, AgentError> {
        let config = crate::issue_connections::read(&self.executor.db)
            .await
            .map_err(storage)?;
        let connection = config
            .connection_for_issue(&self.head.actor_id, &source.access)
            .cloned()
            .ok_or_else(unavailable)?;
        let saved = connection.clone();
        let client = tokio::task::spawn_blocking(move || connection.client())
            .await
            .map_err(|_| unavailable())?
            .map_err(|_| unavailable())?;
        self.check(source).await?;
        let config = crate::issue_connections::read(&self.executor.db)
            .await
            .map_err(storage)?;
        if config.connection_for_issue(&self.head.actor_id, &source.access) != Some(&saved) {
            return Err(unavailable());
        }
        let result = if let Some(snapshot) = snapshot {
            client
                .verify_issue_target(&source.access.target)
                .await
                .map_err(|_| unavailable())?;
            snapshot
        } else {
            client
                .read_issue(&source.access.target, method)
                .await
                .map_err(|_| unavailable())?
        };
        self.check(source).await?;
        let config = crate::issue_connections::read(&self.executor.db)
            .await
            .map_err(storage)?;
        if config.connection_for_issue(&self.head.actor_id, &source.access) != Some(&saved) {
            return Err(unavailable());
        }
        Ok(result)
    }
}
