//! Controlled-edge gateway for the pinned Chrome DevTools MCP process.
//!
//! Raw MCP tool names never cross this module. Callers choose a closed enum,
//! the gateway audits the upstream tool schema at connection time, and only
//! then issues a bounded MCP call. Site-specific business adapters remain
//! responsible for origin/page binding and effect classification.

mod action;
mod projection;

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use action::{plan_action, plan_materialized_upload};
use desk_agent_protocol::browser_control::{
    BROWSER_CONTROL_SCHEMA_VERSION, BrowserAction, BrowserActionRequest, BrowserActionResult,
    BrowserAdapterRef, BrowserControlContractError, BrowserEngineKind, BrowserReadiness,
    BrowserReadinessReason, BrowserToolKind,
};
use desk_agent_protocol::computer_use::{ObjectKind, ObjectRef};
use projection::{
    project_existing_page, project_form_readback, project_opened_page,
    project_opened_page_from_inventory_delta, project_page_after_activation, project_snapshot,
};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResult, Tool},
    service::RunningService,
    transport::TokioChildProcess,
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

pub const CHROME_DEVTOOLS_MCP_VERSION: &str = "1.7.0";
pub const CHROME_DEVTOOLS_MCP_NPM_INTEGRITY: &str = "sha512-6xFW7oiUxTxZuHcfyYBkKQtmttjCbfifKZMSEk5CV8H2FucvKweYiJr8CblddYHtYjA4C14K9VAs1r49906RBA==";
pub const CHROME_DEVTOOLS_MCP_START_TIMEOUT: Duration = Duration::from_secs(20);
pub const CHROME_DEVTOOLS_MCP_CALL_TIMEOUT: Duration = Duration::from_secs(30);
pub const CHROME_DEVTOOLS_MCP_READINESS_TIMEOUT: Duration = Duration::from_secs(90);
const CHROME_DEVTOOLS_MCP_CANCEL_TIMEOUT: Duration = Duration::from_secs(2);
pub const CHROME_DEVTOOLS_MCP_ARGS: &[&str] = &[
    "--autoConnect",
    "--channel=stable",
    "--experimentalPageIdRouting",
    "--experimentalStructuredContent",
    "--no-category-emulation",
    "--no-category-network",
    "--no-category-performance",
    "--no-performance-crux",
    "--no-usage-statistics",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChromeDevtoolsMcpPackage {
    pub node_executable: PathBuf,
    pub package_entrypoint: PathBuf,
    pub package_version: String,
    /// npm lock/bundle provenance checked by the installer before this package
    /// is made available to the worker.
    pub package_integrity: String,
}

impl ChromeDevtoolsMcpPackage {
    pub fn validate(&self) -> Result<(), ChromeDevtoolsMcpError> {
        if self.package_version != CHROME_DEVTOOLS_MCP_VERSION
            || self.package_integrity != CHROME_DEVTOOLS_MCP_NPM_INTEGRITY
        {
            return Err(ChromeDevtoolsMcpError::PackageIdentityMismatch);
        }
        if !self.node_executable.is_absolute()
            || !self.package_entrypoint.is_absolute()
            || !self.node_executable.is_file()
            || !self.package_entrypoint.is_file()
        {
            return Err(ChromeDevtoolsMcpError::PackageUnavailable);
        }
        Ok(())
    }

    fn command(&self) -> tokio::process::Command {
        self.command_with_connection_args(CHROME_DEVTOOLS_MCP_ARGS)
    }

    fn command_with_connection_args(&self, arguments: &[&str]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.node_executable);
        command
            .arg(&self.package_entrypoint)
            .args(arguments.iter().map(OsString::from))
            .env("CHROME_DEVTOOLS_MCP_NO_UPDATE_CHECKS", "1")
            .env("CHROME_DEVTOOLS_MCP_NO_USAGE_STATISTICS", "1")
            .kill_on_drop(true);
        command
    }
}

/// The complete upstream MCP surface that this gateway may call. Discovery
/// helpers are internal-only; the model-facing Provider exposes only the typed
/// actions from `desk_agent_protocol::browser_control`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AllowedChromeMcpTool {
    ListPages,
    SelectPage,
    NewPage,
    NavigatePage,
    TakeSnapshot,
    WaitFor,
    FillForm,
    UploadFile,
    Click,
}

impl AllowedChromeMcpTool {
    fn name(self) -> &'static str {
        match self {
            Self::ListPages => "list_pages",
            Self::SelectPage => "select_page",
            Self::NewPage => "new_page",
            Self::NavigatePage => "navigate_page",
            Self::TakeSnapshot => "take_snapshot",
            Self::WaitFor => "wait_for",
            Self::FillForm => "fill_form",
            Self::UploadFile => "upload_file",
            Self::Click => "click",
        }
    }

    fn required_properties(self) -> &'static [&'static str] {
        match self {
            Self::ListPages => &[],
            Self::SelectPage => &["pageId"],
            Self::NewPage => &["url"],
            Self::NavigatePage => &["pageId"],
            Self::TakeSnapshot => &["pageId"],
            Self::WaitFor => &["pageId", "text"],
            Self::FillForm => &["pageId", "elements"],
            Self::UploadFile => &["pageId", "uid", "filePath"],
            Self::Click => &["pageId", "uid"],
        }
    }

    fn all() -> [Self; 9] {
        [
            Self::ListPages,
            Self::SelectPage,
            Self::NewPage,
            Self::NavigatePage,
            Self::TakeSnapshot,
            Self::WaitFor,
            Self::FillForm,
            Self::UploadFile,
            Self::Click,
        ]
    }
}

pub struct ChromeDevtoolsMcpSession {
    service: RunningService<RoleClient, ()>,
    state: Mutex<BrowserSessionState>,
}

struct BrowserSessionState {
    adapter: BrowserAdapterRef,
    pages: BTreeMap<String, desk_agent_protocol::browser_control::BrowserPageRef>,
}

impl ChromeDevtoolsMcpSession {
    pub async fn connect(
        package: &ChromeDevtoolsMcpPackage,
        adapter: BrowserAdapterRef,
    ) -> Result<Self, ChromeDevtoolsMcpError> {
        package.validate()?;
        adapter
            .validate()
            .map_err(ChromeDevtoolsMcpError::InvalidBrowserContract)?;
        Self::connect_command(package.command(), adapter).await
    }

