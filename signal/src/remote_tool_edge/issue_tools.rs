//! Host-owned issue reads using the current native input and saved connection.

use super::*;
use desk_diagnose_core::{
    external_tools, input_read_context::ReadContextSelection, session::PersistedAgentSession,
};

impl SignalAiAssistantTools {
    async fn checked_issue_input(
        &self,
        session: &PersistedAgentSession,
        call_id: Option<&str>,
    ) -> Result<ReadContextSelection, AgentError> {
        let original = self.original_input.get().ok_or_else(issue_unavailable)?;
        if self.run_id != session.conversation_id
            || self.actor_id != session.actor_id
            || original.revision != session.input_revision
        {
            return Err(issue_unavailable());
        }
        let original = crate::agent_run_event_store::SignalAgentRunEventStore::new(self.db.clone())
            .checked_external_read_input(session, &original.destination, call_id)
            .await?;
        if !desk_assistant_data_store::issue_authority::input_sources_current(
            &self.db,
            session,
            &original,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .map_err(|error| error.into_agent_error())?
        {
            return Err(issue_unavailable());
        }
        Ok(original)
    }

    pub(super) async fn current_issue_access(
        &self,
        session: &PersistedAgentSession,
    ) -> Result<Vec<external_tools::IssueAccess>, AgentError> {
        if self
            .original_input
            .get()
            .is_none_or(|input| input.selection.external_issues.is_empty())
        {
            return Ok(Vec::new());
        }
        let original = self.checked_issue_input(session, None).await?;
        let config = crate::issue_connections::read(&self.db)
            .await
            .map_err(|_| issue_unavailable())?;
        Ok(original
            .external_issues
            .into_iter()
            .filter(|issue| {
                config
                    .connection_for_issue(&session.actor_id, issue)
                    .is_some()
            })
            .collect())
    }

    pub(super) async fn read_external_issue(
        &self,
        session: &PersistedAgentSession,
        call: &ToolCall,
    ) -> Result<ReadOutcome, AgentError> {
        let original = self.checked_issue_input(session, Some(&call.id)).await?;
        let (access, method) = external_tools::resolve(&original.external_issues, call)?;
        let config = crate::issue_connections::read(&self.db)
            .await
            .map_err(|_| issue_unavailable())?;
        let connection = config
            .connection_for_issue(&session.actor_id, access)
            .cloned()
            .ok_or_else(issue_unavailable)?;
        let saved_connection = connection.clone();
        let client = tokio::task::spawn_blocking(move || connection.client())
            .await
            .map_err(|_| issue_unavailable())?
            .map_err(|_| issue_unavailable())?;
        let before_dispatch = self.checked_issue_input(session, Some(&call.id)).await?;
        let live = crate::issue_connections::read(&self.db)
            .await
            .map_err(|_| issue_unavailable())?;
        if before_dispatch != original
            || live.connection_for_issue(&session.actor_id, access) != Some(&saved_connection)
        {
            return Err(issue_unavailable());
        }
        let snapshot = desk_assistant_data_store::issue_authority::resolve_input(
            &self.db,
            session,
            &original,
            access,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .map_err(|error| error.into_agent_error())?;
        let result = if let Some((snapshot, _)) = snapshot {
            let remote = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                client.reopen_issue(&access.target),
            )
            .await
            .map_err(|_| issue_unavailable())?
            .map_err(|error| {
                super::error(
                    AgentErrorKind::TransportError,
                    error.to_string(),
                    false,
                    true,
                )
            })?;
            external_tools::IssueReadOutput {
                content: snapshot.workset.read(method),
                tool_signature: remote.read_signature().into(),
            }
        } else if original
            .external_issue_sources
            .iter()
            .any(|source| source.tool_ref == access.tool_ref())
        {
            return Err(issue_unavailable());
        } else {
            client
                .read_issue(&access.target, method)
                .await
                .map_err(|error| {
                    super::error(
                        AgentErrorKind::TransportError,
                        error.to_string(),
                        false,
                        true,
                    )
                })?
        };
        let current = self.checked_issue_input(session, Some(&call.id)).await?;
        let config = crate::issue_connections::read(&self.db)
            .await
            .map_err(|_| issue_unavailable())?;
        if current != original
            || config.connection_for_issue(&session.actor_id, access) != Some(&saved_connection)
        {
            return Err(issue_unavailable());
        }
        let input = self.original_input.get().ok_or_else(issue_unavailable)?;
        let label = external_tools::label_issue_read(
            session,
            &original,
            &input.destination,
            call,
            &result.tool_signature,
            &result.content,
        )?;
        Ok(ReadOutcome::Completed {
            output: ToolRunOutput {
                format: desk_diagnose_core::seam::ToolOutputFormat::Text,
                content: result.content,
                image_data_url: None,
                document_preview: None,
            },
            ok: true,
            event_id: None,
            data_envelope: Some(label),
            background_task: None,
        })
    }
    pub(super) async fn current_reply_access(
        &self,
        session: &PersistedAgentSession,
    ) -> Result<Vec<external_tools::IssueAccess>, AgentError> {
        if self
            .original_read_context()
            .is_none_or(|original| original.external_issue_sources.is_empty())
        {
            return Ok(vec![]);
        }
        let db = &self.db;
        let original = self.checked_issue_input(session, None).await?;
        let config = crate::issue_connections::read(db)
            .await
            .map_err(|_| issue_unavailable())?;
        let now = chrono::Utc::now().timestamp_millis();
        let mut allowed = Vec::new();
        for issue in &original.external_issues {
            if config
                .connection_for_issue(&session.actor_id, issue)
                .is_some()
                && desk_assistant_data_store::issue_authority::resolve_input(
                    db, session, &original, issue, now,
                )
                .await
                .map_err(|error| error.into_agent_error())?
                .is_some_and(|(snapshot, current)| snapshot.can_publish(&current, now))
            {
                allowed.push(issue.clone());
            }
        }
        Ok(allowed)
    }

    pub(super) async fn prepare_external_reply(
        &self,
        session: &PersistedAgentSession,
        call: &ToolCall,
    ) -> Result<desk_diagnose_core::issue_reply::ReplyGeneration, AgentError> {
        use desk_assistant_data_store::issue_reply::preparation::{self, Preparation};
        let db = &self.db;
        let destination = &self
            .original_input
            .get()
            .ok_or_else(issue_unavailable)?
            .destination;
        let original = self.checked_issue_input(session, Some(&call.id)).await?;
        let prepared = preparation::prepare_on(
            db,
            session,
            &original,
            call,
            destination,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .map_err(|error| error.into_agent_error())?;
        let Preparation::Generate(work) = &prepared else {
            return Ok(prepared.into_generation(String::new()));
        };
        let config = crate::issue_connections::read(db)
            .await
            .map_err(|_| issue_unavailable())?;
        let connection = config
            .connection_for_issue(&session.actor_id, &work.access)
            .cloned()
            .ok_or_else(issue_unavailable)?;
        let saved = connection.clone();
        let client = tokio::task::spawn_blocking(move || connection.client())
            .await
            .map_err(|_| issue_unavailable())?
            .map_err(|_| issue_unavailable())?;
        let signature = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            client
                .reopen_issue(&work.access.target)
                .await?
                .comment_signature()
                .map(str::to_owned)
                .ok_or(desk_signal_facade::mcp_client::McpClientError::InvalidCatalog)
        })
        .await
        .map_err(|_| issue_unavailable())?
        .map_err(|_| issue_unavailable())?;
        let current = self.checked_issue_input(session, Some(&call.id)).await?;
        let live = crate::issue_connections::read(db)
            .await
            .map_err(|_| issue_unavailable())?;
        if current != original
            || live.connection_for_issue(&session.actor_id, &work.access) != Some(&saved)
            || preparation::prepare_on(
                db,
                session,
                &current,
                call,
                destination,
                chrono::Utc::now().timestamp_millis(),
            )
            .await
            .map_err(|error| error.into_agent_error())?
                != prepared
        {
            return Err(issue_unavailable());
        }
        Ok(prepared.into_generation(signature))
    }

    pub(super) async fn freeze_external_reply(
        &self,
        session: &PersistedAgentSession,
        publication: desk_diagnose_core::issue_reply::ReplyPublication,
        turn: desk_diagnose_core::chat::ModelTurn,
    ) -> Result<desk_diagnose_core::issue_reply::ReplyToolReceipt, AgentError> {
        let row = crate::issue_replies::freeze(
            &self.db,
            desk_assistant_data_store::issue_reply::ReplyPreparation {
                held: session,
                origin: publication.origin,
                access: publication.access,
                body: turn.text,
                output: turn
                    .provider_meta
                    .data_envelope
                    .ok_or_else(issue_unavailable)?,
                comment_tool_signature: publication.comment_tool_signature,
                random_marker: uuid::Uuid::new_v4().into_bytes(),
            },
        )
        .await?;
        Ok(desk_assistant_data_store::issue_reply::preparation::tool_receipt(&row))
    }
}

fn issue_unavailable() -> AgentError {
    error(
        AgentErrorKind::PermissionDenied,
        "The original issue or its current connection access is unavailable",
        false,
        true,
    )
}
