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
            startup_mode: Some(self.startup_mode.clone()),
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
/// Screen capture, Accessibility automation, passive input observation, and
/// Apple Events automation for each iWork target use separate TCC decisions and
/// cannot be folded into one privilege bit.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, ToSchema)]
pub struct MacosPermissions {
    /// Screen Recording grant (`CGPreflightScreenCaptureAccess`).
    pub screen_recording: bool,
    /// Accessibility / synthetic input grant (`AXIsProcessTrusted`).
    pub accessibility: bool,
    /// Passive keyboard/pointer event observation grant
    /// (`CGPreflightListenEventAccess`).
    pub input_monitoring: bool,
    /// Apple Events Automation grant for Numbers. `false` also covers an app
    /// that is not currently running, because macOS cannot preflight that target.
    pub numbers_automation: bool,
    /// Apple Events Automation grant for Pages.
    pub pages_automation: bool,
    /// Apple Events Automation grant for Keynote.
    pub keynote_automation: bool,
}

/// Local Wayland Portal authorization target.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WaylandAuthorizationTarget {
    ScreenOnly,
    ScreenAndInput,
}

impl From<WaylandAuthorizationTarget> for desk_wayland_portal::AuthorizationTarget {
    fn from(value: WaylandAuthorizationTarget) -> Self {
        match value {
            WaylandAuthorizationTarget::ScreenOnly => Self::ScreenOnly,
            WaylandAuthorizationTarget::ScreenAndInput => Self::ScreenAndInput,
        }
    }
}

/// Non-sensitive readiness snapshot for the local host UI.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, ToSchema)]
pub struct WaylandPortalInfo {
    pub phase: String,
    pub screen_ready: bool,
    pub input_ready: bool,
    pub target: Option<WaylandAuthorizationTarget>,
    pub recommended_target: WaylandAuthorizationTarget,
    pub operation_id: Option<String>,
    pub generation: u64,
    pub persistent_restore: bool,
    pub requires_local_action: bool,
    /// Shared error code used by local control surfaces for localization.
    pub reason_code: Option<desk_utils::error::DeskErrorCode>,
    /// Diagnostic detail only. User interfaces must render `reason_code`
    /// through their localized domain mapping instead of displaying this text.
    pub reason: Option<String>,
}

impl WaylandPortalInfo {
    pub fn worker_unavailable() -> Self {
        Self {
            phase: "not_configured".into(),
            screen_ready: false,
            input_ready: false,
            target: None,
            recommended_target: WaylandAuthorizationTarget::ScreenAndInput,
            operation_id: None,
            generation: 0,
            persistent_restore: false,
            requires_local_action: true,
            reason_code: Some(desk_utils::error::DeskErrorCode::PRECONDITION_FAILED),
            reason: Some("No active desktop worker is available".into()),
        }
    }
}

impl From<desk_wayland_portal::PortalSnapshot> for WaylandPortalInfo {
    fn from(snapshot: desk_wayland_portal::PortalSnapshot) -> Self {
        let phase = match snapshot.phase {
            desk_wayland_portal::PortalPhase::Unsupported => "unsupported",
            desk_wayland_portal::PortalPhase::NotConfigured => "not_configured",
            desk_wayland_portal::PortalPhase::Restoring => "restoring",
            desk_wayland_portal::PortalPhase::Preparing => "preparing",
            desk_wayland_portal::PortalPhase::Ready => "ready",
            desk_wayland_portal::PortalPhase::NeedsAuthorization => "needs_authorization",
            desk_wayland_portal::PortalPhase::Failed => "failed",
        };
        let target = snapshot.target.map(|target| match target {
            desk_wayland_portal::AuthorizationTarget::ScreenOnly => {
                WaylandAuthorizationTarget::ScreenOnly
            }
            desk_wayland_portal::AuthorizationTarget::ScreenAndInput => {
                WaylandAuthorizationTarget::ScreenAndInput
            }
        });
        Self {
            phase: phase.into(),
            screen_ready: snapshot.capabilities.screen_ready,
            input_ready: snapshot.capabilities.input_ready,
            target,
            recommended_target: WaylandAuthorizationTarget::ScreenAndInput,
            operation_id: snapshot.operation_id,
            generation: snapshot.generation,
            persistent_restore: snapshot.restore_token_persisted,
            requires_local_action: snapshot.requires_local_action,
            reason_code: snapshot.reason_code,
            reason: snapshot.reason,
        }
    }
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
    /// Rust target operating-system name used for local host UI branching.
    pub platform: String,
    /// Startup mode of the server. Typed rather than free-form so the generated
    /// client gets the exact set of mode names to compare against.
    pub startup_mode: StartupMode,
    /// Current API version supported by the server
    pub api_version: i32,
    /// Indicates whether the system is initialized (e.g., admin password set)
    pub initialized: bool,
    /// Whether the OS system service (LcxlDeskService) is installed
    pub service_installed: bool,
    /// Whether the Windows service is currently running.
    pub service_running: bool,
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
    /// Wayland Portal readiness; `None` outside a Wayland desktop session.
    pub wayland_portal: Option<WaylandPortalInfo>,
    /// Optional control-end feature profile for the Device Assistant surface.
    /// Absence means unsupported so a newer client fails closed against an
    /// older server instead of inferring support from Provider inventory.
    pub device_assistant: Option<DeviceAssistantClientCapabilities>,
}

