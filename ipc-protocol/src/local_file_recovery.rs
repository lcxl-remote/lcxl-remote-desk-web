//! Local OS-user recovery is deliberately separate from remote scoped commands.
//! Only a host-authenticated local user may access backups across authorities.
use desk_agent_protocol::file_recovery::{
    FileRecoveryCleanupDto, FileRecoveryCommand, FileRecoveryFailure, FileRecoveryPageDto,
};
use serde::{Deserialize, Serialize};
use wincode::{SchemaRead, SchemaWrite};

#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite)]
pub struct LocalFileRecoveryRequest {
    pub request_id: String,
    pub os_user: String,
    pub session_id: u32,
    pub deadline_unix_ms: u64,
    pub command: LocalFileRecoveryCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite)]
pub struct LocalFileRecoveryReply {
    pub request_id: String,
    pub outcome: Result<LocalFileRecoveryOutcome, FileRecoveryFailure>,
}

impl std::fmt::Debug for LocalFileRecoveryCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalFileRecoveryCommand(<private>)")
    }
}
impl std::fmt::Debug for LocalFileRecoveryOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalFileRecoveryOutcome(<private>)")
    }
}

#[derive(Clone, Serialize, Deserialize, SchemaRead, SchemaWrite)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalFileRecoveryCommand {
    Query {
        after: Option<String>,
    },
    RetryCleanup,
    Discard {
        recovery_id: String,
        conversation_id: String,
        confirmed: bool,
    },
    ConfirmClock {
        displayed_time_unix_ms: u64,
        confirmed: bool,
    },
    Export {
        recovery_id: String,
    },
}

impl LocalFileRecoveryCommand {
    pub fn validate(&self) -> Result<(), &'static str> {
        // Reuse the existing ID, cursor and explicit-confirmation contract.
        // Export is the sole local operation without a conversation scope.
        let equivalent = match self {
            Self::Query { after } => FileRecoveryCommand::Query {
                conversation_id: None,
                after: after.clone(),
            },
            Self::RetryCleanup => FileRecoveryCommand::RetryCleanup,
            Self::Discard {
                recovery_id,
                conversation_id,
                confirmed,
            } => FileRecoveryCommand::Discard {
                recovery_id: recovery_id.clone(),
                conversation_id: conversation_id.clone(),
                confirmed: *confirmed,
            },
            Self::ConfirmClock {
                displayed_time_unix_ms,
                confirmed,
            } => FileRecoveryCommand::ConfirmClock {
                displayed_time_unix_ms: *displayed_time_unix_ms,
                confirmed: *confirmed,
            },
            Self::Export { recovery_id } => {
                // A cursor uses the same record ID format without requiring a
                // conversation that local historical export does not have.
                FileRecoveryCommand::Query {
                    conversation_id: None,
                    after: Some(recovery_id.clone()),
                }
            }
        };
        equivalent.validate()
    }
}

#[derive(Clone, Serialize, Deserialize, SchemaRead, SchemaWrite)]
pub enum LocalFileRecoveryOutcome {
    Page(FileRecoveryPageDto),
    Cleanup(FileRecoveryCleanupDto),
    Export(Vec<u8>),
}

impl LocalFileRecoveryOutcome {
    pub fn into_page(self) -> Result<FileRecoveryPageDto, &'static str> {
        match self {
            Self::Page(page) => Ok(page),
            _ => Err("Unexpected local recovery response"),
        }
    }
    pub fn into_cleanup(self) -> Result<FileRecoveryCleanupDto, &'static str> {
        match self {
            Self::Cleanup(report) => Ok(report),
            _ => Err("Unexpected local recovery response"),
        }
    }
    pub fn into_export(self) -> Result<Vec<u8>, &'static str> {
        match self {
            Self::Export(bytes) => Ok(bytes),
            _ => Err("Unexpected local recovery response"),
        }
    }
}
