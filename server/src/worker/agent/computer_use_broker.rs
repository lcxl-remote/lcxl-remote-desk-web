//! Worker-lifetime Computer Use observation broker.
//!
//! The broker owns the interactive-session incarnation and opaque ObjectRef
//! store. Restarting a worker constructs a new broker, immediately invalidating
//! every prior reference. Typed actions re-resolve those references and run
//! only behind the writer lease, local ceiling, and exact-grant dispatch path.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use chrono::{DateTime, Duration, Utc};
use desk_agent_protocol::browser_control::{
    BrowserActionRequest, BrowserActionResult, BrowserAdapterRef, BrowserEngineKind,
    BrowserReadiness, BrowserReadinessReason,
};
use desk_agent_protocol::communication::{
    CommunicationDraftHandoff, OutlookNewComposeHandoffRequest,
};
#[cfg(target_os = "macos")]
use desk_agent_protocol::computer_use::{
    BatchDocumentArtifact, BatchDocumentSourceProjection, ComputerActionKind,
    LiveDocumentInspectOutput, LiveDocumentInspectParams, LiveDocumentProjection,
};
use desk_agent_protocol::computer_use::{
    COMPUTER_USE_SCHEMA_VERSION, ComputerActionOutput, ComputerUseAdapterKind,
    ComputerUseAdapterRef, ComputerUseCapabilityReadiness, ComputerUseContextReference,
    ComputerUseReadiness, ComputerUseReadinessReason, DesktopSessionInspectOutput,
    DesktopSessionInspectParams, MAX_COMPUTER_USE_INSPECT_BYTES, MAX_COMPUTER_USE_INSPECT_NODES,
    ObjectKind, ObjectRef, OfficeInspectParams, RawInputAction, UiInspectOutput, UiInspectParams,
    UiNodeProjection, UiSemanticAction,
};
#[cfg(any(windows, target_os = "macos"))]
use desk_agent_protocol::computer_use::{OfficeInspectOutput, OfficeSelectionProjection};
use desk_agent_protocol::{AgentError, AgentErrorKind, Capability, ScreenCaptureParams};
#[cfg(target_os = "macos")]
use desk_diagnose_core::device_assistant::MACOS_ACCESSIBILITY_ADAPTER_ID;
#[cfg(not(target_os = "macos"))]
use desk_diagnose_core::device_assistant::WINDOWS_UIA_ADAPTER_ID;
use desk_diagnose_core::device_assistant::{
    CURRENT_SCREEN_ADAPTER_ID, DESKTOP_SESSION_ADAPTER_ID, FILE_ARTIFACT_ADAPTER_ID,
    FILE_WORKSPACE_ADAPTER_ID, IWORK_ADAPTER_VERSION, OFFICE_EXCEL_ADAPTER_ID,
    OUTLOOK_NEW_MAILTO_ADAPTER_VERSION, SPREADSHEET_FILE_ADAPTER_ID, SYSTEM_COMMAND_ADAPTER_ID,
    SYSTEM_DIAGNOSTICS_ADAPTER_ID, TERMINAL_OUTPUT_ADAPTER_ID, WINDOWS_RAW_INPUT_ADAPTER_ID,
    device_assistant_edge_adapter_registry,
};

use crate::model::settings::ComputerUseSettings;

use super::browser_devtools_mcp::{
    BrowserBrokerContext, BrowserDevtoolsBroker, ChromeDevtoolsMcpError,
};
use super::browser_extension_bridge::{BrowserExtensionBridgeError, BrowserExtensionBroker};
use super::computer_use_writer::{
    InputPreemptionSource, WriterLeaseCoordinator, WriterLeaseRequest, WriterLeaseState,
};

// A readiness report can be almost 25 seconds old when the signal starts a
// turn, and the model adapter permits up to 180 seconds for the first response
// containing a tool call. Keep the reference alive across both windows. It is
// still invalidated immediately by local/browser input, worker restart, or an
// Office document identity mismatch.
const OBJECT_REF_TTL_SECS: i64 = 300;
const MAX_UI_INSPECT_DEPTH: u16 = 16;
const MAX_OBJECT_REFS: usize = 8_192;
const SCREEN_CAPTURE_MIN_INTERVAL: StdDuration = StdDuration::from_secs(2);

