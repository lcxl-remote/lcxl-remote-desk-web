//! Original use records for retaining accepted owner-visible text. Callers
//! authenticate the accepted native output and current copy controls separately.

use super::*;
use desk_diagnose_core::ai_assistant::{
    output_archive::ArchivedAssistantOutput, retained_copy::OwnerHistoryReadValidation,
};

pub(crate) async fn load_owner_history_set_on<C: ConnectionTrait>(
    db: &C,
    archive: &ArchivedAssistantOutput,
) -> Result<Vec<OwnerHistoryReadValidation>, AssistantStoreError> {
    let consumed = load_consumed_metadata_on(db, &archive.source_set).await?;
    let mut grants = Vec::with_capacity(consumed.len());
    let mut reads = Vec::with_capacity(consumed.len());
    for value in consumed {
        reads.push(
            OwnerHistoryReadValidation::for_consumed_projection(
                archive,
                &value.authorization.proof,
                &value.projection,
                value.first_consumed_at_unix_ms,
                value.last_consumed_at_unix_ms,
            )
            .map_err(AssistantStoreError::Invalid)?,
        );
        grants.push(*value.authorization.proof);
    }
    archive
        .validate_grants(&grants)
        .map_err(AssistantStoreError::Invalid)?;
    Ok(reads)
}
