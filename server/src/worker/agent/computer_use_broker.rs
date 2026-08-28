//! Worker-lifetime Computer Use observation broker.
//!
//! The broker owns the interactive-session incarnation and opaque ObjectRef
//! store. Restarting a worker constructs a new broker, immediately invalidating
//! every prior reference. A3 exposes only bounded observation; action dispatch
//! remains hard-disabled in the daemon and worker.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Duration, Utc};
use desk_agent_protocol::browser_control::{
    BrowserActionRequest, BrowserActionResult, BrowserAdapterRef, BrowserReadinessReason,
};
use desk_agent_protocol::communication::{
    CommunicationDraftHandoff, OutlookNewComposeHandoffRequest,
};
use desk_agent_protocol::computer_use::{
    COMPUTER_USE_SCHEMA_VERSION, ComputerUseAdapterKind, ComputerUseAdapterRef,
    ComputerUseCapabilityReadiness, ComputerUseContextReference, ComputerUseReadiness,
    ComputerUseReadinessReason, DesktopSessionInspectOutput, DesktopSessionInspectParams,
    MAX_COMPUTER_USE_INSPECT_BYTES, MAX_COMPUTER_USE_INSPECT_NODES, ObjectKind, ObjectRef,
    OfficeInspectOutput, OfficeInspectParams, OfficeSelectionProjection, UiInspectOutput,
    UiInspectParams, UiNodeProjection,
};
use desk_agent_protocol::{AgentError, AgentErrorKind, Capability};
use desk_diagnose_core::device_assistant::{
    CURRENT_SCREEN_ADAPTER_ID, DESKTOP_SESSION_ADAPTER_ID, FILE_ARTIFACT_ADAPTER_ID,
    FILE_WORKSPACE_ADAPTER_ID, OFFICE_EXCEL_ADAPTER_ID, OUTLOOK_NEW_MAILTO_ADAPTER_VERSION,
    SPREADSHEET_FILE_ADAPTER_ID, SYSTEM_COMMAND_ADAPTER_ID, SYSTEM_DIAGNOSTICS_ADAPTER_ID,
    TERMINAL_OUTPUT_ADAPTER_ID, WINDOWS_UIA_ADAPTER_ID, device_assistant_edge_adapter_registry,
};

use crate::model::settings::ComputerUseSettings;

use super::browser_devtools_mcp::{
    BrowserBrokerContext, BrowserDevtoolsBroker, ChromeDevtoolsMcpError,
};
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
        process_id: u32,
        image_path: String,
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