fn screen_capture_readiness(
    observation_enabled: bool,
    platform_supported: bool,
    session_ready: bool,
    session_reason: Option<ComputerUseReadinessReason>,
    allow_screen: bool,
    display_selected: bool,
) -> (bool, Option<ComputerUseReadinessReason>) {
    let ready = session_ready && allow_screen && display_selected;
    let reason = (!ready).then_some(if !observation_enabled || !allow_screen {
        ComputerUseReadinessReason::DisabledByLocalCeiling
    } else if !platform_supported {
        ComputerUseReadinessReason::UnsupportedPlatform
    } else if !display_selected {
        ComputerUseReadinessReason::NoDisplaySelected
    } else {
        session_reason.unwrap_or(ComputerUseReadinessReason::NoInteractiveSession)
    });
    (ready, reason)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResolvedObject {
    DesktopSession {
        session_id: u32,
    },
    Application {
        window_handle: isize,
        process_id: u32,
        image_path: String,
        process_started_at: Option<u64>,
    },
    UiElement {
        process_id: u32,
        image_path: String,
        fingerprint: String,
    },
    OfficeDocument {
        document_url_hash: String,
    },
    Worksheet {
        document_url_hash: String,
        name: String,
    },
    Range {
        document_url_hash: String,
        address: String,
    },
    IworkNumbersCell {
        document_identity_sha256: String,
        sheet_name: String,
        table_name: String,
        cell_address: String,
        before_sha256: String,
    },
    IworkPagesDocument {
        document_identity_sha256: String,
        before_sha256: String,
    },
    IworkKeynoteSlide {
        document_identity_sha256: String,
        slide_number: i64,
        title_before_sha256: String,
        notes_before_sha256: String,
    },
    IworkNumbersBatch {
        source_file: ObjectRef,
        source_sha256: String,
        source_byte_len: u64,
        document_identity_sha256: String,
        sheet_name: String,
        table_name: String,
        cell_address: String,
        before_sha256: String,
    },
    IworkPagesBatch {
        source_file: ObjectRef,
        source_sha256: String,
        source_byte_len: u64,
        document_identity_sha256: String,
        before_sha256: String,
    },
    IworkKeynoteBatch {
        source_file: ObjectRef,
        source_sha256: String,
        source_byte_len: u64,
        document_identity_sha256: String,
        slide_number: i64,
        title_before_sha256: String,
        notes_before_sha256: String,
    },
}

#[derive(Clone)]
struct StoredObject {
    snapshot_id: String,
    object_kind: ObjectKind,
    expires_at: DateTime<Utc>,
    incarnation: String,
    resolved: ResolvedObject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadinessFingerprint {
    server_api_version: i32,
    os: String,
    interactive_session_incarnation: String,
    local_ceiling_revision: u64,
    capabilities: Vec<ComputerUseCapabilityReadiness>,
    context_capabilities: Vec<Capability>,
    browser_adapter: Option<BrowserAdapterRef>,
}

#[derive(Debug, Clone)]
struct ReadinessRevisionState {
    fingerprint: ReadinessFingerprint,
    revision: u64,
}

#[derive(Default)]
struct ScreenCaptureGateState {
    in_flight: bool,
    last_started: Option<StdInstant>,
}

pub struct ComputerUseBroker {
    incarnation_nonce: String,
    worker_generation: AtomicU64,
    snapshot_counter: AtomicU64,
    readiness_revision: AtomicU64,
    readiness_revision_state: Mutex<Option<ReadinessRevisionState>>,
    active_session_incarnation: Mutex<Option<String>>,
    screen_capture_gate: Mutex<ScreenCaptureGateState>,
    human_input_epoch: AtomicU64,
    input_ownership_ready: AtomicBool,
    objects: Mutex<HashMap<String, StoredObject>>,
    writer_lease: WriterLeaseCoordinator,
    browser_extension: Arc<BrowserExtensionBroker>,
    browser_devtools: BrowserDevtoolsBroker,
}

pub(crate) struct SemanticActionResult {
    pub(crate) changed: bool,
    pub(crate) verified: bool,
    pub(crate) summary: String,
    pub(crate) output: Option<ComputerActionOutput>,
}

#[derive(Debug)]
pub(crate) enum BrowserProviderError {
    Extension(BrowserExtensionBridgeError),
    Devtools(ChromeDevtoolsMcpError),
}

impl std::fmt::Display for BrowserProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Extension(error) => error.fmt(formatter),
            Self::Devtools(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for BrowserProviderError {}

impl Default for ComputerUseBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl ComputerUseBroker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            incarnation_nonce: uuid::Uuid::new_v4().to_string(),
            worker_generation: AtomicU64::new(1),
            snapshot_counter: AtomicU64::new(0),
            readiness_revision: AtomicU64::new(0),
            readiness_revision_state: Mutex::new(None),
            active_session_incarnation: Mutex::new(None),
            screen_capture_gate: Mutex::new(ScreenCaptureGateState::default()),
            human_input_epoch: AtomicU64::new(0),
            input_ownership_ready: AtomicBool::new(false),
            objects: Mutex::new(HashMap::new()),
            writer_lease: WriterLeaseCoordinator::new(),
            browser_extension: Arc::new(BrowserExtensionBroker::default()),
            browser_devtools: BrowserDevtoolsBroker::default(),
        }
    }

    pub(crate) fn start_browser_extension_bridge(
        &self,
        data_root: &Path,
        device_id: String,
        os_session_id: String,
    ) -> std::io::Result<()> {
        super::browser_extension_bridge::start_loopback_bridge(
            Arc::clone(&self.browser_extension),
            data_root,
            device_id,
            os_session_id,
        )
    }

    pub async fn refresh_browser_readiness(
        &self,
        device_id: String,
        os_session_id: String,
        enabled: bool,
        interactive_session_unlocked: bool,
    ) {
        self.browser_devtools
            .refresh(&BrowserBrokerContext {
                device_id,
                os_session_id,
                enabled,
                interactive_session_unlocked,
            })
            .await;
    }

    pub(crate) fn acquire_screen_capture_permit(
        self: &Arc<Self>,
        params: &ScreenCaptureParams,
        selected_display: &str,
    ) -> Result<ScreenCapturePermit, AgentError> {
        validate_screen_selection(params, selected_display)?;
        ensure_screen_capture_safe()?;
        let now = StdInstant::now();
        let mut gate = self.screen_capture_gate.lock().map_err(|_| {
            error(
                AgentErrorKind::Internal,
                "screen capture admission state is unavailable",
                true,
            )
        })?;
        admit_screen_capture(&mut gate, now)?;
        drop(gate);
        Ok(ScreenCapturePermit {
            broker: Arc::clone(self),
        })
    }

    pub(crate) fn set_input_ownership_ready(&self, ready: bool) {
        self.input_ownership_ready.store(ready, Ordering::SeqCst);
    }

    pub(crate) fn input_ownership_is_ready(&self) -> bool {
        self.input_ownership_ready.load(Ordering::SeqCst)
    }

    pub(crate) fn preflight_browser_action(
        &self,
        surface: &ObjectRef,
        request: &BrowserActionRequest,
    ) -> Result<(), BrowserProviderError> {
        if self.browser_extension.surface_ref().as_ref() == Some(surface) {
            self.browser_extension
                .preflight(surface, request)
                .map_err(BrowserProviderError::Extension)
        } else {
            self.browser_devtools
                .preflight(surface, request)
                .map_err(BrowserProviderError::Devtools)
        }
    }

    pub(crate) async fn execute_browser_action(
        &self,
        surface: &ObjectRef,
        request: &BrowserActionRequest,
    ) -> Result<BrowserActionResult, BrowserProviderError> {
        if self.browser_extension.surface_ref().as_ref() == Some(surface) {
            self.browser_extension
                .execute(surface, request)
                .await
                .map_err(BrowserProviderError::Extension)
        } else {
            self.browser_devtools
                .execute(surface, request)
                .await
                .map_err(BrowserProviderError::Devtools)
        }
    }

    fn selected_browser_state(&self) -> (Option<BrowserReadiness>, Option<ObjectRef>) {
        if let Some(readiness) = self.browser_extension.readiness()
            && readiness.connected
        {
            return (Some(readiness), self.browser_extension.surface_ref());
        }
        (
            self.browser_devtools.readiness(),
            self.browser_devtools.surface_ref(),
        )
    }

    pub(crate) fn preflight_ui_action(
        &self,
        target: &ObjectRef,
        action: &UiSemanticAction,
        ceiling: &ComputerUseSettings,
    ) -> Result<(), AgentError> {
        if !self.input_ownership_is_ready() {
            return Err(error(
                AgentErrorKind::SessionUnavailable,
                "semantic desktop UI actions require an active local-input ownership monitor",
                true,
            ));
        }
        if !ceiling.enabled || !ceiling.generic_semantic_ui {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "semantic desktop UI actions are disabled by the device-local ceiling",
                false,
            ));
        }
        let ResolvedObject::UiElement {
            process_id,
            image_path,
            fingerprint,
        } = self.resolve_ref(target)?
        else {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "semantic desktop UI action requires a UI element reference",
                false,
            ));
        };
        if !ceiling.application_allowed(&image_path) {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "the target application is not in the device-local Computer Use allowlist",
                false,
            ));
        }
        #[cfg(target_os = "macos")]
        return super::macos_accessibility_observer::preflight_action(
            process_id,
            &image_path,
            &fingerprint,
            action,
        );
        #[cfg(windows)]
        return super::windows_uia_observer::preflight_action(
            process_id,
            &image_path,
            &fingerprint,
            action,
        );
        #[cfg(not(any(windows, target_os = "macos")))]
        Err(error(
            AgentErrorKind::UnsupportedCapability,
            "semantic desktop UI actions are not enabled for this platform adapter",
            false,
        ))
    }

    pub(crate) fn execute_ui_action(
        &self,
        target: &ObjectRef,
        action: &UiSemanticAction,
        ceiling: &ComputerUseSettings,
    ) -> Result<SemanticActionResult, AgentError> {
        self.preflight_ui_action(target, action, ceiling)?;
        let ResolvedObject::UiElement {
            process_id,
            image_path,
            fingerprint,
        } = self.resolve_ref(target)?
        else {
            unreachable!("preflight accepted only a UI element reference")
        };
        #[cfg(target_os = "macos")]
        {
            let result = super::macos_accessibility_observer::apply_action(
                process_id,
                &image_path,
                &fingerprint,
                action,
            )?;
            return Ok(SemanticActionResult {
                changed: result.changed,
                verified: result.verified,
                summary: result.summary,
                output: None,
            });
        }
        #[cfg(windows)]
        {
            let result = super::windows_uia_observer::apply_action(
                process_id,
                &image_path,
                &fingerprint,
                action,
            )?;
            return Ok(SemanticActionResult {
                changed: result.changed,
                verified: result.verified,
                summary: result.summary,
                output: None,
            });
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        Err(error(
            AgentErrorKind::UnsupportedCapability,
            "semantic desktop UI actions are not enabled for this platform adapter",
            false,
        ))
    }

    pub(crate) fn preflight_raw_input(
        &self,
        target: &ObjectRef,
        action: &RawInputAction,
        ceiling: &ComputerUseSettings,
        selected_display: &str,
    ) -> Result<(), AgentError> {
        if !self.input_ownership_is_ready() {
            return Err(error(
                AgentErrorKind::SessionUnavailable,
                "raw input requires an active local-input ownership monitor",
                true,
            ));
        }
        if !ceiling.enabled || !ceiling.raw_input_fallback {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "raw input fallback is disabled by the independent device-local beta ceiling",
                false,
            ));
        }
        let ResolvedObject::Application {
            window_handle,
            process_id,
            image_path,
            process_started_at,
        } = self.resolve_ref(target)?
        else {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "raw input requires a fresh foreground application reference",
                false,
            ));
        };
        if !ceiling.application_allowed(&image_path) {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "the raw-input target application is not in the device-local allowlist",
                false,
            ));
        }
        #[cfg(windows)]
        {
            let process_started_at = process_started_at.ok_or_else(|| {
                error(
                    AgentErrorKind::InvalidInput,
                    "raw input requires a process-incarnation-bound application reference",
                    false,
                )
            })?;
            super::windows_raw_input::preflight(
                window_handle,
                process_id,
                process_started_at,
                selected_display,
                action,
            )?;
            return Ok(());
        }
        #[cfg(not(windows))]
        {
            let _ = (process_id, selected_display, action);
            Err(error(
                AgentErrorKind::UnsupportedCapability,
                "the raw-input beta adapter is not enabled on this platform",
                false,
            ))
        }
    }

    pub(crate) fn execute_raw_input(
        &self,
        target: &ObjectRef,
        action: &RawInputAction,
        ceiling: &ComputerUseSettings,
        selected_display: &str,
    ) -> Result<SemanticActionResult, AgentError> {
        self.preflight_raw_input(target, action, ceiling, selected_display)?;
        let ResolvedObject::Application {
            window_handle,
            process_id,
            process_started_at,
            ..
        } = self.resolve_ref(target)?
        else {
            unreachable!("preflight accepted only an application reference")
        };
        #[cfg(windows)]
        {
            let process_started_at = process_started_at.expect("preflight required process start");
            let summary = super::windows_raw_input::apply(
                window_handle,
                process_id,
                process_started_at,
                selected_display,
                action,
            )?;
            return Ok(SemanticActionResult {
                changed: true,
                verified: false,
                summary,
                output: None,
            });
        }
        #[cfg(not(windows))]
        {
            let _ = (process_id, selected_display, action);
            Err(error(
                AgentErrorKind::UnsupportedCapability,
                "the raw-input beta adapter is not enabled on this platform",
                false,
            ))
        }
    }

    pub fn preflight_outlook_new_handoff(
        &self,
        application: &ObjectRef,
        request: &OutlookNewComposeHandoffRequest,
    ) -> Result<(), AgentError> {
        let ResolvedObject::Application { image_path, .. } = self.resolve_ref(application)? else {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "Outlook handoff target is not an application reference",
                false,
            ));
        };
        let handler = super::outlook_new_handoff::preflight(request)?;
        if !image_path.eq_ignore_ascii_case(&handler.executable_path) {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "the registered mailto handler changed after readiness was observed",
                false,
            ));
        }
        Ok(())
    }

    pub fn execute_outlook_new_handoff(
        &self,
        application: &ObjectRef,
        request: &OutlookNewComposeHandoffRequest,
    ) -> Result<CommunicationDraftHandoff, AgentError> {
        self.preflight_outlook_new_handoff(application, request)?;
        super::outlook_new_handoff::execute(request)
    }

    pub fn inspect_desktop_session(
        &self,
        params: &DesktopSessionInspectParams,
        ceiling: &ComputerUseSettings,
    ) -> Result<DesktopSessionInspectOutput, AgentError> {
        ensure_observation_enabled(ceiling)?;
        let observed = observe_interactive_desktop()?;
        let snapshot_id = self.next_snapshot_id();
        let incarnation = format!(
            "{}:{}",
            observed.session_id,
            self.current_incarnation_nonce()
        );
        let session = self.issue_ref(
            &snapshot_id,
            &incarnation,
            ObjectKind::DesktopSession,
            ResolvedObject::DesktopSession {
                session_id: observed.session_id,
            },
        )?;
        let active_application = if params.include_active_application {
            observed
                .foreground_application
                .filter(|application| ceiling.application_allowed(&application.image_path))
                .map(|application| {
                    self.issue_ref(
                        &snapshot_id,
                        &incarnation,
                        ObjectKind::Application,
                        ResolvedObject::Application {
                            window_handle: application.window_handle,
                            process_id: application.process_id,
                            image_path: application.image_path,
                            process_started_at: application.process_started_at,
                        },
                    )
                })
                .transpose()?
        } else {
            None
        };
        Ok(DesktopSessionInspectOutput {
            session,
            os: std::env::consts::OS.to_string(),
            interactive_session_incarnation: incarnation,
            active_application,
        })
    }

    #[must_use]
    pub fn readiness(
        &self,
        ceiling: &ComputerUseSettings,
        allow_screen: bool,
        display_selected: bool,
    ) -> ComputerUseReadiness {
        let edge_registry = device_assistant_edge_adapter_registry();
        let session_adapter_version = edge_registry
            .adapter(DESKTOP_SESSION_ADAPTER_ID)
            .expect("compiled desktop session adapter is registered")
            .adapter_version
            .clone();
        #[cfg(windows)]
        let ui_adapter = edge_registry
            .adapter(WINDOWS_UIA_ADAPTER_ID)
            .expect("compiled Windows UIA adapter is registered");
        #[cfg(target_os = "macos")]
        let ui_adapter = edge_registry
            .adapter(MACOS_ACCESSIBILITY_ADAPTER_ID)
            .expect("compiled macOS Accessibility adapter is registered");
        #[cfg(not(any(windows, target_os = "macos")))]
        let ui_adapter = edge_registry
            .adapter(WINDOWS_UIA_ADAPTER_ID)
            .expect("compiled fallback UI adapter is registered");
        let ui_adapter_version = ui_adapter.adapter_version.clone();
        let office_adapter_version = edge_registry
            .adapter(OFFICE_EXCEL_ADAPTER_ID)
            .expect("compiled Office Excel adapter is registered")
            .adapter_version
            .clone();
        let file_adapter_version = edge_registry
            .adapter(FILE_WORKSPACE_ADAPTER_ID)
            .expect("compiled file workspace adapter is registered")
            .adapter_version
            .clone();
        let file_artifact_adapter_version = edge_registry
            .adapter(FILE_ARTIFACT_ADAPTER_ID)
            .expect("compiled file artifact adapter is registered")
            .adapter_version
            .clone();
        let spreadsheet_file_adapter_version = edge_registry
            .adapter(SPREADSHEET_FILE_ADAPTER_ID)
            .expect("compiled spreadsheet file adapter is registered")
            .adapter_version
            .clone();
        let terminal_adapter_version = edge_registry
            .adapter(TERMINAL_OUTPUT_ADAPTER_ID)
            .expect("compiled terminal output adapter is registered")
            .adapter_version
            .clone();
        let screen_adapter_version = edge_registry
            .adapter(CURRENT_SCREEN_ADAPTER_ID)
            .expect("compiled current screen adapter is registered")
            .adapter_version
            .clone();
        let raw_input_adapter_version = edge_registry
            .adapter(WINDOWS_RAW_INPUT_ADAPTER_ID)
            .expect("compiled Windows raw-input adapter is registered")
            .adapter_version
            .clone();
        let system_diagnostics_adapter_version = edge_registry
            .adapter(SYSTEM_DIAGNOSTICS_ADAPTER_ID)
            .expect("compiled system diagnostics adapter is registered")
            .adapter_version
            .clone();
        let system_command_adapter_version = edge_registry
            .adapter(SYSTEM_COMMAND_ADAPTER_ID)
            .expect("compiled system command adapter is registered")
            .adapter_version
            .clone();
        let observed_at = Utc::now();
        let expires_at = observed_at + Duration::seconds(25);
        let observation = if ceiling.observation_enabled() {
            Some(observe_interactive_desktop())
        } else {
            None
        };
        let interactive_session_incarnation = observation
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .map(|desktop| {
                format!(
                    "{}:{}",
                    desktop.session_id,
                    self.current_incarnation_nonce()
                )
            })
            .unwrap_or_else(|| format!("unavailable:{}", self.current_incarnation_nonce()));
        self.update_active_session_incarnation(
            observation
                .as_ref()
                .is_some_and(|result| result.is_ok())
                .then(|| interactive_session_incarnation.clone()),
        );
        let platform_supported = cfg!(any(windows, target_os = "macos"));
        let file_provider_supported = cfg!(any(windows, target_os = "macos"));
        let office_provider_supported = cfg!(windows);
        let iwork_provider_supported = cfg!(target_os = "macos");
        let outlook_provider_supported = cfg!(windows);
        let browser_provider_supported = cfg!(any(windows, target_os = "macos"));
        let semantic_action_supported = cfg!(any(windows, target_os = "macos"));
        let raw_input_supported = cfg!(windows);
        #[cfg(target_os = "macos")]
        let macos_permissions = crate::macos_permissions::probe();
        #[cfg(not(target_os = "macos"))]
        let macos_accessibility_ready = true;
        #[cfg(target_os = "macos")]
        let macos_accessibility_ready = macos_permissions.accessibility;
        #[cfg(target_os = "macos")]
        let macos_input_permission_ready = macos_permissions.input_monitoring;
        #[cfg(not(target_os = "macos"))]
        let macos_input_permission_ready = false;
        let input_ownership_ready = if cfg!(target_os = "macos") {
            macos_input_permission_ready && self.input_ownership_is_ready()
        } else {
            self.input_ownership_is_ready()
        };
        let (session_ready, session_reason) = if !ceiling.observation_enabled() {
            (
                false,
                Some(ComputerUseReadinessReason::DisabledByLocalCeiling),
            )
        } else if !platform_supported {
            (false, Some(ComputerUseReadinessReason::UnsupportedPlatform))
        } else if observation.is_some_and(|result| result.is_ok()) {
            (true, None)
        } else {
            (
                false,
                Some(ComputerUseReadinessReason::NoInteractiveSession),
            )
        };
        let ui_ready = session_ready
            && macos_accessibility_ready
            && !ceiling.allowed_application_paths.is_empty();
        let office_configured = super::office_bridge_observer::configured();
        let office_document_ref = (session_ready && ceiling.office_semantic && office_configured)
            .then(super::office_bridge_observer::current_excel_document_hash)
            .flatten()
            .and_then(|document_url_hash| {
                let snapshot_id = self.next_snapshot_id();
                self.issue_ref(
                    &snapshot_id,
                    &interactive_session_incarnation,
                    ObjectKind::OfficeDocument,
                    ResolvedObject::OfficeDocument { document_url_hash },
                )
                .ok()
            });
        let office_ready = office_document_ref.is_some();
        #[cfg(target_os = "macos")]
        let issue_iwork_ref = |application, object_kind| {
            if !session_ready || !ceiling.iwork_semantic {
                return Ok(None);
            }
            let observed = super::macos_iwork_adapter::observe(application)?;
            let snapshot_id = self.next_snapshot_id();
            self.issue_ref(
                &snapshot_id,
                &interactive_session_incarnation,
                object_kind,
                iwork_resolved_object(&observed),
            )
            .map(Some)
        };
        #[cfg(target_os = "macos")]
        let numbers_result = issue_iwork_ref(
            super::macos_iwork_adapter::IworkApplication::Numbers,
            ObjectKind::Range,
        );
        #[cfg(target_os = "macos")]
        let pages_result = issue_iwork_ref(
            super::macos_iwork_adapter::IworkApplication::Pages,
            ObjectKind::Document,
        );
        #[cfg(target_os = "macos")]
        let keynote_result = issue_iwork_ref(
            super::macos_iwork_adapter::IworkApplication::Keynote,
            ObjectKind::Slide,
        );
        #[cfg(target_os = "macos")]
        let (numbers_ref, numbers_error) = split_iwork_readiness(numbers_result);
        #[cfg(target_os = "macos")]
        let (pages_ref, pages_error) = split_iwork_readiness(pages_result);
        #[cfg(target_os = "macos")]
        let (keynote_ref, keynote_error) = split_iwork_readiness(keynote_result);
        #[cfg(not(target_os = "macos"))]
        let (numbers_ref, pages_ref, keynote_ref): (
            Option<ObjectRef>,
            Option<ObjectRef>,
            Option<ObjectRef>,
        ) = (None, None, None);
        #[cfg(not(target_os = "macos"))]
        let (numbers_error, pages_error, keynote_error): (
            Option<AgentErrorKind>,
            Option<AgentErrorKind>,
            Option<AgentErrorKind>,
        ) = (None, None, None);
        let iwork_reason = |ready: bool, failure: Option<AgentErrorKind>| {
            (!ready).then_some(
                if !ceiling.observation_enabled() || !ceiling.iwork_semantic {
                    ComputerUseReadinessReason::DisabledByLocalCeiling
                } else if !iwork_provider_supported {
                    ComputerUseReadinessReason::UnsupportedPlatform
                } else if !session_ready {
                    session_reason.unwrap_or(ComputerUseReadinessReason::NoInteractiveSession)
                } else {
                    match failure {
                        Some(AgentErrorKind::PermissionDenied) => {
                            ComputerUseReadinessReason::PermissionMissing
                        }
                        Some(AgentErrorKind::SessionUnavailable) => {
                            ComputerUseReadinessReason::NoActiveDocument
                        }
                        Some(AgentErrorKind::TargetOffline)
                        | Some(AgentErrorKind::UnsupportedCapability)
                        | Some(AgentErrorKind::TransportError)
                        | Some(AgentErrorKind::Timeout)
                        | Some(AgentErrorKind::Internal) => {
                            ComputerUseReadinessReason::AdapterUnavailable
                        }
                        _ => ComputerUseReadinessReason::NoActiveDocument,
                    }
                },
            )
        };
        let (screen_ready, screen_reason) = screen_capture_readiness(
            ceiling.observation_enabled(),
            platform_supported,
            session_ready,
            session_reason,
            allow_screen,
            display_selected,
        );
        #[cfg(target_os = "macos")]
        let (screen_ready, screen_reason) = if screen_ready && !macos_permissions.screen_recording {
            (false, Some(ComputerUseReadinessReason::PermissionMissing))
        } else {
            (screen_ready, screen_reason)
        };
        let (browser_readiness, browser_surface) = self.selected_browser_state();
        let browser_ready = browser_provider_supported
            && session_ready
            && ceiling.browser_semantic
            && browser_readiness
                .as_ref()
                .is_some_and(|readiness| readiness.connected)
            && browser_surface.is_some();
        let browser_reason = (!browser_ready).then_some(if !ceiling.browser_semantic {
            ComputerUseReadinessReason::DisabledByLocalCeiling
        } else if !browser_provider_supported {
            ComputerUseReadinessReason::UnsupportedPlatform
        } else if !session_ready {
            session_reason.unwrap_or(ComputerUseReadinessReason::NoInteractiveSession)
        } else {
            match browser_readiness
                .as_ref()
                .and_then(|readiness| readiness.reason)
            {
                Some(BrowserReadinessReason::UserApprovalRequired)
                | Some(BrowserReadinessReason::UserDenied)
                | Some(BrowserReadinessReason::PairingRequired)
                | Some(BrowserReadinessReason::HostPermissionMissing) => {
                    ComputerUseReadinessReason::PermissionMissing
                }
                _ => ComputerUseReadinessReason::AdapterUnavailable,
            }
        });
        let slack_ready = browser_ready && ceiling.communication_handoff_enabled();
        let slack_reason = (!slack_ready).then_some(if !ceiling.communication_handoff_enabled() {
            ComputerUseReadinessReason::DisabledByLocalCeiling
        } else {
            browser_reason.unwrap_or(ComputerUseReadinessReason::AdapterUnavailable)
        });
        let slack_browser_surface = slack_ready.then(|| browser_surface.clone()).flatten();
        let browser_adapter = browser_readiness
            .as_ref()
            .map(|readiness| ComputerUseAdapterRef {
                kind: match readiness.adapter.engine {
                    BrowserEngineKind::ChromeExtension => ComputerUseAdapterKind::BrowserExtension,
                    BrowserEngineKind::ChromeDevtoolsMcp => {
                        ComputerUseAdapterKind::BrowserDevtoolsMcp
                    }
                },
                version: readiness.adapter.adapter_version.clone(),
            })
            .unwrap_or_else(|| ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::BrowserExtension,
                version: super::browser_extension_bridge::BROWSER_EXTENSION_VERSION.into(),
            });
        let outlook_handler = if outlook_provider_supported
            && session_ready
            && ceiling.communication_handoff_enabled()
        {
            super::outlook_new_handoff::probe_handler().ok()
        } else {
            None
        };
        let outlook_application_ref = outlook_handler.as_ref().and_then(|handler| {
            let snapshot_id = self.next_snapshot_id();
            self.issue_ref(
                &snapshot_id,
                &interactive_session_incarnation,
                ObjectKind::Application,
                ResolvedObject::Application {
                    window_handle: 0,
                    process_id: 0,
                    image_path: handler.executable_path.clone(),
                    process_started_at: None,
                },
            )
            .ok()
        });
        let outlook_ready = outlook_application_ref.is_some();
        let outlook_reason =
            (!outlook_ready).then_some(if !ceiling.communication_handoff_enabled() {
                ComputerUseReadinessReason::DisabledByLocalCeiling
            } else if !outlook_provider_supported {
                ComputerUseReadinessReason::UnsupportedPlatform
            } else if !session_ready {
                session_reason.unwrap_or(ComputerUseReadinessReason::NoInteractiveSession)
            } else {
                ComputerUseReadinessReason::AdapterUnavailable
            });
        let outlook_adapter = ComputerUseAdapterRef {
            kind: ComputerUseAdapterKind::OutlookNewMailto,
            version: OUTLOOK_NEW_MAILTO_ADAPTER_VERSION.into(),
        };
        let adapter = ComputerUseAdapterRef {
            #[cfg(windows)]
            kind: ComputerUseAdapterKind::WindowsUia,
            #[cfg(target_os = "macos")]
            kind: ComputerUseAdapterKind::MacosAccessibility,
            #[cfg(not(any(windows, target_os = "macos")))]
            kind: ComputerUseAdapterKind::WindowsUia,
            version: session_adapter_version,
        };
        let diagnostic_adapter = ComputerUseAdapterRef {
            kind: ComputerUseAdapterKind::SystemDiagnostics,
            version: system_diagnostics_adapter_version,
        };
        let diagnostic_reason =
            (!platform_supported).then_some(ComputerUseReadinessReason::UnsupportedPlatform);
        let command_shells_available = !crate::exec_shells::available_exec_shells().is_empty();
        let command_ready = platform_supported && command_shells_available;
        let command_reason = (!command_ready).then_some(if !platform_supported {
            ComputerUseReadinessReason::UnsupportedPlatform
        } else {
            ComputerUseReadinessReason::AdapterUnavailable
        });
        let mut readiness = ComputerUseReadiness {
            schema_version: COMPUTER_USE_SCHEMA_VERSION,
            revision: 0,
            observed_at: observed_at.to_rfc3339(),
            expires_at: expires_at.to_rfc3339(),
            server_api_version: desk_server_version::SERVER_API_VERSION,
            os: std::env::consts::OS.into(),
            interactive_session_incarnation,
            local_ceiling_revision: ceiling.revision,
            capabilities: vec![
                ComputerUseCapabilityReadiness {
                    capability: Capability::SystemInfo,
                    adapter: diagnostic_adapter.clone(),
                    supported: platform_supported,
                    ready: platform_supported,
                    reason: diagnostic_reason,
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::ProcessList,
                    adapter: diagnostic_adapter.clone(),
                    supported: platform_supported,
                    ready: platform_supported,
                    reason: diagnostic_reason,
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::NetworkPorts,
                    adapter: diagnostic_adapter.clone(),
                    supported: platform_supported,
                    ready: platform_supported,
                    reason: diagnostic_reason,
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::ServiceStatus,
                    adapter: diagnostic_adapter.clone(),
                    supported: platform_supported,
                    ready: platform_supported,
                    reason: diagnostic_reason,
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::LogRecent,
                    adapter: diagnostic_adapter.clone(),
                    supported: platform_supported,
                    ready: platform_supported,
                    reason: diagnostic_reason,
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::ContainerList,
                    adapter: diagnostic_adapter,
                    supported: platform_supported,
                    ready: platform_supported,
                    reason: diagnostic_reason,
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::ShellExecConfirmed,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::Terminal,
                        version: system_command_adapter_version,
                    },
                    supported: platform_supported,
                    ready: command_ready,
                    reason: command_reason,
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::DesktopSessionInspect,
                    adapter: adapter.clone(),
                    supported: platform_supported,
                    ready: session_ready,
                    reason: session_reason,
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::DesktopUiInspect,
                    adapter: ComputerUseAdapterRef {
                        kind: adapter.kind,
                        version: ui_adapter_version.clone(),
                    },
                    supported: platform_supported,
                    ready: ui_ready,
                    reason: (!ui_ready).then_some(
                        if !ceiling.observation_enabled()
                            || ceiling.allowed_application_paths.is_empty()
                        {
                            ComputerUseReadinessReason::DisabledByLocalCeiling
                        } else if !macos_accessibility_ready {
                            ComputerUseReadinessReason::PermissionMissing
                        } else {
                            session_reason.unwrap_or(ComputerUseReadinessReason::AdapterUnavailable)
                        },
                    ),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::DesktopUiActionConfirmed,
                    adapter: ComputerUseAdapterRef {
                        kind: adapter.kind,
                        version: ui_adapter_version,
                    },
                    supported: semantic_action_supported,
                    ready: semantic_action_supported
                        && ui_ready
                        && input_ownership_ready
                        && ceiling.generic_semantic_ui,
                    reason: (!(semantic_action_supported
                        && ui_ready
                        && input_ownership_ready
                        && ceiling.generic_semantic_ui))
                        .then_some(if !semantic_action_supported {
                            ComputerUseReadinessReason::UnsupportedPlatform
                        } else if !ceiling.generic_semantic_ui {
                            ComputerUseReadinessReason::DisabledByLocalCeiling
                        } else if !macos_accessibility_ready
                            || cfg!(target_os = "macos") && !macos_input_permission_ready
                        {
                            ComputerUseReadinessReason::PermissionMissing
                        } else if !input_ownership_ready {
                            ComputerUseReadinessReason::AdapterUnavailable
                        } else {
                            session_reason.unwrap_or(ComputerUseReadinessReason::AdapterUnavailable)
                        }),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::DesktopInputFallbackConfirmed,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::WindowsRawInput,
                        version: raw_input_adapter_version,
                    },
                    supported: raw_input_supported,
                    ready: raw_input_supported
                        && session_ready
                        && input_ownership_ready
                        && display_selected
                        && !ceiling.allowed_application_paths.is_empty()
                        && ceiling.enabled
                        && ceiling.raw_input_fallback,
                    reason: (!(raw_input_supported
                        && session_ready
                        && input_ownership_ready
                        && display_selected
                        && !ceiling.allowed_application_paths.is_empty()
                        && ceiling.enabled
                        && ceiling.raw_input_fallback))
                        .then_some(if !raw_input_supported {
                            ComputerUseReadinessReason::UnsupportedPlatform
                        } else if !ceiling.enabled
                            || !ceiling.raw_input_fallback
                            || ceiling.allowed_application_paths.is_empty()
                        {
                            ComputerUseReadinessReason::DisabledByLocalCeiling
                        } else if !display_selected {
                            ComputerUseReadinessReason::NoDisplaySelected
                        } else if !input_ownership_ready {
                            ComputerUseReadinessReason::AdapterUnavailable
                        } else {
                            session_reason
                                .unwrap_or(ComputerUseReadinessReason::NoInteractiveSession)
                        }),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::OfficeDocumentInspect,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::OfficeExcel,
                        version: office_adapter_version,
                    },
                    supported: office_provider_supported,
                    ready: office_ready,
                    reason: (!office_ready).then_some(
                        if !ceiling.observation_enabled() || !ceiling.office_semantic {
                            ComputerUseReadinessReason::DisabledByLocalCeiling
                        } else if !office_provider_supported {
                            ComputerUseReadinessReason::UnsupportedPlatform
                        } else if !session_ready {
                            session_reason
                                .unwrap_or(ComputerUseReadinessReason::NoInteractiveSession)
                        } else if !office_configured {
                            ComputerUseReadinessReason::AdapterUnavailable
                        } else {
                            ComputerUseReadinessReason::OfficeBridgeNotPaired
                        },
                    ),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::SpreadsheetLiveInspect,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::IworkNumbers,
                        version: IWORK_ADAPTER_VERSION.into(),
                    },
                    supported: iwork_provider_supported,
                    ready: numbers_ref.is_some(),
                    reason: iwork_reason(numbers_ref.is_some(), numbers_error),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::SpreadsheetLivePatchConfirmed,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::IworkNumbers,
                        version: IWORK_ADAPTER_VERSION.into(),
                    },
                    supported: iwork_provider_supported,
                    ready: numbers_ref.is_some(),
                    reason: iwork_reason(numbers_ref.is_some(), numbers_error),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::DocumentLiveInspect,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::IworkPages,
                        version: IWORK_ADAPTER_VERSION.into(),
                    },
                    supported: iwork_provider_supported,
                    ready: pages_ref.is_some(),
                    reason: iwork_reason(pages_ref.is_some(), pages_error),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::DocumentLivePatchConfirmed,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::IworkPages,
                        version: IWORK_ADAPTER_VERSION.into(),
                    },
                    supported: iwork_provider_supported,
                    ready: pages_ref.is_some(),
                    reason: iwork_reason(pages_ref.is_some(), pages_error),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::PresentationLiveInspect,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::IworkKeynote,
                        version: IWORK_ADAPTER_VERSION.into(),
                    },
                    supported: iwork_provider_supported,
                    ready: keynote_ref.is_some(),
                    reason: iwork_reason(keynote_ref.is_some(), keynote_error),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::PresentationLivePatchConfirmed,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::IworkKeynote,
                        version: IWORK_ADAPTER_VERSION.into(),
                    },
                    supported: iwork_provider_supported,
                    ready: keynote_ref.is_some(),
                    reason: iwork_reason(keynote_ref.is_some(), keynote_error),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::FileMetadataRead,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::FileSystem,
                        version: file_adapter_version.clone(),
                    },
                    supported: file_provider_supported,
                    ready: file_provider_supported,
                    reason: (!file_provider_supported)
                        .then_some(ComputerUseReadinessReason::UnsupportedPlatform),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::FileContentRead,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::FileSystem,
                        version: file_adapter_version,
                    },
                    supported: file_provider_supported,
                    ready: file_provider_supported,
                    reason: (!file_provider_supported)
                        .then_some(ComputerUseReadinessReason::UnsupportedPlatform),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::SpreadsheetFileInspect,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::FileSystem,
                        version: spreadsheet_file_adapter_version.clone(),
                    },
                    supported: file_provider_supported,
                    ready: file_provider_supported,
                    reason: (!file_provider_supported)
                        .then_some(ComputerUseReadinessReason::UnsupportedPlatform),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::SpreadsheetMergePreview,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::FileSystem,
                        version: spreadsheet_file_adapter_version.clone(),
                    },
                    supported: file_provider_supported,
                    ready: file_provider_supported,
                    reason: (!file_provider_supported)
                        .then_some(ComputerUseReadinessReason::UnsupportedPlatform),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::SpreadsheetWorkbookCreateConfirmed,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::FileSystem,
                        version: spreadsheet_file_adapter_version.clone(),
                    },
                    supported: file_provider_supported,
                    ready: file_provider_supported && ceiling.file_artifact_create_enabled(),
                    reason: (!(file_provider_supported && ceiling.file_artifact_create_enabled()))
                        .then_some(if !file_provider_supported {
                            ComputerUseReadinessReason::UnsupportedPlatform
                        } else {
                            ComputerUseReadinessReason::DisabledByLocalCeiling
                        }),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::SpreadsheetFormulaWorkbookCreateConfirmed,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::FileSystem,
                        version: spreadsheet_file_adapter_version.clone(),
                    },
                    supported: file_provider_supported,
                    ready: file_provider_supported && ceiling.file_artifact_create_enabled(),
                    reason: (!(file_provider_supported && ceiling.file_artifact_create_enabled()))
                        .then_some(if !file_provider_supported {
                            ComputerUseReadinessReason::UnsupportedPlatform
                        } else {
                            ComputerUseReadinessReason::DisabledByLocalCeiling
                        }),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::WordDocumentCreateConfirmed,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::FileSystem,
                        version: spreadsheet_file_adapter_version,
                    },
                    supported: file_provider_supported,
                    ready: file_provider_supported && ceiling.file_artifact_create_enabled(),
                    reason: (!(file_provider_supported && ceiling.file_artifact_create_enabled()))
                        .then_some(if !file_provider_supported {
                            ComputerUseReadinessReason::UnsupportedPlatform
                        } else {
                            ComputerUseReadinessReason::DisabledByLocalCeiling
                        }),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::FileArtifactCreateConfirmed,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::FileSystem,
                        version: file_artifact_adapter_version.clone(),
                    },
                    supported: file_provider_supported,
                    ready: file_provider_supported && ceiling.file_artifact_create_enabled(),
                    reason: (!(file_provider_supported && ceiling.file_artifact_create_enabled()))
                        .then_some(if !file_provider_supported {
                            ComputerUseReadinessReason::UnsupportedPlatform
                        } else {
                            ComputerUseReadinessReason::DisabledByLocalCeiling
                        }),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::CommunicationLocalDraftCreateConfirmed,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::FileSystem,
                        version: file_artifact_adapter_version,
                    },
                    supported: file_provider_supported,
                    ready: file_provider_supported && ceiling.file_artifact_create_enabled(),
                    reason: (!(file_provider_supported && ceiling.file_artifact_create_enabled()))
                        .then_some(if !file_provider_supported {
                            ComputerUseReadinessReason::UnsupportedPlatform
                        } else {
                            ComputerUseReadinessReason::DisabledByLocalCeiling
                        }),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::CommunicationOutlookNewHandoffConfirmed,
                    adapter: outlook_adapter,
                    supported: outlook_provider_supported,
                    ready: outlook_ready,
                    reason: outlook_reason,
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::TerminalOutputRead,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::Terminal,
                        version: terminal_adapter_version,
                    },
                    supported: platform_supported,
                    ready: platform_supported,
                    reason: (!platform_supported)
                        .then_some(ComputerUseReadinessReason::UnsupportedPlatform),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::ScreenCaptureCurrent,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::ScreenCapture,
                        version: screen_adapter_version,
                    },
                    supported: platform_supported,
                    ready: screen_ready,
                    reason: screen_reason,
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::BrowserPageObserve,
                    adapter: browser_adapter.clone(),
                    supported: browser_provider_supported,
                    ready: browser_ready,
                    reason: browser_reason,
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::BrowserPageNavigateConfirmed,
                    adapter: browser_adapter.clone(),
                    supported: browser_provider_supported,
                    ready: browser_ready,
                    reason: browser_reason,
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::BrowserInputFallbackConfirmed,
                    adapter: browser_adapter.clone(),
                    supported: browser_provider_supported,
                    ready: browser_ready,
                    reason: browser_reason,
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::BrowserExternalDraftWriteConfirmed,
                    adapter: browser_adapter.clone(),
                    supported: browser_provider_supported,
                    ready: slack_ready,
                    reason: slack_reason,
                },
            ],
            context_references: office_document_ref
                .into_iter()
                .map(|object_ref| ComputerUseContextReference {
                    capability: Capability::OfficeDocumentInspect,
                    object_ref,
                })
                .chain(numbers_ref.into_iter().flat_map(|object_ref| {
                    [
                        Capability::SpreadsheetLiveInspect,
                        Capability::SpreadsheetLivePatchConfirmed,
                    ]
                    .into_iter()
                    .map(move |capability| ComputerUseContextReference {
                        capability,
                        object_ref: object_ref.clone(),
                    })
                }))
                .chain(pages_ref.into_iter().flat_map(|object_ref| {
                    [
                        Capability::DocumentLiveInspect,
                        Capability::DocumentLivePatchConfirmed,
                    ]
                    .into_iter()
                    .map(move |capability| ComputerUseContextReference {
                        capability,
                        object_ref: object_ref.clone(),
                    })
                }))
                .chain(keynote_ref.into_iter().flat_map(|object_ref| {
                    [
                        Capability::PresentationLiveInspect,
                        Capability::PresentationLivePatchConfirmed,
                    ]
                    .into_iter()
                    .map(move |capability| ComputerUseContextReference {
                        capability,
                        object_ref: object_ref.clone(),
                    })
                }))
                .chain(browser_surface.into_iter().flat_map(|object_ref| {
                    [
                        Capability::BrowserPageObserve,
                        Capability::BrowserPageNavigateConfirmed,
                        Capability::BrowserInputFallbackConfirmed,
                    ]
                    .into_iter()
                    .map(move |capability| ComputerUseContextReference {
                        capability,
                        object_ref: object_ref.clone(),
                    })
                }))
                .chain(slack_browser_surface.into_iter().map(|object_ref| {
                    ComputerUseContextReference {
                        capability: Capability::BrowserExternalDraftWriteConfirmed,
                        object_ref,
                    }
                }))
                .chain(outlook_application_ref.into_iter().map(|object_ref| {
                    ComputerUseContextReference {
                        capability: Capability::CommunicationOutlookNewHandoffConfirmed,
                        object_ref,
                    }
                }))
                .collect(),
        };
        readiness.revision = self.stable_readiness_revision(&readiness);
        readiness
    }

    /// A readiness revision is a material-state fence, not a heartbeat
    /// sequence. Reusing it across equivalent reports lets a freshly approved
    /// grant survive timestamp refreshes while still invalidating immediately
    /// when the session incarnation, local ceiling, adapter readiness, or
    /// context-capability availability changes.
    fn stable_readiness_revision(&self, readiness: &ComputerUseReadiness) -> u64 {
        let fingerprint = ReadinessFingerprint {
            server_api_version: readiness.server_api_version,
            os: readiness.os.clone(),
            interactive_session_incarnation: readiness.interactive_session_incarnation.clone(),
            local_ceiling_revision: readiness.local_ceiling_revision,
            capabilities: readiness.capabilities.clone(),
            context_capabilities: readiness
                .context_references
                .iter()
                .map(|reference| reference.capability)
                .collect(),
            browser_adapter: self
                .selected_browser_state()
                .0
                .map(|readiness| readiness.adapter),
        };
        let mut state = self
            .readiness_revision_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(current) = state.as_ref()
            && current.fingerprint == fingerprint
        {
            return current.revision;
        }
        let revision = self.readiness_revision.fetch_add(1, Ordering::Relaxed) + 1;
        *state = Some(ReadinessRevisionState {
            fingerprint,
            revision,
        });
        revision
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn inspect_iwork(
        &self,
        application: super::macos_iwork_adapter::IworkApplication,
        params: &LiveDocumentInspectParams,
        ceiling: &ComputerUseSettings,
    ) -> Result<LiveDocumentInspectOutput, AgentError> {
        ensure_observation_enabled(ceiling)?;
        if !ceiling.iwork_semantic {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "iWork semantic observation is disabled in device-local settings",
                false,
            ));
        }
        if params.max_bytes < 1_024 || params.max_bytes > MAX_COMPUTER_USE_INSPECT_BYTES {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "iWork inspection byte bounds exceed the device ceiling",
                false,
            ));
        }
        if params.target.is_some() && params.batch_file.is_some() {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "interactive and batch iWork targets are mutually exclusive",
                false,
            ));
        }
        let (observed, resolved, batch_source) = if let Some(file) = &params.batch_file {
            let verified = super::file_reference_store::resolve_verified_native_file(
                file,
                iwork_native_extensions(application),
                128 * 1024 * 1024,
            )?;
            let observed = super::macos_iwork_adapter::observe_batch(application, &verified.path)?;
            super::file_reference_store::revalidate_verified_native_file(
                file,
                &verified,
                128 * 1024 * 1024,
            )?;
            let resolved = iwork_batch_resolved(file, &verified, &observed);
            let source = BatchDocumentSourceProjection {
                file: file.clone(),
                display_name: verified.display_name,
                byte_len: verified.byte_len,
                sha256: verified.sha256,
            };
            (observed, resolved, Some(source))
        } else {
            let observed = super::macos_iwork_adapter::observe(application)?;
            let resolved = iwork_resolved_object(&observed);
            (observed, resolved, None)
        };
        if let Some(expected) = &params.target
            && self.resolve_ref(expected)? != resolved
        {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "the iWork target changed during semantic observation",
                false,
            ));
        }
        let desktop = observe_interactive_desktop()?;
        let snapshot_id = self.next_snapshot_id();
        let incarnation = format!(
            "{}:{}",
            desktop.session_id,
            self.current_incarnation_nonce()
        );
        let (adapter_kind, projection) = match observed {
            super::macos_iwork_adapter::IworkObservation::Numbers {
                locator,
                value,
                formula,
                formatted_value,
            } => {
                let document = self.issue_ref(
                    &snapshot_id,
                    &incarnation,
                    ObjectKind::Document,
                    resolved.clone(),
                )?;
                let worksheet = self.issue_ref(
                    &snapshot_id,
                    &incarnation,
                    ObjectKind::Worksheet,
                    resolved.clone(),
                )?;
                let range =
                    self.issue_ref(&snapshot_id, &incarnation, ObjectKind::Range, resolved)?;
                (
                    ComputerUseAdapterKind::IworkNumbers,
                    LiveDocumentProjection::Spreadsheet {
                        document,
                        worksheet,
                        range,
                        sheet_name: locator.sheet_name,
                        table_name: locator.table_name,
                        address: locator.cell_address,
                        value,
                        formula,
                        formatted_value,
                    },
                )
            }
            super::macos_iwork_adapter::IworkObservation::Pages { locator, body_text } => {
                let document = self.issue_ref(
                    &snapshot_id,
                    &incarnation,
                    ObjectKind::Document,
                    resolved.clone(),
                )?;
                (
                    ComputerUseAdapterKind::IworkPages,
                    LiveDocumentProjection::Document {
                        document,
                        body_sha256: locator.before_sha256,
                        body_text,
                    },
                )
            }
            super::macos_iwork_adapter::IworkObservation::Keynote {
                locator,
                title,
                presenter_notes,
            } => {
                let presentation = self.issue_ref(
                    &snapshot_id,
                    &incarnation,
                    ObjectKind::Presentation,
                    resolved.clone(),
                )?;
                let slide =
                    self.issue_ref(&snapshot_id, &incarnation, ObjectKind::Slide, resolved)?;
                (
                    ComputerUseAdapterKind::IworkKeynote,
                    LiveDocumentProjection::Presentation {
                        presentation,
                        slide,
                        slide_number: locator.slide_number,
                        title,
                        presenter_notes,
                    },
                )
            }
        };
        let output = LiveDocumentInspectOutput {
            snapshot_id,
            adapter: ComputerUseAdapterRef {
                kind: adapter_kind,
                version: super::macos_iwork_adapter::IWORK_ADAPTER_VERSION.into(),
            },
            projection,
            batch_source,
        };
        let encoded_bytes = serde_json::to_vec(&output)
            .map_err(|_| {
                error(
                    AgentErrorKind::Internal,
                    "cannot encode iWork projection",
                    true,
                )
            })?
            .len();
        if encoded_bytes > params.max_bytes as usize {
            return Err(error(
                AgentErrorKind::OutputLimitExceeded,
                "the iWork semantic projection exceeds the requested byte budget",
                false,
            ));
        }
        Ok(output)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn preflight_iwork_action(
        &self,
        target: &ObjectRef,
        action: &ComputerActionKind,
        ceiling: &ComputerUseSettings,
    ) -> Result<(), AgentError> {
        if !ceiling.enabled || !ceiling.iwork_semantic {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "iWork semantic mutation is disabled by the host-local ceiling",
                false,
            ));
        }
        let resolved = self.resolve_ref(target)?;
        let compatible = matches!(
            (&resolved, action),
            (
                ResolvedObject::IworkNumbersCell { .. },
                ComputerActionKind::SpreadsheetLive(_)
            ) | (
                ResolvedObject::IworkPagesDocument { .. },
                ComputerActionKind::DocumentLive(_)
            ) | (
                ResolvedObject::IworkKeynoteSlide { .. },
                ComputerActionKind::PresentationLive(_)
            ) | (
                ResolvedObject::IworkNumbersBatch { .. },
                ComputerActionKind::SpreadsheetLiveBatch(_)
            ) | (
                ResolvedObject::IworkPagesBatch { .. },
                ComputerActionKind::DocumentLiveBatch(_)
            ) | (
                ResolvedObject::IworkKeynoteBatch { .. },
                ComputerActionKind::PresentationLiveBatch(_)
            )
        );
        if !compatible {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "the iWork action does not match its typed object reference",
                false,
            ));
        }
        let formula_context = match (&resolved, action) {
            (
                ResolvedObject::IworkNumbersCell {
                    sheet_name,
                    cell_address,
                    ..
                },
                ComputerActionKind::SpreadsheetLive(
                    desk_agent_protocol::computer_use::SpreadsheetLivePatchAction::SetCellFormula {
                        formula,
                    },
                ),
            ) => Some((sheet_name, cell_address, formula)),
            (
                ResolvedObject::IworkNumbersBatch {
                    sheet_name,
                    cell_address,
                    ..
                },
                ComputerActionKind::SpreadsheetLiveBatch(batch),
            ) => match &batch.action {
                desk_agent_protocol::computer_use::SpreadsheetLivePatchAction::SetCellFormula {
                    formula,
                } => Some((sheet_name, cell_address, formula)),
                _ => None,
            },
            _ => None,
        };
        if let Some((sheet_name, cell_address, formula)) = formula_context {
            desk_diagnose_core::spreadsheet_formula::validate_formula_patch(
                formula,
                cell_address,
                desk_diagnose_core::spreadsheet_formula::FORMULA_LOCALE_V1,
                std::slice::from_ref(sheet_name),
            )
            .map_err(|validation_error| {
                error(
                    AgentErrorKind::InvalidInput,
                    &validation_error.to_string(),
                    false,
                )
            })?;
        }
        let batch_output = match action {
            ComputerActionKind::SpreadsheetLiveBatch(action) => Some(&action.output),
            ComputerActionKind::DocumentLiveBatch(action) => Some(&action.output),
            ComputerActionKind::PresentationLiveBatch(action) => Some(&action.output),
            _ => None,
        };
        if let Some(output) = batch_output {
            if !ceiling.file_artifact_create_enabled() {
                return Err(error(
                    AgentErrorKind::PermissionDenied,
                    "iWork batch output requires the host-local artifact creation ceiling",
                    false,
                ));
            }
            if output.destination_parent.object_kind != ObjectKind::Directory
                || output.native_file_name.is_empty()
                || output.native_file_name.len() > 200
            {
                return Err(error(
                    AgentErrorKind::InvalidInput,
                    "iWork batch output requires a fresh directory and bounded native file name",
                    false,
                ));
            }
        }
        match (&resolved, action) {
            (
                ResolvedObject::IworkNumbersBatch {
                    source_file,
                    source_sha256,
                    source_byte_len,
                    ..
                },
                ComputerActionKind::SpreadsheetLiveBatch(batch),
            ) => {
                verify_batch_source(source_file, &[".numbers"], source_sha256, *source_byte_len)?;
                super::file_reference_store::validate_native_artifact_destination(
                    &batch.output.destination_parent,
                    &ceiling.allowed_file_roots,
                    &batch.output.native_file_name,
                    ".numbers",
                )?;
            }
            (
                ResolvedObject::IworkPagesBatch {
                    source_file,
                    source_sha256,
                    source_byte_len,
                    ..
                },
                ComputerActionKind::DocumentLiveBatch(batch),
            ) => {
                verify_batch_source(source_file, &[".pages"], source_sha256, *source_byte_len)?;
                super::file_reference_store::validate_native_artifact_destination(
                    &batch.output.destination_parent,
                    &ceiling.allowed_file_roots,
                    &batch.output.native_file_name,
                    ".pages",
                )?;
            }
            (
                ResolvedObject::IworkKeynoteBatch {
                    source_file,
                    source_sha256,
                    source_byte_len,
                    ..
                },
                ComputerActionKind::PresentationLiveBatch(batch),
            ) => {
                verify_batch_source(source_file, &[".key"], source_sha256, *source_byte_len)?;
                super::file_reference_store::validate_native_artifact_destination(
                    &batch.output.destination_parent,
                    &ceiling.allowed_file_roots,
                    &batch.output.native_file_name,
                    ".key",
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn execute_iwork_action(
        &self,
        target: &ObjectRef,
        action: &ComputerActionKind,
        ceiling: &ComputerUseSettings,
    ) -> Result<SemanticActionResult, AgentError> {
        self.preflight_iwork_action(target, action, ceiling)?;
        let resolved = self.resolve_ref(target)?;
        if matches!(
            action,
            ComputerActionKind::SpreadsheetLiveBatch(_)
                | ComputerActionKind::DocumentLiveBatch(_)
                | ComputerActionKind::PresentationLiveBatch(_)
        ) {
            return self.execute_iwork_batch_action(resolved, action, ceiling);
        }
        let result = match (resolved, action) {
            (
                ResolvedObject::IworkNumbersCell {
                    document_identity_sha256,
                    sheet_name,
                    table_name,
                    cell_address,
                    before_sha256,
                },
                ComputerActionKind::SpreadsheetLive(action),
            ) => super::macos_iwork_adapter::apply_numbers(
                &super::macos_iwork_adapter::NumbersCellLocator {
                    document_identity_sha256,
                    sheet_name,
                    table_name,
                    cell_address,
                    before_sha256,
                },
                action,
            )?,
            (
                ResolvedObject::IworkPagesDocument {
                    document_identity_sha256,
                    before_sha256,
                },
                ComputerActionKind::DocumentLive(action),
            ) => super::macos_iwork_adapter::apply_pages(
                &super::macos_iwork_adapter::PagesDocumentLocator {
                    document_identity_sha256,
                    before_sha256,
                },
                action,
            )?,
            (
                ResolvedObject::IworkKeynoteSlide {
                    document_identity_sha256,
                    slide_number,
                    title_before_sha256,
                    notes_before_sha256,
                },
                ComputerActionKind::PresentationLive(action),
            ) => super::macos_iwork_adapter::apply_keynote(
                &super::macos_iwork_adapter::KeynoteSlideLocator {
                    document_identity_sha256,
                    slide_number,
                    title_before_sha256,
                    notes_before_sha256,
                },
                action,
            )?,
            _ => {
                return Err(error(
                    AgentErrorKind::InvalidInput,
                    "the iWork action target became incompatible",
                    false,
                ));
            }
        };
        Ok(SemanticActionResult {
            changed: result.changed,
            verified: result.verified,
            summary: result.summary,
            output: None,
        })
    }

    #[cfg(target_os = "macos")]
    fn execute_iwork_batch_action(
        &self,
        resolved: ResolvedObject,
        action: &ComputerActionKind,
        ceiling: &ComputerUseSettings,
    ) -> Result<SemanticActionResult, AgentError> {
        use super::macos_iwork_adapter::{IworkBatchExportFormat, IworkBatchOutput};

        let (result, published) = match (resolved, action) {
            (
                ResolvedObject::IworkNumbersBatch {
                    source_file,
                    source_sha256,
                    source_byte_len,
                    document_identity_sha256,
                    sheet_name,
                    table_name,
                    cell_address,
                    before_sha256,
                },
                ComputerActionKind::SpreadsheetLiveBatch(batch),
            ) => {
                let verified = verify_batch_source(
                    &source_file,
                    &[".numbers"],
                    &source_sha256,
                    source_byte_len,
                )?;
                let stage = super::file_reference_store::prepare_native_artifact_stage(
                    &batch.output.destination_parent,
                    &ceiling.allowed_file_roots,
                    &batch.output.native_file_name,
                    ".numbers",
                    ".xlsx",
                )?;
                let result = super::macos_iwork_adapter::apply_numbers_batch(
                    &verified.path,
                    &IworkBatchOutput {
                        native_path: &stage.native_path,
                        export: Some((
                            IworkBatchExportFormat::MicrosoftOffice,
                            &stage.validation_path,
                        )),
                    },
                    &super::macos_iwork_adapter::NumbersCellLocator {
                        document_identity_sha256,
                        sheet_name,
                        table_name,
                        cell_address,
                        before_sha256,
                    },
                    &batch.action,
                )?;
                super::file_reference_store::revalidate_verified_native_file(
                    &source_file,
                    &verified,
                    128 * 1024 * 1024,
                )?;
                let published = result
                    .verified
                    .then(|| stage.publish(b"PK\x03\x04", b"PK\x03\x04"))
                    .transpose()?;
                (result, published)
            }
            (
                ResolvedObject::IworkPagesBatch {
                    source_file,
                    source_sha256,
                    source_byte_len,
                    document_identity_sha256,
                    before_sha256,
                },
                ComputerActionKind::DocumentLiveBatch(batch),
            ) => {
                let verified = verify_batch_source(
                    &source_file,
                    &[".pages"],
                    &source_sha256,
                    source_byte_len,
                )?;
                let stage = super::file_reference_store::prepare_native_artifact_stage(
                    &batch.output.destination_parent,
                    &ceiling.allowed_file_roots,
                    &batch.output.native_file_name,
                    ".pages",
                    ".pdf",
                )?;
                let result = super::macos_iwork_adapter::apply_pages_batch(
                    &verified.path,
                    &IworkBatchOutput {
                        native_path: &stage.native_path,
                        export: Some((IworkBatchExportFormat::Pdf, &stage.validation_path)),
                    },
                    &super::macos_iwork_adapter::PagesDocumentLocator {
                        document_identity_sha256,
                        before_sha256,
                    },
                    &batch.action,
                )?;
                super::file_reference_store::revalidate_verified_native_file(
                    &source_file,
                    &verified,
                    128 * 1024 * 1024,
                )?;
                let published = result
                    .verified
                    .then(|| stage.publish(b"PK\x03\x04", b"%PDF"))
                    .transpose()?;
                (result, published)
            }
            (
                ResolvedObject::IworkKeynoteBatch {
                    source_file,
                    source_sha256,
                    source_byte_len,
                    document_identity_sha256,
                    slide_number,
                    title_before_sha256,
                    notes_before_sha256,
                },
                ComputerActionKind::PresentationLiveBatch(batch),
            ) => {
                let verified =
                    verify_batch_source(&source_file, &[".key"], &source_sha256, source_byte_len)?;
                let stage = super::file_reference_store::prepare_native_artifact_stage(
                    &batch.output.destination_parent,
                    &ceiling.allowed_file_roots,
                    &batch.output.native_file_name,
                    ".key",
                    ".pdf",
                )?;
                let result = super::macos_iwork_adapter::apply_keynote_batch(
                    &verified.path,
                    &IworkBatchOutput {
                        native_path: &stage.native_path,
                        export: Some((IworkBatchExportFormat::Pdf, &stage.validation_path)),
                    },
                    &super::macos_iwork_adapter::KeynoteSlideLocator {
                        document_identity_sha256,
                        slide_number,
                        title_before_sha256,
                        notes_before_sha256,
                    },
                    &batch.action,
                )?;
                super::file_reference_store::revalidate_verified_native_file(
                    &source_file,
                    &verified,
                    128 * 1024 * 1024,
                )?;
                let published = result
                    .verified
                    .then(|| stage.publish(b"PK\x03\x04", b"%PDF"))
                    .transpose()?;
                (result, published)
            }
            _ => {
                return Err(error(
                    AgentErrorKind::InvalidInput,
                    "the iWork batch action target became incompatible",
                    false,
                ));
            }
        };
        let output = published.map(|published| {
            ComputerActionOutput::BatchDocumentArtifact(BatchDocumentArtifact {
                file: published.object_ref,
                file_name: published.file_name,
                byte_len: published.byte_len,
                sha256: published.sha256,
                validation_byte_len: published.validation_byte_len,
                validation_sha256: published.validation_sha256,
            })
        });
        Ok(SemanticActionResult {
            changed: result.changed,
            verified: result.verified,
            summary: result.summary,
            output,
        })
    }

    pub(crate) fn office_document_filter(
        &self,
        params: &OfficeInspectParams,
        ceiling: &ComputerUseSettings,
    ) -> Result<Option<String>, AgentError> {
        ensure_observation_enabled(ceiling)?;
        if !ceiling.office_semantic {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "Office semantic observation is disabled in device-local settings",
                false,
            ));
        }
        if !params.selection_only
            || params.max_objects == 0
            || params.max_objects > 16
            || params.max_bytes < 1024
            || params.max_bytes > 256 * 1024
        {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "Office selection inspection bounds exceed the device ceiling",
                false,
            ));
        }
        match params.document.as_ref() {
            None => Ok(None),
            Some(reference) if reference.object_kind == ObjectKind::OfficeDocument => {
                match self.resolve_ref(reference)? {
                    ResolvedObject::OfficeDocument { document_url_hash } => {
                        Ok(Some(document_url_hash))
                    }
                    _ => Err(error(
                        AgentErrorKind::InvalidInput,
                        "Office document reference has an incompatible target",
                        false,
                    )),
                }
            }
            Some(_) => Err(error(
                AgentErrorKind::InvalidInput,
                "Office inspection requires an Office document reference",
                false,
            )),
        }
    }

    pub(crate) fn project_excel_selection(
        &self,
        params: &OfficeInspectParams,
        ceiling: &ComputerUseSettings,
        observed: super::office_bridge_observer::ExcelSelectionObservation,
    ) -> Result<OfficeInspectOutput, AgentError> {
        let expected_document = self.office_document_filter(params, ceiling)?;
        if expected_document
            .as_deref()
            .is_some_and(|expected| expected != observed.document_url_hash)
        {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "the paired Excel document changed during observation",
                false,
            ));
        }
        let desktop = observe_interactive_desktop()?;
        let snapshot_id = self.next_snapshot_id();
        let incarnation = format!(
            "{}:{}",
            desktop.session_id,
            self.current_incarnation_nonce()
        );
        let worksheet_name = observed
            .address
            .split_once('!')
            .map(|(worksheet, _)| worksheet.trim_matches('\'').to_string())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                error(
                    AgentErrorKind::TransportError,
                    "the Excel selection address has no worksheet identity",
                    true,
                )
            })?;
        let document = self.issue_ref(
            &snapshot_id,
            &incarnation,
            ObjectKind::OfficeDocument,
            ResolvedObject::OfficeDocument {
                document_url_hash: observed.document_url_hash.clone(),
            },
        )?;
        let worksheet = self.issue_ref(
            &snapshot_id,
            &incarnation,
            ObjectKind::Worksheet,
            ResolvedObject::Worksheet {
                document_url_hash: observed.document_url_hash.clone(),
                name: worksheet_name,
            },
        )?;
        let range = self.issue_ref(
            &snapshot_id,
            &incarnation,
            ObjectKind::Range,
            ResolvedObject::Range {
                document_url_hash: observed.document_url_hash,
                address: observed.address.clone(),
            },
        )?;
        let has_formulas = observed.cells.iter().any(|cell| cell.formula.is_some());
        let mut output = OfficeInspectOutput {
            snapshot_id,
            adapter: ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::OfficeExcel,
                version: "office-js-bridge-read/v1".into(),
            },
            selection: OfficeSelectionProjection::Excel {
                document,
                worksheet,
                range,
                address: observed.address,
                row_count: observed.row_count,
                column_count: observed.column_count,
                has_formulas,
                cells: observed.cells,
            },
            truncated: false,
        };
        loop {
            let encoded_len = serde_json::to_vec(&output)
                .map_err(|_| {
                    error(
                        AgentErrorKind::Internal,
                        "cannot encode the Excel selection projection",
                        true,
                    )
                })?
                .len();
            if encoded_len <= params.max_bytes as usize {
                return Ok(output);
            }
            let OfficeSelectionProjection::Excel { cells, .. } = &mut output.selection else {
                unreachable!("the Windows Office bridge only projects Excel here")
            };
            if cells.pop().is_none() {
                return Err(error(
                    AgentErrorKind::OutputLimitExceeded,
                    "the Excel selection byte budget is too small for its response envelope",
                    false,
                ));
            }
            output.truncated = true;
        }
    }

    pub fn inspect_desktop_ui(
        &self,
        params: &UiInspectParams,
        ceiling: &ComputerUseSettings,
    ) -> Result<UiInspectOutput, AgentError> {
        ensure_observation_enabled(ceiling)?;
        if params.max_depth == 0
            || params.max_depth > MAX_UI_INSPECT_DEPTH
            || params.max_nodes == 0
            || params.max_nodes > MAX_COMPUTER_USE_INSPECT_NODES
            || params.max_bytes == 0
            || params.max_bytes > MAX_COMPUTER_USE_INSPECT_BYTES
        {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "desktop UI inspection bounds exceed the device ceiling",
                false,
            ));
        }
        let resolved_root = if let Some(root) = params.root.as_ref() {
            match root.object_kind {
                ObjectKind::DesktopSession | ObjectKind::Application | ObjectKind::Window => {}
                _ => {
                    return Err(error(
                        AgentErrorKind::InvalidInput,
                        "desktop UI inspection root has an incompatible object kind",
                        false,
                    ));
                }
            }
            Some(self.resolve_ref(root)?)
        } else {
            None
        };

        let observed = observe_interactive_desktop()?;
        let application = observed.foreground_application.ok_or_else(|| {
            error(
                AgentErrorKind::SessionUnavailable,
                "the interactive desktop has no foreground application",
                true,
            )
        })?;
        if !ceiling.application_allowed(&application.image_path) {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "the foreground application is not in the device-local Computer Use allowlist",
                false,
            ));
        }
        match resolved_root {
            Some(ResolvedObject::DesktopSession { session_id })
                if session_id == observed.session_id => {}
            Some(ResolvedObject::Application {
                window_handle,
                process_id,
                image_path,
                process_started_at,
            }) if window_handle == application.window_handle
                && process_id == application.process_id
                && path_eq(&image_path, &application.image_path)
                && process_started_at == application.process_started_at => {}
            None => {}
            Some(_) => {
                return Err(error(
                    AgentErrorKind::InvalidInput,
                    "the UI inspection root is stale or is not the foreground application",
                    false,
                ));
            }
        }

        #[cfg(windows)]
        let (collected, adapter_kind, adapter_version, adapter_name) = (
            super::windows_uia_observer::collect_foreground(
                application.process_id,
                &application.image_path,
                params.max_depth,
                params.max_nodes,
                params.max_bytes,
            )?,
            ComputerUseAdapterKind::WindowsUia,
            "a4-windows-uia-read/v1",
            "Windows UI Automation",
        );
        #[cfg(target_os = "macos")]
        let (collected, adapter_kind, adapter_version, adapter_name) = (
            super::macos_accessibility_observer::collect_foreground(
                application.process_id,
                &application.image_path,
                params.max_depth,
                params.max_nodes,
                params.max_bytes,
            )?,
            ComputerUseAdapterKind::MacosAccessibility,
            "macos-accessibility-read/v1",
            "macOS Accessibility",
        );
        #[cfg(not(any(windows, target_os = "macos")))]
        return Err(error(
            AgentErrorKind::UnsupportedPlatform,
            "semantic desktop UI inspection is unavailable on this platform",
            false,
        ));

        let snapshot_id = self.next_snapshot_id();
        let incarnation = format!(
            "{}:{}",
            observed.session_id,
            self.current_incarnation_nonce()
        );
        let mut nodes = Vec::with_capacity(collected.nodes.len());
        // Reserve a conservative envelope budget for snapshot/adapter JSON;
        // each projection is measured exactly before insertion.
        let mut encoded_bytes = 512usize;
        let mut truncated = collected.truncated;
        for node in collected.nodes {
            let object_ref = self.issue_ref(
                &snapshot_id,
                &incarnation,
                ObjectKind::UiElement,
                ResolvedObject::UiElement {
                    process_id: application.process_id,
                    image_path: application.image_path.clone(),
                    fingerprint: node.fingerprint,
                },
            )?;
            let projection = UiNodeProjection {
                object_ref,
                parent_index: node.parent_index,
                role: node.role,
                name: node.name,
                value: node.value,
                is_protected: node.is_protected,
                enabled: node.enabled,
                supported_actions: node.supported_actions,
            };
            let projection_bytes = serde_json::to_vec(&projection).map_err(|_| {
                error(
                    AgentErrorKind::Internal,
                    &format!("cannot encode a {adapter_name} projection"),
                    true,
                )
            })?;
            let additional = projection_bytes.len() + usize::from(!nodes.is_empty());
            if encoded_bytes.saturating_add(additional) > params.max_bytes as usize {
                truncated = true;
                break;
            }
            encoded_bytes += additional;
            nodes.push(projection);
        }
        let mut output = UiInspectOutput {
            snapshot_id,
            adapter: ComputerUseAdapterRef {
                kind: adapter_kind,
                version: adapter_version.into(),
            },
            nodes,
            truncated,
        };
        loop {
            let encoded_len = serde_json::to_vec(&output)
                .map_err(|_| {
                    error(
                        AgentErrorKind::Internal,
                        &format!("cannot encode the {adapter_name} result"),
                        true,
                    )
                })?
                .len();
            if encoded_len <= params.max_bytes as usize {
                return Ok(output);
            }
            if output.nodes.pop().is_none() {
                return Err(error(
                    AgentErrorKind::OutputLimitExceeded,
                    "the desktop UI inspection byte budget is too small for its response envelope",
                    false,
                ));
            }
            output.truncated = true;
        }
    }

    /// Browser or local-human input invalidates every UI/object snapshot before
    /// the input is injected. The read-only paired Office document identity is
    /// retained: an Office read resolves it and independently rechecks the current
    /// bridge document hash, while discarding it here creates a readiness-cache
    /// race immediately after the owner opens or uses the task pane. A later
    /// mutation still cannot reuse a Worksheet, Range, or UI-element ObjectRef
    /// observed before a human changed the UI. AI adapter input will use a
    /// separate marked path when mutation is implemented.
    pub fn note_browser_input(&self) {
        self.note_user_input(InputPreemptionSource::Browser);
    }

    pub fn note_external_input(&self) {
        self.note_user_input(InputPreemptionSource::LocalExternal);
    }

    pub fn acquire_writer_lease(
        &self,
        request: WriterLeaseRequest,
    ) -> Result<WriterLeaseState, AgentError> {
        let active_incarnation = self
            .active_session_incarnation
            .lock()
            .map_err(|_| {
                error(
                    AgentErrorKind::Internal,
                    "Computer Use active session state is unavailable",
                    true,
                )
            })?
            .clone();
        if active_incarnation.as_deref() != Some(request.interactive_session_incarnation.as_str()) {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "Computer Use writer lease targets a stale interactive session incarnation",
                false,
            ));
        }
        self.writer_lease
            .acquire(request, self.human_input_epoch.load(Ordering::SeqCst))
    }

    pub fn require_writer_lease(
        &self,
        execution_generation: &str,
    ) -> Result<WriterLeaseState, AgentError> {
        let state = self.writer_lease.require_active(
            execution_generation,
            self.human_input_epoch.load(Ordering::SeqCst),
        )?;
        let active_incarnation = self
            .active_session_incarnation
            .lock()
            .map_err(|_| {
                error(
                    AgentErrorKind::Internal,
                    "Computer Use active session state is unavailable",
                    true,
                )
            })?
            .clone();
        if active_incarnation.as_deref()
            != Some(state.request.interactive_session_incarnation.as_str())
        {
            return Err(error(
                AgentErrorKind::Cancelled,
                "Computer Use writer lease was fenced by an interactive session change",
                false,
            ));
        }
        Ok(state)
    }

    pub fn cancel_writer_lease(&self, execution_generation: &str) -> bool {
        self.writer_lease.cancel(execution_generation)
    }

    pub fn release_writer_lease(&self, execution_generation: &str) -> bool {
        self.writer_lease.release(execution_generation)
    }

    fn note_user_input(&self, source: InputPreemptionSource) {
        self.human_input_epoch.fetch_add(1, Ordering::SeqCst);
        self.writer_lease.preempt(source);
        if let Ok(mut objects) = self.objects.lock() {
            let before = objects.len();
            objects.retain(|_, object| {
                matches!(&object.resolved, ResolvedObject::OfficeDocument { .. })
            });
            log::debug!(
                "[computer-use-ref] user input source={source:?} epoch={} retained={} removed={}",
                self.human_input_epoch.load(Ordering::SeqCst),
                objects.len(),
                before.saturating_sub(objects.len()),
            );
        }
    }

    fn update_active_session_incarnation(&self, next: Option<String>) {
        let changed_from_live = if let Ok(mut active) = self.active_session_incarnation.lock() {
            let changed = active.is_some() && *active != next;
            *active = next;
            changed
        } else {
            return;
        };
        if !changed_from_live {
            return;
        }
        self.human_input_epoch.fetch_add(1, Ordering::SeqCst);
        self.writer_lease
            .preempt(InputPreemptionSource::LocalExternal);
        if let Ok(mut objects) = self.objects.lock() {
            objects.clear();
        }
    }

    /// Advance the worker incarnation while preserving the shared broker handle
    /// used by portable daemon-side reads. Every prior ObjectRef becomes invalid
    /// before a replacement in-process worker starts.
    pub(crate) fn reset_worker_incarnation(&self) {
        self.set_input_ownership_ready(false);
        if let Ok(mut active) = self.active_session_incarnation.lock() {
            *active = None;
        }
        self.worker_generation.fetch_add(1, Ordering::SeqCst);
        self.snapshot_counter.store(0, Ordering::SeqCst);
        self.human_input_epoch.fetch_add(1, Ordering::SeqCst);
        self.writer_lease
            .preempt(InputPreemptionSource::LocalExternal);
        if let Ok(mut objects) = self.objects.lock() {
            objects.clear();
        }
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn human_input_epoch(&self) -> u64 {
        self.human_input_epoch.load(Ordering::SeqCst)
    }

    fn next_snapshot_id(&self) -> String {
        let sequence = self.snapshot_counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{}:{sequence}", self.current_incarnation_nonce())
    }

    fn current_incarnation_nonce(&self) -> String {
        format!(
            "{}:{}",
            self.incarnation_nonce,
            self.worker_generation.load(Ordering::SeqCst)
        )
    }

    fn issue_ref(
        &self,
        snapshot_id: &str,
        incarnation: &str,
        object_kind: ObjectKind,
        resolved: ResolvedObject,
    ) -> Result<ObjectRef, AgentError> {
        let token = uuid::Uuid::new_v4().to_string();
        let expires_at = Utc::now() + Duration::seconds(OBJECT_REF_TTL_SECS);
        let object_ref = ObjectRef {
            token: token.clone(),
            snapshot_id: snapshot_id.to_string(),
            object_kind,
            expires_at: expires_at.to_rfc3339(),
        };
        let mut objects = self.objects.lock().map_err(|_| {
            error(
                AgentErrorKind::Internal,
                "Computer Use object store is unavailable",
                true,
            )
        })?;
        objects.retain(|_, object| object.expires_at > Utc::now());
        if objects.len() >= MAX_OBJECT_REFS {
            return Err(error(
                AgentErrorKind::OutputLimitExceeded,
                "Computer Use object reference store reached its bounded capacity",
                true,
            ));
        }
        objects.insert(
            token,
            StoredObject {
                snapshot_id: snapshot_id.to_string(),
                object_kind,
                expires_at,
                incarnation: incarnation.to_string(),
                resolved,
            },
        );
        log::debug!(
            "[computer-use-ref] issued snapshot={snapshot_id} kind={object_kind:?} store_size={}",
            objects.len(),
        );
        Ok(object_ref)
    }

    pub(crate) fn resolve_ref(&self, object_ref: &ObjectRef) -> Result<ResolvedObject, AgentError> {
        let mut objects = self.objects.lock().map_err(|_| {
            error(
                AgentErrorKind::Internal,
                "Computer Use object store is unavailable",
                true,
            )
        })?;
        let now = Utc::now();
        objects.retain(|_, object| object.expires_at > now);
        let Some(stored) = objects.get(&object_ref.token) else {
            log::debug!(
                "[computer-use-ref] missing snapshot={} kind={:?} store_size={} epoch={}",
                object_ref.snapshot_id,
                object_ref.object_kind,
                objects.len(),
                self.human_input_epoch.load(Ordering::SeqCst),
            );
            return Err(error(
                AgentErrorKind::InvalidInput,
                "Computer Use object reference is stale or unknown",
                false,
            ));
        };
        if stored.snapshot_id != object_ref.snapshot_id
            || stored.object_kind != object_ref.object_kind
            || stored.expires_at.to_rfc3339() != object_ref.expires_at
            || !object_ref
                .snapshot_id
                .starts_with(&self.current_incarnation_nonce())
            || stored.incarnation.is_empty()
        {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "Computer Use object reference does not belong to this worker incarnation",
                false,
            ));
        }
        Ok(stored.resolved.clone())
    }
}

