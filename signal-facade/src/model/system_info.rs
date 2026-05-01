use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// CPU information
#[derive(Serialize, Deserialize, Debug, Clone, ToSchema)]
pub struct CpuInfo {
    /// CPU name
    pub name: String,
    /// CPU frequency in MHz
    pub frequency: u64,
    /// CPU vendor ID
    pub vendor_id: String,
    /// CPU brand
    pub brand: String,
    /// CPU usage percentage
    pub usage: f32,
}

/// System information (shared between signal-facade consumers).
/// Used for remote system info queries via signaling.
#[derive(Serialize, Deserialize, Debug, Clone, ToSchema, Default)]
pub struct SystemInfo {
    /// System name
    pub name: Option<String>,
    /// System kernel version
    pub kernel_version: Option<String>,
    /// Operating system version
    pub os_version: Option<String>,
    /// Host name
    pub host_name: Option<String>,
    /// Total memory in bytes
    pub total_memory: u64,
    /// Used memory in bytes
    pub used_memory: u64,
    /// Total swap in bytes
    pub total_swap: u64,
    /// Used swap in bytes
    pub used_swap: u64,
    /// List of CPU information
    pub cpus: Vec<CpuInfo>,
    /// Whether the system is running with administrative privileges
    pub is_admin: Option<bool>,
}