/// Server-side Device Assistant product features understood by a control end.
///
/// This profile advertises implementation support only. Authentication,
/// ownership, target readiness and grants are still checked for every request.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, ToSchema)]
pub struct DeviceAssistantClientCapabilities {
    pub schema_version: u16,
    pub turn_stream: bool,
    pub capability_inventory: bool,
    pub full_session_snapshot: bool,
    pub permission_decision: bool,
    pub grant_revoke: bool,
    pub background_task_cancel: bool,
    pub unknown_outcome_disposition: bool,
    pub object_context: bool,
}

impl DeviceAssistantClientCapabilities {
    pub const SCHEMA_VERSION: u16 = 1;

    pub const fn oss() -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            turn_stream: true,
            capability_inventory: true,
            full_session_snapshot: true,
            permission_decision: true,
            grant_revoke: true,
            background_task_cancel: true,
            unknown_outcome_disposition: true,
            object_context: true,
        }
    }
}

/// Runtime backend diagnostics.
#[derive(Serialize, Deserialize, Debug, Clone, ToSchema)]
pub struct BackendInfo {
    pub os: String,
    pub requested_image_capture: Option<String>,
    pub resolved_image_capture: String,
    pub resolved_input_control: String,
    pub input_backend_runtime_status: String,
    pub platform_diagnostics: Vec<BackendDiagnosticSection>,
}

#[derive(Serialize, Deserialize, Debug, Clone, ToSchema)]
pub struct BackendDiagnosticSection {
    pub platform: String,
    pub key: String,
    pub items: Vec<BackendDiagnosticItem>,
}

#[derive(Serialize, Deserialize, Debug, Clone, ToSchema)]
pub struct BackendDiagnosticItem {
    pub key: String,
    pub value: String,
    pub status: BackendDiagnosticStatus,
    pub detail: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackendDiagnosticStatus {
    Neutral,
    Ready,
    Warning,
    Error,
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

#[cfg(test)]
mod tests {
    use super::*;
    use desk_utils::error::DeskErrorCode;

    /// `startup_mode` carries a typed enum, but the JSON it produces is the
    /// same kebab-case name the field held when it was a free-form string.
    /// Clients compare against these exact spellings, so pin every one of them.
    #[test]
    fn startup_mode_serializes_as_the_kebab_case_mode_name() {
        for (mode, expected) in [
            (StartupMode::Default, "default"),
            (StartupMode::Signaling, "signaling"),
            (StartupMode::DeskServer, "desk-server"),
            (StartupMode::ServiceDaemon, "service-daemon"),
            (StartupMode::SessionWorker, "session-worker"),
            (StartupMode::McpStdio, "mcp-stdio"),
        ] {
            let info = ServerInfo {
                platform: "test".into(),
                startup_mode: mode.clone(),
                api_version: 1,
                initialized: true,
                service_installed: false,
                service_running: false,
                is_admin: false,
                server_binary_available: false,
                default_install_path: String::new(),
                background_start: None,
                macos_permissions: None,
                wayland_portal: None,
                device_assistant: Some(DeviceAssistantClientCapabilities::oss()),
            };
            assert_eq!(
                serde_json::to_value(&info).unwrap()["startup_mode"],
                serde_json::Value::String(expected.to_string()),
                "{mode:?} must stay on the wire as {expected}",
            );
            // The strum name the field used to be built from must agree, so no
            // client sees a spelling different from before the field was typed.
            assert_eq!(mode.as_ref(), expected);
        }
    }

    #[test]
    fn device_assistant_profile_is_explicit_and_complete_for_oss() {
        let value = serde_json::to_value(DeviceAssistantClientCapabilities::oss()).unwrap();
        assert_eq!(value["schema_version"], 1);
        for field in [
            "turn_stream",
            "capability_inventory",
            "full_session_snapshot",
            "permission_decision",
            "grant_revoke",
            "background_task_cancel",
            "unknown_outcome_disposition",
            "object_context",
        ] {
            assert_eq!(value[field], true, "OSS must advertise {field}");
        }
    }

    #[test]
    fn wayland_reason_code_is_numeric_and_diagnostic_detail_is_separate() {
        let info = WaylandPortalInfo::from(desk_wayland_portal::PortalSnapshot {
            phase: desk_wayland_portal::PortalPhase::NeedsAuthorization,
            capabilities: desk_wayland_portal::PortalCapabilities::default(),
            availability: desk_wayland_portal::PortalAvailability::default(),
            target: Some(desk_wayland_portal::AuthorizationTarget::ScreenAndInput),
            operation_id: None,
            generation: 4,
            restore_token_persisted: false,
            requires_local_action: true,
            reason_code: Some(DeskErrorCode::WAYLAND_PORTAL_INPUT_PERMISSION_REQUIRED),
            reason: Some("diagnostic-only backend detail".into()),
        });

        let json = serde_json::to_value(info).expect("serialize");
        assert!(json["reason_code"].is_i64());
        assert_eq!(
            json["reason_code"],
            serde_json::to_value(DeskErrorCode::WAYLAND_PORTAL_INPUT_PERMISSION_REQUIRED)
                .expect("serialize error code"),
            "DeskErrorCode must remain a bare integer on the REST wire"
        );
        assert_eq!(json["reason"], "diagnostic-only backend detail");
    }
}
