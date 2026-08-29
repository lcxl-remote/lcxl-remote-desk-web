//! Device-local wire contract for the paired LCXL Chrome extension.
//!
//! Shared Browser actions are converted here only after the worker has
//! validated them. Uploads replace the edge-only ObjectRef with exact verified
//! bytes; native paths and raw browser scripting never exist on this wire.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use actix_web::{App, HttpRequest, HttpResponse, HttpServer, web};
use base64::Engine as _;
use desk_agent_protocol::browser_control::{
    BROWSER_CONTROL_SCHEMA_VERSION, BrowserAction, BrowserActionOutcome, BrowserActionResult,
    BrowserActivationClass, BrowserAdapterRef, BrowserElementRef, BrowserElementRole,
    BrowserFormField, BrowserFormFieldReadback, BrowserMutationClass, BrowserNavigationTarget,
    BrowserOrigin, BrowserPageRef, BrowserSemanticSnapshot, BrowserWaitState,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};

const MAX_EXTENSION_REQUEST_ID_BYTES: usize = 256;
const MAX_EXTENSION_VERSION_BYTES: usize = 64;
const MAX_PROFILE_INCARCATION_BYTES: usize = 256;
const MAX_PAIRING_TOKEN_BYTES: usize = 256;
const BROWSER_EXTENSION_CALL_TIMEOUT: Duration = Duration::from_secs(35);
pub(crate) const BROWSER_EXTENSION_BRIDGE_PORT: u16 = 8091;
pub(crate) const BROWSER_EXTENSION_VERSION: &str = "0.1.0";
const PAIRING_TOKEN_FILE: &str = "browser-extension-pairing-token";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BrowserExtensionHello {
    pub schema_version: u16,
    #[serde(rename = "type")]
    pub message_type: BrowserExtensionHelloType,
    pub pairing_token: String,
    pub extension_version: String,
    pub browser_version: String,
    pub profile_incarnation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum BrowserExtensionHelloType {
    Hello,
}

impl BrowserExtensionHello {
    pub(super) fn validate(&self) -> Result<(), BrowserExtensionBridgeError> {
        if self.schema_version != BROWSER_CONTROL_SCHEMA_VERSION
            || self.message_type != BrowserExtensionHelloType::Hello
            || !bounded_secret(&self.pairing_token, MAX_PAIRING_TOKEN_BYTES)
            || !bounded_id(&self.extension_version, MAX_EXTENSION_VERSION_BYTES)
            || !valid_browser_version(&self.browser_version)
            || !bounded_id(&self.profile_incarnation, MAX_PROFILE_INCARCATION_BYTES)
        {
            return Err(BrowserExtensionBridgeError::InvalidHello);
        }
        Ok(())
    }
}

fn valid_browser_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_EXTENSION_VERSION_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
        && value
            .split('.')
            .next()
            .is_some_and(|major| major.parse::<u16>().is_ok())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BrowserExtensionRequest {
    pub schema_version: u16,
    #[serde(rename = "type")]
    pub message_type: BrowserExtensionRequestType,
    pub request_id: String,
    pub action: BrowserExtensionAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum BrowserExtensionRequestType {
    Request,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum BrowserExtensionAction {
    OpenPage {
        target: BrowserNavigationTarget,
    },
    NavigatePage {
        page: BrowserPageRef,
        target: BrowserNavigationTarget,
    },
    TakeSnapshot {
        page: BrowserPageRef,
        max_elements: u16,
    },
    WaitFor {
        page: BrowserPageRef,
        element: BrowserElementRef,
        state: BrowserWaitState,
        timeout_ms: u32,
    },
    FillForm {
        page: BrowserPageRef,
        fields: Vec<BrowserFormField>,
        mutation_class: BrowserMutationClass,
    },
    FillFormAndUpload {
        page: BrowserPageRef,
        fields: Vec<BrowserFormField>,
        upload_element: BrowserElementRef,
        file_name: String,
        media_type: String,
        size_bytes: u64,
        digest_sha256: String,
        bytes_base64: String,
        mutation_class: BrowserMutationClass,
    },
    UploadFile {
        page: BrowserPageRef,
        element: BrowserElementRef,
        file_name: String,
        media_type: String,
        size_bytes: u64,
        digest_sha256: String,
        bytes_base64: String,
        mutation_class: BrowserMutationClass,
    },
    ActivateElement {
        page: BrowserPageRef,
        element: BrowserElementRef,
        activation_class: BrowserActivationClass,
    },
}

impl BrowserExtensionRequest {
    pub(super) fn from_browser_action(
        request_id: String,
        action: &BrowserAction,
        verified_upload_bytes: Option<&[u8]>,
    ) -> Result<Self, BrowserExtensionBridgeError> {
        action
            .validate()
            .map_err(|_| BrowserExtensionBridgeError::InvalidBrowserAction)?;
        if !bounded_id(&request_id, MAX_EXTENSION_REQUEST_ID_BYTES) {
            return Err(BrowserExtensionBridgeError::InvalidRequestId);
        }
        let action = match action {
            BrowserAction::OpenPage { target } => {
                require_no_upload(verified_upload_bytes)?;
                BrowserExtensionAction::OpenPage {
                    target: target.clone(),
                }
            }
            BrowserAction::NavigatePage { page, target } => {
                require_no_upload(verified_upload_bytes)?;
                BrowserExtensionAction::NavigatePage {
                    page: page.clone(),
                    target: target.clone(),
                }
            }
            BrowserAction::TakeSnapshot { page, max_elements } => {
                require_no_upload(verified_upload_bytes)?;
                BrowserExtensionAction::TakeSnapshot {
                    page: page.clone(),
                    max_elements: *max_elements,
                }
            }
            BrowserAction::WaitFor {
                page,
                element,
                state,
                timeout_ms,
            } => {
                require_no_upload(verified_upload_bytes)?;
                BrowserExtensionAction::WaitFor {
                    page: page.clone(),
                    element: element.clone(),
                    state: *state,
                    timeout_ms: *timeout_ms,
                }
            }
            BrowserAction::FillForm {
                page,
                fields,
                mutation_class,
            } => {
                require_no_upload(verified_upload_bytes)?;
                BrowserExtensionAction::FillForm {
                    page: page.clone(),
                    fields: fields.clone(),
                    mutation_class: *mutation_class,
                }
            }
            BrowserAction::FillFormAndUpload {
                page,
                fields,
                upload_element,
                file_name,
                media_type,
                size_bytes,
                digest_sha256,
                mutation_class,
                ..
            } => {
                let bytes_base64 =
                    encode_verified_upload(verified_upload_bytes, *size_bytes, digest_sha256)?;
                BrowserExtensionAction::FillFormAndUpload {
                    page: page.clone(),
                    fields: fields.clone(),
                    upload_element: upload_element.clone(),
                    file_name: file_name.clone(),
                    media_type: media_type.clone(),
                    size_bytes: *size_bytes,
                    digest_sha256: digest_sha256.clone(),
                    bytes_base64,
                    mutation_class: *mutation_class,
                }
            }
            BrowserAction::UploadFile {
                page,
                element,
                file_name,
                media_type,
                size_bytes,
                digest_sha256,
                mutation_class,
                ..
            } => {
                let bytes_base64 =
                    encode_verified_upload(verified_upload_bytes, *size_bytes, digest_sha256)?;
                BrowserExtensionAction::UploadFile {
                    page: page.clone(),
                    element: element.clone(),
                    file_name: file_name.clone(),
                    media_type: media_type.clone(),
                    size_bytes: *size_bytes,
                    digest_sha256: digest_sha256.clone(),
                    bytes_base64,
                    mutation_class: *mutation_class,
                }
            }
            BrowserAction::ActivateElement {
                page,
                element,
                activation_class,
            } => {
                require_no_upload(verified_upload_bytes)?;
                BrowserExtensionAction::ActivateElement {
                    page: page.clone(),
                    element: element.clone(),
                    activation_class: activation_class.clone(),
                }
            }
        };
        Ok(Self {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            message_type: BrowserExtensionRequestType::Request,
            request_id,
            action,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExtensionResult {
    #[serde(default)]
    page: Option<RawExtensionPage>,
    #[serde(default)]
    snapshot: Option<RawExtensionSnapshot>,
    #[serde(default)]
    form_readback: Vec<BrowserFormFieldReadback>,
    #[serde(default)]
    attachment_file_name: Option<String>,
    #[serde(default)]
    matched: Option<bool>,
    #[serde(default)]
    activated: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExtensionPage {
    page_id: String,
    page_incarnation: String,
    origin: BrowserOrigin,
    document_revision: u64,
    url_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExtensionSnapshot {
    page: RawExtensionPage,
    elements: Vec<RawExtensionElement>,
    truncated: bool,
    captured_at_unix_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExtensionElement {
    element_id: String,
    role: BrowserElementRole,
    accessible_name: String,
    value: Option<String>,
    element_revision: u64,
}

pub(super) fn project_extension_result(
    request: &desk_agent_protocol::browser_control::BrowserActionRequest,
    adapter: &BrowserAdapterRef,
    raw: serde_json::Value,
    completed_at_unix_ms: u64,
) -> Result<BrowserActionResult, BrowserExtensionBridgeError> {
    request
        .validate()
        .map_err(|_| BrowserExtensionBridgeError::InvalidBrowserAction)?;
    adapter
        .validate()
        .map_err(|_| BrowserExtensionBridgeError::InvalidExtensionResult)?;
    let raw: RawExtensionResult = serde_json::from_value(raw)
        .map_err(|_| BrowserExtensionBridgeError::InvalidExtensionResult)?;
    if completed_at_unix_ms == 0 {
        return Err(BrowserExtensionBridgeError::InvalidExtensionResult);
    }
    let raw_page = raw
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.page.clone())
        .or(raw.page.clone())
        .ok_or(BrowserExtensionBridgeError::InvalidExtensionResult)?;
    let page = BrowserPageRef {
        schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
        adapter: adapter.clone(),
        page_id: raw_page.page_id,
        page_incarnation: raw_page.page_incarnation,
        origin: raw_page.origin,
        document_revision: raw_page.document_revision,
        url_sha256: raw_page.url_sha256,
        observed_at_unix_ms: completed_at_unix_ms,
    };
    page.validate()
        .map_err(|_| BrowserExtensionBridgeError::InvalidExtensionResult)?;
    let snapshot = raw.snapshot.map(|snapshot| {
        let elements = snapshot
            .elements
            .into_iter()
            .map(|element| BrowserElementRef {
                page_id: page.page_id.clone(),
                page_incarnation: page.page_incarnation.clone(),
                document_revision: page.document_revision,
                element_id: element.element_id,
                role: element.role,
                accessible_name: element.accessible_name,
                value: element.value,
                element_revision: element.element_revision,
            })
            .collect();
        BrowserSemanticSnapshot {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            page: page.clone(),
            elements,
            truncated: snapshot.truncated,
            captured_at_unix_ms: snapshot.captured_at_unix_ms,
        }
    });
    let outcome = match &request.action {
        BrowserAction::OpenPage { .. } => BrowserActionOutcome::PageOpened,
        BrowserAction::NavigatePage { .. } => BrowserActionOutcome::PageNavigated,
        BrowserAction::TakeSnapshot { .. } => BrowserActionOutcome::SnapshotCaptured,
        BrowserAction::WaitFor { .. } if raw.matched == Some(true) => {
            BrowserActionOutcome::WaitSatisfied
        }
        BrowserAction::FillForm { .. } => BrowserActionOutcome::FormFilled,
        BrowserAction::FillFormAndUpload { file_name, .. }
            if raw.attachment_file_name.as_deref() == Some(file_name) =>
        {
            BrowserActionOutcome::FormFilledWithFile
        }
        BrowserAction::UploadFile { file_name, .. }
            if raw.attachment_file_name.as_deref() == Some(file_name) =>
        {
            BrowserActionOutcome::FileUploaded
        }
        BrowserAction::ActivateElement { .. } if raw.activated == Some(true) => {
            BrowserActionOutcome::ElementActivated
        }
        _ => return Err(BrowserExtensionBridgeError::InvalidExtensionResult),
    };
    let result = BrowserActionResult {
        schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
        call_id: request.call_id.clone(),
        outcome,
        page,
        snapshot,
        form_readback: raw.form_readback,
        completed_at_unix_ms,
    };
    result
        .validate()
        .map_err(|_| BrowserExtensionBridgeError::InvalidExtensionResult)?;
    Ok(result)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserExtensionResponse {
    schema_version: u16,
    #[serde(rename = "type")]
    message_type: BrowserExtensionResponseType,
    request_id: String,
    ok: bool,
    result: Option<serde_json::Value>,
    error_code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BrowserExtensionResponseType {
    Response,
}

#[derive(Debug)]
struct ConnectedExtension {
    revision: u64,
    adapter: BrowserAdapterRef,
    sender: mpsc::UnboundedSender<String>,
}

#[derive(Debug, Default)]
struct BrowserExtensionState {
    connection: Option<ConnectedExtension>,
    readiness: Option<desk_agent_protocol::browser_control::BrowserReadiness>,
    surface: Option<desk_agent_protocol::computer_use::ObjectRef>,
    pages: HashMap<String, BrowserPageRef>,
    pending:
        HashMap<String, oneshot::Sender<Result<serde_json::Value, BrowserExtensionBridgeError>>>,
}

#[derive(Debug, Default)]
pub(super) struct BrowserExtensionBroker {
    state: Mutex<BrowserExtensionState>,
    next_revision: AtomicU64,
}

impl BrowserExtensionBroker {
    pub(super) fn readiness(
        &self,
    ) -> Option<desk_agent_protocol::browser_control::BrowserReadiness> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .readiness
            .clone()
    }

    pub(super) fn surface_ref(&self) -> Option<desk_agent_protocol::computer_use::ObjectRef> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .surface
            .clone()
    }

    fn attach(
        &self,
        device_id: &str,
        os_session_id: &str,
        hello: &BrowserExtensionHello,
        sender: mpsc::UnboundedSender<String>,
    ) -> Result<u64, BrowserExtensionBridgeError> {
        hello.validate()?;
        let browser_major_version = hello
            .browser_version
            .split('.')
            .next()
            .and_then(|value| value.parse::<u16>().ok())
            .ok_or(BrowserExtensionBridgeError::InvalidHello)?;
        let revision = self.next_revision.fetch_add(1, Ordering::SeqCst) + 1;
        let adapter = BrowserAdapterRef {
            engine: desk_agent_protocol::browser_control::BrowserEngineKind::ChromeExtension,
            device_id: device_id.to_string(),
            os_session_id: os_session_id.to_string(),
            browser_major_version,
            browser_version: hello.browser_version.clone(),
            adapter_id: "lcxl-browser-extension".into(),
            adapter_version: hello.extension_version.clone(),
            profile_incarnation: hello.profile_incarnation.clone(),
            connection_revision: revision,
        };
        adapter
            .validate()
            .map_err(|_| BrowserExtensionBridgeError::InvalidHello)?;
        let observed_at_unix_ms = now_unix_ms()?;
        let readiness = desk_agent_protocol::browser_control::BrowserReadiness {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            adapter: adapter.clone(),
            adapter_enabled: true,
            user_authorized: true,
            connected: true,
            interactive_session_unlocked: true,
            tools: vec![
                desk_agent_protocol::browser_control::BrowserToolKind::OpenPage,
                desk_agent_protocol::browser_control::BrowserToolKind::NavigatePage,
                desk_agent_protocol::browser_control::BrowserToolKind::TakeSnapshot,
                desk_agent_protocol::browser_control::BrowserToolKind::WaitFor,
                desk_agent_protocol::browser_control::BrowserToolKind::FillForm,
                desk_agent_protocol::browser_control::BrowserToolKind::UploadFile,
                desk_agent_protocol::browser_control::BrowserToolKind::ActivateElement,
            ],
            reason: None,
            observed_at_unix_ms,
        };
        readiness
            .validate()
            .map_err(|_| BrowserExtensionBridgeError::InvalidHello)?;
        let surface = desk_agent_protocol::computer_use::ObjectRef {
            token: format!(
                "browser-extension-surface-{:x}",
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
            snapshot_id: format!("browser-extension-connection-{revision}"),
            object_kind: desk_agent_protocol::computer_use::ObjectKind::BrowserSurface,
            expires_at: (chrono::Utc::now()
                + chrono::Duration::seconds(super::PERMISSION_FLOW_TTL_SECONDS))
            .to_rfc3339(),
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        fail_pending(&mut state, BrowserExtensionBridgeError::Disconnected);
        state.pages.clear();
        state.connection = Some(ConnectedExtension {
            revision,
            adapter,
            sender,
        });
        state.readiness = Some(readiness);
        state.surface = Some(surface);
        Ok(revision)
    }

    fn detach(&self, revision: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .connection
            .as_ref()
            .map(|connection| connection.revision)
            != Some(revision)
        {
            return;
        }
        fail_pending(&mut state, BrowserExtensionBridgeError::Disconnected);
        state.connection = None;
        state.readiness = None;
        state.surface = None;
        state.pages.clear();
    }

    fn complete(&self, response: BrowserExtensionResponse) {
        if response.schema_version != BROWSER_CONTROL_SCHEMA_VERSION
            || response.message_type != BrowserExtensionResponseType::Response
        {
            return;
        }
        let pending = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .remove(&response.request_id);
        let Some(pending) = pending else {
            return;
        };
        let result = if response.ok {
            response
                .result
                .ok_or(BrowserExtensionBridgeError::InvalidExtensionResult)
        } else {
            Err(BrowserExtensionBridgeError::ExtensionRejected(
                response
                    .error_code
                    .unwrap_or_else(|| "extension_error".into()),
            ))
        };
        let _ = pending.send(result);
    }

    pub(super) fn preflight(
        &self,
        surface: &desk_agent_protocol::computer_use::ObjectRef,
        request: &desk_agent_protocol::browser_control::BrowserActionRequest,
    ) -> Result<(), BrowserExtensionBridgeError> {
        request
            .validate()
            .map_err(|_| BrowserExtensionBridgeError::InvalidBrowserAction)?;
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.surface.as_ref() != Some(surface) || state.connection.is_none() {
            return Err(BrowserExtensionBridgeError::StaleSurface);
        }
        if let Some(candidate) = extension_action_page(&request.action) {
            let Some(authoritative) = state.pages.get(&candidate.page_id) else {
                return Err(BrowserExtensionBridgeError::StaleSurface);
            };
            if !same_page_identity(candidate, authoritative)
                || state
                    .connection
                    .as_ref()
                    .is_none_or(|connection| connection.adapter != authoritative.adapter)
            {
                return Err(BrowserExtensionBridgeError::StaleSurface);
            }
        }
        Ok(())
    }

    pub(super) async fn execute(
        &self,
        surface: &desk_agent_protocol::computer_use::ObjectRef,
        request: &desk_agent_protocol::browser_control::BrowserActionRequest,
    ) -> Result<BrowserActionResult, BrowserExtensionBridgeError> {
        self.preflight(surface, request)?;
        let mut canonical_request = request.clone();
        if let Some(candidate) = extension_action_page(&request.action) {
            let authoritative = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let authoritative = state
                    .pages
                    .get(&candidate.page_id)
                    .filter(|authoritative| same_page_identity(candidate, authoritative))
                    .ok_or(BrowserExtensionBridgeError::StaleSurface)?;
                if state
                    .connection
                    .as_ref()
                    .is_none_or(|connection| connection.adapter != authoritative.adapter)
                {
                    return Err(BrowserExtensionBridgeError::StaleSurface);
                }
                authoritative.clone()
            };
            replace_action_page(&mut canonical_request.action, authoritative);
        }
        let upload = verified_upload_bytes(&canonical_request.action)?;
        let wire = BrowserExtensionRequest::from_browser_action(
            canonical_request.call_id.clone(),
            &canonical_request.action,
            upload.as_deref(),
        )?;
        let serialized = serde_json::to_string(&wire)
            .map_err(|_| BrowserExtensionBridgeError::InvalidBrowserAction)?;
        let (sender, adapter) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let connection = state
                .connection
                .as_ref()
                .ok_or(BrowserExtensionBridgeError::Disconnected)?;
            let sender = connection.sender.clone();
            let adapter = connection.adapter.clone();
            let (result_tx, result_rx) = oneshot::channel();
            // Never replace the original waiter: a duplicate call id must fail
            // independently while the first dispatch retains its result channel.
            if state.pending.contains_key(&request.call_id) {
                return Err(BrowserExtensionBridgeError::DuplicateRequest);
            }
            state.pending.insert(request.call_id.clone(), result_tx);
            (sender, (adapter, result_rx))
        };
        if sender.send(serialized).is_err() {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending
                .remove(&request.call_id);
            return Err(BrowserExtensionBridgeError::Disconnected);
        }
        let (adapter, result_rx) = adapter;
        let raw = match tokio::time::timeout(BROWSER_EXTENSION_CALL_TIMEOUT, result_rx).await {
            Ok(Ok(result)) => result?,
            Ok(Err(_)) => return Err(BrowserExtensionBridgeError::Disconnected),
            Err(_) => {
                self.state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pending
                    .remove(&request.call_id);
                return Err(BrowserExtensionBridgeError::Timeout);
            }
        };
        let result = project_extension_result(&canonical_request, &adapter, raw, now_unix_ms()?)?;
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pages
            .insert(result.page.page_id.clone(), result.page.clone());
        Ok(result)
    }
}

fn extension_action_page(action: &BrowserAction) -> Option<&BrowserPageRef> {
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

fn same_page_identity(candidate: &BrowserPageRef, authoritative: &BrowserPageRef) -> bool {
    candidate.schema_version == authoritative.schema_version
        && candidate.page_id == authoritative.page_id
        && candidate.page_incarnation == authoritative.page_incarnation
        && candidate.origin == authoritative.origin
        && candidate.document_revision == authoritative.document_revision
        && candidate.url_sha256 == authoritative.url_sha256
        && candidate.observed_at_unix_ms == authoritative.observed_at_unix_ms
}

fn replace_action_page(action: &mut BrowserAction, authoritative: BrowserPageRef) {
    match action {
        BrowserAction::OpenPage { .. } => {}
        BrowserAction::NavigatePage { page, .. }
        | BrowserAction::TakeSnapshot { page, .. }
        | BrowserAction::WaitFor { page, .. }
        | BrowserAction::FillForm { page, .. }
        | BrowserAction::FillFormAndUpload { page, .. }
        | BrowserAction::UploadFile { page, .. }
        | BrowserAction::ActivateElement { page, .. } => *page = authoritative,
    }
}

fn verified_upload_bytes(
    action: &BrowserAction,
) -> Result<Option<Vec<u8>>, BrowserExtensionBridgeError> {
    let (file, size_bytes, digest_sha256) = match action {
        BrowserAction::FillFormAndUpload {
            file,
            size_bytes,
            digest_sha256,
            ..
        }
        | BrowserAction::UploadFile {
            file,
            size_bytes,
            digest_sha256,
            ..
        } => (file, size_bytes, digest_sha256),
        _ => return Ok(None),
    };
    let verified = super::file_reference_store::read_verified_bytes(file, *size_bytes)
        .map_err(|_| BrowserExtensionBridgeError::UploadIdentityMismatch)?;
    if verified.bytes.len() as u64 != *size_bytes || verified.sha256 != *digest_sha256 {
        return Err(BrowserExtensionBridgeError::UploadIdentityMismatch);
    }
    Ok(Some(verified.bytes))
}

fn fail_pending(state: &mut BrowserExtensionState, error: BrowserExtensionBridgeError) {
    for (_, pending) in state.pending.drain() {
        let _ = pending.send(Err(error.clone()));
    }
}

#[derive(Clone)]
struct BrowserExtensionEndpointState {
    broker: Arc<BrowserExtensionBroker>,
    pairing_token: Arc<String>,
    device_id: Arc<String>,
    os_session_id: Arc<String>,
}

pub(super) fn start_loopback_bridge(
    broker: Arc<BrowserExtensionBroker>,
    data_root: &Path,
    device_id: String,
    os_session_id: String,
) -> std::io::Result<()> {
    let pairing_token = load_or_create_pairing_token(data_root)?;
    let state = BrowserExtensionEndpointState {
        broker,
        pairing_token: Arc::new(pairing_token),
        device_id: Arc::new(device_id),
        os_session_id: Arc::new(os_session_id),
    };
    let server = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(state.clone()))
            .route("/browser-extension/v1", web::get().to(extension_ws_handler))
    })
    .disable_signals()
    .bind(("127.0.0.1", BROWSER_EXTENSION_BRIDGE_PORT))?
    .run();
    actix_web::rt::spawn(server);
    Ok(())
}

async fn extension_ws_handler(
    state: web::Data<BrowserExtensionEndpointState>,
    request: HttpRequest,
    payload: web::Payload,
) -> Result<HttpResponse, actix_web::Error> {
    if !request
        .headers()
        .get(actix_web::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(valid_extension_origin)
    {
        return Ok(HttpResponse::Forbidden().finish());
    }
    let (response, session, stream) = actix_ws::handle(&request, payload)?;
    actix_web::rt::spawn(run_extension_session(state.into_inner(), session, stream));
    Ok(response)
}

async fn run_extension_session(
    state: Arc<BrowserExtensionEndpointState>,
    mut session: actix_ws::Session,
    mut stream: actix_ws::MessageStream,
) {
    use futures_util::StreamExt as _;

    let first = tokio::time::timeout(Duration::from_secs(10), stream.next()).await;
    let hello = match first {
        Ok(Some(Ok(actix_ws::Message::Text(text)))) => {
            serde_json::from_str::<BrowserExtensionHello>(&text).ok()
        }
        _ => None,
    };
    let Some(hello) = hello else {
        let _ = session.close(None).await;
        return;
    };
    if hello.validate().is_err()
        || !constant_time_eq(
            hello.pairing_token.as_bytes(),
            state.pairing_token.as_bytes(),
        )
    {
        let _ = session.close(None).await;
        return;
    }
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
    let revision =
        match state
            .broker
            .attach(&state.device_id, &state.os_session_id, &hello, outbound_tx)
        {
            Ok(revision) => revision,
            Err(_) => {
                let _ = session.close(None).await;
                return;
            }
        };
    log::info!(
        "[browser-extension] attached device={} os_session={} revision={} extension_version={} browser_version={} profile_incarnation={}",
        state.device_id,
        state.os_session_id,
        revision,
        hello.extension_version,
        hello.browser_version,
        hello.profile_incarnation
    );
    if session
        .text(format!(
            r#"{{"schema_version":{},"type":"hello_ack"}}"#,
            BROWSER_CONTROL_SCHEMA_VERSION
        ))
        .await
        .is_err()
    {
        state.broker.detach(revision);
        return;
    }
    loop {
        tokio::select! {
            outbound = outbound_rx.recv() => {
                let Some(outbound) = outbound else { break };
                if session.text(outbound).await.is_err() { break; }
            }
            inbound = stream.next() => {
                match inbound {
                    Some(Ok(actix_ws::Message::Text(text))) => {
                        if let Ok(response) = serde_json::from_str::<BrowserExtensionResponse>(&text) {
                            state.broker.complete(response);
                        }
                    }
                    Some(Ok(actix_ws::Message::Ping(bytes))) => {
                        let _ = session.pong(&bytes).await;
                    }
                    Some(Ok(actix_ws::Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
    state.broker.detach(revision);
    log::info!(
        "[browser-extension] detached device={} os_session={} revision={}",
        state.device_id,
        state.os_session_id,
        revision
    );
}

fn load_or_create_pairing_token(data_root: &Path) -> std::io::Result<String> {
    let path = data_root.join(PAIRING_TOKEN_FILE);
    match std::fs::read_to_string(&path) {
        Ok(token) if bounded_secret(token.trim(), MAX_PAIRING_TOKEN_BYTES) => {
            return Ok(token.trim().to_string());
        }
        Ok(_) => {
            return Err(std::io::Error::other(
                "browser extension pairing token is malformed",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let token = format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    crate::durable_file::durable_atomic_write(
        &path,
        token.as_bytes(),
        crate::durable_file::FileMode::OwnerOnly,
    )?;
    Ok(token)
}

pub(crate) fn read_pairing_token(data_root: &Path) -> std::io::Result<String> {
    let token = std::fs::read_to_string(data_root.join(PAIRING_TOKEN_FILE))?;
    let token = token.trim();
    if !bounded_secret(token, MAX_PAIRING_TOKEN_BYTES) {
        return Err(std::io::Error::other(
            "browser extension pairing token is malformed",
        ));
    }
    Ok(token.to_string())
}

fn valid_extension_origin(origin: &str) -> bool {
    let Some(extension_id) = origin.strip_prefix("chrome-extension://") else {
        return false;
    };
    extension_id.len() == 32 && extension_id.bytes().all(|byte| matches!(byte, b'a'..=b'p'))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn now_unix_ms() -> Result<u64, BrowserExtensionBridgeError> {
    u64::try_from(chrono::Utc::now().timestamp_millis())
        .map_err(|_| BrowserExtensionBridgeError::Clock)
}

fn encode_verified_upload(
    bytes: Option<&[u8]>,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<String, BrowserExtensionBridgeError> {
    let bytes = bytes.ok_or(BrowserExtensionBridgeError::MissingUploadBytes)?;
    if bytes.len() as u64 != expected_size
        || format!("{:x}", Sha256::digest(bytes)) != expected_sha256
    {
        return Err(BrowserExtensionBridgeError::UploadIdentityMismatch);
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn require_no_upload(bytes: Option<&[u8]>) -> Result<(), BrowserExtensionBridgeError> {
    if bytes.is_some() {
        return Err(BrowserExtensionBridgeError::UnexpectedUploadBytes);
    }
    Ok(())
}

fn bounded_id(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
}

fn bounded_secret(value: &str, maximum: usize) -> bool {
    value.len() >= 16 && value.len() <= maximum && value.bytes().all(|byte| byte.is_ascii_graphic())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BrowserExtensionBridgeError {
    InvalidHello,
    InvalidRequestId,
    InvalidBrowserAction,
    MissingUploadBytes,
    UnexpectedUploadBytes,
    UploadIdentityMismatch,
    InvalidExtensionResult,
    StaleSurface,
    Disconnected,
    DuplicateRequest,
    Timeout,
    ExtensionRejected(String),
    Clock,
}

impl std::fmt::Display for BrowserExtensionBridgeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidHello => formatter.write_str("invalid browser extension hello"),
            Self::InvalidRequestId => formatter.write_str("invalid browser extension request id"),
            Self::InvalidBrowserAction => formatter.write_str("invalid browser action"),
            Self::MissingUploadBytes => formatter.write_str("verified upload bytes are missing"),
            Self::UnexpectedUploadBytes => formatter.write_str("unexpected upload bytes"),
            Self::UploadIdentityMismatch => formatter.write_str("upload identity mismatch"),
            Self::InvalidExtensionResult => formatter.write_str("invalid browser extension result"),
            Self::StaleSurface => formatter.write_str("stale browser surface"),
            Self::Disconnected => formatter.write_str("browser extension is disconnected"),
            Self::DuplicateRequest => formatter.write_str("duplicate browser extension request"),
            Self::Timeout => formatter.write_str("browser extension request timed out"),
            Self::ExtensionRejected(code) => {
                write!(formatter, "browser extension rejected request: {code}")
            }
            Self::Clock => formatter.write_str("system clock is invalid"),
        }
    }
}

impl std::error::Error for BrowserExtensionBridgeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::browser_control::{
        BrowserActionRequest, BrowserAdapterRef, BrowserElementRole, BrowserEngineKind,
        BrowserNavigationTarget, BrowserOrigin, BrowserOriginKind,
    };
    use desk_agent_protocol::computer_use::{ObjectKind, ObjectRef};
    use desk_agent_protocol::data_lineage::ContentRef;

    fn page() -> BrowserPageRef {
        BrowserPageRef {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            adapter: BrowserAdapterRef {
                engine: BrowserEngineKind::ChromeExtension,
                device_id: "device-1".into(),
                os_session_id: "session-1".into(),
                browser_major_version: 151,
                browser_version: "151.0.1".into(),
                adapter_id: "lcxl-browser-extension".into(),
                adapter_version: "0.1.0".into(),
                profile_incarnation: "profile-1".into(),
                connection_revision: 1,
            },
            page_id: "tab-1".into(),
            page_incarnation: "document-1".into(),
            origin: BrowserOrigin {
                kind: BrowserOriginKind::Https,
                host_ascii: "mail.google.com".into(),
                port: 443,
            },
            document_revision: 1,
            url_sha256: "a".repeat(64),
            observed_at_unix_ms: 1,
        }
    }

    fn element(page: &BrowserPageRef) -> BrowserElementRef {
        BrowserElementRef {
            page_id: page.page_id.clone(),
            page_incarnation: page.page_incarnation.clone(),
            document_revision: page.document_revision,
            element_id: "element-1".into(),
            role: BrowserElementRole::Textbox,
            accessible_name: "Attachments".into(),
            value: None,
            element_revision: 1,
        }
    }

    fn upload_action(bytes: &[u8]) -> BrowserAction {
        let page = page();
        let digest = format!("{:x}", Sha256::digest(bytes));
        BrowserAction::UploadFile {
            page: page.clone(),
            element: element(&page),
            file: ObjectRef {
                token: "edge-file-token".into(),
                snapshot_id: "worker:1".into(),
                object_kind: ObjectKind::File,
                expires_at: "2026-08-30T00:00:00Z".into(),
            },
            content: ContentRef::Artifact {
                artifact_id: "edge-file-token".into(),
                sha256: digest.clone(),
                size_bytes: bytes.len() as u64,
                media_type:
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document".into(),
            },
            file_name: "report.docx".into(),
            media_type: "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                .into(),
            size_bytes: bytes.len() as u64,
            digest_sha256: digest,
            mutation_class: BrowserMutationClass::WriteExternalDraft,
        }
    }

    fn extension_hello() -> BrowserExtensionHello {
        BrowserExtensionHello {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            message_type: BrowserExtensionHelloType::Hello,
            pairing_token: "strong-pairing-token-1234".into(),
            extension_version: BROWSER_EXTENSION_VERSION.into(),
            browser_version: "151.0.1".into(),
            profile_incarnation: "profile-1".into(),
        }
    }

    #[test]
    fn upload_wire_contains_exact_bytes_but_no_edge_ref_or_native_path() {
        let bytes = b"PK\x03\x04exact-docx";
        let request = BrowserExtensionRequest::from_browser_action(
            "request-1".into(),
            &upload_action(bytes),
            Some(bytes),
        )
        .unwrap();
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("edge-file-token"));
        assert!(!json.contains("snapshot_id"));
        assert!(!json.contains("native_path"));
        assert!(json.contains(&base64::engine::general_purpose::STANDARD.encode(bytes)));
    }

    #[test]
    fn upload_wire_rejects_digest_or_size_drift() {
        let action = upload_action(b"expected");
        assert_eq!(
            BrowserExtensionRequest::from_browser_action(
                "request-1".into(),
                &action,
                Some(b"changed"),
            ),
            Err(BrowserExtensionBridgeError::UploadIdentityMismatch)
        );
    }

    #[test]
    fn non_upload_action_rejects_smuggled_bytes() {
        let action = BrowserAction::TakeSnapshot {
            page: page(),
            max_elements: 64,
        };
        assert_eq!(
            BrowserExtensionRequest::from_browser_action(
                "request-1".into(),
                &action,
                Some(b"hidden"),
            ),
            Err(BrowserExtensionBridgeError::UnexpectedUploadBytes)
        );
    }

    #[tokio::test]
    async fn connected_extension_round_trips_a_typed_open_action() {
        let broker = Arc::new(BrowserExtensionBroker::default());
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        broker
            .attach("device-1", "session-1", &extension_hello(), outbound_tx)
            .unwrap();
        let surface = broker.surface_ref().unwrap();
        let request = BrowserActionRequest {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            call_id: "call-1".into(),
            action: BrowserAction::OpenPage {
                target: BrowserNavigationTarget {
                    url: "https://mail.google.com/mail/u/0/".into(),
                    origin: BrowserOrigin {
                        kind: BrowserOriginKind::Https,
                        host_ascii: "mail.google.com".into(),
                        port: 443,
                    },
                },
            },
        };
        let task_broker = Arc::clone(&broker);
        let task_surface = surface.clone();
        let task_request = request.clone();
        let task =
            tokio::spawn(async move { task_broker.execute(&task_surface, &task_request).await });
        let wire: BrowserExtensionRequest =
            serde_json::from_str(&outbound_rx.recv().await.unwrap()).unwrap();
        assert_eq!(wire.request_id, "call-1");
        broker.complete(BrowserExtensionResponse {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            message_type: BrowserExtensionResponseType::Response,
            request_id: "call-1".into(),
            ok: true,
            result: Some(serde_json::json!({
                "page": {
                    "page_id": "tab-7",
                    "page_incarnation": "document-7",
                    "origin": {
                        "kind": "https",
                        "host_ascii": "mail.google.com",
                        "port": 443
                    },
                    "document_revision": 1,
                    "url_sha256": "a".repeat(64)
                }
            })),
            error_code: None,
        });
        let result = task.await.unwrap().unwrap();
        assert_eq!(result.outcome, BrowserActionOutcome::PageOpened);
        assert_eq!(result.page.page_id, "tab-7");
        assert_eq!(
            broker.readiness().unwrap().adapter.engine,
            BrowserEngineKind::ChromeExtension
        );
    }

    #[tokio::test]
    async fn duplicate_request_does_not_steal_the_original_waiter() {
        let broker = Arc::new(BrowserExtensionBroker::default());
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        broker
            .attach("device-1", "session-1", &extension_hello(), outbound_tx)
            .unwrap();
        let surface = broker.surface_ref().unwrap();
        let request = BrowserActionRequest {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            call_id: "call-duplicate".into(),
            action: BrowserAction::OpenPage {
                target: BrowserNavigationTarget {
                    url: "https://mail.google.com/mail/u/0/".into(),
                    origin: BrowserOrigin {
                        kind: BrowserOriginKind::Https,
                        host_ascii: "mail.google.com".into(),
                        port: 443,
                    },
                },
            },
        };
        let first_broker = Arc::clone(&broker);
        let first_surface = surface.clone();
        let first_request = request.clone();
        let first =
            tokio::spawn(async move { first_broker.execute(&first_surface, &first_request).await });
        let _: BrowserExtensionRequest =
            serde_json::from_str(&outbound_rx.recv().await.unwrap()).unwrap();

        assert_eq!(
            broker.execute(&surface, &request).await,
            Err(BrowserExtensionBridgeError::DuplicateRequest)
        );
        broker.complete(BrowserExtensionResponse {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            message_type: BrowserExtensionResponseType::Response,
            request_id: request.call_id,
            ok: true,
            result: Some(serde_json::json!({
                "page": {
                    "page_id": "tab-original",
                    "page_incarnation": "document-original",
                    "origin": {
                        "kind": "https",
                        "host_ascii": "mail.google.com",
                        "port": 443
                    },
                    "document_revision": 1,
                    "url_sha256": "a".repeat(64)
                }
            })),
            error_code: None,
        });
        assert_eq!(first.await.unwrap().unwrap().page.page_id, "tab-original");
    }

    #[tokio::test]
    async fn disconnect_fails_the_pending_call_and_invalidates_its_surface() {
        let broker = Arc::new(BrowserExtensionBroker::default());
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        let revision = broker
            .attach("device-1", "session-1", &extension_hello(), outbound_tx)
            .unwrap();
        let surface = broker.surface_ref().unwrap();
        let request = BrowserActionRequest {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            call_id: "call-disconnect".into(),
            action: BrowserAction::OpenPage {
                target: BrowserNavigationTarget {
                    url: "https://mail.google.com/mail/u/0/".into(),
                    origin: BrowserOrigin {
                        kind: BrowserOriginKind::Https,
                        host_ascii: "mail.google.com".into(),
                        port: 443,
                    },
                },
            },
        };
        let task_broker = Arc::clone(&broker);
        let task_surface = surface.clone();
        let task_request = request.clone();
        let task =
            tokio::spawn(async move { task_broker.execute(&task_surface, &task_request).await });
        let _ = outbound_rx.recv().await.unwrap();

        broker.detach(revision);
        assert_eq!(
            task.await.unwrap(),
            Err(BrowserExtensionBridgeError::Disconnected)
        );
        assert_eq!(
            broker.preflight(&surface, &request),
            Err(BrowserExtensionBridgeError::StaleSurface)
        );
    }

    #[tokio::test]
    async fn distinct_pages_remain_independently_addressable() {
        let broker = Arc::new(BrowserExtensionBroker::default());
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        broker
            .attach("device-1", "session-1", &extension_hello(), outbound_tx)
            .unwrap();
        let surface = broker.surface_ref().unwrap();

        let mut pages = Vec::new();
        for (index, host) in ["mail.google.com", "app.slack.com"].into_iter().enumerate() {
            let call_id = format!("call-page-{index}");
            let request = BrowserActionRequest {
                schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
                call_id: call_id.clone(),
                action: BrowserAction::OpenPage {
                    target: BrowserNavigationTarget {
                        url: format!("https://{host}/"),
                        origin: BrowserOrigin {
                            kind: BrowserOriginKind::Https,
                            host_ascii: host.into(),
                            port: 443,
                        },
                    },
                },
            };
            let task_broker = Arc::clone(&broker);
            let task_surface = surface.clone();
            let task =
                tokio::spawn(async move { task_broker.execute(&task_surface, &request).await });
            let _ = outbound_rx.recv().await.unwrap();
            broker.complete(BrowserExtensionResponse {
                schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
                message_type: BrowserExtensionResponseType::Response,
                request_id: call_id,
                ok: true,
                result: Some(serde_json::json!({
                    "page": {
                        "page_id": format!("tab-{index}"),
                        "page_incarnation": format!("document-{index}"),
                        "origin": {
                            "kind": "https",
                            "host_ascii": host,
                            "port": 443
                        },
                        "document_revision": 1,
                        "url_sha256": format!("{:064x}", index + 1)
                    }
                })),
                error_code: None,
            });
            pages.push(task.await.unwrap().unwrap().page);
        }

        assert_eq!(broker.state.lock().unwrap().pages.len(), 2);
        for (index, page) in pages.into_iter().enumerate() {
            let request = BrowserActionRequest {
                schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
                call_id: format!("snapshot-{index}"),
                action: BrowserAction::TakeSnapshot {
                    page,
                    max_elements: 64,
                },
            };
            broker.preflight(&surface, &request).unwrap();
        }
    }

    #[test]
    fn websocket_origin_accepts_only_a_chrome_extension_id() {
        assert!(valid_extension_origin(
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop"
        ));
        assert!(!valid_extension_origin("https://mail.google.com"));
        assert!(!valid_extension_origin("chrome-extension://../../escape"));
    }

    #[test]
    fn pairing_token_is_durable_and_high_entropy() {
        let directory = tempfile::tempdir().unwrap();
        let first = load_or_create_pairing_token(directory.path()).unwrap();
        let second = load_or_create_pairing_token(directory.path()).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.len(), 72);
        assert!(bounded_secret(&first, MAX_PAIRING_TOKEN_BYTES));
        assert_eq!(read_pairing_token(directory.path()).unwrap(), first);
    }

    #[test]
    fn extension_reconnect_invalidates_the_previous_surface() {
        let broker = BrowserExtensionBroker::default();
        let (first_sender, _first_receiver) = mpsc::unbounded_channel();
        broker
            .attach("device-1", "session-1", &extension_hello(), first_sender)
            .unwrap();
        let old_surface = broker.surface_ref().unwrap();

        let mut reconnected = extension_hello();
        reconnected.profile_incarnation = "profile-2".into();
        let (second_sender, _second_receiver) = mpsc::unbounded_channel();
        broker
            .attach("device-1", "session-1", &reconnected, second_sender)
            .unwrap();
        let new_surface = broker.surface_ref().unwrap();
        let request = BrowserActionRequest {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            call_id: "call-after-reconnect".into(),
            action: BrowserAction::OpenPage {
                target: BrowserNavigationTarget {
                    url: "https://mail.google.com/mail/u/0/".into(),
                    origin: BrowserOrigin {
                        kind: BrowserOriginKind::Https,
                        host_ascii: "mail.google.com".into(),
                        port: 443,
                    },
                },
            },
        };

        assert_ne!(old_surface, new_surface);
        assert_eq!(
            broker.preflight(&old_surface, &request),
            Err(BrowserExtensionBridgeError::StaleSurface)
        );
        broker.preflight(&new_surface, &request).unwrap();
    }

    #[test]
    fn preflight_canonicalizes_only_a_model_mutated_adapter() {
        let broker = BrowserExtensionBroker::default();
        let (sender, _receiver) = mpsc::unbounded_channel();
        broker
            .attach("device-1", "session-1", &extension_hello(), sender)
            .unwrap();
        let surface = broker.surface_ref().unwrap();
        let authoritative = page();
        broker
            .state
            .lock()
            .unwrap()
            .pages
            .insert(authoritative.page_id.clone(), authoritative.clone());

        let mut candidate = authoritative.clone();
        candidate.adapter.engine = BrowserEngineKind::ChromeDevtoolsMcp;
        let mut request = BrowserActionRequest {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            call_id: "call-adapter-canonicalization".into(),
            action: BrowserAction::TakeSnapshot {
                page: candidate,
                max_elements: 64,
            },
        };
        broker.preflight(&surface, &request).unwrap();

        if let BrowserAction::TakeSnapshot { page, .. } = &mut request.action {
            page.document_revision += 1;
        }
        assert_eq!(
            broker.preflight(&surface, &request),
            Err(BrowserExtensionBridgeError::StaleSurface)
        );
    }

    #[test]
    fn page_identity_excludes_adapter_but_includes_observation_identity() {
        let authoritative = page();
        let mut candidate = authoritative.clone();
        candidate.adapter.engine = BrowserEngineKind::ChromeDevtoolsMcp;
        assert!(same_page_identity(&candidate, &authoritative));

        candidate.page_incarnation = "different-document".into();
        assert!(!same_page_identity(&candidate, &authoritative));
        candidate = authoritative.clone();
        candidate.url_sha256 = "b".repeat(64);
        assert!(!same_page_identity(&candidate, &authoritative));
        candidate = authoritative.clone();
        candidate.observed_at_unix_ms += 1;
        assert!(!same_page_identity(&candidate, &authoritative));
    }
}
