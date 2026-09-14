//! Owner-only backup management, separate from the model capability/tool catalog.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

pub const MANAGEMENT_GRANT: &str = "file.recovery.manage";

#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileRecoveryCommand {
    Query {
        conversation_id: Option<String>,
        after: Option<String>,
    },
    SetPolicy {
        retention_days: u32,
        max_bytes: u64,
    },
    DeleteConversation {
        conversation_id: String,
    },
    RetryCleanup,
    ConfirmClock {
        displayed_time_unix_ms: u64,
        confirmed: bool,
    },
    Discard {
        recovery_id: String,
        conversation_id: String,
        confirmed: bool,
    },
    Export {
        recovery_id: String,
        conversation_id: String,
    },
}
impl FileRecoveryCommand {
    pub fn validate(&self) -> Result<(), &'static str> {
        fn identity(value: &str) -> bool {
            !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
        }
        fn record_id(value: &str) -> bool {
            value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
        let valid = match self {
            Self::Query {
                conversation_id,
                after,
            } => {
                conversation_id.as_deref().is_none_or(identity)
                    && after.as_deref().is_none_or(record_id)
            }
            Self::SetPolicy {
                retention_days,
                max_bytes,
            } => {
                (1..=3650).contains(retention_days)
                    && (1_048_576..=10_737_418_240).contains(max_bytes)
            }
            Self::DeleteConversation { conversation_id } => identity(conversation_id),
            Self::RetryCleanup => true,
            Self::ConfirmClock {
                displayed_time_unix_ms,
                confirmed,
            } => *confirmed && *displayed_time_unix_ms > 0,
            Self::Discard {
                recovery_id,
                conversation_id,
                confirmed,
            } => *confirmed && record_id(recovery_id) && identity(conversation_id),
            Self::Export {
                recovery_id,
                conversation_id,
            } => record_id(recovery_id) && identity(conversation_id),
        };
        if valid {
            Ok(())
        } else {
            Err("Invalid file recovery command fields")
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FileRecoveryRequest {
    /// Returned by the device; set on follow-ups to reject a changed connection domain.
    pub expected_authority: Option<String>,
    /// Returned by the worker; follow-ups must stay in the same OS user's vault.
    pub expected_os_user: Option<String>,
    pub command: FileRecoveryCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FileRecoveryPolicyDto {
    pub retention_days: u32,
    pub max_bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
pub struct FileRecoveryRecordDto {
    pub recovery_id: String,
    pub conversation_id: String,
    pub file_name: String,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub size_bytes: u64,
    pub change_state: String,
    pub material_state: String,
    pub cleanup_pending: bool,
    pub cleanup_reason: Option<String>,
    pub export_available: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
pub struct FileRecoveryPageDto {
    pub execution_epoch: u64,
    pub policy: FileRecoveryPolicyDto,
    pub used_bytes: u64,
    /// Included in used_bytes; not additional capacity consumption.
    pub reserved_bytes: u64,
    pub records: Vec<FileRecoveryRecordDto>,
    pub next_cursor: Option<String>,
    pub cleanup_warning: Option<FileRecoveryFailure>,
    pub clock_confirmation_time_unix_ms: Option<u64>,
    pub oldest_pending_at_unix_ms: Option<u64>,
    pub oldest_pending_record: Option<FileRecoveryRecordDto>,
}
#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
pub struct FileRecoveryCleanupDto {
    pub pending_files: u64,
    pub unknown_outcomes: u64,
}

#[derive(Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileRecoveryOutcome {
    Page { page: FileRecoveryPageDto },
    Policy { policy: FileRecoveryPolicyDto },
    Cleanup { report: FileRecoveryCleanupDto },
    Deleted { complete: bool },
    Export { zip_base64: String },
    Unavailable { reason: FileRecoveryFailure },
}
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum FileRecoveryFailure {
    Unauthorized,
    InvalidRequest,
    IdentityChanged,
    Busy,
    StorageUnavailable,
    WorkerUnavailable,
    Unsupported,
    MaterialUnavailable,
    MaterialExpired,
    MaterialCleaning,
    MaterialCleaned,
    ClockChanged,
}
#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
pub struct FileRecoveryReply {
    pub authority: String,
    pub os_user: String,
    pub outcome: FileRecoveryOutcome,
}

impl std::fmt::Debug for FileRecoveryOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Page { .. } => "FileRecoveryPage(<private>)",
            Self::Policy { .. } => "FileRecoveryPolicy",
            Self::Cleanup { .. } => "FileRecoveryCleanup",
            Self::Deleted { .. } => "FileRecoveryDeleted",
            Self::Export { .. } => "FileRecoveryExport(<private>)",
            Self::Unavailable { .. } => "FileRecoveryUnavailable",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn management_parameters_do_not_accept_actor_or_unbounded_selectors() {
        assert!(
            FileRecoveryCommand::ConfirmClock {
                displayed_time_unix_ms: 1000,
                confirmed: false
            }
            .validate()
            .is_err()
        );
        assert!(
            FileRecoveryCommand::ConfirmClock {
                displayed_time_unix_ms: 0,
                confirmed: true
            }
            .validate()
            .is_err()
        );
        assert!(
            FileRecoveryCommand::ConfirmClock {
                displayed_time_unix_ms: 1000,
                confirmed: true
            }
            .validate()
            .is_ok()
        );
        assert!(
            FileRecoveryCommand::Discard {
                recovery_id: "a".repeat(64),
                conversation_id: "conversation".into(),
                confirmed: false
            }
            .validate()
            .is_err()
        );
        assert!(
            FileRecoveryCommand::Discard {
                recovery_id: "a".repeat(64),
                conversation_id: "conversation".into(),
                confirmed: true
            }
            .validate()
            .is_ok()
        );
        assert!(serde_json::from_value::<FileRecoveryRequest>(serde_json::json!({
            "expected_authority": null, "actor_id": "another-owner", "command": {"operation": "retry_cleanup"}
        })).is_err());
        assert!(
            FileRecoveryCommand::Query {
                conversation_id: None,
                after: Some("../index".into())
            }
            .validate()
            .is_err()
        );
        assert!(
            FileRecoveryCommand::SetPolicy {
                retention_days: 0,
                max_bytes: 100_000_000
            }
            .validate()
            .is_err()
        );
        assert!(
            FileRecoveryCommand::Export {
                recovery_id: "a".repeat(64),
                conversation_id: String::new()
            }
            .validate()
            .is_err()
        );
        assert!(
            FileRecoveryCommand::Export {
                recovery_id: "a".repeat(64),
                conversation_id: "conversation".into()
            }
            .validate()
            .is_ok()
        );
    }
    #[test]
    fn debug_output_never_includes_backup_content() {
        let outcome = FileRecoveryOutcome::Export {
            zip_base64: "private-backup-bytes".into(),
        };
        assert!(!format!("{outcome:?}").contains("private-backup-bytes"));
    }
}
