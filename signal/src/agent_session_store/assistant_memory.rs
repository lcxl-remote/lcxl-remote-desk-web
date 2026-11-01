//! Memory reads and mutations retain their original native gates and projections.

use super::*;
use crate::assistant_data_store as store;
use desk_agent_protocol::{
    ai_assistant::{
        memory::{
            AssistantDataControlReceipt, MemoryHistoryRequest, MemoryHistoryResult,
            MemorySearchRequest, MemorySearchResult, MemoryWriteRequest, MemoryWriteResult,
        },
        source::AssistantSourceResolution,
    },
    data_lineage::DestinationIdentity,
};
use desk_diagnose_core::ai_assistant::seam::{
    AssistantDataCall, AssistantDataRead, CurrentMemoryBlock, MemoryMutationSeam, MemorySeam,
};

#[async_trait(?Send)]
impl MemorySeam for SignalAgentSessionStore {
    async fn current_memories(
        &self,
        session: &PersistedAgentSession,
        destination: &DestinationIdentity,
        preparation_id: &str,
        maximum_bytes: usize,
        _now_unix_ms: u64,
    ) -> Result<CurrentMemoryBlock, AgentError> {
        desk_diagnose_core::future::boxed(|| {
            store::prepare_current_memory(
                &self.db,
                session,
                destination,
                preparation_id,
                maximum_bytes,
            )
        })
        .await
        .map_err(store::AssistantStoreError::into_agent_error)
    }

    async fn search_memories(
        &self,
        context: AssistantDataCall<'_>,
        request: &MemorySearchRequest,
    ) -> Result<AssistantDataRead<MemorySearchResult>, AgentError> {
        desk_diagnose_core::future::boxed(|| store::search_memories(&self.db, context, request))
            .await
            .map_err(store::AssistantStoreError::into_agent_error)
    }

    async fn read_memory_history(
        &self,
        context: AssistantDataCall<'_>,
        request: &MemoryHistoryRequest,
    ) -> Result<AssistantDataRead<MemoryHistoryResult>, AgentError> {
        desk_diagnose_core::future::send_phase(|| {
            store::read_memory_history(&self.db, context, request)
        })
        .await
        .map_err(store::AssistantStoreError::into_agent_error)
    }

    async fn read_memory_source(
        &self,
        context: AssistantDataCall<'_>,
        source_id: &str,
    ) -> Result<AssistantDataRead<AssistantSourceResolution>, AgentError> {
        desk_diagnose_core::future::boxed(|| {
            store::read_memory_source(&self.db, context, source_id)
        })
        .await
        .map_err(store::AssistantStoreError::into_agent_error)
    }
}

#[async_trait(?Send)]
impl MemoryMutationSeam for SignalAgentSessionStore {
    async fn manage_memory(
        &self,
        context: AssistantDataCall<'_>,
        request: &MemoryWriteRequest,
    ) -> Result<AssistantDataRead<MemoryWriteResult>, AgentError> {
        desk_diagnose_core::future::send_phase(|| store::write_memory(&self.db, context, request))
            .await
            .map_err(store::AssistantStoreError::into_agent_error)
    }

    async fn forget_group(
        &self,
        session: &mut PersistedAgentSession,
        call: &desk_diagnose_core::ai_assistant::seam::MemoryForgetCall<'_>,
    ) -> Result<AssistantDataControlReceipt, AgentError> {
        let (receipt, closed) = desk_diagnose_core::future::boxed(|| {
            store::forget_native_memory(&self.db, session, call)
        })
        .await
        .map_err(store::AssistantStoreError::into_agent_error)?;
        if let Some(closed) = closed {
            *session = *closed;
        }
        Ok(receipt)
    }
}