    async fn connect_command(
        command: tokio::process::Command,
        adapter: BrowserAdapterRef,
    ) -> Result<Self, ChromeDevtoolsMcpError> {
        let (transport, _stderr) = TokioChildProcess::builder(command)
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| ChromeDevtoolsMcpError::Spawn(error.to_string()))?;
        let service = tokio::time::timeout(CHROME_DEVTOOLS_MCP_START_TIMEOUT, ().serve(transport))
            .await
            .map_err(|_| ChromeDevtoolsMcpError::StartTimeout)?
            .map_err(|error| ChromeDevtoolsMcpError::Handshake(error.to_string()))?;
        let tools =
            match tokio::time::timeout(CHROME_DEVTOOLS_MCP_CALL_TIMEOUT, service.list_all_tools())
                .await
            {
                Ok(Ok(tools)) => tools,
                Ok(Err(error)) => {
                    let _ =
                        tokio::time::timeout(CHROME_DEVTOOLS_MCP_CANCEL_TIMEOUT, service.cancel())
                            .await;
                    return Err(ChromeDevtoolsMcpError::ToolSurface(error.to_string()));
                }
                Err(_) => {
                    let _ =
                        tokio::time::timeout(CHROME_DEVTOOLS_MCP_CANCEL_TIMEOUT, service.cancel())
                            .await;
                    return Err(ChromeDevtoolsMcpError::ToolSurfaceTimeout);
                }
            };
        if let Err(error) = audit_tool_surface(&tools) {
            let _ =
                tokio::time::timeout(CHROME_DEVTOOLS_MCP_CANCEL_TIMEOUT, service.cancel()).await;
            return Err(error);
        }
        // MCP initialization alone does not connect to Chrome. A discarded,
        // non-mutating list_pages probe is required before readiness may claim
        // that the native Chrome approval and auto-connect handshake succeeded.
        let probe = match tokio::time::timeout(
            CHROME_DEVTOOLS_MCP_READINESS_TIMEOUT,
            service.call_tool(
                CallToolRequestParams::new(AllowedChromeMcpTool::ListPages.name())
                    .with_arguments(Map::new()),
            ),
        )
        .await
        {
            Ok(Ok(probe)) => probe,
            Ok(Err(error)) => {
                let _ = tokio::time::timeout(CHROME_DEVTOOLS_MCP_CANCEL_TIMEOUT, service.cancel())
                    .await;
                return Err(ChromeDevtoolsMcpError::Handshake(error.to_string()));
            }
            Err(_) => {
                let _ = tokio::time::timeout(CHROME_DEVTOOLS_MCP_CANCEL_TIMEOUT, service.cancel())
                    .await;
                return Err(ChromeDevtoolsMcpError::ReadinessProbeTimeout);
            }
        };
        if probe.is_error == Some(true) {
            return Err(ChromeDevtoolsMcpError::Handshake(tool_error_summary(
                &probe,
            )));
        }
        Ok(Self {
            service,
            state: Mutex::new(BrowserSessionState {
                adapter,
                pages: BTreeMap::new(),
            }),
        })
    }

    #[cfg(test)]
    async fn connect_approved_websocket(
        package: &ChromeDevtoolsMcpPackage,
        adapter: BrowserAdapterRef,
        endpoint: &str,
    ) -> Result<Self, ChromeDevtoolsMcpError> {
        package.validate()?;
        adapter
            .validate()
            .map_err(ChromeDevtoolsMcpError::InvalidBrowserContract)?;
        let endpoint_url = url::Url::parse(endpoint)
            .map_err(|_| ChromeDevtoolsMcpError::InvalidApprovedWebSocket)?;
        if endpoint_url.scheme() != "ws"
            || endpoint_url.host_str() != Some("127.0.0.1")
            || endpoint_url.port().is_none()
            || !endpoint_url.path().starts_with("/devtools/browser/")
            || endpoint_url.path().len() <= "/devtools/browser/".len()
            || !endpoint_url.username().is_empty()
            || endpoint_url.password().is_some()
            || endpoint_url.query().is_some()
            || endpoint_url.fragment().is_some()
        {
            return Err(ChromeDevtoolsMcpError::InvalidApprovedWebSocket);
        }
        let endpoint_argument = format!("--wsEndpoint={endpoint}");
        let mut arguments = CHROME_DEVTOOLS_MCP_ARGS
            .iter()
            .copied()
            .filter(|argument| !matches!(*argument, "--autoConnect" | "--channel=stable"))
            .collect::<Vec<_>>();
        arguments.push(endpoint_argument.as_str());
        let command = package.command_with_connection_args(&arguments);
        Self::connect_command(command, adapter).await
    }

    #[cfg(test)]
    async fn connect_approved_browser_url(
        package: &ChromeDevtoolsMcpPackage,
        adapter: BrowserAdapterRef,
        browser_url: &str,
    ) -> Result<Self, ChromeDevtoolsMcpError> {
        package.validate()?;
        adapter
            .validate()
            .map_err(ChromeDevtoolsMcpError::InvalidBrowserContract)?;
        let url = url::Url::parse(browser_url)
            .map_err(|_| ChromeDevtoolsMcpError::InvalidApprovedWebSocket)?;
        if url.scheme() != "http"
            || url.host_str() != Some("127.0.0.1")
            || url.port().is_none()
            || url.path() != "/"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(ChromeDevtoolsMcpError::InvalidApprovedWebSocket);
        }
        let browser_url_argument = format!("--browserUrl={browser_url}");
        let mut arguments = CHROME_DEVTOOLS_MCP_ARGS
            .iter()
            .copied()
            .filter(|argument| !matches!(*argument, "--autoConnect" | "--channel=stable"))
            .collect::<Vec<_>>();
        arguments.push(browser_url_argument.as_str());
        let command = package.command_with_connection_args(&arguments);
        Self::connect_command(command, adapter).await
    }

    async fn call(
        &self,
        tool: AllowedChromeMcpTool,
        arguments: Map<String, Value>,
    ) -> Result<CallToolResult, ChromeDevtoolsMcpError> {
        validate_call_arguments(tool, &arguments)?;
        let params = CallToolRequestParams::new(tool.name()).with_arguments(arguments);
        tokio::time::timeout(
            CHROME_DEVTOOLS_MCP_CALL_TIMEOUT,
            self.service.call_tool(params),
        )
        .await
        .map_err(|_| ChromeDevtoolsMcpError::CallTimeout)?
        .map_err(|error| ChromeDevtoolsMcpError::Call(error.to_string()))
    }

    /// Execute one already-authorized semantic browser action. The state lock
    /// serializes page revisions so concurrent callers cannot reuse stale
    /// element references. Raw MCP results remain inside this module.
    async fn execute_action(
        &self,
        request: &BrowserActionRequest,
    ) -> Result<BrowserActionResult, ChromeDevtoolsMcpError> {
        request
            .validate()
            .map_err(ChromeDevtoolsMcpError::InvalidBrowserContract)?;
        let materialized_upload = materialize_verified_upload(&request.action)?;
        let mut state = self.state.lock().await;
        if let Some(page) = action_page(&request.action) {
            if state.pages.get(&page.page_id) != Some(page) {
                return Err(ChromeDevtoolsMcpError::StalePage);
            }
        }
        let plan = match materialized_upload.as_ref() {
            Some(upload) => plan_materialized_upload(&request.action, &upload.path),
            None => plan_action(&request.action),
        }
        .map_err(|_| ChromeDevtoolsMcpError::ActionPlan)?;
        let outcome = plan.outcome;
        let includes_snapshot = plan.includes_snapshot;
        let open_inventory_before = if matches!(&request.action, BrowserAction::OpenPage { .. }) {
            Some(self.list_pages().await?)
        } else {
            None
        };
        let activation_inventory_before =
            if matches!(&request.action, BrowserAction::ActivateElement { .. }) {
                Some(self.list_pages().await?)
            } else {
                None
            };
        let raw_call = self.call(plan.tool, plan.arguments).await;
        // A reviewed draft-with-attachment is one semantic mutation and one
        // writer lease, even though the pinned upstream MCP exposes form fill
        // and file upload as two calls. Never start the upload if fill failed;
        // once fill succeeds, any later error is conservatively surfaced to
        // the lifecycle as OutcomeUnknown by the caller.
        let raw_call = match (&request.action, raw_call, materialized_upload.as_ref()) {
            (BrowserAction::FillFormAndUpload { .. }, Ok(fill_result), Some(upload))
                if fill_result.is_error != Some(true) =>
            {
                let upload_plan = plan_materialized_upload(&request.action, &upload.path)
                    .map_err(|_| ChromeDevtoolsMcpError::ActionPlan)?;
                self.call(upload_plan.tool, upload_plan.arguments).await
            }
            (_, result, _) => result,
        };
        let (raw, reconciled_open_page) = match (raw_call, open_inventory_before.as_ref()) {
            (Ok(raw), _) if raw.is_error != Some(true) => (raw, None),
            (Ok(raw), Some(before)) => {
                let upstream_error = tool_error_summary(&raw);
                let after = self.list_pages().await?;
                let BrowserAction::OpenPage { target } = &request.action else {
                    unreachable!("open inventory is captured only for OpenPage")
                };
                match project_opened_page_from_inventory_delta(
                    before,
                    &after,
                    &state.adapter,
                    target,
                    now_unix_ms()?,
                ) {
                    Ok(page) => (after, Some(page)),
                    Err(_) => return Err(ChromeDevtoolsMcpError::Call(upstream_error)),
                }
            }
            (Err(call_error), Some(before)) => {
                let after = self.list_pages().await?;
                let BrowserAction::OpenPage { target } = &request.action else {
                    unreachable!("open inventory is captured only for OpenPage")
                };
                match project_opened_page_from_inventory_delta(
                    before,
                    &after,
                    &state.adapter,
                    target,
                    now_unix_ms()?,
                ) {
                    Ok(page) => (after, Some(page)),
                    Err(_) => return Err(call_error),
                }
            }
            (Ok(raw), None) => (raw, None),
            (Err(call_error), None) => return Err(call_error),
        };
        if raw.is_error == Some(true) {
            return Err(ChromeDevtoolsMcpError::Call(tool_error_summary(&raw)));
        }
        let mut completed_at_unix_ms = now_unix_ms()?;

        let (page, snapshot, form_readback) = match &request.action {
            BrowserAction::OpenPage { target } => {
                let page = if let Some(page) = reconciled_open_page {
                    page
                } else {
                    project_opened_page(&raw, &state.adapter, target, completed_at_unix_ms)
                        .map_err(|_| ChromeDevtoolsMcpError::Projection)?
                };
                let snapshot = if includes_snapshot {
                    let snapshot_result = self
                        .call(
                            AllowedChromeMcpTool::TakeSnapshot,
                            [
                                (
                                    "pageId".to_string(),
                                    json!(
                                        page.page_id
                                            .parse::<u64>()
                                            .map_err(|_| ChromeDevtoolsMcpError::Projection)?
                                    ),
                                ),
                                ("verbose".to_string(), json!(false)),
                            ]
                            .into_iter()
                            .collect(),
                        )
                        .await?;
                    if snapshot_result.is_error == Some(true) {
                        return Err(ChromeDevtoolsMcpError::Call(tool_error_summary(
                            &snapshot_result,
                        )));
                    }
                    let captured_at_unix_ms = now_unix_ms()?;
                    completed_at_unix_ms = captured_at_unix_ms;
                    Some(
                        project_snapshot(
                            &snapshot_result,
                            page.clone(),
                            desk_agent_protocol::browser_control::MAX_BROWSER_ELEMENTS,
                            captured_at_unix_ms,
                        )
                        .map_err(|_| ChromeDevtoolsMcpError::Projection)?,
                    )
                } else {
                    None
                };
                state.pages.insert(page.page_id.clone(), page.clone());
                (page, snapshot, Vec::new())
            }
            action => {
                let previous = action_page(action).ok_or(ChromeDevtoolsMcpError::StalePage)?;
                let expected_origin = match action {
                    BrowserAction::NavigatePage { target, .. } => &target.origin,
                    _ => &previous.origin,
                };
                // Re-list internally after every page-scoped action. This
                // verifies that the exact page still exists and did not cross
                // origin; the inventory is discarded and never projected.
                let listed = self.list_pages().await?;
                let (page, followed_new_tab) =
                    if matches!(action, BrowserAction::ActivateElement { .. }) {
                        project_page_after_activation(
                            activation_inventory_before
                                .as_ref()
                                .ok_or(ChromeDevtoolsMcpError::Projection)?,
                            &listed,
                            &state.adapter,
                            previous,
                            previous.document_revision.saturating_add(1),
                            completed_at_unix_ms,
                        )
                        .map_err(|_| ChromeDevtoolsMcpError::Projection)?
                    } else {
                        (
                            project_existing_page(
                                &listed,
                                previous,
                                expected_origin,
                                previous.document_revision.saturating_add(1),
                                completed_at_unix_ms,
                            )
                            .map_err(|_| ChromeDevtoolsMcpError::Projection)?,
                            false,
                        )
                    };
                let (snapshot, form_readback) = if includes_snapshot {
                    let max_elements = match action {
                        BrowserAction::TakeSnapshot { max_elements, .. } => {
                            usize::from(*max_elements)
                        }
                        _ => desk_agent_protocol::browser_control::MAX_BROWSER_ELEMENTS,
                    };
                    let refreshed_snapshot;
                    let snapshot_result = if followed_new_tab
                        || matches!(action, BrowserAction::ActivateElement { .. })
                    {
                        refreshed_snapshot = self
                            .call(
                                AllowedChromeMcpTool::TakeSnapshot,
                                [
                                    (
                                        "pageId".to_string(),
                                        json!(
                                            page.page_id
                                                .parse::<u64>()
                                                .map_err(|_| ChromeDevtoolsMcpError::Projection)?
                                        ),
                                    ),
                                    ("verbose".to_string(), json!(false)),
                                ]
                                .into_iter()
                                .collect(),
                            )
                            .await?;
                        &refreshed_snapshot
                    } else {
                        &raw
                    };
                    let snapshot = Some(
                        project_snapshot(
                            snapshot_result,
                            page.clone(),
                            max_elements,
                            completed_at_unix_ms,
                        )
                        .map_err(|_| ChromeDevtoolsMcpError::Projection)?,
                    );
                    let form_readback = match action {
                        BrowserAction::FillForm { fields, .. }
                        | BrowserAction::FillFormAndUpload { fields, .. } => {
                            project_form_readback(snapshot_result, fields)
                                .map_err(|_| ChromeDevtoolsMcpError::Projection)?
                        }
                        _ => Vec::new(),
                    };
                    (snapshot, form_readback)
                } else {
                    (None, Vec::new())
                };
                state.pages.insert(page.page_id.clone(), page.clone());
                (page, snapshot, form_readback)
            }
        };
        let result = BrowserActionResult {
            schema_version: desk_agent_protocol::browser_control::BROWSER_CONTROL_SCHEMA_VERSION,
            call_id: request.call_id.clone(),
            outcome,
            page,
            snapshot,
            form_readback,
            completed_at_unix_ms,
        };
        result
            .validate()
            .map_err(ChromeDevtoolsMcpError::InvalidBrowserContract)?;
        Ok(result)
    }

    /// Typed Provider surface. Raw MCP responses, native paths and tool names
    /// remain inside this module. Upload reopens one edge-issued artifact ref,
    /// verifies its immutable digest, materializes it under a private temporary
    /// directory for the single MCP call, and deletes it on return.
    async fn execute(
        &self,
        request: &BrowserActionRequest,
    ) -> Result<BrowserActionResult, ChromeDevtoolsMcpError> {
        self.execute_action(request).await
    }

    async fn list_pages(&self) -> Result<CallToolResult, ChromeDevtoolsMcpError> {
        self.call(AllowedChromeMcpTool::ListPages, Map::new()).await
    }

    async fn close(self) -> Result<(), ChromeDevtoolsMcpError> {
        self.service
            .cancel()
            .await
            .map(|_| ())
            .map_err(|error| ChromeDevtoolsMcpError::Call(error.to_string()))
    }
}

