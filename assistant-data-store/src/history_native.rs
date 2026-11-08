//! Exact native metadata verification shared by owner and model history reads.

use super::{AssistantStoreError, unsigned};
use crate::entity::agent_assistant_history as history;
use desk_agent_protocol::ai_assistant::source::AssistantSourceKind;
use desk_diagnose_core::ai_assistant::native_history::{
    NativeHistoryMetadata, NativeHistoryTimeColumns, read_metadata,
};

pub fn read(row: &history::Model) -> Result<Option<NativeHistoryMetadata>, AssistantStoreError> {
    let kind = match row.kind.as_str() {
        "input" => AssistantSourceKind::Input,
        "assistant_message" => AssistantSourceKind::AssistantMessage,
        "receipt" => AssistantSourceKind::Receipt,
        "native_status" => AssistantSourceKind::NativeStatus,
        _ => return Err(AssistantStoreError::Inaccessible),
    };
    let columns = NativeHistoryTimeColumns {
        at_unix_ms: row.native_action_at_ms.map(unsigned).transpose()?,
        from_unix_ms: row.native_action_from_ms.map(unsigned).transpose()?,
        until_unix_ms: row.native_action_until_ms.map(unsigned).transpose()?,
        basis: row.time_basis.clone(),
    };
    read_metadata(
        kind,
        unsigned(row.recorded_at_ms)?,
        row.state_json.as_deref(),
        &columns,
    )
    .map_err(|_| AssistantStoreError::Inaccessible)
}

pub fn zero_input_allowed(row: &history::Model, native: Option<&NativeHistoryMetadata>) -> bool {
    let kind = match row.kind.as_str() {
        "input" => AssistantSourceKind::Input,
        "assistant_message" => AssistantSourceKind::AssistantMessage,
        "receipt" => AssistantSourceKind::Receipt,
        "native_status" => AssistantSourceKind::NativeStatus,
        _ => return false,
    };
    desk_diagnose_core::ai_assistant::source::zero_owner_input_allowed(
        kind,
        &row.role,
        &row.conversation_id,
        &row.root_conversation_id,
        native,
    )
}