pub(crate) struct ScreenCapturePermit {
    broker: Arc<ComputerUseBroker>,
}

impl Drop for ScreenCapturePermit {
    fn drop(&mut self) {
        if let Ok(mut gate) = self.broker.screen_capture_gate.lock() {
            gate.in_flight = false;
        }
    }
}

fn validate_screen_selection(
    params: &ScreenCaptureParams,
    selected_display: &str,
) -> Result<(), AgentError> {
    let selected_display = selected_display.trim();
    if selected_display.is_empty() {
        return Err(error(
            AgentErrorKind::PermissionDenied,
            "screen capture requires an owner-selected display",
            false,
        ));
    }
    if let Some(requested) = params.display.as_deref() {
        let requested = requested.trim();
        if requested.is_empty() || !screen_target_eq(requested, selected_display) {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "the requested display does not match the owner-selected display",
                false,
            ));
        }
    }
    Ok(())
}

fn admit_screen_capture(
    gate: &mut ScreenCaptureGateState,
    now: StdInstant,
) -> Result<(), AgentError> {
    if gate.in_flight {
        return Err(error(
            AgentErrorKind::HostAtCapacity,
            "another bounded screen capture is already in progress",
            true,
        ));
    }
    if gate
        .last_started
        .is_some_and(|started| now.duration_since(started) < SCREEN_CAPTURE_MIN_INTERVAL)
    {
        return Err(error(
            AgentErrorKind::HostAtCapacity,
            "screen capture frequency exceeds the device-local bounded rate",
            true,
        ));
    }
    gate.in_flight = true;
    gate.last_started = Some(now);
    Ok(())
}

