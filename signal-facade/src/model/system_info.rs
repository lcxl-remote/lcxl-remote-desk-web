use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// CPU information
#[derive(
    Serialize, Deserialize, Debug, Clone, ToSchema, wincode::SchemaWrite, wincode::SchemaRead,
)]
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
#[derive(
    Serialize,
    Deserialize,
    Debug,
    Clone,
    ToSchema,
    Default,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
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

#[cfg(test)]
mod wincode_tests {
    use super::*;
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    #[test]
    fn system_info_round_trips_wincode_with_cpu_list() {
        // Use multiple CPUs so a missed `CpuInfo` derive shows up here
        // rather than as a silently empty list.
        let original = SystemInfo {
            name: Some("alice-pc".to_string()),
            kernel_version: Some("10.0.26200".to_string()),
            os_version: Some("Windows 11 Pro".to_string()),
            host_name: Some("alice".to_string()),
            total_memory: 32 * 1024 * 1024 * 1024,
            used_memory: 16 * 1024 * 1024 * 1024,
            total_swap: 8 * 1024 * 1024 * 1024,
            used_swap: 1024 * 1024 * 1024,
            cpus: vec![
                CpuInfo {
                    name: "Core 0".to_string(),
                    frequency: 3600,
                    vendor_id: "GenuineIntel".to_string(),
                    brand: "Intel(R) Core(TM) i7-12700K".to_string(),
                    usage: 12.5,
                },
                CpuInfo {
                    name: "Core 1".to_string(),
                    frequency: 3600,
                    vendor_id: "GenuineIntel".to_string(),
                    brand: "Intel(R) Core(TM) i7-12700K".to_string(),
                    usage: 7.3,
                },
            ],
            is_admin: Some(true),
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: SystemInfo = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back.name.as_deref(), Some("alice-pc"));
        assert_eq!(back.total_memory, 32 * 1024 * 1024 * 1024);
        assert_eq!(back.is_admin, Some(true));
        assert_eq!(back.cpus.len(), 2);
        assert_eq!(back.cpus[0].name, "Core 0");
        assert!((back.cpus[0].usage - 12.5).abs() < f32::EPSILON);
        assert!((back.cpus[1].usage - 7.3).abs() < f32::EPSILON);
    }
}
