//! Personal-assistant retrieval delegates to the original native call gates.
//! A port advertises host support; it never substitutes for source authorization.

use super::*;
use desk_agent_protocol::ai_assistant::history::{
    ConversationReadRequest, ConversationReadResult, ConversationSearchRequest,
    ConversationSearchResult,
};
use desk_diagnose_core::ai_assistant::seam::{
    AssistantDataCall, AssistantDataRead, ConversationSearchSeam,
};

#[async_trait(?Send)]
impl ConversationSearchSeam for SignalAgentSessionStore {
    async fn search_conversations(
        &self,
        context: AssistantDataCall<'_>,
        request: &ConversationSearchRequest,
    ) -> Result<AssistantDataRead<ConversationSearchResult>, AgentError> {
        desk_diagnose_core::future::boxed(|| {
            crate::assistant_data_store::search_conversations(&self.db, context, request)
        })
        .await
        .map_err(crate::assistant_data_store::AssistantStoreError::into_agent_error)
    }

    async fn read_conversation(
        &self,
        context: AssistantDataCall<'_>,
        request: &ConversationReadRequest,
    ) -> Result<AssistantDataRead<ConversationReadResult>, AgentError> {
        desk_diagnose_core::future::boxed(|| {
            crate::assistant_data_store::read_conversation(&self.db, context, request)
        })
        .await
        .map_err(crate::assistant_data_store::AssistantStoreError::into_agent_error)
    }
}