#[cfg(windows)]
fn screen_target_eq(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

#[cfg(not(windows))]
fn screen_target_eq(left: &str, right: &str) -> bool {
    left == right
}

fn ensure_screen_capture_safe() -> Result<(), AgentError> {
    let observed = observe_interactive_desktop()?;
    let Some(application) = observed.foreground_application else {
        return Err(error(
            AgentErrorKind::SessionUnavailable,
            "screen capture requires a visible foreground application",
            true,
        ));
    };
    if screen_capture_application_blocked(&application.image_path) {
        return Err(error(
            AgentErrorKind::PermissionDenied,
            "the foreground application is blocked from screen capture",
            false,
        ));
    }
    #[cfg(windows)]
    // UIA is an additional sensitive-control detector, not a prerequisite for
    // vision fallback: apps without a UIA tree still remain eligible after the
    // secure-desktop and executable denylist checks above.
    if super::windows_uia_observer::foreground_contains_protected_control(
        application.process_id,
        &application.image_path,
    )
    .unwrap_or(false)
    {
        return Err(error(
            AgentErrorKind::PermissionDenied,
            "the foreground application contains a protected UI control",
            false,
        ));
    }
    #[cfg(target_os = "macos")]
    if super::macos_accessibility_observer::foreground_contains_protected_control(
        application.process_id,
        &application.image_path,
    )
    .unwrap_or(false)
    {
        return Err(error(
            AgentErrorKind::PermissionDenied,
            "the foreground application contains a protected UI control",
            false,
        ));
    }
    Ok(())
}

fn screen_capture_application_blocked(image_path: &str) -> bool {
    let Some(name) = Path::new(image_path)
        .file_name()
        .and_then(|value| value.to_str())
    else {
        return true;
    };
    [
        "consent.exe",
        "credentialuibroker.exe",
        "lockapp.exe",
        "logonui.exe",
        "1password.exe",
        "1password",
        "bitwarden.exe",
        "bitwarden",
        "keepass.exe",
        "keepassxc.exe",
        "keepassxc",
        "loginwindow",
    ]
    .iter()
    .any(|blocked| name.eq_ignore_ascii_case(blocked))
}

#[cfg(target_os = "macos")]
fn iwork_resolved_object(
    observed: &super::macos_iwork_adapter::IworkObservation,
) -> ResolvedObject {
    match observed {
        super::macos_iwork_adapter::IworkObservation::Numbers { locator, .. } => {
            iwork_numbers_resolved(locator)
        }
        super::macos_iwork_adapter::IworkObservation::Pages { locator, .. } => {
            iwork_pages_resolved(locator)
        }
        super::macos_iwork_adapter::IworkObservation::Keynote { locator, .. } => {
            iwork_keynote_resolved(locator)
        }
    }
}

#[cfg(target_os = "macos")]
fn iwork_native_extensions(
    application: super::macos_iwork_adapter::IworkApplication,
) -> &'static [&'static str] {
    match application {
        super::macos_iwork_adapter::IworkApplication::Numbers => &[".numbers"],
        super::macos_iwork_adapter::IworkApplication::Pages => &[".pages"],
        super::macos_iwork_adapter::IworkApplication::Keynote => &[".key"],
    }
}

