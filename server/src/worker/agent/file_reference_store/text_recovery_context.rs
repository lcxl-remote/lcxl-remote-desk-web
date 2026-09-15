//! Trusted recovery binding shared by native text executors.
use super::*;
pub(crate) struct RecoveryContext {
    pub data_root: PathBuf,
    pub execution_epoch: u64,
    pub quota: Option<crate::worker::session::QuotaClient>,
    pub scope: desk_file_recovery::Scope,
    pub conversation_id: String,
    pub operation_id: String,
    pub generation: String,
    #[cfg(test)]
    pub(crate) _test_data: Option<std::sync::Arc<tempfile::TempDir>>,
}
