//! Bounded read preparation, immutable posting and REST-only reconciliation.

use super::{
    McpClientError,
    issue_connection::{IssueConnection, PreparedReply},
    settings::IssueConnectionConfig,
};
use desk_diagnose_core::issue_reply::{
    FrozenIssueReply, IssueReplyReceipt, IssueReplyReceiptKey, ReplyReconciliationPage,
    ReplyTransport, ReplyTransportError,
};
use std::{num::NonZeroU32, time::Duration};

pub struct IssueReplyTransport {
    connection: IssueConnection,
}

impl IssueReplyTransport {
    /// Trust-store loading is blocking; the host holds no transaction here.
    pub async fn from_config(config: IssueConnectionConfig) -> Result<Self, ReplyTransportError> {
        let connection = tokio::task::spawn_blocking(move || config.client())
            .await
            .map_err(|_| ReplyTransportError::Unavailable)?
            .map_err(map_error)?;
        Ok(Self { connection })
    }
}

#[async_trait::async_trait(?Send)]
impl ReplyTransport for IssueReplyTransport {
    type Prepared = PreparedReply;

    async fn prepare(
        &self,
        artifact: &FrozenIssueReply,
    ) -> Result<PreparedReply, ReplyTransportError> {
        tokio::time::timeout(Duration::from_secs(30), async {
            self.connection
                .reopen_issue(&artifact.access.target)
                .await?
                .prepare_reply(artifact)
        })
        .await
        .map_err(|_| ReplyTransportError::Unavailable)?
        .map_err(map_error)
    }

    async fn send(
        &self,
        prepared: PreparedReply,
    ) -> Result<IssueReplyReceipt, ReplyTransportError> {
        tokio::time::timeout(Duration::from_secs(30), prepared.send())
            .await
            .map_err(|_| ReplyTransportError::Unavailable)?
            .map_err(map_error)
    }

    async fn reconcile(
        &self,
        key: &IssueReplyReceiptKey,
        page: NonZeroU32,
    ) -> Result<ReplyReconciliationPage, ReplyTransportError> {
        tokio::time::timeout(
            Duration::from_secs(30),
            self.connection.reconcile_reply_page(key, page),
        )
        .await
        .map_err(|_| ReplyTransportError::Unavailable)?
        .map_err(map_error)
    }
}

fn map_error(error: McpClientError) -> ReplyTransportError {
    match error {
        McpClientError::Unavailable => ReplyTransportError::Unavailable,
        McpClientError::InvalidConfiguration
        | McpClientError::InvalidCatalog
        | McpClientError::TargetChanged
        | McpClientError::ToolFailed => ReplyTransportError::InvalidBinding,
    }
}