#[cfg(target_os = "macos")]
fn verify_batch_source(
    source_file: &ObjectRef,
    extensions: &[&str],
    expected_sha256: &str,
    expected_byte_len: u64,
) -> Result<super::file_reference_store::VerifiedNativeFile, AgentError> {
    let verified = super::file_reference_store::resolve_verified_native_file(
        source_file,
        extensions,
        128 * 1024 * 1024,
    )?;
    if verified.sha256 != expected_sha256 || verified.byte_len != expected_byte_len {
        Err(error(
            AgentErrorKind::InvalidInput,
            "selected iWork batch source changed after preview",
            false,
        ))
    } else {
        Ok(verified)
    }
}

#[cfg(target_os = "macos")]
fn iwork_batch_resolved(
    source_file: &ObjectRef,
    source: &super::file_reference_store::VerifiedNativeFile,
    observed: &super::macos_iwork_adapter::IworkObservation,
) -> ResolvedObject {
    match observed {
        super::macos_iwork_adapter::IworkObservation::Numbers { locator, .. } => {
            ResolvedObject::IworkNumbersBatch {
                source_file: source_file.clone(),
                source_sha256: source.sha256.clone(),
                source_byte_len: source.byte_len,
                document_identity_sha256: locator.document_identity_sha256.clone(),
                sheet_name: locator.sheet_name.clone(),
                table_name: locator.table_name.clone(),
                cell_address: locator.cell_address.clone(),
                before_sha256: locator.before_sha256.clone(),
            }
        }
        super::macos_iwork_adapter::IworkObservation::Pages { locator, .. } => {
            ResolvedObject::IworkPagesBatch {
                source_file: source_file.clone(),
                source_sha256: source.sha256.clone(),
                source_byte_len: source.byte_len,
                document_identity_sha256: locator.document_identity_sha256.clone(),
                before_sha256: locator.before_sha256.clone(),
            }
        }
        super::macos_iwork_adapter::IworkObservation::Keynote { locator, .. } => {
            ResolvedObject::IworkKeynoteBatch {
                source_file: source_file.clone(),
                source_sha256: source.sha256.clone(),
                source_byte_len: source.byte_len,
                document_identity_sha256: locator.document_identity_sha256.clone(),
                slide_number: locator.slide_number,
                title_before_sha256: locator.title_before_sha256.clone(),
                notes_before_sha256: locator.notes_before_sha256.clone(),
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn split_iwork_readiness(
    result: Result<Option<ObjectRef>, AgentError>,
) -> (Option<ObjectRef>, Option<AgentErrorKind>) {
    match result {
        Ok(object_ref) => (object_ref, None),
        Err(error) => (None, Some(error.kind)),
    }
}

#[cfg(target_os = "macos")]
fn iwork_numbers_resolved(
    locator: &super::macos_iwork_adapter::NumbersCellLocator,
) -> ResolvedObject {
    ResolvedObject::IworkNumbersCell {
        document_identity_sha256: locator.document_identity_sha256.clone(),
        sheet_name: locator.sheet_name.clone(),
        table_name: locator.table_name.clone(),
        cell_address: locator.cell_address.clone(),
        before_sha256: locator.before_sha256.clone(),
    }
}

#[cfg(target_os = "macos")]
fn iwork_pages_resolved(
    locator: &super::macos_iwork_adapter::PagesDocumentLocator,
) -> ResolvedObject {
    ResolvedObject::IworkPagesDocument {
        document_identity_sha256: locator.document_identity_sha256.clone(),
        before_sha256: locator.before_sha256.clone(),
    }
}

#[cfg(target_os = "macos")]
fn iwork_keynote_resolved(
    locator: &super::macos_iwork_adapter::KeynoteSlideLocator,
) -> ResolvedObject {
    ResolvedObject::IworkKeynoteSlide {
        document_identity_sha256: locator.document_identity_sha256.clone(),
        slide_number: locator.slide_number,
        title_before_sha256: locator.title_before_sha256.clone(),
        notes_before_sha256: locator.notes_before_sha256.clone(),
    }
}

fn ensure_observation_enabled(ceiling: &ComputerUseSettings) -> Result<(), AgentError> {
    if ceiling.observation_enabled() {
        Ok(())
    } else {
        Err(error(
            AgentErrorKind::PermissionDenied,
            "Computer Use observation is disabled in device-local settings",
            false,
        ))
    }
}

pub(super) struct ObservedDesktop {
    pub(super) session_id: u32,
    pub(super) foreground_application: Option<ObservedApplication>,
}

pub(super) struct ObservedApplication {
    pub(super) window_handle: isize,
    pub(super) process_id: u32,
    pub(super) image_path: String,
    pub(super) process_started_at: Option<u64>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(super) struct CollectedUiNode {
    pub(super) parent_index: Option<u32>,
    pub(super) role: String,
    pub(super) name: Option<String>,
    pub(super) value: Option<String>,
    pub(super) is_protected: bool,
    pub(super) enabled: bool,
    pub(super) supported_actions: Vec<desk_agent_protocol::computer_use::UiSemanticActionKind>,
    pub(super) fingerprint: String,
}

pub(super) struct CollectedUiTree {
    pub(super) nodes: Vec<CollectedUiNode>,
    pub(super) truncated: bool,
}

#[cfg(windows)]
fn observe_interactive_desktop() -> Result<ObservedDesktop, AgentError> {
    use std::mem::size_of;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_READOBJECTS, GetUserObjectInformationW, OpenInputDesktop, UOI_NAME,
    };
    use windows::Win32::System::Threading::GetCurrentProcessId;

    let mut session_id = 0_u32;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id) }.map_err(|_| {
        error(
            AgentErrorKind::SessionUnavailable,
            "cannot resolve the current Windows session",
            true,
        )
    })?;
    if session_id == 0 {
        return Err(error(
            AgentErrorKind::SessionUnavailable,
            "session 0 is not an interactive Computer Use target",
            false,
        ));
    }
    let desktop = unsafe { OpenInputDesktop(Default::default(), false, DESKTOP_READOBJECTS) }
        .map_err(|_| {
            error(
                AgentErrorKind::SessionUnavailable,
                "the Windows input desktop is unavailable",
                true,
            )
        })?;
    let mut buffer = vec![0_u16; 256];
    let mut needed = 0_u32;
    let desktop_name_result = unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(buffer.as_mut_ptr().cast()),
            (buffer.len() * size_of::<u16>()) as u32,
            Some(&mut needed),
        )
    };
    let close_result = unsafe { CloseDesktop(desktop) };
    desktop_name_result.map_err(|_| {
        error(
            AgentErrorKind::SessionUnavailable,
            "cannot identify the Windows input desktop",
            true,
        )
    })?;
    close_result.map_err(|_| {
        error(
            AgentErrorKind::Internal,
            "cannot close the Windows input desktop handle",
            true,
        )
    })?;
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    let desktop_name = String::from_utf16_lossy(&buffer[..length]);
    if !desktop_name.eq_ignore_ascii_case("Default") {
        return Err(error(
            AgentErrorKind::SessionUnavailable,
            "secure or non-default Windows input desktop is not available to Computer Use",
            false,
        ));
    }

    let foreground_application = super::windows_uia_observer::resolve_foreground_application()
        .ok()
        .map(|application| ObservedApplication {
            window_handle: application.window_handle,
            process_id: application.process_id,
            image_path: application.image_path,
            process_started_at: Some(application.process_started_at),
        });
    return Ok(ObservedDesktop {
        session_id,
        foreground_application,
    });
}