pub struct ComputerUseBroker {
    incarnation_nonce: String,
    worker_generation: AtomicU64,
    snapshot_counter: AtomicU64,
    readiness_revision: AtomicU64,
    readiness_revision_state: Mutex<Option<ReadinessRevisionState>>,
    human_input_epoch: AtomicU64,
    objects: Mutex<HashMap<String, StoredObject>>,
    writer_lease: WriterLeaseCoordinator,
    browser: BrowserDevtoolsBroker,
}

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
            human_input_epoch: AtomicU64::new(0),
            objects: Mutex::new(HashMap::new()),
            writer_lease: WriterLeaseCoordinator::new(),
            browser: BrowserDevtoolsBroker::default(),
        }
    }

    pub async fn refresh_browser_readiness(
        &self,
        device_id: String,
        os_session_id: String,
        enabled: bool,
        interactive_session_unlocked: bool,
    ) {
        self.browser
            .refresh(&BrowserBrokerContext {
                device_id,
                os_session_id,
                enabled,
                interactive_session_unlocked,
            })
            .await;
    }

    pub fn preflight_browser_action(
        &self,
        surface: &ObjectRef,
        request: &BrowserActionRequest,
    ) -> Result<(), ChromeDevtoolsMcpError> {
        self.browser.preflight(surface, request)
    }

    pub async fn execute_browser_action(
        &self,
        surface: &ObjectRef,
        request: &BrowserActionRequest,
    ) -> Result<BrowserActionResult, ChromeDevtoolsMcpError> {
        self.browser.execute(surface, request).await
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
                            process_id: application.process_id,
                            image_path: application.image_path,
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
        let ui_adapter_version = edge_registry
            .adapter(WINDOWS_UIA_ADAPTER_ID)
            .expect("compiled Windows UIA adapter is registered")
            .adapter_version
            .clone();
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
        let platform_supported = cfg!(windows);
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
        let ui_ready = session_ready && !ceiling.allowed_application_paths.is_empty();
        #[cfg(windows)]
        let office_configured = super::office_bridge_observer::configured();
        #[cfg(not(windows))]
        let office_configured = false;
        #[cfg(windows)]
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
        #[cfg(not(windows))]
        let office_document_ref: Option<ObjectRef> = None;
        let office_ready = office_document_ref.is_some();
        let (screen_ready, screen_reason) = screen_capture_readiness(
            ceiling.observation_enabled(),
            platform_supported,
            session_ready,
            session_reason,
            allow_screen,
            display_selected,
        );
        let browser_readiness = self.browser.readiness();
        let browser_surface = self.browser.surface_ref();
        let browser_ready = platform_supported
            && session_ready
            && ceiling.browser_semantic
            && browser_readiness
                .as_ref()
                .is_some_and(|readiness| readiness.connected)
            && browser_surface.is_some();
        let browser_reason = (!browser_ready).then_some(if !ceiling.browser_semantic {
            ComputerUseReadinessReason::DisabledByLocalCeiling
        } else if !platform_supported {
            ComputerUseReadinessReason::UnsupportedPlatform
        } else if !session_ready {
            session_reason.unwrap_or(ComputerUseReadinessReason::NoInteractiveSession)
        } else {
            match browser_readiness
                .as_ref()
                .and_then(|readiness| readiness.reason)
            {
                Some(BrowserReadinessReason::UserApprovalRequired)
                | Some(BrowserReadinessReason::UserDenied) => {
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
        let browser_adapter = ComputerUseAdapterRef {
            kind: ComputerUseAdapterKind::BrowserDevtoolsMcp,
            version: super::browser_devtools_mcp::CHROME_DEVTOOLS_MCP_VERSION.into(),
        };
        let outlook_handler =
            if platform_supported && session_ready && ceiling.communication_handoff_enabled() {
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
                    process_id: 0,
                    image_path: handler.executable_path.clone(),
                },
            )
            .ok()
        });
        let outlook_ready = outlook_application_ref.is_some();
        let outlook_reason =
            (!outlook_ready).then_some(if !ceiling.communication_handoff_enabled() {
                ComputerUseReadinessReason::DisabledByLocalCeiling
            } else if !platform_supported {
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
                        kind: ComputerUseAdapterKind::WindowsUia,
                        version: ui_adapter_version,
                    },
                    supported: platform_supported,
                    ready: ui_ready,
                    reason: (!ui_ready).then_some(
                        if !ceiling.observation_enabled()
                            || ceiling.allowed_application_paths.is_empty()
                        {
                            ComputerUseReadinessReason::DisabledByLocalCeiling
                        } else {
                            session_reason.unwrap_or(ComputerUseReadinessReason::AdapterUnavailable)
                        },
                    ),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::OfficeDocumentInspect,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::OfficeExcel,
                        version: office_adapter_version,
                    },
                    supported: platform_supported,
                    ready: office_ready,
                    reason: (!office_ready).then_some(
                        if !ceiling.observation_enabled() || !ceiling.office_semantic {
                            ComputerUseReadinessReason::DisabledByLocalCeiling
                        } else if !platform_supported {
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
                    capability: Capability::FileMetadataRead,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::FileSystem,
                        version: file_adapter_version.clone(),
                    },
                    supported: platform_supported,
                    ready: platform_supported,
                    reason: (!platform_supported)
                        .then_some(ComputerUseReadinessReason::UnsupportedPlatform),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::FileContentRead,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::FileSystem,
                        version: file_adapter_version,
                    },
                    supported: platform_supported,
                    ready: platform_supported,
                    reason: (!platform_supported)
                        .then_some(ComputerUseReadinessReason::UnsupportedPlatform),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::SpreadsheetFileInspect,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::FileSystem,
                        version: spreadsheet_file_adapter_version.clone(),
                    },
                    supported: platform_supported,
                    ready: platform_supported,
                    reason: (!platform_supported)
                        .then_some(ComputerUseReadinessReason::UnsupportedPlatform),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::SpreadsheetMergePreview,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::FileSystem,
                        version: spreadsheet_file_adapter_version.clone(),
                    },
                    supported: platform_supported,
                    ready: platform_supported,
                    reason: (!platform_supported)
                        .then_some(ComputerUseReadinessReason::UnsupportedPlatform),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::SpreadsheetWorkbookCreateConfirmed,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::FileSystem,
                        version: spreadsheet_file_adapter_version.clone(),
                    },
                    supported: platform_supported,
                    ready: platform_supported && ceiling.file_artifact_create_enabled(),
                    reason: (!(platform_supported && ceiling.file_artifact_create_enabled()))
                        .then_some(if !platform_supported {
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
                    supported: platform_supported,
                    ready: platform_supported && ceiling.file_artifact_create_enabled(),
                    reason: (!(platform_supported && ceiling.file_artifact_create_enabled()))
                        .then_some(if !platform_supported {
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
                    supported: platform_supported,
                    ready: platform_supported && ceiling.file_artifact_create_enabled(),
                    reason: (!(platform_supported && ceiling.file_artifact_create_enabled()))
                        .then_some(if !platform_supported {
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
                    supported: platform_supported,
                    ready: platform_supported && ceiling.file_artifact_create_enabled(),
                    reason: (!(platform_supported && ceiling.file_artifact_create_enabled()))
                        .then_some(if !platform_supported {
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
                    supported: platform_supported,
                    ready: platform_supported && ceiling.file_artifact_create_enabled(),
                    reason: (!(platform_supported && ceiling.file_artifact_create_enabled()))
                        .then_some(if !platform_supported {
                            ComputerUseReadinessReason::UnsupportedPlatform
                        } else {
                            ComputerUseReadinessReason::DisabledByLocalCeiling
                        }),
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::CommunicationOutlookNewHandoffConfirmed,
                    adapter: outlook_adapter,
                    supported: platform_supported,
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
                    supported: platform_supported,
                    ready: browser_ready,
                    reason: browser_reason,
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::BrowserPageNavigateConfirmed,
                    adapter: browser_adapter.clone(),
                    supported: platform_supported,
                    ready: browser_ready,
                    reason: browser_reason,
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::BrowserInputFallbackConfirmed,
                    adapter: browser_adapter,
                    supported: platform_supported,
                    ready: browser_ready,
                    reason: browser_reason,
                },
                ComputerUseCapabilityReadiness {
                    capability: Capability::BrowserExternalDraftWriteConfirmed,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::BrowserDevtoolsMcp,
                        version: super::browser_devtools_mcp::CHROME_DEVTOOLS_MCP_VERSION.into(),
                    },
                    supported: platform_supported,
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
            browser_adapter: self.browser.readiness().map(|readiness| readiness.adapter),
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

    #[cfg(windows)]
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
                process_id,
                image_path,
            }) if process_id == application.process_id
                && path_eq(&image_path, &application.image_path) => {}
            None => {}
            Some(_) => {
                return Err(error(
                    AgentErrorKind::InvalidInput,
                    "the UI inspection root is stale or is not the foreground application",
                    false,
                ));
            }
        }

        #[cfg(not(windows))]
        {
            return Err(error(
                AgentErrorKind::UnsupportedPlatform,
                "Windows UI Automation is unavailable on this platform",
                false,
            ));
        }
        #[cfg(windows)]
        {
            let collected = super::windows_uia_observer::collect_foreground(
                application.process_id,
                &application.image_path,
                params.max_depth,
                params.max_nodes,
                params.max_bytes,
            )?;
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
                        "cannot encode a Windows UI Automation projection",
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
                    kind: ComputerUseAdapterKind::WindowsUia,
                    version: "a4-windows-uia-read/v1".into(),
                },
                nodes,
                truncated,
            };
            loop {
                let encoded_len = serde_json::to_vec(&output)
                    .map_err(|_| {
                        error(
                            AgentErrorKind::Internal,
                            "cannot encode the Windows UI Automation result",
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
        self.writer_lease
            .acquire(request, self.human_input_epoch.load(Ordering::SeqCst))
    }

    pub fn require_writer_lease(
        &self,
        execution_generation: &str,
    ) -> Result<WriterLeaseState, AgentError> {
        self.writer_lease.require_active(
            execution_generation,
            self.human_input_epoch.load(Ordering::SeqCst),
        )
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

    /// Advance the worker incarnation while preserving the shared broker handle
    /// used by portable daemon-side reads. Every prior ObjectRef becomes invalid
    /// before a replacement in-process worker starts.
    pub(crate) fn reset_worker_incarnation(&self) {
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

struct ObservedDesktop {
    session_id: u32,
    foreground_application: Option<ObservedApplication>,
}

struct ObservedApplication {
    process_id: u32,
    image_path: String,
}

#[cfg(windows)]
fn observe_interactive_desktop() -> Result<ObservedDesktop, AgentError> {
    use std::mem::size_of;

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_READOBJECTS, GetUserObjectInformationW, OpenInputDesktop, UOI_NAME,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    use windows::core::PWSTR;

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

    let hwnd = unsafe { GetForegroundWindow() };
    let foreground_application = if hwnd.0.is_null() {
        None
    } else {
        let mut process_id = 0_u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
        process_image(process_id).map(|image_path| ObservedApplication {
            process_id,
            image_path,
        })
    };
    return Ok(ObservedDesktop {
        session_id,
        foreground_application,
    });

    fn process_image(process_id: u32) -> Option<String> {
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()?;
            let mut buffer = vec![0_u16; 32_768];
            let mut length = buffer.len() as u32;
            let result = QueryFullProcessImageNameW(
                process,
                Default::default(),
                PWSTR(buffer.as_mut_ptr()),
                &mut length,
            )
            .ok();
            let _ = CloseHandle(process);
            result.map(|_| String::from_utf16_lossy(&buffer[..length as usize]))
        }
    }
}

#[cfg(not(windows))]
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

        broker.reset_worker_incarnation();
        let after_snapshot = broker.next_snapshot_id();

        assert_ne!(before.snapshot_id, after_snapshot);
        assert!(broker.resolve_ref(&before).is_err());
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
        assert_eq!(readiness.capabilities.len(), 26);
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
                entry.ready == cfg!(windows)
            } else if entry.capability == Capability::ShellExecConfirmed {
                entry.ready
                    == (cfg!(windows) && !crate::exec_shells::available_exec_shells().is_empty())
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
                    | Capability::OfficeDocumentInspect
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