struct MaterializedUpload {
    _directory: tempfile::TempDir,
    path: PathBuf,
}

fn materialize_verified_upload(
    action: &BrowserAction,
) -> Result<Option<MaterializedUpload>, ChromeDevtoolsMcpError> {
    let (file, file_name, size_bytes, digest_sha256) = match action {
        BrowserAction::UploadFile {
            file,
            file_name,
            size_bytes,
            digest_sha256,
            ..
        }
        | BrowserAction::FillFormAndUpload {
            file,
            file_name,
            size_bytes,
            digest_sha256,
            ..
        } => (file, file_name, size_bytes, digest_sha256),
        _ => return Ok(None),
    };
    let verified = super::file_reference_store::read_verified_bytes(file, *size_bytes)
        .map_err(|error| ChromeDevtoolsMcpError::ArtifactMaterialization(error.message))?;
    if verified.bytes.len() as u64 != *size_bytes || verified.sha256 != *digest_sha256 {
        return Err(ChromeDevtoolsMcpError::ArtifactMaterialization(
            "artifact bytes changed before browser upload".into(),
        ));
    }
    let directory = tempfile::Builder::new()
        .prefix("lcxl-browser-upload-")
        .tempdir()
        .map_err(|error| ChromeDevtoolsMcpError::ArtifactMaterialization(error.to_string()))?;
    let path = directory.path().join(file_name);
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| ChromeDevtoolsMcpError::ArtifactMaterialization(error.to_string()))?;
    std::io::Write::write_all(&mut output, &verified.bytes)
        .and_then(|_| output.sync_all())
        .map_err(|error| ChromeDevtoolsMcpError::ArtifactMaterialization(error.to_string()))?;
    Ok(Some(MaterializedUpload {
        _directory: directory,
        path,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BrowserBrokerContext {
    pub device_id: String,
    pub os_session_id: String,
    pub enabled: bool,
    pub interactive_session_unlocked: bool,
}

#[derive(Debug, Clone)]
struct BrowserBrokerProjection {
    readiness: Option<BrowserReadiness>,
    surface: Option<ObjectRef>,
    last_reason: BrowserReadinessReason,
    retry_after_unix_ms: u64,
}

/// Worker-lifetime owner of the approved Chrome MCP session. It exposes only
/// readiness, one opaque browser-surface reference, and typed actions. The raw
/// MCP session cannot be reached by the model or the central orchestrator.
pub(super) struct BrowserDevtoolsBroker {
    projection: Arc<StdMutex<BrowserBrokerProjection>>,
    session: Mutex<Option<ChromeDevtoolsMcpSession>>,
    refresh_guard: Mutex<()>,
    connection_revision: std::sync::atomic::AtomicU64,
}

impl Default for BrowserDevtoolsBroker {
    fn default() -> Self {
        Self {
            projection: Arc::new(StdMutex::new(BrowserBrokerProjection {
                readiness: None,
                surface: None,
                last_reason: BrowserReadinessReason::Disconnected,
                retry_after_unix_ms: 0,
            })),
            session: Mutex::new(None),
            refresh_guard: Mutex::new(()),
            connection_revision: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl BrowserDevtoolsBroker {
    pub(super) fn readiness(&self) -> Option<BrowserReadiness> {
        self.projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .readiness
            .clone()
    }

    pub(super) fn surface_ref(&self) -> Option<ObjectRef> {
        self.projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .surface
            .as_ref()
            .map(|surface| super::renew_browser_surface_ref(surface))
    }

    pub(super) async fn refresh(&self, context: &BrowserBrokerContext) {
        // A native Chrome approval may occupy the full readiness timeout. The
        // worker schedules refreshes independently from its short readiness
        // heartbeat, so collapse overlapping ticks into one MCP attempt.
        let Ok(_refresh_guard) = self.refresh_guard.try_lock() else {
            return;
        };
        if !context.enabled || !context.interactive_session_unlocked {
            self.disconnect(
                if context.enabled {
                    BrowserReadinessReason::InteractiveSessionLocked
                } else {
                    BrowserReadinessReason::Disconnected
                },
                context.interactive_session_unlocked,
            )
            .await;
            return;
        }
        if self.session.lock().await.is_some() {
            self.refresh_timestamp(context.interactive_session_unlocked);
            return;
        }
        let now = now_unix_ms().unwrap_or(1);
        if self
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retry_after_unix_ms
            > now
        {
            return;
        }
        let Some((browser_version, browser_major_version, profile_incarnation)) =
            installed_chrome_identity()
        else {
            self.set_unavailable(
                BrowserReadinessReason::McpUnavailable,
                None,
                context.interactive_session_unlocked,
            );
            return;
        };
        if browser_major_version
            < desk_agent_protocol::browser_control::MIN_CHROME_DEVTOOLS_MCP_MAJOR_VERSION
        {
            self.set_unavailable(
                BrowserReadinessReason::UnsupportedBrowserVersion,
                None,
                context.interactive_session_unlocked,
            );
            return;
        }
        let adapter = BrowserAdapterRef {
            engine: BrowserEngineKind::ChromeDevtoolsMcp,
            device_id: context.device_id.clone(),
            os_session_id: context.os_session_id.clone(),
            browser_major_version,
            browser_version,
            adapter_id: "chrome-devtools-mcp".into(),
            adapter_version: CHROME_DEVTOOLS_MCP_VERSION.into(),
            profile_incarnation,
            connection_revision: self
                .connection_revision
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1,
        };
        let package = match locate_pinned_package() {
            Ok(package) => package,
            Err(error) => {
                log::warn!(
                    "[browser-devtools-mcp] pinned package unavailable during readiness refresh: {error}"
                );
                self.set_unavailable(
                    BrowserReadinessReason::McpUnavailable,
                    Some(adapter),
                    context.interactive_session_unlocked,
                );
                return;
            }
        };
        match ChromeDevtoolsMcpSession::connect(&package, adapter.clone()).await {
            Ok(session) => {
                *self.session.lock().await = Some(session);
                let observed_at_unix_ms = now_unix_ms().unwrap_or(1);
                let readiness = BrowserReadiness {
                    schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
                    adapter: adapter.clone(),
                    adapter_enabled: true,
                    user_authorized: true,
                    connected: true,
                    interactive_session_unlocked: context.interactive_session_unlocked,
                    tools: vec![
                        BrowserToolKind::OpenPage,
                        BrowserToolKind::NavigatePage,
                        BrowserToolKind::TakeSnapshot,
                        BrowserToolKind::WaitFor,
                        BrowserToolKind::FillForm,
                        BrowserToolKind::ActivateElement,
                    ],
                    reason: None,
                    observed_at_unix_ms,
                };
                let surface = ObjectRef {
                    token: format!(
                        "browser-surface-{:x}",
                        Sha256::digest(
                            format!(
                                "{}:{}:{}:{}",
                                adapter.device_id,
                                adapter.os_session_id,
                                adapter.profile_incarnation,
                                adapter.connection_revision
                            )
                            .as_bytes()
                        )
                    ),
                    snapshot_id: format!("browser-connection-{}", adapter.connection_revision),
                    object_kind: ObjectKind::BrowserSurface,
                    expires_at: (chrono::Utc::now()
                        + chrono::Duration::seconds(super::PERMISSION_FLOW_TTL_SECONDS))
                    .to_rfc3339(),
                };
                let mut projection = self
                    .projection
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                projection.readiness = Some(readiness);
                projection.surface = Some(surface);
                projection.last_reason = BrowserReadinessReason::Disconnected;
                projection.retry_after_unix_ms = 0;
            }
            Err(error) => {
                log::warn!("[browser-devtools-mcp] readiness connection failed: {error}");
                let reason = if matches!(
                    error,
                    ChromeDevtoolsMcpError::Handshake(_)
                        | ChromeDevtoolsMcpError::ReadinessProbeTimeout
                ) {
                    BrowserReadinessReason::UserApprovalRequired
                } else {
                    BrowserReadinessReason::McpUnavailable
                };
                self.set_unavailable(reason, Some(adapter), context.interactive_session_unlocked);
            }
        }
    }

    pub(super) fn preflight(
        &self,
        surface: &ObjectRef,
        request: &BrowserActionRequest,
    ) -> Result<(), ChromeDevtoolsMcpError> {
        request
            .validate()
            .map_err(ChromeDevtoolsMcpError::InvalidBrowserContract)?;
        let projection = self
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if projection.surface.as_ref().is_none_or(|authoritative| {
            !super::same_browser_surface_identity(surface, authoritative)
        }) || !super::browser_surface_lease_is_current(surface)
            || !projection
                .readiness
                .as_ref()
                .is_some_and(|readiness| readiness.connected)
        {
            return Err(ChromeDevtoolsMcpError::StalePage);
        }
        if let Some(page) = action_page(&request.action)
            && projection
                .readiness
                .as_ref()
                .is_none_or(|readiness| readiness.adapter != page.adapter)
        {
            return Err(ChromeDevtoolsMcpError::StalePage);
        }
        Ok(())
    }

    pub(super) async fn execute(
        &self,
        surface: &ObjectRef,
        request: &BrowserActionRequest,
    ) -> Result<BrowserActionResult, ChromeDevtoolsMcpError> {
        self.preflight(surface, request)?;
        let mut session = self.session.lock().await;
        let result = session
            .as_ref()
            .ok_or(ChromeDevtoolsMcpError::StalePage)?
            .execute(request)
            .await;
        if result
            .as_ref()
            .is_err_and(ChromeDevtoolsMcpError::invalidates_session)
        {
            let interactive_session_unlocked = self
                .projection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .readiness
                .as_ref()
                .is_none_or(|readiness| readiness.interactive_session_unlocked);
            *session = None;
            drop(session);
            self.set_unavailable(
                BrowserReadinessReason::Disconnected,
                None,
                interactive_session_unlocked,
            );
        }
        result
    }

    async fn disconnect(&self, reason: BrowserReadinessReason, interactive_session_unlocked: bool) {
        if let Some(session) = self.session.lock().await.take() {
            let _ = session.close().await;
        }
        self.set_unavailable(reason, None, interactive_session_unlocked);
    }

    fn refresh_timestamp(&self, interactive_session_unlocked: bool) {
        let mut projection = self
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(readiness) = projection.readiness.as_mut() {
            readiness.observed_at_unix_ms = now_unix_ms().unwrap_or(1);
            readiness.interactive_session_unlocked = interactive_session_unlocked;
        }
    }

    fn set_unavailable(
        &self,
        reason: BrowserReadinessReason,
        adapter: Option<BrowserAdapterRef>,
        interactive_session_unlocked: bool,
    ) {
        let mut projection = self
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        projection.last_reason = reason;
        projection.retry_after_unix_ms = now_unix_ms().unwrap_or(1).saturating_add(
            if reason == BrowserReadinessReason::UserApprovalRequired {
                60_000
            } else {
                10_000
            },
        );
        projection.surface = None;
        projection.readiness = adapter.map(|adapter| BrowserReadiness {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            adapter,
            adapter_enabled: devtools_active_port().is_some(),
            user_authorized: false,
            connected: false,
            interactive_session_unlocked,
            tools: Vec::new(),
            reason: Some(reason),
            observed_at_unix_ms: now_unix_ms().unwrap_or(1),
        });
    }
}

fn locate_pinned_package() -> Result<ChromeDevtoolsMcpPackage, ChromeDevtoolsMcpError> {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let package_entrypoint = workspace_root
        .join("node_modules/chrome-devtools-mcp/build/src/bin/chrome-devtools-mcp.js")
        .canonicalize()
        .map_err(|_| ChromeDevtoolsMcpError::PackageUnavailable)?;
    let node_executable = which::which("node")
        .map_err(|_| ChromeDevtoolsMcpError::PackageUnavailable)?
        .canonicalize()
        .map_err(|_| ChromeDevtoolsMcpError::PackageUnavailable)?;
    let package = ChromeDevtoolsMcpPackage {
        node_executable,
        package_entrypoint,
        package_version: CHROME_DEVTOOLS_MCP_VERSION.into(),
        package_integrity: CHROME_DEVTOOLS_MCP_NPM_INTEGRITY.into(),
    };
    package.validate()?;
    Ok(package)
}

fn chrome_user_data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        return std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|root| root.join("Google/Chrome/User Data"));
    }
    #[cfg(target_os = "macos")]
    {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|root| root.join("Library/Application Support/Google/Chrome"));
    }
    #[cfg(target_os = "linux")]
    {
        let config_root = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        let candidates = [
            config_root.join("google-chrome"),
            config_root.join("google-chrome-beta"),
            config_root.join("google-chrome-unstable"),
        ];
        return candidates
            .iter()
            .find(|path| path.join("DevToolsActivePort").is_file())
            .cloned()
            .or_else(|| candidates.into_iter().find(|path| path.is_dir()));
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    None
}

fn devtools_active_port() -> Option<String> {
    let contents = fs::read_to_string(chrome_user_data_dir()?.join("DevToolsActivePort")).ok()?;
    let mut lines = contents.lines();
    lines.next()?.parse::<u16>().ok()?;
    let browser_path = lines.next()?;
    browser_path
        .starts_with("/devtools/browser/")
        .then(|| contents)
}

fn installed_chrome_identity() -> Option<(String, u16, String)> {
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        None
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    {
        #[cfg(windows)]
        let version = {
            let local_app = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
            let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from);
            let application_dir = [
                local_app.map(|root| root.join("Google/Chrome/Application")),
                program_files.map(|root| root.join("Google/Chrome/Application")),
            ]
            .into_iter()
            .flatten()
            .find(|path| path.join("chrome.exe").is_file())?;
            fs::read_dir(application_dir)
                .ok()?
                .filter_map(Result::ok)
                .filter(|entry| entry.path().is_dir())
                .filter_map(|entry| entry.file_name().into_string().ok())
                .max_by_key(|name| parse_chrome_version(name).map(|(_, parts)| parts))?
        };
        #[cfg(target_os = "macos")]
        let version = installed_macos_chrome_version()?;
        #[cfg(target_os = "linux")]
        let version = installed_linux_chrome_version()?;

        let (major, _) = parse_chrome_version(&version)?;
        let active_port = devtools_active_port()?;
        let profile_incarnation = format!("{:x}", Sha256::digest(active_port.as_bytes()));
        Some((version, major, profile_incarnation))
    }
}

#[cfg(target_os = "linux")]
fn installed_linux_chrome_version() -> Option<String> {
    ["google-chrome", "google-chrome-stable"]
        .into_iter()
        .filter_map(|program| which::which(program).ok())
        .chain([PathBuf::from("/opt/google/chrome/google-chrome")])
        .find_map(|program| {
            let output = std::process::Command::new(program)
                .arg("--version")
                .output()
                .ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
                .and_then(|output| parse_linux_chrome_version_output(&output))
        })
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux_chrome_version_output(output: &str) -> Option<String> {
    output
        .split_ascii_whitespace()
        .find(|token| parse_chrome_version(token).is_some())
        .map(str::to_string)
}

fn parse_chrome_version(version: &str) -> Option<(u16, Vec<u32>)> {
    let parts = version
        .split('.')
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if parts.len() != 4 {
        return None;
    }
    let major = u16::try_from(parts[0]).ok()?;
    Some((major, parts))
}

#[cfg(target_os = "macos")]
fn installed_macos_chrome_version() -> Option<String> {
    let user_application = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|root| root.join("Applications/Google Chrome.app"));
    let application = user_application
        .into_iter()
        .chain([PathBuf::from("/Applications/Google Chrome.app")])
        .find(|path| {
            path.join("Contents/MacOS/Google Chrome").is_file()
                && path.join("Contents/Info.plist").is_file()
        })?;
    let output = std::process::Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
        .arg(application.join("Contents/Info.plist"))
        .output()
        .ok()?;
    output.status.success().then_some(())?;
    let version = String::from_utf8(output.stdout).ok()?.trim().to_string();
    parse_chrome_version(&version).map(|_| version)
}

fn action_page(
    action: &BrowserAction,
) -> Option<&desk_agent_protocol::browser_control::BrowserPageRef> {
    match action {
        BrowserAction::OpenPage { .. } => None,
        BrowserAction::NavigatePage { page, .. }
        | BrowserAction::TakeSnapshot { page, .. }
        | BrowserAction::WaitFor { page, .. }
        | BrowserAction::FillForm { page, .. }
        | BrowserAction::FillFormAndUpload { page, .. }
        | BrowserAction::UploadFile { page, .. }
        | BrowserAction::ActivateElement { page, .. } => Some(page),
    }
}

fn now_unix_ms() -> Result<u64, ChromeDevtoolsMcpError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ChromeDevtoolsMcpError::InvalidClock)?
        .as_millis();
    u64::try_from(millis).map_err(|_| ChromeDevtoolsMcpError::InvalidClock)
}