#[cfg(target_os = "macos")]
fn observe_interactive_desktop() -> Result<ObservedDesktop, AgentError> {
    super::macos_accessibility_observer::observe_interactive_desktop()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn observe_interactive_desktop() -> Result<ObservedDesktop, AgentError> {
    Err(error(
        AgentErrorKind::UnsupportedPlatform,
        "Computer Use desktop observation is currently available only on Windows",
        false,
    ))
}

fn error(kind: AgentErrorKind, message: &str, retryable: bool) -> AgentError {
    AgentError {
        kind,
        message: message.to_string(),
        retryable,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(windows)]
fn path_eq(left: &str, right: &str) -> bool {
    left.replace('/', "\\")
        .eq_ignore_ascii_case(&right.replace('/', "\\"))
}

#[cfg(not(windows))]
fn path_eq(left: &str, right: &str) -> bool {
    left == right
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> ComputerUseSettings {
        ComputerUseSettings {
            enabled: true,
            observe: true,
            ..Default::default()
        }
    }

    #[test]
    fn observation_is_disabled_by_default() {
        let broker = ComputerUseBroker::new();
        let error = broker
            .inspect_desktop_session(
                &DesktopSessionInspectParams {
                    include_active_application: false,
                },
                &ComputerUseSettings::default(),
            )
            .unwrap_err();
        assert_eq!(error.kind, AgentErrorKind::PermissionDenied);
    }

    #[test]
    fn ui_bounds_fail_before_the_adapter_is_consulted() {
        let broker = ComputerUseBroker::new();
        let error = broker
            .inspect_desktop_ui(
                &UiInspectParams {
                    root: None,
                    max_depth: MAX_UI_INSPECT_DEPTH + 1,
                    max_nodes: 1,
                    max_bytes: 1,
                },
                &enabled(),
            )
            .unwrap_err();
        assert_eq!(error.kind, AgentErrorKind::InvalidInput);
    }

    #[cfg(not(windows))]
    #[test]
    fn application_paths_use_exact_platform_comparison() {
        assert!(path_eq(
            "/Applications/Calculator.app",
            "/Applications/Calculator.app"
        ));
        assert!(!path_eq(
            "/Applications/Calculator.app",
            "/applications/calculator.app"
        ));
    }

    #[test]
    fn reference_from_another_worker_incarnation_is_rejected() {
        let first = ComputerUseBroker::new();
        let second = ComputerUseBroker::new();
        let reference = first
            .issue_ref(
                &first.next_snapshot_id(),
                "test-incarnation",
                ObjectKind::DesktopSession,
                ResolvedObject::DesktopSession { session_id: 1 },
            )
            .unwrap();
        let error = second.resolve_ref(&reference).unwrap_err();
        assert_eq!(error.kind, AgentErrorKind::InvalidInput);
    }

    #[test]
    fn resetting_shared_broker_advances_incarnation_and_invalidates_refs() {
        let broker = ComputerUseBroker::new();
        let before = broker
            .issue_ref(
                &broker.next_snapshot_id(),
                "test-incarnation",
                ObjectKind::OfficeDocument,
                ResolvedObject::OfficeDocument {
                    document_url_hash: "document-hash".into(),
                },
            )
            .unwrap();
        broker.set_input_ownership_ready(true);
        *broker.active_session_incarnation.lock().unwrap() = Some("session-before-respawn".into());
        broker
            .acquire_writer_lease(WriterLeaseRequest {
                work_id: "work-before-respawn".into(),
                action_request_id: "action-before-respawn".into(),
                execution_generation: "generation-before-respawn".into(),
                interactive_session_incarnation: "session-before-respawn".into(),
                expires_at: Utc::now() + Duration::seconds(30),
            })
            .unwrap();

        broker.reset_worker_incarnation();
        let after_snapshot = broker.next_snapshot_id();

        assert_ne!(before.snapshot_id, after_snapshot);
        assert!(broker.resolve_ref(&before).is_err());
        assert!(!broker.input_ownership_is_ready());
        assert!(broker.active_session_incarnation.lock().unwrap().is_none());
        assert_eq!(
            broker
                .require_writer_lease("generation-before-respawn")
                .unwrap_err()
                .kind,
            AgentErrorKind::Cancelled
        );
    }

    #[test]
    fn tampered_snapshot_is_rejected() {
        let broker = ComputerUseBroker::new();
        let mut reference = broker
            .issue_ref(
                &broker.next_snapshot_id(),
                "test-incarnation",
                ObjectKind::DesktopSession,
                ResolvedObject::DesktopSession { session_id: 1 },
            )
            .unwrap();
        reference.snapshot_id.push_str("-tampered");
        let error = broker.resolve_ref(&reference).unwrap_err();
        assert_eq!(error.kind, AgentErrorKind::InvalidInput);
    }

    #[test]
    fn writer_lease_requires_the_latest_device_session_incarnation() {
        let broker = ComputerUseBroker::new();
        *broker.active_session_incarnation.lock().unwrap() = Some("session-current".into());
        let request = WriterLeaseRequest {
            work_id: "work".into(),
            action_request_id: "action".into(),
            execution_generation: "generation".into(),
            interactive_session_incarnation: "session-stale".into(),
            expires_at: Utc::now() + Duration::seconds(30),
        };
        let error = broker.acquire_writer_lease(request.clone()).unwrap_err();
        assert_eq!(error.kind, AgentErrorKind::InvalidInput);

        let current = WriterLeaseRequest {
            interactive_session_incarnation: "session-current".into(),
            ..request
        };
        broker.acquire_writer_lease(current).unwrap();
    }

    #[test]
    fn live_session_change_preempts_writer_and_invalidates_every_reference() {
        let broker = ComputerUseBroker::new();
        broker.update_active_session_incarnation(Some("session-a".into()));
        let reference = broker
            .issue_ref(
                &broker.next_snapshot_id(),
                "session-a",
                ObjectKind::OfficeDocument,
                ResolvedObject::OfficeDocument {
                    document_url_hash: "document-a".into(),
                },
            )
            .unwrap();
        broker
            .acquire_writer_lease(WriterLeaseRequest {
                work_id: "work".into(),
                action_request_id: "action".into(),
                execution_generation: "generation".into(),
                interactive_session_incarnation: "session-a".into(),
                expires_at: Utc::now() + Duration::seconds(30),
            })
            .unwrap();

        broker.update_active_session_incarnation(Some("session-b".into()));

        assert_eq!(
            broker.require_writer_lease("generation").unwrap_err().kind,
            AgentErrorKind::Cancelled
        );
        assert!(broker.resolve_ref(&reference).is_err());
        assert_eq!(
            broker.active_session_incarnation.lock().unwrap().as_deref(),
            Some("session-b")
        );
    }

    #[test]
    fn human_input_invalidates_existing_references() {
        let broker = ComputerUseBroker::new();
        let reference = broker
            .issue_ref(
                &broker.next_snapshot_id(),
                "test-incarnation",
                ObjectKind::DesktopSession,
                ResolvedObject::DesktopSession { session_id: 1 },
            )
            .unwrap();
        assert!(broker.resolve_ref(&reference).is_ok());
        broker.note_browser_input();
        let error = broker.resolve_ref(&reference).unwrap_err();
        assert_eq!(error.kind, AgentErrorKind::InvalidInput);
    }

    #[test]
    fn human_input_preserves_readonly_office_document_identity() {
        let broker = ComputerUseBroker::new();
        let reference = broker
            .issue_ref(
                &broker.next_snapshot_id(),
                "test-incarnation",
                ObjectKind::OfficeDocument,
                ResolvedObject::OfficeDocument {
                    document_url_hash: "document-hash".into(),
                },
            )
            .unwrap();

        broker.note_external_input();

        assert_eq!(
            broker.resolve_ref(&reference).unwrap(),
            ResolvedObject::OfficeDocument {
                document_url_hash: "document-hash".into(),
            }
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn numbers_live_formula_uses_the_frozen_ast_allowlist() {
        let broker = ComputerUseBroker::new();
        let target = broker
            .issue_ref(
                &broker.next_snapshot_id(),
                "test-incarnation",
                ObjectKind::Range,
                ResolvedObject::IworkNumbersCell {
                    document_identity_sha256: "document".into(),
                    sheet_name: "Sheet 1".into(),
                    table_name: "Table 1".into(),
                    cell_address: "A3".into(),
                    before_sha256: "before".into(),
                },
            )
            .unwrap();
        let mut ceiling = enabled();
        ceiling.iwork_semantic = true;
        let valid = ComputerActionKind::SpreadsheetLive(
            desk_agent_protocol::computer_use::SpreadsheetLivePatchAction::SetCellFormula {
                formula: "=SUM(A1:A2)".into(),
            },
        );
        broker
            .preflight_iwork_action(&target, &valid, &ceiling)
            .unwrap();
        let rejected = ComputerActionKind::SpreadsheetLive(
            desk_agent_protocol::computer_use::SpreadsheetLivePatchAction::SetCellFormula {
                formula: "=WEBSERVICE(A1)".into(),
            },
        );
        assert_eq!(
            broker
                .preflight_iwork_action(&target, &rejected, &ceiling)
                .unwrap_err()
                .kind,
            AgentErrorKind::InvalidInput
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn batch_preflight_rejects_source_drift_before_may_have_started() {
        let root =
            std::env::temp_dir().join(format!("lrd-iwork-preflight-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let source_path = root.join("source.numbers");
        std::fs::write(&source_path, b"PK\x03\x04AAAA").unwrap();
        let source_file = super::super::file_reference_store::issue(&source_path).unwrap();
        let destination = super::super::file_reference_store::issue(&root).unwrap();
        let verified = super::super::file_reference_store::resolve_verified_native_file(
            &source_file,
            &[".numbers"],
            128 * 1024 * 1024,
        )
        .unwrap();
        let broker = ComputerUseBroker::new();
        let target = broker
            .issue_ref(
                &broker.next_snapshot_id(),
                "batch-incarnation",
                ObjectKind::Range,
                ResolvedObject::IworkNumbersBatch {
                    source_file,
                    source_sha256: verified.sha256,
                    source_byte_len: verified.byte_len,
                    document_identity_sha256: "document".into(),
                    sheet_name: "Sheet 1".into(),
                    table_name: "Table 1".into(),
                    cell_address: "A1".into(),
                    before_sha256: "before".into(),
                },
            )
            .unwrap();
        let action = ComputerActionKind::SpreadsheetLiveBatch(
            desk_agent_protocol::computer_use::SpreadsheetLiveBatchPatchAction {
                output: desk_agent_protocol::computer_use::BatchDocumentOutput {
                    destination_parent: destination,
                    native_file_name: "copy.numbers".into(),
                },
                action:
                    desk_agent_protocol::computer_use::SpreadsheetLivePatchAction::SetCellValue {
                        value: "42".into(),
                    },
            },
        );
        let mut ceiling = enabled();
        ceiling.iwork_semantic = true;
        ceiling.file_artifact_create = true;
        ceiling.allowed_file_roots = vec![root.to_string_lossy().into_owned()];
        broker
            .preflight_iwork_action(&target, &action, &ceiling)
            .unwrap();

        std::fs::write(&source_path, b"PK\x03\x04BBBB").unwrap();
        let error = broker
            .preflight_iwork_action(&target, &action, &ceiling)
            .unwrap_err();
        assert_eq!(error.kind, AgentErrorKind::InvalidInput);
        assert!(!error.retryable);

        std::fs::remove_file(source_path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires a logged-in macOS session and open documents in all three iWork apps"]
    fn iwork_readiness_issues_exact_refs_for_all_live_providers() {
        let broker = ComputerUseBroker::new();
        let mut ceiling = enabled();
        ceiling.iwork_semantic = true;
        let readiness = broker.readiness(&ceiling, false, false);
        for capability in [
            Capability::SpreadsheetLiveInspect,
            Capability::SpreadsheetLivePatchConfirmed,
            Capability::DocumentLiveInspect,
            Capability::DocumentLivePatchConfirmed,
            Capability::PresentationLiveInspect,
            Capability::PresentationLivePatchConfirmed,
        ] {
            let item = readiness
                .capabilities
                .iter()
                .find(|item| item.capability == capability)
                .unwrap();
            assert!(item.supported && item.ready, "{capability:?}: {item:?}");
        }
        assert_eq!(
            readiness
                .context_references
                .iter()
                .filter(|reference| {
                    matches!(
                        reference.capability,
                        Capability::SpreadsheetLiveInspect
                            | Capability::DocumentLiveInspect
                            | Capability::PresentationLiveInspect
                    )
                })
                .count(),
            3
        );
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn semantic_action_monitor_state_starts_fail_closed_and_is_explicit() {
        let broker = ComputerUseBroker::new();
        assert!(!broker.input_ownership_ready.load(Ordering::SeqCst));

        broker.set_input_ownership_ready(true);
        assert!(broker.input_ownership_ready.load(Ordering::SeqCst));

        broker.set_input_ownership_ready(false);
        assert!(!broker.input_ownership_ready.load(Ordering::SeqCst));
    }

    #[test]
    fn reference_ttl_covers_readiness_cache_and_model_request_windows() {
        const READINESS_VALIDITY_SECS: i64 = 25;
        const MODEL_REQUEST_TIMEOUT_SECS: i64 = 180;

        assert!(
            OBJECT_REF_TTL_SECS > READINESS_VALIDITY_SECS + MODEL_REQUEST_TIMEOUT_SECS,
            "an advertised context reference must survive until the first model tool call"
        );
    }

    #[test]
    fn disabled_observation_keeps_only_independent_diagnostics_and_safe_exec_ready() {
        let broker = ComputerUseBroker::new();
        let readiness = broker.readiness(&ComputerUseSettings::default(), false, false);
        readiness.validate().unwrap();
        assert_eq!(readiness.capabilities.len(), 34);
        assert!(readiness.capabilities.iter().all(|entry| {
            if matches!(
                entry.capability,
                Capability::SystemInfo
                    | Capability::ProcessList
                    | Capability::NetworkPorts
                    | Capability::ServiceStatus
                    | Capability::LogRecent
                    | Capability::ContainerList
                    | Capability::FileMetadataRead
                    | Capability::FileContentRead
                    | Capability::SpreadsheetFileInspect
                    | Capability::SpreadsheetMergePreview
                    | Capability::TerminalOutputRead
            ) {
                entry.ready == cfg!(any(windows, target_os = "macos"))
            } else if entry.capability == Capability::ShellExecConfirmed {
                entry.ready
                    == (cfg!(any(windows, target_os = "macos"))
                        && !crate::exec_shells::available_exec_shells().is_empty())
            } else {
                !entry.ready
            }
        }));
        assert!(readiness.capabilities.iter().all(|entry| {
            matches!(
                entry.capability,
                Capability::SystemInfo
                    | Capability::ProcessList
                    | Capability::NetworkPorts
                    | Capability::ServiceStatus
                    | Capability::LogRecent
                    | Capability::ContainerList
                    | Capability::DesktopSessionInspect
                    | Capability::DesktopUiInspect
                    | Capability::DesktopUiActionConfirmed
                    | Capability::DesktopInputFallbackConfirmed
                    | Capability::OfficeDocumentInspect
                    | Capability::SpreadsheetLiveInspect
                    | Capability::SpreadsheetLivePatchConfirmed
                    | Capability::DocumentLiveInspect
                    | Capability::DocumentLivePatchConfirmed
                    | Capability::PresentationLiveInspect
                    | Capability::PresentationLivePatchConfirmed
                    | Capability::FileMetadataRead
                    | Capability::FileContentRead
                    | Capability::SpreadsheetFileInspect
                    | Capability::SpreadsheetMergePreview
                    | Capability::SpreadsheetWorkbookCreateConfirmed
                    | Capability::SpreadsheetFormulaWorkbookCreateConfirmed
                    | Capability::WordDocumentCreateConfirmed
                    | Capability::FileArtifactCreateConfirmed
                    | Capability::CommunicationLocalDraftCreateConfirmed
                    | Capability::CommunicationOutlookNewHandoffConfirmed
                    | Capability::TerminalOutputRead
                    | Capability::ScreenCaptureCurrent
                    | Capability::ShellExecConfirmed
                    | Capability::BrowserPageObserve
                    | Capability::BrowserPageNavigateConfirmed
                    | Capability::BrowserInputFallbackConfirmed
                    | Capability::BrowserExternalDraftWriteConfirmed
            )
        }));
    }

    #[test]
    fn equivalent_readiness_heartbeats_keep_the_same_revision() {
        let broker = ComputerUseBroker::new();
        let settings = ComputerUseSettings::default();
        let first = broker.readiness(&settings, false, false);
        let second = broker.readiness(&settings, false, false);
        assert_eq!(first.revision, second.revision);
        assert_ne!(first.observed_at, "");
        assert_ne!(second.expires_at, "");
    }

    #[test]
    fn local_ceiling_change_advances_readiness_revision() {
        let broker = ComputerUseBroker::new();
        let first = broker.readiness(&ComputerUseSettings::default(), false, false);
        let changed = ComputerUseSettings {
            revision: 1,
            ..ComputerUseSettings::default()
        };
        let second = broker.readiness(&changed, false, false);
        assert!(second.revision > first.revision);
    }

    #[test]
    fn screen_capture_is_bound_to_the_owner_selected_display() {
        let selected = r"\\.\DISPLAY2";
        validate_screen_selection(&ScreenCaptureParams { display: None }, selected).unwrap();
        validate_screen_selection(
            &ScreenCaptureParams {
                display: Some(selected.into()),
            },
            selected,
        )
        .unwrap();
        assert_eq!(
            validate_screen_selection(&ScreenCaptureParams::default(), "")
                .unwrap_err()
                .kind,
            AgentErrorKind::PermissionDenied
        );
        assert_eq!(
            validate_screen_selection(
                &ScreenCaptureParams {
                    display: Some(r"\\.\DISPLAY1".into()),
                },
                selected,
            )
            .unwrap_err()
            .kind,
            AgentErrorKind::PermissionDenied
        );
    }

    #[test]
    fn screen_capture_gate_rejects_concurrency_and_bounded_frequency() {
        let now = StdInstant::now();
        let mut gate = ScreenCaptureGateState::default();
        admit_screen_capture(&mut gate, now).unwrap();
        assert_eq!(
            admit_screen_capture(&mut gate, now).unwrap_err().kind,
            AgentErrorKind::HostAtCapacity
        );
        gate.in_flight = false;
        assert_eq!(
            admit_screen_capture(&mut gate, now + StdDuration::from_secs(1))
                .unwrap_err()
                .kind,
            AgentErrorKind::HostAtCapacity
        );
        admit_screen_capture(&mut gate, now + SCREEN_CAPTURE_MIN_INTERVAL).unwrap();
    }

    #[test]
    fn screen_capture_blocks_credential_and_password_manager_surfaces() {
        for path in [
            r"C:\Windows\System32\consent.exe",
            r"C:\Windows\SystemApps\CredentialUIBroker.exe",
            r"C:\Program Files\Bitwarden\Bitwarden.exe",
            r"C:\Tools\KeePassXC.exe",
        ] {
            assert!(screen_capture_application_blocked(path), "{path}");
        }
        assert!(!screen_capture_application_blocked(
            r"C:\Windows\System32\notepad.exe"
        ));
        assert!(screen_capture_application_blocked(""));
    }

    #[test]
    fn current_screen_requires_an_explicit_display_selection() {
        let (ready, reason) = screen_capture_readiness(true, true, true, None, true, false);
        assert!(!ready);
        assert_eq!(reason, Some(ComputerUseReadinessReason::NoDisplaySelected));

        let (ready, reason) = screen_capture_readiness(true, true, true, None, true, true);
        assert!(ready);
        assert_eq!(reason, None);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires a non-sensitive foreground application on an interactive Windows desktop"]
    fn live_screen_safety_accepts_a_non_sensitive_foreground_application() {
        ensure_screen_capture_safe().expect("foreground screen safety gate");
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an allowlisted foreground Windows fixture"]
    fn interactive_uia_observation_is_bounded_and_redacts_protected_controls() {
        let image_path = std::env::var("LRD_COMPUTER_USE_TEST_APP")
            .expect("LRD_COMPUTER_USE_TEST_APP must name the foreground fixture executable");
        let ceiling = ComputerUseSettings {
            enabled: true,
            observe: true,
            allowed_application_paths: vec![image_path],
            ..Default::default()
        };
        let broker = ComputerUseBroker::new();
        let desktop = broker
            .inspect_desktop_session(
                &DesktopSessionInspectParams {
                    include_active_application: true,
                },
                &ceiling,
            )
            .unwrap();
        let application = desktop
            .active_application
            .expect("the allowlisted fixture must be the foreground application");
        let output = broker
            .inspect_desktop_ui(
                &UiInspectParams {
                    root: Some(application),
                    max_depth: 8,
                    max_nodes: 256,
                    max_bytes: 1024 * 1024,
                },
                &ceiling,
            )
            .unwrap();
        assert!(!output.nodes.is_empty());
        assert!(output.nodes.len() <= 256);
        assert!(
            serde_json::to_vec(&output).unwrap().len() <= 1024 * 1024,
            "encoded output must remain inside the caller byte ceiling"
        );
        assert!(output.nodes.iter().all(|node| {
            node.object_ref.object_kind == ObjectKind::UiElement
                && (!node.is_protected || (node.name.is_none() && node.value.is_none()))
        }));
        assert!(
            output.nodes.iter().any(|node| node.is_protected),
            "fixture must expose at least one protected control to prove redaction"
        );
    }
}
