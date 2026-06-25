use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::model::settings::StartupMode;

// Re-export shared types from signal-facade
pub use desk_signal_facade::model::system_info::{CpuInfo, SystemInfo as FacadeSystemInfo};

/// System information — extends the facade's SystemInfo with server-specific fields.
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
    pub startup_mode: StartupMode,
    /// Whether the system is running with administrative privileges
    pub is_admin: Option<bool>,
}

impl SystemInfo {
    /// Convert to the facade's shared SystemInfo (for signaling responses)
    pub fn to_facade(&self) -> FacadeSystemInfo {
        FacadeSystemInfo {
            name: self.name.clone(),
            kernel_version: self.kernel_version.clone(),
            os_version: self.os_version.clone(),
            host_name: self.host_name.clone(),
            total_memory: self.total_memory,
            used_memory: self.used_memory,
            total_swap: self.total_swap,
            used_swap: self.used_swap,
            cpus: self.cpus.clone(),
            is_admin: self.is_admin,
        }
    }
}

/// Background auto-start (macOS LaunchAgent) state. macOS-specific; `None` on
/// other platforms, which use the OS-service install path instead. This is
/// deliberately separate from `service_installed` (a Windows-service signal
/// consumed elsewhere in the console) so the two never alias.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, ToSchema)]
pub struct BackgroundStart {
    /// The LaunchAgent plist exists — the single source of truth for the macOS
    /// `auto_start` flag.
    pub configured: bool,
    /// launchd currently has the agent loaded in this GUI session. `false` right
    /// after enabling is normal: it takes effect at the next login/restart.
    pub loaded: bool,
    /// The executable the plist points at still exists on disk.
    pub path_valid: bool,
}

/// macOS TCC permission grants. macOS-specific; `None` on other platforms.
/// Two independent fields because screen capture and input injection each need
/// their own grant — they cannot be folded into a single privilege bool.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, ToSchema)]
pub struct MacosPermissions {
    /// Screen Recording grant (`CGPreflightScreenCaptureAccess`).
    pub screen_recording: bool,
    /// Accessibility / synthetic input grant (`AXIsProcessTrusted`).
    pub accessibility: bool,
}

/// macOS automatic-login helper state, surfaced to the settings page.
///
/// Automatic login is the unattended fallback on macOS (pre-login capture is
/// blocked by Apple). The app never handles the plaintext password — it only
/// reports read-only state and hands the user a guided deep link plus a
/// copy-paste command that prompts for the password interactively. `supported`
/// is `false` on every non-macOS platform (the whole struct is then inert), so
/// the wire shape stays identical across platforms.
#[derive(Serialize, Deserialize, Debug, Clone, ToSchema)]
pub struct MacosAutologin {
    /// Whether this platform is macOS (the only place the helper applies).
    pub supported: bool,
    /// FileVault is active. When true, macOS disables automatic login entirely
    /// and it cannot be bypassed — the UI must surface this and stop.
    pub filevault_enabled: bool,
    /// Automatic login is currently configured (a user is set).
    pub configured: bool,
    /// The user automatic login is set to, if any.
    pub autologin_user: Option<String>,
    /// Whether automatic login can be enabled right now
    /// (`supported && !filevault_enabled`). Purely a convenience for the UI.
    pub available: bool,
    /// Current login user, used to pre-fill the manual command. `$USER` of the
    /// resident app process.
    pub current_user: Option<String>,
    /// Copy-paste command that enables automatic login for `current_user`
    /// (placeholder `<user>` when unknown). Uses `-password -` so `sysadminctl`
    /// prompts for the password interactively; the app never sees it.
    pub enable_command: String,
    /// Copy-paste command that turns automatic login back off. Takes no password.
    pub disable_command: String,
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
    /// Whether the OS system service (LcxlDeskService) is installed
    pub service_installed: bool,
    /// Whether the current process has admin/root privileges
    pub is_admin: bool,
    /// Whether the server binary is available for service installation.
    /// True when lcxl-remote-desk-server(.exe) exists alongside the current
    /// executable (both binaries share the same target directory in dev and
    /// the same install directory in production).
    pub server_binary_available: bool,
    /// Default installation directory proposed to the user when installing the service.
    pub default_install_path: String,
    /// macOS background auto-start (LaunchAgent) state; `None` on non-macOS.
    pub background_start: Option<BackgroundStart>,
    /// macOS TCC permission grants; `None` on non-macOS.
    pub macos_permissions: Option<MacosPermissions>,
}

/// Runtime backend diagnostics
#[derive(Serialize, Deserialize, Debug, Clone, ToSchema)]
pub struct BackendInfo {
    /// Current target OS name
    pub os: String,
    /// Whether WAYLAND_DISPLAY is present
    pub wayland_env: bool,
    /// Whether DISPLAY is present
    pub x11_env: bool,
    /// Requested image capture from settings
    pub requested_image_capture: Option<String>,
    /// Resolved image capture backend used by factory
    pub resolved_image_capture: String,
    /// Resolved input control backend based on wayland_control_mode and environment
    pub resolved_input_control: String,
    /// Runtime status for input backend (ready/fallback/disabled)
    pub input_backend_runtime_status: String,
    /// Input backend error detail when not ready
    pub input_backend_error: Option<String>,
    /// Whether portal service is reachable
    pub portal_available: Option<bool>,
    /// Portal error detail when unavailable
    pub portal_error: Option<String>,
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
            startup_mode: StartupMode::Default,
            is_admin: None,
        }
    }
}