fn tool_error_summary(result: &CallToolResult) -> String {
    const MAX_ERROR_CHARS: usize = 512;
    let detail = result
        .content
        .iter()
        .filter_map(|content| content.as_text())
        .map(|content| {
            content
                .text
                .replace(|character: char| character.is_control(), " ")
        })
        .collect::<Vec<_>>()
        .join(" ");
    let detail = detail.trim();
    if detail.is_empty() {
        return "Chrome rejected or could not complete auto-connect".into();
    }
    detail.chars().take(MAX_ERROR_CHARS).collect()
}

fn audit_tool_surface(tools: &[Tool]) -> Result<(), ChromeDevtoolsMcpError> {
    let tools = tools
        .iter()
        .map(|tool| (tool.name.as_ref(), tool))
        .collect::<BTreeMap<_, _>>();
    for expected in AllowedChromeMcpTool::all() {
        let tool = tools
            .get(expected.name())
            .ok_or(ChromeDevtoolsMcpError::MissingRequiredTool(expected.name()))?;
        let properties = tool
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .ok_or(ChromeDevtoolsMcpError::ToolSchemaMismatch(expected.name()))?;
        if expected
            .required_properties()
            .iter()
            .any(|property| !properties.contains_key(*property))
        {
            return Err(ChromeDevtoolsMcpError::ToolSchemaMismatch(expected.name()));
        }
    }
    Ok(())
}

