//! Per-turn immutable binding; SQLite remains the original input authority.

use super::*;
use crate::agent_run_event_store::{InputSubject, ReadContextSelection, SignalAgentRunEventStore};
use desk_agent_protocol::data_lineage::DestinationIdentity;
use desk_diagnose_core::input_read_context::object_read::ObjectReadBinding;

pub(super) struct OriginalInput {
    revision: u64,
    selection: ReadContextSelection,
    destination: DestinationIdentity,
    client_conversation_id: Option<String>,
}

impl SignalDeviceAssistantTools {
    pub(crate) fn bind_original_input(
        &self,
        revision: u64,
        selection: ReadContextSelection,
        destination: DestinationIdentity,
        client_conversation_id: Option<String>,
    ) -> Result<(), AgentError> {
        selection.validate()?;
        self.original_input
            .set(OriginalInput {
                revision,
                selection,
                destination,
                client_conversation_id,
            })
            .map_err(|_| denied())
    }

    pub(super) fn object_binding(&self) -> Result<ObjectReadBinding<'_>, AgentError> {
        let input = self.original_input.get().ok_or_else(denied)?;
        Ok(ObjectReadBinding {
            original: &input.selection,
            destination: &input.destination,
            now_unix_ms: u64::try_from(chrono::Utc::now().timestamp_millis())
                .map_err(|_| denied())?,
        })
    }

    pub(crate) async fn validate_original_objects(&self) -> Result<(), AgentError> {
        let input = self.original_input.get().ok_or_else(denied)?;
        SignalAgentRunEventStore::new(self.db.clone())
            .validate_object_read(
                InputSubject {
                    run_id: &self.run_id,
                    actor_id: &self.actor_id,
                    device_id: &self.target_device_id,
                    client_conversation_id: input.client_conversation_id.as_deref(),
                },
                input.revision,
                &input.selection,
                &input.destination,
                u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| denied())?,
            )
            .await?;
        if !input.selection.live_targets.is_empty() {
            let readiness = crate::computer_use_readiness::global_computer_use_readiness_cache()
                .get_fresh(&self.target_connection_id, chrono::Utc::now())
                .ok_or_else(denied)?;
            desk_diagnose_core::input_read_context::live_read::validate_current(
                &input.selection,
                Some(&readiness.readiness),
                u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| denied())?,
            )?;
        }
        Ok(())
    }
}

fn denied() -> AgentError {
    error(
        AgentErrorKind::PermissionDenied,
        "original object input is unavailable",
        false,
        false,
    )
}
