use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Log settings for the application.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(default)]
pub struct LogSettings {
    /// access logs are printed with the INFO level so ensure it is enabled by default
    pub log_level: String,
    /// Enable Rust backtrace for errors
    pub traceback: bool,
    /// Log retention days (default 7)
    pub log_retention_days: u32,
    /// Disk usage threshold for log cleanup (default 90%)
    pub log_cleanup_threshold_percent: u8,
    /// Interval in hours for the cleanup task (default 12)
    pub log_cleanup_interval_hours: u32,
    /// Enable tokio-console subscriber (requires `tokio_unstable` build flag). Default false.
    /// Each startup mode listens on a different port to avoid conflicts:
    /// Default/Signaling/DeskServer → 6669, ServiceDaemon → 6670, SessionWorker → 6671.
    pub tokio_console_enabled: bool,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self {
            log_level: "info".to_string(),
            traceback: true,
            log_retention_days: 7,
            log_cleanup_threshold_percent: 90,
            log_cleanup_interval_hours: 12,
            tokio_console_enabled: false,
        }
    }
}
