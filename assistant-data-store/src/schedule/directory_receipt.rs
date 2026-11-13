//! Verify the original directory proposal without granting or resolving a path.
use super::{ScheduleStoreError, session::ScheduleHostStores};
use desk_diagnose_core::{
    file_scope::transaction::{self, FileScopeMutation},
    session::PersistedAgentSession,
};
use sea_orm::ConnectionTrait;

/// The host reads and matches its stored receipt; the replay and proposal
/// checks are shared.
pub async fn verify_directory_receipt_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    session: &PersistedAgentSession,
    request_id: &str,
) -> Result<(), ScheduleStoreError> {
    let receipt = host.directory_receipt(txn, session, request_id).await?;
    transaction::replay(session, &receipt.update, &receipt)
        .map_err(|_| ScheduleStoreError::Conflict)?;
    let proposal = match &receipt.update.mutation {
        FileScopeMutation::Propose { proposal } | FileScopeMutation::Select { proposal } => {
            proposal
        }
        _ => return Err(ScheduleStoreError::Conflict),
    };
    if !session
        .file_scope
        .records()
        .iter()
        .any(|record| record.proposal == *proposal)
    {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(())
}
