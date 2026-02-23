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

/// System information
#[derive(Serialize, Deserialize, Debug, Clone, ToSchema)]
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
    /// Startup mode
    pub startup_mode: String,
}

/// Server information
#[derive(Serialize, Deserialize, Debug, Clone, ToSchema)]
pub struct ServerInfo {
    /// Startup mode of the server
    pub startup_mode: String,
    /// Current API version supported by the server
    pub api_version: i32,
    /// Indicates whether the system is initialized (e.g., admin password set)
    pub initialized: bool,
}

impl From<&sysinfo::System> for SystemInfo {
    fn from(sys: &sysinfo::System) -> Self {
        let cpus = sys
            .cpus()
            .iter()
            .map(|cpu| CpuInfo {
                name: cpu.name().to_string(),
                frequency: cpu.frequency(),
                vendor_id: cpu.vendor_id().to_string(),
                brand: cpu.brand().to_string(),
                usage: cpu.cpu_usage(),
            })
            .collect();

        SystemInfo {
            name: sysinfo::System::name(),
            kernel_version: sysinfo::System::kernel_version(),
            os_version: sysinfo::System::os_version(),
            host_name: sysinfo::System::host_name(),
            total_memory: sys.total_memory(),
            used_memory: sys.used_memory(),
            total_swap: sys.total_swap(),
            used_swap: sys.used_swap(),
            cpus,
            startup_mode: "Default".to_string(),
        }
    }
}