fn validate_call_arguments(
    tool: AllowedChromeMcpTool,
    arguments: &Map<String, Value>,
) -> Result<(), ChromeDevtoolsMcpError> {
    let allowed = match tool {
        AllowedChromeMcpTool::ListPages => &[][..],
        AllowedChromeMcpTool::SelectPage => &["pageId", "bringToFront"],
        AllowedChromeMcpTool::NewPage => &["url", "background", "timeout"],
        AllowedChromeMcpTool::NavigatePage => {
            &["pageId", "type", "url", "timeout", "handleBeforeUnload"]
        }
        AllowedChromeMcpTool::TakeSnapshot => &["pageId", "verbose", "filePath"],
        AllowedChromeMcpTool::WaitFor => &["pageId", "text", "timeout"],
        AllowedChromeMcpTool::FillForm => &["pageId", "elements", "includeSnapshot"],
        AllowedChromeMcpTool::UploadFile => &["pageId", "uid", "filePath", "includeSnapshot"],
        AllowedChromeMcpTool::Click => &["pageId", "uid", "includeSnapshot"],
    };
    let allowed = allowed.iter().copied().collect::<BTreeSet<_>>();
    if arguments.keys().any(|key| !allowed.contains(key.as_str()))
        || tool
            .required_properties()
            .iter()
            .any(|required| !arguments.contains_key(*required))
    {
        return Err(ChromeDevtoolsMcpError::InvalidArguments(tool.name()));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChromeDevtoolsMcpError {
    PackageIdentityMismatch,
    PackageUnavailable,
    Spawn(String),
    StartTimeout,
    Handshake(String),
    ToolSurfaceTimeout,
    ReadinessProbeTimeout,
    MissingRequiredTool(&'static str),
    ToolSchemaMismatch(&'static str),
    InvalidArguments(&'static str),
    CallTimeout,
    ToolSurface(String),
    Call(String),
    InvalidBrowserContract(BrowserControlContractError),
    ArtifactMaterialization(String),
    ActionPlan,
    Projection,
    StalePage,
    InvalidClock,
    InvalidApprovedWebSocket,
}

impl ChromeDevtoolsMcpError {
    /// Only an upstream transport failure invalidates the MCP session. Stale
    /// page/element references, rejected contracts, local artifact checks and
    /// projection failures are action-scoped and must not churn the adapter
    /// connection revision for every recoverable browser interaction.
    fn invalidates_session(&self) -> bool {
        matches!(self, Self::CallTimeout | Self::Call(_))
    }
}

impl std::fmt::Display for ChromeDevtoolsMcpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PackageIdentityMismatch => {
                formatter.write_str("Chrome DevTools MCP package identity mismatch")
            }
            Self::PackageUnavailable => {
                formatter.write_str("Chrome DevTools MCP package is unavailable")
            }
            Self::Spawn(error) => write!(formatter, "failed to spawn Chrome DevTools MCP: {error}"),
            Self::StartTimeout => formatter.write_str("Chrome DevTools MCP startup timed out"),
            Self::Handshake(error) => {
                write!(formatter, "Chrome DevTools MCP handshake failed: {error}")
            }
            Self::ToolSurfaceTimeout => {
                formatter.write_str("Chrome DevTools MCP tool-surface inspection timed out")
            }
            Self::ReadinessProbeTimeout => {
                formatter.write_str("Chrome DevTools MCP readiness probe timed out")
            }
            Self::MissingRequiredTool(tool) => {
                write!(formatter, "required Chrome MCP tool is missing: {tool}")
            }
            Self::ToolSchemaMismatch(tool) => {
                write!(formatter, "Chrome MCP tool schema changed: {tool}")
            }
            Self::InvalidArguments(tool) => {
                write!(formatter, "invalid arguments for Chrome MCP tool: {tool}")
            }
            Self::CallTimeout => formatter.write_str("Chrome DevTools MCP call timed out"),
            Self::ToolSurface(error) => {
                write!(formatter, "failed to inspect Chrome MCP tools: {error}")
            }
            Self::Call(error) => write!(formatter, "Chrome DevTools MCP call failed: {error}"),
            Self::InvalidBrowserContract(error) => {
                write!(formatter, "invalid browser action contract: {error}")
            }
            Self::ArtifactMaterialization(error) => {
                write!(
                    formatter,
                    "failed to materialize verified browser upload: {error}"
                )
            }
            Self::ActionPlan => formatter.write_str("browser action planning failed"),
            Self::Projection => formatter.write_str("browser result projection failed"),
            Self::StalePage => formatter.write_str("browser page reference is stale"),
            Self::InvalidClock => formatter.write_str("system clock cannot represent Unix time"),
            Self::InvalidApprovedWebSocket => {
                formatter.write_str("approved Chrome WebSocket endpoint is invalid")
            }
        }
    }
}

impl std::error::Error for ChromeDevtoolsMcpError {}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, sync::Arc};

    use desk_agent_protocol::browser_control::{
        BROWSER_CONTROL_SCHEMA_VERSION, BrowserAction, BrowserActionOutcome, BrowserActionRequest,
        BrowserAdapterRef, BrowserElementRef, BrowserElementRole, BrowserEngineKind,
        BrowserFormField, BrowserMutationClass, BrowserNavigationTarget, BrowserOrigin,
        BrowserOriginKind, BrowserPageRef,
    };
    use desk_agent_protocol::data_lineage::ContentRef;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn only_transport_failures_invalidate_the_browser_session() {
        assert!(ChromeDevtoolsMcpError::CallTimeout.invalidates_session());
        assert!(ChromeDevtoolsMcpError::Call("closed".into()).invalidates_session());
        assert!(!ChromeDevtoolsMcpError::StalePage.invalidates_session());
        assert!(!ChromeDevtoolsMcpError::Projection.invalidates_session());
        assert!(
            !ChromeDevtoolsMcpError::ArtifactMaterialization("mismatch".into())
                .invalidates_session()
        );
    }

    fn tool(name: &'static str, properties: &[&str]) -> Tool {
        Tool::new(
            Cow::Borrowed(name),
            "test",
            Arc::new(
                serde_json::from_value(json!({
                    "type": "object",
                    "properties": properties
                        .iter()
                        .map(|property| ((*property).to_string(), json!({})))
                        .collect::<serde_json::Map<_, _>>()
                }))
                .unwrap(),
            ),
        )
    }

    fn audited_tools() -> Vec<Tool> {
        AllowedChromeMcpTool::all()
            .into_iter()
            .map(|tool_kind| tool(tool_kind.name(), tool_kind.required_properties()))
            .collect()
    }

    fn live_adapter() -> BrowserAdapterRef {
        BrowserAdapterRef {
            engine: BrowserEngineKind::ChromeDevtoolsMcp,
            device_id: "live-test-device".into(),
            os_session_id: "live-test-session".into(),
            browser_major_version: 151,
            browser_version: "151.0.7922.174".into(),
            adapter_id: "chrome-devtools-mcp".into(),
            adapter_version: CHROME_DEVTOOLS_MCP_VERSION.into(),
            profile_incarnation: "live-test-profile-incarnation".into(),
            connection_revision: 1,
        }
    }

    fn upload_action(file: ObjectRef, size_bytes: u64, digest_sha256: String) -> BrowserAction {
        let page = BrowserPageRef {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            adapter: live_adapter(),
            page_id: "7".into(),
            page_incarnation: "page-incarnation-1".into(),
            origin: BrowserOrigin {
                kind: BrowserOriginKind::Https,
                host_ascii: "mail.google.com".into(),
                port: 443,
            },
            document_revision: 1,
            url_sha256: "a".repeat(64),
            observed_at_unix_ms: 1,
        };
        BrowserAction::UploadFile {
            element: BrowserElementRef {
                page_id: page.page_id.clone(),
                page_incarnation: page.page_incarnation.clone(),
                document_revision: page.document_revision,
                element_id: "attachment-input-1".into(),
                role: BrowserElementRole::Button,
                accessible_name: "Attach files".into(),
                value: None,
                element_revision: 1,
            },
            page,
            content: ContentRef::Artifact {
                artifact_id: file.token.clone(),
                sha256: digest_sha256.clone(),
                size_bytes,
                media_type: "application/test".into(),
            },
            file,
            file_name: "report.docx".into(),
            media_type: "application/test".into(),
            size_bytes,
            digest_sha256,
            mutation_class: BrowserMutationClass::WriteExternalDraft,
        }
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn verified_upload_materialization_is_exact_private_and_ephemeral() {
        let _guard = super::super::file_reference_store::file_store_test_lock();
        let source_directory = tempfile::tempdir().unwrap();
        let source_path = source_directory.path().join("source.docx");
        let bytes = b"typed immutable artifact bytes";
        std::fs::write(&source_path, bytes).unwrap();
        super::super::file_reference_store::reset_worker_incarnation();
        let file = super::super::file_reference_store::issue(&source_path).unwrap();
        let digest = format!("{:x}", Sha256::digest(bytes));
        let action = upload_action(file, bytes.len() as u64, digest);

        let materialized = materialize_verified_upload(&action).unwrap().unwrap();
        let private_path = materialized.path.clone();
        let private_directory = private_path.parent().unwrap().to_path_buf();
        assert_ne!(private_path, source_path);
        assert_eq!(private_path.file_name().unwrap(), "report.docx");
        assert_eq!(std::fs::read(&private_path).unwrap(), bytes);
        drop(materialized);
        assert!(!private_path.exists());
        assert!(!private_directory.exists());
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn upload_materialization_rejects_digest_drift_before_browser_mutation() {
        let _guard = super::super::file_reference_store::file_store_test_lock();
        let source_directory = tempfile::tempdir().unwrap();
        let source_path = source_directory.path().join("source.docx");
        let bytes = b"typed immutable artifact bytes";
        std::fs::write(&source_path, bytes).unwrap();
        super::super::file_reference_store::reset_worker_incarnation();
        let file = super::super::file_reference_store::issue(&source_path).unwrap();
        let action = upload_action(file, bytes.len() as u64, "b".repeat(64));

        assert!(matches!(
            materialize_verified_upload(&action),
            Err(ChromeDevtoolsMcpError::ArtifactMaterialization(message))
                if message == "artifact bytes changed before browser upload"
        ));
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    #[test]
    fn upload_materialization_fails_closed_without_handle_bound_file_reads() {
        let _guard = super::super::file_reference_store::file_store_test_lock();
        let source_directory = tempfile::tempdir().unwrap();
        let source_path = source_directory.path().join("source.docx");
        let bytes = b"typed immutable artifact bytes";
        std::fs::write(&source_path, bytes).unwrap();
        super::super::file_reference_store::reset_worker_incarnation();
        let file = super::super::file_reference_store::issue(&source_path).unwrap();
        let digest = format!("{:x}", Sha256::digest(bytes));
        let action = upload_action(file, bytes.len() as u64, digest);

        assert!(matches!(
            materialize_verified_upload(&action),
            Err(ChromeDevtoolsMcpError::ArtifactMaterialization(_))
        ));
    }

    #[test]
    fn upload_materialization_rejects_a_pre_restart_artifact_reference() {
        let _guard = super::super::file_reference_store::file_store_test_lock();
        let source_directory = tempfile::tempdir().unwrap();
        let source_path = source_directory.path().join("source.docx");
        let bytes = b"typed immutable artifact bytes";
        std::fs::write(&source_path, bytes).unwrap();
        super::super::file_reference_store::reset_worker_incarnation();
        let file = super::super::file_reference_store::issue(&source_path).unwrap();
        let digest = format!("{:x}", Sha256::digest(bytes));
        let action = upload_action(file, bytes.len() as u64, digest);
        super::super::file_reference_store::reset_worker_incarnation();

        assert!(matches!(
            materialize_verified_upload(&action),
            Err(ChromeDevtoolsMcpError::ArtifactMaterialization(_))
        ));
    }

    #[test]
    fn unavailable_projection_preserves_locked_session_state() {
        let broker = BrowserDevtoolsBroker::default();
        broker.set_unavailable(
            BrowserReadinessReason::InteractiveSessionLocked,
            Some(live_adapter()),
            false,
        );
        let readiness = broker.readiness().expect("browser readiness projection");
        assert!(!readiness.interactive_session_unlocked);
        assert!(!readiness.connected);
        assert_eq!(
            readiness.reason,
            Some(BrowserReadinessReason::InteractiveSessionLocked)
        );
    }

    #[test]
    fn connected_projection_keeps_working_when_session_locks() {
        let broker = BrowserDevtoolsBroker::default();
        broker
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .readiness = Some(BrowserReadiness {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            adapter: live_adapter(),
            adapter_enabled: true,
            user_authorized: true,
            connected: true,
            interactive_session_unlocked: true,
            tools: vec![BrowserToolKind::TakeSnapshot],
            reason: None,
            observed_at_unix_ms: 1,
        });

        broker.refresh_timestamp(false);

        let readiness = broker.readiness().expect("browser readiness projection");
        assert!(readiness.connected);
        assert!(!readiness.interactive_session_unlocked);
        readiness.validate().unwrap();
    }

    fn approved_websocket_endpoint() -> String {
        let port_file = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap())
            .join("Google/Chrome/User Data/DevToolsActivePort");
        let lines = std::fs::read_to_string(port_file).unwrap();
        let mut lines = lines.lines();
        let port = lines.next().unwrap().parse::<u16>().unwrap();
        let path = lines.next().unwrap();
        assert!(path.starts_with("/devtools/browser/"));
        format!("ws://127.0.0.1:{port}{path}")
    }

    async fn isolated_headless_chrome() -> (tempfile::TempDir, tokio::process::Child, String) {
        let profile = tempfile::tempdir().unwrap();
        let chrome = [
            std::env::var_os("ProgramFiles")
                .map(PathBuf::from)
                .map(|root| root.join("Google/Chrome/Application/chrome.exe")),
            std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .map(|root| root.join("Google/Chrome/Application/chrome.exe")),
        ]
        .into_iter()
        .flatten()
        .find(|path| path.is_file())
        .unwrap();
        let mut child = tokio::process::Command::new(chrome)
            .arg("--headless=new")
            .arg("--remote-debugging-port=0")
            .arg("--remote-debugging-address=127.0.0.1")
            .arg("--remote-allow-origins=*")
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .arg("--no-first-run")
            .arg("about:blank")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let port_file = profile.path().join("DevToolsActivePort");
        for _ in 0..100 {
            if let Ok(lines) = std::fs::read_to_string(&port_file) {
                let mut lines = lines.lines();
                let port = lines.next().unwrap().parse::<u16>().unwrap();
                let _path = lines.next().unwrap();
                for _ in 0..100 {
                    if tokio::net::TcpStream::connect(("127.0.0.1", port))
                        .await
                        .is_ok()
                    {
                        return (profile, child, format!("http://127.0.0.1:{port}/"));
                    }
                    assert!(child.try_wait().unwrap().is_none());
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                panic!("isolated Chrome did not accept DevTools connections on {port}");
            }
            assert!(child.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("isolated Chrome did not publish DevToolsActivePort");
    }

    async fn loopback_form_server() -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let html = r#"<!doctype html>
<html><body><main>
<label>Subject <input aria-label="Subject" /></label>
<label>Body <textarea aria-label="Body"></textarea></label>
<button type="button" aria-label="Save draft">Save draft</button>
</main></body></html>"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            html.len(),
            html
        );
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let response = response.clone();
                tokio::spawn(async move {
                    let mut request = [0u8; 8 * 1024];
                    let _ = stream.read(&mut request).await;
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (port, task)
    }

    #[test]
    fn pinned_surface_accepts_required_tools_even_when_upstream_has_forbidden_extras() {
        let mut tools = audited_tools();
        tools.push(tool("evaluate_script", &["pageId", "function"]));
        tools.push(tool("list_network_requests", &["pageId"]));
        audit_tool_surface(&tools).unwrap();
    }

    #[test]
    fn schema_drift_and_missing_tools_fail_connection() {
        let mut missing = audited_tools();
        missing.retain(|tool| tool.name != "take_snapshot");
        assert_eq!(
            audit_tool_surface(&missing),
            Err(ChromeDevtoolsMcpError::MissingRequiredTool("take_snapshot"))
        );

        let mut drifted = audited_tools();
        let click = drifted
            .iter_mut()
            .find(|tool| tool.name == "click")
            .unwrap();
        click.input_schema = Arc::new(
            serde_json::from_value(json!({"type": "object", "properties": {"uid": {}}})).unwrap(),
        );
        assert_eq!(
            audit_tool_surface(&drifted),
            Err(ChromeDevtoolsMcpError::ToolSchemaMismatch("click"))
        );
    }

    #[test]
    fn raw_or_privileged_arguments_cannot_cross_gateway() {
        let mut arguments = Map::new();
        arguments.insert("pageId".into(), json!(1));
        arguments.insert("uid".into(), json!("send"));
        validate_call_arguments(AllowedChromeMcpTool::Click, &arguments).unwrap();

        arguments.insert("function".into(), json!("() => document.cookie"));
        assert_eq!(
            validate_call_arguments(AllowedChromeMcpTool::Click, &arguments),
            Err(ChromeDevtoolsMcpError::InvalidArguments("click"))
        );
    }

    #[test]
    fn pinned_launch_disables_unneeded_categories_and_enables_page_routing() {
        assert!(CHROME_DEVTOOLS_MCP_ARGS.contains(&"--experimentalPageIdRouting"));
        assert!(CHROME_DEVTOOLS_MCP_ARGS.contains(&"--experimentalStructuredContent"));
        assert!(CHROME_DEVTOOLS_MCP_ARGS.contains(&"--no-category-network"));
        assert!(CHROME_DEVTOOLS_MCP_ARGS.contains(&"--no-category-performance"));
        assert!(CHROME_DEVTOOLS_MCP_ARGS.contains(&"--no-category-emulation"));
        assert!(!CHROME_DEVTOOLS_MCP_ARGS.iter().any(|argument| {
            argument.contains("experimentalVision")
                || argument.contains("experimentalThirdParty")
                || argument.contains("allowUnrestrictedPaths")
        }));
    }

    #[test]
    fn chrome_version_parser_requires_one_bounded_four_part_version() {
        assert_eq!(
            parse_chrome_version("151.0.7922.175"),
            Some((151, vec![151, 0, 7922, 175]))
        );
        assert_eq!(parse_chrome_version("151.0.7922"), None);
        assert_eq!(parse_chrome_version("latest"), None);
        assert_eq!(parse_chrome_version("70000.0.0.0"), None);
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    #[test]
    fn chrome_identity_is_unavailable_without_a_platform_adapter() {
        assert!(installed_chrome_identity().is_none());
    }

    #[test]
    fn parses_linux_chrome_version_output_without_trusting_product_text() {
        assert_eq!(
            parse_linux_chrome_version_output("Google Chrome 151.0.7922.175\n"),
            Some("151.0.7922.175".into())
        );
        assert_eq!(parse_linux_chrome_version_output("Chromium latest"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn installed_macos_chrome_bundle_reports_a_supported_version_shape() {
        let Some(version) = installed_macos_chrome_version() else {
            return;
        };
        let (major, parts) = parse_chrome_version(&version).unwrap();
        assert_eq!(u32::from(major), parts[0]);
    }

    #[test]
    fn upstream_tool_errors_are_text_only_and_bounded() {
        let result = CallToolResult::error(vec![rmcp::model::Content::text(format!(
            "cannot connect\n{}",
            "x".repeat(600)
        ))]);
        let summary = tool_error_summary(&result);
        assert!(!summary.contains('\n'));
        assert_eq!(summary.chars().count(), 512);
    }

    #[tokio::test]
    #[ignore = "requires Chrome 144+, remote debugging enabled, and user approval"]
    async fn live_pinned_package_handshake_and_tool_audit() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let package = ChromeDevtoolsMcpPackage {
            node_executable: which::which("node").unwrap().canonicalize().unwrap(),
            package_entrypoint: workspace_root
                .join("node_modules/chrome-devtools-mcp/build/src/bin/chrome-devtools-mcp.js")
                .canonicalize()
                .unwrap(),
            package_version: CHROME_DEVTOOLS_MCP_VERSION.into(),
            package_integrity: CHROME_DEVTOOLS_MCP_NPM_INTEGRITY.into(),
        };
        let session = ChromeDevtoolsMcpSession::connect(&package, live_adapter())
            .await
            .unwrap();
        session.close().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "opens one provider-owned loopback page in an approved Chrome session"]
    async fn live_typed_open_and_snapshot_projection() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let package = ChromeDevtoolsMcpPackage {
            node_executable: which::which("node").unwrap().canonicalize().unwrap(),
            package_entrypoint: workspace_root
                .join("node_modules/chrome-devtools-mcp/build/src/bin/chrome-devtools-mcp.js")
                .canonicalize()
                .unwrap(),
            package_version: CHROME_DEVTOOLS_MCP_VERSION.into(),
            package_integrity: CHROME_DEVTOOLS_MCP_NPM_INTEGRITY.into(),
        };
        let session = ChromeDevtoolsMcpSession::connect_approved_websocket(
            &package,
            live_adapter(),
            &approved_websocket_endpoint(),
        )
        .await
        .unwrap();
        let opened = session
            .execute(&BrowserActionRequest {
                schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
                call_id: "live-open-loopback".into(),
                action: BrowserAction::OpenPage {
                    target: BrowserNavigationTarget {
                        url: "http://127.0.0.1:5174/".into(),
                        origin: BrowserOrigin {
                            kind: BrowserOriginKind::HttpLoopback,
                            host_ascii: "127.0.0.1".into(),
                            port: 5174,
                        },
                    },
                },
            })
            .await
            .unwrap();
        assert_eq!(opened.outcome, BrowserActionOutcome::PageOpened);
        assert_eq!(opened.page.document_revision, 1);
        assert!(opened.snapshot.is_none());

        let captured = session
            .execute(&BrowserActionRequest {
                schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
                call_id: "live-snapshot-loopback".into(),
                action: BrowserAction::TakeSnapshot {
                    page: opened.page,
                    max_elements: 64,
                },
            })
            .await
            .unwrap();
        assert_eq!(captured.outcome, BrowserActionOutcome::SnapshotCaptured);
        assert_eq!(captured.page.document_revision, 2);
        assert!(captured.snapshot.unwrap().elements.len() <= 64);
        session.close().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "fills only a process-owned loopback form in an approved Chrome session"]
    async fn live_typed_fill_form_and_semantic_readback() {
        let (port, server) = loopback_form_server().await;
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let package = ChromeDevtoolsMcpPackage {
            node_executable: which::which("node").unwrap().canonicalize().unwrap(),
            package_entrypoint: workspace_root
                .join("node_modules/chrome-devtools-mcp/build/src/bin/chrome-devtools-mcp.js")
                .canonicalize()
                .unwrap(),
            package_version: CHROME_DEVTOOLS_MCP_VERSION.into(),
            package_integrity: CHROME_DEVTOOLS_MCP_NPM_INTEGRITY.into(),
        };
        let session = ChromeDevtoolsMcpSession::connect_approved_websocket(
            &package,
            live_adapter(),
            &approved_websocket_endpoint(),
        )
        .await
        .unwrap();
        let origin = BrowserOrigin {
            kind: BrowserOriginKind::HttpLoopback,
            host_ascii: "127.0.0.1".into(),
            port,
        };
        let opened = session
            .execute(&BrowserActionRequest {
                schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
                call_id: "live-form-open".into(),
                action: BrowserAction::OpenPage {
                    target: BrowserNavigationTarget {
                        url: format!("http://127.0.0.1:{port}/"),
                        origin,
                    },
                },
            })
            .await
            .unwrap();
        let captured = session
            .execute(&BrowserActionRequest {
                schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
                call_id: "live-form-snapshot".into(),
                action: BrowserAction::TakeSnapshot {
                    page: opened.page,
                    max_elements: 32,
                },
            })
            .await
            .unwrap();
        let snapshot = captured.snapshot.unwrap();
        let subject = snapshot
            .elements
            .iter()
            .find(|element| element.accessible_name == "Subject")
            .unwrap()
            .clone();
        let body = snapshot
            .elements
            .iter()
            .find(|element| element.accessible_name == "Body")
            .unwrap()
            .clone();
        let filled = session
            .execute(&BrowserActionRequest {
                schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
                call_id: "live-form-fill".into(),
                action: BrowserAction::FillForm {
                    page: captured.page,
                    fields: vec![
                        BrowserFormField {
                            element: subject,
                            value: "Quarterly draft".into(),
                        },
                        BrowserFormField {
                            element: body,
                            value: "Local semantic readback only".into(),
                        },
                    ],
                    mutation_class: BrowserMutationClass::WriteExternalDraft,
                },
            })
            .await
            .unwrap();
        assert_eq!(filled.outcome, BrowserActionOutcome::FormFilled);
        assert_eq!(filled.page.document_revision, 3);
        let readback = filled.snapshot.unwrap();
        assert_eq!(
            readback
                .elements
                .iter()
                .find(|element| element.accessible_name == "Subject")
                .and_then(|element| element.value.as_deref()),
            Some("Quarterly draft")
        );
        assert_eq!(
            readback
                .elements
                .iter()
                .find(|element| element.accessible_name == "Body")
                .and_then(|element| element.value.as_deref()),
            Some("Local semantic readback only")
        );
        session.close().await.unwrap();
        server.abort();
    }

    #[tokio::test]
    #[ignore = "requires the approved Chrome auto-connect session"]
    async fn live_worker_broker_readiness_surface_and_typed_dispatch() {
        let (port, server) = loopback_form_server().await;
        let (profile, mut chrome, browser_url) = isolated_headless_chrome().await;
        let broker = BrowserDevtoolsBroker::default();
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let package = ChromeDevtoolsMcpPackage {
            node_executable: which::which("node").unwrap().canonicalize().unwrap(),
            package_entrypoint: workspace_root
                .join("node_modules/chrome-devtools-mcp/build/src/bin/chrome-devtools-mcp.js")
                .canonicalize()
                .unwrap(),
            package_version: CHROME_DEVTOOLS_MCP_VERSION.into(),
            package_integrity: CHROME_DEVTOOLS_MCP_NPM_INTEGRITY.into(),
        };
        let adapter = live_adapter();
        let session = ChromeDevtoolsMcpSession::connect_approved_browser_url(
            &package,
            adapter.clone(),
            &browser_url,
        )
        .await
        .unwrap();
        *broker.session.lock().await = Some(session);
        let surface = ObjectRef {
            token: "live-browser-surface".into(),
            snapshot_id: "live-browser-connection-1".into(),
            object_kind: ObjectKind::BrowserSurface,
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(25)).to_rfc3339(),
        };
        {
            let mut projection = broker.projection.lock().unwrap();
            projection.readiness = Some(BrowserReadiness {
                schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
                adapter,
                adapter_enabled: true,
                user_authorized: true,
                connected: true,
                interactive_session_unlocked: true,
                tools: vec![
                    BrowserToolKind::OpenPage,
                    BrowserToolKind::NavigatePage,
                    BrowserToolKind::TakeSnapshot,
                    BrowserToolKind::WaitFor,
                    BrowserToolKind::FillForm,
                    BrowserToolKind::ActivateElement,
                ],
                reason: None,
                observed_at_unix_ms: now_unix_ms().unwrap(),
            });
            projection.surface = Some(surface);
        }
        let readiness = broker.readiness().expect("browser readiness");
        assert!(readiness.connected);
        readiness.validate().unwrap();
        let surface = broker.surface_ref().expect("browser surface");
        let opened = broker
            .execute(
                &surface,
                &BrowserActionRequest {
                    schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
                    call_id: "live-broker-open".into(),
                    action: BrowserAction::OpenPage {
                        target: BrowserNavigationTarget {
                            url: format!("http://127.0.0.1:{port}/"),
                            origin: BrowserOrigin {
                                kind: BrowserOriginKind::HttpLoopback,
                                host_ascii: "127.0.0.1".into(),
                                port,
                            },
                        },
                    },
                },
            )
            .await
            .unwrap();
        let captured = broker
            .execute(
                &surface,
                &BrowserActionRequest {
                    schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
                    call_id: "live-broker-snapshot".into(),
                    action: BrowserAction::TakeSnapshot {
                        page: opened.page,
                        max_elements: 16,
                    },
                },
            )
            .await
            .unwrap();
        assert_eq!(captured.outcome, BrowserActionOutcome::SnapshotCaptured);
        assert!(captured.snapshot.unwrap().elements.len() <= 16);
        server.abort();
        chrome.kill().await.unwrap();
        let _ = chrome.wait().await;
        drop(profile);
    }
}
